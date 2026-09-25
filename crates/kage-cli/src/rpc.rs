//! `kage rpc`: a spec-conformant Agent Client Protocol agent.
//!
//! Speaks ACP (newline-delimited JSON-RPC 2.0 over stdio, protocol
//! version 1) so editors that speak ACP can drive kage. Every ACP session
//! is an engine session: prompts become engine commands, and a bus
//! subscriber turns engine events into `session/update` notifications and
//! `session/request_permission` requests. Recorded sessions can be
//! listed, loaded with a replay of their transcript, or resumed.
//!
//! Agent sessions started by the `agent` tool are not ACP sessions. Their
//! permission requests and progress go to the client session at the root
//! of their tree, on that session's top-level `agent` call.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use kage_acp::acp::{
    AgentCapabilities, BlobContent, ContentBlock, Cost, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, MessageChunk, NewSessionRequest, NewSessionResponse, PROTOCOL_VERSION,
    PromptCapabilities, PromptRequest, PromptResponse, ResourceLink, ResumeSessionRequest,
    ResumeSessionResponse, SessionCapabilities, SessionInfo, SessionInfoUpdate, SessionUpdate,
    StopReason, Supported, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolKind,
    UsageUpdate,
};
use kage_acp::agent::{Agent, PermissionDecision, PromptContext, send_update, serve_agent};
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{
    AgentNode, AgentTree, Command, CommandKind, Delivery, Envelope, Event, HostEvent,
    PermissionDecision as Decision, RequestId, RunOutcome, Usage,
};
use kage_core::sync::lock;
use kage_core::{
    Content, ImageSource, LoopEvent, Message, MessageId, Role, SessionId,
    StopReason as CoreStopReason, ToolOutput,
};
use kage_jsonrpc::{Peer, RpcError};
use kage_loop::{AgentContext, LoopConfig, TokenBudget};
use kage_provider::ProviderRegistry;
use kage_session::SessionWriter;
use kage_tools::builtin_registry;

use crate::engine::{AgentSetup, Commander, Engine, Recorder, SessionSpec};
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
    let sessions = match crate::sessions_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("kage: rpc: {e}");
            return ExitCode::from(1);
        }
    };
    let registry = Arc::new(registry);
    let spec = {
        let registry = Arc::clone(&registry);
        let system_role = system_role.to_owned();
        Box::new(move |id, cwd: &str, model: &str| {
            session_spec(&registry, &system_role, id, cwd, model)
        })
    };
    let reader = BufReader::new(std::io::stdin());
    let result = serve_agent(reader, std::io::stdout(), |peer| {
        CliAcpAgent::new(registry, default_model, sessions, spec, peer)
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

/// Builds the engine session for a client session from its id, working
/// directory and model. The caller fills in the history and recorder.
type SpecBuilder =
    Box<dyn Fn(SessionId, &str, &str) -> Result<SessionSpec, RpcError> + Send + Sync>;

/// Sessions per `session/list` page.
const LIST_PAGE: usize = 50;

/// The ACP agent `kage rpc` exposes.
struct CliAcpAgent {
    engine: Engine,
    registry: Arc<ProviderRegistry>,
    default_model: String,
    sessions: PathBuf,
    spec: SpecBuilder,
    ids: Arc<Mutex<Ids>>,
    waiters: Waiters,
}

impl CliAcpAgent {
    fn new(
        registry: Arc<ProviderRegistry>,
        default_model: String,
        sessions: PathBuf,
        spec: SpecBuilder,
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
            tree: AgentTree::default(),
        };
        engine.subscribe(Box::new(move |envelope| bridge.handle(envelope)));
        Self {
            engine,
            registry,
            default_model,
            sessions,
            spec,
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

    /// Opens the recorded session `client_id` names the way the TUI resumes
    /// one: on its recorded model when that resolves, with its thinking
    /// level and token totals. With `ctx`, first replays its history and
    /// title to the client.
    fn open_recorded(
        &self,
        client_id: &str,
        cwd: &str,
        ctx: Option<&PromptContext>,
    ) -> Result<(), RpcError> {
        let path = kage_session::find_by_prefix(&self.sessions, client_id)
            .map_err(|e| RpcError::internal(e.to_string()))?
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {client_id}")))?;
        let id = crate::engine::session_id_of(&path).ok_or_else(|| {
            RpcError::internal(format!("bad session file name {}", path.display()))
        })?;
        let replay = kage_session::replay(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        if let Some(ctx) = ctx {
            for update in replay_updates(&replay.history) {
                ctx.update(update);
            }
            if let Some(title) = replay.title {
                ctx.update(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                    title: Some(title),
                    updated_at: None,
                }));
            }
        }
        if lock(&self.ids).by_engine.contains_key(&id) {
            lock(&self.ids).insert(client_id.to_owned(), id);
            return Ok(());
        }
        let writer = SessionWriter::open(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        let model = if self.registry.resolve(&replay.model).is_ok() {
            replay.model
        } else {
            eprintln!(
                "kage: rpc: session model {} unavailable; using {} instead",
                replay.model, self.default_model
            );
            self.default_model.clone()
        };
        let mut spec = (self.spec)(id, cwd, &model)?;
        spec.cx.history = replay.history;
        spec.cx.budget = TokenBudget {
            used_input: replay.usage_total.input,
            used_output: replay.usage_total.output,
            used_cache_read: replay.usage_total.cache_read,
            used_cache_write: replay.usage_total.cache_write,
            current_context: replay.usage_total.last_context,
        };
        spec.cx.thinking_level = replay
            .thinking_level
            .as_deref()
            .and_then(kage_core::ThinkingLevel::parse);
        spec.recorder = Some(Recorder::new(writer, spec.plugins.clone()));
        lock(&self.ids).insert(client_id.to_owned(), id);
        self.engine.open(spec);
        Ok(())
    }
}

/// Everything an engine session for `cwd` on `model` runs with. The caller
/// fills in the history and recorder.
fn session_spec(
    registry: &ProviderRegistry,
    system_role: &str,
    id: SessionId,
    cwd: &str,
    model: &str,
) -> Result<SessionSpec, RpcError> {
    let workdir = if cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(cwd)
    };
    crate::trust::warn_if_untrusted(&workdir);
    let model = model.to_owned();
    let bare = runtime_env::build_system_prompt(system_role, &workdir, &model, &[]);
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
    let system_prompt = runtime_env::build_system_prompt(system_role, &workdir, &model, &skills);
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
    let (defs, agent_errors) = crate::agents::load(&workdir);
    for err in agent_errors {
        eprintln!("kage: {err}");
    }
    let agents = AgentSetup::from_config(defs, &config);
    config
        .permissions
        .validate()
        .map_err(|e| RpcError::internal(format!("permissions: {e}")))?;
    let mut cx = AgentContext::new(model.clone(), &system_prompt).with_workdir(&workdir);
    if config.permissions.confine_paths {
        cx = cx.with_confine_paths();
    }
    if let Some(window) = runtime_env::context_window_for(registry, &model) {
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
        agents: Some(agents),
    })
}

impl Agent for CliAcpAgent {
    fn initialize(&self, _req: InitializeRequest) -> InitializeResponse {
        InitializeResponse {
            protocol_version: PROTOCOL_VERSION,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                prompt_capabilities: PromptCapabilities::default(),
                session_capabilities: SessionCapabilities {
                    list: Some(Supported {}),
                    resume: Some(Supported {}),
                },
                ..AgentCapabilities::default()
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
        let mut spec = (self.spec)(id, &req.cwd, &self.default_model)?;
        header.cwd.clone_from(&spec.cx.workdir);
        header.system_prompt.clone_from(&spec.cx.system_prompt);
        spec.recorder = Some(Recorder::planned(path, header, spec.plugins.clone()));
        lock(&self.ids).insert(id.to_string(), id);
        self.engine.open(spec);
        Ok(NewSessionResponse {
            session_id: id.to_string(),
            config_options: vec![],
        })
    }

    fn load_session(
        &self,
        req: LoadSessionRequest,
        ctx: &PromptContext,
    ) -> Result<LoadSessionResponse, RpcError> {
        self.open_recorded(&req.session_id, &req.cwd, Some(ctx))?;
        Ok(LoadSessionResponse::default())
    }

    fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, RpcError> {
        list_page(&self.sessions, &req)
    }

    fn resume_session(&self, req: ResumeSessionRequest) -> Result<ResumeSessionResponse, RpcError> {
        self.open_recorded(&req.session_id, &req.cwd, None)?;
        Ok(ResumeSessionResponse::default())
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

/// Turns engine events into ACP traffic for the sessions a client opened
/// and the agents under them.
struct Bridge {
    peer: Peer,
    commander: Commander,
    ids: Arc<Mutex<Ids>>,
    waiters: Waiters,
    seen: HashMap<SessionId, HashSet<String>>,
    stops: HashMap<SessionId, CoreStopReason>,
    asks: HashMap<SessionId, Vec<Arc<AtomicBool>>>,
    tree: AgentTree,
}

impl Bridge {
    fn handle(&mut self, envelope: &Envelope) {
        let session = envelope.session;
        let is_agent = self.tree.apply(envelope);
        let client_id = lock(&self.ids).by_engine.get(&session).cloned();
        match client_id {
            Some(client_id) => self.handle_client(session, client_id, &envelope.event),
            None if is_agent => self.handle_agent(session, &envelope.event),
            None => {}
        }
    }

    fn handle_client(&mut self, session: SessionId, client_id: String, event: &Event) {
        match event {
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
                self.ask(session, *request_id, client_id, tool_call);
            }
            Event::Host(HostEvent::UsageUpdated { usage }) => {
                if let Some(update) = usage_update(usage) {
                    send_update(&self.peer, &client_id, update);
                }
            }
            Event::Host(HostEvent::TitleChanged { title }) => {
                let update = SessionInfoUpdate {
                    title: Some(title.clone()),
                    updated_at: None,
                };
                send_update(
                    &self.peer,
                    &client_id,
                    SessionUpdate::SessionInfoUpdate(update),
                );
            }
            Event::Host(HostEvent::RunEnded { outcome }) => {
                self.end_asks(session);
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

    /// Shows an agent's activity as the content of the root session's
    /// top-level `agent` call, and forwards its permission requests to
    /// that session.
    fn handle_agent(&mut self, session: SessionId, event: &Event) {
        let Some(top) = top_agent(&self.tree, session) else {
            return;
        };
        let Some(client_id) = lock(&self.ids).by_engine.get(&top.parent).cloned() else {
            return;
        };
        let call_id = top.tool_call_id.to_string();
        let agent = self
            .tree
            .get(session)
            .map_or_else(String::new, |node| node.agent.clone());
        let progress = |text: String| {
            let update = ToolCallUpdate {
                tool_call_id: call_id.clone(),
                content: vec![text_content(format!("{agent}: {text}"))],
                ..ToolCallUpdate::default()
            };
            send_update(
                &self.peer,
                &client_id,
                SessionUpdate::ToolCallUpdate(update),
            );
        };
        match event {
            Event::Loop(LoopEvent::ToolCallStart {
                name,
                input_partial,
                ..
            }) => progress(describe_call(name, input_partial)),
            Event::Host(HostEvent::PermissionRequested {
                request_id,
                tool,
                input,
                ..
            }) => {
                progress(format!(
                    "Waiting for approval: {}",
                    describe_call(tool, input)
                ));
                let tool_call = ToolCallUpdate {
                    tool_call_id: call_id.clone(),
                    title: Some(format!("{agent}: {tool}")),
                    kind: Some(tool_kind(tool)),
                    raw_input: Some(input.clone()),
                    ..ToolCallUpdate::default()
                };
                self.ask(session, *request_id, client_id, tool_call);
            }
            Event::Host(HostEvent::RunEnded { outcome }) => {
                progress(
                    match outcome {
                        RunOutcome::Completed => "done",
                        RunOutcome::Cancelled => "stopped",
                        RunOutcome::Failed { .. } => "failed",
                    }
                    .to_owned(),
                );
                self.end_asks(session);
            }
            _ => {}
        }
    }

    /// Asks the client on `client_id` and resolves `request_id` of
    /// `session` with the answer. The ask is dropped when that session's
    /// run ends first.
    fn ask(
        &mut self,
        session: SessionId,
        request_id: RequestId,
        client_id: String,
        tool_call: ToolCallUpdate,
    ) {
        let answered = Arc::new(AtomicBool::new(false));
        self.asks
            .entry(session)
            .or_default()
            .push(Arc::clone(&answered));
        let peer = self.peer.clone();
        let commander = self.commander.clone();
        std::thread::spawn(move || {
            let title = tool_call.title.clone().unwrap_or_default();
            let decision =
                kage_acp::agent::request_permission(&peer, &client_id, tool_call, &title, &|| {
                    answered.load(Ordering::SeqCst)
                });
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

    fn end_asks(&mut self, session: SessionId) {
        for answered in self.asks.remove(&session).unwrap_or_default() {
            answered.store(true, Ordering::SeqCst);
        }
    }
}

/// The agent that a client session's own `agent` call started, at the top
/// of `session`'s branch.
fn top_agent(tree: &AgentTree, session: SessionId) -> Option<&AgentNode> {
    let mut node = tree.get(session)?;
    while let Some(parent) = tree.get(node.parent) {
        node = parent;
    }
    Some(node)
}

/// One line naming what a tool call does, such as `Read src/lib.rs`.
fn describe_call(name: &str, input: &serde_json::Value) -> String {
    let label = kage_tui::view::tool_view::describe(name, input);
    if label.target.is_empty() {
        label.verb.to_owned()
    } else {
        format!("{} {}", label.verb, label.target)
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

/// The `session/update`s that show `history` as the live bridge showed
/// it: user chunks, then each assistant block and tool result mapped
/// through [`to_update`].
fn replay_updates(history: &[Message]) -> Vec<SessionUpdate> {
    let mut seen = HashSet::new();
    let mut updates = Vec::new();
    for message in history {
        for block in &message.content {
            let update = match (message.role, block) {
                (_, Content::Text { text } | Content::Thinking { text }) if text.is_empty() => None,
                (Role::User, Content::Text { text }) => Some(user_chunk(ContentBlock::text(text))),
                (Role::User, Content::Image { source, mime }) => {
                    Some(user_chunk(image_block(source, mime)))
                }
                (_, block) => {
                    replay_event(message.id, block).and_then(|e| to_update(&mut seen, &e))
                }
            };
            updates.extend(update);
        }
    }
    updates
}

/// The loop event that streamed `block` of message `id`, if it shows.
fn replay_event(id: MessageId, block: &Content) -> Option<LoopEvent> {
    match block {
        Content::Text { text } => Some(LoopEvent::TextDelta {
            id,
            delta: text.clone(),
        }),
        Content::Thinking { text } => Some(LoopEvent::ThinkingDelta {
            id,
            delta: text.clone(),
        }),
        Content::ToolCall { id, name, input } => Some(LoopEvent::ToolCallStart {
            id: id.clone(),
            name: name.clone(),
            input_partial: input.clone(),
        }),
        Content::ToolResultBlock {
            call_id,
            output,
            is_error,
        } => Some(LoopEvent::ToolCallEnd {
            id: call_id.clone(),
            output: ToolOutput {
                is_error: *is_error,
                text: output.clone(),
                ..ToolOutput::default()
            },
        }),
        Content::Image { .. } | Content::Custom { .. } => None,
    }
}

fn user_chunk(content: ContentBlock) -> SessionUpdate {
    SessionUpdate::UserMessageChunk(MessageChunk { content })
}

fn image_block(source: &ImageSource, mime: &str) -> ContentBlock {
    match source {
        ImageSource::Base64 { data } => ContentBlock::Image(BlobContent {
            data: data.clone(),
            mime_type: mime.to_owned(),
            uri: None,
        }),
        ImageSource::Url { url } => ContentBlock::ResourceLink(ResourceLink {
            uri: url.clone(),
            name: url.clone(),
            mime_type: Some(mime.to_owned()),
        }),
    }
}

/// The `usage_update` for `usage`, or `None` while the context window is
/// unknown. Cost is left out until the model has pricing.
fn usage_update(usage: &Usage) -> Option<SessionUpdate> {
    if usage.context_window == 0 {
        return None;
    }
    let cost = (usage.cost > 0.0).then(|| Cost {
        amount: usage.cost,
        currency: "USD".to_owned(),
    });
    Some(SessionUpdate::UsageUpdate(UsageUpdate {
        used: usage.context_used,
        size: usage.context_window,
        cost,
    }))
}

/// One `session/list` page of the client sessions recorded in `dir`,
/// newest activity first. The cursor is the offset of the page.
fn list_page(dir: &Path, req: &ListSessionsRequest) -> Result<ListSessionsResponse, RpcError> {
    let start = match &req.cursor {
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| RpcError::new(-32602, format!("invalid cursor {cursor}")))?,
        None => 0,
    };
    let cwd = req.cwd.as_deref().map(Path::new);
    let mut summaries: Vec<_> = kage_session::list(dir)
        .map_err(|e| RpcError::internal(e.to_string()))?
        .into_iter()
        .filter(|s| s.agent.is_none() && cwd.is_none_or(|cwd| s.cwd == cwd))
        .collect();
    summaries.sort_by_key(|s| Reverse(s.updated_at));
    let end = start.saturating_add(LIST_PAGE);
    let next_cursor = (end < summaries.len()).then(|| end.to_string());
    let sessions = summaries
        .into_iter()
        .skip(start)
        .take(LIST_PAGE)
        .map(|s| SessionInfo {
            session_id: s.id.to_string(),
            cwd: s.cwd.display().to_string(),
            title: s.title,
            updated_at: Some(
                s.updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
        })
        .collect();
    Ok(ListSessionsResponse {
        sessions,
        next_cursor,
    })
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
    use std::time::Duration;

    use kage_core::agents::AgentDefs;
    use kage_core::permissions::{PermissionsConfig, ToolPermissionRules};
    use kage_core::{MessageId, TokenUsage, ToolCallId, ToolOutput, ToolUpdate};
    use kage_jsonrpc::Inbound;
    use kage_provider::testing::MockProvider;
    use kage_provider::{ProviderError, ProviderEvent};
    use kage_session::{EntryId, FORMAT_VERSION, Header, MessageEntry, SessionEntry};

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
        session: String,
    }

    /// Serves `kage rpc` over pipes on the sessions recorded in
    /// `sessions`, with one open session. Every session runs in
    /// `workdir`, asks before `ls` calls and generates a title.
    fn serve(scripts: Vec<Script>, workdir: &Path, sessions: &Path) -> Harness {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let id = SessionId::new();
        let workdir = workdir.to_path_buf();
        let sessions = sessions.to_path_buf();
        let mock = MockProvider::sequence(scripts);
        let provider = mock.clone();
        std::thread::spawn(move || {
            serve_agent(BufReader::new(srv_r), srv_w, |peer| {
                let registry = Arc::new(ProviderRegistry::new().with(Arc::new(provider)));
                let spec = Box::new(move |id, _cwd: &str, model: &str| {
                    let mut rules = PermissionsConfig::default();
                    rules.tools.insert(
                        "ls".into(),
                        ToolPermissionRules {
                            default: PermissionAction::Ask,
                            allow: Vec::new(),
                            deny: Vec::new(),
                        },
                    );
                    Ok(SessionSpec {
                        id,
                        model: model.to_owned(),
                        cx: AgentContext::new(model, "")
                            .with_workdir(&workdir)
                            .with_context_window(WINDOW),
                        recorder: None,
                        tools: builtin_registry(),
                        gate: PermissionGate::new(rules),
                        loop_cfg: LoopConfig::default(),
                        plugins: None,
                        mcp: None,
                        interactive: true,
                        title: true,
                        agents: Some(AgentSetup {
                            defs: Arc::new(AgentDefs::builtin()),
                            max_depth: 1,
                            max_running: 4,
                        }),
                    })
                });
                let agent = CliAcpAgent::new(registry, "mock:m".into(), sessions, spec, peer);
                agent.engine.open((agent.spec)(id, "", "mock:m").unwrap());
                lock(&agent.ids).insert(id.to_string(), id);
                agent
            })
        });
        let (client, inbox, _reader) = kage_jsonrpc::connect(BufReader::new(cli_r), cli_w);
        Harness {
            client,
            inbox,
            mock,
            session: id.to_string(),
        }
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
            progress[..3],
            [
                format!("general: List {path}"),
                format!("general: Waiting for approval: List {path}"),
                "general: done".to_owned(),
            ]
        );
        assert!(contents_of(&updates, "call_child").is_empty());
        assert!(progress[3].contains("child done"), "{progress:?}");
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
            if let Inbound::Notification { params, .. } =
                inbox.recv_timeout(WAIT).expect("no update")
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
        assert_eq!(loaded, serde_json::json!({}));
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
        assert_eq!(resumed, serde_json::json!({}));
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
