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
//! as prompt text. The MCP servers a client passes when it opens a session
//! run for that session like the user's own configured servers, and win a
//! name clash with them.
//!
//! Agent sessions started by the `agent` tool are shown as subagent
//! sessions (draft RFD PR #1992) to a client that advertises the
//! `subagents` capability: announced with `subagent_update` on their
//! parent's session, streaming on their own session, and asking there.
//! For every other client they are not ACP sessions. Their permission
//! requests and progress go to the client session at the root of their
//! tree, on that session's top-level `agent` call.

mod bridge;
mod content;
mod mcp;
mod options;
mod sessions;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufReader;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use kage_acp::acp::{
    AgentCapabilities, Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, McpCapabilities,
    NewSessionRequest, NewSessionResponse, PROTOCOL_VERSION, PromptCapabilities, PromptRequest,
    PromptResponse, ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities,
    SessionConfigOption, SessionConfigSelectOption, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, Supported,
};
use kage_acp::agent::{Agent, PromptContext, send_update, serve_agent};
use kage_core::config::McpServer as McpSpec;
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{AgentTree, Command, CommandKind, Delivery, RunOutcome};
use kage_core::sync::lock;
use kage_core::{LoopError, SessionId, StopReason as CoreStopReason};
use kage_jsonrpc::{Peer, RpcError};
use kage_loop::{AgentContext, LoopConfig};
use kage_provider::ProviderRegistry;
use kage_tools::builtin_registry;

use bridge::Bridge;
use content::prompt_content;
use mcp::{editor_servers, without_login};
use options::{Settings, Shown, choice, config_options};
use sessions::list_page;

use crate::engine::{AgentSetup, Engine, Recorder, SessionSpec};
use crate::permissions::PermissionGate;
use crate::runtime_env;

/// Entry point for the `Rpc` subcommand.
pub(crate) fn run(model_override: Option<&str>, system_role: &str) -> ExitCode {
    let registry = match crate::build_provider_registry() {
        Ok(registry) => registry,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
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
        Box::new(move |id, cwd: &str, model: &str, servers| {
            session_spec(&registry, &system_role, id, cwd, model, servers)
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

/// Updates for client sessions whose opening response is not written
/// yet. Sending them earlier would reach the client before it knows the
/// session.
type Held = Arc<Mutex<HashMap<SessionId, Vec<SessionUpdate>>>>;

/// Builds the engine session for a client session from its id, working
/// directory, model and the MCP servers the client passed. The caller
/// fills in the history and recorder.
type SpecBuilder = Box<
    dyn Fn(SessionId, &str, &str, BTreeMap<String, McpSpec>) -> Result<SessionSpec, RpcError>
        + Send
        + Sync,
>;

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
    peer: Peer,
    held: Held,
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
        let held = Held::default();
        let mut bridge = Bridge {
            peer: peer.clone(),
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
            held: Arc::clone(&held),
            approving: HashMap::new(),
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
            peer,
            held,
        }
    }

    /// Opens `spec` as the client session `client_id` and returns its
    /// config options. Updates for it wait for
    /// [`Agent::session_announced`].
    fn open(&self, client_id: String, spec: SessionSpec) -> Vec<SessionConfigOption> {
        let settings = Settings::of(&spec, &self.registry);
        let options = config_options(&self.models, &settings);
        let shown = Shown {
            settings,
            catching_up: false,
        };
        lock(&self.shown).insert(spec.id, shown);
        lock(&self.held).insert(spec.id, Vec::new());
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
}

/// Everything an engine session for `cwd` on `model` runs with, including
/// the client's MCP `servers`. The caller fills in the history and
/// recorder.
fn session_spec(
    registry: &ProviderRegistry,
    system_role: &str,
    id: SessionId,
    cwd: &str,
    model: &str,
    servers: BTreeMap<String, McpSpec>,
) -> Result<SessionSpec, RpcError> {
    let workdir = if cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(cwd)
    };
    crate::trust::warn_if_untrusted(&workdir);
    let config = kage_core::config::Config::load_layered(&workdir)
        .map_err(|e| RpcError::internal(e.to_string()))?;
    let model = model.to_owned();
    let bare = runtime_env::build_system_prompt(system_role, &workdir, &model, &[], None);
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
    let system_prompt = runtime_env::build_system_prompt(
        system_role,
        &workdir,
        &model,
        &skills,
        config.shell.program.as_deref(),
    );
    let mut tools = builtin_registry().with_shell_config(&config.shell);
    let editor: Vec<String> = servers.keys().cloned().collect();
    let (mcp, mcp_errors) =
        crate::mcp::spawn_and_register_with(&mut tools, &workdir, plugins.as_deref(), servers);
    for (server, err) in mcp_errors {
        eprintln!("kage: mcp `{server}`: {}", without_login(err, &editor));
    }
    let (defs, agent_errors) = crate::agents::load(&workdir);
    for err in agent_errors {
        eprintln!("kage: {err}");
    }
    let agents = AgentSetup::from_config(defs, &config);
    config
        .permissions
        .validate()
        .map_err(|e| RpcError::internal(format!("permissions: {e}")))?;
    config
        .shell
        .validate()
        .map_err(|e| RpcError::internal(format!("shell: {e}")))?;
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
        shell: config.shell.program.clone(),
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
                mcp_capabilities: McpCapabilities {
                    http: true,
                    sse: false,
                },
                session_capabilities: SessionCapabilities {
                    list: Some(Supported {}),
                    resume: Some(Supported {}),
                },
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
        let servers = editor_servers(&req.mcp_servers)?;
        let (path, mut header) =
            crate::plan_session(&self.default_model, "").map_err(RpcError::internal)?;
        let id = header.session;
        let mut spec = (self.spec)(id, &req.cwd, &self.default_model, servers)?;
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
        let config_options =
            self.open_recorded(&req.session_id, &req.cwd, &req.mcp_servers, Some(ctx))?;
        Ok(LoadSessionResponse { config_options })
    }

    fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, RpcError> {
        list_page(&self.sessions, &req)
    }

    fn resume_session(&self, req: ResumeSessionRequest) -> Result<ResumeSessionResponse, RpcError> {
        let config_options =
            self.open_recorded(&req.session_id, &req.cwd, &req.mcp_servers, None)?;
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
            RunOutcome::Failed {
                error: LoopError::InvalidPrompt { message },
            } => Err(RpcError::new(-32602, message)),
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

    fn session_announced(&self, session_id: &str) {
        let Some(id) = lock(&self.ids).by_client.get(session_id).copied() else {
            return;
        };
        let mut held = lock(&self.held);
        for update in held.remove(&id).unwrap_or_default() {
            send_update(&self.peer, session_id, update);
        }
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
mod tests;
