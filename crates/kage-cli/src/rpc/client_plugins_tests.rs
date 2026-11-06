use std::io::BufReader;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Instant;

use kage_core::CancelFlag;
use kage_jsonrpc::Inbound;
use kage_plugin::HostLog;
use serde_json::json;

use super::*;

const SHORT: Duration = Duration::from_millis(150);
const LONG: Duration = Duration::from_secs(10);

type Handler = Box<dyn Fn(&str, &Value) -> Option<Result<Value, RpcError>> + Send>;
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// The client end: answers each reverse request through its handler, or
/// never when the handler returns `None`.
struct Owner {
    calls: Calls,
    stop: Arc<AtomicBool>,
}

impl Owner {
    fn calls_to(&self, method: &str) -> Vec<Value> {
        lock(&self.calls)
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    /// Drops the client end, closing the connection.
    fn leave(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn spawn_owner(handler: Handler) -> (AgentPeer, Owner) {
    let (host_r, owner_w) = std::io::pipe().unwrap();
    let (owner_r, host_w) = std::io::pipe().unwrap();
    let (host, host_inbox, _) = kage_jsonrpc::connect(BufReader::new(host_r), host_w);
    let (owner_peer, inbox, _) = kage_jsonrpc::connect(BufReader::new(owner_r), owner_w);
    let calls = Calls::default();
    let stop = Arc::new(AtomicBool::new(false));
    let (record, halt) = (Arc::clone(&calls), Arc::clone(&stop));
    std::thread::spawn(move || {
        let _keep = host_inbox;
        while !halt.load(Ordering::SeqCst) {
            let Ok(Inbound::Request { id, method, params }) =
                inbox.recv_timeout(Duration::from_millis(10))
            else {
                continue;
            };
            lock(&record).push((method.clone(), params.clone()));
            if let Some(answer) = handler(&method, &params) {
                let _ = owner_peer.respond(&id, answer);
            }
        }
    });
    (host.into(), Owner { calls, stop })
}

fn snapshot() -> Value {
    json!({
        "tools": [{
            "name": "echo_client",
            "description": "Echoes",
            "schema": { "type": "object" },
        }],
        "handlers": { "message_start": 1, "tool_call": 1 },
    })
}

/// An owner that serves `snapshot` and answers everything else with
/// `reply`.
fn owner_answering(reply: Value) -> (ClientPlugins, Owner) {
    let (peer, owner) = spawn_owner(Box::new(move |method, _| {
        Some(Ok(if method == PLUGIN_TOOLS {
            snapshot()
        } else {
            reply.clone()
        }))
    }));
    let plugins = ClientPlugins::open_with(SessionId::new(), peer, LONG).unwrap();
    (plugins, owner)
}

/// An owner that serves `snapshot` and then never answers.
fn silent_owner(timeout: Duration) -> (ClientPlugins, Owner) {
    let (peer, owner) = spawn_owner(Box::new(|method, _| {
        (method == PLUGIN_TOOLS).then(|| Ok(snapshot()))
    }));
    let plugins = ClientPlugins::open_with(SessionId::new(), peer, timeout).unwrap();
    (plugins, owner)
}

#[derive(Default)]
struct Lines(Arc<Mutex<Vec<String>>>);

impl HostLog for Lines {
    fn notify(&mut self, _message: &str) {}

    fn log(&mut self, level: LogLevel, message: &str) {
        lock(&self.0).push(format!("{level:?}: {message}"));
    }
}

/// Routes the proxy's host log into the returned buffer.
fn capture_log(plugins: &mut ClientPlugins) -> Arc<Mutex<Vec<String>>> {
    let lines = Lines::default();
    let seen = Arc::clone(&lines.0);
    plugins.sink = Arc::new(Mutex::new(Box::new(lines)));
    seen
}

fn wait_closed(plugins: &ClientPlugins) {
    let deadline = Instant::now() + LONG;
    while plugins.peer.notify("ping", Value::Null).is_ok() {
        assert!(Instant::now() < deadline, "the connection never closed");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn run_tool(plugins: &ClientPlugins, cancel: &CancelFlag) -> Result<ToolOutput, ToolError> {
    let tool = plugins.registered_tools().remove(0);
    let dir = std::env::temp_dir();
    tool.execute(json!({ "text": "ping" }), &ToolContext::new(&dir, cancel))
}

#[test]
fn a_gone_owner_fails_tools_and_passes_hooks_and_drains() {
    let (plugins, owner) = owner_answering(Value::Null);
    owner.leave();
    wait_closed(&plugins);

    let err = run_tool(&plugins, &CancelFlag::new()).unwrap_err();
    assert!(matches!(err, ToolError::Other(_)), "{err}");
    let payload = json!({ "n": 1 });
    assert_eq!(
        plugins
            .dispatch_transform("message_start", payload.clone())
            .unwrap(),
        payload
    );
    assert!(
        !plugins
            .dispatch_predicate("tool_call", &Value::Null)
            .unwrap()
    );
    plugins.dispatch_event("message_start", &payload).unwrap();
    plugins.notify_event("message_start", &payload).unwrap();
    assert!(plugins.take_mcp_restarts().is_empty());
    assert!(plugins.take_pending_messages().is_empty());
    assert!(plugins.take_pending_session_ops().is_empty());
    assert!(
        plugins
            .take_pending_session_ops_for(SessionId::new())
            .is_empty()
    );
}

#[test]
fn a_transform_hook_replaces_the_payload_and_null_keeps_it() {
    let (plugins, owner) = owner_answering(json!({ "n": 2 }));
    let out = plugins
        .dispatch_transform("message_start", json!({ "n": 1 }))
        .unwrap();
    assert_eq!(out, json!({ "n": 2 }));
    let hook = &owner.calls_to(PLUGIN_HOOK)[0];
    assert_eq!(hook["kind"], "transform");
    assert_eq!(hook["event"], "message_start");
    assert_eq!(hook["payload"], json!({ "n": 1 }));

    let (plugins, _owner) = owner_answering(Value::Null);
    let out = plugins
        .dispatch_transform("message_start", json!({ "n": 1 }))
        .unwrap();
    assert_eq!(out, json!({ "n": 1 }), "null is no change");
}

#[test]
fn a_predicate_hook_reads_the_boolean_reply() {
    let (plugins, owner) = owner_answering(json!(true));
    assert!(
        plugins
            .dispatch_predicate("tool_call", &json!({ "a": 1 }))
            .unwrap()
    );
    assert_eq!(owner.calls_to(PLUGIN_HOOK)[0]["kind"], "predicate");

    let (plugins, _owner) = owner_answering(json!(false));
    assert!(
        !plugins
            .dispatch_predicate("tool_call", &Value::Null)
            .unwrap()
    );

    let (plugins, _owner) = owner_answering(json!("yes"));
    let err = plugins
        .dispatch_predicate("tool_call", &Value::Null)
        .unwrap_err();
    assert!(err.to_string().contains("predicate"), "{err}");
}

#[test]
fn hooks_skip_events_without_a_reported_handler() {
    let (plugins, owner) = owner_answering(Value::Null);
    plugins.dispatch_event("turn_end", &Value::Null).unwrap();
    plugins.notify_event("turn_end", &Value::Null).unwrap();
    plugins.dispatch_transform("turn_end", json!(1)).unwrap();
    assert!(
        !plugins
            .dispatch_predicate("turn_end", &Value::Null)
            .unwrap()
    );
    assert!(owner.calls_to(PLUGIN_EVENT).is_empty());
    assert!(owner.calls_to(PLUGIN_HOOK).is_empty());
}

#[test]
fn each_drain_asks_for_its_own_queue_once_and_maps_its_field() {
    let reply = json!({
        "restarts": ["srv"],
        "messages": [{ "text": "hi", "triggerTurn": true, "deliverAs": "assistant" }],
        "ops": [
            { "op": "append_custom", "kind": "p:k", "data": { "a": 1 } },
            { "op": "set_label", "anchor": "e1", "text": "l" },
        ],
    });
    let (plugins, owner) = owner_answering(reply);
    let session = plugins.session_id.clone();

    assert_eq!(plugins.take_mcp_restarts(), ["srv"]);
    let messages = plugins.take_pending_messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "hi");
    assert!(messages[0].trigger_turn);
    assert!(matches!(messages[0].deliver_as, PendingRole::Assistant));
    let ops = plugins.take_pending_session_ops();
    assert!(matches!(&ops[0], PendingSessionOp::AppendCustom { kind, .. } if kind == "p:k"));
    assert!(
        matches!(&ops[1], PendingSessionOp::SetLabel { anchor, text } if anchor == "e1" && text == "l")
    );
    let other = SessionId::new();
    assert_eq!(plugins.take_pending_session_ops_for(other).len(), 2);

    let drains = owner.calls_to(PLUGIN_DRAIN);
    let asked = drains
        .iter()
        .map(|d| {
            (
                d["queue"].as_str().unwrap(),
                d["sessionId"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        asked,
        [
            ("mcp_restarts", session.as_str()),
            ("messages", session.as_str()),
            ("session_ops_for", session.as_str()),
            ("session_ops_for", other.to_string().as_str()),
        ],
        "one request per drain, and none drains across sessions"
    );
}

#[test]
fn a_tool_reply_keeps_is_error_and_structured_output() {
    let (plugins, owner) = owner_answering(json!({
        "text": "bad input",
        "isError": true,
        "structured": { "code": 3 },
    }));
    let out = run_tool(&plugins, &CancelFlag::new()).unwrap();
    assert!(out.is_error);
    assert_eq!(out.text, "bad input");
    assert_eq!(out.structured, Some(json!({ "code": 3 })));
    assert!(!out.terminate);
    let call = &owner.calls_to(PLUGIN_CALL)[0];
    assert_eq!(call["tool"], "echo_client");
    assert_eq!(call["sessionId"], plugins.session_id);
}

#[test]
fn a_malformed_or_failed_tool_reply_is_a_tool_error() {
    let (plugins, _owner) = owner_answering(json!({ "text": 5 }));
    let err = run_tool(&plugins, &CancelFlag::new()).unwrap_err();
    assert!(
        matches!(&err, ToolError::Other(m) if m.contains("echo_client")),
        "{err}"
    );

    let (peer, _owner) = spawn_owner(Box::new(|method, _| {
        Some(if method == PLUGIN_TOOLS {
            Ok(snapshot())
        } else {
            Err(RpcError::internal("boom"))
        })
    }));
    let plugins = ClientPlugins::open_with(SessionId::new(), peer, LONG).unwrap();
    let err = run_tool(&plugins, &CancelFlag::new()).unwrap_err();
    assert!(
        matches!(&err, ToolError::Other(m) if m.contains("boom")),
        "{err}"
    );
}

#[test]
fn opening_fails_when_the_owner_errors_or_is_malformed() {
    let (peer, _owner) = spawn_owner(Box::new(|_, _| Some(Err(RpcError::internal("no tools")))));
    let err = ClientPlugins::open_with(SessionId::new(), peer, LONG).unwrap_err();
    assert!(err.message.contains("no tools"), "{err}");

    let (peer, _owner) = spawn_owner(Box::new(|_, _| Some(Ok(json!({ "tools": 3 })))));
    let err = ClientPlugins::open_with(SessionId::new(), peer, LONG).unwrap_err();
    assert!(err.message.contains("plugin tools"), "{err}");
}

#[test]
fn opening_times_out_on_a_silent_owner() {
    let (peer, _owner) = spawn_owner(Box::new(|_, _| None));
    let started = Instant::now();
    let err = ClientPlugins::open_with(SessionId::new(), peer, SHORT).unwrap_err();
    assert!(err.message.contains("did not answer"), "{err}");
    assert!(err.message.contains(PLUGIN_TOOLS), "{err}");
    assert!(started.elapsed() < LONG);
}

fn open_with_risk(risk: Option<&str>) -> Result<Vec<Arc<dyn Tool>>, RpcError> {
    let mut tools = snapshot();
    if let Some(risk) = risk {
        tools["tools"][0]["risk"] = json!(risk);
    }
    let (peer, _owner) = spawn_owner(Box::new(move |_, _| Some(Ok(tools.clone()))));
    ClientPlugins::open_with(SessionId::new(), peer, LONG).map(|p| p.registered_tools())
}

#[test]
fn a_tool_with_an_unknown_risk_is_refused_at_open() {
    let err = open_with_risk(Some("harmless")).map(|_| ()).unwrap_err();
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("echo_client"), "{err}");
    assert!(err.message.contains("harmless"), "{err}");
}

#[test]
fn known_risks_and_an_unset_one_are_kept() {
    assert_eq!(open_with_risk(None).unwrap()[0].risk(), Risk::Read);
    for (name, risk) in [
        ("read", Risk::Read),
        ("write", Risk::Write),
        ("exec", Risk::Exec),
        ("network", Risk::Network),
    ] {
        assert_eq!(open_with_risk(Some(name)).unwrap()[0].risk(), risk);
    }
}

#[test]
fn a_silent_owner_degrades_hooks_and_drains_with_a_warning() {
    let (mut plugins, _owner) = silent_owner(SHORT);
    let lines = capture_log(&mut plugins);
    let payload = json!({ "n": 1 });
    assert_eq!(
        plugins
            .dispatch_transform("message_start", payload.clone())
            .unwrap(),
        payload
    );
    assert!(!plugins.dispatch_predicate("tool_call", &payload).unwrap());
    plugins.dispatch_event("message_start", &payload).unwrap();
    assert!(plugins.take_mcp_restarts().is_empty());
    assert!(plugins.take_pending_session_ops().is_empty());
    let lines = lock(&lines).clone();
    assert!(
        lines.len() >= 5 && lines.iter().all(|l| l.contains("did not answer")),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains(PLUGIN_HOOK)));
    assert!(lines.iter().any(|l| l.contains(PLUGIN_EVENT)));
    assert!(lines.iter().any(|l| l.contains(PLUGIN_DRAIN)));
}

#[test]
fn a_tool_call_is_cancelled_mid_flight() {
    let (plugins, owner) = silent_owner(LONG);
    let cancel = CancelFlag::new();
    let trip = cancel.clone();
    let (called, saw_call) = mpsc::channel();
    let watcher = std::thread::spawn(move || {
        let deadline = Instant::now() + LONG;
        while owner.calls_to(PLUGIN_CALL).is_empty() {
            assert!(
                Instant::now() < deadline,
                "the call never reached the owner"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        called.send(()).unwrap();
        trip.cancel();
        owner
    });
    let started = Instant::now();
    let err = run_tool(&plugins, &cancel).unwrap_err();
    assert!(matches!(err, ToolError::Cancelled), "{err}");
    assert!(started.elapsed() < LONG);
    saw_call.recv().unwrap();
    watcher.join().unwrap();
}

#[test]
fn a_cancelled_flag_stops_a_tool_before_it_asks() {
    let (plugins, owner) = silent_owner(LONG);
    let cancel = CancelFlag::new();
    cancel.cancel();
    let err = run_tool(&plugins, &cancel).unwrap_err();
    assert!(matches!(err, ToolError::Cancelled), "{err}");
    assert!(owner.calls_to(PLUGIN_CALL).is_empty());
}
