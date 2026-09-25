//! `kage rpc`: a spec-conformant Agent Client Protocol agent.
//!
//! Speaks ACP (newline-delimited JSON-RPC 2.0 over stdio, protocol
//! version 1) so editors that speak ACP can drive kage. Every ACP session
//! is an engine session: prompts become engine commands, and a bus
//! subscriber turns engine events into `session/update` notifications and
//! `session/request_permission` requests. Recorded sessions can be
//! listed, loaded with a replay of their transcript, or resumed. Each
//! session offers its model, thinking level and permission mode as config
//! options, and the prompts of its live MCP servers as slash commands
//! named `<server>:<prompt>`, which the engine expands when they come back
//! as prompt text.
//!
//! Agent sessions started by the `agent` tool are shown as subagent
//! sessions (draft RFD PR #1992) to a client that advertises the
//! `subagents` capability: announced with `subagent_update` on their
//! parent's session, streaming on their own session, and asking there.
//! For every other client they are not ACP sessions. Their permission
//! requests and progress go to the client session at the root of their
//! tree, on that session's top-level `agent` call.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use kage_acp::acp::{
    AgentCapabilities, AvailableCommandsUpdate, BlobContent, ConfigOptionUpdate, ContentBlock,
    Cost, Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, MessageChunk, NewSessionRequest,
    NewSessionResponse, PROTOCOL_VERSION, PromptCapabilities, PromptRequest, PromptResponse,
    ResourceLink, ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities,
    SessionConfigCategory, SessionConfigKind, SessionConfigOption, SessionConfigSelectOption,
    SessionInfo, SessionInfoUpdate, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, SubagentSessionCapabilities, SubagentState,
    SubagentUpdate, Supported, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolKind,
    UsageUpdate,
};
use kage_acp::agent::{Agent, PermissionDecision, PromptContext, send_update, serve_agent};
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{
    AgentNode, AgentTree, Command, CommandKind, Delivery, Envelope, Event, HostEvent,
    McpServerInfo, McpServerStatus, PermissionDecision as Decision, RequestId, RunOutcome,
    SessionState, Usage,
};
use kage_core::sync::lock;
use kage_core::{
    Content, ImageSource, LoopEvent, Message, MessageId, Role, SessionId,
    StopReason as CoreStopReason, ThinkingLevel, ToolCallId, ToolOutput,
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
    /// Running subagents, which the client may only cancel.
    subagents: HashMap<String, SessionId>,
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

type ShownBySession = Arc<Mutex<HashMap<SessionId, Shown>>>;

/// What a client session's config options last showed.
struct Shown {
    settings: Settings,
    /// Set while the engine has yet to apply a change the client made, so
    /// the older states it still reports are not sent back to the client.
    catching_up: bool,
}

impl Shown {
    /// Takes in a state the engine reported, and says whether the client
    /// has to hear of it.
    fn observe(&mut self, settings: &Settings) -> bool {
        if self.catching_up {
            self.catching_up = *settings != self.settings;
            return false;
        }
        if *settings == self.settings {
            return false;
        }
        self.settings = settings.clone();
        true
    }
}

/// The session settings a client sees and changes as config options.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Settings {
    model: String,
    thinking: ThinkingLevel,
    mode: Option<PermissionAction>,
}

impl Settings {
    fn of(spec: &SessionSpec) -> Self {
        Self {
            model: spec.model.clone(),
            thinking: spec.cx.thinking_level.unwrap_or_default(),
            mode: spec.gate.mode(),
        }
    }

    /// Sets option `id` to `value` and returns the engine command that
    /// makes the same change. `models` are the models the client may pick.
    fn apply(
        &mut self,
        models: &[SessionConfigSelectOption],
        id: &str,
        value: &str,
    ) -> Result<CommandKind, RpcError> {
        let invalid = || RpcError::new(-32602, format!("invalid value {value} for option {id}"));
        match id {
            "model" => {
                if value != self.model && !models.iter().any(|m| m.value == value) {
                    return Err(invalid());
                }
                value.clone_into(&mut self.model);
                Ok(CommandKind::SetModel {
                    model: value.to_owned(),
                })
            }
            "thinking" => {
                let level = ThinkingLevel::parse(value).ok_or_else(invalid)?;
                self.thinking = level;
                Ok(CommandKind::SetThinking { level })
            }
            "mode" => {
                let (_, mode, ..) = MODES.iter().find(|m| m.0 == value).ok_or_else(invalid)?;
                self.mode = *mode;
                Ok(CommandKind::SetPermissionMode { mode: *mode })
            }
            _ => Err(RpcError::new(-32602, format!("unknown config option {id}"))),
        }
    }
}

impl From<&SessionState> for Settings {
    fn from(state: &SessionState) -> Self {
        Self {
            model: state.model.clone(),
            thinking: state.thinking,
            mode: state.permission_mode,
        }
    }
}

/// The values of the `mode` option, named as `/permission` names them,
/// with the override each applies, a label and a description.
const MODES: [(&str, Option<PermissionAction>, &str, &str); 4] = [
    (
        "default",
        None,
        "Default",
        "The configured permission rules decide",
    ),
    (
        "ask",
        Some(PermissionAction::Ask),
        "Ask",
        "Ask before every tool call",
    ),
    (
        "allow",
        Some(PermissionAction::Allow),
        "Allow",
        "Run every tool call without asking",
    ),
    (
        "deny",
        Some(PermissionAction::Deny),
        "Deny",
        "Refuse every tool call",
    ),
];

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
    models: Arc<[SessionConfigSelectOption]>,
    shown: ShownBySession,
    subagents: Arc<AtomicBool>,
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
        let models: Arc<[SessionConfigSelectOption]> =
            crate::tui::available_model_items(&registry, "")
                .into_iter()
                .map(|item| choice(&item.value, &item.label, item.group.as_deref()))
                .collect();
        let shown = ShownBySession::default();
        let subagents = Arc::new(AtomicBool::new(false));
        let mut bridge = Bridge {
            peer,
            commander: engine.commander(),
            ids: Arc::clone(&ids),
            waiters: Arc::clone(&waiters),
            models: Arc::clone(&models),
            shown: Arc::clone(&shown),
            seen: HashMap::new(),
            stops: HashMap::new(),
            asks: HashMap::new(),
            tree: AgentTree::default(),
            subagents: Arc::clone(&subagents),
            live: HashSet::new(),
            ended: HashMap::new(),
            commands: HashMap::new(),
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
            models,
            shown,
            subagents,
        }
    }

    /// Opens `spec` as the client session `client_id` and returns its
    /// config options.
    fn open(&self, client_id: String, spec: SessionSpec) -> Vec<SessionConfigOption> {
        let settings = Settings::of(&spec);
        let options = config_options(&self.models, &settings);
        let shown = Shown {
            settings,
            catching_up: false,
        };
        lock(&self.shown).insert(spec.id, shown);
        lock(&self.ids).insert(client_id, spec.id);
        self.engine.open(spec);
        options
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
    /// title to the client. Returns the session's config options.
    fn open_recorded(
        &self,
        client_id: &str,
        cwd: &str,
        ctx: Option<&PromptContext>,
    ) -> Result<Vec<SessionConfigOption>, RpcError> {
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
            let shown = lock(&self.shown);
            return Ok(shown
                .get(&id)
                .map(|shown| config_options(&self.models, &shown.settings))
                .unwrap_or_default());
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
        Ok(self.open(client_id.to_owned(), spec))
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
    fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
        self.subagents.store(
            req.client_capabilities.supports_subagents(),
            Ordering::SeqCst,
        );
        InitializeResponse {
            protocol_version: PROTOCOL_VERSION,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                prompt_capabilities: PromptCapabilities {
                    image: true,
                    embedded_context: true,
                    ..PromptCapabilities::default()
                },
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
        let config_options = self.open(id.to_string(), spec);
        Ok(NewSessionResponse {
            session_id: id.to_string(),
            config_options,
        })
    }

    fn load_session(
        &self,
        req: LoadSessionRequest,
        ctx: &PromptContext,
    ) -> Result<LoadSessionResponse, RpcError> {
        let config_options = self.open_recorded(&req.session_id, &req.cwd, Some(ctx))?;
        Ok(LoadSessionResponse { config_options })
    }

    fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, RpcError> {
        list_page(&self.sessions, &req)
    }

    fn resume_session(&self, req: ResumeSessionRequest) -> Result<ResumeSessionResponse, RpcError> {
        let config_options = self.open_recorded(&req.session_id, &req.cwd, None)?;
        Ok(ResumeSessionResponse { config_options })
    }

    fn set_config_option(
        &self,
        req: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let (command, config_options) = {
            let mut shown = lock(&self.shown);
            let shown = shown.get_mut(&id).ok_or_else(|| {
                RpcError::new(-32602, format!("unknown session {}", req.session_id))
            })?;
            let command = shown
                .settings
                .apply(&self.models, &req.config_id, &req.value)?;
            shown.catching_up = true;
            (command, config_options(&self.models, &shown.settings))
        };
        self.engine.send(Command::to(id, command));
        Ok(SetSessionConfigOptionResponse { config_options })
    }

    fn prompt(&self, req: PromptRequest, _ctx: &PromptContext) -> Result<PromptResponse, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let content = req.prompt.into_iter().map(prompt_content).collect();
        let (done, end) = mpsc::channel();
        lock(&self.waiters).insert(id, done);
        self.engine.send(Command::to(
            id,
            CommandKind::Prompt {
                content,
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
        let id = {
            let ids = lock(&self.ids);
            ids.by_client
                .get(session_id)
                .or_else(|| ids.subagents.get(session_id))
                .copied()
        };
        if let Some(id) = id {
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
    models: Arc<[SessionConfigSelectOption]>,
    shown: ShownBySession,
    seen: HashMap<SessionId, HashSet<String>>,
    stops: HashMap<SessionId, CoreStopReason>,
    asks: HashMap<SessionId, Vec<Ask>>,
    tree: AgentTree,
    /// Whether the client advertised the subagents capability.
    subagents: Arc<AtomicBool>,
    /// Announced subagents whose terminal state is not sent yet.
    live: HashSet<SessionId>,
    /// Ended runs held back until their live subagents end.
    ended: HashMap<SessionId, PromptEnd>,
    /// The commands last sent to each client session.
    commands: HashMap<SessionId, Vec<serde_json::Value>>,
}

/// A permission question in flight on its own thread.
struct Ask {
    answered: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Bridge {
    fn handle(&mut self, envelope: &Envelope) {
        let session = envelope.session;
        let is_agent = self.tree.apply(envelope);
        let client_id = lock(&self.ids).by_engine.get(&session).cloned();
        match client_id {
            Some(client_id) => self.handle_client(session, client_id, &envelope.event),
            None if is_agent && self.subagents.load(Ordering::SeqCst) => {
                self.handle_subagent(session, &envelope.event);
            }
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
                let tool_call = permission_call(tool_call_id.as_ref(), tool, input);
                self.ask(session, *request_id, client_id, tool_call);
            }
            Event::Host(HostEvent::UsageUpdated { usage }) => {
                if let Some(update) = usage_update(usage) {
                    send_update(&self.peer, &client_id, update);
                }
            }
            Event::Host(HostEvent::StateChanged { state }) => {
                let settings = Settings::from(state);
                let changed = lock(&self.shown)
                    .get_mut(&session)
                    .is_some_and(|shown| shown.observe(&settings));
                if changed {
                    let update = ConfigOptionUpdate {
                        config_options: config_options(&self.models, &settings),
                    };
                    send_update(
                        &self.peer,
                        &client_id,
                        SessionUpdate::ConfigOptionUpdate(update),
                    );
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
            Event::Host(HostEvent::McpServers { servers }) if !self.live.contains(&session) => {
                let commands = prompt_commands(servers);
                let last = self.commands.insert(session, commands.clone());
                if last.unwrap_or_default() != commands {
                    let update = AvailableCommandsUpdate {
                        available_commands: commands,
                    };
                    send_update(
                        &self.peer,
                        &client_id,
                        SessionUpdate::AvailableCommandsUpdate(update),
                    );
                }
            }
            Event::Host(HostEvent::RunEnded { outcome }) => {
                self.end_asks(session);
                self.seen.remove(&session);
                let stop = self.stops.remove(&session);
                let end = PromptEnd {
                    outcome: outcome.clone(),
                    stop,
                };
                self.ended.insert(session, end);
                self.settle(session);
            }
            Event::Host(_) => {}
        }
    }

    /// Announces an agent as a subagent of its parent's client session,
    /// then shows its activity on its own session until it ends.
    fn handle_subagent(&mut self, session: SessionId, event: &Event) {
        if let Event::Host(HostEvent::AgentSpawned {
            parent,
            agent,
            description,
            ..
        }) = event
        {
            let Some(parent_id) = self.client_of(*parent) else {
                return;
            };
            self.live.insert(session);
            lock(&self.ids)
                .subagents
                .insert(session.to_string(), session);
            let update = SubagentUpdate {
                subagent_session_id: session.to_string(),
                name: Some(agent.clone()),
                task: Some(description.clone()),
                capabilities: Some(SubagentSessionCapabilities { cancel: true }),
                state: None,
            };
            send_update(
                &self.peer,
                &parent_id,
                SessionUpdate::SubagentUpdate(update),
            );
        } else if self.live.contains(&session) {
            self.handle_client(session, session.to_string(), event);
        }
    }

    /// The client's id for `session`: a client session's own id, or a
    /// live subagent's engine id.
    fn client_of(&self, session: SessionId) -> Option<String> {
        let client_id = lock(&self.ids).by_engine.get(&session).cloned();
        client_id.or_else(|| self.live.contains(&session).then(|| session.to_string()))
    }

    /// Finishes the ended run of `session` once none of its subagents is
    /// live: a subagent sends its terminal state to its parent, and a
    /// client session answers its waiting prompt. The RFD wants every
    /// subagent to end before its parent does.
    fn settle(&mut self, session: SessionId) {
        let waits = self
            .live
            .iter()
            .any(|s| self.tree.get(*s).is_some_and(|node| node.parent == session));
        if waits {
            return;
        }
        let Some(end) = self.ended.remove(&session) else {
            return;
        };
        if !self.live.remove(&session) {
            if let Some(waiter) = lock(&self.waiters).remove(&session) {
                let _ = waiter.send(end);
            }
            return;
        }
        lock(&self.ids).subagents.remove(&session.to_string());
        let Some(parent) = self.tree.get(session).map(|node| node.parent) else {
            return;
        };
        if let Some(parent_id) = self.client_of(parent) {
            let update = SubagentUpdate {
                subagent_session_id: session.to_string(),
                state: Some(match end.outcome {
                    RunOutcome::Completed => SubagentState::Completed,
                    RunOutcome::Cancelled => SubagentState::Cancelled,
                    RunOutcome::Failed { .. } => SubagentState::Failed,
                }),
                ..SubagentUpdate::default()
            };
            send_update(
                &self.peer,
                &parent_id,
                SessionUpdate::SubagentUpdate(update),
            );
        }
        self.settle(parent);
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
    /// `session` with the answer. The ask is withdrawn when that
    /// session's run ends first.
    fn ask(
        &mut self,
        session: SessionId,
        request_id: RequestId,
        client_id: String,
        tool_call: ToolCallUpdate,
    ) {
        let answered = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&answered);
        let peer = self.peer.clone();
        let commander = self.commander.clone();
        let thread = std::thread::spawn(move || {
            let title = tool_call.title.clone().unwrap_or_default();
            let decision =
                kage_acp::agent::request_permission(&peer, &client_id, tool_call, &title, &|| {
                    answered.load(Ordering::SeqCst)
                });
            let decision = match decision {
                PermissionDecision::Allow => Decision::AllowOnce,
                PermissionDecision::AllowSession => Decision::AllowSession,
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
        self.asks.entry(session).or_default().push(Ask {
            answered: flag,
            thread,
        });
    }

    /// Withdraws the open asks of `session` and waits until each is
    /// answered or withdrawn, so no update that follows overtakes them.
    fn end_asks(&mut self, session: SessionId) {
        let asks = self.asks.remove(&session).unwrap_or_default();
        for ask in &asks {
            ask.answered.store(true, Ordering::SeqCst);
        }
        for ask in asks {
            let _ = ask.thread.join();
        }
    }
}

/// The tool call a permission request for `tool` shows.
fn permission_call(
    tool_call_id: Option<&ToolCallId>,
    tool: &str,
    input: &serde_json::Value,
) -> ToolCallUpdate {
    ToolCallUpdate {
        tool_call_id: tool_call_id.map_or_else(String::new, ToString::to_string),
        title: Some(tool.to_owned()),
        kind: Some(tool_kind(tool)),
        status: Some(ToolCallStatus::Pending),
        raw_input: Some(input.clone()),
        ..ToolCallUpdate::default()
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

/// One ACP prompt block as the content the engine receives. Embedded
/// text becomes a resource block, images stay images, and what the model
/// cannot take becomes one line saying what was attached.
fn prompt_content(block: ContentBlock) -> Content {
    let image = |data: String, mime: String| Content::Image {
        source: ImageSource::Base64 { data },
        mime,
    };
    let text = |text: String| Content::Text { text };
    match block {
        ContentBlock::Text(t) => text(t.text),
        ContentBlock::Image(blob) => image(blob.data, blob.mime_type),
        ContentBlock::Audio(_) => text("[audio omitted]".to_owned()),
        ContentBlock::ResourceLink(link) => text(match link.uri.strip_prefix("file://") {
            Some(path) => format!("Referenced file: {path}"),
            None => format!("Referenced resource: {} ({})", link.uri, link.name),
        }),
        ContentBlock::Resource(embedded) => {
            let field = |key: &str| {
                embedded
                    .resource
                    .get(key)
                    .and_then(serde_json::Value::as_str)
            };
            let uri = field("uri").unwrap_or_default();
            let mime = field("mimeType");
            match (field("text"), field("blob"), mime) {
                (Some(body), ..) => text(kage_core::resource_block::render(uri, None, mime, body)),
                (None, Some(data), Some(mime)) if mime.starts_with("image/") => {
                    image(data.to_owned(), mime.to_owned())
                }
                (None, Some(_), _) => text(format!(
                    "[binary resource {uri}: {}]",
                    mime.unwrap_or("application/octet-stream")
                )),
                (None, None, _) => text("[resource omitted]".to_owned()),
            }
        }
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

/// One `available_commands_update` entry per prompt of a live server in
/// `servers`, named `<server>:<prompt>`. The input hint lists required
/// arguments as `<name>` and optional ones as `[name]`, and a prompt
/// without arguments takes no input.
fn prompt_commands(servers: &[McpServerInfo]) -> Vec<serde_json::Value> {
    let live = servers
        .iter()
        .filter(|server| server.status == McpServerStatus::Connected);
    live.flat_map(|server| {
        server.prompts.iter().map(move |prompt| {
            let hint = prompt
                .arguments
                .iter()
                .map(|arg| {
                    if arg.required {
                        format!("<{}>", arg.name)
                    } else {
                        format!("[{}]", arg.name)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let mut command = serde_json::json!({
                "name": format!("{}:{}", server.name, prompt.name),
                "description": prompt.description.clone().unwrap_or_default(),
            });
            if !hint.is_empty() {
                command["input"] = serde_json::json!({ "hint": hint });
            }
            command
        })
    })
    .collect()
}

/// The model, thinking and mode options showing `settings`. The model
/// option offers `models`, plus the current model when they lack it.
fn config_options(
    models: &[SessionConfigSelectOption],
    settings: &Settings,
) -> Vec<SessionConfigOption> {
    let mut model_choices = models.to_vec();
    if !models.iter().any(|m| m.value == settings.model) {
        model_choices.insert(0, choice(&settings.model, &settings.model, None));
    }
    let levels = std::iter::successors(Some(ThinkingLevel::Off), |level| {
        Some(level.cycle()).filter(|next| !next.is_off())
    });
    let mode = MODES
        .iter()
        .find(|m| m.1 == settings.mode)
        .map_or("default", |m| m.0);
    vec![
        select(
            "model",
            "Model",
            SessionConfigCategory::Model,
            &settings.model,
            model_choices,
        ),
        select(
            "thinking",
            "Thinking",
            SessionConfigCategory::ThoughtLevel,
            settings.thinking.as_str(),
            levels
                .map(|level| choice(level.as_str(), level.label(), None))
                .collect(),
        ),
        select(
            "mode",
            "Mode",
            SessionConfigCategory::Mode,
            mode,
            MODES
                .iter()
                .map(|(value, _, name, description)| choice(value, name, Some(description)))
                .collect(),
        ),
    ]
}

fn select(
    id: &str,
    name: &str,
    category: SessionConfigCategory,
    current: &str,
    options: Vec<SessionConfigSelectOption>,
) -> SessionConfigOption {
    SessionConfigOption {
        id: id.to_owned(),
        name: name.to_owned(),
        description: None,
        category: Some(category),
        kind: SessionConfigKind::Select,
        current_value: current.to_owned(),
        options,
    }
}

fn choice(value: &str, name: &str, description: Option<&str>) -> SessionConfigSelectOption {
    SessionConfigSelectOption {
        value: value.to_owned(),
        name: name.to_owned(),
        description: description.map(str::to_owned),
    }
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
                    context: None,
                    max_output: None,
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
    /// [`mcp_connection`] as `srv` when `mcp` is set.
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
                    let mut tools = builtin_registry();
                    let mcp = mcp.then(|| mcp_manager(&mut tools));
                    Ok(SessionSpec {
                        id,
                        model: model.to_owned(),
                        cx: AgentContext::new(model, "")
                            .with_workdir(&workdir)
                            .with_context_window(WINDOW),
                        recorder: None,
                        tools,
                        gate: PermissionGate::new(rules),
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
                let agent = CliAcpAgent::new(registry, "mock:m".into(), sessions, spec, peer);
                agent.open(id.to_string(), (agent.spec)(id, "", "mock:m").unwrap());
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

    fn mcp_manager(tools: &mut kage_tools::ToolRegistry) -> kage_mcp::McpManager {
        let cfg = kage_core::config::McpConfig::default();
        let (mut mcp, _errors) = kage_mcp::McpManager::spawn_all(&cfg, Vec::new(), None);
        mcp.adopt("srv", mcp_connection());
        assert!(mcp.register_into(tools).is_empty());
        mcp
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
        params["update"]["sessionUpdate"] == "subagent_update"
            && !params["update"]["state"].is_null()
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
        let params =
            serde_json::json!({"protocolVersion": PROTOCOL_VERSION, "clientCapabilities": {}});
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
                text(
                    "<resource uri=\"file:///w/a.rs\" mime=\"text/rust\">\nfn a() {}\n</resource>"
                ),
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
        assert_eq!(current_values(&resumed), ["mock:m", "off", "default"]);
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
        assert_eq!(current_values(&created), ["mock:m", "off", "default"]);
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
            ["off", "minimal", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(
            values_of(&options[2], "value"),
            ["default", "ask", "allow", "deny"]
        );
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
        assert_eq!(current_values(update), ["mock:other", "off", "default"]);

        h.command(model("mock:other"));
        h.command(CommandKind::SetThinking {
            level: ThinkingLevel::Low,
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
            thinking,
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
