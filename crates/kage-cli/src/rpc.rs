//! `kage rpc`: a spec-conformant Agent Client Protocol agent.
//!
//! Speaks ACP (newline-delimited JSON-RPC 2.0 over stdio, protocol
//! version 1) so editors that speak ACP can drive kage. Every ACP session
//! is an engine session: prompts become engine commands, and a bus
//! subscriber turns engine events into `session/update` notifications and
//! `session/request_permission` requests.

use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use kage_acp::acp::{
    AgentCapabilities, ContentBlock, Implementation, InitializeRequest, InitializeResponse,
    LoadSessionRequest, MessageChunk, NewSessionRequest, NewSessionResponse, PROTOCOL_VERSION,
    PromptCapabilities, PromptRequest, PromptResponse, SessionUpdate, StopReason, ToolCall,
    ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use kage_acp::agent::{Agent, PermissionDecision, PromptContext, send_update, serve_agent};
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{
    Command, CommandKind, Delivery, Envelope, Event, HostEvent, PermissionDecision as Decision,
    RunOutcome,
};
use kage_core::sync::lock;
use kage_core::{Content, LoopEvent, Role, SessionId, StopReason as CoreStopReason};
use kage_jsonrpc::{Peer, RpcError};
use kage_loop::{AgentContext, LoopConfig};
use kage_provider::ProviderRegistry;
use kage_session::SessionWriter;
use kage_tools::builtin_registry;

use crate::engine::{Commander, Engine, Recorder, SessionSpec};
use crate::permissions::PermissionGate;
use crate::runtime_env;

/// Entry point for the `Rpc` subcommand.
pub(crate) fn run(model_override: Option<&str>, system_role: &str) -> ExitCode {
    let registry = crate::build_provider_registry();
    if !crate::has_usable_provider(&registry) && model_override.is_none() {
        eprintln!(
            "kage: rpc: no provider credentials found; run `kage auth login` or set an API-key env var"
        );
        return ExitCode::from(1);
    }
    let default_model =
        model_override.map_or_else(|| crate::default_model(&registry), str::to_owned);
    if let Err(e) = registry.resolve(&default_model) {
        eprintln!("kage: rpc: cannot resolve model {default_model}: {e}");
        return ExitCode::from(1);
    }
    let registry = Arc::new(registry);
    let reader = BufReader::new(std::io::stdin());
    let result = serve_agent(reader, std::io::stdout(), |peer| {
        CliAcpAgent::new(registry, default_model, system_role.to_owned(), peer)
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: rpc: {e}");
            ExitCode::from(1)
        }
    }
}

/// Maps between the session ids a client uses and engine session ids. A
/// client may load a session by an id prefix, so the two can differ.
#[derive(Default)]
struct Ids {
    by_client: HashMap<String, SessionId>,
    by_engine: HashMap<SessionId, String>,
}

impl Ids {
    fn insert(&mut self, client: String, engine: SessionId) {
        self.by_engine.insert(engine, client.clone());
        self.by_client.insert(client, engine);
    }
}

/// How a prompt's run ended, handed from the bridge to the waiting prompt.
struct PromptEnd {
    outcome: RunOutcome,
    stop: Option<CoreStopReason>,
}

type Waiters = Arc<Mutex<HashMap<SessionId, mpsc::Sender<PromptEnd>>>>;

/// The ACP agent `kage rpc` exposes.
struct CliAcpAgent {
    engine: Engine,
    registry: Arc<ProviderRegistry>,
    default_model: String,
    system_role: String,
    ids: Arc<Mutex<Ids>>,
    waiters: Waiters,
}

impl CliAcpAgent {
    fn new(
        registry: Arc<ProviderRegistry>,
        default_model: String,
        system_role: String,
        peer: Peer,
    ) -> Self {
        let engine = Engine::start(Arc::clone(&registry));
        let ids = Arc::new(Mutex::new(Ids::default()));
        let waiters = Waiters::default();
        let mut bridge = Bridge {
            peer,
            commander: engine.commander(),
            ids: Arc::clone(&ids),
            waiters: Arc::clone(&waiters),
            seen: HashMap::new(),
            stops: HashMap::new(),
            asks: HashMap::new(),
        };
        engine.subscribe(Box::new(move |envelope| bridge.handle(envelope)));
        Self {
            engine,
            registry,
            default_model,
            system_role,
            ids,
            waiters,
        }
    }

    fn engine_id(&self, client_id: &str) -> Result<SessionId, RpcError> {
        lock(&self.ids)
            .by_client
            .get(client_id)
            .copied()
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {client_id}")))
    }

    /// Everything an engine session for `cwd` runs with. The caller fills
    /// in the history and recorder.
    fn session_spec(&self, id: SessionId, cwd: &str) -> Result<SessionSpec, RpcError> {
        let workdir = if cwd.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        } else {
            PathBuf::from(cwd)
        };
        crate::trust::warn_if_untrusted(&workdir);
        let model = self.default_model.clone();
        let bare = runtime_env::build_system_prompt(&self.system_role, &workdir, &model, &[]);
        let plugins = match crate::plugins_dir() {
            Ok(dir) => {
                crate::plugins::setup_runtime(&dir, &workdir, &model, &bare).unwrap_or_else(|e| {
                    eprintln!("kage: {e}");
                    None
                })
            }
            Err(e) => {
                eprintln!("kage: {e}");
                None
            }
        };
        let skills = crate::load_skills(&workdir, plugins.as_deref());
        let system_prompt =
            runtime_env::build_system_prompt(&self.system_role, &workdir, &model, &skills);
        let mut tools = builtin_registry();
        let (mcp, mcp_errors) =
            crate::mcp::spawn_and_register(&mut tools, &workdir, plugins.as_deref());
        for (server, err) in mcp_errors {
            eprintln!("kage: mcp `{server}`: {err}");
        }
        let config = kage_core::config::Config::load_layered(&workdir).unwrap_or_else(|e| {
            eprintln!("kage: rpc: {e}; using defaults");
            kage_core::config::Config::default()
        });
        config
            .permissions
            .validate()
            .map_err(|e| RpcError::internal(format!("permissions: {e}")))?;
        let mut cx = AgentContext::new(model.clone(), &system_prompt).with_workdir(&workdir);
        if config.permissions.confine_paths {
            cx = cx.with_confine_paths();
        }
        if let Some(window) = runtime_env::context_window_for(&self.registry, &model) {
            cx = cx.with_context_window(window);
        }
        Ok(SessionSpec {
            id,
            model,
            cx,
            recorder: None,
            tools,
            gate: PermissionGate::new(config.permissions)
                .with_fallback(PermissionAction::Ask)
                .with_mcp_servers(mcp.server_names().map(str::to_owned).collect()),
            loop_cfg: LoopConfig {
                compaction_threshold: config.loop_settings.compaction_threshold,
                parallel_tools: false,
                ..LoopConfig::default()
            },
            plugins,
            mcp: Some(mcp),
            interactive: true,
            title: true,
        })
    }
}

impl Agent for CliAcpAgent {
    fn initialize(&self, _req: InitializeRequest) -> InitializeResponse {
        InitializeResponse {
            protocol_version: PROTOCOL_VERSION,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                prompt_capabilities: PromptCapabilities::default(),
            },
            agent_info: Some(Implementation {
                name: "kage".to_owned(),
                title: None,
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            auth_methods: vec![],
        }
    }

    fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
        let (path, mut header) =
            crate::plan_session(&self.default_model, "").map_err(RpcError::internal)?;
        let id = header.session;
        let mut spec = self.session_spec(id, &req.cwd)?;
        header.cwd.clone_from(&spec.cx.workdir);
        header.system_prompt.clone_from(&spec.cx.system_prompt);
        spec.recorder = Some(Recorder::planned(path, header, spec.plugins.clone()));
        lock(&self.ids).insert(id.to_string(), id);
        self.engine.open(spec);
        Ok(NewSessionResponse {
            session_id: id.to_string(),
        })
    }

    fn load_session(&self, req: LoadSessionRequest, ctx: &PromptContext) -> Result<(), RpcError> {
        let dir = crate::sessions_dir().map_err(RpcError::internal)?;
        let path = kage_session::find_by_prefix(&dir, &req.session_id)
            .map_err(|e| RpcError::internal(e.to_string()))?
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {}", req.session_id)))?;
        let id = crate::engine::session_id_of(&path).ok_or_else(|| {
            RpcError::internal(format!("bad session file name {}", path.display()))
        })?;
        let replay = kage_session::replay(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        for message in &replay.history {
            let text = crate::cli_loop_run::first_user_text(message);
            if text.is_empty() {
                continue;
            }
            let chunk = MessageChunk {
                content: ContentBlock::text(text),
            };
            match message.role {
                Role::User => ctx.update(SessionUpdate::UserMessageChunk(chunk)),
                Role::Assistant => ctx.update(SessionUpdate::AgentMessageChunk(chunk)),
                _ => {}
            }
        }
        if lock(&self.ids).by_engine.contains_key(&id) {
            lock(&self.ids).insert(req.session_id, id);
            return Ok(());
        }
        let writer = SessionWriter::open(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        let mut spec = self.session_spec(id, &req.cwd)?;
        spec.cx.history = replay.history;
        spec.recorder = Some(Recorder::new(writer, spec.plugins.clone()));
        lock(&self.ids).insert(req.session_id, id);
        self.engine.open(spec);
        Ok(())
    }

    fn prompt(&self, req: PromptRequest, _ctx: &PromptContext) -> Result<PromptResponse, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let text = req
            .prompt
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("\n");
        let (done, end) = mpsc::channel();
        lock(&self.waiters).insert(id, done);
        self.engine.send(Command::to(
            id,
            CommandKind::Prompt {
                content: vec![Content::Text { text }],
                delivery: Delivery::Queue,
            },
        ));
        let end = end
            .recv()
            .map_err(|_| RpcError::internal("engine stopped"))?;
        match end.outcome {
            RunOutcome::Completed => Ok(PromptResponse {
                stop_reason: stop_reason(end.stop),
            }),
            RunOutcome::Cancelled => Ok(PromptResponse {
                stop_reason: StopReason::Cancelled,
            }),
            RunOutcome::Failed { error } => Err(RpcError::internal(error.to_string())),
        }
    }

    fn cancel(&self, session_id: &str) {
        if let Ok(id) = self.engine_id(session_id) {
            self.engine.send(Command::to(id, CommandKind::Cancel));
        }
    }
}

/// Turns engine events into ACP traffic for the sessions a client opened.
struct Bridge {
    peer: Peer,
    commander: Commander,
    ids: Arc<Mutex<Ids>>,
    waiters: Waiters,
    seen: HashMap<SessionId, HashSet<String>>,
    stops: HashMap<SessionId, CoreStopReason>,
    asks: HashMap<SessionId, Vec<Arc<AtomicBool>>>,
}

impl Bridge {
    fn handle(&mut self, envelope: &Envelope) {
        let session = envelope.session;
        let Some(client_id) = lock(&self.ids).by_engine.get(&session).cloned() else {
            return;
        };
        match &envelope.event {
            Event::Loop(event) => {
                if let LoopEvent::MessageEnd { stop_reason, .. } = event {
                    self.stops.insert(session, *stop_reason);
                }
                let seen = self.seen.entry(session).or_default();
                if let Some(update) = to_update(seen, event) {
                    send_update(&self.peer, &client_id, update);
                }
            }
            Event::Host(HostEvent::PermissionRequested {
                request_id,
                tool_call_id,
                tool,
                input,
                ..
            }) => {
                let answered = Arc::new(AtomicBool::new(false));
                self.asks
                    .entry(session)
                    .or_default()
                    .push(Arc::clone(&answered));
                let tool_call = ToolCallUpdate {
                    tool_call_id: tool_call_id
                        .as_ref()
                        .map_or_else(String::new, ToString::to_string),
                    title: Some(tool.clone()),
                    kind: Some(tool_kind(tool)),
                    status: Some(ToolCallStatus::Pending),
                    raw_input: Some(input.clone()),
                    ..ToolCallUpdate::default()
                };
                let peer = self.peer.clone();
                let commander = self.commander.clone();
                let request_id = *request_id;
                let tool = tool.clone();
                std::thread::spawn(move || {
                    let decision = kage_acp::agent::request_permission(
                        &peer,
                        &client_id,
                        tool_call,
                        &tool,
                        &|| answered.load(Ordering::SeqCst),
                    );
                    let decision = match decision {
                        PermissionDecision::Allow => Decision::AllowOnce,
                        PermissionDecision::Deny(_) => Decision::Deny,
                    };
                    commander.send(Command::to(
                        session,
                        CommandKind::ResolvePermission {
                            request_id,
                            decision,
                        },
                    ));
                });
            }
            Event::Host(HostEvent::RunEnded { outcome }) => {
                for answered in self.asks.remove(&session).unwrap_or_default() {
                    answered.store(true, Ordering::SeqCst);
                }
                self.seen.remove(&session);
                let stop = self.stops.remove(&session);
                if let Some(waiter) = lock(&self.waiters).remove(&session) {
                    let _ = waiter.send(PromptEnd {
                        outcome: outcome.clone(),
                        stop,
                    });
                }
            }
            Event::Host(_) => {}
        }
    }
}

/// Translate a loop event into the matching ACP `session/update`. The
/// first sighting of a tool call id sends `tool_call`; everything after
/// it for that id is a `tool_call_update`.
fn to_update(seen: &mut HashSet<String>, event: &LoopEvent) -> Option<SessionUpdate> {
    match event {
        LoopEvent::TextDelta { delta, .. } => {
            Some(SessionUpdate::AgentMessageChunk(MessageChunk {
                content: ContentBlock::text(delta.clone()),
            }))
        }
        LoopEvent::ThinkingDelta { delta, .. } => {
            Some(SessionUpdate::AgentThoughtChunk(MessageChunk {
                content: ContentBlock::text(delta.clone()),
            }))
        }
        LoopEvent::ToolCallArgsDelta {
            id,
            name,
            input_partial,
        }
        | LoopEvent::ToolCallStart {
            id,
            name,
            input_partial,
        } => {
            if seen.insert(id.to_string()) {
                Some(SessionUpdate::ToolCall(ToolCall {
                    tool_call_id: id.to_string(),
                    title: name.clone(),
                    kind: tool_kind(name),
                    status: ToolCallStatus::Pending,
                    content: Vec::new(),
                    raw_input: Some(input_partial.clone()),
                }))
            } else {
                Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                    tool_call_id: id.to_string(),
                    status: Some(ToolCallStatus::Pending),
                    raw_input: Some(input_partial.clone()),
                    ..ToolCallUpdate::default()
                }))
            }
        }
        LoopEvent::ToolExecutionStart { id } => {
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: id.to_string(),
                status: Some(ToolCallStatus::InProgress),
                ..ToolCallUpdate::default()
            }))
        }
        LoopEvent::ToolUpdate { id, update } => {
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: id.to_string(),
                content: vec![text_content(update.content.clone())],
                ..ToolCallUpdate::default()
            }))
        }
        LoopEvent::ToolCallEnd { id, output } => {
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: id.to_string(),
                status: Some(if output.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                }),
                content: vec![text_content(output.text.clone())],
                raw_output: output.structured.clone(),
                ..ToolCallUpdate::default()
            }))
        }
        _ => None,
    }
}

fn text_content(text: String) -> ToolCallContent {
    ToolCallContent::Content(MessageChunk {
        content: ContentBlock::text(text),
    })
}

/// ACP kind hint for a built-in tool name.
fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" | "ls" => ToolKind::Read,
        "grep" | "find" => ToolKind::Search,
        "write" | "edit" => ToolKind::Edit,
        "bash" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// A completed turn that hit the output-token cap surfaces as `MaxTokens`
/// so editors can warn the reply was cut off; every other ending is an
/// ordinary turn.
fn stop_reason(last: Option<CoreStopReason>) -> StopReason {
    match last {
        Some(CoreStopReason::MaxTokens) => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use kage_core::{MessageId, ToolCallId, ToolOutput, ToolUpdate};

    use super::*;

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
    fn a_tool_call_is_announced_once_then_updated() {
        let id = ToolCallId::new("call_1");
        let events = [
            LoopEvent::ToolCallArgsDelta {
                id: id.clone(),
                name: "bash".into(),
                input_partial: serde_json::json!({}),
            },
            LoopEvent::ToolCallStart {
                id: id.clone(),
                name: "bash".into(),
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
        let mut seen = HashSet::new();
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
                Some(ToolCallStatus::Pending),
                Some(ToolCallStatus::InProgress),
                None,
                Some(ToolCallStatus::Completed)
            ]
        );
    }

    #[test]
    fn text_maps_to_agent_message_chunks() {
        let update = to_update(
            &mut HashSet::new(),
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
}
