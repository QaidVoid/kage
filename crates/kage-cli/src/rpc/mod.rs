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
mod config_set;
mod content;
mod directory;
mod folders;
mod fs;
pub(crate) mod host;
#[cfg(unix)]
pub(crate) mod link;
mod live;
mod mcp;
mod models;
mod options;
mod plugin_files;
mod probe;
mod registry;
mod sessions;

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
    Implementation, InitializeRequest, InitializeResponse, InstalledPlugin, KageAgentInfo,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    McpCapabilities, ModelsResponse, NewSessionRequest, NewSessionResponse, OptionSetRequest,
    OptionsResponse, PROTOCOL_VERSION, PluginInstallRequest, PluginRemoveRequest,
    PromptCapabilities, PromptDelivery, PromptRequest, PromptResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionCapabilities, SessionConfigOption, SessionExportResponse,
    SessionForkRequest, SessionForkResponse, SessionRenameRequest, SessionRequest, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, Supported,
    SwarmResumeRequest, SwarmResumeResponse,
};
use kage_acp::agent::{Agent, PromptContext, send_update};
use kage_core::config::{Config, McpServer as McpSpec};
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{AgentTree, Command, CommandKind, Delivery, RunOutcome};
use kage_core::sync::lock;
use kage_core::{LoopError, SessionId, StopReason as CoreStopReason};
use kage_jsonrpc::{Peer, RpcError};

use bridge::{Ask, AskSet, Bridge};
use content::prompt_content;
use host::Host;
use live::Seed;
use mcp::editor_servers;
use options::{Settings, Shown, config_options};
use sessions::list_page;

use crate::engine::{Background, Recorder, SessionSpec, SubscriptionId};

/// What a redacted config value reads as.
const REDACTED: &str = "<redacted>";

/// The `*.lua` plugin files in `dir`, by name, each marked with whether
/// the `enabled` allowlist lets it load. A missing directory has none.
fn installed_plugins(dir: &Path, enabled: &[String]) -> Vec<InstalledPlugin> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut plugins: Vec<InstalledPlugin> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()? != "lua" {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_owned();
            (!name.starts_with('@')).then(|| InstalledPlugin {
                enabled: enabled.is_empty() || enabled.contains(&name),
                name,
            })
        })
        .collect();
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    plugins
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
pub(crate) fn run(model_override: Option<&str>, system_role: &str) -> ExitCode {
    let served = Host::start(model_override, system_role)
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

type Waiters = Arc<Mutex<HashMap<SessionId, Vec<mpsc::Sender<PromptEnd>>>>>;

type ShownBySession = Arc<Mutex<HashMap<SessionId, Shown>>>;

/// Updates for client sessions whose opening response is not written
/// yet. Sending them earlier would reach the client before it knows the
/// session. Capped per session at [`bridge::HELD_CAP`]; later updates
/// are dropped.
type Held = Arc<Mutex<HashMap<SessionId, Vec<SessionUpdate>>>>;

/// The ACP agent one connection on the host is served through. Holds the
/// per-connection maps; the engine and the session setup live in the
/// host every connection shares.
struct CliAcpAgent {
    host: Arc<Host>,
    /// This connection's id on the host.
    connection: u64,
    ids: Arc<Mutex<Ids>>,
    waiters: Waiters,
    shown: ShownBySession,
    subagents: Arc<AtomicBool>,
    /// Whether the client asked for tools without a `[permissions]`
    /// rule to run, as in the TUI, instead of asking first.
    unconfigured_run: AtomicBool,
    /// Whether the client is a kage client, which shows runs it did not
    /// prompt, so an idle session wakes for a background agent's result.
    kage_client: AtomicBool,
    peer: Peer,
    held: Held,
    asks: AskSet,
    subscription: SubscriptionId,
    /// Attaches waiting to be applied by the bridge.
    seeds: Arc<Mutex<Vec<Seed>>>,
}

impl CliAcpAgent {
    /// The agent for one connection on `host`.
    fn new(host: Arc<Host>, peer: Peer) -> Self {
        let connection = host.next_connection();
        let ids = Arc::new(Mutex::new(Ids::default()));
        let waiters = Waiters::default();
        let shown = ShownBySession::default();
        let subagents = Arc::new(AtomicBool::new(false));
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
            streaming: HashSet::new(),
            ended: HashMap::new(),
            commands: HashMap::new(),
            modes: HashMap::new(),
            names: HashMap::new(),
            statuses: HashMap::new(),
            fills: HashMap::new(),
            pruned_cost: HashMap::new(),
            compacting: HashMap::new(),
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
            ids,
            waiters,
            shown,
            subagents,
            unconfigured_run: AtomicBool::new(false),
            kage_client: AtomicBool::new(false),
            peer,
            held,
            asks,
            subscription,
            seeds,
        }
    }

    /// The config options a new session opens with: the default model,
    /// its automatic thinking level and no permission mode.
    fn default_options(&self) -> Vec<SessionConfigOption> {
        let settings = Settings::fresh(&self.host.default_model, &self.host.registry());
        config_options(&self.host.models(), &settings)
    }

    /// What a session of this connection runs with: the host's spec,
    /// with the permission fallback the client asked for at
    /// `initialize`, and background results waking an idle session for
    /// kage clients. Agents the session starts inherit its gate.
    fn session_spec(
        &self,
        id: SessionId,
        cwd: &str,
        model: &str,
        servers: BTreeMap<String, McpSpec>,
    ) -> Result<SessionSpec, RpcError> {
        let mut spec = (self.host.spec)(&self.host.registry(), id, cwd, model, servers)?;
        if self.unconfigured_run.load(Ordering::SeqCst) {
            spec.gate = spec.gate.with_fallback(PermissionAction::Allow);
        }
        if self.kage_client.load(Ordering::SeqCst)
            && let Some(agents) = spec.agents.as_mut()
        {
            agents.background = Background::Wake;
        }
        Ok(spec)
    }

    /// Opens `spec` as the client session `client_id` and returns its
    /// config options. Updates for it wait for
    /// [`Agent::session_announced`].
    fn open(&self, client_id: String, spec: SessionSpec) -> Vec<SessionConfigOption> {
        let settings = Settings::of(&spec, &self.host.registry());
        let options = config_options(&self.host.models(), &settings);
        let shown = Shown {
            settings: settings.clone(),
            catching_up: false,
        };
        lock(&self.shown).insert(spec.id, shown);
        lock(&self.held).insert(spec.id, Vec::new());
        lock(&self.ids).insert(client_id, spec.id);
        self.host.launch(spec, settings);
        options
    }

    /// The layered config for the request's session workdir, or for the
    /// server's directory when the request names no known session.
    fn load_config(&self, req: &ConfigGetRequest) -> Result<Config, RpcError> {
        let workdir = req
            .session_id
            .as_deref()
            .and_then(|client_id| self.engine_id(client_id).ok())
            .and_then(|id| self.host.workdir(id));
        let dir = workdir.unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        });
        Config::load_layered(&dir).map_err(|e| RpcError::internal(e.to_string()))
    }

    fn engine_id(&self, client_id: &str) -> Result<SessionId, RpcError> {
        lock(&self.ids)
            .by_client
            .get(client_id)
            .copied()
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {client_id}")))
    }

    /// The connection ended: stop delivering engine events to it, free
    /// what its prompts wait on, withdraw its open asks, and release
    /// every session it held. An ask whose session no other connection
    /// holds is denied, so the run goes on instead of waiting for a
    /// client that may never come back. The engine and its runs keep
    /// going for the connections that stay.
    fn detach(&self) {
        self.host.engine.unsubscribe(self.subscription);
        self.host.release_prompts_of(self.connection);
        lock(&self.waiters).clear();
        let asks: Vec<(SessionId, Ask)> = lock(&self.asks)
            .drain()
            .flat_map(|(session, asks)| asks.into_iter().map(move |ask| (session, ask)))
            .collect();
        let withdrawn: Vec<(SessionId, kage_core::protocol::RequestId)> = asks
            .into_iter()
            .map(|(session, ask)| (session, ask.stop()))
            .collect();
        let sessions: Vec<SessionId> = lock(&self.ids).by_engine.keys().copied().collect();
        for id in sessions {
            self.host.release(id);
        }
        for (session, request_id) in withdrawn {
            if !self.host.held(session) {
                self.host.engine.send(Command::to(
                    session,
                    CommandKind::ResolvePermission {
                        request_id,
                        decision: kage_core::protocol::PermissionDecision::Deny,
                    },
                ));
            }
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
        InitializeResponse {
            protocol_version: PROTOCOL_VERSION,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                steer: true,
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
                    close: Some(Supported {}),
                },
            },
            agent_info: Some(Implementation {
                name: "kage".to_owned(),
                title: None,
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            auth_methods: vec![],
            meta: Some(AgentMeta {
                kage: Some(KageAgentInfo {
                    cwd: std::env::current_dir()
                        .ok()
                        .map(|dir| dir.display().to_string()),
                    config_options: self.default_options(),
                }),
            }),
        }
    }

    fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
        let servers = editor_servers(&req.mcp_servers)?;
        let (path, mut header) =
            crate::plan_session_in(&self.host.sessions, &self.host.default_model, "");
        let id = header.session;
        let mut spec = self.session_spec(id, &req.cwd, &self.host.default_model, servers)?;
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
            self.open_recorded(&req.session_id, &req.cwd, &req.mcp_servers, Some(ctx), true)?;
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
        let config_options = self.open_recorded(
            &req.session_id,
            &req.cwd,
            &req.mcp_servers,
            Some(ctx),
            false,
        )?;
        Ok(ResumeSessionResponse { config_options })
    }

    /// Releases the caller's attachment to `req.session_id`, closing
    /// the session when it was the last one and the session is idle.
    /// A subagent session is not attachable, so closing one answers
    /// invalid params.
    fn close_session(&self, req: CloseSessionRequest) -> Result<CloseSessionResponse, RpcError> {
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
            .map(|dir| installed_plugins(&dir, &config.plugins.enabled))
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
    /// filling what the probe leaves out.
    fn config_test(&self, req: ConfigTestRequest) -> Result<ConfigTestResult, RpcError> {
        let config = Config::load_default().map_err(|e| RpcError::internal(e.to_string()))?;
        if let Some(provider) = &req.provider {
            let store =
                crate::auth::AuthStore::load().unwrap_or_else(|_| crate::auth::AuthStore::empty());
            return Ok(probe::probe(provider, &config, &store));
        }
        let path =
            Config::default_path().ok_or_else(|| RpcError::internal("no user config directory"))?;
        let saved = |keys: &[&str]| {
            kage_core::config_edit::current(&path, keys)
                .map_err(|e| RpcError::internal(e.to_string()))
        };
        let invalid = |e: serde_json::Error| RpcError::new(-32602, e.to_string());
        if let Some(mcp) = &req.mcp {
            let old = saved(&["mcp", "servers", &mcp.name])?;
            let server = config_set::unredacted(&mcp.server, old.as_ref(), &mcp.name)?;
            let spec = serde_json::from_value(server).map_err(invalid)?;
            return Ok(probe::probe_mcp(&mcp.name, &spec));
        }
        if let Some(acp) = &req.acp {
            let old = saved(&["acp", "agents", &acp.name, "env"])?;
            let env = config_set::unredacted(&serde_json::json!(acp.env), old.as_ref(), "env")?;
            let agent = kage_core::config::AcpAgent {
                command: acp.command.clone(),
                args: acp.args.clone(),
                env: serde_json::from_value(env).map_err(invalid)?,
            };
            return Ok(probe::probe_acp(&agent));
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

    fn session_fork(&self, req: SessionForkRequest) -> Result<SessionForkResponse, RpcError> {
        self.fork_recorded(&req)
    }

    fn session_export(&self, req: SessionRequest) -> Result<SessionExportResponse, RpcError> {
        self.export_recorded(&req.session_id)
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

    fn prompt(&self, req: PromptRequest, _ctx: &PromptContext) -> Result<PromptResponse, RpcError> {
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
        if !self.host.claim_prompt(id, self.connection) {
            return Err(RpcError::new(
                -32603,
                "session is busy; wait for the running prompt to finish",
            ));
        }
        let content = req.prompt.into_iter().map(prompt_content).collect();
        let delivery = match req.delivery {
            Some(PromptDelivery::Steer) => Delivery::Steer,
            _ => Delivery::Queue,
        };
        let (done, end) = mpsc::channel();
        lock(&self.waiters).entry(id).or_default().push(done);
        self.host
            .engine
            .send(Command::to(id, CommandKind::Prompt { content, delivery }));
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
            self.host.engine.send(Command::to(id, CommandKind::Cancel));
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

    fn detached(&self) {
        self.detach();
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
