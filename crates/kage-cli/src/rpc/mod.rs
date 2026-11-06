//! `kage rpc`: a spec-conformant Agent Client Protocol agent.
//!
//! Speaks ACP (newline-delimited JSON-RPC 2.0 over stdio, protocol
//! version 1) so editors that speak ACP can drive kage. Every connection
//! is served on one [`Host`], whose engine it shares with the other
//! connections, and every ACP session is an engine session: prompts
//! become engine commands, and a per-connection bridge turns engine
//! events into `session/update` notifications and
//! `session/request_permission` requests. A session another connection
//! has open is attached to instead of reopened: the attaching client
//! gets the recorded transcript plus the turn in flight, the title and
//! the open permission asks, all under one bus lock so nothing arrives
//! twice. A session no connection holds any more is closed once idle.
//! Recorded sessions can be listed, loaded with a replay of their
//! transcript, released with `session/close`, or resumed. Each session
//! offers its model, thinking level and permission mode as config
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
pub(crate) mod client_plugins;
pub(crate) mod config_set;
mod content;
mod directory;
// Retained for future transport work, but external session creation is disabled.
#[allow(dead_code)]
mod external;
mod folders;
mod fs;
pub(crate) mod host;
mod live;
mod mcp;
mod models;
mod options;
mod plugin_files;
mod plugin_registry;
pub(crate) mod probe;
mod projects;
mod registry;
mod rewind;
mod sessions;
mod themes;
mod ui;
#[cfg(unix)]
#[allow(dead_code)]
mod unix_stdio;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufReader;
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use kage_acp::acp::{
    AgentCapabilities, AgentMeta, AuthSetRequest, CloseSessionRequest, CloseSessionResponse,
    ConfigGetRequest, ConfigGetResult, ConfigSetRequest, ConfigTestRequest, ConfigTestResult,
    DirectoryRequest, DirectoryResult, FoldersRequest, FoldersResult, FsRequest, FsResult,
    ImageSaveRequest, Implementation, InitializeRequest, InitializeResponse, InstalledPlugin,
    KageAgentInfo, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, McpCapabilities, McpRestartRequest, ModelsResponse, NewSessionMeta,
    NewSessionRequest, NewSessionResponse, OptionSetRequest, OptionsResponse, PROTOCOL_VERSION,
    PluginInstallRequest, PluginRemoveRequest, PluginsSource, PromptCapabilities, PromptDelivery,
    PromptRequest, PromptResponse, PromptWithdrawn, RegistryInstallRequest, RegistryInstallResult,
    RegistrySearchRequest, RegistrySearchResult, ResumeSessionRequest, ResumeSessionResponse,
    SessionBackground, SessionCapabilities, SessionConfigOption, SessionExportResponse,
    SessionForkRequest, SessionForkResponse, SessionOpt, SessionPromptsResponse,
    SessionRenameRequest, SessionRequest, SessionRewindRequest, SessionUndoResponse,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, ShellRequest, StopReason,
    Supported, SwarmResumeRequest, SwarmResumeResponse, ThemesRequest, ThemesResult,
    WithdrawRequest,
};
use kage_acp::acp::{UiAttachRequest, UiCommandRequest, UiKeymapRequest, UiOutput, UiSnapshot};
use kage_acp::agent::{Agent, AgentPeer, PromptContext};
use kage_core::config::{Config, McpServer as McpSpec, PluginsConfig};
use kage_core::environment::Environment;
use kage_core::options::OptionValue;

/// How large a client's environment may be when it announces itself in
/// `_kage/environment`. A larger one is not sent, and that client's
/// sessions use the host's own.
pub(crate) const MAX_ENVIRONMENT_BYTES: usize = 1024 * 1024;

/// This process's environment, UTF-8 variables only. Variables that
/// are not UTF-8 are left out.
pub(crate) fn environment_vars() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{AgentTree, Command, CommandKind, Delivery, RunOutcome};
use kage_core::sync::lock;
use kage_core::{LoopError, SessionId, StopReason as CoreStopReason, ThinkingLevel};
use kage_jsonrpc::RpcError;
use kage_plugin::PluginRuntime;
use kage_plugin::session_plugins::SessionPlugins;

use bridge::{Ask, AskSet, Bridge, HeldUpdates};
use content::prompt_content;
use host::Host;
use live::Seed;
use mcp::editor_servers;
use options::{Changed, Settings, Shown, config_options};
use sessions::list_page;

use crate::engine::{AUTO_THINKING, Background, Recorder, SessionSpec, SubscriptionId};

/// What a redacted config value reads as.
const REDACTED: &str = "<redacted>";

/// What the connection asked one session to run with, beyond what the
/// host defaults to: the `session/new` `_meta.kage` settings a kage
/// client sets for itself. Unset fields keep the host's own reading.
#[derive(Clone, Default)]
pub(crate) struct SessionSettings {
    /// The system role the session starts with, replacing the host's.
    /// Skills and context the host adds still follow it.
    system_prompt: Option<String>,
    /// How the session treats a background agent's result while it is
    /// idle.
    background: Option<Background>,
    /// The session opens in allow mode: tools run without asking.
    yolo: bool,
    /// Loop settings and the thinking level, from the client's option
    /// store.
    opt: Option<SessionOpt>,
    /// The most tokens one provider answer may carry.
    max_output_tokens: Option<u32>,
}

impl SessionSettings {
    /// What a `session/new`'s `_meta.kage` asked for, on top of
    /// [`Self::host_defaults`].
    ///
    /// # Errors
    ///
    /// `-32602` when a value is out of range: a compaction threshold
    /// outside the option's bounds, retries outside the option's range,
    /// or a thinking level that is neither `auto` nor a known level.
    fn of(meta: Option<&NewSessionMeta>, kage_client: bool) -> Result<Self, RpcError> {
        let kage = meta.and_then(|meta| meta.kage.as_ref());
        let defaults = Self::host_defaults(kage_client);
        let opt = kage.and_then(|kage| kage.opt.clone());
        if let Some(opt) = &opt {
            validate_opt(opt)?;
        }
        Ok(Self {
            system_prompt: kage.and_then(|kage| kage.system_prompt.clone()),
            background: kage
                .and_then(|kage| kage.background)
                .map(|background| match background {
                    SessionBackground::Wake => Background::Wake,
                    SessionBackground::Hold => Background::Hold,
                })
                .or(defaults.background),
            yolo: kage.is_some_and(|kage| kage.yolo),
            opt,
            max_output_tokens: kage.and_then(|kage| kage.max_output_tokens),
        })
    }

    /// What a session the wire names no settings for runs with: the
    /// host's own defaults, with a kage client's background agents
    /// waking it. The one place the background default is derived.
    fn host_defaults(kage_client: bool) -> Self {
        Self {
            background: Some(if kage_client {
                Background::Wake
            } else {
                Background::Hold
            }),
            ..Self::default()
        }
    }

    /// Applies what acts on the built spec: the background agents, the
    /// allow mode, the loop settings, the thinking level and the
    /// output limit. The system prompt rode along into the spec build.
    fn apply(&self, spec: &mut SessionSpec) {
        if let Some(agents) = spec.agents.as_mut() {
            agents.background = self.background.unwrap_or(Background::Hold);
        }
        if self.yolo {
            spec.gate.set_mode(Some(PermissionAction::Allow));
        }
        if let Some(opt) = &self.opt {
            if let Some(threshold) = opt.compaction_threshold {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the threshold is a fraction that f32 holds"
                )]
                {
                    spec.loop_cfg.compaction_threshold = threshold as f32;
                }
            }
            if let Some(retries) = opt.max_provider_retries {
                spec.loop_cfg.max_provider_retries = u32::try_from(retries).unwrap_or(0);
            }
            if let Some(level) = opt.thinking_level.as_deref().and_then(ThinkingLevel::parse) {
                spec.cx.thinking_level = Some(level);
            }
        }
        if let Some(tokens) = self.max_output_tokens {
            spec.cx.max_output_tokens = Some(tokens);
        }
    }
}

/// Checks the client's loop settings against the bounds the option
/// store enforces for the same settings.
fn validate_opt(opt: &SessionOpt) -> Result<(), RpcError> {
    let invalid = |detail: String| RpcError::new(-32602, detail);
    if let Some(threshold) = opt.compaction_threshold {
        let def = kage_core::options::lookup("compaction_threshold")
            .map_err(|e| RpcError::internal(e.to_string()))?;
        def.validate(OptionValue::Float(threshold))
            .map_err(|e| invalid(format!("opt.compactionThreshold: {e}")))?;
    }
    if let Some(retries) = opt.max_provider_retries {
        let def = kage_core::options::lookup("max_provider_retries")
            .map_err(|e| RpcError::internal(e.to_string()))?;
        def.validate(OptionValue::Int(retries))
            .map_err(|e| invalid(format!("opt.maxProviderRetries: {e}")))?;
    }
    if let Some(level) = opt.thinking_level.as_deref()
        && level != "auto"
        && level != AUTO_THINKING
        && ThinkingLevel::parse(level).is_none()
    {
        return Err(invalid(format!(
            "opt.thinkingLevel: unknown level `{level}`"
        )));
    }
    Ok(())
}

/// The plugins in `dir`, scripts and packages, by name, each marked
/// with whether `plugins` lets it load, its version and whether it came
/// from a registry. A missing or unreadable directory has none, and an
/// entry that cannot load is left out (the load reports it).
fn installed_plugins(dir: &Path, plugins: &PluginsConfig) -> Vec<InstalledPlugin> {
    let mut installed: Vec<InstalledPlugin> = kage_kura::status(dir, None)
        .unwrap_or_default()
        .into_iter()
        .map(|status| InstalledPlugin {
            enabled: plugins.loads(&status.name),
            version: status.version.map(|v| v.to_string()),
            from_registry: status.locked.is_some(),
            name: status.name,
        })
        .collect();
    installed.sort_by(|a, b| a.name.cmp(&b.name));
    installed
}

/// Blanks the config values that may carry credentials before they leave
/// the process: provider and MCP header values, MCP environment values
/// and plugin settings. The names stay, so a client can still say what
/// is configured.
fn redact_secrets(config: &mut Config) {
    let blank = |map: &mut BTreeMap<String, String>| {
        for value in map.values_mut() {
            REDACTED.clone_into(value);
        }
    };
    for provider in config.providers.custom.values_mut() {
        blank(&mut provider.headers);
    }
    for provider in config.providers.overrides.values_mut() {
        blank(&mut provider.headers);
    }
    for server in config.mcp.servers.values_mut() {
        blank(&mut server.headers);
        blank(&mut server.env);
    }
    for agent in config.acp.agents.values_mut() {
        blank(&mut agent.env);
    }
    for settings in config.plugins.config.values_mut() {
        *settings = serde_json::Value::String(REDACTED.to_owned());
    }
}

/// Entry point for the `Rpc` subcommand.
pub(crate) fn run(
    model_override: Option<&str>,
    system_role: &str,
    system_file: Option<&Path>,
    yolo: bool,
) -> ExitCode {
    let system_role = match resolve_system(system_role, system_file) {
        Ok(system_role) => system_role,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    kage_core::reaper::start();
    let served = Host::start(model_override, &system_role, yolo)
        .map_err(|e| format!("rpc: {e}"))
        .and_then(|host| {
            host.serve(BufReader::new(std::io::stdin()), std::io::stdout())
                .map_err(|e| format!("rpc: {e}"))
        });
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: {e}");
            ExitCode::from(1)
        }
    }
}

/// The system prompt a run starts with: the inline text, with the
/// contents of `file` appended when one was given.
///
/// The file exists so a caller can keep a long or private prompt out of
/// its own command line, which `/proc/<pid>/cmdline` publishes to every
/// user on the host. Reading it once here rather than per session gives
/// the failure one path: a file that cannot be read stops the run,
/// instead of starting sessions that quietly lack their instructions.
fn resolve_system(inline: &str, file: Option<&Path>) -> Result<String, String> {
    let Some(file) = file else {
        return Ok(inline.to_owned());
    };
    let appended = std::fs::read_to_string(file)
        .map_err(|e| format!("cannot read the system prompt at {}: {e}", file.display()))?;
    let appended = appended.trim_end();
    match (inline.trim().is_empty(), appended.is_empty()) {
        (true, _) => Ok(appended.to_owned()),
        (false, true) => Ok(inline.to_owned()),
        (false, false) => Ok(format!("{inline}\n\n{appended}")),
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
    /// Agent sessions the client loaded to read. The engine never
    /// hosts them for the client, so they take no prompts.
    read_only: HashSet<String>,
    external: HashSet<String>,
}

impl Ids {
    fn insert(&mut self, client: String, engine: SessionId) {
        self.by_engine.insert(engine, client.clone());
        self.by_client.insert(client, engine);
    }

    /// Forgets a client session, after the connection released it.
    fn remove_client(&mut self, client: &str) {
        if let Some(engine) = self.by_client.remove(client) {
            self.by_engine.remove(&engine);
        }
    }
}

/// How a prompt's run ended, handed from the bridge to every waiting
/// prompt. Several prompts can wait on one run: one running and the
/// prompts steering or queuing behind it.
#[derive(Clone)]
struct PromptEnd {
    outcome: RunOutcome,
    stop: Option<CoreStopReason>,
}

struct PreparedExternalPrompt {
    session: Arc<external::External>,
    ticket: external::PromptTicket,
}

type Waiters = Arc<Mutex<HashMap<SessionId, Vec<mpsc::Sender<PromptEnd>>>>>;

type ShownBySession = Arc<Mutex<HashMap<SessionId, Shown>>>;

/// Updates for client sessions whose opening response is not written
/// yet, capped per session at [`bridge::HELD_CAP`]; later updates are
/// dropped and counted. Sending them earlier would reach the client
/// before it knows the session.
type Held = bridge::HeldMap;

/// Stops the updates of the plugin UI this connection draws, with the
/// highest attachment id it processed: an attach naming an older id
/// belongs to a watcher already replaced, or to one still being
/// resolved, and neither may follow the newest.
struct UiWatch {
    stop: Arc<AtomicBool>,
    high: u64,
}

/// The ACP agent one connection on the host is served through. Holds the
/// per-connection maps; the engine and the session setup live in the
/// host every connection shares.
struct CliAcpAgent {
    host: Arc<Host>,
    /// This connection's id on the host.
    connection: u64,
    disconnecting: AtomicBool,
    ids: Arc<Mutex<Ids>>,
    waiters: Waiters,
    shown: ShownBySession,
    subagents: Arc<AtomicBool>,
    /// Whether the client asked for tools without a `[permissions]`
    /// rule to run, as in the TUI, instead of asking first.
    unconfigured_run: AtomicBool,
    /// Whether the client is a kage client, which shows runs it did not
    /// prompt, so an idle session wakes for a background agent's result.
    kage_client: Arc<AtomicBool>,
    /// Whether the client hosts session plugins and answers the
    /// `_kage/plugin/*` reverse requests for them.
    plugin_host: AtomicBool,
    /// The environment the client sent, which sessions it opens from now
    /// on start their processes with. The host's own until it sends one.
    env: Mutex<Environment>,
    /// Whether this connection may send its environment: only a local
    /// client of this user, never a remote one.
    takes_env: AtomicBool,
    /// The plugin UI watcher this connection follows, with the highest
    /// attachment id it accepted. A connection follows one watcher;
    /// attaching again stops the first.
    ui_watch: Mutex<UiWatch>,
    peer: AgentPeer,
    held: Held,
    asks: AskSet,
    subscription: SubscriptionId,
    /// Attaches waiting to be applied by the bridge.
    seeds: Arc<Mutex<Vec<Seed>>>,
}

impl CliAcpAgent {
    /// The agent for one connection on `host`.
    fn new(host: Arc<Host>, peer: AgentPeer) -> Self {
        let connection = host.next_connection();
        let ids = Arc::new(Mutex::new(Ids::default()));
        let waiters = Waiters::default();
        let shown = ShownBySession::default();
        let subagents = Arc::new(AtomicBool::new(false));
        let kage_client = Arc::new(AtomicBool::new(false));
        let held = Held::default();
        let asks = AskSet::default();
        let seeds = Arc::default();
        let mut bridge = Bridge {
            peer: peer.clone(),
            commander: host.engine.commander(),
            connection,
            live: Arc::clone(&host.live),
            ids: Arc::clone(&ids),
            waiters: Arc::clone(&waiters),
            models: host.models(),
            shown: Arc::clone(&shown),
            aliases: host.aliases.clone(),
            seen: HashMap::new(),
            stops: HashMap::new(),
            asks: Arc::clone(&asks),
            tree: AgentTree::default(),
            subagents: Arc::clone(&subagents),
            kage_client: Arc::clone(&kage_client),
            streaming: HashSet::new(),
            ended: HashMap::new(),
            commands: HashMap::new(),
            modes: HashMap::new(),
            names: HashMap::new(),
            statuses: HashMap::new(),
            fills: HashMap::new(),
            pruned_cost: HashMap::new(),
            compacting: HashMap::new(),
            compacting_started: HashSet::new(),
            paused: HashSet::new(),
            held: Arc::clone(&held),
            approving: HashMap::new(),
            seeds: Arc::clone(&seeds),
        };
        let subscription = host
            .engine
            .subscribe(Box::new(move |envelope| bridge.handle(envelope)));
        Self {
            host,
            connection,
            disconnecting: AtomicBool::new(false),
            ids,
            waiters,
            shown,
            subagents,
            unconfigured_run: AtomicBool::new(false),
            plugin_host: AtomicBool::new(false),
            kage_client,
            env: Mutex::new(Environment::inherit()),
            takes_env: AtomicBool::new(false),
            ui_watch: Mutex::new(UiWatch {
                stop: Arc::new(AtomicBool::new(false)),
                high: 0,
            }),
            peer,
            held,
            asks,
            subscription,
            seeds,
        }
    }

    /// The runtime of the project at `cwd`, and the invocation of work this
    /// connection asks for in its session `client_session`.
    fn ui_context(
        &self,
        cwd: &str,
        client_session: Option<&str>,
    ) -> Result<(Arc<kage_plugin::PluginRuntime>, kage_plugin::Invocation), RpcError> {
        let invocation = |session| kage_plugin::Invocation {
            session,
            client: Some(self.connection),
        };
        if let Some(client_session) = client_session {
            let id = self.engine_id(client_session)?;
            let runtime = self
                .host
                .session_runtime(id)
                .ok_or_else(|| RpcError::new(-32602, "the session runs without plugins"))?;
            return Ok((runtime, invocation(Some(id))));
        }
        let env = lock(&self.env).clone();
        let cwd = Path::new(cwd);
        let project = match self.host.project(cwd, &env) {
            Some(project) => project,
            None if !self.host.project_has_content(cwd) => {
                return Err(RpcError::new(
                    -32602,
                    format!("the project at {} has no plugins", cwd.display()),
                ));
            }
            None => {
                return Err(RpcError::internal("the project's plugins did not load"));
            }
        };
        Ok((project.runtime, invocation(None)))
    }

    /// Accept the client's `_kage/environment` on this connection.
    pub(super) fn take_environment(&self) {
        self.takes_env.store(true, Ordering::SeqCst);
    }

    /// The config options a new session opens with: the default model,
    /// its automatic thinking level and no permission mode.
    fn default_options(&self) -> Vec<SessionConfigOption> {
        let settings = Settings::fresh(&self.host.default_model, &self.host.registry());
        config_options(&self.host.models(), &settings)
    }

    /// The session plugins a `session/new` asks for: a proxy over the
    /// client's own plugin host when it names the `client` source. A
    /// client that did not advertise the capability gets refused.
    fn hosted_plugins(
        &self,
        id: SessionId,
        meta: Option<&NewSessionMeta>,
    ) -> Result<Option<Arc<dyn SessionPlugins>>, RpcError> {
        let source = meta
            .and_then(|meta| meta.kage.as_ref())
            .and_then(|kage| kage.plugins);
        if source == Some(PluginsSource::Client) && !self.plugin_host.load(Ordering::SeqCst) {
            return Err(RpcError::new(
                -32602,
                "client-hosted plugins need the pluginHost capability",
            ));
        }
        match source {
            Some(PluginsSource::Client) => Ok(Some(Arc::new(client_plugins::ClientPlugins::open(
                id,
                self.peer.clone(),
            )?) as Arc<dyn SessionPlugins>)),
            None => Ok(None),
        }
    }

    /// What a session of this connection runs with: the host's spec,
    /// with the permission fallback the client asked for at
    /// `initialize`, and the settings its `session/new` asked for.
    /// Agents the session starts inherit its gate.
    fn session_spec(
        &self,
        id: SessionId,
        cwd: &str,
        model: &str,
        servers: BTreeMap<String, McpSpec>,
        plugins: Option<Arc<dyn SessionPlugins>>,
        settings: &SessionSettings,
    ) -> Result<(SessionSpec, Option<Arc<PluginRuntime>>), RpcError> {
        let env = lock(&self.env).clone();
        let (mut spec, runtime) = (self.host.spec)(
            &self.host.registry(),
            id,
            cwd,
            model,
            servers,
            &env,
            plugins,
            settings.system_prompt.as_deref(),
        )?;
        if self.unconfigured_run.load(Ordering::SeqCst) {
            spec.gate = spec.gate.with_fallback(PermissionAction::Allow);
        }
        settings.apply(&mut spec);
        // A concrete runtime always stands in the spec. The reverse,
        // plugins without a concrete runtime, is the client-hosted
        // case: the spec carries the client's proxy and there is no
        // project runtime to keep.
        debug_assert!(
            runtime.as_ref().is_none_or(|runtime| {
                spec.plugins.as_ref().is_some_and(|plugins| {
                    std::ptr::addr_eq(Arc::as_ptr(plugins), Arc::as_ptr(runtime))
                })
            }),
            "a spec's runtime is the one its plugins are"
        );
        Ok((spec, runtime))
    }

    /// Opens `spec` as the client session `client_id` and returns its
    /// config options. Updates for it wait for
    /// [`Agent::session_announced`].
    fn open(
        &self,
        client_id: String,
        spec: SessionSpec,
        runtime: Option<Arc<PluginRuntime>>,
    ) -> Vec<SessionConfigOption> {
        let settings = Settings::of(&spec, &self.host.registry());
        let options = config_options(&self.host.models(), &settings);
        let shown = Shown {
            settings: settings.clone(),
            catching_up: false,
            changed: Changed::default(),
            swallowed: 0,
        };
        lock(&self.shown).insert(spec.id, shown);
        lock(&self.held).insert(spec.id, HeldUpdates::default());
        lock(&self.ids).insert(client_id, spec.id);
        self.host.launch(spec, settings, runtime);
        options
    }

    /// The layered config for the request's session workdir, or for the
    /// server's directory when the request names no known session.
    /// Read without validation: this serves diagnostics and the
    /// settings pages, so a broken config stays readable and fixable
    /// through the RPC surface.
    fn load_config(&self, req: &ConfigGetRequest) -> Result<Config, RpcError> {
        let workdir = req
            .session_id
            .as_deref()
            .and_then(|client_id| self.engine_id(client_id).ok())
            .and_then(|id| self.host.workdir(id));
        let dir = workdir.unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        });
        Config::load_layered_raw(&dir).map_err(|e| RpcError::internal(e.to_string()))
    }

    fn engine_id(&self, client_id: &str) -> Result<SessionId, RpcError> {
        lock(&self.ids)
            .by_client
            .get(client_id)
            .copied()
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {client_id}")))
    }

    fn external_session(&self, id: &str) -> Option<Arc<external::External>> {
        let sessions = lock(&self.host.external);
        if !lock(&self.ids).external.contains(id) {
            return None;
        }
        sessions.get(id).cloned()
    }

    fn release_external(&self, id: &str) -> bool {
        let detached = {
            // Always acquire the host map before the connection ids, as in attach.
            let mut sessions = lock(&self.host.external);
            let mut ids = lock(&self.ids);
            if !ids.external.remove(id) {
                return false;
            }
            sessions.get(id).cloned().map(|session| {
                let (last, cancel) = session.detach_mark(self.connection);
                if last {
                    sessions.remove(id);
                }
                (session, last, cancel)
            })
        };
        if let Some((session, last, cancel)) = detached {
            session.finish_detach(last, cancel);
        }
        true
    }

    fn attach_external(
        &self,
        id: &str,
        servers: &[kage_acp::acp::McpServer],
    ) -> Result<bool, RpcError> {
        let sessions = lock(&self.host.external);
        let Some(session) = sessions.get(id) else {
            return Ok(false);
        };
        if !servers.is_empty() {
            return Err(RpcError::new(
                -32602,
                "external sessions do not support downstream MCP servers",
            ));
        }
        let mut ids = lock(&self.ids);
        if self.disconnecting.load(Ordering::SeqCst) {
            return Err(RpcError::internal("client is disconnecting"));
        }
        session.attach(self.connection, self.peer.clone())?;
        ids.external.insert(id.to_owned());
        Ok(true)
    }

    /// The connection ended: stop delivering engine events to it, free
    /// what its prompts wait on, withdraw its open asks, and release
    /// every session it held. An ask whose session no other connection
    /// holds is denied, so the run goes on instead of waiting for a
    /// client that may never come back. The engine and its runs keep
    /// going for the connections that stay.
    fn disconnect_external(&self) {
        self.disconnecting.store(true, Ordering::SeqCst);
        let external: Vec<_> = lock(&self.ids).external.iter().cloned().collect();
        for id in external {
            self.release_external(&id);
        }
    }

    fn detach(&self) {
        lock(&self.ui_watch).stop.store(true, Ordering::SeqCst);
        self.disconnect_external();
        self.host.engine.unsubscribe(self.subscription);
        self.host.release_prompts_of(self.connection);
        lock(&self.waiters).clear();
        let asks: Vec<(SessionId, Ask)> = lock(&self.asks)
            .drain()
            .flat_map(|(session, asks)| asks.into_iter().map(move |ask| (session, ask)))
            .collect();
        let withdrawn: Vec<(SessionId, kage_core::protocol::RequestId, bool)> = asks
            .into_iter()
            .map(|(session, ask)| {
                let question = ask.is_question();
                (session, ask.stop(), question)
            })
            .collect();
        let sessions: Vec<SessionId> = lock(&self.ids).by_engine.keys().copied().collect();
        for id in sessions {
            self.host.release(id);
        }
        for (session, request_id, question) in withdrawn {
            if self.host.held(session) {
                continue;
            }
            let kind = if question {
                CommandKind::AnswerQuestion {
                    request_id,
                    answers: None,
                }
            } else {
                CommandKind::ResolvePermission {
                    request_id,
                    decision: kage_core::protocol::PermissionDecision::Deny,
                }
            };
            self.host.engine.send(Command::to(session, kind));
        }
    }
}

impl Agent for CliAcpAgent {
    fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
        self.subagents.store(
            req.client_capabilities.supports_subagents(),
            Ordering::SeqCst,
        );
        self.unconfigured_run.store(
            req.client_capabilities.unconfigured_tools_run(),
            Ordering::SeqCst,
        );
        self.kage_client.store(
            req.client_capabilities
                .meta
                .as_ref()
                .is_some_and(|meta| meta.kage.is_some()),
            Ordering::SeqCst,
        );
        self.plugin_host
            .store(req.client_capabilities.hosts_plugins(), Ordering::SeqCst);
        InitializeResponse {
            protocol_version: PROTOCOL_VERSION,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                steer: true,
                prompt_capabilities: PromptCapabilities {
                    image: true,
                    audio: true,
                    embedded_context: true,
                },
                mcp_capabilities: McpCapabilities {
                    http: true,
                    sse: false,
                },
                session_capabilities: SessionCapabilities {
                    list: Some(Supported {}),
                    resume: Some(Supported {}),
                    close: Some(Supported {}),
                },
            },
            agent_info: Some(Implementation {
                name: "kage".to_owned(),
                title: None,
                version: Some(kage_core::build::version().to_owned()),
            }),
            auth_methods: vec![],
            meta: Some(AgentMeta {
                kage: Some(KageAgentInfo {
                    cwd: std::env::current_dir()
                        .ok()
                        .map(|dir| dir.display().to_string()),
                    config_options: self.default_options(),
                    external_sessions: false,
                }),
            }),
        }
    }

    fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
        if req
            .meta
            .as_ref()
            .and_then(|meta| meta.kage.as_ref())
            .and_then(|kage| kage.external_agent.as_deref())
            .is_some()
        {
            return Err(RpcError::new(
                -32602,
                "external agent sessions are temporarily unavailable",
            ));
        }
        let servers = editor_servers(&req.mcp_servers)?;
        let (path, mut header) =
            crate::plan_session_in(&self.host.sessions, &self.host.default_model, "");
        let id = header.session;
        let plugins = self.hosted_plugins(id, req.meta.as_ref())?;
        let settings =
            SessionSettings::of(req.meta.as_ref(), self.kage_client.load(Ordering::SeqCst))?;
        let (mut spec, runtime) = self.session_spec(
            id,
            &req.cwd,
            &self.host.default_model,
            servers,
            plugins,
            &settings,
        )?;
        header.cwd.clone_from(&spec.cx.workdir);
        header.system_prompt.clone_from(&spec.cx.system_prompt);
        spec.recorder = Some(Recorder::planned(path, header, spec.plugins.clone()));
        let config_options = self.open(id.to_string(), spec, runtime);
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
        if lock(&self.host.external).contains_key(&req.session_id) {
            return Err(RpcError::new(
                -32602,
                "external session history replay is unavailable; use session/resume to attach to the live session",
            ));
        }
        let config_options = self.open_recorded(
            &req.session_id,
            &req.cwd,
            &req.mcp_servers,
            Some(ctx),
            true,
            req.meta.as_ref(),
        )?;
        Ok(LoadSessionResponse { config_options })
    }

    fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, RpcError> {
        list_page(&self.host.sessions, &req)
    }

    fn resume_session(
        &self,
        req: ResumeSessionRequest,
        ctx: &PromptContext,
    ) -> Result<ResumeSessionResponse, RpcError> {
        if self.attach_external(&req.session_id, &req.mcp_servers)? {
            return Ok(ResumeSessionResponse {
                config_options: Vec::new(),
            });
        }
        let config_options = self.open_recorded(
            &req.session_id,
            &req.cwd,
            &req.mcp_servers,
            Some(ctx),
            false,
            req.meta.as_ref(),
        )?;
        Ok(ResumeSessionResponse { config_options })
    }

    /// Releases the caller's attachment to `req.session_id`, closing
    /// the session when it was the last one and the session is idle.
    /// A subagent session is not attachable, so closing one answers
    /// invalid params.
    fn close_session(&self, req: CloseSessionRequest) -> Result<CloseSessionResponse, RpcError> {
        if self.release_external(&req.session_id) {
            return Ok(CloseSessionResponse {});
        }
        let id = {
            let mut ids = lock(&self.ids);
            if ids.read_only.remove(&req.session_id) {
                return Ok(CloseSessionResponse {});
            }
            if ids.subagents.contains_key(&req.session_id) {
                return Err(RpcError::new(
                    -32602,
                    format!("{} is a subagent session", req.session_id),
                ));
            }
            ids.by_client.get(&req.session_id).copied()
        };
        let Some(id) = id else {
            return Err(RpcError::new(
                -32602,
                format!("unknown session {}", req.session_id),
            ));
        };
        let asks = lock(&self.asks).remove(&id).unwrap_or_default();
        for ask in asks {
            ask.stop();
        }
        lock(&self.ids).remove_client(&req.session_id);
        lock(&self.shown).remove(&id);
        lock(&self.held).remove(&id);
        self.host.release(id);
        if !self.host.held(id) {
            // No connection is left to answer the asks parked under the
            // session, so they are declined and the session closes once
            // idle.
            self.host.decline_asks_under(id);
        }
        Ok(CloseSessionResponse {})
    }

    fn set_config_option(
        &self,
        req: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let (commands, config_options) = {
            let mut shown = lock(&self.shown);
            let shown = shown.get_mut(&id).ok_or_else(|| {
                RpcError::new(-32602, format!("unknown session {}", req.session_id))
            })?;
            let commands = shown
                .settings
                .apply(&self.host.models(), &req.config_id, &req.value)?;
            shown.catching_up = true;
            shown.changed.absorb(&commands);
            shown.swallowed = 0;
            (
                commands,
                config_options(&self.host.models(), &shown.settings),
            )
        };
        for command in commands {
            self.host.engine.send(Command::to(id, command));
        }
        Ok(SetSessionConfigOptionResponse { config_options })
    }

    fn config_get(&self, req: ConfigGetRequest) -> Result<ConfigGetResult, RpcError> {
        let mut config = self.load_config(&req)?;
        redact_secrets(&mut config);
        let store =
            crate::auth::AuthStore::load().unwrap_or_else(|_| crate::auth::AuthStore::empty());
        let provider_keys = probe::provider_keys(&config, &store);
        let installed_plugins = crate::plugins_dir()
            .map(|dir| installed_plugins(&dir, &config.plugins))
            .unwrap_or_default();
        Ok(ConfigGetResult {
            providers: config.providers,
            mcp: config.mcp,
            permissions: config.permissions,
            plugins: config.plugins,
            ui: config.ui,
            acp: config.acp,
            installed_plugins,
            provider_keys,
        })
    }

    /// Writes the entry into the user config and answers with the
    /// snapshot after it. An edited `[providers]` section is reloaded
    /// at once; MCP servers, permissions and plugins apply to sessions
    /// opened after the write.
    fn config_set(&self, req: ConfigSetRequest) -> Result<ConfigGetResult, RpcError> {
        let path =
            Config::default_path().ok_or_else(|| RpcError::internal("no user config directory"))?;
        config_set::set(&path, &req.path, req.value.as_ref())?;
        if req
            .path
            .first()
            .is_some_and(|section| section == "providers" || section == "acp")
        {
            self.host.reload_providers().map_err(RpcError::internal)?;
        }
        self.config_get(ConfigGetRequest {
            session_id: req.session_id,
        })
    }

    /// Lists the provider's models with the user config and saved keys
    /// filling what the probe leaves out, or connects to the saved MCP
    /// server or ACP agent the request names. Only specs already saved
    /// in the user config are probed: a request's own command line and
    /// environment never run, so a leaked token cannot spawn processes.
    fn config_test(&self, req: ConfigTestRequest) -> Result<ConfigTestResult, RpcError> {
        let config = Config::load_default_raw().map_err(|e| RpcError::internal(e.to_string()))?;
        if let Some(provider) = &req.provider {
            let store =
                crate::auth::AuthStore::load().unwrap_or_else(|_| crate::auth::AuthStore::empty());
            return Ok(probe::probe(provider, &config, &store));
        }
        let path =
            Config::default_path().ok_or_else(|| RpcError::internal("no user config directory"))?;
        if let Some(mcp) = &req.mcp {
            return test_saved_mcp(&path, &mcp.name);
        }
        if let Some(acp) = &req.acp {
            return test_saved_acp(&path, &acp.name);
        }
        Err(RpcError::new(
            -32602,
            "name a provider, MCP server or ACP agent to test",
        ))
    }

    /// Saves or removes the key, then reloads the providers so one that
    /// had no key before registers.
    fn auth_set(&self, req: AuthSetRequest) -> Result<serde_json::Value, RpcError> {
        if req.provider.is_empty() {
            return Err(RpcError::new(-32602, "name the provider the key is for"));
        }
        let mut store = crate::auth::AuthStore::load().map_err(RpcError::internal)?;
        match req.key.filter(|key| !key.trim().is_empty()) {
            Some(key) => {
                store.set_api_key(&req.provider, key.trim());
            }
            None => {
                store.remove(&req.provider);
            }
        }
        store.save().map_err(RpcError::internal)?;
        self.host.reload_providers().map_err(RpcError::internal)?;
        Ok(serde_json::json!({}))
    }

    fn providers_directory(&self, req: DirectoryRequest) -> Result<DirectoryResult, RpcError> {
        directory::directory(&req)
    }

    fn folders(&self, req: FoldersRequest) -> Result<FoldersResult, RpcError> {
        folders::folders(&req)
    }

    fn themes(&self, _req: ThemesRequest) -> Result<ThemesResult, RpcError> {
        let dir = crate::themes_dir().map_err(RpcError::internal)?;
        Ok(themes::themes(&dir))
    }

    fn plugin_install(&self, req: PluginInstallRequest) -> Result<serde_json::Value, RpcError> {
        let dir = crate::plugins_dir().map_err(RpcError::internal)?;
        let name = plugin_files::install(&dir, &req)?;
        Ok(serde_json::json!({ "name": name }))
    }

    fn plugin_remove(&self, req: PluginRemoveRequest) -> Result<serde_json::Value, RpcError> {
        let dir = crate::plugins_dir().map_err(RpcError::internal)?;
        plugin_files::remove(&dir, &req)?;
        Ok(serde_json::json!({}))
    }

    fn registry_search(
        &self,
        req: RegistrySearchRequest,
    ) -> Result<RegistrySearchResult, RpcError> {
        plugin_registry::search(&req)
    }

    fn registry_install(
        &self,
        req: RegistryInstallRequest,
    ) -> Result<RegistryInstallResult, RpcError> {
        plugin_registry::install(&req)
    }

    fn session_fork(&self, req: SessionForkRequest) -> Result<SessionForkResponse, RpcError> {
        self.fork_recorded(&req)
    }

    fn session_export(&self, req: SessionRequest) -> Result<SessionExportResponse, RpcError> {
        self.export_recorded(&req.session_id)
    }

    fn session_undo(&self, req: SessionRequest) -> Result<SessionUndoResponse, RpcError> {
        self.undo(&req.session_id)
    }

    fn session_rewind(&self, req: SessionRewindRequest) -> Result<serde_json::Value, RpcError> {
        self.rewind(req)
    }

    fn session_prompts(&self, req: SessionRequest) -> Result<SessionPromptsResponse, RpcError> {
        self.prompts(&req.session_id)
    }

    fn session_delete(&self, req: SessionRequest) -> Result<serde_json::Value, RpcError> {
        self.delete(&req.session_id)
    }

    fn session_shell(&self, req: ShellRequest) -> Result<serde_json::Value, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let command = CommandKind::Shell {
            command: req.command,
        };
        self.host.engine.send(Command::to(id, command));
        Ok(serde_json::json!({}))
    }

    fn prompt_withdraw(&self, req: WithdrawRequest) -> Result<PromptWithdrawn, RpcError> {
        self.withdraw(&req)
    }

    fn ui_attach(&self, req: UiAttachRequest) -> Result<UiSnapshot, RpcError> {
        let env = lock(&self.env).clone();
        // Held across the whole attach, so two attaches of one
        // connection resolve, replace their watchers and answer in the
        // order the host processed them.
        let mut ui_watch = lock(&self.ui_watch);
        if req.attach_id <= ui_watch.high {
            return Err(RpcError::new(
                -32602,
                format!(
                    "attach id {} is not newer than {}",
                    req.attach_id, ui_watch.high
                ),
            ));
        }
        // Counted before the runtime resolves: a failed attach still
        // spent its id, so a retry must name a newer one.
        ui_watch.high = req.attach_id;
        let (runtime, reload) = if let Some(client_session) = req.session_id.as_deref() {
            let id = self.engine_id(client_session)?;
            let runtime = self
                .host
                .session_runtime(id)
                .ok_or_else(|| RpcError::new(-32602, "the session runs without plugins"))?;
            (runtime, false)
        } else {
            let cwd = Path::new(&req.cwd);
            match self.host.project(cwd, &env) {
                Some(project) => (project.runtime, true),
                // A project with nothing to load answers the empty
                // snapshot it would have served, not an error: the
                // TUI attaches once at startup, plugins or not.
                None if !self.host.project_has_content(cwd) => {
                    return Ok(UiSnapshot::default());
                }
                None => {
                    return Err(RpcError::internal("the project's plugins did not load"));
                }
            }
        };
        // Marked first, so a change while the snapshot is built is sent.
        let marker = ui::marker(&runtime);
        let snapshot = ui::snapshot(&runtime);
        let stop = Arc::new(AtomicBool::new(false));
        ui::watch(ui::Watch {
            host: Arc::clone(&self.host),
            cwd: req.cwd,
            env,
            runtime,
            reload,
            attach_id: req.attach_id,
            marker,
            sent: snapshot.clone(),
            peer: self.peer.clone(),
            stop: Arc::clone(&stop),
        });
        ui_watch.stop.store(true, Ordering::SeqCst);
        ui_watch.stop = stop;
        Ok(snapshot)
    }

    fn ui_command(&self, req: UiCommandRequest) -> Result<UiOutput, RpcError> {
        let (runtime, invocation) = self.ui_context(&req.cwd, req.session_id.as_deref())?;
        invocation.scope(|| ui::run_command(&runtime, &req.name, &req.args, &self.peer))
    }

    fn ui_keymap(&self, req: UiKeymapRequest) -> Result<UiOutput, RpcError> {
        let (runtime, invocation) = self.ui_context(&req.cwd, req.session_id.as_deref())?;
        invocation.scope(|| ui::run_keymap(&runtime, req.id, &self.peer))
    }

    fn mcp_restart(&self, req: McpRestartRequest) -> Result<serde_json::Value, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let command = CommandKind::RestartMcp { server: req.server };
        self.host.engine.send(Command::to(id, command));
        Ok(serde_json::json!({}))
    }

    /// Asks the engine to compact the session now. The result reaches
    /// the client as the compaction's updates.
    fn session_compact(&self, req: SessionRequest) -> Result<serde_json::Value, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        self.host.engine.send(Command::to(id, CommandKind::Compact));
        Ok(serde_json::json!({}))
    }

    fn models_list(&self) -> Result<ModelsResponse, RpcError> {
        Ok(models::catalog(&self.host.registry()))
    }

    fn options_list(&self, req: ConfigGetRequest) -> Result<OptionsResponse, RpcError> {
        Ok(registry::entries(&self.load_config(&req)?))
    }

    /// Writes the option into the user config. A project config that
    /// sets the same key still wins, which the answer shows.
    fn option_set(&self, req: OptionSetRequest) -> Result<OptionsResponse, RpcError> {
        let path =
            Config::default_path().ok_or_else(|| RpcError::internal("no user config directory"))?;
        registry::set(&path, &req)?;
        self.options_list(ConfigGetRequest::default())
    }

    /// Saves an image or audio clip the model or a tool produced. The
    /// engine reports where it went, or why it could not, as a notice
    /// on the session.
    fn image_save(&self, req: ImageSaveRequest) -> Result<serde_json::Value, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        self.host.engine.send(Command::to(
            id,
            CommandKind::Media {
                action: kage_core::protocol::ImageAction::Save,
                sha256: Some(req.sha256),
                path: req.path.map(std::path::PathBuf::from),
            },
        ));
        Ok(serde_json::json!({}))
    }

    fn session_rename(&self, req: SessionRenameRequest) -> Result<serde_json::Value, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let title = req.title.trim();
        if title.is_empty() {
            return Err(RpcError::new(-32602, "a title needs text"));
        }
        self.host.engine.send(Command::to(
            id,
            CommandKind::SetTitle {
                title: title.to_owned(),
            },
        ));
        Ok(serde_json::json!({}))
    }

    /// Continues swarm children of the session. Answers once the engine
    /// checked and attached every member: one that is not a swarm child
    /// of the session, or is still working, refuses the whole request.
    /// The members' results reach the session once all have reported.
    fn swarm_resume(&self, req: SwarmResumeRequest) -> Result<SwarmResumeResponse, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let mut members = BTreeMap::new();
        for (key, prompt) in req.members {
            let child = key
                .parse::<ulid::Ulid>()
                .map(SessionId)
                .map_err(|_| RpcError::new(-32602, format!("{key} is not a session id")))?;
            members.insert(child, prompt);
        }
        let resumed = self
            .host
            .engine
            .resume_swarm(id, members)
            .map_err(|text| RpcError::new(-32602, text))?;
        Ok(SwarmResumeResponse {
            resumed: resumed.iter().map(ToString::to_string).collect(),
        })
    }

    fn fs(&self, req: FsRequest) -> Result<FsResult, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let workdir = self
            .host
            .workdir(id)
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {}", req.session_id)))?;
        let permissions = Config::load_layered(&workdir)
            .map(|config| config.permissions)
            .unwrap_or_default();
        fs::handle(&workdir, &permissions, &req)
    }

    fn prepare_prompt(
        &self,
        req: &PromptRequest,
    ) -> Result<Option<Box<dyn std::any::Any + Send>>, RpcError> {
        let sessions = lock(&self.host.external);
        let ids = lock(&self.ids);
        if !ids.external.contains(&req.session_id) {
            return Ok(None);
        }
        let session = sessions
            .get(&req.session_id)
            .cloned()
            .ok_or_else(|| RpcError::new(-32602, "external session is unavailable"))?;
        let ticket = session.register_prompt(self.connection)?;
        Ok(Some(Box::new(PreparedExternalPrompt { session, ticket })))
    }

    fn prompt_prepared(
        &self,
        req: PromptRequest,
        ctx: &PromptContext,
        prepared: Option<Box<dyn std::any::Any + Send>>,
    ) -> Result<PromptResponse, RpcError> {
        let Some(prepared) = prepared else {
            return self.prompt(req, ctx);
        };
        let prepared = prepared
            .downcast::<PreparedExternalPrompt>()
            .map_err(|_| RpcError::internal("invalid prepared external prompt"))?;
        if ctx.start(|| ()).is_none() {
            return Ok(PromptResponse {
                stop_reason: StopReason::Cancelled,
            });
        }
        prepared.session.prompt(prepared.ticket, req)
    }

    fn prompt(&self, req: PromptRequest, ctx: &PromptContext) -> Result<PromptResponse, RpcError> {
        if lock(&self.ids).read_only.contains(&req.session_id) {
            return Err(RpcError::new(
                -32602,
                format!(
                    "{} is an agent's session; agent sessions are read-only",
                    req.session_id
                ),
            ));
        }
        let id = self.engine_id(&req.session_id)?;
        let content = req.prompt.into_iter().map(prompt_content).collect();
        let delivery = match req.delivery {
            Some(PromptDelivery::Steer) => Delivery::Steer,
            _ => Delivery::Queue,
        };
        let end = match ctx.start(|| {
            if !self.host.claim_prompt(id, self.connection) {
                return Err(RpcError::new(
                    -32603,
                    "session is busy; wait for the running prompt to finish",
                ));
            }
            let (done, end) = mpsc::channel();
            lock(&self.waiters).entry(id).or_default().push(done);
            self.host
                .engine
                .send(Command::to(id, CommandKind::Prompt { content, delivery }));
            Ok(end)
        }) {
            Some(result) => result?,
            None => {
                return Ok(PromptResponse {
                    stop_reason: StopReason::Cancelled,
                });
            }
        };
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
        if let Some(session) = self.external_session(session_id) {
            session.cancel(self.connection);
            return;
        }
        let id = {
            let ids = lock(&self.ids);
            ids.by_client
                .get(session_id)
                .or_else(|| ids.subagents.get(session_id))
                .copied()
        };
        if let Some(id) = id {
            self.host.engine.send(Command::to(id, CommandKind::Cancel));
        }
    }

    fn session_announced(&self, session_id: &str) {
        if let Some(session) = self.external_session(session_id) {
            session.announce(self.connection);
            return;
        }
        let Some(id) = lock(&self.ids).by_client.get(session_id).copied() else {
            return;
        };
        let held = lock(&self.held).remove(&id).unwrap_or_default();
        bridge::flush_held(&self.peer, session_id, held);
    }

    fn disconnecting(&self) {
        self.disconnect_external();
    }

    fn detached(&self) {
        self.detach();
    }

    fn environment(&self, vars: BTreeMap<String, String>) {
        if !self.takes_env.load(Ordering::SeqCst) {
            return;
        }
        let vars = vars
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        *lock(&self.env) = Environment::captured(vars);
    }
}

/// Probes the `[mcp.servers.<name>]` entry saved in the user config at
/// `path`: the request names the server and nothing else, so only what
/// is on file is ever spawned. A name with no saved entry is invalid
/// params.
fn test_saved_mcp(path: &Path, name: &str) -> Result<ConfigTestResult, RpcError> {
    let invalid = |message: String| RpcError::new(-32602, message);
    let saved = saved_entry(path, &["mcp", "servers", name])?
        .ok_or_else(|| invalid(format!("save the server `{name}`, then test it")))?;
    let spec = serde_json::from_value::<McpSpec>(saved).map_err(|e| invalid(e.to_string()))?;
    Ok(probe::probe_mcp(name, &spec))
}

/// Probes the `[acp.agents.<name>]` entry saved in the user config at
/// `path`, command and environment included, the same way: an unsaved
/// name is refused instead of running a request-supplied command line.
fn test_saved_acp(path: &Path, name: &str) -> Result<ConfigTestResult, RpcError> {
    let invalid = |message: String| RpcError::new(-32602, message);
    let saved = saved_entry(path, &["acp", "agents", name])?
        .ok_or_else(|| invalid(format!("save the agent `{name}`, then test it")))?;
    let agent = serde_json::from_value::<kage_core::config::AcpAgent>(saved)
        .map_err(|e| invalid(e.to_string()))?;
    Ok(probe::probe_acp(&agent))
}

/// The JSON value the user config at `path` holds at `keys`, or `None`
/// when the file or the entry is not there.
fn saved_entry(path: &Path, keys: &[&str]) -> Result<Option<serde_json::Value>, RpcError> {
    kage_core::config_edit::current(path, keys).map_err(|e| RpcError::internal(e.to_string()))
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
