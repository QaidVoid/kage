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
mod content;
mod host;
mod live;
mod mcp;
mod options;
mod sessions;

use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use kage_acp::acp::{
    AgentCapabilities, CloseSessionRequest, CloseSessionResponse, Implementation,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, McpCapabilities, NewSessionRequest,
    NewSessionResponse, PROTOCOL_VERSION, PromptCapabilities, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionConfigOption,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
    Supported,
};
use kage_acp::agent::{Agent, PromptContext, send_update};
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

use crate::engine::{Recorder, SessionSpec, SubscriptionId};

/// Entry point for the `Rpc` subcommand.
pub(crate) fn run(model_override: Option<&str>, system_role: &str) -> ExitCode {
    let served = Host::start(model_override, system_role).and_then(|host| {
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

/// How a prompt's run ended, handed from the bridge to the waiting prompt.
struct PromptEnd {
    outcome: RunOutcome,
    stop: Option<CoreStopReason>,
}

type Waiters = Arc<Mutex<HashMap<SessionId, mpsc::Sender<PromptEnd>>>>;

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
            models: Arc::clone(&host.models),
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
            peer,
            held,
            asks,
            subscription,
            seeds,
        }
    }

    /// Opens `spec` as the client session `client_id` and returns its
    /// config options. Updates for it wait for
    /// [`Agent::session_announced`].
    fn open(&self, client_id: String, spec: SessionSpec) -> Vec<SessionConfigOption> {
        let settings = Settings::of(&spec, &self.host.registry);
        let options = config_options(&self.host.models, &settings);
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

    fn engine_id(&self, client_id: &str) -> Result<SessionId, RpcError> {
        lock(&self.ids)
            .by_client
            .get(client_id)
            .copied()
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {client_id}")))
    }

    /// The connection ended: stop delivering engine events to it, free
    /// what its prompts wait on, withdraw its open asks without
    /// answering them, and release every session it held. The engine
    /// and its runs keep going for the connections that stay.
    fn detach(&self) {
        self.host.engine.unsubscribe(self.subscription);
        self.host.release_prompts_of(self.connection);
        lock(&self.waiters).clear();
        let asks: Vec<Ask> = lock(&self.asks)
            .drain()
            .flat_map(|(_, asks)| asks)
            .collect();
        for ask in asks {
            ask.stop();
        }
        let sessions: Vec<SessionId> = lock(&self.ids).by_engine.keys().copied().collect();
        for id in sessions {
            self.host.release(id);
        }
    }
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
                    close: Some(Supported {}),
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
            crate::plan_session(&self.host.default_model, "").map_err(RpcError::internal)?;
        let id = header.session;
        let mut spec = (self.host.spec)(id, &req.cwd, &self.host.default_model, servers)?;
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
            let ids = lock(&self.ids);
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
        let (command, config_options) = {
            let mut shown = lock(&self.shown);
            let shown = shown.get_mut(&id).ok_or_else(|| {
                RpcError::new(-32602, format!("unknown session {}", req.session_id))
            })?;
            let command = shown
                .settings
                .apply(&self.host.models, &req.config_id, &req.value)?;
            shown.catching_up = true;
            (command, config_options(&self.host.models, &shown.settings))
        };
        self.host.engine.send(Command::to(id, command));
        Ok(SetSessionConfigOptionResponse { config_options })
    }

    fn prompt(&self, req: PromptRequest, _ctx: &PromptContext) -> Result<PromptResponse, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        if !self.host.claim_prompt(id, self.connection) {
            return Err(RpcError::new(
                -32603,
                "session is busy; wait for the running prompt to finish",
            ));
        }
        let content = req.prompt.into_iter().map(prompt_content).collect();
        let (done, end) = mpsc::channel();
        lock(&self.waiters).insert(id, done);
        self.host.engine.send(Command::to(
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
