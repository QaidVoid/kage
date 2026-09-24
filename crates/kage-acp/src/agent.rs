//! ACP agent server.
//!
//! Drives an injected [`Agent`] over the [`kage_jsonrpc`] peer, conformant
//! with the published ACP spec: it answers `initialize`, `session/new`,
//! `session/load` and `session/prompt`, forwards the `session/cancel`
//! notification, and lets the agent stream `session/update`
//! notifications and issue `session/request_permission` requests.
//!
//! Every request except `initialize` runs on its own thread, so the
//! dispatch loop keeps draining inbound messages: a `session/cancel`
//! always lands, and a slow `session/new` never blocks a running prompt.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::thread;

use kage_jsonrpc::{Inbound, Peer, RpcError, connect};

use crate::acp::{
    InitializeRequest, InitializeResponse, LoadSessionRequest, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, PermissionOutcome, PromptRequest,
    PromptResponse, RequestPermissionRequest, RequestPermissionResponse, SessionNotification,
    SessionUpdate, ToolCallUpdate,
};

/// The client's answer to a `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Run the tool call.
    Allow,
    /// Block it, with an optional reason for the model.
    Deny(Option<String>),
}

/// Ask the client to allow or deny `tool_call`. Blocks until the client
/// answers or `cancelled` returns `true`. Never auto-approves: any error,
/// cancel, or rejection resolves to [`PermissionDecision::Deny`].
pub fn request_permission(
    peer: &Peer,
    session_id: &str,
    tool_call: ToolCallUpdate,
    title: &str,
    cancelled: &dyn Fn() -> bool,
) -> PermissionDecision {
    let req = RequestPermissionRequest {
        session_id: session_id.to_owned(),
        tool_call,
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
    match peer.request_cancellable("session/request_permission", params, cancelled) {
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

/// Handed to [`Agent::prompt`] and [`Agent::load_session`]: streams
/// updates for one session.
pub struct PromptContext {
    peer: Peer,
    session_id: String,
}

impl PromptContext {
    /// Emit a `session/update` notification for this session.
    pub fn update(&self, update: SessionUpdate) {
        send_update(&self.peer, &self.session_id, update);
    }

    /// The underlying peer, for agent-initiated requests.
    #[must_use]
    pub fn peer(&self) -> &Peer {
        &self.peer
    }

    /// The session this call belongs to.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// Emit a `session/update` notification for `session_id` on `peer`.
pub fn send_update(peer: &Peer, session_id: &str, update: SessionUpdate) {
    let note = SessionNotification {
        session_id: session_id.to_owned(),
        update,
    };
    if let Ok(params) = serde_json::to_value(&note) {
        let _ = peer.notify("session/update", params);
    }
}

/// The host-supplied agent the server drives. The server owns the
/// protocol; this trait owns the agent. Methods take `&self` and may run
/// concurrently on different sessions.
pub trait Agent: Send + Sync + 'static {
    /// Handshake. Return the agent's capabilities and identity.
    fn initialize(&self, req: InitializeRequest) -> InitializeResponse;

    /// Create a session for `cwd`.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if a session cannot be created.
    fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError>;

    /// Resume a previously recorded session, streaming its history
    /// back through `ctx` as `session/update` notifications. The
    /// default rejects: only agents that advertise
    /// `agentCapabilities.loadSession` override it.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session id is unknown or its
    /// history cannot be replayed.
    fn load_session(&self, _req: LoadSessionRequest, _ctx: &PromptContext) -> Result<(), RpcError> {
        Err(RpcError::method_not_found("session/load"))
    }

    /// Run one prompt turn to completion, streaming `session/update`
    /// notifications through `ctx`.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the turn cannot start or fails
    /// irrecoverably (streamed progress already reached the client).
    fn prompt(&self, req: PromptRequest, ctx: &PromptContext) -> Result<PromptResponse, RpcError>;

    /// The client asked to cancel the running prompt of `session_id`.
    fn cancel(&self, session_id: &str);
}

fn parse<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))
}

/// Serve the ACP agent protocol over `reader`/`writer` until the peer
/// disconnects. `make_agent` receives the peer so the agent can send
/// notifications and requests outside a prompt call.
///
/// # Errors
///
/// Returns an [`RpcError`] only for a fatal transport failure; a
/// clean disconnect is `Ok(())`.
pub fn serve_agent<R, W, A, F>(reader: R, writer: W, make_agent: F) -> Result<(), RpcError>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
    A: Agent,
    F: FnOnce(Peer) -> A,
{
    let (peer, inbound, _reader) = connect(reader, writer);
    let agent = Arc::new(make_agent(peer.clone()));

    for message in inbound {
        match message {
            Inbound::Notification { method, params } => {
                if method == "session/cancel"
                    && let Ok(c) = parse::<crate::acp::CancelNotification>(params)
                {
                    agent.cancel(&c.session_id);
                }
            }
            Inbound::Request { id, method, params } => {
                handle_request(&peer, &agent, id, &method, params);
            }
        }
    }
    Ok(())
}

fn jval<T: serde::Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// Answer request `id` with `op`, run on its own thread.
fn spawn_op<A, F>(peer: &Peer, agent: &Arc<A>, id: serde_json::Value, op: F)
where
    A: Agent,
    F: FnOnce(&A) -> Result<serde_json::Value, RpcError> + Send + 'static,
{
    let agent = Arc::clone(agent);
    let peer = peer.clone();
    thread::spawn(move || {
        let outcome = op(&agent);
        let _ = peer.respond(&id, outcome);
    });
}

fn handle_request<A: Agent>(
    peer: &Peer,
    agent: &Arc<A>,
    id: serde_json::Value,
    method: &str,
    params: serde_json::Value,
) {
    match method {
        "initialize" => {
            let outcome = parse::<InitializeRequest>(params).map(|req| jval(agent.initialize(req)));
            let _ = peer.respond(&id, outcome);
        }
        "session/new" => match parse::<NewSessionRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
            }
            Ok(req) => spawn_op(peer, agent, id, move |a| a.new_session(req).map(jval)),
        },
        "session/prompt" => match parse::<PromptRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
            }
            Ok(req) => {
                let ctx = PromptContext {
                    peer: peer.clone(),
                    session_id: req.session_id.clone(),
                };
                spawn_op(peer, agent, id, move |a| a.prompt(req, &ctx).map(jval));
            }
        },
        "session/load" => match parse::<LoadSessionRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
            }
            Ok(req) => {
                let ctx = PromptContext {
                    peer: peer.clone(),
                    session_id: req.session_id.clone(),
                };
                spawn_op(peer, agent, id, move |a| {
                    a.load_session(req, &ctx).map(|()| serde_json::Value::Null)
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
        fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
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

        fn new_session(&self, _req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            Ok(NewSessionResponse {
                session_id: "sess-1".into(),
            })
        }

        fn prompt(
            &self,
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
                stop_reason: StopReason::EndTurn,
            })
        }

        fn cancel(&self, _session_id: &str) {}
    }

    #[test]
    fn initialize_new_prompt_round_trip() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| MockAgent));

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
        fn initialize(&self, _req: InitializeRequest) -> InitializeResponse {
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

        fn new_session(&self, _r: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            Ok(NewSessionResponse {
                session_id: "s1".into(),
            })
        }

        fn prompt(
            &self,
            _r: PromptRequest,
            _c: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            Ok(PromptResponse {
                stop_reason: StopReason::EndTurn,
            })
        }

        fn cancel(&self, _session_id: &str) {}

        fn load_session(
            &self,
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
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| LoadAgent));
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
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| MockAgent));
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
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| MockAgent));
        let (client, _inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        let err = client
            .request("bogus/method", serde_json::Value::Null)
            .unwrap_err();
        assert_eq!(err.code, -32601);
        drop(client);
        server.join().unwrap().unwrap();
    }
}
