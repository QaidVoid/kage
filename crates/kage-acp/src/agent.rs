//! ACP agent server.
//!
//! Drives an injected [`Agent`] over the [`kage_jsonrpc`] peer, conformant
//! with the published ACP spec: it answers `initialize`,
//! `session/new`, `session/prompt`, and handles the `session/cancel`
//! notification, streaming progress back as `session/update`
//! notifications. `session/load` is rejected until that capability
//! lands.
//!
//! A prompt turn runs on its own worker thread so the dispatch loop
//! keeps draining inbound messages - that is what lets a
//! `session/cancel` take effect mid-turn and (once wired) lets the
//! agent issue its own `session/request_permission` requests without
//! deadlocking.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use kage_jsonrpc::{Inbound, Peer, RpcError, connect};

use crate::acp::{
    InitializeRequest, InitializeResponse, LoadSessionRequest, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, PermissionOutcome, PromptRequest,
    PromptResponse, RequestPermissionRequest, RequestPermissionResponse, SessionNotification,
    SessionUpdate, ToolCallStatus, ToolCallUpdate,
};

/// The verdict the agent's `before_tool_call` hook acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Run the tool call.
    Allow,
    /// Block it, with an optional reason for the model.
    Deny(Option<String>),
}

/// A cloneable, `'static` handle that asks the client to allow or
/// deny a tool call via `session/request_permission`. Detached from
/// [`PromptContext`] so it can live inside the agent's loop hooks.
#[derive(Clone)]
pub struct AcpPermission {
    peer: Peer,
    session_id: String,
    cancel: Arc<AtomicBool>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl AcpPermission {
    /// Ask the client to permit a tool call. Blocks on
    /// `session/request_permission` until the client answers or the
    /// turn is cancelled. Never auto-approves: any error, cancel, or
    /// rejection resolves to [`PermissionDecision::Deny`].
    #[must_use]
    pub fn request(&self, title: &str, raw_input: serde_json::Value) -> PermissionDecision {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = RequestPermissionRequest {
            session_id: self.session_id.clone(),
            tool_call: ToolCallUpdate {
                tool_call_id: format!("call-{id}"),
                status: Some(ToolCallStatus::Pending),
                content: Vec::new(),
                raw_output: Some(raw_input),
            },
            options: vec![
                PermissionOption {
                    option_id: "allow".to_owned(),
                    name: format!("Allow {title}"),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionOption {
                    option_id: "reject".to_owned(),
                    name: format!("Reject {title}"),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ],
        };
        let Ok(params) = serde_json::to_value(&req) else {
            return PermissionDecision::Deny(Some("encode permission request".to_owned()));
        };
        let cancel = Arc::clone(&self.cancel);
        let outcome =
            self.peer
                .request_cancellable("session/request_permission", params, &move || {
                    cancel.load(Ordering::SeqCst)
                });
        match outcome {
            Ok(value) => match serde_json::from_value::<RequestPermissionResponse>(value) {
                Ok(resp) => match resp.outcome {
                    PermissionOutcome::Selected(sel) if sel.option_id == "allow" => {
                        PermissionDecision::Allow
                    }
                    PermissionOutcome::Selected(_) => {
                        PermissionDecision::Deny(Some("rejected by client".to_owned()))
                    }
                    PermissionOutcome::Cancelled => {
                        PermissionDecision::Deny(Some("cancelled".to_owned()))
                    }
                },
                Err(e) => PermissionDecision::Deny(Some(format!("decode outcome: {e}"))),
            },
            Err(e) => PermissionDecision::Deny(Some(e.message)),
        }
    }
}

/// Per-session cancel flags, keyed by session id.
type Sessions = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

/// Handed to [`Agent::prompt`]: stream updates and observe
/// cancellation for the running session.
pub struct PromptContext {
    peer: Peer,
    session_id: String,
    cancel: Arc<AtomicBool>,
    next_call_id: Arc<std::sync::atomic::AtomicU64>,
}

impl PromptContext {
    /// Emit a `session/update` notification for this session.
    pub fn update(&self, update: SessionUpdate) {
        let note = SessionNotification {
            session_id: self.session_id.clone(),
            update,
        };
        if let Ok(params) = serde_json::to_value(&note) {
            let _ = self.peer.notify("session/update", params);
        }
    }

    /// Whether the client asked to cancel this turn.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// The underlying peer, for agent-initiated requests
    /// (`session/request_permission`).
    #[must_use]
    pub fn peer(&self) -> &Peer {
        &self.peer
    }

    /// The session this turn belongs to.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// A detached, cloneable permission handle for the agent's loop
    /// hooks. It shares this turn's cancel flag and call-id counter.
    #[must_use]
    pub fn permission(&self) -> AcpPermission {
        AcpPermission {
            peer: self.peer.clone(),
            session_id: self.session_id.clone(),
            cancel: Arc::clone(&self.cancel),
            next_id: Arc::clone(&self.next_call_id),
        }
    }
}

/// The host-supplied agent the server drives. The server owns the
/// protocol; this trait owns the agent.
pub trait Agent: Send + 'static {
    /// Handshake. Return the agent's capabilities and identity.
    fn initialize(&mut self, req: InitializeRequest) -> InitializeResponse;

    /// Create a session for `cwd`.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if a session cannot be created.
    fn new_session(&mut self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError>;

    /// Resume a previously recorded session, streaming its history
    /// back through `ctx` as `session/update` notifications. The
    /// default rejects: only agents that advertise
    /// `agentCapabilities.loadSession` override it.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session id is unknown or its
    /// history cannot be replayed.
    fn load_session(
        &mut self,
        _req: LoadSessionRequest,
        _ctx: &PromptContext,
    ) -> Result<(), RpcError> {
        Err(RpcError::method_not_found("session/load"))
    }

    /// Run one prompt turn to completion, streaming `session/update`
    /// notifications through `ctx`.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the turn cannot start or fails
    /// irrecoverably (streamed progress already reached the client).
    fn prompt(
        &mut self,
        req: PromptRequest,
        ctx: &PromptContext,
    ) -> Result<PromptResponse, RpcError>;
}

fn parse<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))
}

/// Serve the ACP agent protocol over `reader`/`writer` until the peer
/// disconnects.
///
/// # Errors
///
/// Returns an [`RpcError`] only for a fatal transport failure; a
/// clean disconnect is `Ok(())`.
pub fn serve_agent<R, W, A>(reader: R, writer: W, agent: A) -> Result<(), RpcError>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
    A: Agent,
{
    let (peer, inbound, _reader) = connect(reader, writer);
    let agent = Arc::new(Mutex::new(agent));
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    for message in inbound {
        match message {
            Inbound::Notification { method, params } => {
                if method == "session/cancel"
                    && let Ok(c) = parse::<crate::acp::CancelNotification>(params)
                    && let Some(flag) = sessions
                        .lock()
                        .expect("acp sessions mutex poisoned")
                        .get(&c.session_id)
                {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            Inbound::Request { id, method, params } => {
                handle_request(&peer, &agent, &sessions, id, &method, params);
            }
        }
    }
    Ok(())
}

fn jval<T: serde::Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// Run a per-session operation (prompt or load) on a worker thread so
/// the dispatch loop keeps draining inbound and the op can issue its
/// own outgoing requests.
fn spawn_op<A, F>(
    peer: &Peer,
    agent: &Arc<Mutex<A>>,
    sessions: &Sessions,
    id: serde_json::Value,
    session_id: String,
    op: F,
) where
    A: Agent,
    F: FnOnce(&mut A, &PromptContext) -> Result<serde_json::Value, RpcError> + Send + 'static,
{
    let flag = sessions
        .lock()
        .expect("acp sessions mutex poisoned")
        .entry(session_id.clone())
        .or_insert_with(|| Arc::new(AtomicBool::new(false)))
        .clone();
    flag.store(false, Ordering::SeqCst);
    let ctx = PromptContext {
        peer: peer.clone(),
        session_id,
        cancel: flag,
        next_call_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };
    let agent = Arc::clone(agent);
    let peer = peer.clone();
    thread::spawn(move || {
        let mut guard = agent.lock().expect("acp agent mutex poisoned");
        let outcome = op(&mut guard, &ctx);
        let _ = peer.respond(&id, outcome);
    });
}

fn handle_request<A: Agent>(
    peer: &Peer,
    agent: &Arc<Mutex<A>>,
    sessions: &Sessions,
    id: serde_json::Value,
    method: &str,
    params: serde_json::Value,
) {
    match method {
        "initialize" => {
            let outcome = parse::<InitializeRequest>(params).map(|req| {
                jval(
                    agent
                        .lock()
                        .expect("acp agent mutex poisoned")
                        .initialize(req),
                )
            });
            let _ = peer.respond(&id, outcome);
        }
        "session/new" => {
            let result = parse::<NewSessionRequest>(params).and_then(|req| {
                agent
                    .lock()
                    .expect("acp agent mutex poisoned")
                    .new_session(req)
            });
            if let Ok(resp) = &result {
                sessions
                    .lock()
                    .expect("acp sessions mutex poisoned")
                    .insert(resp.session_id.clone(), Arc::new(AtomicBool::new(false)));
            }
            let _ = peer.respond(&id, result.map(jval));
        }
        "session/prompt" => match parse::<PromptRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
            }
            Ok(req) => {
                let sid = req.session_id.clone();
                spawn_op(peer, agent, sessions, id, sid, move |a, ctx| {
                    a.prompt(req, ctx).map(jval)
                });
            }
        },
        "session/load" => match parse::<LoadSessionRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
            }
            Ok(req) => {
                let sid = req.session_id.clone();
                spawn_op(peer, agent, sessions, id, sid, move |a, ctx| {
                    a.load_session(req, ctx).map(|()| serde_json::Value::Null)
                });
            }
        },
        other => {
            let _ = peer.respond(&id, Err(RpcError::method_not_found(other)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use super::*;
    use kage_jsonrpc::connect;

    use crate::acp::{
        AgentCapabilities, ContentBlock, Implementation, MessageChunk, PromptCapabilities,
        StopReason,
    };

    struct MockAgent;

    impl Agent for MockAgent {
        fn initialize(&mut self, req: InitializeRequest) -> InitializeResponse {
            assert_eq!(req.protocol_version, crate::acp::PROTOCOL_VERSION);
            InitializeResponse {
                protocol_version: crate::acp::PROTOCOL_VERSION,
                agent_capabilities: AgentCapabilities {
                    load_session: false,
                    prompt_capabilities: PromptCapabilities::default(),
                },
                agent_info: Some(Implementation {
                    name: "mock".into(),
                    title: None,
                    version: None,
                }),
                auth_methods: vec![],
            }
        }

        fn new_session(&mut self, _req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            Ok(NewSessionResponse {
                session_id: "sess-1".into(),
            })
        }

        fn prompt(
            &mut self,
            req: PromptRequest,
            ctx: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            assert_eq!(ctx.session_id(), "sess-1");
            let echoed = req
                .prompt
                .first()
                .and_then(ContentBlock::as_text)
                .unwrap_or_default()
                .to_owned();
            ctx.update(SessionUpdate::AgentMessageChunk(MessageChunk {
                content: ContentBlock::text(format!("echo: {echoed}")),
            }));
            Ok(PromptResponse {
                stop_reason: if ctx.is_cancelled() {
                    StopReason::Cancelled
                } else {
                    StopReason::EndTurn
                },
            })
        }
    }

    #[test]
    fn initialize_new_prompt_round_trip() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server = thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, MockAgent));

        let (client, inbox, _h) = connect(BufReader::new(cli_r), cli_w);

        let init = client
            .request(
                "initialize",
                serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
            )
            .unwrap();
        assert_eq!(init["protocolVersion"], 1);
        assert_eq!(init["agentInfo"]["name"], "mock");

        let new = client
            .request("session/new", serde_json::json!({"cwd": "/tmp"}))
            .unwrap();
        assert_eq!(new["sessionId"], "sess-1");

        let res = client
            .request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": "sess-1",
                    "prompt": [{"type": "text", "text": "hi"}]
                }),
            )
            .unwrap();
        assert_eq!(res["stopReason"], "end_turn");

        let note = inbox.recv().unwrap();
        match note {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "session/update");
                assert_eq!(params["sessionId"], "sess-1");
                assert_eq!(params["update"]["sessionUpdate"], "agent_message_chunk");
                assert_eq!(params["update"]["content"]["text"], "echo: hi");
            }
            Inbound::Request { .. } => panic!("expected a notification"),
        }

        drop(client);
        drop(inbox);
        server.join().unwrap().unwrap();
    }

    struct LoadAgent;

    impl Agent for LoadAgent {
        fn initialize(&mut self, _req: InitializeRequest) -> InitializeResponse {
            InitializeResponse {
                protocol_version: crate::acp::PROTOCOL_VERSION,
                agent_capabilities: AgentCapabilities {
                    load_session: true,
                    prompt_capabilities: PromptCapabilities::default(),
                },
                agent_info: None,
                auth_methods: vec![],
            }
        }

        fn new_session(&mut self, _r: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            Ok(NewSessionResponse {
                session_id: "s1".into(),
            })
        }

        fn prompt(
            &mut self,
            _r: PromptRequest,
            _c: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            Ok(PromptResponse {
                stop_reason: StopReason::EndTurn,
            })
        }

        fn load_session(
            &mut self,
            req: LoadSessionRequest,
            ctx: &PromptContext,
        ) -> Result<(), RpcError> {
            ctx.update(SessionUpdate::AgentMessageChunk(MessageChunk {
                content: ContentBlock::text(format!("history of {}", req.session_id)),
            }));
            Ok(())
        }
    }

    #[test]
    fn session_load_streams_history_then_resolves() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server = thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, LoadAgent));
        let (client, inbox, _h) = connect(BufReader::new(cli_r), cli_w);

        let res = client
            .request(
                "session/load",
                serde_json::json!({"sessionId": "s9", "cwd": "/tmp", "mcpServers": []}),
            )
            .unwrap();
        assert_eq!(res, serde_json::Value::Null);

        match inbox.recv().unwrap() {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "session/update");
                assert_eq!(params["update"]["content"]["text"], "history of s9");
            }
            Inbound::Request { .. } => panic!("expected a notification"),
        }
        drop(client);
        drop(inbox);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn session_load_default_is_method_not_found() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server = thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, MockAgent));
        let (client, _inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        let err = client
            .request(
                "session/load",
                serde_json::json!({"sessionId": "x", "cwd": "/", "mcpServers": []}),
            )
            .unwrap_err();
        assert_eq!(err.code, -32601);
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server = thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, MockAgent));
        let (client, _inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        let err = client
            .request("bogus/method", serde_json::Value::Null)
            .unwrap_err();
        assert_eq!(err.code, -32601);
        drop(client);
        server.join().unwrap().unwrap();
    }
}
