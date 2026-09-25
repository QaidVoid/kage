//! ACP agent server.
//!
//! Drives an injected [`Agent`] over the [`kage_jsonrpc`] peer, conformant
//! with the published ACP spec: it answers `initialize`, `session/new`,
//! `session/load`, `session/list`, `session/resume`,
//! `session/set_config_option` and `session/prompt`, forwards the
//! `session/cancel`
//! notification, and lets the agent stream `session/update`
//! notifications and issue `session/request_permission` requests. A
//! request the agent abandons (a permission ask outlived by its run) is
//! withdrawn with `$/cancel_request`, so the client can close its dialog.
//!
//! Every request except `initialize` runs on its own thread, so the
//! dispatch loop keeps draining inbound messages: a `session/cancel`
//! always lands, and a slow `session/new` never blocks a running prompt.
//! When the input ends, every request but a prompt that is still in
//! flight is answered before [`serve_agent`] returns.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::thread;

use kage_core::CancelFlag;
use kage_jsonrpc::{CancelNotice, Inbound, Peer, RpcError, connect_with};

use crate::acp::{
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionKind, PermissionOutcome, PromptRequest, PromptResponse,
    RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, ToolCallUpdate,
};

/// The client's answer to a `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Run the tool call.
    Allow,
    /// Run the tool call and allow the tool for the rest of the session.
    AllowSession,
    /// Block it, with an optional reason for the model.
    Deny(Option<String>),
}

/// Ask the client to allow `tool_call` once or for the session, or to
/// deny it. Blocks until the client answers or `cancel` is cancelled,
/// and withdraws the ask on a cancel. Never auto-approves: any error,
/// cancel, or rejection resolves to [`PermissionDecision::Deny`].
#[must_use]
pub fn request_permission(
    peer: &Peer,
    session_id: &str,
    tool_call: ToolCallUpdate,
    title: &str,
    cancel: &CancelFlag,
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
                option_id: "allow_session".to_owned(),
                name: format!("Allow {title} for this session"),
                kind: PermissionOptionKind::AllowAlways,
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
    match peer.request_cancellable("session/request_permission", params, cancel) {
        Ok(value) => match serde_json::from_value::<RequestPermissionResponse>(value) {
            Ok(resp) => match resp.outcome {
                PermissionOutcome::Selected(sel) => match sel.option_id.as_str() {
                    "allow" => PermissionDecision::Allow,
                    "allow_session" => PermissionDecision::AllowSession,
                    _ => PermissionDecision::Deny(Some("rejected by client".to_owned())),
                },
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
    fn load_session(
        &self,
        _req: LoadSessionRequest,
        _ctx: &PromptContext,
    ) -> Result<LoadSessionResponse, RpcError> {
        Err(RpcError::method_not_found("session/load"))
    }

    /// List recorded sessions, one page at a time. The default rejects:
    /// only agents that advertise `sessionCapabilities.list` override
    /// it.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the sessions cannot be listed.
    fn list_sessions(&self, _req: ListSessionsRequest) -> Result<ListSessionsResponse, RpcError> {
        Err(RpcError::method_not_found("session/list"))
    }

    /// Reopen a recorded session without replaying its history. The
    /// default rejects: only agents that advertise
    /// `sessionCapabilities.resume` override it.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session id is unknown or cannot
    /// be reopened.
    fn resume_session(
        &self,
        _req: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, RpcError> {
        Err(RpcError::method_not_found("session/resume"))
    }

    /// Change one config option of a session. The default rejects:
    /// only agents that return config options override it.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session, option or value is
    /// unknown.
    fn set_config_option(
        &self,
        _req: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, RpcError> {
        Err(RpcError::method_not_found("session/set_config_option"))
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

    /// The response naming `session_id` to a `session/new`,
    /// `session/load` or `session/resume` was written, so updates for
    /// the session sent from now on cannot overtake it. The default does
    /// nothing.
    fn session_announced(&self, _session_id: &str) {}
}

/// The `$/cancel_request` notice ACP expects for an abandoned request.
fn cancel_notice() -> CancelNotice {
    Arc::new(|id, _method| {
        Some((
            "$/cancel_request".to_owned(),
            serde_json::json!({"requestId": id}),
        ))
    })
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
    let (peer, inbound, _reader) = connect_with(reader, writer, Some(cancel_notice()));
    let agent = Arc::new(make_agent(peer.clone()));

    let mut ops = Vec::new();
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
                if let Some(op) = handle_request(&peer, &agent, id, &method, params) {
                    ops.retain(|op: &thread::JoinHandle<()>| !op.is_finished());
                    ops.push(op);
                }
            }
        }
    }
    for op in ops {
        let _ = op.join();
    }
    Ok(())
}

fn jval<T: serde::Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// Answer request `id` with `op`, run on its own thread.
fn spawn_op<A, F>(
    peer: &Peer,
    agent: &Arc<A>,
    id: serde_json::Value,
    op: F,
) -> thread::JoinHandle<()>
where
    A: Agent,
    F: FnOnce(&A) -> Result<serde_json::Value, RpcError> + Send + 'static,
{
    let agent = Arc::clone(agent);
    let peer = peer.clone();
    thread::spawn(move || {
        let outcome = op(&agent);
        let _ = peer.respond(&id, outcome);
    })
}

/// Answer request `id` with `op`, which opens the session it returns
/// the id of, on its own thread, then tell the agent the client has the
/// response.
fn spawn_open<A, F>(
    peer: &Peer,
    agent: &Arc<A>,
    id: serde_json::Value,
    op: F,
) -> thread::JoinHandle<()>
where
    A: Agent,
    F: FnOnce(&A) -> Result<(String, serde_json::Value), RpcError> + Send + 'static,
{
    let agent = Arc::clone(agent);
    let peer = peer.clone();
    thread::spawn(move || match op(&agent) {
        Ok((session, response)) => {
            let _ = peer.respond(&id, Ok(response));
            agent.session_announced(&session);
        }
        Err(e) => {
            let _ = peer.respond(&id, Err(e));
        }
    })
}

/// Answer one request, on its own thread for all but `initialize`.
/// Returns the thread to wait for at the end of the input, which is
/// every one but a prompt's.
fn handle_request<A: Agent>(
    peer: &Peer,
    agent: &Arc<A>,
    id: serde_json::Value,
    method: &str,
    params: serde_json::Value,
) -> Option<thread::JoinHandle<()>> {
    let op = match method {
        "initialize" => {
            let outcome = parse::<InitializeRequest>(params).map(|req| jval(agent.initialize(req)));
            let _ = peer.respond(&id, outcome);
            return None;
        }
        "session/new" => match parse::<NewSessionRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
                return None;
            }
            Ok(req) => spawn_open(peer, agent, id, move |a| {
                a.new_session(req)
                    .map(|resp| (resp.session_id.clone(), jval(resp)))
            }),
        },
        "session/prompt" => match parse::<PromptRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
                return None;
            }
            Ok(req) => {
                let ctx = PromptContext {
                    peer: peer.clone(),
                    session_id: req.session_id.clone(),
                };
                spawn_op(peer, agent, id, move |a| a.prompt(req, &ctx).map(jval));
                return None;
            }
        },
        "session/load" => match parse::<LoadSessionRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
                return None;
            }
            Ok(req) => {
                let ctx = PromptContext {
                    peer: peer.clone(),
                    session_id: req.session_id.clone(),
                };
                spawn_open(peer, agent, id, move |a| {
                    let session = req.session_id.clone();
                    a.load_session(req, &ctx).map(|resp| (session, jval(resp)))
                })
            }
        },
        "session/list" => match parse::<ListSessionsRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
                return None;
            }
            Ok(req) => spawn_op(peer, agent, id, move |a| a.list_sessions(req).map(jval)),
        },
        "session/resume" => match parse::<ResumeSessionRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
                return None;
            }
            Ok(req) => spawn_open(peer, agent, id, move |a| {
                let session = req.session_id.clone();
                a.resume_session(req).map(|resp| (session, jval(resp)))
            }),
        },
        "session/set_config_option" => match parse::<SetSessionConfigOptionRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
                return None;
            }
            Ok(req) => spawn_op(peer, agent, id, move |a| a.set_config_option(req).map(jval)),
        },
        other => {
            let _ = peer.respond(&id, Err(RpcError::method_not_found(other)));
            return None;
        }
    };
    Some(op)
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use super::*;
    use kage_jsonrpc::connect;

    use crate::acp::{
        AgentCapabilities, ContentBlock, Implementation, MessageChunk, PromptCapabilities,
        SessionInfo, StopReason,
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
                    ..AgentCapabilities::default()
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
                config_options: vec![],
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

    /// Opens sessions slowly and notes which ones it announced.
    struct SlowAgent(Arc<std::sync::Mutex<Vec<String>>>);

    impl Agent for SlowAgent {
        fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
            MockAgent.initialize(req)
        }

        fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            thread::sleep(std::time::Duration::from_millis(100));
            MockAgent.new_session(req)
        }

        fn prompt(
            &self,
            _req: PromptRequest,
            _ctx: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            unreachable!("no prompt is sent")
        }

        fn cancel(&self, _session_id: &str) {}

        fn session_announced(&self, session_id: &str) {
            kage_core::sync::lock(&self.0).push(session_id.to_owned());
        }
    }

    #[test]
    fn a_new_session_is_answered_after_the_input_ends() {
        use std::io::Read as _;

        let (srv_r, mut cli_w) = std::io::pipe().unwrap();
        let (mut cli_r, srv_w) = std::io::pipe().unwrap();
        writeln!(
            cli_w,
            r#"{{"jsonrpc":"2.0","id":1,"method":"session/new","params":{{"cwd":"/tmp"}}}}"#
        )
        .unwrap();
        drop(cli_w);
        let announced = Arc::default();
        let agent = SlowAgent(Arc::clone(&announced));
        serve_agent(BufReader::new(srv_r), srv_w, |_| agent).unwrap();
        assert_eq!(*kage_core::sync::lock(&announced), ["sess-1"]);

        let mut out = String::new();
        cli_r.read_to_string(&mut out).unwrap();
        assert!(out.contains(r#""sessionId":"sess-1""#), "{out}");
    }

    struct LoadAgent;

    impl Agent for LoadAgent {
        fn initialize(&self, _req: InitializeRequest) -> InitializeResponse {
            InitializeResponse {
                protocol_version: crate::acp::PROTOCOL_VERSION,
                agent_capabilities: AgentCapabilities {
                    load_session: true,
                    prompt_capabilities: PromptCapabilities::default(),
                    ..AgentCapabilities::default()
                },
                agent_info: None,
                auth_methods: vec![],
            }
        }

        fn new_session(&self, _r: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            Ok(NewSessionResponse {
                session_id: "s1".into(),
                config_options: vec![],
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
        ) -> Result<LoadSessionResponse, RpcError> {
            ctx.update(SessionUpdate::AgentMessageChunk(MessageChunk {
                content: ContentBlock::text(format!("history of {}", req.session_id)),
            }));
            Ok(LoadSessionResponse::default())
        }

        fn list_sessions(
            &self,
            req: ListSessionsRequest,
        ) -> Result<ListSessionsResponse, RpcError> {
            Ok(ListSessionsResponse {
                sessions: vec![SessionInfo {
                    session_id: "s1".into(),
                    cwd: req.cwd.unwrap_or_default(),
                    title: None,
                    updated_at: None,
                }],
                next_cursor: None,
            })
        }

        fn resume_session(
            &self,
            _req: ResumeSessionRequest,
        ) -> Result<ResumeSessionResponse, RpcError> {
            Ok(ResumeSessionResponse::default())
        }

        fn set_config_option(
            &self,
            req: SetSessionConfigOptionRequest,
        ) -> Result<SetSessionConfigOptionResponse, RpcError> {
            Err(RpcError::new(
                -32602,
                format!("unknown option {}", req.config_id),
            ))
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
        assert_eq!(res, serde_json::json!({}));

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
    fn session_methods_dispatch_to_the_agent() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| LoadAgent));
        let (client, _inbox, _h) = connect(BufReader::new(cli_r), cli_w);

        let list = client
            .request("session/list", serde_json::json!({"cwd": "/w"}))
            .unwrap();
        assert_eq!(
            list,
            serde_json::json!({"sessions": [{"sessionId": "s1", "cwd": "/w"}]})
        );
        let resume = client
            .request(
                "session/resume",
                serde_json::json!({"sessionId": "s1", "cwd": "/w", "mcpServers": []}),
            )
            .unwrap();
        assert_eq!(resume, serde_json::json!({}));
        let err = client
            .request(
                "session/set_config_option",
                serde_json::json!({"sessionId": "s1", "configId": "nope", "value": "x"}),
            )
            .unwrap_err();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "unknown option nope");
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn new_session_methods_default_to_method_not_found() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| MockAgent));
        let (client, _inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        for (method, params) in [
            ("session/list", serde_json::json!({})),
            (
                "session/resume",
                serde_json::json!({"sessionId": "x", "cwd": "/", "mcpServers": []}),
            ),
            (
                "session/set_config_option",
                serde_json::json!({"sessionId": "x", "configId": "model", "value": "m"}),
            ),
        ] {
            let err = client.request(method, params).unwrap_err();
            assert_eq!(err.code, -32601, "{method}");
        }
        drop(client);
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

    /// Asks permission on every prompt and gives up once cancelled.
    #[derive(Default)]
    struct AskAgent {
        cancel: CancelFlag,
    }

    impl Agent for AskAgent {
        fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
            MockAgent.initialize(req)
        }

        fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            MockAgent.new_session(req)
        }

        fn prompt(
            &self,
            req: PromptRequest,
            ctx: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            let tool_call = ToolCallUpdate {
                tool_call_id: "call-1".into(),
                ..ToolCallUpdate::default()
            };
            let decision =
                request_permission(ctx.peer(), &req.session_id, tool_call, "bash", &self.cancel);
            assert!(matches!(decision, PermissionDecision::Deny(_)));
            Ok(PromptResponse {
                stop_reason: StopReason::Cancelled,
            })
        }

        fn cancel(&self, _session_id: &str) {
            self.cancel.cancel();
        }
    }

    #[test]
    fn abandoned_permission_ask_sends_cancel_request() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server = thread::spawn(move || {
            serve_agent(BufReader::new(srv_r), srv_w, |_| AskAgent::default())
        });
        let (client, inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        let prompt = {
            let client = client.clone();
            thread::spawn(move || {
                client.request(
                    "session/prompt",
                    serde_json::json!({"sessionId": "sess-1", "prompt": []}),
                )
            })
        };
        let timeout = std::time::Duration::from_secs(5);
        let Ok(Inbound::Request { id, method, .. }) = inbox.recv_timeout(timeout) else {
            panic!("expected the permission request");
        };
        assert_eq!(method, "session/request_permission");
        client
            .notify("session/cancel", serde_json::json!({"sessionId": "sess-1"}))
            .unwrap();
        match inbox.recv_timeout(timeout) {
            Ok(Inbound::Notification { method, params }) => {
                assert_eq!(method, "$/cancel_request");
                assert_eq!(params, serde_json::json!({"requestId": id}));
            }
            other => panic!("expected $/cancel_request, got {other:?}"),
        }
        let res = prompt.join().unwrap().unwrap();
        assert_eq!(res["stopReason"], "cancelled");
        drop(client);
        drop(inbox);
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
