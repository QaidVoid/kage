//! `kage mcp serve`: expose kage's built-in tools as an MCP server.
//!
//! The mirror image of the client side. [`serve`] speaks the same
//! newline-delimited JSON-RPC over a reader/writer pair (stdio in the
//! binary), answering `initialize`, `tools/list`, and `tools/call` by
//! dispatching into a [`ToolRegistry`]. Requests are handled
//! sequentially on the inbound thread: a simple MCP client awaits
//! each response, and sequential dispatch keeps tool side effects
//! ordered without a work-stealing pool.
//!
//! Tool failures are reported the MCP way - a normal result with
//! `isError: true` - so the calling agent sees the message instead of
//! a transport-level fault. Only genuinely unknown JSON-RPC methods
//! get a JSON-RPC error.

use std::io::{BufRead, Write};
use std::path::Path;

use kage_core::CancelFlag;
use kage_jsonrpc::{Inbound, RpcError, connect};
use kage_tools::ToolRegistry;
use kage_tools::tool::ToolContext;

use crate::server::PROTOCOL_VERSION;

/// Run the MCP server loop until the client closes the connection.
///
/// `workdir` scopes filesystem tools; the binary passes the process
/// working directory.
///
/// # Errors
///
/// Returns an error only if the reader thread cannot be joined; all
/// per-request failures are reported in-band to the client.
pub fn serve<R, W>(
    registry: &ToolRegistry,
    workdir: &Path,
    reader: R,
    writer: W,
) -> std::io::Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let (peer, inbound, handle) = connect(reader, writer);
    for msg in inbound {
        let Inbound::Request { id, method, params } = msg else {
            continue;
        };
        let outcome = match method.as_str() {
            "initialize" => Ok(serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "kage",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            })),
            "tools/list" => Ok(serde_json::json!({ "tools": tool_list(registry) })),
            "tools/call" => Ok(call_tool(registry, workdir, &params)),
            "ping" => Ok(serde_json::json!({})),
            other => Err(RpcError::method_not_found(other)),
        };
        if peer.respond(&id, outcome).is_err() {
            break;
        }
    }
    handle
        .join()
        .map_err(|_| std::io::Error::other("mcp serve: reader thread panicked"))
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

/// Dispatch one `tools/call`. An unknown tool, bad params, or a tool
/// error all become an `isError` result rather than a fault.
fn call_tool(
    registry: &ToolRegistry,
    workdir: &Path,
    params: &serde_json::Value,
) -> serde_json::Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let Some(tool) = registry.get(name) else {
        return error_result(format!("unknown tool: {name}"));
    };
    let cancel = CancelFlag::new();
    let cx = ToolContext::new(workdir, &cancel);
    match tool.execute(arguments, &cx) {
        Ok(out) => serde_json::json!({
            "content": [{ "type": "text", "text": out.text }],
            "isError": out.is_error,
        }),
        Err(e) => error_result(e.to_string()),
    }
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
            serve(&reg, &wd, BufReader::new(srv_r), srv_w).unwrap();
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
    fn unknown_method_is_a_jsonrpc_error() {
        let peer = client();
        let err = peer
            .request("resources/list", serde_json::json!({}))
            .unwrap_err();
        assert_eq!(err.code, -32601);
    }
}
