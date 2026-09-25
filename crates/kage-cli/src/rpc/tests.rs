use std::path::Path;
use std::time::Duration;

use kage_acp::acp::{McpServer, ToolCallStatus, ToolKind};
use kage_core::agents::AgentDefs;
use kage_core::permissions::{PermissionsConfig, ToolPermissionRules};
use kage_core::protocol::{McpServerInfo, McpServerStatus, Usage};
use kage_core::{
    Content, ImageSource, LoopEvent, Message, MessageId, Role, ThinkingLevel, TokenUsage,
    ToolCallId, ToolOutput, ToolUpdate,
};
use kage_jsonrpc::Inbound;
use kage_mcp::McpError;
use kage_provider::testing::MockProvider;
use kage_provider::{ProviderError, ProviderEvent};
use kage_session::{EntryId, FORMAT_VERSION, Header, MessageEntry, SessionEntry, SessionWriter};

use crate::engine::Commander;

use super::bridge::{to_update, tool_kind, usage_update};
use super::mcp::prompt_commands;
use super::*;

const WAIT: Duration = Duration::from_secs(5);

type Script = Vec<Result<ProviderEvent, ProviderError>>;

fn tool_turn(id: &str, name: &str, input: serde_json::Value) -> Script {
    let id = ToolCallId::new(id);
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: name.into(),
        }),
        Ok(ProviderEvent::ToolCallEnd { id, input }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: CoreStopReason::ToolUse,
            usage: TokenUsage::default(),
        }),
    ]
}

fn text_turn(text: &str) -> Script {
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::TextDelta { delta: text.into() }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: CoreStopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
    ]
}

const WINDOW: u64 = 1000;

/// The client side of a served `kage rpc`, the provider its sessions
/// call, and the id of the session open from the start.
struct Harness {
    client: Peer,
    inbox: mpsc::Receiver<Inbound>,
    mock: MockProvider,
    commander: Commander,
    id: SessionId,
    session: String,
}

impl Harness {
    /// Sends `kind` to the open session the way a non-client change
    /// would reach the engine.
    fn command(&self, kind: CommandKind) {
        self.commander.send(Command::to(self.id, kind));
    }
}

/// The mock provider, offering `mock:m` and `mock:other` to pickers.
#[derive(Debug)]
struct Listed(MockProvider);

impl kage_provider::Provider for Listed {
    fn metadata(&self) -> &kage_provider::ProviderMetadata {
        self.0.metadata()
    }

    fn stream(
        &self,
        req: kage_provider::StreamRequest,
        cancel: &kage_core::CancelFlag,
    ) -> Result<kage_provider::EventStream, ProviderError> {
        self.0.stream(req, cancel)
    }

    fn models(&self) -> Vec<kage_provider::ProviderModel> {
        ["m", "other"]
            .map(|id| kage_provider::ProviderModel {
                id: id.into(),
                name: format!("Mock {id}"),
                ..kage_provider::ProviderModel::default()
            })
            .into()
    }
}

/// Serves `kage rpc` over pipes on the sessions recorded in
/// `sessions`, with one open session. Every session runs in
/// `workdir`, asks before `ls` calls and generates a title.
fn serve(scripts: Vec<Script>, workdir: &Path, sessions: &Path) -> Harness {
    serve_with(scripts, workdir, sessions, false)
}

/// [`serve`], where every session also has the MCP server of
/// [`mcp_connection`] as `srv` when `mcp` is set. Servers a client
/// passes are spawned, and the tools of every server ask.
fn serve_with(scripts: Vec<Script>, workdir: &Path, sessions: &Path, mcp: bool) -> Harness {
    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let id = SessionId::new();
    let workdir = workdir.to_path_buf();
    let sessions = sessions.to_path_buf();
    let mock = MockProvider::sequence(scripts);
    let provider = Listed(mock.clone());
    let (commander_tx, commander) = mpsc::channel();
    std::thread::spawn(move || {
        serve_agent(BufReader::new(srv_r), srv_w, |peer| {
            let agent = test_agent(peer, provider, workdir, sessions, mcp);
            let spec = (agent.spec)(id, "", "mock:m", BTreeMap::new()).unwrap();
            agent.open(id.to_string(), spec);
            agent.session_announced(&id.to_string());
            let _ = commander_tx.send(agent.engine.commander());
            agent
        })
    });
    let (client, inbox, _reader) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
    Harness {
        client,
        inbox,
        mock,
        commander: commander.recv_timeout(WAIT).unwrap(),
        id,
        session: id.to_string(),
    }
}

/// The agent [`serve_with`] serves, on `provider`.
fn test_agent(
    peer: Peer,
    provider: Listed,
    workdir: PathBuf,
    sessions: PathBuf,
    mcp: bool,
) -> CliAcpAgent {
    let registry = Arc::new(ProviderRegistry::new().with(Arc::new(provider)));
    let spec = Box::new(move |id, _cwd: &str, model: &str, servers| {
        let mut rules = PermissionsConfig::default();
        rules.tools.insert(
            "ls".into(),
            ToolPermissionRules {
                default: PermissionAction::Ask,
                allow: Vec::new(),
                deny: Vec::new(),
            },
        );
        let mut tools = builtin_registry();
        let mcp = mcp_manager(&mut tools, servers, mcp);
        let names = mcp.iter().flat_map(kage_mcp::McpManager::server_names);
        let gate = PermissionGate::new(rules).with_mcp_servers(names.map(str::to_owned).collect());
        Ok(SessionSpec {
            id,
            model: model.to_owned(),
            cx: AgentContext::new(model, "")
                .with_workdir(&workdir)
                .with_context_window(WINDOW),
            recorder: None,
            tools,
            gate,
            loop_cfg: LoopConfig::default(),
            plugins: None,
            mcp,
            interactive: true,
            title: true,
            agents: Some(AgentSetup {
                defs: Arc::new(AgentDefs::builtin()),
                max_depth: 1,
                max_running: 4,
            }),
        })
    });
    CliAcpAgent::new(registry, "mock:m".into(), sessions, spec, peer)
}

/// An in-process MCP server with the prompt `p(a, b?)`, which answers
/// `p a=<a>`.
fn mcp_connection() -> Arc<kage_mcp::McpConnection> {
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (cli_peer, cli_in, _c) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
    let (srv_peer, srv_in, _s) = kage_jsonrpc::connect(BufReader::new(srv_r), srv_w);
    std::thread::spawn(move || {
        for msg in srv_in {
            let Inbound::Request { id, method, params } = msg else {
                continue;
            };
            let outcome = match method.as_str() {
                "initialize" => Ok(serde_json::json!({
                    "protocolVersion": kage_mcp::PROTOCOL_VERSION,
                    "capabilities": { "tools": {}, "prompts": {} },
                })),
                "tools/list" => Ok(serde_json::json!({ "tools": [] })),
                "prompts/list" => Ok(serde_json::json!({ "prompts": [{
                    "name": "p",
                    "description": "Run p",
                    "arguments": [{ "name": "a", "required": true }, { "name": "b" }],
                }] })),
                "prompts/get" => Ok(serde_json::json!({ "messages": [{
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": format!("p a={}", params["arguments"]["a"].as_str().unwrap()),
                    },
                }] })),
                other => Err(RpcError::method_not_found(other)),
            };
            let _ = srv_peer.respond(&id, outcome);
        }
    });
    let conn = kage_mcp::McpConnection::initialize("srv", cli_peer, cli_in, &[], None).unwrap();
    Arc::new(conn)
}

/// The manager of `servers`, with the server of [`mcp_connection`] as
/// `srv` when `adopt` is set, or `None` when it would have no server.
fn mcp_manager(
    tools: &mut kage_tools::ToolRegistry,
    servers: BTreeMap<String, McpSpec>,
    adopt: bool,
) -> Option<kage_mcp::McpManager> {
    if servers.is_empty() && !adopt {
        return None;
    }
    let cfg = kage_core::config::McpConfig {
        servers,
        ..kage_core::config::McpConfig::default()
    };
    let (mut mcp, errors) = kage_mcp::McpManager::spawn_all(&cfg, Vec::new(), None);
    assert!(errors.is_empty(), "{errors:?}");
    if adopt {
        mcp.adopt("srv", mcp_connection());
    }
    assert!(mcp.register_into(tools).is_empty());
    Some(mcp)
}

/// A stdio MCP server script in `dir` with the tool `show`, which
/// answers `ED_VAR=<$ED_VAR>`, and the prompt `greet`.
fn editor_server(dir: &Path) -> String {
    let script = dir.join("server.sh");
    let body = r#"while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *'"method":"initialize"'*)
      result='{"protocolVersion":"2025-06-18","capabilities":{"tools":{},"prompts":{}}}' ;;
    *'"method":"tools/list"'*)
      result='{"tools":[{"name":"show","inputSchema":{"type":"object"}}]}' ;;
    *'"method":"tools/call"'*)
      result="{\"content\":[{\"type\":\"text\",\"text\":\"ED_VAR=$ED_VAR\"}]}" ;;
    *'"method":"prompts/list"'*)
      result='{"prompts":[{"name":"greet","description":"Say hi"}]}' ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no"}}\n' "$id"
      continue ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#;
    std::fs::write(&script, body).unwrap();
    script.display().to_string()
}

/// The `mcpServers` entry of [`editor_server`] as `ed`, with `ED_VAR`
/// set to `hello`.
fn editor_entry(dir: &Path) -> serde_json::Value {
    serde_json::json!({
        "name": "ed",
        "command": "sh",
        "args": [editor_server(dir)],
        "env": [{"name": "ED_VAR", "value": "hello"}],
    })
}

/// Collects `session/update` params until a permission request
/// arrives, and returns them with that request's id and params.
fn until_ask(
    inbox: &mpsc::Receiver<Inbound>,
    updates: &mut Vec<serde_json::Value>,
) -> (serde_json::Value, serde_json::Value) {
    loop {
        match inbox.recv_timeout(WAIT).expect("no permission request") {
            Inbound::Notification { params, .. } => updates.push(params),
            Inbound::Request { id, method, params } => {
                assert_eq!(method, "session/request_permission");
                return (id, params);
            }
        }
    }
}

fn allow(client: &Peer, id: &serde_json::Value) {
    let outcome = serde_json::json!({"outcome": {"outcome": "selected", "optionId": "allow"}});
    client.respond(id, Ok(outcome)).unwrap();
}

fn contents_of(updates: &[serde_json::Value], call: &str) -> Vec<String> {
    updates
        .iter()
        .map(|p| &p["update"])
        .filter(|u| u["toolCallId"] == call)
        .filter_map(|u| u["content"][0]["content"]["text"].as_str())
        .map(str::to_owned)
        .collect()
}

#[test]
fn agent_asks_and_progress_reach_the_root_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let task = serde_json::json!({"description": "list files", "prompt": "list"});
    let Harness {
        client,
        inbox,
        session,
        ..
    } = serve(
        vec![
            tool_turn("call_agent", "agent", task),
            tool_turn("call_child", "ls", serde_json::json!({ "path": path })),
            text_turn("child done"),
            tool_turn("call_root", "ls", serde_json::json!({ "path": path })),
            text_turn("parent done"),
        ],
        dir.path(),
        dir.path(),
    );
    let (done, prompt_end) = mpsc::channel();
    let prompter = client.clone();
    let params = serde_json::json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": "go"}],
    });
    std::thread::spawn(move || {
        let _ = done.send(prompter.request("session/prompt", params));
    });

    let mut updates = Vec::new();
    let (child_ask, params) = until_ask(&inbox, &mut updates);
    assert_eq!(params["sessionId"], session);
    assert_eq!(params["toolCall"]["toolCallId"], "call_agent");
    assert_eq!(params["toolCall"]["title"], "general: ls");
    assert_eq!(params["toolCall"]["rawInput"]["path"], path);
    allow(&client, &child_ask);

    let (root_ask, params) = until_ask(&inbox, &mut updates);
    assert_eq!(params["sessionId"], session);
    assert_eq!(params["toolCall"]["toolCallId"], "call_root");
    assert!(prompt_end.recv_timeout(Duration::from_millis(100)).is_err());
    allow(&client, &root_ask);

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    while let Ok(Inbound::Notification { params, .. }) = inbox.try_recv() {
        updates.push(params);
    }
    assert!(updates.iter().all(|p| p["sessionId"] == session));
    let announced = updates
        .iter()
        .find(|p| p["update"]["toolCallId"] == "call_agent");
    assert_eq!(announced.unwrap()["update"]["sessionUpdate"], "tool_call");
    let progress = contents_of(&updates, "call_agent");
    assert_eq!(
        progress[..4],
        [
            format!("general: List {path}"),
            format!("general: Waiting for approval: List {path}"),
            format!("general: List {path}"),
            "general: done".to_owned(),
        ]
    );
    assert!(contents_of(&updates, "call_child").is_empty());
    assert!(progress[4].contains("child done"), "{progress:?}");
}

/// Initializes as a client that advertises subagents.
fn initialize(client: &Peer) {
    let params = serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {"subagents": {}},
    });
    client.request("initialize", params).unwrap();
}

/// Starts a `session/prompt` of `text` on `session` and returns where
/// its response arrives.
fn prompt_async(
    client: &Peer,
    session: &str,
    text: &str,
) -> mpsc::Receiver<Result<serde_json::Value, kage_jsonrpc::RpcError>> {
    let (done, end) = mpsc::channel();
    let client = client.clone();
    let params = serde_json::json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": text}],
    });
    std::thread::spawn(move || {
        let _ = done.send(client.request("session/prompt", params));
    });
    end
}

fn is_terminal(params: &serde_json::Value) -> bool {
    params["update"]["sessionUpdate"] == "subagent_update" && !params["update"]["state"].is_null()
}

/// Collects notifications with their methods until a subagent's
/// terminal update arrives.
fn until_terminal(inbox: &mpsc::Receiver<Inbound>) -> Vec<(String, serde_json::Value)> {
    let mut notes = Vec::new();
    loop {
        if let Inbound::Notification { method, params } =
            inbox.recv_timeout(WAIT).expect("no terminal update")
        {
            let done = is_terminal(&params);
            notes.push((method, params));
            if done {
                return notes;
            }
        }
    }
}

#[test]
fn subagents_stream_and_ask_on_their_own_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let task = serde_json::json!({"description": "list files", "prompt": "list"});
    let h = serve(
        vec![
            tool_turn("call_agent", "agent", task),
            tool_turn("call_child", "ls", serde_json::json!({ "path": path })),
            text_turn("child done"),
            text_turn("parent done"),
        ],
        dir.path(),
        dir.path(),
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let mut updates = Vec::new();
    let (ask, params) = until_ask(&h.inbox, &mut updates);
    let announced = updates
        .iter()
        .find(|p| p["update"]["sessionUpdate"] == "subagent_update")
        .expect("subagent announced before its ask");
    assert_eq!(announced["sessionId"], h.session);
    let child = announced["update"]["subagentSessionId"].clone();
    assert_eq!(announced["update"]["name"], "general");
    assert_eq!(announced["update"]["task"], "list files");
    assert_eq!(announced["update"]["capabilities"]["cancel"], true);
    assert!(announced["update"].get("state").is_none());
    assert_eq!(params["sessionId"], child);
    assert_eq!(params["toolCall"]["toolCallId"], "call_child");
    assert_eq!(params["toolCall"]["title"], "ls");
    allow(&h.client, &ask);

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    updates.extend(drain(&h.inbox));
    let terminal = updates
        .iter()
        .position(is_terminal)
        .expect("terminal update before the prompt answer");
    assert_eq!(updates[terminal]["sessionId"], h.session);
    assert_eq!(updates[terminal]["update"]["subagentSessionId"], child);
    assert_eq!(updates[terminal]["update"]["state"], "completed");
    let last_child = updates.iter().rposition(|p| p["sessionId"] == child);
    assert!(last_child < Some(terminal), "{updates:#?}");
    let own: Vec<_> = updates.iter().filter(|p| p["sessionId"] == child).collect();
    assert!(
        own.iter()
            .any(|p| p["update"]["toolCallId"] == "call_child")
    );
    assert!(
        own.iter()
            .any(|p| p["update"]["content"]["text"] == "child done")
    );
    assert!(
        updates
            .iter()
            .all(|p| p["sessionId"] == h.session || p["sessionId"] == child)
    );
    let root_call = contents_of(&updates, "call_agent");
    assert_eq!(root_call.len(), 1, "{root_call:?}");
    assert!(root_call[0].contains("child done"));

    let refused = h
        .client
        .request(
            "session/prompt",
            serde_json::json!({"sessionId": child, "prompt": []}),
        )
        .unwrap_err();
    assert_eq!(refused.code, -32602);
}

#[test]
fn cancelling_a_subagent_withdraws_its_ask_and_the_parent_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let task = serde_json::json!({"description": "list files", "prompt": "list"});
    let h = serve(
        vec![
            tool_turn("call_agent", "agent", task),
            tool_turn("call_child", "ls", serde_json::json!({ "path": path })),
            text_turn("parent done"),
        ],
        dir.path(),
        dir.path(),
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let (ask, params) = until_ask(&h.inbox, &mut Vec::new());
    let child = params["sessionId"].clone();
    assert_ne!(child, h.session);
    h.client
        .notify("session/cancel", serde_json::json!({"sessionId": child}))
        .unwrap();

    let notes = until_terminal(&h.inbox);
    let withdrawn = notes
        .iter()
        .position(|(method, p)| method == "$/cancel_request" && p["requestId"] == ask);
    assert!(withdrawn.is_some(), "{notes:#?}");
    let (_, terminal) = notes.last().unwrap();
    assert_eq!(terminal["sessionId"], h.session);
    assert_eq!(terminal["update"]["subagentSessionId"], child);
    assert_eq!(terminal["update"]["state"], "cancelled");
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
}

#[test]
fn a_cancelled_prompt_answers_after_its_subagents_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let task = serde_json::json!({"description": "list files", "prompt": "list"});
    let h = serve(
        vec![
            tool_turn("call_agent", "agent", task),
            tool_turn("call_child", "ls", serde_json::json!({ "path": path })),
        ],
        dir.path(),
        dir.path(),
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let (ask, params) = until_ask(&h.inbox, &mut Vec::new());
    let child = params["sessionId"].clone();
    h.client
        .notify(
            "session/cancel",
            serde_json::json!({"sessionId": h.session}),
        )
        .unwrap();

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "cancelled");
    let notes: Vec<_> = std::iter::from_fn(|| h.inbox.try_recv().ok())
        .filter_map(|message| match message {
            Inbound::Notification { method, params } => Some((method, params)),
            Inbound::Request { .. } => None,
        })
        .collect();
    let terminal = notes.iter().position(|(_, p)| is_terminal(p));
    let withdrawn = notes
        .iter()
        .position(|(method, p)| method == "$/cancel_request" && p["requestId"] == ask);
    assert!(withdrawn.is_some() && withdrawn < terminal, "{notes:#?}");
    let (_, terminal) = &notes[terminal.unwrap()];
    assert_eq!(terminal["update"]["subagentSessionId"], child);
    assert_eq!(terminal["update"]["state"], "cancelled");
}

#[test]
fn initialize_advertises_image_and_embedded_context() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());
    let params = serde_json::json!({"protocolVersion": PROTOCOL_VERSION, "clientCapabilities": {}});
    let init = h.client.request("initialize", params).unwrap();
    let caps = &init["agentCapabilities"]["promptCapabilities"];
    assert_eq!(caps["image"], true);
    assert_eq!(caps["embeddedContext"], true);
    assert_eq!(caps["audio"], false);
}

#[test]
fn prompt_blocks_reach_the_engine_as_content() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("ok")], dir.path(), dir.path());
    let params = serde_json::json!({
        "sessionId": h.session,
        "prompt": [
            {"type": "text", "text": "review these"},
            {"type": "resource", "resource": {
                "uri": "file:///w/a.rs", "mimeType": "text/rust", "text": "fn a() {}",
            }},
            {"type": "resource_link", "uri": "file:///w/b.rs", "name": "b.rs"},
            {"type": "resource_link", "uri": "https://example.com/doc", "name": "doc"},
            {"type": "image", "data": "aGk=", "mimeType": "image/png"},
            {"type": "resource", "resource": {
                "uri": "file:///w/c.png", "mimeType": "image/png", "blob": "aGk=",
            }},
            {"type": "resource", "resource": {"uri": "file:///w/d.bin", "blob": "AAAA"}},
            {"type": "audio", "data": "AAAA", "mimeType": "audio/wav"},
        ],
    });
    let response = h.client.request("session/prompt", params).unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let image = || Content::Image {
        source: ImageSource::Base64 {
            data: "aGk=".into(),
        },
        mime: "image/png".into(),
    };
    let request = h.mock.requests().swap_remove(0);
    assert_eq!(
        request.messages.last().unwrap().content,
        [
            text("review these"),
            text("<resource uri=\"file:///w/a.rs\" mime=\"text/rust\">\nfn a() {}\n</resource>"),
            text("Referenced file: /w/b.rs"),
            text("Referenced resource: https://example.com/doc (doc)"),
            image(),
            image(),
            text("[binary resource file:///w/d.bin: application/octet-stream]"),
            text("[audio omitted]"),
        ]
    );
}

#[test]
fn allow_for_this_session_stops_the_next_identical_ask() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let input = serde_json::json!({ "path": path });
    let h = serve(
        vec![
            tool_turn("call_1", "ls", input.clone()),
            tool_turn("call_2", "ls", input),
            text_turn("done"),
        ],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let (ask, params) = until_ask(&h.inbox, &mut Vec::new());
    let options: Vec<_> = params["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| (o["optionId"].as_str().unwrap(), o["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        options,
        [
            ("allow", "allow_once"),
            ("allow_session", "allow_always"),
            ("reject", "reject_once"),
        ]
    );
    assert_eq!(params["options"][1]["name"], "Allow ls for this session");
    let outcome =
        serde_json::json!({"outcome": {"outcome": "selected", "optionId": "allow_session"}});
    h.client.respond(&ask, Ok(outcome)).unwrap();

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let asked_again = std::iter::from_fn(|| h.inbox.try_recv().ok())
        .any(|message| matches!(message, Inbound::Request { .. }));
    assert!(!asked_again);
    let turn = h.mock.requests().swap_remove(2);
    let results: Vec<_> = turn
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResultBlock { is_error, .. } => Some(*is_error),
            _ => None,
        })
        .collect();
    assert_eq!(results, [false, false]);
}

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input,
        output,
        ..TokenUsage::default()
    }
}

fn message(role: Role, content: Vec<Content>, usage: Option<TokenUsage>) -> SessionEntry {
    SessionEntry::Message(MessageEntry {
        id: EntryId::new(),
        ts: chrono::Utc::now(),
        message: Message::new(role, content, None),
        usage,
    })
}

fn text(text: &str) -> Content {
    Content::Text { text: text.into() }
}

/// Records a session in `dir` created in `cwd` on `model`, `age`
/// minutes ago, and returns its id.
fn record(dir: &Path, cwd: &str, model: &str, age: i64, entries: &[SessionEntry]) -> String {
    let session = kage_session::SessionId::new();
    let header = Header {
        version: FORMAT_VERSION,
        session,
        id: EntryId::new(),
        ts: chrono::Utc::now() - chrono::Duration::minutes(age),
        cwd: cwd.into(),
        model: model.into(),
        system_prompt: String::new(),
        parent_session: None,
        parent_entry: None,
    };
    let path = crate::build_session_path(dir, session);
    let mut writer = SessionWriter::create(&path, header).unwrap();
    for entry in entries {
        writer.append(entry).unwrap();
    }
    session.to_string()
}

/// Collects the `session/update` params of `session` until one of
/// `kind` arrives, and returns them all.
fn updates_until(
    inbox: &mpsc::Receiver<Inbound>,
    session: &str,
    kind: &str,
) -> Vec<serde_json::Value> {
    let mut updates = Vec::new();
    loop {
        if let Inbound::Notification { params, .. } = inbox.recv_timeout(WAIT).expect("no update")
            && params["sessionId"] == session
        {
            let done = params["update"]["sessionUpdate"] == kind;
            updates.push(params);
            if done {
                return updates;
            }
        }
    }
}

fn drain(inbox: &mpsc::Receiver<Inbound>) -> Vec<serde_json::Value> {
    let mut updates = Vec::new();
    while let Ok(Inbound::Notification { params, .. }) = inbox.try_recv() {
        updates.push(params);
    }
    updates
}

fn update_kinds(updates: &[serde_json::Value]) -> Vec<&str> {
    updates
        .iter()
        .filter_map(|p| p["update"]["sessionUpdate"].as_str())
        .collect()
}

fn prompt(client: &Peer, session: &str, text: &str) -> serde_json::Value {
    let params = serde_json::json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": text}],
    });
    client.request("session/prompt", params).unwrap()
}

fn listed(client: &Peer, params: serde_json::Value) -> (Vec<String>, serde_json::Value) {
    let page = client.request("session/list", params).unwrap();
    let ids = page["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["sessionId"].as_str().unwrap().to_owned())
        .collect();
    (ids, page["nextCursor"].clone())
}

#[test]
fn session_list_hides_agents_filters_by_cwd_and_orders_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let title = SessionEntry::Title(kage_session::SessionTitle {
        id: EntryId::new(),
        ts: chrono::Utc::now() - chrono::Duration::minutes(1),
        title: "Fix the build".into(),
    });
    let marker = SessionEntry::Custom(kage_session::Custom {
        id: EntryId::new(),
        ts: chrono::Utc::now(),
        kind: kage_session::list::AGENT_ENTRY_KIND.into(),
        data: serde_json::json!({ "agent": "explore" }),
    });
    let recent = record(sessions, "/p", "mock:m", 30, &[title]);
    let old = record(sessions, "/p", "mock:m", 10, &[]);
    record(sessions, "/p", "mock:m", 0, &[marker]);
    let elsewhere = record(sessions, "/q", "mock:m", 5, &[]);
    let h = serve(Vec::new(), sessions, sessions);

    let page = h
        .client
        .request("session/list", serde_json::json!({ "cwd": "/p" }))
        .unwrap();
    assert_eq!(page["sessions"][0]["sessionId"], recent);
    assert_eq!(page["sessions"][0]["cwd"], "/p");
    assert_eq!(page["sessions"][0]["title"], "Fix the build");
    assert!(
        page["sessions"][0]["updatedAt"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    let (ids, next) = listed(&h.client, serde_json::json!({ "cwd": "/p" }));
    assert_eq!(ids, [recent.clone(), old.clone()]);
    assert!(next.is_null());
    let (ids, _) = listed(&h.client, serde_json::json!({}));
    assert_eq!(ids, [recent, elsewhere, old]);
}

#[test]
fn session_list_pages_with_a_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let ids: Vec<String> = (0..52)
        .map(|age| record(sessions, "/p", "mock:m", age, &[]))
        .collect();
    let h = serve(Vec::new(), sessions, sessions);

    let (first, next) = listed(&h.client, serde_json::json!({}));
    assert_eq!(first, ids[..50]);
    assert_eq!(next, "50");
    let (rest, next) = listed(&h.client, serde_json::json!({ "cursor": "50" }));
    assert_eq!(rest, ids[50..]);
    assert!(next.is_null());
    let bad = h
        .client
        .request("session/list", serde_json::json!({ "cursor": "x" }))
        .unwrap_err();
    assert_eq!(bad.code, -32602);
}

#[test]
fn session_load_replays_the_whole_transcript_and_restores_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let call = ToolCallId::new("call_1");
    let session = record(
        dir.path(),
        &cwd,
        "mock:recorded",
        1,
        &[
            SessionEntry::ThinkingLevelChange(kage_session::ThinkingLevelChange {
                id: EntryId::new(),
                ts: chrono::Utc::now(),
                level: "high".into(),
            }),
            message(Role::User, vec![text("list files")], None),
            message(
                Role::Assistant,
                vec![
                    Content::Thinking {
                        text: "look around".into(),
                        signature: None,
                    },
                    text("Listing."),
                    Content::ToolCall {
                        id: call.clone(),
                        name: "ls".into(),
                        input: serde_json::json!({ "path": "." }),
                    },
                ],
                Some(usage(300, 20)),
            ),
            message(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: call,
                    output: "a.txt".into(),
                    is_error: false,
                }],
                None,
            ),
            message(
                Role::Assistant,
                vec![text("Found a.txt.")],
                Some(usage(400, 10)),
            ),
            SessionEntry::Title(kage_session::SessionTitle {
                id: EntryId::new(),
                ts: chrono::Utc::now(),
                title: "Listing files".into(),
            }),
        ],
    );
    let h = serve(vec![text_turn("ok")], dir.path(), dir.path());

    let params = serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []});
    let loaded = h.client.request("session/load", params).unwrap();
    assert_eq!(
        current_values(&loaded),
        ["mock:recorded", "high", "default"]
    );
    let updates = updates_until(&h.inbox, &session, "usage_update");
    assert_eq!(
        update_kinds(&updates),
        [
            "user_message_chunk",
            "agent_thought_chunk",
            "agent_message_chunk",
            "tool_call",
            "tool_call_update",
            "agent_message_chunk",
            "session_info_update",
            "usage_update",
        ]
    );
    let update = |i: usize| &updates[i]["update"];
    assert_eq!(update(0)["content"]["text"], "list files");
    assert_eq!(update(1)["content"]["text"], "look around");
    assert_eq!(update(3)["toolCallId"], "call_1");
    assert_eq!(update(3)["rawInput"]["path"], ".");
    assert_eq!(update(4)["status"], "completed");
    assert_eq!(update(4)["content"][0]["content"]["text"], "a.txt");
    assert_eq!(update(5)["content"]["text"], "Found a.txt.");
    assert_eq!(update(6)["title"], "Listing files");
    assert_eq!(update(7)["used"], 410);
    assert_eq!(update(7)["size"], WINDOW);

    assert_eq!(
        prompt(&h.client, &session, "again")["stopReason"],
        "end_turn"
    );
    let request = h.mock.last_request().unwrap();
    assert_eq!(request.model, "recorded");
    assert_eq!(request.level, Some(kage_core::ThinkingLevel::High));
    assert_eq!(request.messages.len(), 5);
}

#[test]
fn session_resume_skips_the_replay_and_continues_the_history() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let session = record(
        dir.path(),
        &cwd,
        "mock:m",
        1,
        &[
            message(Role::User, vec![text("hello")], None),
            message(Role::Assistant, vec![text("hi")], None),
        ],
    );
    let h = serve(vec![text_turn("sure")], dir.path(), dir.path());

    let params = serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []});
    let resumed = h.client.request("session/resume", params).unwrap();
    assert_eq!(current_values(&resumed), ["mock:m", "default", "default"]);
    assert_eq!(
        prompt(&h.client, &session, "next")["stopReason"],
        "end_turn"
    );
    let updates = drain(&h.inbox);
    let kinds = update_kinds(&updates);
    assert!(!kinds.contains(&"user_message_chunk"), "{kinds:?}");
    let chunks: Vec<_> = updates
        .iter()
        .filter(|p| p["update"]["sessionUpdate"] == "agent_message_chunk")
        .map(|p| p["update"]["content"]["text"].clone())
        .collect();
    assert_eq!(chunks, ["sure"]);
    let texts: Vec<String> = h
        .mock
        .last_request()
        .unwrap()
        .messages
        .iter()
        .map(crate::cli_loop_run::first_user_text)
        .collect();
    assert_eq!(texts, ["hello", "hi", "next"]);
}

fn current_values(result: &serde_json::Value) -> Vec<&str> {
    result["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["currentValue"].as_str().unwrap())
        .collect()
}

fn values_of<'a>(option: &'a serde_json::Value, field: &str) -> Vec<&'a str> {
    option["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o[field].as_str().unwrap())
        .collect()
}

fn set_option(
    h: &Harness,
    id: &str,
    value: &str,
) -> Result<serde_json::Value, kage_jsonrpc::RpcError> {
    let params = serde_json::json!({"sessionId": h.session, "configId": id, "value": value});
    h.client.request("session/set_config_option", params)
}

#[test]
fn a_new_session_lists_model_thinking_and_mode() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());

    let params = serde_json::json!({"cwd": dir.path(), "mcpServers": []});
    let created = h.client.request("session/new", params).unwrap();
    assert_eq!(current_values(&created), ["mock:m", "default", "default"]);
    let options = created["configOptions"].as_array().unwrap();
    let ids: Vec<_> = options.iter().map(|o| o["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["model", "thinking", "mode"]);
    let categories: Vec<_> = options.iter().map(|o| o["category"].clone()).collect();
    assert_eq!(categories, ["model", "thought_level", "mode"]);
    assert!(options.iter().all(|o| o["type"] == "select"));
    assert_eq!(values_of(&options[0], "value"), ["mock:m", "mock:other"]);
    assert_eq!(values_of(&options[0], "name"), ["Mock m", "Mock other"]);
    assert_eq!(options[0]["options"][0]["description"], "Mock");
    assert_eq!(
        values_of(&options[1], "value"),
        [
            "default", "off", "minimal", "low", "medium", "high", "xhigh"
        ]
    );
    assert_eq!(
        values_of(&options[2], "value"),
        ["default", "ask", "allow", "deny"]
    );
}

#[test]
fn the_thinking_option_offers_default_and_the_model_levels() {
    use ThinkingLevel::{High, Low};
    let mut settings = Settings {
        model: "m".into(),
        thinking: None,
        levels: vec![Low, High],
        mode: None,
    };
    let options = config_options(&[], &settings);
    let values: Vec<&str> = options[1]
        .options
        .iter()
        .map(|o| o.value.as_str())
        .collect();
    assert_eq!(values, ["default", "low", "high"]);
    assert_eq!(options[1].current_value, "default");
    assert!(settings.apply(&[], "thinking", "medium").is_err());
    assert_eq!(
        settings.apply(&[], "thinking", "high").unwrap(),
        CommandKind::SetThinking { level: Some(High) }
    );
    assert_eq!(
        settings.apply(&[], "thinking", "default").unwrap(),
        CommandKind::SetThinking { level: None }
    );
    assert_eq!(settings.thinking, None);
}

#[test]
fn setting_options_changes_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("ok")], dir.path(), dir.path());

    let set = set_option(&h, "thinking", "high").unwrap();
    assert_eq!(current_values(&set), ["mock:m", "high", "default"]);
    set_option(&h, "model", "mock:other").unwrap();
    let set = set_option(&h, "mode", "ask").unwrap();
    assert_eq!(current_values(&set), ["mock:other", "high", "ask"]);

    assert_eq!(
        prompt(&h.client, &h.session, "hi")["stopReason"],
        "end_turn"
    );
    let request = &h.mock.requests()[0];
    assert_eq!(request.model, "other");
    assert_eq!(request.level, Some(ThinkingLevel::High));
    let kinds = update_kinds(&drain(&h.inbox)).join(" ");
    assert!(!kinds.contains("config_option_update"), "{kinds}");
}

#[test]
fn unknown_options_and_values_are_invalid_params() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());

    for (id, value) in [
        ("colour", "red"),
        ("model", "mock:missing"),
        ("thinking", "extreme"),
        ("mode", "yolo"),
    ] {
        let err = set_option(&h, id, value).unwrap_err();
        assert_eq!(err.code, -32602, "{id}={value}");
    }
    let params = serde_json::json!({"sessionId": "nope", "configId": "mode", "value": "ask"});
    let err = h
        .client
        .request("session/set_config_option", params)
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

#[test]
fn a_change_the_client_did_not_make_sends_config_option_update() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());
    let model = |model: &str| CommandKind::SetModel {
        model: model.into(),
    };

    h.command(model("mock:other"));
    let updates = updates_until(&h.inbox, &h.session, "config_option_update");
    let update = &updates.last().unwrap()["update"];
    assert_eq!(current_values(update), ["mock:other", "default", "default"]);

    h.command(model("mock:other"));
    h.command(CommandKind::SetThinking {
        level: Some(ThinkingLevel::Low),
    });
    let updates = updates_until(&h.inbox, &h.session, "config_option_update");
    assert_eq!(update_kinds(&updates), ["config_option_update"]);
    let update = &updates[0]["update"];
    assert_eq!(current_values(update), ["mock:other", "low", "default"]);
}

#[test]
fn states_older_than_a_client_change_are_not_sent_back() {
    let settings = |model: &str, thinking| Settings {
        model: model.into(),
        thinking: Some(thinking),
        levels: Vec::new(),
        mode: None,
    };
    let mut shown = Shown {
        settings: settings("mock:other", ThinkingLevel::High),
        catching_up: true,
    };
    assert!(!shown.observe(&settings("mock:other", ThinkingLevel::Off)));
    assert!(!shown.observe(&settings("mock:m", ThinkingLevel::Off)));
    assert!(!shown.observe(&settings("mock:other", ThinkingLevel::High)));
    assert!(!shown.observe(&settings("mock:other", ThinkingLevel::High)));
    assert!(shown.observe(&settings("mock:m", ThinkingLevel::High)));
    assert_eq!(shown.settings.model, "mock:m");
}

#[test]
fn a_turn_reports_usage_and_a_generated_title() {
    let dir = tempfile::tempdir().unwrap();
    let turn = vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::TextDelta {
            delta: "hello".into(),
        }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: CoreStopReason::EndTurn,
            usage: usage(100, 20),
        }),
    ];
    let h = serve(
        vec![turn, text_turn("Greeting title")],
        dir.path(),
        dir.path(),
    );

    assert_eq!(
        prompt(&h.client, &h.session, "hi")["stopReason"],
        "end_turn"
    );
    let updates = updates_until(&h.inbox, &h.session, "session_info_update");
    let usage = updates
        .iter()
        .map(|p| &p["update"])
        .find(|u| u["sessionUpdate"] == "usage_update" && u["used"] == 120)
        .expect("usage_update after the turn");
    assert_eq!(usage["size"], WINDOW);
    assert!(usage.get("cost").is_none());
    let info = &updates.last().unwrap()["update"];
    assert_eq!(info["title"], "Greeting title");
}

#[test]
fn mcp_prompts_are_commands_that_expand_when_sent_back() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_with(vec![text_turn("ok")], dir.path(), dir.path(), true);

    let updates = updates_until(&h.inbox, &h.session, "available_commands_update");
    let update = &updates.last().unwrap()["update"];
    assert_eq!(
        update["availableCommands"],
        serde_json::json!([{
            "name": "srv:p",
            "description": "Run p",
            "input": { "hint": "<a> [b]" },
        }])
    );

    assert_eq!(
        prompt(&h.client, &h.session, "/srv:p x")["stopReason"],
        "end_turn"
    );
    assert_eq!(h.mock.requests()[0].messages[0].content, [text("p a=x")]);

    let params = serde_json::json!({
        "sessionId": h.session,
        "prompt": [{"type": "text", "text": "/srv:p"}],
    });
    let err = h.client.request("session/prompt", params).unwrap_err();
    assert_eq!(err.code, -32602);
    assert_eq!(err.message, "mcp srv:p: missing argument a");
}

#[test]
fn editor_entries_become_server_specs_and_sse_is_refused() {
    let entries: Vec<McpServer> = serde_json::from_value(serde_json::json!([
        {
            "name": "local",
            "command": "srv",
            "args": ["--stdio"],
            "env": [{"name": "TOKEN", "value": "t"}],
        },
        {
            "type": "http",
            "name": "remote",
            "url": "https://mcp.example.com/mcp",
            "headers": [{"name": "Authorization", "value": "Bearer x"}],
        },
    ]))
    .unwrap();
    let specs = editor_servers(&entries).unwrap();
    let local = &specs["local"];
    assert_eq!(local.command.as_deref(), Some("srv"));
    assert_eq!(local.args, ["--stdio"]);
    assert_eq!(local.env["TOKEN"], "t");
    assert!(local.url.is_none());
    let remote = &specs["remote"];
    assert!(remote.command.is_none());
    assert_eq!(remote.url.as_deref(), Some("https://mcp.example.com/mcp"));
    assert_eq!(remote.headers["Authorization"], "Bearer x");
    assert!(!remote.disabled && remote.oauth.is_none());

    let sse: Vec<McpServer> = serde_json::from_value(serde_json::json!([
        {"type": "sse", "name": "old", "url": "https://mcp.example.com/sse", "headers": []},
    ]))
    .unwrap();
    let err = editor_servers(&sse).unwrap_err();
    assert_eq!(err.code, -32602);
    assert!(
        err.message.contains("`old` uses the sse transport"),
        "{}",
        err.message
    );
}

#[test]
fn editor_servers_get_no_login_hint() {
    let editor = ["ed".to_owned()];
    let refused = |server: &str| McpError::Unauthorized {
        server: server.to_owned(),
        login: true,
    };
    assert_eq!(
        without_login(refused("ed"), &editor).to_string(),
        "server `ed` needs authorization"
    );
    assert!(
        without_login(refused("conf"), &editor)
            .to_string()
            .contains("run kage mcp login conf")
    );
}

#[test]
fn initialize_advertises_http_mcp_servers_only() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());
    let params = serde_json::json!({"protocolVersion": PROTOCOL_VERSION, "clientCapabilities": {}});
    let init = h.client.request("initialize", params).unwrap();
    let caps = &init["agentCapabilities"]["mcpCapabilities"];
    assert_eq!(caps["http"], true);
    assert_eq!(caps["sse"], false);
}

#[test]
fn a_new_session_connects_the_editor_servers() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());

    let params = serde_json::json!({"cwd": dir.path(), "mcpServers": [editor_entry(dir.path())]});
    let created = h.client.request("session/new", params).unwrap();
    let session = created["sessionId"].as_str().unwrap();
    let updates = updates_until(&h.inbox, session, "available_commands_update");
    let update = &updates.last().unwrap()["update"];
    assert_eq!(
        update["availableCommands"],
        serde_json::json!([{ "name": "ed:greet", "description": "Say hi" }])
    );

    let sse = serde_json::json!({"type": "sse", "name": "old", "url": "http://x", "headers": []});
    let params = serde_json::json!({"cwd": dir.path(), "mcpServers": [sse]});
    let err = h.client.request("session/new", params).unwrap_err();
    assert_eq!(err.code, -32602);
}

#[test]
fn a_tool_of_an_editor_server_asks_and_sees_its_env() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let session = record(dir.path(), &cwd, "mock:m", 1, &[]);
    let h = serve(
        vec![
            tool_turn("call_1", "ed__show", serde_json::json!({})),
            text_turn("done"),
            text_turn("title"),
        ],
        dir.path(),
        dir.path(),
    );
    let params = serde_json::json!({
        "sessionId": session,
        "cwd": cwd,
        "mcpServers": [editor_entry(dir.path())],
    });
    h.client.request("session/resume", params).unwrap();
    let prompt_end = prompt_async(&h.client, &session, "show it");

    let (ask, params) = until_ask(&h.inbox, &mut Vec::new());
    assert_eq!(params["sessionId"], session);
    assert_eq!(params["toolCall"]["title"], "ed.show");
    allow(&h.client, &ask);

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let requests = h.mock.requests();
    assert!(requests[0].tools.iter().any(|t| t.name == "ed__show"));
    let results: Vec<_> = requests[1]
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResultBlock {
                output, is_error, ..
            } => Some((output.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results, [("ED_VAR=hello", false)]);
}

#[test]
fn a_new_session_is_answered_before_its_updates_as_the_input_ends() {
    use std::io::{BufRead as _, Write as _};

    let dir = tempfile::tempdir().unwrap();
    let (srv_r, mut cli_w) = std::io::pipe().unwrap();
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let params = serde_json::json!({"cwd": dir.path(), "mcpServers": []});
    let request =
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": params});
    writeln!(cli_w, "{request}").unwrap();
    drop(cli_w);
    let provider = Listed(MockProvider::sequence(Vec::new()));
    let (workdir, sessions) = (dir.path().to_path_buf(), dir.path().to_path_buf());
    serve_agent(BufReader::new(srv_r), srv_w, |peer| {
        test_agent(peer, provider, workdir, sessions, true)
    })
    .unwrap();

    let lines: Vec<serde_json::Value> = BufReader::new(cli_r)
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    let answered = lines.iter().position(|l| l["id"] == 1).expect("answered");
    let session = &lines[answered]["result"]["sessionId"];
    let updates: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i]["params"]["sessionId"] == *session)
        .collect();
    assert!(!updates.is_empty(), "{lines:?}");
    assert!(updates.iter().all(|&i| i > answered), "{lines:?}");
}

#[test]
fn a_session_without_mcp_prompts_gets_no_commands() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("ok")], dir.path(), dir.path());
    prompt(&h.client, &h.session, "hi");
    let updates = drain(&h.inbox);
    assert!(!update_kinds(&updates).contains(&"available_commands_update"));
}

#[test]
fn only_live_servers_offer_commands_and_bare_prompts_take_no_input() {
    use kage_core::protocol::McpPrompt;

    let prompt = McpPrompt {
        name: "p".into(),
        description: None,
        arguments: Vec::new(),
    };
    let server = |name: &str, status| McpServerInfo {
        name: name.into(),
        status,
        tools: 0,
        resources: Vec::new(),
        templates: Vec::new(),
        prompts: vec![prompt.clone()],
    };
    let failed = McpServerStatus::Failed {
        error: "gone".into(),
    };
    let servers = [
        server("down", failed),
        server("auth", McpServerStatus::NeedsAuth),
        server("up", McpServerStatus::Connected),
    ];
    assert_eq!(
        prompt_commands(&servers),
        [serde_json::json!({ "name": "up:p", "description": "" })]
    );
}

#[test]
fn usage_updates_carry_cost_only_when_priced() {
    let mut usage = Usage {
        context_used: 50,
        context_window: 200,
        ..Usage::default()
    };
    let Some(SessionUpdate::UsageUpdate(update)) = usage_update(&usage) else {
        panic!("no usage_update");
    };
    assert_eq!((update.used, update.size, update.cost), (50, 200, None));
    usage.cost = 0.25;
    let Some(SessionUpdate::UsageUpdate(update)) = usage_update(&usage) else {
        panic!("no usage_update");
    };
    let cost = update.cost.unwrap();
    assert_eq!((cost.amount, cost.currency.as_str()), (0.25, "USD"));
    usage.context_window = 0;
    assert!(usage_update(&usage).is_none());
}

fn kind(update: &SessionUpdate) -> &'static str {
    match update {
        SessionUpdate::ToolCall(_) => "tool_call",
        SessionUpdate::ToolCallUpdate(_) => "tool_call_update",
        _ => "other",
    }
}

fn status(update: &SessionUpdate) -> Option<ToolCallStatus> {
    match update {
        SessionUpdate::ToolCall(call) => Some(call.status),
        SessionUpdate::ToolCallUpdate(update) => update.status,
        _ => None,
    }
}

#[test]
fn a_tool_call_is_announced_once_then_updated_when_something_changed() {
    let id = ToolCallId::new("call_1");
    let args = |input: serde_json::Value| LoopEvent::ToolCallArgsDelta {
        id: id.clone(),
        name: "ed__run".into(),
        input_partial: input,
    };
    let events = [
        args(serde_json::json!({})),
        args(serde_json::json!({ "command": "ls" })),
        args(serde_json::json!({ "command": "ls" })),
        LoopEvent::ToolCallStart {
            id: id.clone(),
            name: "ed__run".into(),
            input_partial: serde_json::json!({ "command": "ls" }),
        },
        LoopEvent::ToolExecutionStart { id: id.clone() },
        LoopEvent::ToolUpdate {
            id: id.clone(),
            update: ToolUpdate {
                content: "a.txt".into(),
                structured: None,
            },
        },
        LoopEvent::ToolCallEnd {
            id,
            output: ToolOutput::default(),
        },
    ];
    let mut seen = HashMap::new();
    let updates: Vec<SessionUpdate> = events
        .iter()
        .filter_map(|e| to_update(&mut seen, e))
        .collect();
    let kinds: Vec<&str> = updates.iter().map(kind).collect();
    assert_eq!(
        kinds,
        [
            "tool_call",
            "tool_call_update",
            "tool_call_update",
            "tool_call_update",
            "tool_call_update"
        ]
    );
    let statuses: Vec<Option<ToolCallStatus>> = updates.iter().map(status).collect();
    assert_eq!(
        statuses,
        [
            Some(ToolCallStatus::Pending),
            None,
            Some(ToolCallStatus::InProgress),
            None,
            Some(ToolCallStatus::Completed)
        ]
    );
    let SessionUpdate::ToolCall(call) = &updates[0] else {
        unreachable!()
    };
    assert_eq!(call.title, "ed.run");
    let SessionUpdate::ToolCallUpdate(update) = &updates[1] else {
        unreachable!()
    };
    assert_eq!(
        update.raw_input,
        Some(serde_json::json!({ "command": "ls" }))
    );
}

#[test]
fn text_maps_to_agent_message_chunks() {
    let update = to_update(
        &mut HashMap::new(),
        &LoopEvent::TextDelta {
            id: MessageId::new(),
            delta: "hi".into(),
        },
    );
    assert!(matches!(update, Some(SessionUpdate::AgentMessageChunk(_))));
}

#[test]
fn max_tokens_surfaces_as_max_tokens() {
    assert_eq!(
        stop_reason(Some(CoreStopReason::MaxTokens)),
        StopReason::MaxTokens
    );
}

#[test]
fn ordinary_endings_map_to_end_turn() {
    assert_eq!(stop_reason(None), StopReason::EndTurn);
    assert_eq!(
        stop_reason(Some(CoreStopReason::EndTurn)),
        StopReason::EndTurn
    );
    assert_eq!(
        stop_reason(Some(CoreStopReason::ToolUse)),
        StopReason::EndTurn
    );
}

#[test]
fn built_in_tools_get_kind_hints() {
    assert_eq!(tool_kind("bash"), ToolKind::Execute);
    assert_eq!(tool_kind("grep"), ToolKind::Search);
    assert_eq!(tool_kind("github__create_issue"), ToolKind::Other);
}
