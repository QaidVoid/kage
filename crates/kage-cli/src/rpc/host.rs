//! The shared engine and session setup behind every `kage rpc`
//! connection.

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kage_acp::acp::SessionConfigSelectOption;
use kage_acp::agent::serve_agent;
use kage_core::config::{Config, McpServer as McpSpec};
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{Event, HostEvent, SessionId};
use kage_core::sync::lock;
use kage_jsonrpc::RpcError;
use kage_loop::{AgentContext, LoopConfig};
use kage_provider::ProviderRegistry;
use kage_tools::{ToolRegistry, builtin_registry};

use super::options::{Settings, choice};
use super::{CliAcpAgent, live::Live, mcp};
use crate::engine::{AgentSetup, Engine, SessionSpec};
use crate::permissions::PermissionGate;

/// Builds the engine session for a client session from its id, working
/// directory, model and the MCP servers the client passed. The caller
/// fills in the history and recorder.
pub(super) type SpecBuilder = Box<
    dyn Fn(SessionId, &str, &str, BTreeMap<String, McpSpec>) -> Result<SessionSpec, RpcError>
        + Send
        + Sync,
>;

/// A session open in the engine, with what a connection attaching to it
/// needs.
pub(super) struct Open {
    /// What the session's config options last showed.
    pub(super) settings: Settings,
}

/// One engine with the shared setup of [`CliAcpAgent`], behind every
/// connection served on the host.
pub(super) struct Host {
    pub(super) engine: Engine,
    pub(super) registry: Arc<ProviderRegistry>,
    pub(super) default_model: String,
    pub(super) sessions: PathBuf,
    pub(super) spec: SpecBuilder,
    pub(super) models: Arc<[SessionConfigSelectOption]>,
    /// Advertised name to real name for tools the host renamed, for the
    /// bridges' card titles and kind hints. Loaded once at startup, like
    /// the TUI does.
    pub(super) aliases: BTreeMap<String, String>,
    /// Live engine state one subscriber folds envelopes into: the turn
    /// in flight, open asks, the agent tree, prompt owners, and what a
    /// session's attachments allow closing.
    pub(super) live: Arc<Mutex<Live>>,
    /// Sessions open in the engine, so a load from another connection
    /// attaches to the live session instead of reopening its file.
    open: Arc<Mutex<HashMap<SessionId, Open>>>,
    /// The workdir of every launched session, so `_kage/fs` can confine
    /// its paths per session.
    workdirs: Arc<Mutex<HashMap<SessionId, PathBuf>>>,
    /// The id the next connection gets.
    next_connection: AtomicU64,
}

impl Host {
    /// The host `kage rpc` serves, with the credential and model checks
    /// the subcommand ran before serving. `Err` names the failure, ready
    /// for the `kage: ` error line.
    pub(super) fn start(
        model_override: Option<&str>,
        system_role: &str,
    ) -> Result<Arc<Self>, String> {
        let registry = crate::build_provider_registry()?;
        if !crate::has_usable_provider(&registry) && model_override.is_none() {
            return Err(
                "rpc: no provider credentials found; run `kage auth login` or set an API-key env var"
                    .to_owned(),
            );
        }
        let default_model =
            model_override.map_or_else(|| crate::default_model(&registry), str::to_owned);
        registry
            .resolve(&default_model)
            .map_err(|e| format!("rpc: cannot resolve model {default_model}: {e}"))?;
        let sessions = crate::sessions_dir().map_err(|e| format!("rpc: {e}"))?;
        let registry = Arc::new(registry);
        let aliases = {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let renames = Config::load_layered(&cwd)
                .map(|config| config.tools.rename)
                .unwrap_or_default();
            ToolRegistry::new().with_renames(&renames).alias_map()
        };
        let spec: SpecBuilder = {
            let registry = Arc::clone(&registry);
            let system_role = system_role.to_owned();
            Box::new(move |id, cwd: &str, model: &str, servers| {
                session_spec(&registry, &system_role, id, cwd, model, servers)
            })
        };
        Ok(Self::new(registry, default_model, sessions, spec, aliases))
    }

    /// The engine and shared setup every connection works through. The
    /// model list follows `registry`.
    pub(super) fn new(
        registry: Arc<ProviderRegistry>,
        default_model: String,
        sessions: PathBuf,
        spec: SpecBuilder,
        aliases: BTreeMap<String, String>,
    ) -> Arc<Self> {
        let models: Arc<[SessionConfigSelectOption]> =
            crate::tui::available_model_items(&registry, "")
                .into_iter()
                .map(|item| choice(&item.value, &item.label, item.group.as_deref()))
                .collect();
        let engine = Engine::start(Arc::clone(&registry));
        let live = Arc::new(Mutex::new(Live::new(engine.commander())));
        let host = Arc::new(Self {
            engine,
            registry,
            default_model,
            sessions,
            spec,
            models,
            aliases,
            live,
            open: Arc::default(),
            workdirs: Arc::default(),
            next_connection: AtomicU64::new(0),
        });
        let open = Arc::clone(&host.open);
        host.engine.subscribe(Box::new(move |envelope| {
            if let Event::Host(HostEvent::StateChanged { state }) = &envelope.event
                && let Some(open) = lock(&open).get_mut(&envelope.session)
            {
                open.settings = Settings::from(state);
            }
        }));
        let live = Arc::clone(&host.live);
        host.engine
            .subscribe(Box::new(move |envelope| lock(&live).observe(envelope)));
        host
    }

    /// Serves one connection: a fresh agent on this host for `reader`
    /// and `writer`, sharing the engine with every other connection.
    pub(super) fn serve<R, W>(self: Arc<Self>, reader: R, writer: W) -> Result<(), RpcError>
    where
        R: BufRead + Send + 'static,
        W: std::io::Write + Send + 'static,
    {
        self.serve_with(reader, writer, |_| {})
    }

    /// [`Host::serve`] with `prepare` run on the connection's agent
    /// before the first request arrives. The rpc tests open their
    /// standing session there.
    pub(super) fn serve_with<R, W, P>(
        self: Arc<Self>,
        reader: R,
        writer: W,
        prepare: P,
    ) -> Result<(), RpcError>
    where
        R: BufRead + Send + 'static,
        W: std::io::Write + Send + 'static,
        P: FnOnce(&CliAcpAgent) + Send + 'static,
    {
        serve_agent(reader, writer, |peer| {
            let agent = CliAcpAgent::new(Arc::clone(&self), peer);
            prepare(&agent);
            agent
        })
    }

    /// Hosts `spec` in the engine and records it open with `settings`,
    /// so a later load from another connection attaches to it. The
    /// opening connection counts as the session's first attachment.
    pub(super) fn launch(&self, spec: SessionSpec, settings: Settings) {
        lock(&self.live).attach(spec.id, true);
        lock(&self.open).insert(spec.id, Open { settings });
        lock(&self.workdirs).insert(spec.id, spec.cx.workdir.clone());
        self.engine.open(spec);
    }

    /// The workdir a session was launched with, the root `_kage/fs`
    /// confines its paths to.
    pub(super) fn workdir(&self, id: SessionId) -> Option<PathBuf> {
        lock(&self.workdirs).get(&id).cloned()
    }

    /// The settings of a session another connection has open, so the
    /// caller attaches to it instead of reopening its file. A session
    /// a close was just sent for reads as unopened.
    pub(super) fn open_settings(&self, id: SessionId) -> Option<Settings> {
        if lock(&self.live).is_closing(id) {
            return None;
        }
        lock(&self.open).get(&id).map(|open| open.settings.clone())
    }

    /// Counts a connection's attachment to `id`. A fresh open follows
    /// a close of the same session file, so Live drops stale state.
    pub(super) fn attach(&self, id: SessionId, fresh: bool) {
        lock(&self.live).attach(id, fresh);
    }

    /// Records that a connection released `id`, closing the session
    /// when it was the last attachment and the session is idle.
    pub(super) fn release(&self, id: SessionId) {
        lock(&self.live).release(id);
    }

    /// The next connection's id on this host.
    pub(super) fn next_connection(&self) -> u64 {
        self.next_connection.fetch_add(1, Ordering::Relaxed)
    }

    /// Records `connection` as the owner of `id`'s running prompt, or
    /// returns false when another connection owns it.
    pub(super) fn claim_prompt(&self, id: SessionId, connection: u64) -> bool {
        lock(&self.live).claim_prompt(id, connection)
    }

    /// Forgets every prompt `connection` owns, after it disconnected.
    pub(super) fn release_prompts_of(&self, connection: u64) {
        lock(&self.live).release_prompts_of(connection);
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
    let config = Config::load_layered(&workdir).map_err(|e| RpcError::internal(e.to_string()))?;
    let model = model.to_owned();
    let bare = crate::runtime_env::build_system_prompt(system_role, &workdir, &model, &[], None);
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
    let system_prompt = crate::runtime_env::build_system_prompt(
        system_role,
        &workdir,
        &model,
        &skills,
        config.shell.program.as_deref(),
    );
    let mut tools = builtin_registry()
        .with_shell_config(&config.shell)
        .with_renames(&config.tools.rename);
    let aliases = tools.alias_map();
    let editor: Vec<String> = servers.keys().cloned().collect();
    let (mcp, mcp_errors) =
        crate::mcp::spawn_and_register_with(&mut tools, &workdir, plugins.as_deref(), servers);
    for (server, err) in mcp_errors {
        eprintln!("kage: mcp `{server}`: {}", mcp::without_login(err, &editor));
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
    if let Some(window) = crate::runtime_env::context_window_for(registry, &model) {
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
            .with_aliases(aliases)
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
