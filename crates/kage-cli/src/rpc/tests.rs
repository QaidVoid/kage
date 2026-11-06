use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use kage_acp::acp::{
    ContentBlock, EmbeddedResource, McpServer, NewSessionKage, SessionBackground, SessionOpt,
    SessionUpdate, ToolCallStatus, ToolKind,
};
use kage_core::agents::AgentDefs;
use kage_core::config::{McpServer as McpSpec, PluginsConfig};
use kage_core::environment::Environment;
use kage_core::permissions::{PermissionAction, PermissionsConfig, ToolPermissionRules};
use kage_core::protocol::{McpServerInfo, McpServerStatus, Usage};
use kage_core::{
    Content, LoopEvent, MediaSource, Message, MessageId, Modalities, Modality, Role, ThinkingLevel,
    TokenUsage, ToolCallId, ToolOutput, ToolUpdate,
};
use kage_jsonrpc::{Inbound, Peer};
use kage_loop::{AgentContext, LoopConfig};
use kage_mcp::McpError;
use kage_provider::testing::MockProvider;
use kage_provider::{ProviderError, ProviderEvent, ProviderRegistry};
use kage_session::{
    EntryId, FORMAT_VERSION, Header, MessageEntry, SessionEntry, SessionError, SessionWriter,
};
use kage_tools::builtin_registry;

use crate::engine::{AgentSetup, Commander};
use crate::permissions::PermissionGate;

use super::bridge::{to_update, tool_kind, usage_update};
use super::content::prompt_content;
use super::host::Host;
use super::mcp::{prompt_commands, without_login};
use super::*;

const WAIT: Duration = Duration::from_secs(20);

/// How long a turn that runs a real shell loop may take. A loaded CI
/// runner spawns processes slowly, and a passing run returns early.
#[cfg(unix)]
const SHELL_WAIT: Duration = Duration::from_secs(30);

type Script = Vec<Result<ProviderEvent, ProviderError>>;

fn tool_turn(id: &str, name: &str, input: serde_json::Value) -> Script {
    tool_turn_with_usage(id, name, input, 0)
}

fn tool_turn_with_usage(
    id: &str,
    name: &str,
    input: serde_json::Value,
    input_tokens: u64,
) -> Script {
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
            usage: usage(input_tokens, 0),
        }),
    ]
}

fn text_turn(text: &str) -> Script {
    text_turn_with_usage(text, 0)
}

fn text_turn_with_usage(text: &str, input_tokens: u64) -> Script {
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::TextDelta { delta: text.into() }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: CoreStopReason::EndTurn,
            usage: usage(input_tokens, 0),
        }),
    ]
}

const WINDOW: u64 = 1000;

/// The client side of a served `kage rpc`, the provider its sessions
/// call, and the id of the session open from the start.
struct Harness {
    host: Arc<Host>,
    client: Peer,
    inbox: mpsc::Receiver<Inbound>,
    mock: MockProvider,
    commander: Commander,
    id: SessionId,
    session: String,
}

/// A second client on a harness host.
struct Connection {
    client: Peer,
    inbox: mpsc::Receiver<Inbound>,
}

impl Harness {
    /// Sends `kind` to the open session the way a non-client change
    /// would reach the engine.
    fn command(&self, kind: CommandKind) {
        self.commander.send(Command::to(self.id, kind));
    }

    /// Opens a second connection on the same host.
    fn connect(&self) -> Connection {
        connect(&self.host)
    }
}

/// Opens a second connection on `host`.
fn connect(host: &Arc<Host>) -> Connection {
    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let host = Arc::clone(host);
    std::thread::spawn(move || host.serve(BufReader::new(srv_r), srv_w).unwrap());
    let (client, inbox, _reader) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
    Connection { client, inbox }
}

/// The mock provider, offering `mock:m` and `mock:other` to pickers.
/// `input` is what both listed models declare; empty means unknown.
#[derive(Debug)]
struct Listed {
    mock: MockProvider,
    input: Modalities,
}

impl Listed {
    fn of(mock: MockProvider) -> Self {
        Self {
            mock,
            input: Modalities::default(),
        }
    }
}

impl kage_provider::Provider for Listed {
    fn metadata(&self) -> &kage_provider::ProviderMetadata {
        self.mock.metadata()
    }

    fn stream(
        &self,
        req: kage_provider::StreamRequest,
        cancel: &kage_core::CancelFlag,
    ) -> Result<kage_provider::EventStream, ProviderError> {
        self.mock.stream(req, cancel)
    }

    fn models(&self) -> Vec<kage_provider::ProviderModel> {
        ["m", "other"]
            .map(|id| kage_provider::ProviderModel {
                id: id.into(),
                name: format!("Mock {id}"),
                input: self.input,
                ..kage_provider::ProviderModel::default()
            })
            .into()
    }
}

/// Serves `kage rpc` over pipes on the sessions recorded in
/// `sessions`, with one open session. Every session runs in
/// `workdir`, asks before `ls` calls and generates a title.
fn serve(scripts: Vec<Script>, workdir: &Path, sessions: &Path) -> Harness {
    serve_with(scripts, workdir, sessions, false, Modalities::default())
}

/// [`serve`], where every session also has the MCP server of
/// [`mcp_connection`] as `srv` when `mcp` is set. Servers a client
/// passes are spawned, and the tools of every server ask. `input` is
/// what the listed models declare; empty means unknown.
fn serve_with(
    scripts: Vec<Script>,
    workdir: &Path,
    sessions: &Path,
    mcp: bool,
    input: Modalities,
) -> Harness {
    serve_inner(
        scripts,
        workdir,
        sessions,
        mcp,
        input,
        default_agents(),
        false,
    )
}

/// [`serve`] with the session's agent setup replaced, for swarm tests
/// that need their children to run in a fixed order. The open session
/// is recorded, so its swarm children can be resumed.
fn serve_agents(
    scripts: Vec<Script>,
    workdir: &Path,
    sessions: &Path,
    agents: AgentSetup,
) -> Harness {
    serve_inner(
        scripts,
        workdir,
        sessions,
        false,
        Modalities::default(),
        agents,
        true,
    )
}

fn serve_inner(
    scripts: Vec<Script>,
    workdir: &Path,
    sessions: &Path,
    mcp: bool,
    input: Modalities,
    agents: AgentSetup,
    record: bool,
) -> Harness {
    let mock = MockProvider::sequence(scripts);
    let provider = Listed {
        mock: mock.clone(),
        input,
    };
    serve_on(
        Arc::new(provider),
        mock,
        workdir,
        sessions,
        mcp,
        agents,
        record,
    )
}

/// [`serve_inner`] on `provider`, with `mock` kept for the harness.
fn serve_on(
    provider: Arc<dyn kage_provider::Provider>,
    mock: MockProvider,
    workdir: &Path,
    sessions: &Path,
    mcp: bool,
    agents: AgentSetup,
    record: bool,
) -> Harness {
    let host = test_host_agents(
        provider,
        workdir.to_path_buf(),
        sessions.to_path_buf(),
        mcp,
        agents,
        PermissionAction::Allow,
    );
    serve_on_host(mock, host, SessionId::new(), sessions, record)
}

/// [`serve_on`] on a host the caller built, opening its standing
/// session as the host's spec names it.
fn serve_on_host(
    mock: MockProvider,
    host: Arc<Host>,
    id: SessionId,
    sessions: &Path,
    record: bool,
) -> Harness {
    let sessions = sessions.to_path_buf();
    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let commander = host.engine.commander();
    let standing = Arc::clone(&host);
    let (opened_tx, opened_rx) = mpsc::channel();
    std::thread::spawn(move || {
        standing
            .serve_with(BufReader::new(srv_r), srv_w, move |agent| {
                let (mut spec, runtime) = (agent.host.spec)(
                    &agent.host.registry(),
                    id,
                    "",
                    "mock/m",
                    BTreeMap::new(),
                    &Environment::inherit(),
                    None,
                    None,
                )
                .unwrap();
                if record {
                    let path = sessions.join(format!("{id}.jsonl"));
                    let header = Header {
                        version: FORMAT_VERSION,
                        session: id,
                        id: EntryId::new(),
                        ts: chrono::Utc::now(),
                        cwd: spec.cx.workdir.clone(),
                        model: "mock/m".into(),
                        backend: None,
                        system_prompt: spec.cx.system_prompt.clone(),
                        parent_session: None,
                        parent_entry: None,
                    };
                    spec.recorder = Some(Recorder::planned(path, header, spec.plugins.clone()));
                }
                agent.open(id.to_string(), spec, runtime);
                agent.session_announced(&id.to_string());
                let _ = opened_tx.send(());
            })
            .unwrap();
    });
    let (client, inbox, _reader) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
    opened_rx.recv_timeout(WAIT).unwrap();
    Harness {
        host,
        client,
        inbox,
        mock,
        commander,
        id,
        session: id.to_string(),
    }
}

/// A host on `provider` whose sessions run in `workdir`, ask before
/// `ls` calls and generate a title.
fn test_host(
    provider: Arc<dyn kage_provider::Provider>,
    workdir: PathBuf,
    sessions: PathBuf,
    mcp: bool,
) -> Arc<Host> {
    test_host_agents(
        provider,
        workdir,
        sessions,
        mcp,
        default_agents(),
        PermissionAction::Allow,
    )
}

/// The agent setup every non-swarm test runs with.
fn default_agents() -> AgentSetup {
    AgentSetup {
        defs: Arc::new(AgentDefs::builtin()),
        max_depth: 1,
        max_running: 4,
        swarm_max_items: 32,
        swarm_timeout_ms: 60_000,
        background: crate::engine::Background::Hold,
        max_turns: 0,
        timeout: None,
        budget: 0,
        thinking: None,
        model: None,
    }
}

/// [`test_host`] with the session's agent setup replaced, and with
/// `fallback` for tools that have no rule (production asks).
fn test_host_agents(
    provider: Arc<dyn kage_provider::Provider>,
    workdir: PathBuf,
    sessions: PathBuf,
    mcp: bool,
    agents: AgentSetup,
    fallback: PermissionAction,
) -> Arc<Host> {
    test_host_with_plugins(provider, workdir, sessions, mcp, agents, fallback, None)
}

/// [`test_host_agents`] whose sessions run with `runtime` as their
/// plugins, so a session-bound UI attach has a runtime to pin to.
fn test_host_with_plugins(
    provider: Arc<dyn kage_provider::Provider>,
    workdir: PathBuf,
    sessions: PathBuf,
    mcp: bool,
    agents: AgentSetup,
    fallback: PermissionAction,
    runtime: Option<Arc<kage_plugin::PluginRuntime>>,
) -> Arc<Host> {
    let registry = Arc::new(ProviderRegistry::new().with(provider));
    let agents = Arc::new(agents);
    let spec: super::host::SpecBuilder = Box::new(
        move |_: &ProviderRegistry,
              id,
              _cwd: &str,
              model: &str,
              servers,
              env: &Environment,
              plugins: Option<Arc<dyn SessionPlugins>>,
              _client_system: Option<&str>| {
            let agents = Arc::clone(&agents);
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
            let gate = PermissionGate::new(rules)
                .with_fallback(fallback)
                .with_mcp_servers(names.map(str::to_owned).collect());
            Ok((
                SessionSpec {
                    id,
                    model: model.to_owned(),
                    cx: AgentContext {
                        env: env.clone(),
                        ..AgentContext::new(model, "")
                            .with_workdir(&workdir)
                            .with_context_window(WINDOW)
                    },
                    recorder: None,
                    tools,
                    gate,
                    loop_cfg: LoopConfig::default(),
                    plugins: plugins.or_else(|| {
                        runtime.clone().map(|runtime| {
                            runtime as Arc<dyn kage_plugin::session_plugins::SessionPlugins>
                        })
                    }),
                    mcp,
                    interactive: true,
                    title: true,
                    shell: None,
                    agents: Some((*agents).clone()),
                },
                runtime.clone(),
            ))
        },
    );
    Host::new(registry, "mock/m".into(), sessions, spec, BTreeMap::new())
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
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9]*\),.*/\1/p')
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

/// A client that asks for the TUI's rules runs tools that have no rule
/// without a round-trip, and its agents inherit that; a client that
/// does not ask gets the editor default and is asked.
#[test]
fn a_client_asking_for_tui_rules_runs_unconfigured_tools() {
    for tui_rules in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().display().to_string();
        let mock = MockProvider::sequence(vec![
            tool_turn(
                "call_find",
                "find",
                serde_json::json!({ "pattern": "*.rs", "path": path }),
            ),
            text_turn("done"),
            text_turn("title"),
        ]);
        let provider = Listed {
            mock: mock.clone(),
            input: Modalities::default(),
        };
        let host = test_host_agents(
            Arc::new(provider),
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            false,
            default_agents(),
            PermissionAction::Ask,
        );
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        std::thread::spawn(move || {
            host.serve_with(BufReader::new(srv_r), srv_w, |_| {})
                .unwrap();
        });
        let (client, inbox, _reader) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
        let mut capabilities = serde_json::json!({});
        if tui_rules {
            capabilities["_meta"] = serde_json::json!({"kage": {"unconfiguredTools": "allow"}});
        }
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": capabilities,
        });
        client.request("initialize", params).unwrap();
        let params = serde_json::json!({ "cwd": path, "mcpServers": [] });
        let created = client.request("session/new", params).unwrap();
        let session = created["sessionId"].as_str().unwrap().to_owned();
        let end = prompt_async(&client, &session, "find the sources");
        if !tui_rules {
            let (ask, params) = until_ask(&inbox, &mut Vec::new());
            assert_eq!(params["toolCall"]["title"], "find");
            allow(&client, &ask);
        }
        let response = end.recv_timeout(WAIT).unwrap().unwrap();
        assert_eq!(response["stopReason"], "end_turn");
        if tui_rules {
            let asked = inbox
                .try_iter()
                .any(|message| matches!(message, Inbound::Request { .. }));
            assert!(!asked, "a tool without a rule ran without asking");
        }
    }
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
    prompt_async_with(client, session, text, None)
}

/// [`prompt_async`] with a `delivery` marker on the params.
fn prompt_async_with(
    client: &Peer,
    session: &str,
    text: &str,
    delivery: Option<&str>,
) -> mpsc::Receiver<Result<serde_json::Value, kage_jsonrpc::RpcError>> {
    let (done, end) = mpsc::channel();
    let client = client.clone();
    let mut params = serde_json::json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": text}],
    });
    if let Some(delivery) = delivery {
        params["delivery"] = serde_json::json!(delivery);
    }
    std::thread::spawn(move || {
        let _ = done.send(client.request("session/prompt", params));
    });
    end
}

fn is_terminal(params: &serde_json::Value) -> bool {
    let state = &params["update"]["state"];
    params["update"]["sessionUpdate"] == "subagent_update"
        && !state.is_null()
        && state != "running"
        && state != "paused"
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
    assert_eq!(announced["update"]["state"], "running");
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
    let ended = &updates[terminal]["update"];
    assert!(
        ended["model"].as_str().is_some_and(|m| m.contains('/')),
        "{ended}"
    );
    assert!(ended["usage"]["runMs"].is_u64(), "{ended}");
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

/// One provider call that fails with a rate limit carrying a tiny
/// retry hint, so the loop's own retries stay fast in tests.
fn rate_limited() -> Script {
    vec![Err(ProviderError::RateLimited {
        retry_after: Some(Duration::from_millis(5)),
        body: None,
    })]
}

fn swarm_input(items: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "description": "review crates",
        "agent": "general",
        "prompt_template": "review {{item}}",
        "items": items,
    })
}

#[test]
fn a_swarm_call_announces_its_members_and_batch() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_agents(
        vec![
            tool_turn("call_s", "swarm", swarm_input(&["kage-core", "kage-tui"])),
            text_turn("core done"),
            text_turn("tui done"),
            text_turn("parent done"),
        ],
        dir.path(),
        dir.path(),
        AgentSetup {
            max_running: 1,
            ..default_agents()
        },
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let updates = drain(&h.inbox);

    let announced = updates
        .iter()
        .find(|p| {
            p["update"]["toolCallId"] == "call_s"
                && p["update"]["_meta"]["kage"]["swarm"].is_object()
        })
        .expect("the swarm call announces its members");
    let meta = &announced["update"]["_meta"]["kage"]["swarm"];
    assert_eq!(
        meta["members"],
        serde_json::json!(["kage-core", "kage-tui"])
    );
    assert_eq!(meta["template"], "review {{item}}");

    let members: Vec<_> = updates
        .iter()
        .filter(|p| {
            p["update"]["sessionUpdate"] == "subagent_update" && p["update"]["swarm"].is_object()
        })
        .map(|p| &p["update"])
        .collect();
    assert_eq!(members.len(), 2, "{updates:#?}");
    let batch = members[0]["swarm"]["id"].as_str().unwrap().to_owned();
    assert!(batch.starts_with("swarm_"), "{batch}");
    assert_eq!(members[0]["swarm"]["index"], 0);
    assert_eq!(members[0]["swarm"]["item"], "kage-core");
    assert_eq!(members[0]["swarm"]["total"], 2);
    assert_eq!(members[1]["swarm"]["id"], batch.as_str());
    assert_eq!(members[1]["swarm"]["index"], 1);
    assert_eq!(members[1]["swarm"]["item"], "kage-tui");
    for member in &members {
        assert_eq!(member["task"], "review crates");
    }
    let states: Vec<_> = updates
        .iter()
        .filter(|p| is_terminal(p))
        .map(|p| p["update"]["state"].clone())
        .collect();
    assert_eq!(states, ["completed", "completed"]);
}

#[test]
fn a_rate_limited_swarm_child_pauses_with_a_reason_and_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let mut scripts = vec![tool_turn("call_s", "swarm", swarm_input(&["a", "b"]))];
    scripts.push(text_turn("a done"));
    scripts.extend(std::iter::repeat_n(rate_limited(), 5));
    scripts.push(text_turn("b done"));
    scripts.push(text_turn("parent done"));
    let h = serve_agents(
        scripts,
        dir.path(),
        dir.path(),
        AgentSetup {
            max_running: 1,
            ..default_agents()
        },
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let updates = drain(&h.inbox);

    let announced = updates
        .iter()
        .find(|p| p["update"]["swarm"]["item"] == "b")
        .expect("b announced");
    let b = announced["update"]["subagentSessionId"].as_str().unwrap();
    let paused_at = updates
        .iter()
        .position(|p| p["update"]["subagentSessionId"] == b && p["update"]["state"] == "paused")
        .expect("b pauses");
    let reason = updates[paused_at]["update"]["reason"].as_str().unwrap();
    assert!(reason.contains("rate limited"), "{reason}");
    let terminal_at = updates
        .iter()
        .position(|p| is_terminal(p) && p["update"]["subagentSessionId"] == b)
        .expect("b ends");
    assert!(paused_at < terminal_at, "{updates:#?}");
    let running_again = updates
        .iter()
        .skip(paused_at)
        .position(|p| p["update"]["subagentSessionId"] == b && p["update"]["state"] == "running")
        .map(|at| at + paused_at)
        .expect("b reports running again");
    assert!(running_again < terminal_at, "{updates:#?}");
    assert_eq!(updates[terminal_at]["update"]["state"], "completed");
    let failed = updates.iter().any(|p| {
        is_terminal(p) && p["update"]["subagentSessionId"] == b && p["update"]["state"] == "failed"
    });
    assert!(!failed, "{updates:#?}");
}

#[test]
fn a_failed_swarm_member_resumes_from_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    // The open session is recorded, so it has swarm children to resume.
    // Member b fails on an auth error, which no requeue retries. The
    // parent's title call follows its first turn; waiting for it keeps
    // it from consuming b's resumed script.
    let h = serve_agents(
        vec![
            tool_turn("call_s", "swarm", swarm_input(&["a", "b"])),
            text_turn("a done"),
            vec![Err(ProviderError::Auth("boom".into()))],
            text_turn("parent done"),
            text_turn("Parent title"),
            text_turn("b resumed"),
        ],
        dir.path(),
        dir.path(),
        AgentSetup {
            max_running: 1,
            ..default_agents()
        },
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let updates = drain(&h.inbox);
    let announced = updates
        .iter()
        .find(|p| p["update"]["swarm"]["item"] == "b")
        .expect("b announced");
    let b = announced["update"]["subagentSessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let batch = announced["update"]["swarm"]["id"].clone();
    let call = announced["update"]["toolCallId"].clone();
    let failed = updates
        .iter()
        .find(|p| is_terminal(p) && p["update"]["subagentSessionId"] == b)
        .expect("b ends");
    assert_eq!(failed["update"]["state"], "failed");

    // The parent's title call follows its first turn. Waiting for its
    // recorded title keeps the call from consuming b's resumed script.
    let title = updates
        .iter()
        .find(|p| {
            p["update"]["sessionUpdate"] == "session_info_update"
                && p["update"]["title"] == "Parent title"
        })
        .cloned()
        .unwrap_or_else(|| {
            loop {
                let Inbound::Notification { params, .. } =
                    h.inbox.recv_timeout(WAIT).expect("no title update")
                else {
                    continue;
                };
                if params["update"]["sessionUpdate"] == "session_info_update"
                    && params["update"]["title"] == "Parent title"
                {
                    break params;
                }
            }
        });
    assert_eq!(title["update"]["title"], "Parent title");

    // A session that is no swarm child refuses the whole request.
    let stranger = SessionId::new().to_string();
    let refused = h.client.request(
        "_kage/swarm/resume",
        serde_json::json!({"sessionId": h.session, "members": {&stranger: "go on"}}),
    );
    assert!(refused.is_err(), "{refused:?}");

    // Resuming the failed member continues it on its own card.
    let resumed = h
        .client
        .request(
            "_kage/swarm/resume",
            serde_json::json!({"sessionId": h.session, "members": {&b: "go on"}}),
        )
        .unwrap();
    assert_eq!(resumed["resumed"], serde_json::json!([b]));
    let updates: Vec<_> = until_terminal(&h.inbox)
        .into_iter()
        .map(|(_, p)| p)
        .collect();
    let reannounced = updates
        .iter()
        .find(|p| p["update"]["subagentSessionId"] == b && p["update"]["swarm"].is_object())
        .expect("b re-announced");
    assert_eq!(reannounced["update"]["swarm"]["id"], batch);
    assert_eq!(reannounced["update"]["swarm"]["item"], "b");
    assert_eq!(reannounced["update"]["toolCallId"], call);
    assert_eq!(reannounced["update"]["state"], "running");
    let terminal = updates
        .iter()
        .find(|p| {
            is_terminal(p)
                && p["update"]["subagentSessionId"] == b
                && p["update"]["state"] != "failed"
        })
        .expect("b ends");
    assert_eq!(terminal["update"]["state"], "completed");
    assert_eq!(terminal["sessionId"], h.session);
    assert!(updates.iter().any(|p| p["sessionId"] == b));
}

#[test]
fn the_swarm_option_toggles_swarm_mode_with_enter_and_exit_notices() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("ok")], dir.path(), dir.path());

    let created = set_option(&h, "swarm", "on").unwrap();
    let options = created["configOptions"].as_array().unwrap();
    assert_eq!(options[3]["id"], "swarm");
    assert_eq!(options[3]["currentValue"], "on");
    assert_eq!(values_of(&options[3], "value"), ["off", "on"]);
    // A change the client made rides the response, so only the notice
    // follows.
    let updates = updates_until(&h.inbox, &h.session, "_kage/notice");
    let notice = updates.last().unwrap();
    assert_eq!(notice["update"]["tone"], "info");
    assert_eq!(notice["update"]["text"], "swarm mode on");

    let cleared = set_option(&h, "swarm", "off").unwrap();
    assert_eq!(cleared["configOptions"][3]["currentValue"], "off");
    let updates = updates_until(&h.inbox, &h.session, "_kage/notice");
    let notice = updates.last().unwrap();
    assert_eq!(notice["update"]["tone"], "info");
    assert_eq!(notice["update"]["text"], "swarm mode off");
}

#[test]
fn the_goal_option_sets_and_clears_the_goal() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("ok")], dir.path(), dir.path());

    let created = set_option(&h, "goal", "ship it").unwrap();
    let options = created["configOptions"].as_array().unwrap();
    assert_eq!(options[4]["id"], "goal");
    assert_eq!(options[4]["type"], "text");
    assert_eq!(options[4]["currentValue"], "ship it");

    let cleared = set_option(&h, "goal", "").unwrap();
    assert_eq!(cleared["configOptions"][4]["currentValue"], "");
}

#[test]
fn a_set_goal_is_sent_to_the_agent_checked_each_turn_and_clearing_stops_the_checks() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            text_turn("hello"),  // the "first" turn
            text_turn("T1"),     // its title call
            text_turn("on it"),  // the turn the goal set starts
            text_turn("YES"),    // the check after it: met
            text_turn("again"),  // the "go" turn
            text_turn("NO"),     // the check after it: reopens the goal
            text_turn("fixing"), // the turn the nudge starts
            text_turn("YES"),    // the check after it: met again
            text_turn("done"),   // the "last" turn
        ],
        dir.path(),
        dir.path(),
    );

    // No goal: the turn and its title call are the only provider calls.
    // The title call runs on its own thread, so wait for it to land.
    let response = prompt(&h.client, &h.session, "first");
    assert_eq!(response["stopReason"], "end_turn");
    let deadline = Instant::now() + WAIT;
    while h.mock.call_count() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(h.mock.call_count(), 2);

    // Setting a goal sends it to the agent as a turn of its own, and
    // meeting the goal reaches the client as a success notice.
    let created = set_option(&h, "goal", "ship it").unwrap();
    assert_eq!(created["configOptions"][4]["currentValue"], "ship it");
    let deadline = Instant::now() + WAIT;
    while h.mock.call_count() < 4 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(h.mock.call_count(), 4);
    let requests = h.mock.requests();
    let intro = requests[2].messages.last().unwrap();
    let Content::Text { text } = &intro.content[0] else {
        panic!("text intro");
    };
    assert!(text.contains("Work toward this goal: ship it"), "{text}");
    let deadline = Instant::now() + WAIT;
    let mut texts: Vec<String> = Vec::new();
    loop {
        texts.extend(notice_texts(&h.inbox));
        if h.mock.call_count() >= 4 && texts.iter().any(|t| t == "goal met: ship it") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "count {}, {texts:?}",
            h.mock.call_count()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(h.mock.call_count(), 4);

    // A later turn is still checked; a not-met verdict reopens the
    // goal and keeps the session working until the check says met.
    let response = prompt(&h.client, &h.session, "go");
    assert_eq!(response["stopReason"], "end_turn");
    let deadline = Instant::now() + WAIT;
    let mut texts: Vec<String> = Vec::new();
    loop {
        texts.extend(notice_texts(&h.inbox));
        let reopened = texts
            .iter()
            .any(|t| t == "goal not met yet; continuing (1/8)");
        let met = texts.iter().any(|t| t == "goal met: ship it");
        if h.mock.call_count() >= 8 && reopened && met {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "count {}, {texts:?}",
            h.mock.call_count()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(h.mock.call_count(), 8);

    // Clearing the goal stops the checks.
    let cleared = set_option(&h, "goal", "").unwrap();
    assert_eq!(cleared["configOptions"][4]["currentValue"], "");
    let response = prompt(&h.client, &h.session, "last");
    assert_eq!(response["stopReason"], "end_turn");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(h.mock.call_count(), 9);
}

#[test]
fn initialize_advertises_image_audio_and_embedded_context() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());
    let params = serde_json::json!({"protocolVersion": PROTOCOL_VERSION, "clientCapabilities": {}});
    let init = h.client.request("initialize", params).unwrap();
    let caps = &init["agentCapabilities"]["promptCapabilities"];
    assert_eq!(caps["image"], true);
    assert_eq!(caps["embeddedContext"], true);
    assert_eq!(caps["audio"], true);
    assert_eq!(init["agentCapabilities"]["steer"], true);
    let cwd = std::env::current_dir().unwrap().display().to_string();
    assert_eq!(
        init["_meta"]["kage"]["cwd"], cwd,
        "a client without a directory learns the server's"
    );
    let defaults = init["_meta"]["kage"]["configOptions"].as_array().unwrap();
    let current = |id: &str| {
        defaults
            .iter()
            .find(|option| option["id"] == id)
            .map(|option| option["currentValue"].clone())
    };
    assert_eq!(current("model"), Some(serde_json::json!("mock/m")));
    assert_eq!(current("mode"), Some(serde_json::json!("default")));
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
        source: MediaSource::Base64 {
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
            Content::Audio {
                source: MediaSource::Base64 {
                    data: "AAAA".into(),
                },
                mime: "audio/wav".into(),
            },
        ]
    );
}

#[test]
fn an_unknown_prompt_block_is_omitted_with_a_note() {
    let content = prompt_content(ContentBlock::Unknown);
    assert_eq!(content, text("[unsupported block omitted]"));
}

#[test]
fn a_resource_without_text_or_blob_is_omitted_with_a_note() {
    let block = ContentBlock::Resource(EmbeddedResource {
        resource: serde_json::json!({ "uri": "file:///w/e.txt" }),
    });
    let content = prompt_content(block);
    assert_eq!(content, text("[resource omitted]"));
}

#[test]
fn an_image_only_prompt_to_a_text_only_model_answers_invalid_params() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_with(
        vec![],
        dir.path(),
        dir.path(),
        false,
        Modalities::of(&[Modality::Text]),
    );
    let params = serde_json::json!({
        "sessionId": h.session,
        "prompt": [{"type": "image", "data": "aGk=", "mimeType": "image/png"}],
    });
    let err = h.client.request("session/prompt", params).unwrap_err();
    assert_eq!(err.code, -32602);
    assert!(
        err.message.contains("does not accept images"),
        "{}",
        err.message
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

#[test]
fn plan_mode_refuses_a_write_and_announces_entry_and_exit() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("new.txt");
    let h = serve(
        vec![
            tool_turn(
                "call_w",
                "write",
                serde_json::json!({"path": file.display().to_string(), "content": "nope"}),
            ),
            text_turn("still planning"),
        ],
        dir.path(),
        dir.path(),
    );

    let set = set_option(&h, "mode", "plan").unwrap();
    assert_eq!(
        current_values(&set),
        ["mock/m", "default", "plan", "off", ""]
    );
    let updates = updates_until(&h.inbox, &h.session, "current_mode_update");
    assert_eq!(updates.last().unwrap()["update"]["currentModeId"], "plan");

    let response = prompt(&h.client, &h.session, "investigate");
    assert_eq!(response["stopReason"], "end_turn");
    let updates = drain(&h.inbox);
    let refused = updates
        .iter()
        .find(|p| p["update"]["toolCallId"] == "call_w" && p["update"]["status"] == "failed")
        .expect("the write is refused");
    assert!(
        refused["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("plan mode is on"),
        "{refused}"
    );
    assert!(!file.exists(), "the write must not execute");

    let set = set_option(&h, "mode", "default").unwrap();
    assert_eq!(
        current_values(&set),
        ["mock/m", "default", "default", "off", ""]
    );
    let updates = updates_until(&h.inbox, &h.session, "current_mode_update");
    assert_eq!(
        updates.last().unwrap()["update"]["currentModeId"],
        "default"
    );
}

#[test]
fn a_plan_review_offers_three_options_and_approve_resumes_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn(
                "call_p",
                "exit_plan",
                serde_json::json!({"plan": "# Fix\n\n1. Edit a.rs"}),
            ),
            text_turn("doing the work"),
        ],
        dir.path(),
        dir.path(),
    );
    set_option(&h, "mode", "plan").unwrap();
    updates_until(&h.inbox, &h.session, "current_mode_update");

    let prompt_end = prompt_async(&h.client, &h.session, "plan it");
    let mut updates = Vec::new();
    let (ask, params) = until_ask(&h.inbox, &mut updates);
    assert_eq!(params["toolCall"]["toolCallId"], "call_p");
    assert_eq!(params["toolCall"]["title"], "exit_plan");
    let options: Vec<&str> = params["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(options, ["approve", "revise", "reject"]);
    assert_eq!(
        params["_meta"]["kage"]["planReview"]["plan"],
        "# Fix\n\n1. Edit a.rs"
    );

    let outcome = serde_json::json!({"outcome": {"outcome": "selected", "optionId": "approve"}});
    h.client.respond(&ask, Ok(outcome)).unwrap();
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    updates.extend(drain(&h.inbox));

    let ran = updates
        .iter()
        .find(|p| p["update"]["toolCallId"] == "call_p" && p["update"]["status"] == "completed")
        .expect("exit_plan runs once approved");
    assert!(
        ran["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("approved"),
        "{ran}"
    );
    assert!(updates.iter().any(|p| {
        p["update"]["sessionUpdate"] == "agent_message_chunk"
            && p["update"]["content"]["text"] == "doing the work"
    }));
    let modes: Vec<&str> = updates
        .iter()
        .filter(|p| p["update"]["sessionUpdate"] == "current_mode_update")
        .filter_map(|p| p["update"]["currentModeId"].as_str())
        .collect();
    assert_eq!(modes, ["default"], "plan mode ends with the approval");
}

#[test]
fn a_rejected_plan_ends_the_turn_like_a_denial() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![tool_turn(
            "call_p",
            "exit_plan",
            serde_json::json!({"plan": "# Fix"}),
        )],
        dir.path(),
        dir.path(),
    );
    set_option(&h, "mode", "plan").unwrap();
    updates_until(&h.inbox, &h.session, "current_mode_update");

    let prompt_end = prompt_async(&h.client, &h.session, "plan it");
    let (ask, _) = until_ask(&h.inbox, &mut Vec::new());
    let outcome = serde_json::json!({"outcome": {"outcome": "selected", "optionId": "reject"}});
    h.client.respond(&ask, Ok(outcome)).unwrap();

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    until(|| h.mock.call_count() >= 2);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        h.mock.call_count(),
        2,
        "a rejected plan must not resume the run"
    );
    let late = drain(&h.inbox);
    assert!(
        !late
            .iter()
            .any(|p| p["update"]["sessionUpdate"] == "current_mode_update"),
        "plan mode stays on: {late:?}"
    );
    assert!(
        !late.iter().any(|p| {
            p["update"]["sessionUpdate"] == "_kage/notice"
                && p["update"]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("plan mode off"))
        }),
        "{late:?}"
    );
}

#[test]
fn a_revised_plan_delivers_the_text_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn("call_p", "exit_plan", serde_json::json!({"plan": "# Fix"})),
            text_turn("revised"),
        ],
        dir.path(),
        dir.path(),
    );
    set_option(&h, "mode", "plan").unwrap();
    updates_until(&h.inbox, &h.session, "current_mode_update");

    let prompt_end = prompt_async(&h.client, &h.session, "plan it");
    let (ask, _) = until_ask(&h.inbox, &mut Vec::new());
    let outcome = serde_json::json!({
        "outcome": {"outcome": "selected", "optionId": "revise"},
        "_meta": {"kage": {"planReview": {"revision": "cover the tests too"}}},
    });
    h.client.respond(&ask, Ok(outcome)).unwrap();

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    until(|| h.mock.call_count() >= 3);
    let requests = h.mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests.iter().any(|request| request
            .messages
            .last()
            .is_some_and(|m| m.content == [text("cover the tests too")])),
        "the revision reaches the model as its own prompt"
    );
    // The revision runs as a fresh turn; the ended run's title call
    // may take the scripted turn, so only the turn boundary is
    // asserted on the wire.
    let mut updates = updates_until(&h.inbox, &h.session, "_kage/turn");
    updates.extend(drain(&h.inbox));
    assert!(
        !updates.iter().any(|p| {
            p["update"]["sessionUpdate"] == "current_mode_update"
                && p["update"]["currentModeId"] != "plan"
        }),
        "plan mode stays on through a revision: {updates:?}"
    );
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
        message: Arc::new(Message::new(role, content, None)),
        usage,
    })
}

fn text(text: &str) -> Content {
    Content::Text { text: text.into() }
}

/// Records a session in `dir` created in `cwd` on `model`, `age`
/// minutes ago, and returns its id.
fn record(dir: &Path, cwd: &str, model: &str, age: i64, entries: &[SessionEntry]) -> String {
    record_as(dir, SessionId::new(), cwd, model, age, entries)
}

/// [`record`] under the fixed `session` id, so a test can host the
/// same session in the engine its file was recorded for.
fn record_as(
    dir: &Path,
    session: SessionId,
    cwd: &str,
    model: &str,
    age: i64,
    entries: &[SessionEntry],
) -> String {
    let header = Header {
        version: FORMAT_VERSION,
        session,
        id: EntryId::new(),
        ts: chrono::Utc::now() - chrono::Duration::minutes(age),
        cwd: cwd.into(),
        model: model.into(),
        backend: None,
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

/// The text of every notice pending in the inbox, drained.
fn notice_texts(inbox: &mpsc::Receiver<Inbound>) -> Vec<String> {
    drain(inbox)
        .iter()
        .filter_map(|params| params["update"]["text"].as_str().map(str::to_owned))
        .collect()
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
    let recent = record(sessions, "/p", "mock/m", 30, &[title]);
    let old = record(sessions, "/p", "mock/m", 10, &[]);
    record(sessions, "/p", "mock/m", 0, &[marker]);
    let elsewhere = record(sessions, "/q", "mock/m", 5, &[]);
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
fn session_fork_names_an_image_only_prompt_without_text() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let image = || Content::Image {
        source: MediaSource::Base64 {
            data: "AAAA".into(),
        },
        mime: "image/png".into(),
    };
    let entries = [
        message(Role::User, vec![image()], None),
        message(Role::Assistant, vec![text("one")], None),
        message(Role::User, vec![image()], None),
    ];
    let source = record(sessions, "/p", "mock/m", 1, &entries);
    let h = serve(Vec::new(), sessions, sessions);
    let history = |id: &str| {
        let path = kage_session::find_by_prefix(sessions, id).unwrap().unwrap();
        kage_session::replay(&path).unwrap().history.len()
    };
    let fork = |before: serde_json::Value| {
        h.client
            .request(
                "_kage/session/fork",
                serde_json::json!({ "sessionId": source, "before": before }),
            )
            .map(|page| page["sessionId"].as_str().unwrap().to_owned())
    };
    let second = fork(serde_json::json!({ "occurrence": 1 })).unwrap();
    assert_eq!(
        history(&second),
        2,
        "copies through the first image-only turn"
    );
    let first = fork(serde_json::json!({})).unwrap();
    assert_eq!(
        history(&first),
        0,
        "the first image-only prompt has no past"
    );
    let textful = fork(serde_json::json!({ "text": "" })).unwrap_err();
    assert_eq!(
        textful.code, -32602,
        "empty text no longer matches a textless prompt"
    );
}

#[test]
fn session_fork_copies_up_to_the_named_prompt_and_links_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let turn = |prompt: &str, reply: &str| {
        [
            message(Role::User, vec![text(prompt)], None),
            message(Role::Assistant, vec![text(reply)], None),
        ]
    };
    let entries: Vec<SessionEntry> = [
        turn("again", "one"),
        turn("next", "two"),
        turn("again", "three"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let source = record(sessions, "/p", "mock/m", 1, &entries);
    let h = serve(Vec::new(), sessions, sessions);
    let fork = |before: serde_json::Value| {
        let mut params = serde_json::json!({ "sessionId": source });
        if !before.is_null() {
            params["before"] = before;
        }
        h.client.request("_kage/session/fork", params)
    };
    let history = |id: &str| {
        let path = kage_session::find_by_prefix(sessions, id).unwrap().unwrap();
        kage_session::replay(&path).unwrap().history.len()
    };

    let second = fork(serde_json::json!({ "text": "again", "occurrence": 1 })).unwrap();
    let second = second["sessionId"].as_str().unwrap();
    assert_eq!(history(second), 4, "copies the two turns before it");
    let first = fork(serde_json::json!({ "text": "again" })).unwrap();
    assert_eq!(history(first["sessionId"].as_str().unwrap()), 0);
    let whole = fork(serde_json::Value::Null).unwrap();
    assert_eq!(history(whole["sessionId"].as_str().unwrap()), 6);
    let missing = fork(serde_json::json!({ "text": "never" })).unwrap_err();
    assert_eq!(missing.code, -32602);

    let page = h
        .client
        .request("session/list", serde_json::json!({}))
        .unwrap();
    let listed = page["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["sessionId"] == second)
        .unwrap();
    assert_eq!(listed["_meta"]["kage"]["parentSessionId"], source);
}

#[test]
fn session_export_renders_the_recorded_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let source = record(
        sessions,
        "/p",
        "mock/m",
        1,
        &[
            message(Role::User, vec![text("explain the parser")], None),
            message(Role::Assistant, vec![text("it reads tokens")], None),
        ],
    );
    let h = serve(Vec::new(), sessions, sessions);

    let out = h
        .client
        .request(
            "_kage/session/export",
            serde_json::json!({ "sessionId": source }),
        )
        .unwrap();
    let markdown = out["markdown"].as_str().unwrap();
    assert!(markdown.contains("explain the parser"), "{markdown}");
    assert!(markdown.contains("it reads tokens"), "{markdown}");
    let unknown = h
        .client
        .request(
            "_kage/session/export",
            serde_json::json!({ "sessionId": "zz" }),
        )
        .unwrap_err();
    assert_eq!(unknown.code, -32602);
}

#[test]
fn session_rename_names_the_session_for_every_client() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());
    let rename = |title: &str| {
        h.client.request(
            "_kage/session/rename",
            serde_json::json!({ "sessionId": h.session, "title": title }),
        )
    };

    assert_eq!(rename("  Parser work ").unwrap(), serde_json::json!({}));
    let updates = updates_until(&h.inbox, &h.session, "session_info_update");
    assert_eq!(updates.last().unwrap()["update"]["title"], "Parser work");
    assert_eq!(rename(" ").unwrap_err().code, -32602);
}

#[test]
fn session_compact_needs_an_open_session() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());
    let params = serde_json::json!({"cwd": dir.path(), "mcpServers": []});
    let created = h.client.request("session/new", params).unwrap();

    let compact = |id: &serde_json::Value| {
        h.client.request(
            "_kage/session/compact",
            serde_json::json!({ "sessionId": id }),
        )
    };
    assert_eq!(
        compact(&created["sessionId"]).unwrap(),
        serde_json::json!({})
    );
    assert_eq!(compact(&serde_json::json!("zz")).unwrap_err().code, -32602);
}

#[test]
fn session_list_pages_with_a_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let ids: Vec<String> = (0..52)
        .map(|age| record(sessions, "/p", "mock/m", age, &[]))
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
fn a_session_load_meta_sets_the_session_up_like_a_new_ones() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let session = record(
        dir.path(),
        &cwd,
        "mock/recorded",
        1,
        &[message(Role::User, vec![text("hello")], None)],
    );
    let h = serve(vec![text_turn("ok")], dir.path(), dir.path());
    let params = serde_json::json!({
        "sessionId": session,
        "cwd": cwd,
        "mcpServers": [],
        "_meta": { "kage": {
            "yolo": true,
            "opt": { "thinkingLevel": "high" },
        }},
    });
    let loaded = h.client.request("session/load", params.clone()).unwrap();
    let values = |result: &serde_json::Value, id: &str| {
        result["configOptions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|option| option["id"] == id)
            .unwrap_or_else(|| panic!("{id} option in {result}"))["currentValue"]
            .clone()
    };
    assert_eq!(values(&loaded, "mode"), "allow", "yolo survives a load");
    assert_eq!(values(&loaded, "thinking"), "high");

    // The same ask rides `session/resume`.
    let resumed = h
        .client
        .request(
            "session/resume",
            serde_json::json!({
                "sessionId": params["sessionId"],
                "cwd": params["cwd"],
                "mcpServers": [],
                "_meta": params["_meta"],
            }),
        )
        .unwrap();
    assert_eq!(values(&resumed, "mode"), "allow", "yolo survives a resume");
    assert_eq!(values(&resumed, "thinking"), "high");
}

#[test]
fn a_load_without_the_plugin_host_capability_refuses_client_plugins() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let session = record(
        dir.path(),
        &cwd,
        "mock/recorded",
        1,
        &[message(Role::User, vec![text("hello")], None)],
    );
    let h = serve(vec![text_turn("ok")], dir.path(), dir.path());
    let params = serde_json::json!({
        "sessionId": session,
        "cwd": cwd,
        "mcpServers": [],
        "_meta": { "kage": { "plugins": "client" } },
    });
    let error = h.client.request("session/load", params).unwrap_err();
    assert_eq!(error.code, -32602);
    assert!(error.message.contains("pluginHost"), "{}", error.message);
}

#[test]
fn session_load_replays_the_whole_transcript_and_restores_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let call = ToolCallId::new("call_1");
    let session = record(
        dir.path(),
        &cwd,
        "mock/recorded",
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
                        duration_ms: None,
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
        ["mock/recorded", "high", "default", "off", ""]
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
            "_kage/turn",
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
    assert_eq!(update(7)["title"], "Listing files");
    assert_eq!(update(8)["used"], 410);
    assert_eq!(update(8)["size"], WINDOW);

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
fn a_loaded_session_replays_the_recorded_times() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let t0 = chrono::Utc::now() - chrono::Duration::minutes(5);
    let at = |secs: i64, role: Role, content: Vec<Content>| {
        let mut message = Message::new(role, content, None);
        message.ts = t0 + chrono::Duration::seconds(secs);
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: message.ts,
            message: Arc::new(message),
            usage: None,
        })
    };
    let call = |id: &str| Content::ToolCall {
        id: ToolCallId::new(id),
        name: "ls".into(),
        input: serde_json::json!({ "path": "." }),
    };
    let session = record(
        dir.path(),
        &cwd,
        "mock/recorded",
        1,
        &[
            at(0, Role::User, vec![text("list files")]),
            at(
                3,
                Role::Assistant,
                vec![
                    Content::Thinking {
                        text: "look around".into(),
                        signature: None,
                        duration_ms: Some(2_400),
                    },
                    call("call_1"),
                ],
            ),
            at(
                5,
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new("call_1"),
                    output: "a.txt".into(),
                    is_error: false,
                }],
            ),
            at(6, Role::Assistant, vec![text("Found a.txt.")]),
            at(10, Role::User, vec![text("again")]),
            at(11, Role::Assistant, vec![call("call_2")]),
        ],
    );
    let h = serve(vec![text_turn("ok")], dir.path(), dir.path());

    let params = serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []});
    h.client.request("session/load", params).unwrap();
    let updates = updates_until(&h.inbox, &session, "usage_update");
    let of = |kind: &str| -> Vec<&serde_json::Value> {
        updates
            .iter()
            .map(|p| &p["update"])
            .filter(|u| u["sessionUpdate"] == kind)
            .collect()
    };
    assert_eq!(
        of("agent_thought_chunk")[0]["_meta"]["kage"]["durationMs"],
        2_400
    );
    let ended = of("tool_call_update");
    assert_eq!(ended[0]["toolCallId"], "call_1");
    assert_eq!(ended[0]["_meta"]["kage"]["durationMs"], 2_000);
    let ends = of("_kage/turn");
    assert_eq!(ends.len(), 1, "the run in flight has no end: {ends:?}");
    assert_eq!(ends[0]["phase"], "end");
    assert_eq!(ends[0]["reason"], "no_tool_calls");
    assert_eq!(ends[0]["tookMs"], 6_000);
    assert_eq!(
        ends[0]["at"],
        (t0 + chrono::Duration::seconds(6)).timestamp()
    );
    let kinds = update_kinds(&updates);
    let end_at = kinds.iter().position(|k| *k == "_kage/turn").unwrap();
    assert_eq!(kinds[end_at - 1], "agent_message_chunk");
    assert_eq!(kinds[end_at + 1], "user_message_chunk");
}

#[test]
fn session_resume_skips_the_replay_and_continues_the_history() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let session = record(
        dir.path(),
        &cwd,
        "mock/m",
        1,
        &[
            message(Role::User, vec![text("hello")], None),
            message(Role::Assistant, vec![text("hi")], None),
        ],
    );
    let h = serve(vec![text_turn("sure")], dir.path(), dir.path());

    let params = serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []});
    let resumed = h.client.request("session/resume", params).unwrap();
    assert_eq!(
        current_values(&resumed),
        ["mock/m", "default", "default", "off", ""]
    );
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
        .map(|m| crate::cli_loop_run::first_user_text(m))
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
fn unknown_external_session_is_refused_instead_of_opening_an_engine_session() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());

    for name in ["trusted-agent", ""] {
        let params = serde_json::json!({
            "cwd": dir.path(),
            "mcpServers": [],
            "_meta": {"kage": {"externalAgent": name}}
        });
        let err = h.client.request("session/new", params).unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("external agent"));
        assert!(err.message.contains("temporarily unavailable"));
    }

    let params = serde_json::json!({"cwd": dir.path(), "mcpServers": []});
    let created = h.client.request("session/new", params).unwrap();
    assert!(created["sessionId"].as_str().is_some());
}

#[test]
fn a_new_session_lists_model_thinking_and_mode() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());

    let params = serde_json::json!({"cwd": dir.path(), "mcpServers": []});
    let created = h.client.request("session/new", params).unwrap();
    assert_eq!(
        current_values(&created),
        ["mock/m", "default", "default", "off", ""]
    );
    let options = created["configOptions"].as_array().unwrap();
    let ids: Vec<_> = options.iter().map(|o| o["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["model", "thinking", "mode", "swarm", "goal"]);
    let categories: Vec<_> = options.iter().map(|o| o["category"].clone()).collect();
    assert_eq!(
        categories,
        [
            serde_json::json!("model"),
            serde_json::json!("thought_level"),
            serde_json::json!("mode"),
            serde_json::json!("mode"),
            serde_json::Value::Null,
        ]
    );
    assert_eq!(
        [options[0]["type"].clone(), options[3]["type"].clone()],
        ["select", "select"]
    );
    assert_eq!(options[4]["type"], "text");
    assert!(options[4]["options"].as_array().unwrap().is_empty());
    assert!(options.iter().take(4).all(|o| o["type"] == "select"));
    assert_eq!(values_of(&options[0], "value"), ["mock/m", "mock/other"]);
    assert_eq!(values_of(&options[0], "name"), ["Mock m", "Mock other"]);
    assert_eq!(options[0]["options"][0]["description"], "Mock");
    assert_eq!(
        values_of(&options[1], "value"),
        [
            "default", "off", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"
        ]
    );
    assert_eq!(
        values_of(&options[2], "value"),
        ["default", "ask", "allow", "deny", "plan"]
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
        plan: false,
        swarm: false,
        goal: None,
    };
    let options = config_options(&[], &settings);
    let values: Vec<&str> = options[1]
        .options
        .iter()
        .map(|o| o.value.as_str())
        .collect();
    assert_eq!(values, ["default", "low", "high"]);
    assert_eq!(options[1].options[0].name, "auto");
    assert_eq!(options[1].current_value, "default");
    assert!(settings.apply(&[], "thinking", "medium").is_err());
    assert_eq!(
        settings.apply(&[], "thinking", "high").unwrap(),
        [CommandKind::SetThinking { level: Some(High) }]
    );
    assert_eq!(
        settings.apply(&[], "thinking", "default").unwrap(),
        [CommandKind::SetThinking { level: None }]
    );
    assert_eq!(settings.thinking, None);
}

#[test]
fn the_mode_option_selects_and_leaves_plan_mode() {
    use PermissionAction::Allow;
    let mut settings = Settings {
        model: "m".into(),
        thinking: None,
        levels: Vec::new(),
        mode: None,
        plan: false,
        swarm: false,
        goal: None,
    };
    assert_eq!(settings.mode_id(), "default");
    assert_eq!(
        settings.apply(&[], "mode", "plan").unwrap(),
        [CommandKind::PlanMode { on: true }]
    );
    assert_eq!(settings.mode_id(), "plan");
    assert_eq!(
        settings.apply(&[], "mode", "allow").unwrap(),
        [
            CommandKind::PlanMode { on: false },
            CommandKind::SetPermissionMode { mode: Some(Allow) },
        ]
    );
    assert_eq!(settings.mode_id(), "allow");
    assert_eq!(
        settings.apply(&[], "mode", "default").unwrap(),
        [CommandKind::SetPermissionMode { mode: None }]
    );
    assert_eq!(settings.mode_id(), "default");
    assert!(settings.apply(&[], "mode", "yolo").is_err());
}

#[test]
fn setting_options_changes_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("ok")], dir.path(), dir.path());

    let set = set_option(&h, "thinking", "high").unwrap();
    assert_eq!(
        current_values(&set),
        ["mock/m", "high", "default", "off", ""]
    );
    set_option(&h, "model", "mock/other").unwrap();
    let set = set_option(&h, "mode", "ask").unwrap();
    assert_eq!(
        current_values(&set),
        ["mock/other", "high", "ask", "off", ""]
    );

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
        ("model", "mock/missing"),
        ("thinking", "extreme"),
        ("mode", "yolo"),
        ("swarm", "maybe"),
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

    h.command(model("mock/other"));
    let updates = updates_until(&h.inbox, &h.session, "config_option_update");
    let update = &updates.last().unwrap()["update"];
    assert_eq!(
        current_values(update),
        ["mock/other", "default", "default", "off", ""]
    );

    h.command(model("mock/other"));
    h.command(CommandKind::SetThinking {
        level: Some(ThinkingLevel::Low),
    });
    let updates = updates_until(&h.inbox, &h.session, "config_option_update");
    assert_eq!(update_kinds(&updates), ["config_option_update"]);
    let update = &updates[0]["update"];
    assert_eq!(
        current_values(update),
        ["mock/other", "low", "default", "off", ""]
    );
}

#[test]
fn states_older_than_a_client_change_are_not_sent_back() {
    use super::options::Changed;
    let settings = |model: &str, thinking| Settings {
        model: model.into(),
        thinking: Some(thinking),
        levels: Vec::new(),
        mode: None,
        plan: false,
        swarm: false,
        goal: None,
    };
    let mut changed = Changed::default();
    changed.absorb(&[
        CommandKind::SetModel {
            model: "mock/other".into(),
        },
        CommandKind::SetThinking {
            level: Some(ThinkingLevel::High),
        },
    ]);
    let mut shown = Shown {
        settings: settings("mock/other", ThinkingLevel::High),
        catching_up: true,
        changed,
        swallowed: 0,
    };
    assert!(!shown.observe(&settings("mock/other", ThinkingLevel::Off)));
    assert!(!shown.observe(&settings("mock/m", ThinkingLevel::Off)));
    assert!(!shown.observe(&settings("mock/other", ThinkingLevel::High)));
    assert!(!shown.observe(&settings("mock/other", ThinkingLevel::High)));
    assert!(shown.observe(&settings("mock/m", ThinkingLevel::High)));
    assert_eq!(shown.settings.model, "mock/m");
}

/// A `Shown` catching up on a model change, with every other field
/// requested and settled.
fn catching_up_on_model() -> (Shown, Settings) {
    use super::options::Changed;
    let requested = Settings {
        model: "mock/other".into(),
        thinking: Some(ThinkingLevel::High),
        levels: Vec::new(),
        mode: None,
        plan: false,
        swarm: false,
        goal: Some("ship it".into()),
    };
    let mut changed = Changed::default();
    changed.absorb(&[CommandKind::SetModel {
        model: "mock/other".into(),
    }]);
    let shown = Shown {
        settings: Settings {
            model: "mock/m".into(),
            ..requested.clone()
        },
        catching_up: true,
        changed,
        swallowed: 0,
    };
    (shown, requested)
}

#[test]
fn an_unrelated_change_while_catching_up_is_forwarded_at_once() {
    let (mut shown, requested) = catching_up_on_model();

    // The engine reports a swarm flip it did not owe to the change the
    // client made: it goes out at once and the catch-up ends.
    let unrelated = Settings {
        swarm: true,
        ..requested.clone()
    };
    assert!(shown.observe(&unrelated));
    assert!(!shown.catching_up);
    assert!(shown.settings.swarm);

    // A later state the client already saw is no longer suppressed.
    assert!(!shown.observe(&unrelated));
}

#[test]
fn a_catch_up_that_never_settles_is_forwarded_after_a_bound() {
    use super::options::CATCH_UP_CAP;
    let (mut shown, requested) = catching_up_on_model();

    // States that differ only in the changed field are swallowed
    // while the catch-up is young.
    let mut forwarded = 0;
    for _ in 0..CATCH_UP_CAP {
        forwarded += i32::from(shown.observe(&requested));
    }
    assert_eq!(forwarded, 0, "young catch-ups swallow what may settle");

    // Past the bound the filter gives up, so a change the engine can
    // never reproduce cannot wedge it.
    assert!(shown.observe(&requested));
    assert!(!shown.catching_up);
}

#[test]
fn a_whitespace_goal_converges_instead_of_wedging() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("ok")], dir.path(), dir.path());

    let set = set_option(&h, "goal", "   ").unwrap();
    assert_eq!(set["configOptions"][4]["currentValue"], "");

    // The engine filtered the goal the same way, so a change it
    // reports later still reaches the client.
    h.command(CommandKind::SetModel {
        model: "mock/other".into(),
    });
    let updates = updates_until(&h.inbox, &h.session, "config_option_update");
    let update = updates.last().unwrap()["update"].clone();
    assert_eq!(
        current_values(&update),
        ["mock/other", "default", "default", "off", ""]
    );
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

fn turn_phases(updates: &[serde_json::Value]) -> Vec<(&str, Option<&str>)> {
    updates
        .iter()
        .filter_map(|p| {
            let u = &p["update"];
            (u["sessionUpdate"] == "_kage/turn")
                .then(|| (u["phase"].as_str().unwrap_or(""), u["reason"].as_str()))
        })
        .collect()
}

#[test]
fn a_tool_run_brackets_its_calls_with_turn_updates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let h = serve(
        vec![
            tool_turn("call_1", "ls", serde_json::json!({ "path": path })),
            text_turn("done"),
            text_turn("Listed"),
        ],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let mut updates = Vec::new();
    let (ask, _) = until_ask(&h.inbox, &mut updates);
    allow(&h.client, &ask);
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    updates.extend(drain(&h.inbox));

    assert_eq!(
        turn_phases(&updates),
        [
            ("start", None),
            ("end", Some("tool_calls")),
            ("start", None),
            ("end", Some("no_tool_calls")),
        ]
    );
    let first_start = updates
        .iter()
        .position(|p| p["update"]["sessionUpdate"] == "_kage/turn")
        .unwrap();
    let first_call = updates
        .iter()
        .position(|p| p["update"]["sessionUpdate"] == "tool_call")
        .unwrap();
    assert!(first_start < first_call);
    let last_tool_update = updates
        .iter()
        .rposition(|p| p["update"]["sessionUpdate"] == "tool_call_update")
        .unwrap();
    let last_end = updates
        .iter()
        .rposition(|p| p["update"]["sessionUpdate"] == "_kage/turn")
        .unwrap();
    assert!(last_end > last_tool_update);
}

#[test]
fn a_plain_reply_still_ends_its_turn_without_tools() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![text_turn("hi there"), text_turn("Greeted")],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "hello");
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let updates = drain(&h.inbox);
    assert_eq!(
        turn_phases(&updates),
        [("start", None), ("end", Some("no_tool_calls"))]
    );
}

#[test]
fn a_steer_marked_prompt_lands_at_the_running_runs_next_turn_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let h = serve_paused(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: ToolCallId::new("call_1"),
                name: "ls".into(),
            }),
        ],
        vec![
            Ok(ProviderEvent::ToolCallEnd {
                id: ToolCallId::new("call_1"),
                input: serde_json::json!({ "path": path }),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![text_turn("run one done"), text_turn("Titles")],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    until(|| h.paused.is_parked());

    // Sent while the run is parked mid-turn, so the steer is queued
    // before the ask can be answered and turn two must carry it.
    let steer_end = prompt_async_with(&h.client, &h.session, "hurry", Some("steer"));
    std::thread::sleep(Duration::from_millis(250));
    h.release.send(()).unwrap();

    let mut updates = Vec::new();
    let (ask, _) = until_ask(&h.inbox, &mut updates);
    allow(&h.client, &ask);

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let steered = steer_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(steered["stopReason"], "end_turn");
    until(|| lock(&h.paused.requests).len() >= 2);
    let turn_two = &lock(&h.paused.requests)[1];
    assert_eq!(turn_two.messages.last().unwrap().content, [text("hurry")]);
}

#[test]
fn an_unmarked_prompt_during_a_run_queues_instead_of_steering() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let h = serve(
        vec![
            tool_turn("call_1", "ls", serde_json::json!({ "path": path })),
            text_turn("run one done"),
            text_turn("queued done"),
            text_turn("Titles"),
        ],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    let mut updates = Vec::new();
    let (ask, _) = until_ask(&h.inbox, &mut updates);

    let queued_end = prompt_async(&h.client, &h.session, "later");
    allow(&h.client, &ask);

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let queued = queued_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(queued["stopReason"], "end_turn");
    until(|| h.mock.requests().len() >= 2);
    let turn_two = &h.mock.requests()[1];
    assert_ne!(turn_two.messages.last().unwrap().content, [text("later")]);
    until(|| {
        h.mock.requests().iter().any(|request| {
            request
                .messages
                .iter()
                .any(|m| m.content == [text("later")])
        })
    });
}

#[test]
fn a_close_refusal_during_a_run_reaches_the_client_as_a_warn_notice() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_paused(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "Hel".into(),
            }),
        ],
        vec![
            Ok(ProviderEvent::TextDelta { delta: "lo".into() }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ],
        vec![text_turn("titled")],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    until(|| h.paused.is_parked());
    h.host
        .engine
        .commander()
        .send(Command::to(h.id, CommandKind::Close));

    let updates = updates_until(&h.inbox, &h.session, "_kage/notice");
    let notice = &updates.last().unwrap()["update"];
    assert_eq!(notice["tone"], "warn");
    assert_eq!(
        notice["text"],
        "close: wait for the current run to finish or cancel it"
    );

    h.release.send(()).unwrap();
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
}

#[test]
fn a_compacting_run_reports_kept_and_the_usage_around_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "tiny").unwrap();
    let input = serde_json::json!({ "path": "notes.txt" });
    let h = serve(
        vec![
            tool_turn_with_usage("call_1", "read", input.clone(), 900),
            tool_turn_with_usage("call_2", "read", input, 900),
            text_turn("kept decisions"),
            text_turn_with_usage("after the compaction", 120),
            text_turn("Compacted title"),
        ],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let updates = updates_until(&h.inbox, &h.session, "_kage/compaction");
    let started = &updates.last().unwrap()["update"];
    assert_eq!(started["phase"], "started");
    assert_eq!(started["summarized"], 1);
    assert_eq!(started["kept"], 4);

    let updates = updates_until(&h.inbox, &h.session, "_kage/compaction");
    let update = &updates.last().unwrap()["update"];
    assert_eq!(update["phase"], "complete");
    assert_eq!(update["kept"], 4);
    assert_eq!(update["before"], 900);
    assert_eq!(update["after"], 120);
    assert_eq!(update["summarized"], 1);
    let summary = update["summary"].as_str().unwrap();
    assert!(summary.contains("kept decisions"), "{summary:?}");
    assert!(summary.contains("<summary>"), "{summary:?}");

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
}

#[test]
fn a_compaction_the_run_ends_with_arrives_without_an_after() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "tiny").unwrap();
    let input = serde_json::json!({ "path": "notes.txt" });
    // The third turn is consumed by the summarizer; the script then
    // runs dry, the run fails, and no turn ever reports a fill after
    // the compaction.
    let h = serve(
        vec![
            tool_turn_with_usage("call_1", "read", input.clone(), 900),
            tool_turn_with_usage("call_2", "read", input, 900),
            text_turn("kept decisions"),
        ],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let updates = updates_until(&h.inbox, &h.session, "_kage/compaction");
    assert_eq!(updates.last().unwrap()["update"]["phase"], "started");
    let updates = updates_until(&h.inbox, &h.session, "_kage/compaction");
    let update = &updates.last().unwrap()["update"];
    assert_eq!(update["phase"], "complete");
    assert_eq!(update["before"], 900);
    assert_eq!(update["summarized"], 1);
    assert!(
        update.get("after").is_none(),
        "no turn reported a fill after the compaction: {update}"
    );

    let response = prompt_end.recv_timeout(WAIT).unwrap();
    assert!(response.is_err(), "the exhausted script fails the run");
}

#[test]
fn a_failed_summarizer_aborts_the_compaction() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "tiny").unwrap();
    let input = serde_json::json!({ "path": "notes.txt" });
    let failure: Script = vec![Err(ProviderError::Auth("token expired".into()))];
    let h = serve(
        vec![
            tool_turn_with_usage("call_1", "read", input.clone(), 900),
            tool_turn_with_usage("call_2", "read", input, 900),
            failure,
        ],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let updates = updates_until(&h.inbox, &h.session, "_kage/compaction");
    assert_eq!(updates.last().unwrap()["update"]["phase"], "started");
    let updates = updates_until(&h.inbox, &h.session, "_kage/compaction");
    let update = &updates.last().unwrap()["update"];
    assert_eq!(update["phase"], "aborted");

    let response = prompt_end.recv_timeout(WAIT).unwrap();
    assert!(response.is_err(), "{response:?}");
}

#[test]
fn a_failed_run_sends_an_error_notice_before_the_prompt_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let failure: Script = vec![Err(ProviderError::Auth("token expired".into()))];
    let h = serve(vec![failure], dir.path(), dir.path());
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let updates = updates_until(&h.inbox, &h.session, "_kage/notice");
    let notice = &updates.last().unwrap()["update"];
    assert_eq!(notice["tone"], "error");
    assert_eq!(notice["text"], "authentication failed: token expired");

    let response = prompt_end.recv_timeout(WAIT).unwrap();
    assert!(response.is_err(), "{response:?}");
}

#[test]
fn a_provider_retry_reaches_the_client_as_an_info_notice() {
    let dir = tempfile::tempdir().unwrap();
    let failure: Script = vec![Err(ProviderError::Transport("connection reset".into()))];
    let h = serve(
        vec![failure, text_turn("recovered"), text_turn("Retried title")],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let updates = updates_until(&h.inbox, &h.session, "_kage/notice");
    let notice = &updates.last().unwrap()["update"];
    assert_eq!(notice["tone"], "info");
    assert_eq!(
        notice["text"],
        "provider error (transport: connection reset); retrying 1/4 in 1s"
    );

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
}

/// The client's notifications until the prompt resolves, each with the
/// time it arrived.
#[cfg(unix)]
fn streamed_until_response(
    h: &Harness,
    prompt_end: &mpsc::Receiver<Result<serde_json::Value, kage_jsonrpc::RpcError>>,
) -> (Vec<(Instant, serde_json::Value)>, Vec<serde_json::Value>) {
    let mut stream = Vec::new();
    let deadline = Instant::now() + SHELL_WAIT;
    loop {
        if let Ok(response) = prompt_end.try_recv() {
            assert_eq!(response.unwrap()["stopReason"], "end_turn");
            return (stream, drain(&h.inbox));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "prompt did not resolve");
        let slice = remaining.min(Duration::from_millis(25));
        match h.inbox.recv_timeout(slice) {
            Ok(Inbound::Notification { params, .. }) => stream.push((Instant::now(), params)),
            Err(RecvTimeoutError::Disconnected) => panic!("inbox closed before prompt resolved"),
            Ok(Inbound::Request { .. }) | Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// The status and `raw_output.exit_code` of the final update of `call`.
fn final_shell_update<'a>(
    updates: &'a [(Instant, serde_json::Value)],
    call: &str,
) -> &'a serde_json::Value {
    updates
        .iter()
        .rev()
        .map(|(_, params)| params)
        .find(|p| p["update"]["toolCallId"] == call && p["update"]["status"].is_string())
        .map(|p| &p["update"])
        .expect("no final shell update")
}

#[cfg(unix)]
#[test]
fn shell_progress_streams_the_latest_tail_and_ends_with_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn(
                "call_sh",
                "shell",
                serde_json::json!({
                    "command": "for i in $(seq 1 30); do echo line$i; sleep 0.05; done",
                }),
            ),
            text_turn("done"),
            text_turn("Shell title"),
        ],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    let (stream, after) = streamed_until_response(&h, &prompt_end);

    let mut progress: Vec<(Instant, Vec<usize>)> = Vec::new();
    for (at, params) in &stream {
        let update = &params["update"];
        if update["toolCallId"] != "call_sh" || update["status"].is_string() {
            continue;
        }
        if let Some(text) = update["content"][0]["content"]["text"].as_str() {
            let lines = text
                .lines()
                .map(|l| l.strip_prefix("line").unwrap().parse::<usize>().unwrap())
                .collect::<Vec<_>>();
            progress.push((*at, lines));
        }
    }

    assert!(progress.len() >= 2, "expected several progress ticks");
    let mut last_end = 0;
    for lines in progress.iter().map(|(_, lines)| lines) {
        for pair in lines.windows(2) {
            assert_eq!(pair[1], pair[0] + 1, "tail is not contiguous: {lines:?}");
        }
        assert!(lines.len() <= 10, "tail exceeds ten lines: {lines:?}");
        let end = *lines.last().unwrap();
        assert!(end > last_end, "tail did not advance: {lines:?}");
        assert_eq!(
            lines[0],
            end.saturating_sub(9).max(1),
            "tail is not the latest: {lines:?}"
        );
        last_end = end;
    }
    assert!(
        last_end >= 20,
        "progress stopped tracking early: {last_end}"
    );
    assert_eq!(progress.last().unwrap().1.len(), 10);

    let final_update = final_shell_update(&stream, "call_sh");
    assert_eq!(final_update["status"], "completed");
    assert_eq!(final_update["rawOutput"]["exit_code"], 0);
    assert!(
        after.iter().all(|p| p["update"]["toolCallId"] != "call_sh"),
        "updates after the run ended: {after:?}"
    );
}

#[test]
fn shell_failure_reports_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn(
                "call_sh",
                "shell",
                serde_json::json!({ "command": "echo boom; exit 7" }),
            ),
            text_turn("done"),
            text_turn("Shell title"),
        ],
        dir.path(),
        dir.path(),
    );
    prompt(&h.client, &h.session, "go");
    let updates = drain(&h.inbox);
    let stream: Vec<(Instant, serde_json::Value)> = updates
        .into_iter()
        .map(|params| (Instant::now(), params))
        .collect();
    let final_update = final_shell_update(&stream, "call_sh");
    assert_eq!(final_update["status"], "failed");
    assert_eq!(final_update["rawOutput"]["exit_code"], 7);
}

#[cfg(unix)]
#[test]
fn shell_signal_reports_a_null_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn(
                "call_sh",
                "shell",
                serde_json::json!({ "command": "kill -TERM $$" }),
            ),
            text_turn("done"),
            text_turn("Shell title"),
        ],
        dir.path(),
        dir.path(),
    );
    prompt(&h.client, &h.session, "go");
    let updates = drain(&h.inbox);
    let stream: Vec<(Instant, serde_json::Value)> = updates
        .into_iter()
        .map(|params| (Instant::now(), params))
        .collect();
    let final_update = final_shell_update(&stream, "call_sh");
    assert_eq!(final_update["status"], "failed");
    assert!(final_update["rawOutput"]["exit_code"].is_null());
    let texts: Vec<String> = stream
        .iter()
        .map(|(_, p)| p)
        .filter(|p| p["update"]["toolCallId"] == "call_sh")
        .filter_map(|p| p["update"]["content"][0]["content"]["text"].as_str())
        .map(str::to_owned)
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("exit: signal")),
        "{texts:?}"
    );
}

#[test]
fn mcp_prompts_are_commands_that_expand_when_sent_back() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_with(
        vec![text_turn("ok")],
        dir.path(),
        dir.path(),
        true,
        Modalities::default(),
    );

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
    assert_eq!(
        h.mock.requests()[0].messages[0].content,
        [text("p a=x"), text("x")]
    );

    let params = serde_json::json!({
        "sessionId": h.session,
        "prompt": [{"type": "text", "text": "/srv:p"}],
    });
    let err = h.client.request("session/prompt", params).unwrap_err();
    assert_eq!(err.code, -32602);
    assert_eq!(err.message, "mcp srv:p: missing argument a");
}

#[test]
fn installed_plugins_are_the_scripts_and_packages_the_allowlist_marks() {
    let dir = tempfile::tempdir().unwrap();
    for file in ["b.lua", "a.lua", "@kage.lua", "notes.txt"] {
        std::fs::write(dir.path().join(file), "").unwrap();
    }
    std::fs::create_dir(dir.path().join("c")).unwrap();
    std::fs::write(dir.path().join("c/plugin.toml"), "").unwrap();
    std::fs::create_dir(dir.path().join("data")).unwrap();
    let all = super::installed_plugins(dir.path(), &PluginsConfig::default());
    let names: Vec<_> = all.iter().map(|p| (p.name.as_str(), p.enabled)).collect();
    assert_eq!(names, [("a", true), ("b", true), ("c", true)]);
    let off_a = PluginsConfig {
        disabled: vec!["a".to_owned()],
        ..PluginsConfig::default()
    };
    let names: Vec<_> = super::installed_plugins(dir.path(), &off_a)
        .into_iter()
        .map(|p| (p.name, p.enabled))
        .collect();
    assert_eq!(
        names,
        [("a", false), ("b", true), ("c", true)].map(|(n, on)| (n.to_owned(), on))
    );
    let only_b = PluginsConfig {
        enabled: vec!["b".to_owned()],
        ..PluginsConfig::default()
    };
    let names: Vec<_> = super::installed_plugins(dir.path(), &only_b)
        .into_iter()
        .map(|p| (p.name, p.enabled))
        .collect();
    assert_eq!(
        names,
        [("a", false), ("b", true), ("c", false)].map(|(n, on)| (n.to_owned(), on))
    );
    let missing = dir.path().join("missing");
    assert!(super::installed_plugins(&missing, &PluginsConfig::default()).is_empty());
}

#[test]
fn config_get_serves_the_read_only_sections_without_writing_config() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![], dir.path(), dir.path());

    let result = h
        .client
        .request("_kage/config/get", serde_json::json!({}))
        .unwrap();
    for section in ["providers", "mcp", "permissions", "plugins", "ui"] {
        assert!(result.get(section).is_some(), "missing {section}");
    }
    assert!(!dir.path().join("config.toml").exists());
}

#[test]
fn config_get_and_options_list_read_the_session_workdir() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".kage")).unwrap();
    std::fs::write(
        dir.path().join(".kage/config.toml"),
        "[ui]\ntheme = \"session-workdir-theme\"\n",
    )
    .unwrap();
    let h = serve(vec![], dir.path(), dir.path());
    let theme = |options: &serde_json::Value| {
        options["options"]
            .as_array()
            .unwrap()
            .iter()
            .find(|option| option["name"] == "theme")
            .unwrap()["value"]
            .clone()
    };

    let session = serde_json::json!({"sessionId": h.session});
    let config = h
        .client
        .request("_kage/config/get", session.clone())
        .unwrap();
    assert_eq!(config["ui"]["theme"], "session-workdir-theme");
    let options = h.client.request("_kage/options/list", session).unwrap();
    assert_eq!(theme(&options), "session-workdir-theme");

    let config = h
        .client
        .request("_kage/config/get", serde_json::json!({}))
        .unwrap();
    assert_ne!(config["ui"]["theme"], "session-workdir-theme");
    let options = h
        .client
        .request("_kage/options/list", serde_json::json!({}))
        .unwrap();
    assert_ne!(theme(&options), "session-workdir-theme");
}

#[test]
fn config_get_names_headers_and_env_without_their_values() {
    let mut config = kage_core::config::Config::default();
    config.mcp.servers.insert(
        "gh".into(),
        McpSpec {
            startup_timeout_secs: None,
            command: None,
            args: Vec::new(),
            url: Some("https://mcp.example".into()),
            headers: [("Authorization".to_owned(), "Bearer sk-live".to_owned())].into(),
            env: [("TOKEN".to_owned(), "t0ps3cret".to_owned())].into(),
            disabled: false,
            oauth: None,
            disabled_tools: Vec::new(),
        },
    );
    config.plugins.config.insert(
        "notify".into(),
        serde_json::json!({"webhook": "https://hook"}),
    );
    super::redact_secrets(&mut config);
    let server = &config.mcp.servers["gh"];
    assert_eq!(server.headers["Authorization"], super::REDACTED);
    assert_eq!(server.env["TOKEN"], super::REDACTED);
    assert_eq!(config.plugins.config["notify"], super::REDACTED);
}

/// Writes a config.toml holding the JSON `entry` under `keys`.
fn save_entry(path: &Path, keys: &[&str], entry: &serde_json::Value) {
    let text = kage_core::config_edit::edited(path, keys, Some(entry)).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn config_test_refuses_an_unsaved_agent_instead_of_running_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    // No `[acp.agents.pwned]` on file: the request names one and the
    // saved-spec contract refuses before anything can spawn.
    let err = super::test_saved_acp(&path, "pwned").unwrap_err();
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("save the agent"), "{}", err.message);

    let err = super::test_saved_mcp(&path, "pwned").unwrap_err();
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("save the server"), "{}", err.message);
    assert!(!path.exists());
}

#[test]
fn config_test_probes_the_saved_agent_spec() {
    let answer = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"fake","version":"1.0"}}}"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    save_entry(
        &path,
        &["acp", "agents", "echo"],
        &serde_json::json!({
            "command": "sh",
            "args": ["-c", format!("read line; printf '%s\\n' '{answer}'; sleep 5")],
            "env": {},
        }),
    );
    let result = super::test_saved_acp(&path, "echo").unwrap();
    assert!(result.ok, "{}", result.message);
    assert_eq!(result.agent.as_deref(), Some("fake 1.0"));
}

#[test]
fn config_test_probes_the_saved_server_spec() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    save_entry(
        &path,
        &["mcp", "servers", "gone"],
        &serde_json::json!({ "command": "/nonexistent/config-test-server" }),
    );
    let result = super::test_saved_mcp(&path, "gone").unwrap();
    assert!(!result.ok);
    assert!(!result.message.is_empty(), "{}", result.message);
}

#[test]
fn mcp_status_arrives_per_server_and_only_carries_changes() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_with(
        vec![text_turn("ok")],
        dir.path(),
        dir.path(),
        true,
        Modalities::default(),
    );

    let updates = updates_until(&h.inbox, &h.session, "_kage/mcp_status");
    let statuses: Vec<(&str, &str)> = updates
        .iter()
        .filter(|p| p["update"]["sessionUpdate"] == "_kage/mcp_status")
        .map(|p| {
            (
                p["update"]["name"].as_str().unwrap(),
                p["update"]["status"].as_str().unwrap(),
            )
        })
        .collect();
    let (name, status) = statuses.last().unwrap();
    assert_eq!(*name, "srv");
    assert_eq!(*status, "connected");
    for pair in statuses.windows(2) {
        assert_ne!(pair[0], pair[1], "repeated status: {statuses:?}");
    }
}

#[test]
fn fs_requests_list_and_read_under_the_session_workdir() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hi").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let h = serve(vec![], dir.path(), dir.path());
    let created = h
        .client
        .request(
            "session/new",
            serde_json::json!({"cwd": dir.path().display().to_string()}),
        )
        .unwrap();
    let session = created["sessionId"].as_str().unwrap();

    let list = h
        .client
        .request(
            "_kage/fs",
            serde_json::json!({"sessionId": session, "op": "list", "path": ""}),
        )
        .unwrap();
    assert_eq!(
        list,
        serde_json::json!({
            "op": "list",
            "entries": [
                {"path": "hello.txt", "kind": "file", "size": 2},
                {"path": "sub", "kind": "directory", "size": 0}
            ],
            "truncated": false
        })
    );

    let read = h
        .client
        .request(
            "_kage/fs",
            serde_json::json!({"sessionId": session, "op": "read", "path": "hello.txt"}),
        )
        .unwrap();
    assert_eq!(
        read,
        serde_json::json!({
            "op": "read",
            "content": "hi",
            "truncated": false,
            "binary": false
        })
    );

    let err = h
        .client
        .request(
            "_kage/fs",
            serde_json::json!({"sessionId": session, "op": "read", "path": "../escape"}),
        )
        .unwrap_err();
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("escapes workdir"));
}

#[test]
fn a_todo_write_reaches_the_client_as_a_plan_update() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn(
                "call_1",
                "todo_list",
                serde_json::json!({"todos": [
                    {"title": "Read", "status": "in_progress", "id": "1"},
                    {"title": "Write", "status": "pending", "owner": "agent", "blockedBy": ["1"]},
                    {"title": "Ship", "status": "done"}
                ]}),
            ),
            text_turn("planned"),
            text_turn("Titles"),
        ],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let updates = updates_until(&h.inbox, &h.session, "plan");
    let plan = &updates.last().unwrap()["update"];
    assert_eq!(
        plan["entries"],
        serde_json::json!([
            {
                "content": "Read",
                "priority": "medium",
                "status": "in_progress",
                "_meta": {"kage": {"id": "1"}}
            },
            {
                "content": "Write",
                "priority": "medium",
                "status": "pending",
                "_meta": {"kage": {"owner": "agent", "blockedBy": ["1"]}}
            },
            {"content": "Ship", "priority": "medium", "status": "completed"}
        ])
    );
    prompt_end.recv_timeout(WAIT).unwrap().unwrap();
}

#[test]
fn a_todo_read_sends_no_plan_update() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn("call_1", "todo_list", serde_json::json!({})),
            text_turn("read it"),
            text_turn("Titles"),
        ],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let drained = drain(&h.inbox);
    assert!(
        !update_kinds(&drained).contains(&"plan"),
        "{:?}",
        update_kinds(&drained)
    );
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
    let session = record(dir.path(), &cwd, "mock/m", 1, &[]);
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
    let dir = tempfile::tempdir().unwrap();
    let (srv_r, mut cli_w) = std::io::pipe().unwrap();
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let params = serde_json::json!({"cwd": dir.path(), "mcpServers": []});
    let request =
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": params});
    writeln!(cli_w, "{request}").unwrap();
    let provider = Listed::of(MockProvider::sequence(Vec::new()));
    let host = test_host(
        Arc::new(provider),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        true,
    );
    std::thread::spawn(move || host.serve(BufReader::new(srv_r), srv_w).unwrap());

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(cli_r).lines() {
            if tx
                .send(serde_json::from_str::<serde_json::Value>(&line.unwrap()).unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    let mut lines = Vec::new();
    loop {
        match rx.recv_timeout(WAIT).expect("no session/new answer") {
            line if line["id"] == 1 => {
                lines.push(line);
                break;
            }
            line => lines.push(line),
        }
    }
    loop {
        match rx.recv_timeout(WAIT).expect("no session updates") {
            line if line["params"]["sessionId"].is_string() => {
                lines.push(line);
                break;
            }
            line => lines.push(line),
        }
    }
    drop(cli_w);
    while let Ok(line) = rx.recv_timeout(WAIT) {
        lines.push(line);
    }
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
fn usage_updates_carry_tokens_and_cost_only_when_priced() {
    let mut usage = Usage {
        context_used: 50,
        context_window: 200,
        ..Usage::default()
    };
    let Some(SessionUpdate::UsageUpdate(update)) = usage_update(&usage) else {
        panic!("no usage_update");
    };
    assert_eq!((update.used, update.size, update.cost), (50, 200, None));
    usage.total.input = 30;
    usage.total.cache_read = 12;
    let Some(SessionUpdate::UsageUpdate(update)) = usage_update(&usage) else {
        panic!("no usage_update");
    };
    let tokens = update.meta.and_then(|meta| meta.kage).unwrap();
    assert_eq!((tokens.input, tokens.cache_read), (30, 12));
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

/// The per-session held buffer keeps the first updates and drops
/// later ones, so an unannounced session cannot grow it without
/// bound.
#[test]
fn held_updates_stop_at_a_hard_cap() {
    use super::bridge::{HELD_CAP, HeldUpdates, hold};
    use kage_acp::acp::{ContentBlock, MessageChunk};

    let held = Held::default();
    let session = SessionId::new();
    lock(&held).insert(session, HeldUpdates::default());
    let chunk = |n: u64| {
        SessionUpdate::AgentMessageChunk(MessageChunk {
            content: ContentBlock::text(n.to_string()),
            meta: None,
        })
    };
    for n in 0..HELD_CAP as u64 + 100 {
        let mut held = lock(&held);
        hold(held.get_mut(&session).unwrap(), chunk(n));
    }
    let updates = lock(&held).remove(&session).unwrap();
    assert_eq!(updates.updates.len(), HELD_CAP);
    assert_eq!(updates.updates[0], chunk(0));
    assert_eq!(updates.updates.last().unwrap(), &chunk(HELD_CAP as u64 - 1));
    assert_eq!(updates.dropped, 100);
}

/// The agent side of a pipe pair, and the inbox its notifications
/// land in.
fn peer_inbox() -> (Peer, mpsc::Receiver<Inbound>) {
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (agent, _agent_in, _reader) = kage_jsonrpc::connect(BufReader::new(srv_r), srv_w);
    let (_client, inbox, _client_reader) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
    (agent, inbox)
}

#[test]
fn a_flushed_held_buffer_reports_what_the_cap_dropped() {
    use super::bridge::{HELD_CAP, HeldUpdates, flush_held, hold};
    use kage_acp::acp::{ContentBlock, MessageChunk};

    let (peer, inbox) = peer_inbox();
    let chunk = |n: usize| {
        SessionUpdate::AgentMessageChunk(MessageChunk {
            content: ContentBlock::text(n.to_string()),
            meta: None,
        })
    };
    let mut held = HeldUpdates::default();
    for n in 0..HELD_CAP + 3 {
        hold(&mut held, chunk(n));
    }
    // The pipe plus the inbound channel hold far less than HELD_CAP
    // updates, so the flush must run while this thread drains.
    let flush_peer = peer.clone();
    let flusher = std::thread::spawn(move || flush_held(&flush_peer.into(), "c1", held));

    let mut seen = 0;
    let marker = loop {
        let Inbound::Notification { params, .. } = inbox.recv_timeout(WAIT).expect("no update")
        else {
            continue;
        };
        let text = params["update"]["content"]["text"].as_str().unwrap_or("");
        if text.contains("were dropped") {
            break text.to_owned();
        }
        seen += 1;
    };
    flusher.join().unwrap();
    assert_eq!(seen, HELD_CAP, "every kept update is replayed first");
    assert!(marker.contains("3 updates were dropped"), "{marker}");
}

#[test]
fn an_under_cap_flush_sends_no_marker() {
    use super::bridge::{HeldUpdates, flush_held, hold};
    use kage_acp::acp::{ContentBlock, MessageChunk};

    let (peer, inbox) = peer_inbox();
    let mut held = HeldUpdates::default();
    hold(
        &mut held,
        SessionUpdate::AgentMessageChunk(MessageChunk {
            content: ContentBlock::text("only"),
            meta: None,
        }),
    );
    let flush_peer = peer.clone();
    let flusher = std::thread::spawn(move || flush_held(&flush_peer.into(), "c1", held));
    flusher.join().unwrap();

    let mut texts = Vec::new();
    while let Ok(Inbound::Notification { params, .. }) =
        inbox.recv_timeout(Duration::from_millis(300))
    {
        texts.push(
            params["update"]["content"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        );
    }
    assert_eq!(texts, ["only"]);
}

#[test]
fn a_malformed_question_ask_shows_as_an_ordinary_permission() {
    use super::bridge::{AskKind, ask_kind, permission_call, question_prompts};

    let malformed = serde_json::json!({"questions": "nope"});
    let tool_call = permission_call(
        Some(&ToolCallId::new("call_q")),
        "ask_user_question",
        &malformed,
    );
    let kind = ask_kind("ask_user_question", &malformed, tool_call);
    let AskKind::Permission(update) = kind else {
        panic!("a malformed questions array must not spawn questions");
    };
    assert_eq!(update.raw_input.unwrap(), malformed);

    assert_eq!(question_prompts(&malformed), None);
    assert_eq!(
        question_prompts(&serde_json::json!({"questions": []})),
        None
    );
    let valid = serde_json::json!({"questions": [
        {"header": "Store", "question": "Where?",
         "options": [{"label": "Disk", "description": "Kept"}]}
    ]});
    assert_eq!(question_prompts(&valid).unwrap().len(), 1);
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
    // The running call carries the complete input, so a client showing
    // the call as it runs can name what is running.
    let SessionUpdate::ToolCallUpdate(update) = &updates[2] else {
        unreachable!()
    };
    assert_eq!(
        update.raw_input,
        Some(serde_json::json!({ "command": "ls" }))
    );
}

#[test]
fn a_finished_edit_carries_its_changes_as_diffs() {
    let id = ToolCallId::new("call_edit");
    let mut seen = HashMap::new();
    let input = serde_json::json!({
        "path": "src/a.rs",
        "changes": [
            {"old_str": "a", "new_str": "b"},
            {"range": {"start": 2, "end": 2}, "text": "c\n"},
        ],
    });
    to_update(
        &mut seen,
        &LoopEvent::ToolCallStart {
            id: id.clone(),
            name: "edit".into(),
            input_partial: input,
        },
    );
    let Some(SessionUpdate::ToolCallUpdate(update)) = to_update(
        &mut seen,
        &LoopEvent::ToolCallEnd {
            id,
            output: ToolOutput {
                text: "edited".into(),
                ..ToolOutput::default()
            },
        },
    ) else {
        panic!("the end is an update");
    };
    let diffs: Vec<_> = update
        .content
        .iter()
        .filter_map(|content| match content {
            kage_acp::acp::ToolCallContent::Diff(diff) => Some(diff),
            _ => None,
        })
        .collect();
    assert_eq!(diffs.len(), 1, "a line-range change names no old text");
    assert_eq!(diffs[0].path, "src/a.rs");
    assert_eq!(diffs[0].old_text.as_deref(), Some("a"));
    assert_eq!(diffs[0].new_text, "b");
}

#[test]
fn a_streamed_swarm_call_sends_its_meta_only_when_it_changes() {
    let id = ToolCallId::new("call_swarm");
    let inputs = [
        serde_json::json!({"description": "fan"}),
        serde_json::json!({"description": "fan", "items": ["a"]}),
        serde_json::json!({"description": "fan", "items": ["a"], "agent": "general"}),
        serde_json::json!({"description": "fan", "items": ["a"], "agent": "general", "fork": true}),
        serde_json::json!({"description": "fan", "items": ["a", "b"], "agent": "general", "fork": true}),
    ];
    let mut seen = HashMap::new();
    let members: Vec<Option<Vec<String>>> = inputs
        .into_iter()
        .map(|input_partial| {
            let update = to_update(
                &mut seen,
                &LoopEvent::ToolCallArgsDelta {
                    id: id.clone(),
                    name: "swarm".into(),
                    input_partial,
                },
            );
            let meta = match update {
                Some(SessionUpdate::ToolCall(call)) => call.meta,
                Some(SessionUpdate::ToolCallUpdate(update)) => update.meta,
                other => panic!("expected a tool call update, got {other:?}"),
            };
            meta.and_then(|meta| meta.kage.swarm)
                .map(|swarm| swarm.members)
        })
        .collect();
    assert_eq!(
        members,
        [
            None,
            Some(vec!["a".to_owned()]),
            None,
            None,
            Some(vec!["a".to_owned(), "b".to_owned()]),
        ]
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
    assert_eq!(tool_kind("shell"), ToolKind::Execute);
    assert_eq!(tool_kind("grep"), ToolKind::Search);
    assert_eq!(tool_kind("github__create_issue"), ToolKind::Other);
}

fn chunk_texts(updates: &[serde_json::Value]) -> Vec<String> {
    updates
        .iter()
        .filter(|p| p["update"]["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|p| p["update"]["content"]["text"].as_str())
        .map(str::to_owned)
        .collect()
}

/// Waits until `call` holds, so an async side effect (a generated title
/// taking the next provider script) settles before the test moves on.
fn until(call: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + WAIT;
    while !call() {
        assert!(
            std::time::Instant::now() < deadline,
            "condition not reached"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Attaches the harness client and a second connection to the recorded
/// `session`, one connection each on the same host.
fn attach_both(h: &Harness, c2: &Connection, session: &str, cwd: &str) {
    let params = serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []});
    h.client.request("session/resume", params.clone()).unwrap();
    c2.client.request("session/load", params).unwrap();
}

/// Waits for the `$/cancel_request` withdrawing `ask`.
fn wait_cancel(inbox: &mpsc::Receiver<Inbound>, ask: &serde_json::Value) {
    loop {
        match inbox.recv_timeout(WAIT).expect("no cancel") {
            Inbound::Notification { method, params }
                if method == "$/cancel_request" && params["requestId"] == *ask =>
            {
                return;
            }
            _ => {}
        }
    }
}

/// A connection driven by hand, so the test can close it while a
/// request is still out.
struct RawClient {
    writer: std::io::PipeWriter,
    reader: BufReader<std::io::PipeReader>,
}

impl RawClient {
    fn send(&mut self, id: u64, method: &str, params: &serde_json::Value) {
        let request =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.writer, "{request}").unwrap();
    }

    /// The next message, or `None` when the connection closed.
    fn recv(&mut self) -> Option<serde_json::Value> {
        let mut line = String::new();
        self.reader.read_line(&mut line).ok().filter(|n| *n > 0)?;
        Some(serde_json::from_str(&line).unwrap())
    }

    /// Reads until the `session/request_permission` request arrives.
    fn until_ask(&mut self) -> serde_json::Value {
        loop {
            let message = self.recv().expect("no permission request");
            if message["method"] == "session/request_permission" {
                return message["id"].clone();
            }
        }
    }
}

impl Harness {
    /// Opens a second connection on the same host, driven by hand.
    fn connect_raw(&self) -> RawClient {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let host = Arc::clone(&self.host);
        std::thread::spawn(move || host.serve(BufReader::new(srv_r), srv_w).unwrap());
        RawClient {
            writer: cli_w,
            reader: BufReader::new(cli_r),
        }
    }
}

#[test]
fn the_first_answer_wins_and_the_other_ask_is_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let path = dir.path().display().to_string();
    let session = record(dir.path(), &cwd, "mock/m", 0, &[]);
    let h = serve(
        vec![
            tool_turn("call_1", "ls", serde_json::json!({ "path": path })),
            text_turn("done"),
        ],
        dir.path(),
        dir.path(),
    );
    let c2 = h.connect();
    attach_both(&h, &c2, &session, &cwd);
    let prompt_end = prompt_async(&h.client, &session, "go");

    let mut first = Vec::new();
    let (ask_1, params_1) = until_ask(&h.inbox, &mut first);
    let mut second = Vec::new();
    let (ask_2, params_2) = until_ask(&c2.inbox, &mut second);
    assert_eq!(params_1["sessionId"], session);
    assert_eq!(params_2["sessionId"], session);
    assert_eq!(params_1["toolCall"]["toolCallId"], "call_1");
    assert_eq!(params_2["toolCall"]["toolCallId"], "call_1");

    allow(&h.client, &ask_1);
    wait_cancel(&c2.inbox, &ask_2);
    allow(&c2.client, &ask_2);

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let mut all = first;
    all.extend(drain(&h.inbox));
    let announced = all
        .iter()
        .filter(|p| {
            p["update"]["sessionUpdate"] == "tool_call" && p["update"]["toolCallId"] == "call_1"
        })
        .count();
    assert_eq!(announced, 1, "the tool ran more than once");
}

#[test]
fn a_connection_that_closes_while_asked_sends_no_decision() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let path = dir.path().display().to_string();
    let session = record(dir.path(), &cwd, "mock/m", 0, &[]);
    let h = serve(
        vec![
            tool_turn("call_1", "ls", serde_json::json!({ "path": path })),
            text_turn("done"),
        ],
        dir.path(),
        dir.path(),
    );
    let c2 = h.connect();
    let mut raw = h.connect_raw();

    let params = serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []});
    raw.send(1, "session/resume", &params);
    while raw.recv().is_some_and(|m| m["id"] != 1) {}
    let params = serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []});
    c2.client.request("session/load", params).unwrap();

    let params = serde_json::json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": "go"}],
    });
    raw.send(2, "session/prompt", &params);
    raw.until_ask();
    let (ask_2, _) = until_ask(&c2.inbox, &mut Vec::new());

    drop(raw);

    // The closed connection sent no decision: the engine request is
    // still open, so the bystander's answer is what runs the tool.
    assert!(c2.inbox.recv_timeout(Duration::from_millis(300)).is_err());
    allow(&c2.client, &ask_2);
    let seen = updates_until(&c2.inbox, &session, "agent_message_chunk");
    assert_eq!(chunk_texts(&seen).last().unwrap(), "done");
}

#[test]
fn a_prompt_from_another_client_is_refused_while_a_run_is_out() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let path = dir.path().display().to_string();
    let session = record(dir.path(), &cwd, "mock/m", 0, &[]);
    let h = serve(
        vec![
            tool_turn("call_1", "ls", serde_json::json!({ "path": path })),
            text_turn("done"),
            text_turn("run title"),
            text_turn("again"),
        ],
        dir.path(),
        dir.path(),
    );
    let c2 = h.connect();
    attach_both(&h, &c2, &session, &cwd);
    let prompt_end = prompt_async(&h.client, &session, "go");

    let (ask_1, _) = until_ask(&h.inbox, &mut Vec::new());
    let params = serde_json::json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": "me too"}],
    });
    let refused = c2.client.request("session/prompt", params).unwrap_err();
    assert_eq!(refused.code, -32603);
    assert!(refused.message.contains("busy"), "{}", refused.message);

    allow(&h.client, &ask_1);
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    until(|| h.mock.call_count() >= 3);

    assert_eq!(
        prompt(&c2.client, &session, "again")["stopReason"],
        "end_turn"
    );
}

#[test]
fn the_non_owner_sees_the_prompt_and_the_owner_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let session = record(dir.path(), &cwd, "mock/m", 0, &[]);
    let h = serve(
        vec![
            text_turn("hi there"),
            text_turn("first title"),
            text_turn("look reply"),
        ],
        dir.path(),
        dir.path(),
    );
    let c2 = h.connect();
    attach_both(&h, &c2, &session, &cwd);

    let params = serde_json::json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": "hello"}],
    });
    let response = h.client.request("session/prompt", params).unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    until(|| h.mock.call_count() >= 2);

    let owner = drain(&h.inbox);
    assert!(
        !owner
            .iter()
            .any(|p| p["update"]["sessionUpdate"] == "user_message_chunk")
    );
    let other = updates_until(&c2.inbox, &session, "agent_message_chunk");
    let echo = other
        .iter()
        .position(|p| p["update"]["sessionUpdate"] == "user_message_chunk")
        .expect("no echo");
    let reply = other
        .iter()
        .position(|p| p["update"]["sessionUpdate"] == "agent_message_chunk")
        .expect("no reply");
    assert!(echo < reply);
    assert_eq!(other[echo]["update"]["content"]["text"], "hello");

    let params = serde_json::json!({
        "sessionId": session,
        "prompt": [
            {"type": "text", "text": "look"},
            {"type": "image", "data": "aGk=", "mimeType": "image/png"},
        ],
    });
    let response = h.client.request("session/prompt", params).unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    until(|| h.mock.call_count() >= 3);

    let other = drain(&c2.inbox);
    let echoed: Vec<_> = other
        .iter()
        .filter(|p| p["update"]["sessionUpdate"] == "user_message_chunk")
        .map(|p| p["update"]["content"].clone())
        .collect();
    assert_eq!(echoed.len(), 2, "{other:?}");
    assert_eq!(echoed[0]["text"], "look");
    assert_eq!(echoed[1]["type"], "image");
    assert_eq!(echoed[1]["data"], "aGk=");
}

#[test]
fn the_owner_sees_an_agent_burst_with_prose_between_reports() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let session = record(dir.path(), &cwd, "mock/m", 0, &[]);
    let burst = format!(
        "<agent name=\"alpha\" session=\"{}\" state=\"completed\">\nfirst body\n</agent>\
         \n\nwords between\n\n<agent name=\"beta\" session=\"{}\" state=\"completed\">\nsecond \
         body\n</agent>",
        "01ARZ3NDEKTSV4RRFFQ69G5FAV", "01ARZ3NDEKTSV4RRFFQ69G5FAW"
    );
    let h = serve(
        vec![text_turn("reply"), text_turn("title")],
        dir.path(),
        dir.path(),
    );
    let params = serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []});
    h.client.request("session/resume", params).unwrap();

    let params = serde_json::json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": burst}],
    });
    let response = h.client.request("session/prompt", params).unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    until(|| h.mock.call_count() >= 2);

    let echoed: Vec<String> = drain(&h.inbox)
        .iter()
        .filter(|p| p["update"]["sessionUpdate"] == "user_message_chunk")
        .filter_map(|p| p["update"]["content"]["text"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(echoed.len(), 1, "{echoed:?}");
    assert_eq!(echoed[0], burst);
    assert_eq!(
        kage_core::agent_report::AgentReport::all_in(&echoed[0]).len(),
        2
    );
}

#[test]
fn a_cancel_from_the_second_client_ends_the_owners_run() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let path = dir.path().display().to_string();
    let session = record(dir.path(), &cwd, "mock/m", 0, &[]);
    let h = serve(
        vec![
            tool_turn("call_1", "ls", serde_json::json!({ "path": path })),
            text_turn("done"),
        ],
        dir.path(),
        dir.path(),
    );
    let c2 = h.connect();
    attach_both(&h, &c2, &session, &cwd);
    let prompt_end = prompt_async(&h.client, &session, "go");

    let (_ask, _) = until_ask(&h.inbox, &mut Vec::new());
    c2.client
        .notify("session/cancel", serde_json::json!({"sessionId": session}))
        .unwrap();

    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "cancelled");
}

#[test]
fn a_new_session_is_recorded_in_the_host_sessions_directory() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![text_turn("hi"), text_turn("title")],
        dir.path(),
        dir.path(),
    );
    let params = serde_json::json!({"cwd": dir.path(), "mcpServers": []});
    let created = h.client.request("session/new", params).unwrap();
    let session = created["sessionId"].as_str().unwrap().to_owned();
    assert_eq!(prompt(&h.client, &session, "hi")["stopReason"], "end_turn");
    let file = dir.path().join(format!("{session}.jsonl"));
    until(|| file.is_file());
}

#[test]
fn two_connections_prompt_their_own_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            text_turn("one"),
            text_turn("one title"),
            text_turn("two"),
            text_turn("two title"),
        ],
        dir.path(),
        dir.path(),
    );
    let c2 = h.connect();
    let params = serde_json::json!({"cwd": dir.path(), "mcpServers": []});
    let mine = h.client.request("session/new", params.clone()).unwrap();
    let theirs = c2.client.request("session/new", params).unwrap();
    let mine = mine["sessionId"].as_str().unwrap().to_owned();
    let theirs = theirs["sessionId"].as_str().unwrap().to_owned();
    assert_ne!(mine, theirs);

    assert_eq!(prompt(&h.client, &mine, "hi")["stopReason"], "end_turn");
    until(|| h.mock.call_count() >= 2);
    assert_eq!(prompt(&c2.client, &theirs, "hi")["stopReason"], "end_turn");
    until(|| h.mock.call_count() >= 4);
    assert_eq!(chunk_texts(&drain(&h.inbox)), ["one"]);
    assert_eq!(chunk_texts(&drain(&c2.inbox)), ["two"]);
}

#[test]
fn a_second_connection_attaches_to_an_open_session() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let session = record(dir.path(), &cwd, "mock/m", 0, &[]);
    let h = serve(vec![text_turn("attached reply")], dir.path(), dir.path());
    let c2 = h.connect();

    let params = serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []});
    h.client.request("session/resume", params.clone()).unwrap();
    let loaded = c2.client.request("session/load", params).unwrap();
    assert_eq!(
        current_values(&loaded),
        ["mock/m", "default", "default", "off", ""]
    );

    let prompt_end = prompt_async(&h.client, &session, "hello");
    updates_until(&h.inbox, &session, "agent_message_chunk");
    updates_until(&c2.inbox, &session, "agent_message_chunk");
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");

    let hosted = h.host.engine.hosted_sessions();
    assert_eq!(
        hosted
            .iter()
            .filter(|(id, _)| id.to_string() == session)
            .count(),
        1,
        "{hosted:?}"
    );
}

#[test]
fn a_connection_that_ends_leaves_its_run_running() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let path = dir.path().display().to_string();
    let session = record(dir.path(), &cwd, "mock/m", 0, &[]);
    let h = serve(
        vec![
            tool_turn("call_1", "ls", serde_json::json!({ "path": path })),
            text_turn("done"),
        ],
        dir.path(),
        dir.path(),
    );
    let c2 = h.connect();
    let params = serde_json::json!({"sessionId": session, "cwd": cwd, "mcpServers": []});
    h.client.request("session/resume", params.clone()).unwrap();
    c2.client.request("session/load", params).unwrap();
    let prompt_end = prompt_async(&h.client, &session, "go");

    let mut updates = Vec::new();
    let (_, params) = until_ask(&h.inbox, &mut updates);
    assert_eq!(params["sessionId"], session);
    let (ask_2, _) = until_ask(&c2.inbox, &mut Vec::new());

    let Harness { client, inbox, .. } = h;
    drop(client);
    drop(inbox);

    // The run outlives its client: the bystander answers the still open
    // ask and sees the rest of the turn.
    allow(&c2.client, &ask_2);
    let seen = updates_until(&c2.inbox, &session, "agent_message_chunk");
    assert_eq!(chunk_texts(&seen).last().unwrap(), "done");
    // The prompt's client is gone, so its request fails even though the
    // run it started completed.
    let response = prompt_end.recv_timeout(WAIT).unwrap();
    assert!(response.is_err(), "{response:?}");
}

/// A provider whose first stream parks on a channel between `head` and
/// `tail`, so a test can attach a second connection while the turn is
/// still streaming. Every later stream takes one script from `rest`,
/// and every request is recorded.
struct Paused {
    metadata: kage_provider::ProviderMetadata,
    requests: Mutex<Vec<kage_provider::StreamRequest>>,
    head: Script,
    tail: Script,
    rest: Mutex<Vec<Script>>,
    gate: Mutex<Option<mpsc::Receiver<()>>>,
    parked: Arc<AtomicBool>,
}

impl std::fmt::Debug for Paused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Paused").finish_non_exhaustive()
    }
}

impl Paused {
    fn new(head: Script, tail: Script, rest: Vec<Script>, gate: mpsc::Receiver<()>) -> Self {
        Self {
            metadata: kage_provider::ProviderMetadata {
                id: "mock".into(),
                display_name: "Mock".into(),
                supports_caching: false,
                supports_thinking: true,
                supports_tool_use: true,
            },
            requests: Mutex::new(Vec::new()),
            head,
            tail,
            rest: Mutex::new(rest),
            gate: Mutex::new(Some(gate)),
            parked: Arc::default(),
        }
    }

    /// Whether the first stream is waiting on the gate.
    fn is_parked(&self) -> bool {
        self.parked.load(Ordering::SeqCst)
    }
}

impl kage_provider::Provider for Paused {
    fn metadata(&self) -> &kage_provider::ProviderMetadata {
        &self.metadata
    }

    fn stream(
        &self,
        req: kage_provider::StreamRequest,
        _cancel: &kage_core::CancelFlag,
    ) -> Result<kage_provider::EventStream, ProviderError> {
        lock(&self.requests).push(req);
        let first = lock(&self.requests).len() == 1;
        let (head, tail, gate) = if first {
            (
                self.head.clone(),
                self.tail.clone(),
                lock(&self.gate).take(),
            )
        } else {
            let mut rest = lock(&self.rest);
            let script = if rest.is_empty() {
                Vec::new()
            } else {
                rest.remove(0)
            };
            (Vec::new(), script, None)
        };
        Ok(Box::new(PausedStream {
            head: head.into_iter(),
            tail: tail.into_iter(),
            gate,
            parked: Arc::clone(&self.parked),
        }))
    }

    fn models(&self) -> Vec<kage_provider::ProviderModel> {
        ["m", "other"]
            .map(|id| kage_provider::ProviderModel {
                id: id.into(),
                name: format!("Mock {id}"),
                input: Modalities::default(),
                ..kage_provider::ProviderModel::default()
            })
            .into()
    }
}

/// The stream that parks between its head and its tail.
struct PausedStream {
    head: std::vec::IntoIter<Result<ProviderEvent, ProviderError>>,
    tail: std::vec::IntoIter<Result<ProviderEvent, ProviderError>>,
    gate: Option<mpsc::Receiver<()>>,
    parked: Arc<AtomicBool>,
}

impl Iterator for PausedStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(event) = self.head.next() {
            return Some(event);
        }
        if let Some(gate) = self.gate.take() {
            self.parked.store(true, Ordering::SeqCst);
            let _ = gate.recv();
        }
        self.tail.next()
    }
}

/// The client side of a served `kage rpc` on a [`Paused`] provider,
/// with the standing session recorded in `sessions` under its own id,
/// so a second connection can load it. `release` lets the parked
/// stream finish its `tail`.
struct PausedHarness {
    host: Arc<Host>,
    client: Peer,
    inbox: mpsc::Receiver<Inbound>,
    paused: Arc<Paused>,
    release: mpsc::Sender<()>,
    id: SessionId,
    session: String,
    /// The `session/load` and `session/resume` params for the standing
    /// session.
    resume: serde_json::Value,
}

/// [`serve`] on a [`Paused`] provider, with the standing session's
/// file holding one recorded user message so loads find it.
fn serve_paused(
    head: Script,
    tail: Script,
    rest: Vec<Script>,
    workdir: &Path,
    sessions: &Path,
) -> PausedHarness {
    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let id = SessionId::new();
    let cwd = workdir.display().to_string();
    record_as(
        sessions,
        id,
        &cwd,
        "mock/m",
        0,
        &[message(Role::User, vec![text("hello")], None)],
    );
    let (release, gate) = mpsc::channel();
    let paused = Arc::new(Paused::new(head, tail, rest, gate));
    let host = test_host(
        paused.clone(),
        workdir.to_path_buf(),
        sessions.to_path_buf(),
        false,
    );
    let standing = Arc::clone(&host);
    let (opened_tx, opened_rx) = mpsc::channel();
    std::thread::spawn(move || {
        standing
            .serve_with(BufReader::new(srv_r), srv_w, move |agent| {
                let (spec, runtime) = (agent.host.spec)(
                    &agent.host.registry(),
                    id,
                    "",
                    "mock/m",
                    BTreeMap::new(),
                    &Environment::inherit(),
                    None,
                    None,
                )
                .unwrap();
                agent.open(id.to_string(), spec, runtime);
                agent.session_announced(&id.to_string());
                let _ = opened_tx.send(());
            })
            .unwrap();
    });
    let (client, inbox, _reader) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
    opened_rx.recv_timeout(WAIT).unwrap();
    PausedHarness {
        host,
        client,
        inbox,
        paused,
        release,
        id,
        session: id.to_string(),
        resume: serde_json::json!({"sessionId": id.to_string(), "cwd": cwd, "mcpServers": []}),
    }
}

#[test]
fn attaching_mid_turn_shows_the_file_then_the_flight_then_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let h = serve_paused(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "Hel".into(),
            }),
            Ok(ProviderEvent::ToolCallStart {
                id: ToolCallId::new("call_1"),
                name: "ls".into(),
            }),
        ],
        vec![
            Ok(ProviderEvent::ToolCallEnd {
                id: ToolCallId::new("call_1"),
                input: serde_json::json!({ "path": cwd }),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![text_turn("done"), text_turn("titled")],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    until(|| h.paused.is_parked());

    let c2 = connect(&h.host);
    c2.client.request("session/load", h.resume.clone()).unwrap();
    let updates = updates_until(&c2.inbox, &h.session, "tool_call");
    assert_eq!(
        update_kinds(&updates),
        ["user_message_chunk", "agent_message_chunk", "tool_call"]
    );
    assert_eq!(chunk_texts(&updates), ["Hel"]);
    assert_eq!(updates[2]["update"]["toolCallId"], "call_1");
    assert_eq!(updates[2]["update"]["status"], "pending");

    h.release.send(()).unwrap();
    let mut asked = Vec::new();
    let (ask_1, params) = until_ask(&h.inbox, &mut asked);
    assert_eq!(params["toolCall"]["toolCallId"], "call_1");
    let (ask_2, _) = until_ask(&c2.inbox, &mut Vec::new());
    allow(&h.client, &ask_1);
    wait_cancel(&c2.inbox, &ask_2);

    let rest = updates_until(&c2.inbox, &h.session, "agent_message_chunk");
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let mut all = updates;
    all.extend(rest);
    all.extend(drain(&c2.inbox));
    assert_eq!(chunk_texts(&all), ["Hel", "done"]);
    let statuses: Vec<&str> = all
        .iter()
        .filter(|p| p["update"]["toolCallId"] == "call_1")
        .filter_map(|p| p["update"]["status"].as_str())
        .collect();
    assert_eq!(statuses, ["pending", "in_progress", "completed"]);
    let announced = all
        .iter()
        .filter(|p| {
            p["update"]["sessionUpdate"] == "tool_call" && p["update"]["toolCallId"] == "call_1"
        })
        .count();
    assert_eq!(announced, 1, "the tool call was announced twice");
}

#[test]
fn an_ask_opened_before_attach_is_re_asked_of_a_late_connection() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let h = serve_paused(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: ToolCallId::new("call_1"),
                name: "ls".into(),
            }),
        ],
        vec![
            Ok(ProviderEvent::ToolCallEnd {
                id: ToolCallId::new("call_1"),
                input: serde_json::json!({ "path": cwd }),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![text_turn("done"), text_turn("titled")],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    until(|| h.paused.is_parked());
    h.release.send(()).unwrap();

    let mut asked = Vec::new();
    let (ask_1, params) = until_ask(&h.inbox, &mut asked);
    assert_eq!(params["toolCall"]["toolCallId"], "call_1");

    let c2 = connect(&h.host);
    c2.client.request("session/load", h.resume.clone()).unwrap();
    let mut updates = Vec::new();
    let (ask_2, params) = until_ask(&c2.inbox, &mut updates);
    assert_eq!(update_kinds(&updates), ["user_message_chunk"]);
    assert_eq!(params["sessionId"], h.session);
    assert_eq!(params["toolCall"]["toolCallId"], "call_1");
    assert_eq!(params["toolCall"]["status"], "pending");

    allow(&c2.client, &ask_2);
    wait_cancel(&h.inbox, &ask_1);
    let seen = updates_until(&c2.inbox, &h.session, "agent_message_chunk");
    assert_eq!(chunk_texts(&seen).last().unwrap(), "done");
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let request = lock(&h.paused.requests)[1].clone();
    let results: Vec<_> = request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResultBlock { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results, [ToolCallId::new("call_1")]);
}

#[test]
fn an_ask_whose_last_client_drops_is_denied_and_the_run_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let h = serve_paused(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: ToolCallId::new("call_1"),
                name: "ls".into(),
            }),
        ],
        vec![
            Ok(ProviderEvent::ToolCallEnd {
                id: ToolCallId::new("call_1"),
                input: serde_json::json!({ "path": cwd }),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![text_turn("done"), text_turn("titled")],
        dir.path(),
        dir.path(),
    );
    // The prompt gives up on its answer, so its clone of the client
    // goes and dropping the harness's own closes the connection.
    let (done, prompt_end) = mpsc::channel();
    let prompter = h.client.clone();
    let params = serde_json::json!({
        "sessionId": h.session,
        "prompt": [{"type": "text", "text": "go"}],
    });
    std::thread::spawn(move || {
        let _ =
            done.send(prompter.request_timeout("session/prompt", params, Duration::from_secs(2)));
    });
    until(|| h.paused.is_parked());
    h.release.send(()).unwrap();
    until_ask(&h.inbox, &mut Vec::new());

    let PausedHarness {
        client,
        inbox,
        paused,
        ..
    } = h;
    drop(client);
    drop(inbox);
    until(|| lock(&paused.requests).len() >= 2);
    let request = lock(&paused.requests)[1].clone();
    let denied = request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|c| match c {
            Content::ToolResultBlock {
                call_id, is_error, ..
            } if *call_id == ToolCallId::new("call_1") => Some(*is_error),
            _ => None,
        });
    assert_eq!(denied, Some(true), "the ask was answered with a deny");
    let response = prompt_end.recv_timeout(WAIT).unwrap();
    assert!(response.is_err(), "the dropped client got an answer");
}

#[test]
fn an_ask_whose_last_client_drops_answers_questions_with_no_answers() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn("call_q", "ask_user_question", questions_input()),
            text_turn("noted"),
            text_turn("titled"),
        ],
        dir.path(),
        dir.path(),
    );
    // The prompt gives up on its answer, so dropping the harness's
    // own client closes the connection.
    let (done, prompt_end) = mpsc::channel();
    let prompter = h.client.clone();
    let params = serde_json::json!({
        "sessionId": h.session,
        "prompt": [{"type": "text", "text": "set it up"}],
    });
    std::thread::spawn(move || {
        let _ =
            done.send(prompter.request_timeout("session/prompt", params, Duration::from_secs(2)));
    });
    until_ask(&h.inbox, &mut Vec::new());

    let Harness {
        client,
        inbox,
        mock,
        ..
    } = h;
    drop(client);
    drop(inbox);
    // The question ask is answered with no answers, so the tool
    // reports a decline and the run goes on instead of waiting.
    until(|| mock.requests().len() >= 2);
    let sent = mock.requests()[1].messages.clone();
    let declined = sent.iter().flat_map(|m| &m.content).any(|block| {
        matches!(block, Content::ToolResultBlock { output, .. }
            if output.starts_with("The user declined"))
    });
    assert!(declined, "{sent:#?}");
    let response = prompt_end.recv_timeout(WAIT).unwrap();
    assert!(response.is_err(), "the dropped client got an answer");
}

#[test]
fn closing_a_session_with_an_open_ask_denies_it_and_closes_once_idle() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let h = serve_paused(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: ToolCallId::new("call_1"),
                name: "ls".into(),
            }),
        ],
        vec![
            Ok(ProviderEvent::ToolCallEnd {
                id: ToolCallId::new("call_1"),
                input: serde_json::json!({ "path": cwd }),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![text_turn("done"), text_turn("titled")],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    until(|| h.paused.is_parked());
    h.release.send(()).unwrap();
    let (ask, _) = until_ask(&h.inbox, &mut Vec::new());

    h.client
        .request("session/close", serde_json::json!({"sessionId": h.session}))
        .unwrap();
    // The deny reaches the engine and the run finishes before the
    // session closes.
    until(|| lock(&h.paused.requests).len() >= 2);
    let request = lock(&h.paused.requests)[1].clone();
    let denied = request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|c| match c {
            Content::ToolResultBlock {
                call_id, is_error, ..
            } if *call_id == ToolCallId::new("call_1") => Some(*is_error),
            _ => None,
        });
    assert_eq!(denied, Some(true), "the ask was answered with a deny");
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    wait_cancel(&h.inbox, &ask);
    until(|| {
        !h.host
            .engine
            .hosted_sessions()
            .iter()
            .any(|(id, _)| *id == h.id)
    });
}

#[test]
fn closing_a_session_with_an_open_question_answers_with_no_answers() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn("call_q", "ask_user_question", questions_input()),
            text_turn("noted"),
            text_turn("titled"),
        ],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "set it up");
    let (ask, _) = until_ask(&h.inbox, &mut Vec::new());

    h.client
        .request("session/close", serde_json::json!({"sessionId": h.session}))
        .unwrap();
    wait_cancel(&h.inbox, &ask);
    until(|| h.mock.requests().len() >= 2);
    let sent = h.mock.requests()[1].messages.clone();
    let declined = sent.iter().flat_map(|m| &m.content).any(|block| {
        matches!(block, Content::ToolResultBlock { output, .. }
            if output.starts_with("The user declined"))
    });
    assert!(declined, "{sent:#?}");
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    until(|| {
        !h.host
            .engine
            .hosted_sessions()
            .iter()
            .any(|(id, _)| *id == h.id)
    });
}

#[test]
fn a_late_subagent_client_hears_of_a_running_agent_and_its_ask() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let task = serde_json::json!({"description": "list files", "prompt": "list"});
    let h = serve_paused(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::ToolCallStart {
                id: ToolCallId::new("call_agent"),
                name: "agent".into(),
            }),
        ],
        vec![
            Ok(ProviderEvent::ToolCallEnd {
                id: ToolCallId::new("call_agent"),
                input: task,
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![
            tool_turn("call_child", "ls", serde_json::json!({ "path": path })),
            text_turn("child done"),
            text_turn("parent done"),
            text_turn("titled"),
        ],
        dir.path(),
        dir.path(),
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    until(|| h.paused.is_parked());
    h.release.send(()).unwrap();
    let (root_ask, params) = until_ask(&h.inbox, &mut Vec::new());
    let child = params["sessionId"].clone();
    assert_ne!(child, h.session);
    assert_eq!(params["toolCall"]["toolCallId"], "call_child");

    let c2 = connect(&h.host);
    initialize(&c2.client);
    c2.client.request("session/load", h.resume.clone()).unwrap();
    let updates = updates_until(&c2.inbox, &h.session, "subagent_update");
    let announced = updates.last().unwrap();
    assert_eq!(announced["update"]["subagentSessionId"], child);
    assert_eq!(announced["update"]["name"], "general");
    assert_eq!(announced["update"]["task"], "list files");
    assert_eq!(announced["update"]["capabilities"]["cancel"], true);
    assert_eq!(announced["update"]["state"], "running");
    let (re_asked, params) = until_ask(&c2.inbox, &mut Vec::new());
    assert_eq!(params["sessionId"], child);
    assert_eq!(params["toolCall"]["toolCallId"], "call_agent");
    assert_eq!(params["toolCall"]["title"], "general: ls");

    let refused = c2
        .client
        .request("session/close", serde_json::json!({"sessionId": child}))
        .unwrap_err();
    assert_eq!(refused.code, -32602);

    allow(&c2.client, &re_asked);
    wait_cancel(&h.inbox, &root_ask);
    let notes = until_terminal(&c2.inbox);
    let (_, terminal) = notes.last().unwrap();
    assert_eq!(terminal["update"]["subagentSessionId"], child);
    assert_eq!(terminal["update"]["state"], "completed");
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
}

#[test]
fn session_close_releases_only_the_callers_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_paused(
        vec![Ok(ProviderEvent::MessageStart)],
        vec![
            Ok(ProviderEvent::TextDelta {
                delta: "one".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ],
        vec![text_turn("one title")],
        dir.path(),
        dir.path(),
    );
    let c2 = connect(&h.host);
    c2.client.request("session/load", h.resume.clone()).unwrap();
    let hosted = || {
        h.host
            .engine
            .hosted_sessions()
            .iter()
            .any(|(id, _)| *id == h.id)
    };
    assert!(hosted());

    let closed = h
        .client
        .request("session/close", serde_json::json!({"sessionId": h.session}))
        .unwrap();
    assert_eq!(closed, serde_json::json!({}));
    assert!(hosted(), "the other connection still holds the session");
    let refused = h
        .client
        .request("session/close", serde_json::json!({"sessionId": h.session}))
        .unwrap_err();
    assert_eq!(refused.code, -32602);
    let stale = drain(&h.inbox);

    let prompt_end = prompt_async(&c2.client, &h.session, "hi");
    until(|| h.paused.is_parked());
    h.release.send(()).unwrap();
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    assert_eq!(chunk_texts(&drain(&c2.inbox)), ["one"]);
    assert!(
        h.inbox.recv_timeout(Duration::from_millis(300)).is_err(),
        "the closed connection still receives updates after {stale:?}"
    );

    c2.client
        .request("session/close", serde_json::json!({"sessionId": h.session}))
        .unwrap();
    until(|| {
        !h.host
            .engine
            .hosted_sessions()
            .iter()
            .any(|(id, _)| *id == h.id)
    });
}

#[test]
fn the_last_detach_of_an_idle_session_closes_it() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_paused(
        vec![Ok(ProviderEvent::MessageStart)],
        vec![
            Ok(ProviderEvent::TextDelta {
                delta: "sure".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ],
        vec![text_turn("titled")],
        dir.path(),
        dir.path(),
    );
    let hosted = || {
        h.host
            .engine
            .hosted_sessions()
            .iter()
            .any(|(id, _)| *id == h.id)
    };
    assert!(hosted());
    let PausedHarness {
        client,
        inbox,
        host,
        paused,
        release,
        id,
        session,
        resume,
    } = h;
    drop(client);
    drop(inbox);
    until(|| {
        !host
            .engine
            .hosted_sessions()
            .iter()
            .any(|(sid, _)| *sid == id)
    });

    let c2 = connect(&host);
    c2.client.request("session/resume", resume).unwrap();
    let prompt_end = prompt_async(&c2.client, &session, "next");
    until(|| paused.is_parked());
    release.send(()).unwrap();
    let seen = updates_until(&c2.inbox, &session, "agent_message_chunk");
    assert_eq!(chunk_texts(&seen), ["sure"]);
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
}

#[test]
fn an_unattached_working_session_closes_after_its_run_ends() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_paused(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "Hel".into(),
            }),
        ],
        vec![
            Ok(ProviderEvent::TextDelta { delta: "lo".into() }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ],
        vec![text_turn("titled"), text_turn("sure")],
        dir.path(),
        dir.path(),
    );
    let _prompt_end = prompt_async(&h.client, &h.session, "go");
    until(|| h.paused.is_parked());

    let PausedHarness {
        client,
        inbox,
        host,
        release,
        id,
        session,
        resume,
        ..
    } = h;
    drop(client);
    drop(inbox);
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        host.engine
            .hosted_sessions()
            .iter()
            .any(|(sid, _)| *sid == id),
        "the working session stays hosted"
    );

    release.send(()).unwrap();
    until(|| {
        !host
            .engine
            .hosted_sessions()
            .iter()
            .any(|(sid, _)| *sid == id)
    });

    let c2 = connect(&host);
    c2.client.request("session/resume", resume).unwrap();
    assert_eq!(
        prompt(&c2.client, &session, "next")["stopReason"],
        "end_turn"
    );
}

#[test]
fn an_agent_session_loads_to_read_and_refuses_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let task = serde_json::json!({"description": "look around", "prompt": "list"});
    let h = serve_agents(
        vec![
            tool_turn("call_agent", "agent", task),
            text_turn("child done"),
            text_turn("parent done"),
        ],
        dir.path(),
        dir.path(),
        default_agents(),
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    let updates = drain(&h.inbox);
    let child = updates
        .iter()
        .find(|p| p["update"]["sessionUpdate"] == "subagent_update")
        .and_then(|p| p["update"]["subagentSessionId"].as_str())
        .expect("child announced")
        .to_owned();

    let cwd = dir.path().display().to_string();
    let params = serde_json::json!({"sessionId": child, "cwd": cwd, "mcpServers": []});
    let loaded = h.client.request("session/load", params).unwrap();
    assert!(
        loaded["configOptions"].as_array().is_none_or(Vec::is_empty),
        "{loaded}"
    );
    let shown = updates_until(&h.inbox, &child, "agent_message_chunk");
    assert!(
        shown
            .iter()
            .any(|p| p["update"]["content"]["text"] == "child done"),
        "{shown:#?}"
    );
    let calls = h.mock.call_count();

    let params = serde_json::json!({
        "sessionId": child,
        "prompt": [{"type": "text", "text": "more"}],
    });
    let refused = h.client.request("session/prompt", params).unwrap_err();
    assert!(refused.to_string().contains("read-only"), "{refused}");
    assert_eq!(h.mock.call_count(), calls, "nothing ran");
    let hosted: Vec<String> = h
        .host
        .engine
        .hosted_sessions()
        .into_iter()
        .map(|(id, _)| id.to_string())
        .collect();
    assert!(!hosted.contains(&child), "{hosted:?}");
}

#[test]
fn loading_a_session_rebuilds_its_agents_at_every_depth() {
    let dir = tempfile::tempdir().unwrap();
    let task = |prompt: &str| serde_json::json!({"description": prompt, "prompt": prompt});
    let h = serve_agents(
        vec![
            tool_turn("call_outer", "agent", task("outer")),
            tool_turn("call_inner", "agent", task("inner")),
            text_turn("inner done"),
            text_turn("outer done"),
            text_turn("parent done"),
        ],
        dir.path(),
        dir.path(),
        AgentSetup {
            max_depth: 2,
            ..default_agents()
        },
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    drain(&h.inbox);
    h.client
        .request("session/close", serde_json::json!({"sessionId": h.session}))
        .unwrap();

    let cwd = dir.path().display().to_string();
    let params = serde_json::json!({"sessionId": h.session, "cwd": cwd, "mcpServers": []});
    h.client.request("session/load", params).unwrap();
    let updates: Vec<serde_json::Value> = drain(&h.inbox)
        .into_iter()
        .filter(|p| p["update"]["sessionUpdate"] == "subagent_update")
        .collect();
    let outer = updates
        .iter()
        .find(|p| p["update"]["toolCallId"] == "call_outer")
        .expect("the outer agent comes back");
    assert_eq!(outer["sessionId"], h.session);
    assert_eq!(outer["update"]["state"], "completed");
    assert_eq!(outer["update"]["name"], "general");
    assert_eq!(outer["update"]["task"], "outer");
    let outer_id = outer["update"]["subagentSessionId"].clone();
    let inner = updates
        .iter()
        .find(|p| p["update"]["toolCallId"] == "call_inner")
        .expect("the inner agent comes back from the outer one's file");
    assert_eq!(inner["sessionId"], outer_id, "announced on its parent");
    assert_eq!(inner["update"]["state"], "completed");
}

#[test]
fn loading_a_compacted_session_still_lists_its_agents() {
    let dir = tempfile::tempdir().unwrap();
    let task = serde_json::json!({"description": "look around", "prompt": "list"});
    let h = serve_agents(
        vec![
            tool_turn("call_agent", "agent", task),
            text_turn("child done"),
            text_turn("parent done"),
        ],
        dir.path(),
        dir.path(),
        default_agents(),
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    drain(&h.inbox);
    h.client
        .request("session/close", serde_json::json!({"sessionId": h.session}))
        .unwrap();

    // A compaction that summarizes the whole run, agent call included.
    let path = kage_session::find_by_prefix(dir.path(), &h.session)
        .unwrap()
        .unwrap();
    let summarized = kage_session::replay(&path).unwrap().history.len();
    // The close reply races the engine dropping the session's writer,
    // so wait the window out the way the hosted load path does.
    let mut writer = {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            match SessionWriter::open(&path) {
                Ok(writer) => break writer,
                Err(SessionError::Locked { .. }) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the closed session's file stays locked"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("{err}"),
            }
        }
    };
    writer
        .append(&SessionEntry::Compaction(kage_session::Compaction {
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            kept: 0,
            summarized,
            summary: "the agent looked around".into(),
            before: None,
        }))
        .unwrap();
    drop(writer);

    let cwd = dir.path().display().to_string();
    let params = serde_json::json!({"sessionId": h.session, "cwd": cwd, "mcpServers": []});
    h.client.request("session/load", params).unwrap();
    let updates = drain(&h.inbox);
    let agent = updates
        .iter()
        .find(|p| p["update"]["sessionUpdate"] == "subagent_update")
        .unwrap_or_else(|| panic!("the compacted agent comes back: {updates:#?}"));
    assert_eq!(agent["update"]["toolCallId"], "call_agent");
    assert_eq!(agent["update"]["state"], "completed");
}

/// A restart shows the compaction card with the token counts the file
/// recorded: the restore emits the `_kage/compaction` update carrying
/// the `before` the loop stored and the `after` replay read from the
/// next turn's usage, and the summarized turns stay visible around it.
#[test]
fn restoring_a_compacted_session_emits_the_card_with_its_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_agents(
        vec![text_turn_with_usage("early reply", 500)],
        dir.path(),
        dir.path(),
        default_agents(),
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    drain(&h.inbox);
    h.client
        .request("session/close", serde_json::json!({"sessionId": h.session}))
        .unwrap();

    let path = kage_session::find_by_prefix(dir.path(), &h.session)
        .unwrap()
        .unwrap();
    let summarized = kage_session::replay(&path).unwrap().history.len();
    let mut writer = {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            match SessionWriter::open(&path) {
                Ok(writer) => break writer,
                Err(SessionError::Locked { .. }) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the closed session's file stays locked"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("{err}"),
            }
        }
    };
    writer
        .append(&SessionEntry::Compaction(kage_session::Compaction {
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            kept: 0,
            summarized,
            summary: "the handoff".into(),
            before: Some(900),
        }))
        .unwrap();
    // The post-compaction turn whose fill pairs with `before`.
    writer
        .append(&SessionEntry::Message(kage_session::MessageEntry {
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            message: std::sync::Arc::new(kage_core::Message::new(
                kage_core::Role::Assistant,
                vec![kage_core::Content::Text {
                    text: "after the cut".into(),
                }],
                None,
            )),
            usage: Some(kage_core::TokenUsage {
                input: 42_000,
                output: 100,
                ..kage_core::TokenUsage::default()
            }),
        }))
        .unwrap();
    drop(writer);

    let cwd = dir.path().display().to_string();
    let params = serde_json::json!({"sessionId": h.session, "cwd": cwd, "mcpServers": []});
    h.client.request("session/load", params).unwrap();
    let updates = drain(&h.inbox);
    let card = updates
        .iter()
        .find(|p| p["update"]["sessionUpdate"] == "_kage/compaction")
        .unwrap_or_else(|| panic!("the compaction card is restored: {updates:#?}"));
    assert_eq!(card["update"]["phase"], "complete");
    assert_eq!(card["update"]["before"], 900);
    assert_eq!(
        card["update"]["after"], 42_100,
        "the first usage after the cut"
    );
    assert_eq!(card["update"]["summary"], "the handoff");
    let prompts = updates
        .iter()
        .filter(|p| p["update"]["sessionUpdate"] == "user_message_chunk")
        .count();
    assert!(
        prompts >= 1,
        "the pre-compaction turns stay visible on a restore: {updates:#?}"
    );
}

#[test]
fn a_client_attaching_mid_swarm_hears_each_members_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let h = serve_agents(
        vec![
            tool_turn("call_s", "swarm", swarm_input(&["a", "b"])),
            tool_turn("call_a", "ls", serde_json::json!({ "path": path })),
            text_turn("a done"),
            text_turn("b done"),
            text_turn("parent done"),
        ],
        dir.path(),
        dir.path(),
        AgentSetup {
            max_running: 1,
            ..default_agents()
        },
    );
    initialize(&h.client);
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    let (ask, _) = until_ask(&h.inbox, &mut Vec::new());

    let c2 = connect(&h.host);
    initialize(&c2.client);
    let cwd = dir.path().display().to_string();
    let params = serde_json::json!({"sessionId": h.session, "cwd": cwd, "mcpServers": []});
    c2.client.request("session/load", params).unwrap();
    let mut seeds = updates_until(&c2.inbox, &h.session, "subagent_update");
    seeds.extend(drain(&c2.inbox));
    let items: Vec<(serde_json::Value, serde_json::Value)> = seeds
        .iter()
        .filter(|p| p["update"]["sessionUpdate"] == "subagent_update")
        .map(|p| {
            (
                p["update"]["swarm"]["item"].clone(),
                p["update"]["swarm"]["index"].clone(),
            )
        })
        .collect();
    assert!(items.contains(&("a".into(), 0.into())), "{seeds:#?}");
    assert!(items.contains(&("b".into(), 1.into())), "{seeds:#?}");

    allow(&h.client, &ask);
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
}

#[test]
fn the_model_catalog_lists_each_provider_with_its_models() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());
    let catalog = h
        .client
        .request("_kage/models/list", serde_json::json!({}))
        .unwrap();
    let mock = catalog["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "mock")
        .expect("the mock provider");
    assert_eq!(mock["name"], "Mock");
    let ids: Vec<_> = mock["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mock/m", "mock/other"]);
    assert_eq!(mock["models"][0]["name"], "Mock m");
}

/// A provider under a catalog id that declares no models, so the
/// catalog drives what its picker lists.
#[derive(Debug)]
struct CatalogDriven {
    meta: kage_provider::ProviderMetadata,
}

impl CatalogDriven {
    fn of(id: &str, display_name: &str) -> Self {
        Self {
            meta: kage_provider::ProviderMetadata {
                id: id.into(),
                display_name: display_name.into(),
                supports_caching: false,
                supports_thinking: false,
                supports_tool_use: true,
            },
        }
    }
}

impl kage_provider::Provider for CatalogDriven {
    fn metadata(&self) -> &kage_provider::ProviderMetadata {
        &self.meta
    }

    fn stream(
        &self,
        _req: kage_provider::StreamRequest,
        _cancel: &kage_core::CancelFlag,
    ) -> Result<kage_provider::EventStream, ProviderError> {
        Err(ProviderError::Auth("not used".into()))
    }
}

#[test]
fn a_provider_outside_the_catalog_with_no_declared_models_is_left_out() {
    let registry = ProviderRegistry::new().with(Arc::new(MockProvider::replaying(vec![])));
    let catalog = models::catalog(&registry);
    assert!(
        !catalog.providers.iter().any(|p| p.id == "mock"),
        "mock declares no models and has no catalog entry"
    );
}

#[test]
fn a_catalog_provider_without_declared_models_lists_catalog_entries() {
    let registry =
        ProviderRegistry::new().with(Arc::new(CatalogDriven::of("anthropic", "Anthropic Direct")));
    let catalog = models::catalog(&registry);
    assert_eq!(catalog.providers.len(), 1);
    let provider = &catalog.providers[0];
    assert_eq!(provider.id, "anthropic");
    assert_eq!(provider.name, "Anthropic", "the catalog names the provider");
    let entry = provider
        .models
        .iter()
        .find(|m| m.id == "anthropic/claude-fable-5")
        .expect("the catalog's claude-fable-5");
    assert_eq!(entry.name, "Claude Fable 5");
    assert_eq!(entry.context, Some(1_000_000));
    assert_eq!(entry.input_cost, Some(10.0));
    assert!(
        entry.released.is_some(),
        "catalog entries carry release dates"
    );
}

/// Serves the main session from `main` and its agents from `agents`,
/// told apart by the `agent` tool only the main session has. A request
/// without tools, such as the title, gets a fixed answer.
#[derive(Debug)]
struct Routed {
    main: Listed,
    agents: Arc<dyn kage_provider::Provider>,
}

impl kage_provider::Provider for Routed {
    fn metadata(&self) -> &kage_provider::ProviderMetadata {
        self.main.metadata()
    }

    fn stream(
        &self,
        req: kage_provider::StreamRequest,
        cancel: &kage_core::CancelFlag,
    ) -> Result<kage_provider::EventStream, ProviderError> {
        if req.tools.is_empty() {
            Ok(Box::new(text_turn("title").into_iter()))
        } else if req.tools.iter().any(|tool| tool.name == "agent") {
            self.main.stream(req, cancel)
        } else {
            self.agents.stream(req, cancel)
        }
    }

    fn models(&self) -> Vec<kage_provider::ProviderModel> {
        self.main.models()
    }
}

/// Holds the agents provider's stream until the gate fires, so a test
/// decides when a background agent completes.
#[derive(Debug)]
struct Gated {
    inner: MockProvider,
    gate: Mutex<mpsc::Receiver<()>>,
}

impl kage_provider::Provider for Gated {
    fn metadata(&self) -> &kage_provider::ProviderMetadata {
        self.inner.metadata()
    }

    fn stream(
        &self,
        req: kage_provider::StreamRequest,
        cancel: &kage_core::CancelFlag,
    ) -> Result<kage_provider::EventStream, ProviderError> {
        let _ = self.gate.lock().unwrap().recv();
        self.inner.stream(req, cancel)
    }
}

#[test]
fn a_prompt_returns_while_its_background_agent_still_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let task =
        serde_json::json!({"description": "list files", "prompt": "list", "background": true});
    let main = MockProvider::sequence(vec![
        tool_turn("call_agent", "agent", task),
        text_turn("the agent is on it"),
        text_turn("the agent listed the files"),
    ]);
    let agents = MockProvider::sequence(vec![
        tool_turn("call_child", "ls", serde_json::json!({ "path": path })),
        text_turn("child done"),
    ]);
    let routed = Routed {
        main: Listed::of(main.clone()),
        agents: Arc::new(agents),
    };
    let h = serve_on(
        Arc::new(routed),
        main.clone(),
        dir.path(),
        dir.path(),
        false,
        default_agents(),
        false,
    );
    initialize(&h.client);
    let first = prompt_async(&h.client, &h.session, "go");
    let response = first.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");

    let mut updates = Vec::new();
    let (ask, _) = until_ask(&h.inbox, &mut updates);
    assert!(!updates.iter().any(is_terminal), "{updates:#?}");
    allow(&h.client, &ask);
    let notes = until_terminal(&h.inbox);
    let (_, terminal) = notes.last().unwrap();
    assert_eq!(terminal["sessionId"], h.session);
    assert_eq!(terminal["update"]["state"], "completed");

    let second = prompt_async(&h.client, &h.session, "anything new?");
    let response = second.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let read = main.requests()[2].messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, kage_core::Content::Text { text }
                if text.starts_with("<agent name=\"general\"") && text.contains("child done"))
        })
    });
    assert!(read, "{:#?}", main.requests()[2].messages);
}

#[test]
fn a_background_agent_and_its_ask_survive_the_parent_run_ending() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let task =
        serde_json::json!({"description": "list files", "prompt": "list", "background": true});
    let main = MockProvider::sequence(vec![
        tool_turn("call_agent", "agent", task),
        text_turn("the agent is on it"),
    ]);
    let agents = MockProvider::sequence(vec![
        tool_turn("call_child", "ls", serde_json::json!({ "path": path })),
        text_turn("child done"),
        text_turn("titled twice"),
    ]);
    let routed = Routed {
        main: Listed::of(main.clone()),
        agents: Arc::new(agents),
    };
    let h = serve_on(
        Arc::new(routed),
        main.clone(),
        dir.path(),
        dir.path(),
        false,
        default_agents(),
        true,
    );
    initialize(&h.client);
    let resume = serde_json::json!({"sessionId": h.session, "cwd": path, "mcpServers": []});
    let response = prompt_async(&h.client, &h.session, "go")
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(response["stopReason"], "end_turn");

    // The child asks while its parent's run has already ended.
    let (ask, params) = until_ask(&h.inbox, &mut Vec::new());
    let child = params["sessionId"].clone();
    assert_ne!(child, h.session);
    assert_eq!(params["toolCall"]["toolCallId"], "call_child");

    // A late attach still hears of the running agent. The replay
    // re-asks the parent's own agent call; the child's open ask stays
    // with the original client, so it is answered there.
    let c2 = connect(&h.host);
    initialize(&c2.client);
    c2.client.request("session/load", resume.clone()).unwrap();
    let updates = updates_until(&c2.inbox, &h.session, "subagent_update");
    let announced = updates.last().unwrap();
    assert_eq!(announced["update"]["subagentSessionId"], child);
    assert_eq!(announced["update"]["state"], "running");

    allow(&h.client, &ask);
    let notes = until_terminal(&c2.inbox);
    let (_, terminal) = notes.last().unwrap();
    assert_eq!(terminal["update"]["subagentSessionId"], child);
    assert_eq!(terminal["update"]["state"], "completed");
}

#[test]
fn a_session_with_a_live_background_agent_stays_open_after_close() {
    let dir = tempfile::tempdir().unwrap();
    let task = serde_json::json!({"description": "ponder", "prompt": "ponder", "background": true});
    let main = MockProvider::sequence(vec![
        tool_turn("call_agent", "agent", task),
        text_turn("the agent is on it"),
    ]);
    let (release, gate) = mpsc::channel();
    let agents = Paused::new(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "wor".into(),
            }),
        ],
        vec![
            Ok(ProviderEvent::TextDelta {
                delta: "king".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ],
        Vec::new(),
        gate,
    );
    let routed = Routed {
        main: Listed::of(main.clone()),
        agents: Arc::new(agents),
    };
    let h = serve_on(
        Arc::new(routed),
        main.clone(),
        dir.path(),
        dir.path(),
        false,
        default_agents(),
        false,
    );
    initialize(&h.client);
    let response = prompt_async(&h.client, &h.session, "go")
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let hosted = || {
        h.host
            .engine
            .hosted_sessions()
            .iter()
            .any(|(id, _)| *id == h.id)
    };
    assert!(hosted());

    // The last client closes the session while the background agent
    // still works: no Close goes out, so the run is not cancelled.
    h.client
        .request("session/close", serde_json::json!({"sessionId": h.session}))
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(hosted(), "the live background agent keeps the session open");

    // Once the agent ends, the close goes out after all.
    release.send(()).unwrap();
    until(|| !hosted());
}

#[test]
fn a_kage_client_session_wakes_for_a_background_result() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    let task =
        serde_json::json!({"description": "list files", "prompt": "list", "background": true});
    let main = MockProvider::sequence(vec![
        tool_turn("call_agent", "agent", task),
        text_turn("the agent is on it"),
        text_turn("the agent listed the files"),
    ]);
    let agents = MockProvider::sequence(vec![text_turn("child done")]);
    let (release, gate) = mpsc::channel();
    let routed = Routed {
        main: Listed::of(main.clone()),
        agents: Arc::new(Gated {
            inner: agents,
            gate: Mutex::new(gate),
        }),
    };
    let host = test_host_agents(
        Arc::new(routed),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        false,
        default_agents(),
        PermissionAction::Allow,
    );
    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    std::thread::spawn(move || {
        host.serve_with(BufReader::new(srv_r), srv_w, |_| {})
            .unwrap();
    });
    let (client, inbox, _reader) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
    let params = serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {"subagents": {}, "_meta": {"kage": {}}},
    });
    client.request("initialize", params).unwrap();
    let params = serde_json::json!({ "cwd": path, "mcpServers": [] });
    let created = client.request("session/new", params).unwrap();
    let session = created["sessionId"].as_str().unwrap().to_owned();
    let response = prompt_async(&client, &session, "go")
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    // Run 1 has ended; only now let the agent complete, so its report
    // wakes the idle session instead of riding along mid-run.
    release.send(()).unwrap();
    let mut report = None;
    loop {
        let Inbound::Notification { params, .. } = inbox.recv_timeout(WAIT).expect("no wake run")
        else {
            continue;
        };
        if params["sessionId"] != session.as_str() {
            continue;
        }
        let update = &params["update"];
        if update["sessionUpdate"] == "user_message_chunk" {
            report = update["content"]["text"].as_str().map(str::to_owned);
        }
        if update["content"]["text"] == "the agent listed the files" {
            break;
        }
    }
    let report = report.expect("the report reaches the client that owns the session");
    assert!(
        report.starts_with("<agent name=\"general\"") && report.contains("child done"),
        "{report}"
    );
    assert_eq!(main.call_count(), 3);
}

fn questions_input() -> serde_json::Value {
    serde_json::json!({"questions": [
        {"header": "Store", "question": "Where should sessions live?",
         "options": [{"label": "Disk", "description": "Survives restarts"}, {"label": "Memory"}]},
        {"header": "Format", "question": "Which formats?", "multi_select": true,
         "options": [{"label": "JSON"}, {"label": "TOML"}, {"label": "YAML"}]}
    ]})
}

#[test]
fn questions_reach_an_editor_as_permission_asks_one_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn("call_q", "ask_user_question", questions_input()),
            text_turn("noted"),
            text_turn("title"),
        ],
        dir.path(),
        dir.path(),
    );
    let end = prompt_async(&h.client, &h.session, "set it up");

    let (ask, params) = until_ask(&h.inbox, &mut Vec::new());
    let names: Vec<&str> = params["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Disk", "Memory", "Skip"]);
    assert_eq!(
        params["toolCall"]["title"],
        "Store: Where should sessions live?"
    );
    assert_eq!(
        params["_meta"]["kage"]["question"]["prompt"]["header"],
        "Store"
    );
    let pick = serde_json::json!({"outcome": {"outcome": "selected", "optionId": "choice-1"}});
    h.client.respond(&ask, Ok(pick)).unwrap();

    let (ask, params) = until_ask(&h.inbox, &mut Vec::new());
    assert_eq!(
        params["_meta"]["kage"]["question"]["prompt"]["multiSelect"],
        true
    );
    let several = serde_json::json!({
        "outcome": {"outcome": "selected", "optionId": "choice-0"},
        "_meta": {"kage": {"question": {"answer": ["JSON", "YAML"]}}}
    });
    h.client.respond(&ask, Ok(several)).unwrap();

    let response = end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let sent = h.mock.requests()[1].messages.clone();
    let answered = sent.iter().flat_map(|m| &m.content).any(|block| {
        matches!(block, kage_core::Content::ToolResultBlock { output, .. }
            if output == "The user answered:\n- Where should sessions live?: Memory\n\
                          - Which formats?: JSON, YAML")
    });
    assert!(answered, "{sent:#?}");
}

#[test]
fn skipping_a_question_declines_them_all() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn("call_q", "ask_user_question", questions_input()),
            text_turn("ok"),
            text_turn("title"),
        ],
        dir.path(),
        dir.path(),
    );
    let end = prompt_async(&h.client, &h.session, "set it up");
    let (ask, _) = until_ask(&h.inbox, &mut Vec::new());
    let skip = serde_json::json!({"outcome": {"outcome": "selected", "optionId": "skip"}});
    h.client.respond(&ask, Ok(skip)).unwrap();
    end.recv_timeout(WAIT).unwrap().unwrap();
    let sent = h.mock.requests()[1].messages.clone();
    let declined = sent.iter().flat_map(|m| &m.content).any(|block| {
        matches!(block, kage_core::Content::ToolResultBlock { output, .. }
            if output.starts_with("The user declined"))
    });
    assert!(declined, "{sent:#?}");
}

#[test]
fn an_interrupted_swarm_gets_its_members_back_from_their_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = SessionId::new();
    let member = |index: u64| {
        SessionEntry::Custom(kage_session::Custom {
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            kind: kage_session::list::AGENT_ENTRY_KIND.into(),
            data: serde_json::json!({
                "parent": root.to_string(),
                "tool_call_id": "call_sw",
                "agent": "general",
                "description": "audit",
                "batch_id": "swarm_1",
                "index": index,
                "total": 2,
                "item": format!("item {index}"),
            }),
        })
    };
    let reply = message(Role::Assistant, vec![text("done")], None);
    let busy = message(
        Role::Assistant,
        vec![Content::ToolCall {
            id: kage_core::ToolCallId::new("c1"),
            name: "shell".into(),
            input: serde_json::json!({}),
        }],
        None,
    );
    record(dir.path(), "/p", "mock/m", 0, &[member(0), reply]);
    record(dir.path(), "/p", "mock/m", 0, &[member(1), busy]);
    let history = vec![Message::new(
        Role::Assistant,
        vec![Content::ToolCall {
            id: kage_core::ToolCallId::new("call_sw"),
            name: "swarm".into(),
            input: serde_json::json!({}),
        }],
        None,
    )];
    let members = super::sessions::orphaned_agents(dir.path(), root, &history);
    let states: Vec<_> = members
        .iter()
        .map(|m| (m.swarm.as_ref().unwrap().index, m.state))
        .collect();
    assert_eq!(
        states,
        [
            (0, Some(kage_acp::acp::SubagentState::Completed)),
            (1, Some(kage_acp::acp::SubagentState::Failed)),
        ]
    );
    assert!(
        members
            .iter()
            .all(|m| m.tool_call_id.as_deref() == Some("call_sw"))
    );
}

/// A caller with a long or private prompt has to be able to keep it off the
/// command line, where `/proc/<pid>/cmdline` publishes it to every user on
/// the host. The file is appended rather than replacing the inline text, so
/// a fixed role and a session's own instructions can arrive separately.
#[test]
fn the_system_prompt_can_come_from_a_file() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("prompt.md");
    std::fs::write(&path, "house rules\n").expect("written");

    assert_eq!(resolve_system("", Some(&path)).unwrap(), "house rules");
    assert_eq!(
        resolve_system("a role", Some(&path)).unwrap(),
        "a role\n\nhouse rules"
    );
    assert_eq!(resolve_system("a role", None).unwrap(), "a role");
}

/// A session that starts without its instructions is worse than one that
/// does not start, so an unreadable file is an error the caller sees.
#[test]
fn an_unreadable_system_prompt_file_stops_the_run() {
    let error = resolve_system("a role", Some(Path::new("/nonexistent/prompt.md")))
        .expect_err("a missing file must be refused");

    assert!(error.contains("/nonexistent/prompt.md"), "{error}");
}

#[test]
fn the_host_is_busy_only_while_a_turn_runs() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_paused(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "Hel".into(),
            }),
        ],
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: CoreStopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
        vec![text_turn("titled")],
        dir.path(),
        dir.path(),
    );
    assert!(!h.host.busy());
    let client = h.client.clone();
    let session = h.session.clone();
    let prompt = std::thread::spawn(move || {
        client.request(
            "session/prompt",
            serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": "hi"}]}),
        )
    });
    let until = Instant::now() + WAIT;
    while !h.host.busy() {
        assert!(
            Instant::now() < until,
            "a running turn did not make the host busy"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    h.release.send(()).unwrap();
    let reply = prompt.join().unwrap().unwrap();
    assert_eq!(reply["stopReason"], "end_turn");
    let until = Instant::now() + WAIT;
    while h.host.busy() {
        assert!(
            Instant::now() < until,
            "the host stayed busy after the turn ended"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Only a client on the local socket gives the sessions it opens its
/// environment; a remote connection's `_kage/environment` is ignored.
#[test]
fn only_a_local_client_sets_the_environment_of_its_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let registry =
        Arc::new(ProviderRegistry::new().with(Arc::new(MockProvider::sequence(Vec::new()))));
    let spec: super::host::SpecBuilder = Box::new(move |_, _, _, _, _, env: &Environment, _, _| {
        let from_client = env
            .vars()
            .iter()
            .any(|(name, value)| name == "KAGE_TEST_CLIENT" && value == "editor");
        lock(&record).push(from_client);
        Err(RpcError::internal("recorded"))
    });
    let host = Host::new(
        registry,
        "mock/m".into(),
        dir.path().to_path_buf(),
        spec,
        BTreeMap::new(),
    );
    for local in [false, true] {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let serving = Arc::clone(&host);
        std::thread::spawn(move || {
            let reader = BufReader::new(srv_r);
            if local {
                serving.serve_local(reader, srv_w)
            } else {
                serving.serve(reader, srv_w)
            }
        });
        let (client, _inbox, _reader) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
        client
            .notify(
                "_kage/environment",
                serde_json::json!({"vars": {"KAGE_TEST_CLIENT": "editor"}}),
            )
            .unwrap();
        let params = serde_json::json!({"cwd": dir.path(), "mcpServers": []});
        assert!(client.request("session/new", params).is_err());
    }
    assert_eq!(*lock(&seen), [false, true]);
}

/// Two recorded turns, `first` then `second`, for undo and rewind.
fn two_turns(sessions: &Path) -> String {
    record(
        sessions,
        &sessions.display().to_string(),
        "mock/m",
        1,
        &[
            message(Role::User, vec![text("first")], None),
            message(Role::Assistant, vec![text("one")], None),
            message(Role::User, vec![text("second")], None),
            message(Role::Assistant, vec![text("two")], None),
        ],
    )
}

/// The user prompts in `updates`, in order.
fn user_texts(updates: &[serde_json::Value]) -> Vec<String> {
    updates
        .iter()
        .filter(|u| u["update"]["sessionUpdate"] == "user_message_chunk")
        .map(|u| u["update"]["content"]["text"].as_str().unwrap().to_owned())
        .collect()
}

/// Drains `inbox` until no update arrives for a moment.
fn drain_quiet(inbox: &mpsc::Receiver<Inbound>, session: &str) -> Vec<serde_json::Value> {
    let mut updates = Vec::new();
    while let Ok(Inbound::Notification { params, .. }) =
        inbox.recv_timeout(Duration::from_millis(300))
    {
        if params["sessionId"] == session {
            updates.push(params);
        }
    }
    updates
}

#[test]
fn undo_returns_the_prompt_and_replaces_the_session_for_every_client() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let source = two_turns(sessions);
    let h = serve(Vec::new(), sessions, sessions);
    let c2 = h.connect();
    as_kage_client(&c2);
    attach_both(&h, &c2, &source, &sessions.display().to_string());
    drain_quiet(&c2.inbox, &source);

    let undone = h
        .client
        .request(
            "_kage/session/undo",
            serde_json::json!({"sessionId": source}),
        )
        .unwrap();
    assert_eq!(undone["prompt"][0]["text"], "second");
    let seen = drain_quiet(&c2.inbox, &source);
    let replaced = seen
        .iter()
        .position(|u| u["update"]["sessionUpdate"] == "_kage/session_replaced")
        .expect("the other client hears of the replacement");
    assert_eq!(user_texts(&seen[replaced..]), ["first"]);
    let theirs = drain_quiet(&h.inbox, &source);
    assert!(
        !theirs
            .iter()
            .any(|u| u["update"]["sessionUpdate"] == "_kage/session_replaced"),
        "a client that is not kage keeps its transcript"
    );
}

/// Initializes `c` as a kage client, which understands kage-only updates.
fn as_kage_client(c: &Connection) {
    let params = serde_json::json!({
        "protocolVersion": 1,
        "clientCapabilities": {"_meta": {"kage": {}}},
    });
    c.client.request("initialize", params).unwrap();
}

#[test]
fn prompts_list_the_branch_tree_and_rewind_continues_from_one() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let source = two_turns(sessions);
    let h = serve(Vec::new(), sessions, sessions);
    let c2 = h.connect();
    as_kage_client(&c2);
    attach_both(&h, &c2, &source, &sessions.display().to_string());
    drain_quiet(&c2.inbox, &source);

    let listed = h
        .client
        .request(
            "_kage/session/prompts",
            serde_json::json!({"sessionId": source}),
        )
        .unwrap();
    let prompts = listed["prompts"].as_array().unwrap();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0]["prompt"][0]["text"], "first");
    assert_eq!(prompts[1]["active"], true);

    let at = prompts[0]["at"].as_str().unwrap();
    let rewind = |at: &str| {
        h.client.request(
            "_kage/session/rewind",
            serde_json::json!({"sessionId": source, "at": at}),
        )
    };
    assert_eq!(rewind(at).unwrap(), serde_json::json!({}));
    let seen = drain_quiet(&c2.inbox, &source);
    assert!(
        seen.iter()
            .any(|u| u["update"]["sessionUpdate"] == "_kage/session_replaced")
    );
    assert_eq!(user_texts(&seen), ["first"]);
    assert!(rewind("zzzz").is_err(), "an unknown prompt is refused");
}

#[test]
fn delete_refuses_an_open_session_and_removes_a_closed_one() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let closed = two_turns(sessions);
    let h = serve(Vec::new(), sessions, sessions);
    let delete = |id: &str| {
        h.client
            .request("_kage/session/delete", serde_json::json!({"sessionId": id}))
    };
    assert!(delete(&h.session).is_err(), "the open session stays");
    assert_eq!(delete(&closed).unwrap(), serde_json::json!({}));
    assert!(
        kage_session::find_by_prefix(sessions, &closed)
            .unwrap()
            .is_none()
    );
}

/// A session this host recorded, closed and actually taken down, is
/// deletable without restarting the host: the close that completed
/// clears the record that kept the file in use.
#[test]
fn delete_works_after_a_hosted_session_actually_closed() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_agents(
        vec![text_turn("done")],
        dir.path(),
        dir.path(),
        default_agents(),
    );
    let delete = || {
        h.client.request(
            "_kage/session/delete",
            serde_json::json!({"sessionId": h.session}),
        )
    };
    let prompt = prompt_async(&h.client, &h.session, "go");
    prompt.recv_timeout(WAIT).unwrap().unwrap();
    assert!(
        kage_session::find_by_prefix(dir.path(), &h.session)
            .unwrap()
            .is_some(),
        "the standing session is recorded"
    );
    h.client
        .request("session/close", serde_json::json!({"sessionId": h.session}))
        .unwrap();
    until(|| h.host.workdir(h.id).is_none());
    assert_eq!(delete().unwrap(), serde_json::json!({}));
    assert!(
        kage_session::find_by_prefix(dir.path(), &h.session)
            .unwrap()
            .is_none()
    );
}

/// A close report naming a launch the host already replaced is
/// skipped whole: a reopen keeps its records, and only a report
/// naming the current launch clears them.
#[test]
fn a_stale_close_report_spares_a_relaunched_session() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(Vec::new());
    let provider = Listed {
        mock: mock.clone(),
        input: Modalities::default(),
    };
    let host = test_host(
        Arc::new(provider),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        false,
    );
    let id = SessionId::new();
    let launch = |host: &Host| {
        let (spec, runtime) = (host.spec)(
            &host.registry(),
            id,
            "",
            "mock/m",
            BTreeMap::new(),
            &Environment::inherit(),
            None,
            None,
        )
        .unwrap();
        let settings = super::options::Settings::of(&spec, &host.registry());
        host.launch(spec, settings, runtime);
    };
    launch(&host);
    let first = lock(&host.instances).get(&id).copied().unwrap();
    host.engine.report_closed_for_test(id, Some(first));
    until(|| host.workdir(id).is_none());
    launch(&host);
    until(|| host.workdir(id).is_some());
    let second = lock(&host.instances).get(&id).copied().unwrap();
    assert!(second > first, "a reopen gets a fresh launch token");
    host.engine.report_closed_for_test(id, Some(first));
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(5));
        assert!(host.workdir(id).is_some(), "the reopen keeps its workdir");
        assert!(
            host.open_settings(id).is_some(),
            "the reopen stays attachable"
        );
        assert!(
            !lock(&host.live).is_closing(id),
            "the reopen is not closing"
        );
        assert_eq!(
            lock(&host.instances).get(&id),
            Some(&second),
            "the reopen keeps its launch token"
        );
    }
    host.engine.report_closed_for_test(id, Some(second));
    until(|| host.workdir(id).is_none() && host.open_settings(id).is_none());
    assert!(!lock(&host.live).is_closing(id));
    assert!(
        lock(&host.instances).get(&id).is_none(),
        "the consumed launch token is dropped"
    );
}

/// A launch waits for a close report being consumed: the instances
/// lock is held across both, so the launch installs its records only
/// after the consumption of the launch it replaced finished, and the
/// waiting launch still supersedes that consumption.
#[test]
fn a_launch_installs_only_after_a_report_is_consumed() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(Vec::new());
    let provider = Listed {
        mock: mock.clone(),
        input: Modalities::default(),
    };
    let host = test_host(
        Arc::new(provider),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        false,
    );
    let id = SessionId::new();
    let launch = move |host: &Host| {
        let (spec, runtime) = (host.spec)(
            &host.registry(),
            id,
            "",
            "mock/m",
            BTreeMap::new(),
            &Environment::inherit(),
            None,
            None,
        )
        .unwrap();
        let settings = super::options::Settings::of(&spec, &host.registry());
        host.launch(spec, settings, runtime);
    };
    launch(&host);
    let first = lock(&host.instances).get(&id).copied().unwrap();
    host.engine.report_closed_for_test(id, Some(first));
    until(|| host.workdir(id).is_none());
    let hold = lock(&host.instances);
    let installing = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || launch(&host))
    };
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(5));
        assert!(
            host.open_settings(id).is_none(),
            "the launch installed while a consumption holds the lock"
        );
    }
    drop(hold);
    until(|| host.open_settings(id).is_some() && host.workdir(id).is_some());
    installing.join().unwrap();
    let second = lock(&host.instances).get(&id).copied().unwrap();
    assert!(second > first, "a launch gets a fresh token");
    host.engine.report_closed_for_test(id, Some(first));
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(5));
        assert!(
            host.workdir(id).is_some(),
            "the stale report removed the new launch's workdir"
        );
    }
    host.engine.report_closed_for_test(id, Some(second));
    until(|| host.workdir(id).is_none());
}

/// A load that finds content but fails anyway, for the attach-id test.
fn failing_load(_: &Path, _: &str, _: &str, _: &Environment) -> Option<super::projects::Project> {
    None
}

/// An attach whose id is not newer than the newest one this
/// connection processed is refused, and a failed attach still counts,
/// so the bar never moves backwards.
#[test]
fn an_attach_id_that_is_not_newer_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());
    // A load that finds content but fails anyway, so the first attach
    // errors after it counted its id.
    h.host
        .set_projects_for_test(super::projects::Projects::with_hooks(failing_load, |_| {
            true
        }));
    let attach = |id: u64| {
        h.client.request(
            "_kage/ui/attach",
            serde_json::json!({
                "cwd": dir.path().display().to_string(),
                "attachId": id,
            }),
        )
    };
    // A failed attach counts its id but must not lower the bar.
    let spent = attach(2);
    assert_eq!(spent.unwrap_err().code, -32603);
    let older = attach(1);
    assert_eq!(older.unwrap_err().code, -32602);
    let repeat = attach(2);
    assert_eq!(repeat.unwrap_err().code, -32602);
}

/// A session the host launched with plugins runs the commands and
/// mappings its own runtime registered; the UI a client attaches to
/// that session pins to it.
#[test]
fn a_session_bound_attach_pins_its_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let mut caps = std::collections::BTreeMap::new();
    caps.insert("p".to_owned(), Vec::<String>::new());
    let runtime = Arc::new(
        kage_plugin::PluginRuntime::builder()
            .capabilities(caps)
            .build()
            .unwrap(),
    );
    runtime
        .eval_plugin(
            "p",
            "kage.register_command({ name = 'first', description = 'd', \
               handler = function() return 'one' end }) \
             kage.keymap.set('n', '<F1>', function() return 'k1' end)",
        )
        .unwrap();
    let mock = MockProvider::sequence(vec![text_turn("ready")]);
    let provider = Listed {
        mock: mock.clone(),
        input: Modalities::default(),
    };
    let host = test_host_with_plugins(
        Arc::new(provider),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        false,
        default_agents(),
        PermissionAction::Allow,
        Some(Arc::clone(&runtime)),
    );
    let h = serve_on_host(mock, host, SessionId::new(), dir.path(), false);
    let cwd = dir.path().display().to_string();
    let attach = h
        .client
        .request(
            "_kage/ui/attach",
            serde_json::json!({"cwd": cwd, "attachId": 1, "sessionId": h.session}),
        )
        .unwrap_or_else(|e| panic!("attach failed: {e}"));
    let ui = &attach;
    assert!(
        ui["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "first"),
        "{ui}"
    );
    assert!(
        ui["keymap"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["lhs"] == "<F1>"),
        "{ui}"
    );
    let run = h
        .client
        .request(
            "_kage/ui/command",
            serde_json::json!({"cwd": cwd, "name": "first", "sessionId": h.session}),
        )
        .unwrap();
    assert_eq!(run["text"], "one", "{run}");
    let key = h
        .client
        .request(
            "_kage/ui/keymap",
            serde_json::json!({"cwd": cwd, "sessionId": h.session, "id":
                ui["keymap"].as_array().unwrap().iter().find(|m| m["lhs"] == "<F1>").unwrap()["rhs"]["id"].clone()}),
        )
        .unwrap();
    assert_eq!(key["text"], "k1", "{key}");
    let missing = h
        .client
        .request(
            "_kage/ui/command",
            serde_json::json!({"cwd": cwd, "name": "nope", "sessionId": h.session}),
        )
        .unwrap_err();
    assert_eq!(missing.code, -32602);
}

/// The cwd-based attach follows the same plugin-content gate as a
/// session open: a project with nothing to load has no runtime to
/// attach to, and one with content does.
#[test]
fn the_cwd_attach_follows_the_plugin_content_gate() {
    #[expect(clippy::unnecessary_wraps, reason = "the loader's signature")]
    fn load(_: &Path, _: &str, _: &str, _: &Environment) -> Option<super::projects::Project> {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("p".to_owned(), Vec::<String>::new());
        let runtime = Arc::new(
            kage_plugin::PluginRuntime::builder()
                .capabilities(caps)
                .build()
                .unwrap(),
        );
        runtime
            .eval_plugin(
                "p",
                "kage.register_command({ name = 'first', description = 'd', \
                   handler = function() return 'one' end })",
            )
            .unwrap();
        Some(super::projects::Project {
            runtime,
            options: kage_plugin::SharedOptions::default(),
        })
    }
    let attach = |has_content: fn(&Path) -> bool| {
        let dir = tempfile::tempdir().unwrap();
        let h = serve(Vec::new(), dir.path(), dir.path());
        h.host
            .set_projects_for_test(super::projects::Projects::with_hooks(load, has_content));
        h.client.request(
            "_kage/ui/attach",
            serde_json::json!({
                "cwd": dir.path().display().to_string(),
                "attachId": 1,
            }),
        )
    };
    // Nothing to load answers the empty snapshot it would have served.
    let empty = attach(|_| false).unwrap();
    assert_eq!(empty["commands"].as_array().map(Vec::len), Some(0));
    let ui = attach(|_| true).unwrap();
    assert!(
        ui["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "first"),
        "{ui}"
    );
}

/// A session without plugins refuses a session-bound attach, and so
/// does an unknown session id.
#[test]
fn an_attach_for_a_session_without_plugins_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());
    let attach = |session: &str, id: u64| {
        h.client.request(
            "_kage/ui/attach",
            serde_json::json!({
                "cwd": dir.path().display().to_string(),
                "attachId": id,
                "sessionId": session,
            }),
        )
    };
    let refused = attach(&h.session, 1);
    assert_eq!(refused.unwrap_err().code, -32602);
    let unknown = attach("zz", 2);
    assert_eq!(unknown.unwrap_err().code, -32602);
}

#[test]
fn fork_at_an_entry_copies_through_it() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let source = two_turns(sessions);
    let h = serve(Vec::new(), sessions, sessions);
    let path = kage_session::find_by_prefix(sessions, &source)
        .unwrap()
        .unwrap();
    let ids: Vec<String> = kage_session::SessionReader::iter(&path)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| matches!(entry, SessionEntry::Message(_)))
        .map(|entry| entry.id().to_string())
        .collect();
    let forked = h
        .client
        .request(
            "_kage/session/fork",
            serde_json::json!({"sessionId": source, "at": ids[1]}),
        )
        .unwrap();
    let copy = kage_session::find_by_prefix(sessions, forked["sessionId"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(kage_session::replay(&copy).unwrap().history.len(), 2);
}

#[test]
fn undo_is_refused_at_once_while_a_turn_runs_or_with_nothing_to_undo() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_paused(
        vec![Ok(ProviderEvent::MessageStart)],
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: CoreStopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
        vec![text_turn("titled")],
        dir.path(),
        dir.path(),
    );
    let undo = || {
        h.client.request(
            "_kage/session/undo",
            serde_json::json!({"sessionId": h.session}),
        )
    };
    let started = Instant::now();
    assert!(
        undo().is_err(),
        "an unrecorded idle session has nothing to undo"
    );
    let client = h.client.clone();
    let session = h.session.clone();
    let prompt = std::thread::spawn(move || {
        client.request(
            "session/prompt",
            serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": "hi"}]}),
        )
    });
    until(|| h.host.busy());
    let refused = undo().unwrap_err();
    assert!(
        refused.message.contains("wait for the current run"),
        "{}",
        refused.message
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "refusals answer at once"
    );
    h.release.send(()).unwrap();
    prompt.join().unwrap().unwrap();
}

#[test]
fn delete_needs_the_whole_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let closed = two_turns(sessions);
    let h = serve(Vec::new(), sessions, sessions);
    let delete = |id: &str| {
        h.client
            .request("_kage/session/delete", serde_json::json!({"sessionId": id}))
    };
    assert!(delete(&closed[..8]).is_err(), "a prefix deletes nothing");
    assert!(
        kage_session::find_by_prefix(sessions, &closed)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_user_shell_command_streams_to_the_session_and_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());
    let ran = h.client.request(
        "_kage/shell",
        serde_json::json!({"sessionId": h.session, "command": "echo shared"}),
    );
    assert_eq!(ran.unwrap(), serde_json::json!({}));
    let updates = updates_until(&h.inbox, &h.session, "_kage/shell");
    let mut last = updates.last().unwrap()["update"].clone();
    while last["finished"] != true {
        last = updates_until(&h.inbox, &h.session, "_kage/shell")
            .last()
            .unwrap()["update"]
            .clone();
    }
    assert_eq!(last["command"], "echo shared");
    assert_eq!(last["exitCode"], 0);
    assert!(
        last["output"].as_str().unwrap().contains("shared"),
        "{last}"
    );
}

#[test]
fn withdrawing_from_an_empty_queue_answers_with_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(Vec::new(), dir.path(), dir.path());
    let withdrawn = h
        .client
        .request(
            "_kage/prompt/withdraw",
            serde_json::json!({"sessionId": h.session, "delivery": "queue"}),
        )
        .unwrap();
    assert_eq!(withdrawn, serde_json::json!({"delivery": "queue"}));
}

#[test]
fn the_mcp_catalog_arrives_whole() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_with(
        vec![text_turn("ok")],
        dir.path(),
        dir.path(),
        true,
        Modalities::default(),
    );
    let updates = updates_until(&h.inbox, &h.session, "_kage/mcp_servers");
    let servers = &updates.last().unwrap()["update"]["servers"];
    assert_eq!(servers[0]["name"], "srv");
    assert_eq!(servers[0]["prompts"][0]["name"], "p");
    let restarted = h.client.request(
        "_kage/mcp/restart",
        serde_json::json!({"sessionId": h.session, "server": "srv"}),
    );
    assert_eq!(restarted.unwrap(), serde_json::json!({}));
}

#[test]
fn withdrawing_a_queued_prompt_answers_the_requester_alone() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_paused(
        vec![Ok(ProviderEvent::MessageStart)],
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: CoreStopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
        vec![text_turn("titled")],
        dir.path(),
        dir.path(),
    );
    let first = prompt_async(&h.client, &h.session, "now");
    until(|| h.host.busy());
    let queued = prompt_async_with(&h.client, &h.session, "later", Some("queue"));
    let withdraw = || {
        h.client.request(
            "_kage/prompt/withdraw",
            serde_json::json!({"sessionId": h.session, "delivery": "queue"}),
        )
    };
    let deadline = Instant::now() + WAIT;
    let withdrawn = loop {
        let withdrawn = withdraw().unwrap();
        if withdrawn.get("content").is_some() || Instant::now() > deadline {
            break withdrawn;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(withdrawn["content"][0]["text"], "later");
    let seen = drain_quiet(&h.inbox, &h.session);
    assert!(
        !seen
            .iter()
            .any(|u| u["update"]["sessionUpdate"] == "_kage/prompt_withdrawn"),
        "an answered withdraw is not broadcast"
    );
    h.release.send(()).unwrap();
    first.recv_timeout(WAIT).unwrap().unwrap();
    drop(queued);
}

/// What a scripted plugin-host client answers to `_kage/plugin/tools`:
/// one tool, and one handler for `message_start`.
fn client_tools() -> serde_json::Value {
    serde_json::json!({
        "tools": [{
            "name": "echo_client",
            "description": "Echoes on the client",
            "schema": { "type": "object", "properties": {} },
        }],
        "handlers": { "message_start": 1 },
    })
}

/// Initializes as a client that hosts session plugins.
fn initialize_plugin_host(client: &Peer) {
    let params = serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": { "_meta": { "kage": { "pluginHost": true } } },
    });
    client.request("initialize", params).unwrap();
}

/// What the scripted plugin-host client answered, per method.
type PluginLog = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

/// Answers the `_kage/plugin/*` reverse requests of a client-hosted
/// session: `tools` and `call` with the given results, events and
/// hooks with nothing, drains with empty queues. Everything else is
/// forwarded to the test, and every answered method lands in the
/// returned log with its params.
fn serve_plugin_answers(
    client: &Peer,
    inbox: mpsc::Receiver<Inbound>,
    tools: serde_json::Value,
    call: serde_json::Value,
) -> (mpsc::Receiver<Inbound>, PluginLog) {
    let (tx, rx) = mpsc::channel();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let client = client.clone();
    std::thread::spawn(move || {
        for msg in inbox {
            let Inbound::Request { id, method, params } = &msg else {
                let _ = tx.send(msg);
                continue;
            };
            let answer = match method.as_str() {
                "_kage/plugin/tools" => Some(tools.clone()),
                "_kage/plugin/call" => Some(call.clone()),
                "_kage/plugin/event" | "_kage/plugin/hook" => Some(serde_json::Value::Null),
                "_kage/plugin/drain" => Some(serde_json::json!({})),
                _ => None,
            };
            match answer {
                Some(answer) => {
                    lock(&record).push((method.clone(), params.clone()));
                    let _ = client.respond(id, Ok(answer));
                }
                None => {
                    let _ = tx.send(msg);
                }
            }
        }
    });
    (rx, seen)
}

/// A `session/new` that asks for the client to host the session's
/// plugins.
fn client_hosted_new_session(client: &Peer, dir: &tempfile::TempDir) -> String {
    let params = serde_json::json!({
        "cwd": dir.path().display().to_string(),
        "mcpServers": [],
        "_meta": { "kage": { "plugins": "client" } },
    });
    let created = client.request("session/new", params).unwrap();
    created["sessionId"].as_str().unwrap().to_owned()
}

#[test]
fn a_client_plugin_ask_without_the_capability_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("hi")], dir.path(), dir.path());
    initialize(&h.client);
    let params = serde_json::json!({
        "cwd": dir.path().display().to_string(),
        "mcpServers": [],
        "_meta": { "kage": { "plugins": "client" } },
    });
    let err = h.client.request("session/new", params).unwrap_err();
    assert!(err.to_string().contains("pluginHost"), "{err}");
    let probe = h
        .client
        .request("_kage/round_trip", serde_json::Value::Null);
    assert!(
        probe.is_err(),
        "an unknown method is answered with an error"
    );
    let asked_plugin = std::iter::from_fn(|| h.inbox.try_recv().ok()).any(|msg| {
        matches!(
            msg,
            Inbound::Request { method, .. } if method.starts_with("_kage/plugin/")
        )
    });
    assert!(
        !asked_plugin,
        "no plugin request may reach a client without the capability"
    );
}

#[test]
fn session_new_fails_when_the_client_cannot_list_its_plugin_tools() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("hi")], dir.path(), dir.path());
    initialize_plugin_host(&h.client);
    let client = h.client.clone();
    std::thread::spawn(move || {
        for msg in h.inbox {
            if let Inbound::Request { id, method, .. } = msg {
                assert_eq!(method, "_kage/plugin/tools");
                let _ = client.respond(&id, Err(RpcError::internal("plugin host crashed")));
            }
        }
    });
    let params = serde_json::json!({
        "cwd": dir.path().display().to_string(),
        "mcpServers": [],
        "_meta": { "kage": { "plugins": "client" } },
    });
    let err = h.client.request("session/new", params).unwrap_err();
    assert!(err.to_string().contains("plugin host crashed"), "{err}");
}

#[test]
fn a_client_hosted_tool_runs_on_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(
        vec![
            tool_turn(
                "call_echo",
                "echo_client",
                serde_json::json!({ "text": "ping" }),
            ),
            text_turn("done"),
            text_turn("title"),
        ],
        dir.path(),
        dir.path(),
    );
    initialize_plugin_host(&h.client);
    let (inbox, seen) = serve_plugin_answers(
        &h.client,
        h.inbox,
        client_tools(),
        serde_json::json!({ "text": "client says hi" }),
    );
    let session = client_hosted_new_session(&h.client, &dir);
    let end = prompt_async(&h.client, &session, "call the client tool");
    let response = end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let requests = lock(&seen).clone();
    let snapshots = requests
        .iter()
        .filter(|(m, _)| m.as_str() == "_kage/plugin/tools")
        .collect::<Vec<_>>();
    assert_eq!(snapshots.len(), 1, "the snapshot is taken once");
    assert_eq!(snapshots[0].1["sessionId"], session);
    let calls = requests
        .iter()
        .filter(|(m, _)| m.as_str() == "_kage/plugin/call")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["tool"], "echo_client");
    assert_eq!(calls[0].1["input"]["text"], "ping");
    assert_eq!(calls[0].1["sessionId"], session);
    let updates = drain_quiet(&inbox, &session);
    assert!(
        contents_of(&updates, "call_echo")
            .iter()
            .any(|text| text.contains("client says hi"))
    );
}

#[test]
fn client_hosted_hooks_fire_only_for_reported_handlers() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("hi")], dir.path(), dir.path());
    initialize_plugin_host(&h.client);
    let (_inbox, seen) =
        serve_plugin_answers(&h.client, h.inbox, client_tools(), serde_json::json!({}));
    let session = client_hosted_new_session(&h.client, &dir);
    let end = prompt_async(&h.client, &session, "say hi");
    let response = end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    let requests = lock(&seen).clone();
    assert!(
        !requests
            .iter()
            .any(|(m, _)| m.as_str() == "_kage/plugin/call"),
        "no tool ran"
    );
    let events = requests
        .iter()
        .filter(|(m, _)| m.as_str() == "_kage/plugin/event")
        .collect::<Vec<_>>();
    assert!(!events.is_empty(), "message_start has a handler");
    assert!(
        events
            .iter()
            .all(|(_, params)| { params["event"] == "message_start" && params["notify"] == false })
    );
}

/// A spec the meta settings can apply to, with everything at its
/// default.
fn default_spec() -> SessionSpec {
    SessionSpec {
        id: SessionId::new(),
        model: "mock/m".to_owned(),
        cx: AgentContext::new("mock/m", ""),
        recorder: None,
        tools: builtin_registry(),
        gate: PermissionGate::new(PermissionsConfig::default()),
        loop_cfg: LoopConfig::default(),
        plugins: None,
        mcp: None,
        interactive: true,
        title: true,
        agents: Some(AgentSetup::from_options(
            AgentDefs::default(),
            &kage_core::options::OptionStore::default(),
            Background::Hold,
        )),
        shell: None,
    }
}

#[test]
fn session_meta_settings_reach_the_spec() {
    let meta = NewSessionMeta {
        kage: Some(NewSessionKage {
            plugins: None,
            background: Some(SessionBackground::Wake),
            yolo: true,
            system_prompt: Some("be terse".into()),
            opt: Some(SessionOpt {
                compaction_threshold: Some(0.5),
                max_provider_retries: Some(2),
                thinking_level: Some("high".into()),
            }),
            max_output_tokens: Some(1_024),
            external_agent: None,
        }),
    };
    let settings = SessionSettings::of(Some(&meta), false).unwrap();
    assert_eq!(settings.system_prompt.as_deref(), Some("be terse"));
    let mut spec = default_spec();
    settings.apply(&mut spec);
    assert_eq!(spec.gate.mode(), Some(PermissionAction::Allow));
    assert!((spec.loop_cfg.compaction_threshold - 0.5).abs() < f32::EPSILON);
    assert_eq!(spec.loop_cfg.max_provider_retries, 2);
    assert_eq!(spec.cx.thinking_level, Some(ThinkingLevel::High));
    assert_eq!(spec.cx.max_output_tokens, Some(1_024));
    let agents = spec.agents.unwrap();
    assert_eq!(agents.background, Background::Wake);
}

#[test]
fn a_session_without_meta_keeps_the_host_defaults() {
    let settings = SessionSettings::of(None, false).unwrap();
    let mut spec = default_spec();
    settings.apply(&mut spec);
    assert_eq!(spec.gate.mode(), None);
    assert_eq!(spec.cx.max_output_tokens, None);
    assert_eq!(spec.agents.unwrap().background, Background::Hold);

    // A kage client's background agents wake it, as before the meta.
    let mut spec = default_spec();
    SessionSettings::of(None, true).unwrap().apply(&mut spec);
    assert_eq!(spec.agents.unwrap().background, Background::Wake);
    let mut spec = default_spec();
    SessionSettings::host_defaults(true).apply(&mut spec);
    assert_eq!(spec.agents.unwrap().background, Background::Wake);
}

#[test]
fn a_session_new_meta_sets_the_session_up() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("hi")], dir.path(), dir.path());
    initialize(&h.client);
    let params = serde_json::json!({
        "cwd": dir.path().display().to_string(),
        "mcpServers": [],
        "_meta": { "kage": {
            "yolo": true,
            "background": "wake",
            "opt": { "thinkingLevel": "high" },
            "maxOutputTokens": 1_024,
        }},
    });
    let created = h.client.request("session/new", params).unwrap();
    let thinking = created["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == "thinking")
        .unwrap_or_else(|| panic!("thinking option in {created}"));
    assert_eq!(thinking["currentValue"], "high");
    let mode = created["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == "mode")
        .unwrap_or_else(|| panic!("mode option in {created}"));
    assert_eq!(mode["currentValue"], "allow", "yolo opens in allow mode");
}

fn new_session_with_opt(
    opt: &serde_json::Value,
) -> Result<serde_json::Value, kage_jsonrpc::RpcError> {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("hi")], dir.path(), dir.path());
    initialize(&h.client);
    let params = serde_json::json!({
        "cwd": dir.path().display().to_string(),
        "mcpServers": [],
        "_meta": { "kage": { "opt": opt } },
    });
    h.client.request("session/new", params)
}

#[test]
fn out_of_range_session_opts_are_invalid_params() {
    for opt in [
        serde_json::json!({ "compactionThreshold": 1.5 }),
        serde_json::json!({ "compactionThreshold": -0.1 }),
        serde_json::json!({ "maxProviderRetries": -1 }),
        serde_json::json!({ "maxProviderRetries": 11 }),
        serde_json::json!({ "thinkingLevel": "bogus" }),
    ] {
        let err = new_session_with_opt(&opt).unwrap_err();
        assert_eq!(err.code, -32602, "{opt}: {err}");
    }
}

#[test]
fn in_range_session_opts_are_accepted() {
    for opt in [
        serde_json::json!({ "compactionThreshold": 0.0, "maxProviderRetries": 0 }),
        serde_json::json!({ "compactionThreshold": 1.0, "maxProviderRetries": 10 }),
        serde_json::json!({ "thinkingLevel": "auto" }),
        serde_json::json!({ "thinkingLevel": "default" }),
    ] {
        new_session_with_opt(&opt).unwrap_or_else(|e| panic!("{opt}: {e}"));
    }
}
#[test]
fn external_host_refuses_configured_agents_in_isolated_process() {
    use serde_json::{Value, json};

    // The production config intentionally ignores project ACP settings. Run the
    // server test in a child with its own user config and environment instead.
    if std::env::var_os("KAGE_TEST_EXTERNAL_CHILD").is_none() {
        run_external_host_test_in_child();
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![text_turn("engine answer"), text_turn("title")]);
    let host = test_host(
        Arc::new(Listed::of(mock.clone())),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        false,
    );
    let (srv_r, mut cli_w) = std::io::pipe().unwrap();
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let running = Arc::clone(&host);
    std::thread::spawn(move || running.serve(BufReader::new(srv_r), srv_w).unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(cli_r).lines() {
            if tx
                .send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    let send = |writer: &mut std::io::PipeWriter, value: Value| {
        writeln!(writer, "{value}").unwrap();
        writer.flush().unwrap();
    };
    let recv = || rx.recv_timeout(WAIT).expect("no RPC message");
    send(
        &mut cli_w,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}),
    );
    let init = recv();
    assert_ne!(init["result"]["_meta"]["kage"]["externalSessions"], true);
    let new = |id, name: &str| json!({"jsonrpc":"2.0","id":id,"method":"session/new","params":{"cwd":dir.path(),"mcpServers":[],"_meta":{"kage":{"externalAgent":name}}}});
    for (id, name) in [(2, "missing"), (3, "broken"), (4, "fake")] {
        send(&mut cli_w, new(id, name));
        let error = recv();
        assert_eq!(error["id"], id);
        assert_eq!(error["error"]["code"], -32602);
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("temporarily unavailable")
        );
        assert!(mock.requests().is_empty());
        assert!(lock(&host.external).is_empty());
    }
    send(
        &mut cli_w,
        json!({"jsonrpc":"2.0","id":10,"method":"session/new","params":{"cwd":dir.path(),"mcpServers":[]}}),
    );
    let ordinary = loop {
        let msg = recv();
        if msg["id"] == 10 {
            break msg;
        }
    };
    let ordinary_id = ordinary["result"]["sessionId"].as_str().unwrap();
    send(
        &mut cli_w,
        json!({"jsonrpc":"2.0","id":11,"method":"session/prompt","params":{"sessionId":ordinary_id,"prompt":[{"type":"text","text":"engine"}]}}),
    );
    loop {
        let msg = recv();
        if msg["id"] == 11 {
            assert_eq!(msg["result"]["stopReason"], "end_turn");
            break;
        }
    }
    assert!(!mock.requests().is_empty());
}

fn run_external_host_test_in_child() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/external_acp.py");
    let contents = format!(
        "[acp.agents.fake]\ncommand = \"python3\"\nargs = [{}]\n\n[acp.agents.broken]\ncommand = \"/nonexistent/kage-fake-agent\"\n",
        serde_json::to_string(script.to_str().unwrap()).unwrap()
    );
    std::fs::write(config.join("config.toml"), contents).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "rpc::tests::external_host_refuses_configured_agents_in_isolated_process",
            "--nocapture",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("KAGE_TEST_EXTERNAL_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[cfg(target_os = "linux")]
// The creator's open stays unresolved, as while a creation response is in
// flight, until the test resolves or drops the returned ticket.
fn external_race_fixture() -> (
    tempfile::TempDir,
    Arc<Host>,
    Arc<CliAcpAgent>,
    Arc<CliAcpAgent>,
    external::OpenTicket,
) {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::sequence(vec![]);
    let host = test_host(
        Arc::new(Listed::of(mock)),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        false,
    );
    let peer = || {
        let (peer, _, _) = kage_jsonrpc::connect(std::io::empty(), std::io::sink());
        peer.into()
    };
    let owner = Arc::new(CliAcpAgent::new(Arc::clone(&host), peer()));
    let other = Arc::new(CliAcpAgent::new(Arc::clone(&host), peer()));
    let (session, created) = external_race_session(dir.path(), &owner);
    lock(&host.external).insert("external".into(), session);
    lock(&owner.ids).external.insert("external".into());
    (dir, host, owner, other, created)
}

#[cfg(target_os = "linux")]
fn external_race_session(
    dir: &Path,
    owner: &CliAcpAgent,
) -> (Arc<external::External>, external::OpenTicket) {
    let config = kage_core::config::AcpAgent {
        command: "python3".into(),
        args: vec![
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/external_acp.py")
                .to_string_lossy()
                .into_owned(),
        ],
        env: BTreeMap::new(),
    };
    external::External::start(
        &owner.host.lifecycles,
        "external".into(),
        "fake",
        dir.to_str().unwrap(),
        &config,
        owner.connection,
        owner.peer.clone(),
    )
    .unwrap()
}

#[cfg(target_os = "linux")]
fn wait_until_external_locked(host: &Host) -> bool {
    let deadline = Instant::now() + WAIT;
    while host.external.try_lock().is_ok() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::yield_now();
    }
    true
}

#[cfg(target_os = "linux")]
#[test]
fn external_attach_before_last_close_keeps_session_alive() {
    let (_dir, host, owner, other, _created) = external_race_fixture();
    let ids = lock(&other.ids);
    std::thread::scope(|scope| {
        let attaching = scope.spawn(|| other.attach_external("external", &[], None));
        if !wait_until_external_locked(&host) {
            drop(ids);
            panic!("operation did not lock external sessions");
        }
        let closing = scope.spawn(|| {
            owner.close_session(CloseSessionRequest {
                session_id: "external".into(),
            })
        });
        drop(ids);
        attaching.join().unwrap().unwrap().unwrap()();
        closing.join().unwrap().unwrap();
    });
    assert!(lock(&host.external).contains_key("external"));
    assert!(other.external_session("external").is_some());
    other.disconnect_external();
    assert!(lock(&host.external).is_empty());
    owner.detach();
    other.detach();
}

#[cfg(target_os = "linux")]
#[test]
fn external_attach_blocked_by_disconnect_does_not_leak_attachment() {
    let (_dir, host, owner, other, _created) = external_race_fixture();
    let ids = lock(&other.ids);
    std::thread::scope(|scope| {
        let attaching = scope.spawn(|| other.attach_external("external", &[], None));
        if !wait_until_external_locked(&host) {
            drop(ids);
            panic!("operation did not lock external sessions");
        }
        let disconnecting = scope.spawn(|| other.disconnect_external());
        let deadline = Instant::now() + WAIT;
        while !other.disconnecting.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                drop(ids);
                panic!("disconnect did not start");
            }
            std::thread::yield_now();
        }
        drop(ids);
        let error = attaching
            .join()
            .unwrap()
            .err()
            .expect("attachment succeeded");
        assert!(error.message.contains("disconnecting"));
        disconnecting.join().unwrap();
    });
    assert!(lock(&other.ids).external.is_empty());
    owner.disconnect_external();
    assert!(lock(&host.external).is_empty());
    owner.detach();
    other.detach();
}

#[cfg(target_os = "linux")]
#[test]
fn external_last_close_before_attach_removes_session() {
    let (_dir, host, owner, other, _created) = external_race_fixture();
    owner
        .close_session(CloseSessionRequest {
            session_id: "external".into(),
        })
        .unwrap();
    assert!(lock(&host.external).is_empty());
    assert!(
        other
            .attach_external("external", &[], None)
            .unwrap()
            .is_none()
    );
    assert!(lock(&other.ids).external.is_empty());
    owner.detach();
    other.detach();
}

#[cfg(target_os = "linux")]
#[test]
fn external_close_and_reattach_same_connection_are_ordered() {
    let (_dir, host, owner, other, _created) = external_race_fixture();
    other
        .attach_external("external", &[], None)
        .unwrap()
        .unwrap()();
    let ids = lock(&owner.ids);
    std::thread::scope(|scope| {
        let closing = scope.spawn(|| {
            owner.close_session(CloseSessionRequest {
                session_id: "external".into(),
            })
        });
        if !wait_until_external_locked(&host) {
            drop(ids);
            panic!("operation did not lock external sessions");
        }
        let attaching = scope.spawn(|| owner.attach_external("external", &[], None));
        drop(ids);
        closing.join().unwrap().unwrap();
        attaching.join().unwrap().unwrap().unwrap()();
    });
    let session = owner.external_session("external").unwrap();
    assert!(session.register_prompt(owner.connection).is_ok());
    owner.detach();
    other.detach();
}

#[cfg(target_os = "linux")]
struct DelayedPromptAgent {
    inner: Arc<CliAcpAgent>,
    entered: mpsc::Sender<bool>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
}

#[cfg(target_os = "linux")]
impl Agent for DelayedPromptAgent {
    fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
        self.inner.initialize(req)
    }

    fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
        self.inner.new_session(req)
    }

    fn prepare_prompt(
        &self,
        req: &PromptRequest,
    ) -> Result<Option<Box<dyn std::any::Any + Send>>, RpcError> {
        self.inner.prepare_prompt(req)
    }

    fn prompt_prepared(
        &self,
        req: PromptRequest,
        ctx: &PromptContext,
        prepared: Option<Box<dyn std::any::Any + Send>>,
    ) -> Result<PromptResponse, RpcError> {
        let release = lock(&self.release).take();
        if let Some(release) = release {
            self.entered.send(prepared.is_some()).unwrap();
            release.recv_timeout(WAIT).unwrap();
        }
        self.inner.prompt_prepared(req, ctx, prepared)
    }

    fn prompt(&self, req: PromptRequest, ctx: &PromptContext) -> Result<PromptResponse, RpcError> {
        self.inner.prompt(req, ctx)
    }

    fn cancel(&self, session_id: &str) {
        self.inner.cancel(session_id);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn saturated_external_session_leaves_workers_for_another_session() {
    let (dir, host, owner, other, _created) = external_race_fixture();
    let session = owner.external_session("external").unwrap();
    session
        .attach_open(owner.connection, owner.peer.clone())
        .unwrap()
        .announce();
    let (independent, _independent_created) = external_race_session(dir.path(), &owner);
    independent
        .attach_open(owner.connection, owner.peer.clone())
        .unwrap()
        .announce();
    lock(&host.external).insert("independent".into(), independent);
    lock(&owner.ids).external.insert("independent".into());

    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let agent = Arc::clone(&owner);
    let (entered, _entered_rx) = mpsc::channel();
    std::thread::spawn(move || {
        kage_acp::agent::serve_agent(BufReader::new(srv_r), srv_w, |_| DelayedPromptAgent {
            inner: agent,
            entered,
            release: Mutex::new(None),
        })
        .unwrap();
    });
    let (client, _inbox, _reader) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
    let pending: Vec<_> = (0..16)
        .map(|_| prompt_async(&client, "external", "wait"))
        .collect();
    let deadline = Instant::now() + WAIT;
    loop {
        match session.register_prompt(owner.connection) {
            Ok(ticket) => drop(ticket),
            Err(_) => break,
        }
        assert!(Instant::now() < deadline, "external admission did not fill");
        std::thread::yield_now();
    }
    let rejected = client.request(
        "session/prompt",
        serde_json::json!({"sessionId":"external", "prompt":[{"type":"text", "text":"extra"}]}),
    );
    assert!(
        rejected
            .unwrap_err()
            .message
            .contains("too many external prompts")
    );
    let result = prompt_async(&client, "independent", "hello")
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(result["stopReason"], "end_turn");

    owner.cancel("external");
    for response in pending {
        assert_eq!(
            response.recv_timeout(WAIT).unwrap().unwrap()["stopReason"],
            "cancelled"
        );
    }
    owner.detach();
    other.detach();
}

#[cfg(target_os = "linux")]
fn delayed_external_prompt_test(reattach: bool) {
    let (_dir, _host, owner, other, _created) = external_race_fixture();
    owner
        .external_session("external")
        .unwrap()
        .attach_open(owner.connection, owner.peer.clone())
        .unwrap()
        .announce();
    if reattach {
        other
            .attach_external("external", &[], None)
            .unwrap()
            .unwrap()();
    }
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let inner = Arc::clone(&owner);
    std::thread::spawn(move || {
        kage_acp::agent::serve_agent(BufReader::new(srv_r), srv_w, |_| DelayedPromptAgent {
            inner,
            entered: entered_tx,
            release: Mutex::new(Some(release_rx)),
        })
        .unwrap();
    });
    let (client, _inbox, _reader) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
    let params =
        |text| serde_json::json!({"sessionId":"external", "prompt":[{"type":"text", "text":text}]});
    let (done_tx, done_rx) = mpsc::channel();
    let requester = client.clone();
    std::thread::spawn(move || {
        done_tx
            .send(requester.request("session/prompt", params("wait")))
            .unwrap();
    });
    assert!(
        entered_rx.recv_timeout(WAIT).unwrap(),
        "external prompt was not prepared at dispatch"
    );
    if reattach {
        owner
            .close_session(CloseSessionRequest {
                session_id: "external".into(),
            })
            .unwrap();
        owner
            .attach_external("external", &[], None)
            .unwrap()
            .unwrap()();
    } else {
        owner.cancel("external");
    }
    release_tx.send(()).unwrap();
    let result = done_rx.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(result["stopReason"], "cancelled");
    let result = client.request("session/prompt", params("next")).unwrap();
    assert_eq!(result["stopReason"], "end_turn");
    owner.detach();
    other.detach();
}

#[cfg(target_os = "linux")]
#[test]
fn dispatch_prepared_external_prompt_does_not_cross_reattachment() {
    delayed_external_prompt_test(true);
}

#[cfg(target_os = "linux")]
#[test]
fn dispatch_prepared_external_prompt_is_cancelled_before_worker_runs() {
    delayed_external_prompt_test(false);
}

#[test]
fn native_prompt_preparation_defaults_to_normal_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![text_turn("native ok")], dir.path(), dir.path());
    let peer = || {
        let (peer, _, _) = kage_jsonrpc::connect(std::io::empty(), std::io::sink());
        peer.into()
    };
    let agent = CliAcpAgent::new(Arc::clone(&h.host), peer());
    let req: PromptRequest = serde_json::from_value(serde_json::json!({
        "sessionId": h.session, "prompt": [{"type":"text", "text":"hello"}]
    }))
    .unwrap();
    assert!(agent.prepare_prompt(&req).unwrap().is_none());
    let result = h
        .client
        .request("session/prompt", serde_json::to_value(req).unwrap())
        .unwrap();
    assert_eq!(result["stopReason"], "end_turn");
}

#[cfg(target_os = "linux")]
fn external_receipt_prompt(
    session: &external::External,
    owner: u64,
    text: &str,
) -> Result<PromptResponse, RpcError> {
    let req: PromptRequest = serde_json::from_value(serde_json::json!({
        "sessionId": "external", "prompt": [{"type": "text", "text": text}]
    }))
    .unwrap();
    session.prompt(session.register_prompt(owner).unwrap(), req)
}

#[cfg(target_os = "linux")]
fn pending_external_receipt_prompt(
    session: &Arc<external::External>,
    owner: u64,
    text: &str,
) -> mpsc::Receiver<Result<PromptResponse, RpcError>> {
    let req: PromptRequest = serde_json::from_value(serde_json::json!({
        "sessionId": "external", "prompt": [{"type": "text", "text": text}]
    }))
    .unwrap();
    let ticket = session.register_prompt(owner).unwrap();
    let session = Arc::clone(session);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(session.prompt(ticket, req));
    });
    rx
}

#[cfg(target_os = "linux")]
#[test]
fn prepared_external_prompt_waits_for_receipt_and_cancellation_prevents_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("agent.py");
    let marker = dir.path().join("prompts");
    std::fs::write(
        &script,
        r"
import json, os, sys
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get('method')
    if method == 'initialize':
        result = {'protocolVersion': 1, 'agentCapabilities': {}}
    elif method == 'session/new':
        result = {'sessionId': 'upstream'}
    elif method == 'session/prompt':
        with open(os.environ['PROMPT_MARKER'], 'a') as f:
            f.write(msg['params']['prompt'][0]['text'] + '\n')
        result = {'stopReason': 'end_turn'}
    else:
        continue
    print(json.dumps({'jsonrpc': '2.0', 'id': msg['id'], 'result': result}), flush=True)
",
    )
    .unwrap();
    let mock = MockProvider::sequence(vec![]);
    let host = test_host(
        Arc::new(Listed::of(mock)),
        dir.path().into(),
        dir.path().into(),
        false,
    );
    let (peer, _, _) = kage_jsonrpc::connect(std::io::empty(), std::io::sink());
    let owner = CliAcpAgent::new(Arc::clone(&host), peer.into());
    let config = kage_core::config::AcpAgent {
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        env: BTreeMap::from([(
            "PROMPT_MARKER".into(),
            marker.to_string_lossy().into_owned(),
        )]),
    };
    let (session, receipt) = external::External::start(
        &host.lifecycles,
        "external".into(),
        "fake",
        dir.path().to_str().unwrap(),
        &config,
        owner.connection,
        owner.peer.clone(),
    )
    .unwrap();
    lock(&host.external).insert("external".into(), Arc::clone(&session));
    lock(&owner.ids).external.insert("external".into());

    let pending = pending_external_receipt_prompt(&session, owner.connection, "after receipt");
    assert!(matches!(
        pending.recv_timeout(Duration::from_millis(200)),
        Err(RecvTimeoutError::Timeout)
    ));
    assert!(!marker.exists(), "prompt dispatched before receipt");
    receipt.announce();
    assert!(pending.recv_timeout(WAIT).unwrap().is_ok());
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "after receipt\n");

    let (peer, _, _) = kage_jsonrpc::connect(std::io::empty(), std::io::sink());
    let other = CliAcpAgent::new(Arc::clone(&host), peer.into());
    let receipt = other
        .attach_external("external", &[], None)
        .unwrap()
        .unwrap();
    let pending = pending_external_receipt_prompt(&session, other.connection, "cancelled");
    assert!(matches!(
        pending.recv_timeout(Duration::from_millis(200)),
        Err(RecvTimeoutError::Timeout)
    ));
    other.cancel("external");
    assert_eq!(
        pending.recv_timeout(WAIT).unwrap().unwrap().stop_reason,
        kage_acp::acp::StopReason::Cancelled
    );
    receipt();
    assert!(external_receipt_prompt(&session, other.connection, "next").is_ok());
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "after receipt\nnext\n"
    );
    owner.detach();
    other.detach();
}

#[cfg(target_os = "linux")]
#[test]
fn external_concurrent_open_receipts_survive_reverse_response_order() {
    let (_dir, _host, owner, other, _created) = external_race_fixture();
    let session = owner.external_session("external").unwrap();
    let (first_tx, first_rx) = mpsc::channel();
    let (second_tx, second_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let other = &other;
        scope.spawn(move || {
            first_tx
                .send(
                    other
                        .attach_external("external", &[], None)
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
        });
        scope.spawn(move || {
            second_tx
                .send(
                    other
                        .attach_external("external", &[], None)
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
        });
    });
    let first = first_rx.recv_timeout(WAIT).unwrap();
    let second = second_rx.recv_timeout(WAIT).unwrap();
    let before = pending_external_receipt_prompt(&session, other.connection, "before");
    assert!(matches!(
        before.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout)
    ));
    // The second response completes before the first.
    second();
    assert!(before.recv_timeout(WAIT).unwrap().is_ok());
    assert!(external_receipt_prompt(&session, other.connection, "after").is_ok());
    first();
    assert!(external_receipt_prompt(&session, other.connection, "again").is_ok());
    owner.detach();
    other.detach();
}

#[cfg(target_os = "linux")]
#[test]
fn external_failed_open_releases_the_connection_claim_and_last_session() {
    let (_dir, host, owner, other, _created) = external_race_fixture();
    let session = owner.external_session("external").unwrap();
    drop(
        other
            .attach_external("external", &[], None)
            .unwrap()
            .unwrap(),
    );
    assert!(!lock(&other.ids).external.contains("external"));
    assert!(!session.owners().contains(&other.connection));
    assert!(lock(&host.external).contains_key("external"));

    let first = other
        .attach_external("external", &[], None)
        .unwrap()
        .unwrap();
    let second = other
        .attach_external("external", &[], None)
        .unwrap()
        .unwrap();
    drop(first);
    assert!(lock(&other.ids).external.contains("external"));
    owner
        .close_session(CloseSessionRequest {
            session_id: "external".into(),
        })
        .unwrap();
    assert!(lock(&host.external).contains_key("external"));
    let unwound = std::thread::spawn(move || {
        let _second = second;
        panic!("injected open worker panic");
    })
    .join();
    assert!(unwound.is_err());
    assert!(!lock(&other.ids).external.contains("external"));
    assert!(lock(&host.external).is_empty());
    owner.detach();
    other.detach();
}

#[cfg(target_os = "linux")]
#[test]
fn external_dropped_and_stale_receipts_do_not_announce_replacement() {
    let (_dir, _host, owner, other, _created) = external_race_fixture();
    let session = owner.external_session("external").unwrap();
    let stale = session
        .attach_open(owner.connection, owner.peer.clone())
        .unwrap();
    let dropped = session
        .attach_open(owner.connection, owner.peer.clone())
        .unwrap();
    drop(dropped);
    let dropped_prompt = pending_external_receipt_prompt(&session, owner.connection, "dropped");
    assert!(matches!(
        dropped_prompt.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout)
    ));
    owner.cancel("external");
    assert_eq!(
        dropped_prompt
            .recv_timeout(WAIT)
            .unwrap()
            .unwrap()
            .stop_reason,
        kage_acp::acp::StopReason::Cancelled
    );

    // Keep the process alive while replacing the owner's attachment.
    let other_receipt = other
        .attach_external("external", &[], None)
        .unwrap()
        .unwrap();
    other_receipt();
    owner
        .close_session(CloseSessionRequest {
            session_id: "external".into(),
        })
        .unwrap();
    let replacement = owner
        .attach_external("external", &[], None)
        .unwrap()
        .unwrap();
    stale.announce();
    let pending = pending_external_receipt_prompt(&session, owner.connection, "stale");
    assert!(matches!(
        pending.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout)
    ));
    replacement();
    assert!(pending.recv_timeout(WAIT).unwrap().is_ok());
    assert!(external_receipt_prompt(&session, owner.connection, "replacement").is_ok());
    owner.detach();
    other.detach();
}

#[cfg(target_os = "linux")]
#[test]
fn external_stale_receipt_does_not_announce_replaced_session() {
    let (dir, host, owner, other, _created) = external_race_fixture();
    let old = owner.external_session("external").unwrap();
    let stale = owner
        .attach_external("external", &[], None)
        .unwrap()
        .unwrap();
    owner
        .close_session(CloseSessionRequest {
            session_id: "external".into(),
        })
        .unwrap();
    let (replacement, _replacement_created) = external_race_session(dir.path(), &owner);
    assert!(
        lock(&host.external)
            .insert("external".into(), Arc::clone(&replacement))
            .is_none()
    );
    let receipt = owner
        .attach_external("external", &[], None)
        .unwrap()
        .unwrap();
    stale();
    let pending = pending_external_receipt_prompt(&replacement, owner.connection, "stale");
    assert!(matches!(
        pending.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout)
    ));
    receipt();
    assert!(pending.recv_timeout(WAIT).unwrap().is_ok());
    assert!(external_receipt_prompt(&replacement, owner.connection, "replacement").is_ok());
    assert!(!old.detach(owner.connection));
    owner.detach();
    other.detach();
    assert!(lock(&host.external).is_empty());
}

#[test]
fn native_new_session_with_receipt_preserves_legacy_announcement() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve(vec![], dir.path(), dir.path());
    let (peer, _, _) = kage_jsonrpc::connect(std::io::empty(), std::io::sink());
    let agent = CliAcpAgent::new(Arc::clone(&h.host), peer.into());
    let (response, receipt) = agent
        .new_session_with_receipt(NewSessionRequest {
            cwd: dir.path().to_string_lossy().into_owned(),
            mcp_servers: vec![],
            meta: None,
        })
        .unwrap();
    assert!(receipt.is_none());
    agent.session_announced(&response.session_id);
    agent.detach();
}

/// Forwards to a host connection, holding the first resume and the first
/// close worker until released.
#[cfg(target_os = "linux")]
struct FencedAgent {
    inner: Arc<CliAcpAgent>,
    resume_entered: mpsc::Sender<()>,
    resume_release: Mutex<Option<mpsc::Receiver<()>>>,
    close_release: Mutex<Option<mpsc::Receiver<()>>>,
}

#[cfg(target_os = "linux")]
impl Agent for FencedAgent {
    fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
        self.inner.initialize(req)
    }

    fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
        self.inner.new_session(req)
    }

    fn prepare_prompt(
        &self,
        req: &PromptRequest,
    ) -> Result<Option<Box<dyn std::any::Any + Send>>, RpcError> {
        self.inner.prepare_prompt(req)
    }

    fn prompt_prepared(
        &self,
        req: PromptRequest,
        ctx: &PromptContext,
        prepared: Option<Box<dyn std::any::Any + Send>>,
    ) -> Result<PromptResponse, RpcError> {
        self.inner.prompt_prepared(req, ctx, prepared)
    }

    fn prompt(&self, req: PromptRequest, ctx: &PromptContext) -> Result<PromptResponse, RpcError> {
        self.inner.prompt(req, ctx)
    }

    fn cancel(&self, session_id: &str) {
        self.inner.cancel(session_id);
    }

    fn close_session(&self, req: CloseSessionRequest) -> Result<CloseSessionResponse, RpcError> {
        self.inner.close_session(req)
    }

    fn fence_close(&self, req: &CloseSessionRequest) -> Option<Box<dyn std::any::Any + Send>> {
        self.inner.fence_close(req)
    }

    fn close_session_fenced(
        &self,
        req: CloseSessionRequest,
        fence: Option<Box<dyn std::any::Any + Send>>,
    ) -> Result<CloseSessionResponse, RpcError> {
        let release = lock(&self.close_release).take();
        if let Some(release) = release {
            release.recv_timeout(WAIT).unwrap();
        }
        self.inner.close_session_fenced(req, fence)
    }

    fn prepare_resume(&self, req: &ResumeSessionRequest) -> Option<Box<dyn std::any::Any + Send>> {
        self.inner.prepare_resume(req)
    }

    fn resume_session_prepared(
        &self,
        req: ResumeSessionRequest,
        ctx: &PromptContext,
        prepared: Option<Box<dyn std::any::Any + Send>>,
    ) -> kage_acp::agent::OpenResult<ResumeSessionResponse> {
        let release = lock(&self.resume_release).take();
        if let Some(release) = release {
            self.resume_entered.send(()).unwrap();
            release.recv_timeout(WAIT).unwrap();
        }
        self.inner.resume_session_prepared(req, ctx, prepared)
    }
}

/// Serves `agent` and returns a raw writer and a reader of responses by id,
/// so tests control the exact order of messages on the wire.
#[cfg(target_os = "linux")]
fn serve_raw(
    agent: FencedAgent,
) -> (
    std::io::PipeWriter,
    mpsc::Receiver<(u64, serde_json::Value)>,
) {
    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    std::thread::spawn(move || {
        let _ = kage_acp::agent::serve_agent(BufReader::new(srv_r), srv_w, |_| agent);
    });
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(cli_r).lines() {
            let Ok(line) = line else { break };
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if let Some(id) = value["id"].as_u64() {
                let _ = tx.send((id, value));
            }
        }
    });
    (cli_w, rx)
}

#[cfg(target_os = "linux")]
#[test]
fn external_close_fences_a_prompt_dispatched_after_it() {
    let (_dir, host, owner, other, _created) = external_race_fixture();
    other
        .attach_external("external", &[], None)
        .unwrap()
        .unwrap()();
    let session = owner.external_session("external").unwrap();
    session
        .attach_open(owner.connection, owner.peer.clone())
        .unwrap()
        .announce();
    let (entered, _) = mpsc::channel();
    let (release, close_release) = mpsc::channel();
    let (mut wire, rx) = serve_raw(FencedAgent {
        inner: Arc::clone(&owner),
        resume_entered: entered,
        resume_release: Mutex::new(None),
        close_release: Mutex::new(Some(close_release)),
    });
    writeln!(
        wire,
        "{}\n{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"session/close","params":{"sessionId":"external"}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"external","prompt":[{"type":"text","text":"late"}]}}),
    )
    .unwrap();
    // The close worker is held, so only the dispatch-time fence can stop
    // the prompt from reaching the session.
    let (id, prompt) = rx.recv_timeout(WAIT).unwrap();
    assert_eq!(id, 2);
    assert_eq!(prompt["error"]["code"], -32602, "{prompt}");
    release.send(()).unwrap();
    let (id, close) = rx.recv_timeout(WAIT).unwrap();
    assert_eq!(id, 1);
    assert_eq!(close["result"], serde_json::json!({}));
    assert!(!session.owners().contains(&owner.connection));
    assert!(lock(&host.external).contains_key("external"));
    drop(wire);
    owner.detach();
    other.detach();
}

#[cfg(target_os = "linux")]
#[test]
fn external_close_fences_a_resume_dispatched_before_it() {
    let (_dir, host, owner, other, _created) = external_race_fixture();
    let session = owner.external_session("external").unwrap();
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let (close, close_release) = mpsc::channel();
    let (mut wire, rx) = serve_raw(FencedAgent {
        inner: Arc::clone(&other),
        resume_entered: entered_tx,
        resume_release: Mutex::new(Some(release_rx)),
        close_release: Mutex::new(Some(close_release)),
    });
    writeln!(
        wire,
        "{}\n{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"session/resume","params":{"sessionId":"external","cwd":"/","mcpServers":[]}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"session/close","params":{"sessionId":"external"}}),
    )
    .unwrap();
    entered.recv_timeout(WAIT).unwrap();
    // The close worker is held, so only the dispatch-time fence can stop
    // the resume from attaching.
    release.send(()).unwrap();
    let (id, resume) = rx.recv_timeout(WAIT).unwrap();
    assert_eq!(id, 1);
    assert!(resume["error"].is_object(), "{resume}");
    close.send(()).unwrap();
    let (id, closed) = rx.recv_timeout(WAIT).unwrap();
    assert_eq!(id, 2);
    assert_eq!(closed["result"], serde_json::json!({}), "{closed}");
    assert!(!session.owners().contains(&other.connection));
    assert!(lock(&other.ids).external.is_empty());
    assert!(lock(&other.ids).resuming.is_empty());
    assert!(lock(&host.external).contains_key("external"));
    drop(wire);
    owner.detach();
    other.detach();
}
