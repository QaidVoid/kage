//! `kage mcp serve`: expose kage's built-in tools as an MCP server.
//!
//! The mirror image of the client side. [`serve`] speaks the same
//! newline-delimited JSON-RPC over a reader/writer pair (stdio in the
//! binary), answering `initialize`, `tools/list`, and `tools/call` by
//! dispatching into a [`ToolRegistry`]. Requests are answered in
//! arrival order; a running `tools/call` executes on a worker thread so
//! the loop keeps reading, and a `notifications/cancelled` for an
//! in-flight call sets its cancel flag, waits for the tool to unwind
//! and answers it. Notices for unknown or finished ids are ignored.
//!
//! The caller decides what is exposed: the registry holds only the tools
//! to serve, and a [`ServeGate`] may refuse individual calls.
//!
//! Tool failures and refusals are reported the MCP way, as a normal
//! result with `isError: true`, so the calling agent sees the message
//! instead of a transport-level fault. Only genuinely unknown JSON-RPC
//! methods get a JSON-RPC error.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kage_core::CancelFlag;
use kage_jsonrpc::{Inbound, Peer, RpcError, connect};
use kage_tools::ToolRegistry;
use kage_tools::tool::{Tool, ToolContext};

use crate::server::PROTOCOL_VERSION;

/// Protocol revisions `kage mcp serve` can speak. A client asking for
/// one of them gets it back; any other request gets [`PROTOCOL_VERSION`].
const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// How often the serve loop polls in-flight calls for completion while
/// waiting for the next inbound message.
const CALL_POLL: Duration = Duration::from_millis(20);

/// Decides whether one `tools/call` may run: `None` runs it, `Some(reason)`
/// refuses it with `reason` as the error text. Receives the tool name and
/// its arguments.
pub type ServeGate<'a> = &'a dyn Fn(&str, &serde_json::Value) -> Option<String>;

/// One dispatched `tools/call`: its cancel flag and the worker thread
/// computing the response.
struct InFlight<'scope> {
    cancel: Arc<CancelFlag>,
    worker: thread::ScopedJoinHandle<'scope, serde_json::Value>,
}

/// The result of preparing one `tools/call`: an immediate response for an
/// unknown tool, a refusal or malformed arguments, or a running worker.
enum Prepared<'scope> {
    Now(serde_json::Value),
    Running(
        Arc<CancelFlag>,
        thread::ScopedJoinHandle<'scope, serde_json::Value>,
    ),
}

/// Run the MCP server loop until the client closes the connection.
///
/// `workdir` scopes filesystem tools; the binary passes the process
/// working directory. `confine` keeps tool paths under `workdir`, and
/// `gate` is consulted before every call.
///
/// # Errors
///
/// Returns an error only if the reader thread cannot be joined; all
/// per-request failures are reported in-band to the client.
pub fn serve<R, W>(
    registry: &ToolRegistry,
    workdir: &Path,
    confine: bool,
    gate: ServeGate<'_>,
    reader: R,
    writer: W,
) -> std::io::Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let (peer, inbound, handle) = connect(reader, writer);
    thread::scope(|scope| {
        let mut in_flight: HashMap<serde_json::Value, InFlight<'_>> = HashMap::new();
        loop {
            let msg = if in_flight.is_empty() {
                match inbound.recv() {
                    Ok(msg) => msg,
                    Err(_) => break,
                }
            } else {
                match inbound.recv_timeout(CALL_POLL) {
                    Ok(msg) => msg,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if reap(&mut in_flight, &peer) {
                            break;
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            };
            match msg {
                Inbound::Request { id, method, params } => {
                    if method == "tools/call" {
                        match prepare_call(scope, registry, workdir, confine, gate, &params) {
                            Prepared::Now(response) => {
                                if peer.respond(&id, Ok(response)).is_err() {
                                    break;
                                }
                            }
                            Prepared::Running(cancel, worker) => {
                                in_flight.insert(id, InFlight { cancel, worker });
                            }
                        }
                    } else {
                        let outcome = match method.as_str() {
                            "initialize" => Ok(serde_json::json!({
                                "protocolVersion": negotiate(&params),
                                "capabilities": { "tools": { "listChanged": false } },
                                "serverInfo": {
                                    "name": "kage",
                                    "version": env!("CARGO_PKG_VERSION"),
                                },
                            })),
                            "tools/list" => Ok(serde_json::json!({ "tools": tool_list(registry) })),
                            "ping" => Ok(serde_json::json!({})),
                            other => Err(RpcError::method_not_found(other)),
                        };
                        if peer.respond(&id, outcome).is_err() {
                            break;
                        }
                    }
                }
                Inbound::Notification { method, params } => {
                    if method != "notifications/cancelled" {
                        continue;
                    }
                    let Some(request_id) = params.get("requestId").cloned() else {
                        continue;
                    };
                    let Some(call) = in_flight.remove(&request_id) else {
                        continue;
                    };
                    call.cancel.cancel();
                    // Wait for the tool to observe the cancellation,
                    // then answer the cancelled request.
                    if let Ok(response) = call.worker.join()
                        && peer.respond(&request_id, Ok(response)).is_err()
                    {
                        break;
                    }
                }
            }
            if !in_flight.is_empty() && reap(&mut in_flight, &peer) {
                break;
            }
        }
        // The client is gone or the connection broke: stop any call
        // still running so `serve` returns promptly.
        for (_, call) in in_flight {
            call.cancel.cancel();
            let _ = call.worker.join();
        }
    });
    handle
        .join()
        .map_err(|_| std::io::Error::other("mcp serve: reader thread panicked"))
}

/// Write the responses of workers that finished since the last check.
/// Returns whether the connection closed while answering.
fn reap(in_flight: &mut HashMap<serde_json::Value, InFlight<'_>>, peer: &Peer) -> bool {
    let finished: Vec<serde_json::Value> = in_flight
        .iter()
        .filter(|(_, call)| call.worker.is_finished())
        .map(|(id, _)| id.clone())
        .collect();
    for request_id in finished {
        let Some(call) = in_flight.remove(&request_id) else {
            continue;
        };
        if let Ok(response) = call.worker.join()
            && peer.respond(&request_id, Ok(response)).is_err()
        {
            return true;
        }
    }
    false
}

/// Prepare one `tools/call`: judge it through the gate and either answer
/// immediately or spawn the worker that runs the tool.
fn prepare_call<'scope, 'env>(
    scope: &'scope thread::Scope<'scope, 'env>,
    registry: &ToolRegistry,
    workdir: &'scope Path,
    confine: bool,
    gate: ServeGate<'_>,
    params: &serde_json::Value,
) -> Prepared<'scope> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let Some(tool) = registry.get(name) else {
        return Prepared::Now(error_result(format!("unknown tool: {name}")));
    };
    let tool = Arc::clone(tool);
    // The gate judges the tool's real name: permission rules are keyed
    // by it, so a call that arrived as an alias must not slip past the
    // rules for the registered tool.
    if let Some(reason) = gate(registry.canonical_name(name), &arguments) {
        return Prepared::Now(error_result(reason));
    }
    let cancel = Arc::new(CancelFlag::new());
    let worker_cancel = Arc::clone(&cancel);
    let worker = scope.spawn(move || run_tool(tool, workdir, confine, arguments, &worker_cancel));
    Prepared::Running(cancel, worker)
}

/// Run one tool call to completion, as an MCP `tools/call` result.
fn run_tool(
    tool: Arc<dyn Tool>,
    workdir: &Path,
    confine: bool,
    arguments: serde_json::Value,
    cancel: &CancelFlag,
) -> serde_json::Value {
    let mut cx = ToolContext::new(workdir, cancel);
    if confine {
        cx = cx.with_confine();
    }
    match tool.execute(arguments, &cx) {
        Ok(out) => serde_json::json!({
            "content": [{ "type": "text", "text": out.text }],
            "isError": out.is_error,
        }),
        Err(e) => error_result(e.to_string()),
    }
}

/// The version to answer `initialize` with: the client's requested
/// `protocolVersion` when kage knows it, else its own, with whether
/// the answer is not the requested version.
fn negotiate_with_flag(params: &serde_json::Value) -> (&'static str, bool) {
    let requested = params.get("protocolVersion").and_then(|v| v.as_str());
    match SUPPORTED_VERSIONS
        .into_iter()
        .find(|v| Some(*v) == requested)
    {
        Some(version) => (version, false),
        None => (PROTOCOL_VERSION, requested.is_some()),
    }
}

/// [`negotiate_with_flag`] with one stderr line on a downgrade, so a
/// client asking for an unknown revision is visible; stderr is
/// log-safe for a stdio MCP server.
fn negotiate(params: &serde_json::Value) -> &'static str {
    let (version, drifted) = negotiate_with_flag(params);
    if drifted {
        let requested = params.get("protocolVersion").and_then(|v| v.as_str());
        eprintln!(
            "kage mcp serve: client asked for protocol {}, answering {version}",
            requested.unwrap_or_default()
        );
    }
    version
}

/// The registry as MCP tool descriptors.
fn tool_list(registry: &ToolRegistry) -> Vec<serde_json::Value> {
    registry
        .list_for_provider()
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.schema,
            })
        })
        .collect()
}

/// An MCP `tools/call` result carrying an error message.
fn error_result(message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    use kage_core::{Risk, ToolOutput};
    use kage_tools::error::ToolError;
    use kage_tools::tool::Tool;

    use super::*;
    use kage_jsonrpc::connect;

    #[derive(Debug)]
    struct Echo;

    impl Tool for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "echo the message argument"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "message": { "type": "string" } },
            })
        }
        fn risk(&self) -> Risk {
            Risk::Read
        }
        fn execute(
            &self,
            input: serde_json::Value,
            _cx: &ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            let msg = input
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_owned();
            Ok(ToolOutput {
                is_error: false,
                text: msg,
                structured: None,
                terminate: false,
            })
        }
    }

    /// Spawn `serve` on one end of a pipe pair, returning a client
    /// peer wired to the other end.
    fn client() -> kage_jsonrpc::Peer {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        thread::spawn(move || {
            let mut reg = ToolRegistry::new();
            reg.register(Arc::new(Echo));
            let wd = std::env::temp_dir();
            let gate = |_: &str, input: &serde_json::Value| {
                (input["message"] == "forbidden").then(|| "refused by gate".to_owned())
            };
            serve(&reg, &wd, false, &gate, BufReader::new(srv_r), srv_w).unwrap();
        });
        let (peer, _in, _h) = connect(BufReader::new(cli_r), cli_w);
        peer
    }

    #[test]
    fn initialize_reports_kage_server() {
        let peer = client();
        let res = peer.request("initialize", serde_json::json!({})).unwrap();
        assert_eq!(res["serverInfo"]["name"], "kage");
        assert_eq!(res["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn initialize_echoes_a_known_requested_version() {
        let peer = client();
        let res = peer
            .request(
                "initialize",
                serde_json::json!({ "protocolVersion": "2025-03-26" }),
            )
            .unwrap();
        assert_eq!(res["protocolVersion"], "2025-03-26");
    }

    #[test]
    fn initialize_answers_its_own_version_for_an_unknown_one() {
        let peer = client();
        let res = peer
            .request(
                "initialize",
                serde_json::json!({ "protocolVersion": "2099-01-01" }),
            )
            .unwrap();
        assert_eq!(res["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn negotiate_flags_only_an_unsupported_requested_version() {
        let (version, drifted) =
            negotiate_with_flag(&serde_json::json!({ "protocolVersion": "2099-01-01" }));
        assert_eq!(version, PROTOCOL_VERSION);
        assert!(drifted, "an unknown version is a downgrade");

        let (version, drifted) =
            negotiate_with_flag(&serde_json::json!({ "protocolVersion": "2025-03-26" }));
        assert_eq!(version, "2025-03-26");
        assert!(!drifted, "a supported version is answered verbatim");

        let (version, drifted) = negotiate_with_flag(&serde_json::json!({}));
        assert_eq!(version, PROTOCOL_VERSION);
        assert!(!drifted, "no request, no downgrade");
    }

    #[test]
    fn tools_list_exposes_registered_tools() {
        let peer = client();
        let res = peer.request("tools/list", serde_json::json!({})).unwrap();
        let tools = res["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "echo");
        assert_eq!(tools[0]["inputSchema"]["type"], "object");
    }

    #[test]
    fn tools_call_dispatches_and_round_trips() {
        let peer = client();
        let res = peer
            .request(
                "tools/call",
                serde_json::json!({
                    "name": "echo",
                    "arguments": { "message": "hi there" },
                }),
            )
            .unwrap();
        assert_eq!(res["isError"], false);
        assert_eq!(res["content"][0]["text"], "hi there");
    }

    #[test]
    fn unknown_tool_is_an_in_band_error() {
        let peer = client();
        let res = peer
            .request(
                "tools/call",
                serde_json::json!({ "name": "nope", "arguments": {} }),
            )
            .unwrap();
        assert_eq!(res["isError"], true);
        assert!(
            res["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("unknown tool")
        );
    }

    #[test]
    fn gate_refusal_is_an_in_band_error() {
        let peer = client();
        let res = peer
            .request(
                "tools/call",
                serde_json::json!({
                    "name": "echo",
                    "arguments": { "message": "forbidden" },
                }),
            )
            .unwrap();
        assert_eq!(res["isError"], true);
        assert_eq!(res["content"][0]["text"], "refused by gate");
    }

    /// A tool the test gate allows, so the alias path is proven both
    /// ways.
    #[derive(Debug)]
    struct Pass;

    impl Tool for Pass {
        fn name(&self) -> &'static str {
            "free"
        }
        fn description(&self) -> &'static str {
            "always runs"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn risk(&self) -> Risk {
            Risk::Read
        }
        fn execute(
            &self,
            _input: serde_json::Value,
            _cx: &ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                is_error: false,
                text: "ran".to_owned(),
                structured: None,
                terminate: false,
            })
        }
    }

    #[test]
    fn a_call_under_an_alias_is_judged_by_its_real_name() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        thread::spawn(move || {
            let reg = ToolRegistry::new()
                .with(Arc::new(Echo))
                .alias("bash", "echo")
                .with(Arc::new(Pass))
                .alias("freely", "free");
            let wd = std::env::temp_dir();
            let gate = |name: &str, _: &serde_json::Value| {
                (name == "echo").then(|| "`echo` is denied by permissions".to_owned())
            };
            serve(&reg, &wd, false, &gate, BufReader::new(srv_r), srv_w).unwrap();
        });
        let (peer, _in, _h) = connect(BufReader::new(cli_r), cli_w);
        let refused = peer
            .request(
                "tools/call",
                serde_json::json!({ "name": "bash", "arguments": {} }),
            )
            .unwrap();
        assert_eq!(refused["isError"], true);
        assert!(
            refused["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("`echo` is denied"),
            "{refused}"
        );
        let allowed = peer
            .request(
                "tools/call",
                serde_json::json!({ "name": "freely", "arguments": {} }),
            )
            .unwrap();
        assert_eq!(allowed["isError"], false);
        assert_eq!(allowed["content"][0]["text"], "ran");
    }

    #[test]
    fn unknown_method_is_a_jsonrpc_error() {
        let peer = client();
        let err = peer
            .request("resources/list", serde_json::json!({}))
            .unwrap_err();
        assert_eq!(err.code, -32601);
    }

    /// A registry with `tools` behind a pass-all gate, served on one end
    /// of a pipe pair; returns the client peer.
    fn serve_with(tools: Vec<Arc<dyn Tool>>) -> kage_jsonrpc::Peer {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        thread::spawn(move || {
            let mut reg = ToolRegistry::new();
            for tool in tools {
                reg.register(tool);
            }
            let wd = std::env::temp_dir();
            let gate = |_: &str, _: &serde_json::Value| None;
            serve(&reg, &wd, false, &gate, BufReader::new(srv_r), srv_w).unwrap();
        });
        let (peer, _in, _h) = connect(BufReader::new(cli_r), cli_w);
        peer
    }

    /// A tool that loops until cancelled, giving up after a generous
    /// deadline and reporting its natural end.
    #[derive(Debug)]
    struct UntilCancelled {
        started: Arc<AtomicBool>,
    }

    impl Tool for UntilCancelled {
        fn name(&self) -> &'static str {
            "until_cancelled"
        }
        fn description(&self) -> &'static str {
            "loops until cancelled"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn risk(&self) -> Risk {
            Risk::Read
        }
        fn execute(
            &self,
            _input: serde_json::Value,
            cx: &ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            self.started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !cx.is_cancelled() && std::time::Instant::now() < deadline {
                thread::sleep(std::time::Duration::from_millis(5));
            }
            let text = if cx.is_cancelled() {
                "cancelled"
            } else {
                "natural end"
            };
            Ok(ToolOutput {
                is_error: false,
                text: text.to_owned(),
                structured: None,
                terminate: false,
            })
        }
    }

    #[test]
    fn a_cancel_notice_stops_an_in_flight_call() {
        let started = Arc::new(AtomicBool::new(false));
        let peer = serve_with(vec![Arc::new(UntilCancelled {
            started: Arc::clone(&started),
        })]);
        // Consumes request id 1, so the call below runs as id 2.
        peer.request("initialize", serde_json::json!({})).unwrap();
        let start = std::time::Instant::now();
        let call_peer = peer.clone();
        let call = thread::spawn(move || {
            call_peer.request(
                "tools/call",
                serde_json::json!({ "name": "until_cancelled", "arguments": {} }),
            )
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !started.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            started.load(std::sync::atomic::Ordering::SeqCst),
            "tool never started"
        );
        peer.notify(
            "notifications/cancelled",
            serde_json::json!({ "requestId": 2 }),
        )
        .unwrap();
        let outcome = call.join().unwrap().unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(9),
            "the tool must stop before its own deadline"
        );
        assert_eq!(outcome["content"][0]["text"], "cancelled");
    }

    #[test]
    fn a_cancel_notice_for_an_unknown_id_is_a_no_op() {
        let peer = serve_with(vec![Arc::new(Echo)]);
        peer.request("initialize", serde_json::json!({})).unwrap();
        let call_peer = peer.clone();
        let call = thread::spawn(move || {
            call_peer.request(
                "tools/call",
                serde_json::json!({
                    "name": "echo",
                    "arguments": { "message": "hi there" },
                }),
            )
        });
        peer.notify(
            "notifications/cancelled",
            serde_json::json!({ "requestId": 99 }),
        )
        .unwrap();
        let outcome = call.join().unwrap().unwrap();
        assert_eq!(outcome["content"][0]["text"], "hi there");
    }
}
