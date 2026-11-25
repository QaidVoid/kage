//! The shared engine and session setup behind every `kage rpc` and
//! `kage serve` connection.

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::Duration;

use kage_acp::acp::SessionConfigSelectOption;
use kage_acp::agent::serve_agent;
use kage_core::config::{Config, McpServer as McpSpec};
use kage_core::environment::Environment;
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{
    Command, CommandId, CommandKind, Event, HostEvent, PermissionDecision, SessionId,
};
use kage_core::sync::lock;
use kage_jsonrpc::RpcError;
use kage_loop::{AgentContext, LoopConfig};
use kage_plugin::{
    LogLevel, PluginRuntime, SharedHostLog, default_host_log, session_plugins::SessionPlugins,
};
use kage_provider::ProviderRegistry;

use super::options::{Settings, choice};
use super::projects::{Project, Projects};
use super::{
    CliAcpAgent,
    live::{AskRef, Live},
};
use crate::engine::{AgentSetup, Background, Engine, SessionSpec, WithMediaTools as _};
use crate::permissions::PermissionGate;

/// How long an ask may hold a host after its last client left.
pub(crate) const ASK_HOLD: Duration = Duration::from_secs(12 * 60 * 60);

/// Builds the engine session for a client session from the providers
/// in effect, its id, working directory, model, the MCP servers the
/// client passed, the environment its processes start with, the
/// session plugins the connection brings, which replace the project's
/// when present, and the client's own system role, which replaces the
/// host's when given. The caller fills in the history and recorder,
/// and applies the rest of the connection's settings.
/// Each open session's renamed tools: advertised name to real name.
pub(super) type SessionAliases = Arc<Mutex<HashMap<SessionId, Arc<BTreeMap<String, String>>>>>;

pub(crate) type SpecBuilder = Box<
    dyn Fn(
            &ProviderRegistry,
            SessionId,
            &str,
            &str,
            BTreeMap<String, McpSpec>,
            &Environment,
            Option<Arc<dyn SessionPlugins>>,
            Option<&str>,
        ) -> Result<(SessionSpec, Option<Arc<kage_plugin::PluginRuntime>>), RpcError>
        + Send
        + Sync,
>;

/// A session open in the engine, with what a connection attaching to it
/// needs.
pub(super) struct Open {
    /// What the session's config options last showed.
    pub(super) settings: Settings,
}

/// How long one native write may wait for a slow client.
#[cfg(unix)]
const NATIVE_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Inbound lines as large as the generic transport accepts, with fewer queued
/// so the reader's memory stays within the native peer's budget.
#[cfg(unix)]
const NATIVE_LIMITS: kage_jsonrpc::unix_peer::ReadLimits = kage_jsonrpc::unix_peer::ReadLimits {
    max_line_bytes: 8 * 1024 * 1024,
    inbound_capacity: 2,
};

/// How long serving waits for its last output to reach the client.
#[cfg(unix)]
const NATIVE_DRAIN: std::time::Duration = std::time::Duration::from_secs(5);

/// A writer that reports when the pump is done with it.
#[cfg(unix)]
struct DoneOnDrop<W>(W, Option<std::sync::mpsc::Sender<()>>);

#[cfg(unix)]
impl<W: std::io::Write> std::io::Write for DoneOnDrop<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[cfg(unix)]
impl<W> Drop for DoneOnDrop<W> {
    fn drop(&mut self) {
        if let Some(done) = self.1.take() {
            let _ = done.send(());
        }
    }
}

/// The providers, model choices, and default published by one host reload.
pub(crate) struct ProviderSnapshot {
    /// The providers available to run models.
    pub(crate) registry: Arc<ProviderRegistry>,
    /// The model choices those providers advertise.
    pub(crate) models: Arc<[SessionConfigSelectOption]>,
    /// The model new sessions use without an explicit choice.
    pub(crate) default_model: String,
}

/// One engine with the shared setup of [`CliAcpAgent`], behind every
/// connection served on the host.
pub(crate) struct Host {
    pub(super) engine: Engine,
    pub(super) external: Mutex<HashMap<String, Arc<super::external::External>>>,
    /// Capacity for external agent processes, held until cleanup finishes.
    pub(super) lifecycles: super::external::Lifecycles,
    /// External sessions being restored, so one starts only one process.
    pub(super) restoring: Mutex<std::collections::HashSet<String>>,
    /// The providers in effect, replaced when the config's change.
    providers: RwLock<Arc<ProviderSnapshot>>,
    model_override: Mutex<Option<String>>,
    pub(super) sessions: PathBuf,
    pub(super) spec: SpecBuilder,
    /// The model choices `registry` offers, shared with every
    /// connection so a reload reaches the updates they send.
    models: Arc<RwLock<Arc<[SessionConfigSelectOption]>>>,
    /// Held across a reload of the providers, so two at once cannot
    /// leave the older build in effect.
    reloading: Mutex<()>,
    /// Advertised name to real name for the tools each open session
    /// renamed, by `[tools.rename]` in its project or by its plugins,
    /// for the bridges' card titles and kind hints.
    pub(super) aliases: SessionAliases,
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
    /// The plugin runtime of every launched session that has one, so a
    /// client's plugin command runs where its session's hooks do.
    runtimes: Arc<Mutex<HashMap<SessionId, Arc<kage_plugin::PluginRuntime>>>>,
    /// The id the next connection gets.
    next_connection: AtomicU64,
    /// The id the next command a request waits on gets.
    next_command: AtomicU64,
    /// The launch token of the session each id was last launched as,
    /// so a close report names the exact launch whose bookkeeping it
    /// may drop. Entries are removed when that close is consumed, so
    /// the map only ever holds hosted sessions.
    pub(super) instances: Arc<Mutex<HashMap<SessionId, u64>>>,
    /// The launch token the next launch gets.
    next_instance: AtomicU64,
    /// The plugin runtime of each project, when the host loads plugins.
    projects: OnceLock<Arc<Projects>>,
    /// The system prompt role sessions start with.
    system_role: Mutex<String>,
    sink: SharedHostLog,
}

impl Host {
    /// The host every connection is served on, with the credential and
    /// model checks the subcommand ran before serving. The error is
    /// ready for the caller's `kage: ` line; the caller adds its own
    /// subcommand prefix. With `yolo`, every session opens in allow
    /// mode.
    pub(crate) fn start(
        model_override: Option<&str>,
        system_role: &str,
        yolo: bool,
    ) -> Result<Arc<Self>, String> {
        Self::start_with_log(model_override, system_role, yolo, default_host_log())
    }

    /// Starts a host whose plugin runtimes and diagnostics use `sink`,
    /// including provider reloads and project sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when providers cannot be configured, the selected
    /// model cannot be resolved, or the sessions directory cannot be found.
    pub(crate) fn start_with_log(
        model_override: Option<&str>,
        system_role: &str,
        yolo: bool,
        sink: SharedHostLog,
    ) -> Result<Arc<Self>, String> {
        let mut registry = crate::providers::build_provider_registry_with_log(&sink)?;
        merge_plugin_providers(&mut registry, model_override, system_role, &sink);
        let default_model =
            model_override.map_or_else(|| crate::default_model(&registry), str::to_owned);
        if crate::has_usable_provider(&registry) {
            registry
                .resolve(&default_model)
                .map_err(|e| format!("cannot resolve model {default_model}: {e}"))?;
        } else {
            // A headless entry point starts anyway: a GUI or editor
            // client connects, saves a key through `_kage/auth/set`,
            // and the reloaded providers resolve. Terminal users get
            // the hint here; runs fail with the same hint per prompt.
            lock(&sink).log(
                LogLevel::Warn,
                "no provider credentials found; add one with `kage auth login` or an API-key env var",
            );
        }
        let sessions = crate::sessions_dir()?;
        let registry = Arc::new(registry);
        let projects = Arc::new(Projects::with_log(Arc::clone(&sink)));
        let spec: SpecBuilder = {
            let system_role = system_role.to_owned();
            let projects = Arc::clone(&projects);
            Box::new(
                move |registry: &ProviderRegistry,
                      id,
                      cwd: &str,
                      model: &str,
                      servers,
                      env,
                      plugins,
                      client_system| {
                    let (spec, runtime) = session_spec(
                        registry,
                        &system_role,
                        id,
                        cwd,
                        model,
                        servers,
                        env,
                        &projects,
                        plugins,
                        client_system,
                    )?;
                    if yolo {
                        spec.gate.set_mode(Some(PermissionAction::Allow));
                    }
                    Ok((spec, runtime))
                },
            )
        };
        let host = Self::new_with_log(registry, default_model, sessions, spec, sink);
        *lock(&host.model_override) = model_override.map(str::to_owned);
        let _ = host.projects.set(projects);
        system_role.clone_into(&mut lock(&host.system_role));
        host.watch_config();
        Ok(host)
    }

    /// Follows edits to the user configuration and to the project
    /// configuration of each open session, on a thread that ends with
    /// the host.
    fn watch_config(self: &Arc<Self>) {
        let host = Arc::downgrade(self);
        let mut stamps = crate::config_watch::Stamps::default();
        stamps.changed(&self.config_files());
        let spawned = thread::Builder::new()
            .name("kage-config-watch".to_owned())
            .spawn(move || {
                loop {
                    thread::sleep(crate::config_watch::POLL);
                    let Some(host) = host.upgrade() else {
                        return;
                    };
                    let changed = stamps.changed(&host.config_files());
                    if !changed.is_empty() {
                        host.config_changed(&changed);
                    }
                }
            });
        if let Err(e) = spawned {
            lock(&self.sink).log(
                LogLevel::Warn,
                &format!("cannot watch the configuration: {e}"),
            );
        }
    }

    /// The user configuration and the project configuration of every
    /// open session's directory.
    fn config_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = Config::default_path().into_iter().collect();
        if let Ok(dir) = crate::config_dir() {
            files.push(dir.join("init.lua"));
        }
        for workdir in lock(&self.workdirs).values() {
            let project = Config::project_path(workdir);
            if !files.contains(&project) {
                files.push(project);
            }
        }
        files
    }

    /// Applies edited configuration `changed`: an edited user file
    /// reloads the providers, so a model it now names resolves, and each
    /// open session whose configuration changed brings its MCP servers
    /// in line.
    fn config_changed(self: &Arc<Self>, changed: &[PathBuf]) {
        let user = Config::default_path().is_some_and(|path| changed.contains(&path))
            || crate::config_dir().is_ok_and(|dir| changed.contains(&dir.join("init.lua")));
        if user {
            match self.reload_providers() {
                Ok(()) => {
                    if Config::load_default_raw().is_ok_and(|config| config.models.auto_refresh) {
                        self.fetch_missing_lists();
                    }
                }
                Err(e) => {
                    self.log(LogLevel::Error, &format!("providers not reloaded: {e}"));
                }
            }
        }
        let sessions: Vec<(SessionId, PathBuf)> = lock(&self.workdirs)
            .iter()
            .map(|(id, workdir)| (*id, workdir.clone()))
            .collect();
        let commander = self.engine.commander();
        for (id, workdir) in sessions {
            if !user && !changed.contains(&Config::project_path(&workdir)) {
                continue;
            }
            let runtime = lock(&self.runtimes).get(&id).cloned();
            if let Some(servers) = crate::mcp::configured_servers(&workdir, runtime.as_deref()) {
                commander.sync_mcp(id, servers);
            }
        }
    }

    /// The engine and shared setup every connection works through. The
    /// model list follows `registry`.
    #[cfg(test)]
    pub(crate) fn new(
        registry: Arc<ProviderRegistry>,
        default_model: String,
        sessions: PathBuf,
        spec: SpecBuilder,
    ) -> Arc<Self> {
        Self::new_with_log(registry, default_model, sessions, spec, default_host_log())
    }

    fn new_with_log(
        registry: Arc<ProviderRegistry>,
        default_model: String,
        sessions: PathBuf,
        spec: SpecBuilder,
        sink: SharedHostLog,
    ) -> Arc<Self> {
        let models = model_choices(&registry);
        let engine = Engine::start(Arc::clone(&registry));
        let live = Arc::new(Mutex::new(Live::new(engine.commander())));
        let host = Arc::new(Self {
            sink,
            engine,
            external: Mutex::default(),
            lifecycles: super::external::Lifecycles::new(super::external::MAX_SESSIONS),
            restoring: Mutex::default(),
            providers: RwLock::new(Arc::new(ProviderSnapshot {
                registry,
                models: Arc::clone(&models),
                default_model,
            })),
            model_override: Mutex::default(),
            sessions,
            spec,
            models: Arc::new(RwLock::new(models)),
            reloading: Mutex::default(),
            aliases: Arc::default(),
            live,
            open: Arc::default(),
            workdirs: Arc::default(),
            runtimes: Arc::default(),
            next_connection: AtomicU64::new(0),
            next_command: AtomicU64::new(0),
            next_instance: AtomicU64::new(0),
            instances: Arc::default(),
            projects: OnceLock::new(),
            system_role: Mutex::default(),
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
        host.prune_closed_sessions();
        host
    }

    /// Drops the bookkeeping of every hosted session the engine closed
    /// and clears its close-in-flight record, so its file may be
    /// deleted and the maps cannot grow with the sessions a long-lived
    /// host served. A report names the launch token its open carried;
    /// a report whose token no longer matches, or that names none, is
    /// stale: a newer launch replaced it, or the engine opened the
    /// session itself. The whole consumption happens under the
    /// instances lock, which [`Host::launch`] holds across its whole
    /// bookkeeping, so the two never interleave and a checked report
    /// can only remove the records of the launch it names. The stream
    /// ends when the engine stops.
    fn prune_closed_sessions(self: &Arc<Self>) {
        let closed = self.engine.closed_sessions();
        let host = Arc::clone(self);
        thread::Builder::new()
            .name("kage-host-prune".to_owned())
            .spawn(move || {
                for (id, instance) in closed {
                    let Some(instance) = instance else {
                        continue;
                    };
                    let mut instances = lock(&host.instances);
                    if instances.get(&id) != Some(&instance) {
                        continue;
                    }
                    instances.remove(&id);
                    lock(&host.open).remove(&id);
                    lock(&host.workdirs).remove(&id);
                    lock(&host.aliases).remove(&id);
                    lock(&host.runtimes).remove(&id);
                    lock(&host.live).closed(id);
                }
            })
            .expect("the host prune thread");
    }

    /// The sink shared by the host's runtimes and background diagnostics.
    pub(super) fn sink(&self) -> SharedHostLog {
        Arc::clone(&self.sink)
    }

    /// Records a diagnostic through the host's configured sink.
    pub(super) fn log(&self, level: LogLevel, message: &str) {
        lock(&self.sink).log(level, message);
    }

    /// The providers, model choices, and default from the same reload.
    pub(crate) fn providers(&self) -> Arc<ProviderSnapshot> {
        Arc::clone(
            &self
                .providers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// The providers in effect.
    pub(super) fn registry(&self) -> Arc<ProviderRegistry> {
        Arc::clone(&self.providers().registry)
    }

    /// The model new sessions use without an explicit choice.
    pub(super) fn default_model(&self) -> String {
        self.providers().default_model.clone()
    }

    /// The model choices shared with connection event bridges.
    pub(super) fn shared_models(&self) -> Arc<RwLock<Arc<[SessionConfigSelectOption]>>> {
        Arc::clone(&self.models)
    }

    /// The model choices the providers in effect offer.
    pub(super) fn models(&self) -> Arc<[SessionConfigSelectOption]> {
        Arc::clone(&self.providers().models)
    }

    /// Rebuilds configured and Lua providers without fetching model lists.
    pub(crate) fn reload_providers(&self) -> Result<(), String> {
        let _reloading = lock(&self.reloading);
        let mut registry = crate::providers::build_provider_registry_with_log(&self.sink)?;
        let model_override = lock(&self.model_override);
        merge_plugin_providers(
            &mut registry,
            model_override.as_deref(),
            &lock(&self.system_role),
            &self.sink,
        );
        let default_model = model_override
            .as_deref()
            .map_or_else(|| crate::default_model(&registry), str::to_owned);
        let registry = Arc::new(registry);
        let models = model_choices(&registry);
        let snapshot = Arc::new(ProviderSnapshot {
            registry: Arc::clone(&registry),
            models: Arc::clone(&models),
            default_model,
        });
        *self
            .providers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        *self
            .models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = models;
        self.engine.commander().set_registry(registry);
        Ok(())
    }

    /// Fetches, off the caller's thread, the model list of each custom
    /// provider with a `models_url` that has none cached yet, as one
    /// just saved or given its key, and reloads the providers when one
    /// arrived.
    pub(super) fn fetch_missing_lists(self: &Arc<Self>) {
        let host = Arc::downgrade(self);
        let _ = thread::Builder::new()
            .name("kage-fetch-models".to_owned())
            .spawn(move || {
                let Ok(config) = Config::load_default_raw() else {
                    return;
                };
                if crate::custom_models::fetch_missing(&config)
                    && let Some(host) = host.upgrade()
                {
                    let _ = host.reload_providers();
                }
            });
    }

    /// Serves one connection: a fresh agent on this host for `reader`
    /// and `writer`, sharing the engine with every other connection.
    pub(crate) fn serve<R, W>(self: Arc<Self>, reader: R, writer: W) -> Result<(), RpcError>
    where
        R: BufRead + Send + 'static,
        W: std::io::Write + Send + 'static,
    {
        self.serve_with(reader, writer, |_| {})
    }

    /// [`Host::serve`] for a client of this user on the local socket,
    /// whose `_kage/environment` the sessions it opens run tools with.
    #[cfg_attr(not(unix), allow(dead_code, reason = "the shared host is unix only"))]
    pub(crate) fn serve_local<R, W>(self: Arc<Self>, reader: R, writer: W) -> Result<(), RpcError>
    where
        R: BufRead + Send + 'static,
        W: std::io::Write + Send + 'static,
    {
        self.serve_with(reader, writer, CliAcpAgent::take_environment)
    }

    /// Serves one connection over the native transport, writing to `writer`
    /// through a pumped pipe.
    #[cfg(all(test, unix))]
    pub(super) fn serve_native<R, W>(self: Arc<Self>, reader: R, writer: W) -> Result<(), RpcError>
    where
        R: BufRead + Send + 'static,
        W: std::io::Write + Send + 'static,
    {
        self.serve_native_with(reader, writer, |_| {})
    }

    // Output still queued when serving ends gets a bounded time to drain.
    // A thread that keeps the peer alive past it must not hold up the caller.
    #[cfg(unix)]
    fn serve_native_with<R, W, P>(
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
        let (done, drained) = std::sync::mpsc::channel::<()>();
        let (fd, _pump) = super::native::pumped(DoneOnDrop(writer, Some(done)))
            .map_err(|e| RpcError::internal(format!("start native output: {e}")))?;
        let (peer, inbound, _reader) =
            kage_jsonrpc::unix_peer::connect_ordered_server_with_limits(reader, fd, NATIVE_LIMITS)
                .map_err(|e| RpcError::internal(format!("start native transport: {e}")))?;
        let served = kage_acp::agent::serve_agent_unix_connected(
            peer.clone(),
            &inbound,
            NATIVE_WRITE_TIMEOUT,
            |peer| {
                let agent = CliAcpAgent::new(Arc::clone(&self), peer);
                prepare(&agent);
                agent
            },
        );
        // Prompt workers may still hold the peer. Closing it ends the pump
        // once its queued lines are out.
        peer.shutdown();
        let _ = drained.recv_timeout(NATIVE_DRAIN);
        served
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
        #[cfg(unix)]
        if super::external::SUPPORTED {
            return self.serve_native_with(reader, writer, prepare);
        }
        serve_agent(reader, writer, |peer| {
            let agent = CliAcpAgent::new(Arc::clone(&self), peer);
            prepare(&agent);
            agent
        })
    }

    /// Hosts `spec` in the engine and records it open with `settings`,
    /// so a later load from another connection attaches to it. The
    /// opening connection counts as the session's first attachment.
    ///
    /// The launch hands the engine a fresh token and holds the
    /// instances lock across every step, the same lock the prune
    /// thread holds across a whole close-report consumption, so a
    /// report of an instance this launch replaced can never remove
    /// this launch's records, and a launch never installs into records
    /// a consumption is still removing. The lock is taken first and
    /// nothing else takes it while holding another, so the two
    /// critical sections share one order.
    pub(super) fn launch(
        &self,
        spec: SessionSpec,
        settings: Settings,
        runtime: Option<Arc<PluginRuntime>>,
    ) {
        let instance = self.next_instance.fetch_add(1, Ordering::Relaxed) + 1;
        let mut instances = lock(&self.instances);
        instances.insert(spec.id, instance);
        lock(&self.live).attach(spec.id, true);
        lock(&self.open).insert(spec.id, Open { settings });
        lock(&self.workdirs).insert(spec.id, spec.cx.workdir.clone());
        let renames = spec
            .plugins
            .as_ref()
            .map(|plugins| plugins.registered_renames())
            .unwrap_or_default();
        self.store_aliases(spec.id, spec.tools.alias_map(), &renames);
        if let Some(runtime) = runtime {
            lock(&self.runtimes).insert(spec.id, runtime);
        }
        self.engine.open_with_instance(spec, instance);
    }

    /// Records the renamed tools of session `id`: the `config` renames
    /// its registry carries plus its plugins' `renames`.
    fn store_aliases(
        &self,
        id: SessionId,
        mut aliases: BTreeMap<String, String>,
        renames: &[(String, String)],
    ) {
        for (from, to) in renames {
            aliases.insert(to.clone(), from.clone());
        }
        if aliases.is_empty() {
            lock(&self.aliases).remove(&id);
        } else {
            lock(&self.aliases).insert(id, Arc::new(aliases));
        }
    }

    /// Records the renames session `id`'s plugins apply after they
    /// reloaded, over the `[tools.rename]` of its project.
    pub(super) fn set_plugin_renames(&self, id: SessionId, renames: &[(String, String)]) {
        let Some(workdir) = self.workdir(id) else {
            return;
        };
        let config = Config::load_layered(&workdir)
            .map(|config| config.tools.rename)
            .unwrap_or_default();
        let aliases = kage_tools::ToolRegistry::new()
            .with_renames(&config)
            .alias_map();
        self.store_aliases(id, aliases, renames);
    }

    /// The plugin runtime of the project at `workdir`, loading it with the
    /// default model, the system prompt a session there would start with,
    /// and `env`, when no session has.
    pub(super) fn project(&self, workdir: &std::path::Path, env: &Environment) -> Option<Project> {
        let model = self.default_model();
        let prompt = || {
            crate::runtime_env::build_system_prompt(
                lock(&self.system_role).as_str(),
                workdir,
                &model,
                &[],
                None,
            )
        };
        self.projects.get()?.get(workdir, &model, &prompt, env)
    }

    /// Installs the project registry a test host loads runtimes from.
    #[cfg(test)]
    pub(super) fn set_projects_for_test(&self, projects: Projects) {
        assert!(self.projects.set(Arc::new(projects)).is_ok());
    }

    /// The plugin runtime the session `id` was launched with.
    pub(super) fn session_runtime(&self, id: SessionId) -> Option<Arc<kage_plugin::PluginRuntime>> {
        lock(&self.runtimes).get(&id).cloned()
    }

    /// Drop unused project runtimes and terminated external sessions.
    pub(crate) fn prune_projects(&self) {
        self.prune_external_sessions();
        if let Some(projects) = self.projects.get() {
            projects.prune();
        }
    }

    /// Release host entries whose upstream process has terminated.
    pub(super) fn prune_external_sessions(&self) {
        lock(&self.external).retain(|_, session| !session.is_closed());
    }

    /// Whether the project at `workdir` has anything to load, by the
    /// same check a runtime load would make.
    pub(super) fn project_has_content(&self, workdir: &std::path::Path) -> bool {
        self.projects
            .get()
            .is_some_and(|projects| projects.has_content(workdir))
    }

    /// A fresh id for a command a request waits on.
    pub(super) fn command_id(&self) -> CommandId {
        CommandId(self.next_command.fetch_add(1, Ordering::Relaxed))
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

    /// Whether some connection still holds `session` or the session its
    /// agent tree hangs under.
    pub(super) fn held(&self, session: SessionId) -> bool {
        lock(&self.live).held(session)
    }

    /// Declines the asks of sessions whose ask-hold window started at
    /// least `cap` ago: a permission ask is denied, a question ask is
    /// answered with no answers. An ask nobody can answer then stops
    /// holding the session and the host after the cap.
    pub(crate) fn decline_expired_asks(&self, cap: Duration) {
        let expired = lock(&self.live).take_expired_asks(cap);
        for ask in expired {
            self.decline(&ask);
        }
    }

    /// Declines every open ask under `root`: a permission ask is
    /// denied, a question ask is answered with no answers. Sent once no
    /// connection is left to answer, so runs parked on an ask go on
    /// instead of waiting for a client that is gone.
    pub(super) fn decline_asks_under(&self, root: SessionId) {
        let asks = lock(&self.live).asks_under(root);
        for ask in asks {
            self.decline(&ask);
        }
    }

    fn decline(&self, ask: &AskRef) {
        let kind = if ask.question {
            CommandKind::AnswerQuestion {
                request_id: ask.request_id,
                answers: None,
            }
        } else {
            CommandKind::ResolvePermission {
                request_id: ask.request_id,
                decision: PermissionDecision::Deny,
            }
        };
        self.engine.send(Command::to(ask.session, kind));
    }

    /// The next connection's id on this host.
    pub(super) fn next_connection(&self) -> u64 {
        self.next_connection.fetch_add(1, Ordering::Relaxed)
    }

    /// Whether a run, an agent, or an ask is in flight, so the shared host
    /// must not exit for being idle.
    #[cfg_attr(not(unix), allow(dead_code, reason = "the shared host is unix only"))]
    pub(crate) fn busy(&self) -> bool {
        lock(&self.live).busy()
    }

    /// Cancels every run. The dispatcher stops once each session is
    /// idle, which closes the session files; the caller gives it a
    /// moment before the process exits.
    pub(crate) fn shutdown(&self) {
        self.engine.send(Command::active(CommandKind::Shutdown));
    }

    /// Records `connection` as the owner of `id`'s running prompt, or
    /// returns false when another connection owns it or an ancestor.
    pub(super) fn claim_prompt(&self, id: SessionId, connection: u64) -> bool {
        lock(&self.live).claim_prompt(id, connection)
    }

    /// Forgets every prompt `connection` owns, after it disconnected.
    pub(super) fn release_prompts_of(&self, connection: u64) {
        lock(&self.live).release_prompts_of(connection);
    }
}

/// Loads the host's Lua providers, including the user's init.lua.
fn merge_plugin_providers(
    registry: &mut ProviderRegistry,
    model_override: Option<&str>,
    system_role: &str,
    sink: &SharedHostLog,
) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let model = model_override.map_or_else(|| crate::default_model(registry), str::to_owned);
    let bare = crate::runtime_env::build_system_prompt(system_role, &cwd, &model, &[], None);
    let Some(project) = Projects::with_log(Arc::clone(sink)).get(
        &cwd,
        &model,
        &|| bare.clone(),
        &Environment::inherit(),
    ) else {
        crate::acp_glue::set_runtime(None);
        return;
    };
    for id in crate::plugins::merge_plugin_providers(&project.runtime, registry) {
        lock(sink).log(
            LogLevel::Warn,
            &format!("plugin provider `{id}` shadows the built-in registration"),
        );
    }
    crate::acp_glue::set_runtime(Some(&project.runtime));
}

/// The model choices `registry` offers a client.
fn model_choices(registry: &ProviderRegistry) -> Arc<[SessionConfigSelectOption]> {
    crate::tui::available_model_items(registry, "")
        .into_iter()
        .map(|item| choice(&item.value, &item.label, item.group.as_deref()))
        .collect()
}

/// The project runtime a session opens with, or `None` when the project
/// has no plugin content or its runtime did not load.
fn project_runtime(
    projects: &Projects,
    workdir: &Path,
    model: &str,
    system_role: &str,
    env: &Environment,
) -> Option<Project> {
    let bare = crate::runtime_env::build_system_prompt(system_role, workdir, model, &[], None);
    projects.get(workdir, model, &|| bare.clone(), env)
}

/// The loop settings a session runs with and the thinking level init.lua or
/// a plugin chose, if one did. The project's options start from the config
/// and then take what init.lua set; with no project, the config's own.
fn project_options(
    project: Option<&Project>,
    config: &Config,
) -> (LoopConfig, Option<kage_core::ThinkingLevel>) {
    let Some(project) = project else {
        // The config's own values, through the option store's
        // validation, so an out-of-range one keeps its default, like a
        // session with plugins sees.
        let (store, _) = kage_core::options::OptionStore::from_config(config);
        let (loop_cfg, _) = crate::tui::startup_options(&Arc::new(Mutex::new(store)));
        let loop_cfg = LoopConfig {
            parallel_tools: false,
            ..loop_cfg
        };
        return (loop_cfg, None);
    };
    let (loop_cfg, thinking) = crate::tui::startup_options(&project.options);
    let from_lua = lock(&project.options).source("thinking_level")
        == Some(kage_core::options::OptionSource::Lua);
    let loop_cfg = LoopConfig {
        parallel_tools: false,
        ..loop_cfg
    };
    (loop_cfg, thinking.filter(|_| from_lua))
}

/// Everything an engine session for `cwd` on `model` runs with, including
/// the client's MCP `servers` and the plugin runtime of its project from
/// `projects`. Its shell commands, stdio servers and plugin processes start
/// with `env`. The caller fills in the history and recorder.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one input of the session the client asked for"
)]
fn session_spec(
    registry: &ProviderRegistry,
    system_role: &str,
    id: SessionId,
    cwd: &str,
    model: &str,
    servers: BTreeMap<String, McpSpec>,
    env: &Environment,
    projects: &Projects,
    plugins: Option<Arc<dyn SessionPlugins>>,
    client_system: Option<&str>,
) -> Result<(SessionSpec, Option<Arc<PluginRuntime>>), RpcError> {
    let workdir = if cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(cwd)
    };
    let sink = projects.sink();
    crate::trust::warn_if_untrusted_with_log(&workdir, sink);
    let config = Config::load_layered(&workdir).map_err(|e| RpcError::internal(e.to_string()))?;
    let model = model.to_owned();
    // The client's own role replaces the host's; what the host adds to
    // it (skills, context) still follows.
    let system_role = client_system.unwrap_or(system_role);
    // The connection's own plugins replace the project's: nothing of
    // the project runtime loads, so its plugin MCP servers and option
    // stores stay out of a session whose plugins live in the client. A
    // project without plugin content builds no runtime either: nothing
    // a session consumes would register in one, so the session runs
    // without plugins.
    let project = plugins
        .is_none()
        .then(|| project_runtime(projects, &workdir, &model, system_role, env))
        .flatten();
    let concrete = project.as_ref().map(|project| Arc::clone(&project.runtime));
    if let Some(runtime) = &concrete {
        runtime.set_session_env(id, env.clone());
    }
    let session_plugins = plugins.map_or_else(
        || concrete.clone().map(|rt| rt as Arc<dyn SessionPlugins>),
        Some,
    );
    let skills = kage_plugin::Invocation::session(id)
        .scope(|| crate::load_skills_with_log(&workdir, session_plugins.as_deref(), sink));
    let shell = config
        .shell
        .program
        .clone()
        .or_else(|| env.var("SHELL")?.into_string().ok());
    let system_prompt = crate::runtime_env::build_system_prompt(
        system_role,
        &workdir,
        &model,
        &skills,
        shell.as_deref(),
    );
    let tools = kage_tools::configured_registry(&config.tools, &config.shell)
        .with_media_tools_log(&config, sink);
    let aliases = tools.alias_map();
    let mcp = crate::mcp::unstarted_with(&workdir, concrete.as_deref(), servers, env.clone(), sink);
    let (defs, agent_errors) = crate::agents::load(&workdir);
    for err in agent_errors {
        lock(sink).log(LogLevel::Warn, &err);
    }
    let agents = AgentSetup::from_config(defs, &config, Background::Hold);
    config
        .permissions
        .validate()
        .map_err(|e| RpcError::internal(format!("permissions: {e}")))?;
    config
        .shell
        .validate()
        .map_err(|e| RpcError::internal(format!("shell: {e}")))?;
    let mut cx = AgentContext::new(model.clone(), &system_prompt).with_workdir(&workdir);
    cx.env = env.clone();
    let (loop_cfg, thinking) = project_options(project.as_ref(), &config);
    if thinking.is_some() {
        cx.thinking_level = thinking;
    }
    if config.permissions.confine_paths {
        cx = cx.with_confine_paths();
    } else {
        cx = cx.without_confine_paths();
        lock(sink).log(
            LogLevel::Warn,
            "path confinement is OFF for this session (permissions.confine_paths = false)",
        );
    }
    if let Some(window) = crate::runtime_env::context_window_for(registry, &model) {
        cx = cx.with_context_window(window);
    }
    let spec = SessionSpec {
        id,
        model,
        cx,
        initial_usage: None,
        recorder: None,
        tools,
        gate: PermissionGate::new(config.permissions)
            .with_log(Arc::clone(sink))
            .with_fallback(PermissionAction::Ask)
            .with_aliases(aliases)
            .with_mcp_servers(mcp.server_names().map(str::to_owned).collect()),
        loop_cfg,
        plugins: session_plugins,
        mcp: Some(mcp),
        interactive: true,
        title: true,
        agents: Some(agents),
        shell: config.shell.program.clone(),
    };
    Ok((spec, concrete))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::permissions::PermissionsConfig;
    use kage_core::protocol::{Envelope, Question, QuestionOption, RequestId};
    use kage_provider::testing::MockProvider;

    use kage_tools::builtin_registry;

    use super::super::options::Settings;
    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    #[derive(Clone, Default)]
    struct RecordedLog(Arc<Mutex<Vec<(LogLevel, String)>>>);

    impl kage_plugin::HostLog for RecordedLog {
        fn notify(&mut self, message: &str) {
            self.log(LogLevel::Info, message);
        }

        fn log(&mut self, level: LogLevel, message: &str) {
            lock(&self.0).push((level, message.to_owned()));
        }
    }

    impl RecordedLog {
        fn sink(&self) -> SharedHostLog {
            Arc::new(Mutex::new(Box::new(self.clone())))
        }

        fn count(&self, text: &str) -> usize {
            lock(&self.0)
                .iter()
                .filter(|(_, message)| message.contains(text))
                .count()
        }
    }

    fn isolated_log_test(name: &str) -> bool {
        if std::env::var("KAGE_HOST_LOG_TEST").as_deref() == Ok(name) {
            return false;
        }
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env("XDG_CACHE_HOME", dir.path().join("cache"))
            .env("KAGE_HOST_LOG_TEST", name)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("lua-print-marker"));
        true
    }

    #[test]
    fn injected_log_covers_provider_and_project_runtime_reloads() {
        if isolated_log_test(
            "rpc::host::tests::injected_log_covers_provider_and_project_runtime_reloads",
        ) {
            return;
        }
        let config_dir = crate::config_dir().unwrap();
        let plugins = config_dir.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(
            &config_path,
            "[plugins]\ndisabled = ['skip']\n[loop]\nmax_provider_retries = 100\n\
             [providers.custom.deepseek]\nbase_url = 'http://127.0.0.1:1/v1'\napi_key_env = ''\n\
             [[providers.custom.deepseek.models]]\nid = 'local'\n",
        )
        .unwrap();
        std::fs::write(plugins.join("broken.lua"), "error('broken-plugin-marker')").unwrap();
        std::fs::write(plugins.join("skip.lua"), "error('must-not-run')").unwrap();
        std::fs::write(
            config_dir.join("init.lua"),
            "kage.notify('runtime-load-marker')\nprint('lua-print-marker')\n\
             kage.register_provider({ id = 'acp', display_name = 'Test', models = {{ id = 'test' }}, \
             stream = function() return {} end })",
        ).unwrap();
        let log = RecordedLog::default();
        let host = Host::start_with_log(Some("acp/test"), "test", false, log.sink()).unwrap();
        assert_eq!(log.count("replaces the catalog provider"), 1);
        assert_eq!(log.count("runtime-load-marker"), 1);
        assert_eq!(log.count("lua-print-marker"), 1);
        assert!(log.count("broken-plugin-marker") > 0);
        assert!(log.count("skip") > 0);
        assert!(log.count("max_provider_retries") > 0);
        assert_eq!(log.count("shadows the built-in registration"), 1);
        host.reload_providers().unwrap();
        host.reload_providers().unwrap();
        assert_eq!(log.count("replaces the catalog provider"), 1);
        assert_eq!(log.count("runtime-load-marker"), 3);
        assert_eq!(log.count("shadows the built-in registration"), 3);
        let workdir = std::env::current_dir().unwrap();
        let open = || {
            (host.spec)(
                &host.registry(),
                SessionId::new(),
                workdir.to_str().unwrap(),
                "acp/test",
                BTreeMap::new(),
                &Environment::inherit(),
                None,
                None,
            )
            .unwrap()
        };
        let (_, first) = open();
        let (_, cached) = open();
        assert!(Arc::ptr_eq(
            first.as_ref().unwrap(),
            cached.as_ref().unwrap()
        ));
        assert_eq!(log.count("runtime-load-marker"), 4);
        std::fs::write(
            plugins.join("broken.lua"),
            "error('changed-broken-plugin-marker')",
        )
        .unwrap();
        thread::sleep(Duration::from_millis(1100));
        let (_, fresh) = open();
        assert!(!Arc::ptr_eq(
            first.as_ref().unwrap(),
            fresh.as_ref().unwrap()
        ));
        assert_eq!(log.count("runtime-load-marker"), 5);
        assert!(log.count("changed-broken-plugin-marker") > 0);
        std::fs::write(&config_path, "[broken").unwrap();
        host.config_changed(&[config_path]);
        assert!(log.count("providers not reloaded") > 0);
        host.shutdown();
    }

    #[test]
    fn injected_log_covers_missing_credentials_and_runtime_fallback_errors() {
        if isolated_log_test(
            "rpc::host::tests::injected_log_covers_missing_credentials_and_runtime_fallback_errors",
        ) {
            return;
        }
        let log = RecordedLog::default();
        let host = Host::start_with_log(None, "test", false, log.sink()).unwrap();
        assert_eq!(log.count("no provider credentials found"), 1);
        host.shutdown();
        drop(host);
        let config_dir = crate::config_dir().unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), "[broken").unwrap();
        std::fs::write(config_dir.join("init.lua"), "error('init-error-marker')").unwrap();
        std::fs::write(config_dir.join("plugins"), "not a directory").unwrap();
        let projects = Projects::with_log(log.sink());
        let workdir = std::env::current_dir().unwrap();
        for _ in 0..2 {
            assert!(
                projects
                    .get(&workdir, "acp/test", &String::new, &Environment::inherit())
                    .is_none()
            );
        }
        assert_eq!(log.count("loading with the defaults"), 2);
        assert_eq!(log.count("plugin load:"), 2);
        std::fs::remove_file(config_dir.join("plugins")).unwrap();
        std::fs::create_dir(config_dir.join("plugins")).unwrap();
        let project = projects
            .get(&workdir, "acp/test", &String::new, &Environment::inherit())
            .unwrap();
        assert!(Arc::ptr_eq(&project.runtime.sink(), projects.sink()));
        assert_eq!(log.count("loading with the defaults"), 3);
        assert!(log.count("init-error-marker") > 0);
        assert!(
            lock(&log.0)
                .iter()
                .any(|(level, message)| *level == LogLevel::Error
                    && message.contains("plugin load:"))
        );
    }

    #[test]
    fn an_out_of_range_loop_config_keeps_its_default_without_a_runtime() {
        let mut config = Config::default();
        config.loop_settings.max_provider_retries = 1_000_000;
        let (loop_cfg, thinking) = project_options(None, &config);
        assert_eq!(loop_cfg.max_provider_retries, 4);
        assert_eq!(thinking, None);
    }

    #[test]
    fn an_in_range_loop_config_reaches_the_session_without_a_runtime() {
        let mut config = Config::default();
        config.loop_settings.max_provider_retries = 7;
        let (loop_cfg, thinking) = project_options(None, &config);
        assert_eq!(loop_cfg.max_provider_retries, 7);
        assert_eq!(thinking, None);
    }

    #[test]
    fn compaction_reserve_config_reaches_the_session_without_a_runtime() {
        for reserve in [0, 24_000, u32::MAX] {
            let mut config = Config::default();
            config.loop_settings.compaction_reserve_tokens = reserve;
            let (loop_cfg, _) = project_options(None, &config);
            assert_eq!(loop_cfg.compaction_reserve_tokens, reserve);
        }
    }

    /// A host with no plugins and a plain session spec on the mock.
    fn host_in(dir: &tempfile::TempDir) -> Arc<Host> {
        let registry =
            Arc::new(ProviderRegistry::new().with(Arc::new(MockProvider::sequence(Vec::new()))));
        let spec: SpecBuilder = Box::new(|_, id, cwd, _, _, _, _, _| {
            Ok((
                SessionSpec {
                    id,
                    model: "mock/m".to_owned(),
                    cx: AgentContext::new("mock/m", "").with_workdir(cwd),
                    initial_usage: None,
                    recorder: None,
                    tools: builtin_registry(),
                    gate: PermissionGate::new(PermissionsConfig::default()),
                    loop_cfg: LoopConfig::default(),
                    plugins: None,
                    mcp: None,
                    interactive: true,
                    title: true,
                    agents: None,
                    shell: None,
                },
                None,
            ))
        });
        Host::new(registry, "mock/m".into(), dir.path().to_path_buf(), spec)
    }

    /// A client that attaches while a background agent runs learns it
    /// is one, so its prompts do not wait for that agent.
    #[test]
    fn an_attach_snapshot_keeps_an_agent_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(&dir);
        let root = SessionId::new();
        let child = SessionId::new();
        lock(&host.live).observe(&Envelope {
            session: child,
            seq: 0,
            event: HostEvent::AgentSpawned {
                parent: root,
                tool_call_id: kage_core::ToolCallId::new("call-1"),
                agent: "general".to_owned(),
                description: "watch".to_owned(),
                swarm: None,
                background: true,
            }
            .into(),
        });
        let snapshot = lock(&host.live).snapshot(root);
        assert!(matches!(
            snapshot.spawns[0].envelope().event,
            kage_core::protocol::Event::Host(HostEvent::AgentSpawned {
                background: true,
                ..
            })
        ));
    }

    #[test]
    fn a_released_session_prunes_the_host_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(&dir);
        let id = SessionId::new();
        let (spec, runtime) = (host.spec)(
            &host.registry(),
            id,
            dir.path().to_str().unwrap(),
            "mock/m",
            BTreeMap::new(),
            &Environment::inherit(),
            None,
            None,
        )
        .unwrap();
        let settings = Settings::of(&spec, &host.registry());
        host.launch(spec, settings, runtime);

        assert!(host.open_settings(id).is_some());
        assert!(host.workdir(id).is_some());

        host.attach(id, false);
        host.release(id);
        assert!(
            host.open_settings(id).is_some() && host.workdir(id).is_some(),
            "one attachment is left"
        );

        host.release(id);
        let deadline = std::time::Instant::now() + WAIT;
        while std::time::Instant::now() < deadline && host.workdir(id).is_some() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(host.workdir(id).is_none(), "the workdir is pruned");
        assert!(
            host.open_settings(id).is_none(),
            "the open record is pruned"
        );
        host.shutdown();
    }

    /// Launches a mock session in `dir` and folds one event into the
    /// host's live view of it.
    fn launched_with(host: &Host, id: SessionId, dir: &tempfile::TempDir, event: HostEvent) {
        let (spec, runtime) = (host.spec)(
            &host.registry(),
            id,
            dir.path().to_str().unwrap(),
            "mock/m",
            BTreeMap::new(),
            &Environment::inherit(),
            None,
            None,
        )
        .unwrap();
        let settings = Settings::of(&spec, &host.registry());
        host.launch(spec, settings, runtime);
        lock(&host.live).observe(&Envelope {
            session: id,
            seq: 1,
            event: Event::Host(event),
        });
    }

    /// A question the engine would wait on, under `request_id`.
    fn question_event(request_id: u64) -> HostEvent {
        HostEvent::QuestionAsked {
            request_id: RequestId(request_id),
            tool_call_id: None,
            questions: vec![Question {
                header: "Auth".to_owned(),
                question: "Which way?".to_owned(),
                options: vec![
                    QuestionOption {
                        label: "key".to_owned(),
                        description: String::new(),
                    },
                    QuestionOption {
                        label: "none".to_owned(),
                        description: String::new(),
                    },
                ],
                multi_select: false,
            }],
        }
    }

    #[test]
    fn an_ask_on_a_clientless_session_expires_after_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(&dir);
        let id = SessionId::new();
        launched_with(&host, id, &dir, question_event(1));

        // The ask holds the session: releasing it stamps the moment
        // instead of closing, and the stamp counts from release.
        // Launching counts as the first attachment, so this release is
        // the last client leaving.
        host.release(id);
        assert!(host.open_settings(id).is_some(), "the ask holds it open");
        let live = lock(&host.live);
        assert!(live.expired_ask_roots(Duration::ZERO).contains(&id));
        assert!(
            !live
                .expired_ask_roots(Duration::from_secs(3600))
                .contains(&id),
            "a fresh stamp outlives the cap"
        );
        drop(live);

        // A client coming back clears the moment.
        host.attach(id, false);
        assert!(
            lock(&host.live)
                .expired_ask_roots(Duration::ZERO)
                .is_empty()
        );

        // Past the cap the reaper declines the ask and forgets the
        // session.
        host.release(id);
        assert!(
            lock(&host.live)
                .expired_ask_roots(Duration::ZERO)
                .contains(&id)
        );
        host.decline_expired_asks(Duration::ZERO);
        assert!(
            lock(&host.live)
                .expired_ask_roots(Duration::ZERO)
                .is_empty()
        );
        host.shutdown();
    }

    #[test]
    fn a_resolved_ask_stops_expiring_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(&dir);
        let id = SessionId::new();
        launched_with(&host, id, &dir, question_event(2));
        host.release(id);
        assert!(
            lock(&host.live)
                .expired_ask_roots(Duration::ZERO)
                .contains(&id)
        );

        lock(&host.live).observe(&Envelope {
            session: id,
            seq: 2,
            event: Event::Host(HostEvent::QuestionClosed {
                request_id: RequestId(2),
            }),
        });
        // The answered ask lets the session close instead of sitting
        // orphaned: the stamp is gone and nothing holds it open.
        assert!(host.open_settings(id).is_none(), "the session closes");
        assert!(
            lock(&host.live)
                .expired_ask_roots(Duration::ZERO)
                .is_empty()
        );
        host.shutdown();
    }

    #[test]
    fn an_ask_on_a_clientless_session_stamps_a_fresh_window() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(&dir);
        let id = SessionId::new();
        // A session released with nothing open closes, so the ask that
        // follows reaches a clientless session with no stamp of its
        // own: only the track_ask stamping can hold it.
        launched_with(&host, id, &dir, HostEvent::RunStarted);
        host.release(id);
        lock(&host.live).observe(&Envelope {
            session: id,
            seq: 2,
            event: Event::Host(question_event(4)),
        });
        assert!(
            lock(&host.live)
                .expired_ask_roots(Duration::ZERO)
                .contains(&id)
        );
        assert!(
            !lock(&host.live)
                .expired_ask_roots(Duration::from_secs(3600))
                .contains(&id),
            "a fresh ask outlives the cap"
        );
        host.shutdown();
    }

    #[test]
    fn a_new_ask_starts_its_own_window() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(&dir);
        let id = SessionId::new();
        launched_with(&host, id, &dir, question_event(5));
        host.release(id);
        let released = lock(&host.live).orphan_stamp(id).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        lock(&host.live).observe(&Envelope {
            session: id,
            seq: 2,
            event: Event::Host(question_event(6)),
        });
        let asked = lock(&host.live).orphan_stamp(id).unwrap();
        assert!(asked > released, "the new ask restarted the window");
        host.shutdown();
    }

    #[test]
    fn expired_asks_are_taken_once_and_not_from_a_client_that_came_back() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(&dir);
        let id = SessionId::new();
        launched_with(&host, id, &dir, question_event(7));
        host.release(id);

        host.attach(id, false);
        assert!(
            lock(&host.live)
                .take_expired_asks(Duration::ZERO)
                .is_empty(),
            "an attached session keeps its asks"
        );

        host.release(id);
        let first = lock(&host.live).take_expired_asks(Duration::ZERO);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].session, id);
        // The engine's state lands after the take and orphans the
        // session again, before the decline closes the ask.
        lock(&host.live).observe(&Envelope {
            session: id,
            seq: 9,
            event: Event::Host(HostEvent::StateChanged {
                state: kage_core::protocol::SessionState::default(),
            }),
        });
        assert!(
            lock(&host.live)
                .take_expired_asks(Duration::ZERO)
                .is_empty(),
            "a second housekeeping thread gets nothing"
        );
        host.shutdown();
    }
}
