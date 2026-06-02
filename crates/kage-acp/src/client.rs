//! ACP client adapter: drive another ACP agent as a kage provider.
//!
//! [`AcpProvider`] implements [`kage_provider::Provider`] by speaking
//! real ACP (newline-delimited JSON-RPC 2.0, protocol version 1) as
//! the *client* to an external agent process - `claude-code-acp`,
//! `goose`, `gemini` in ACP mode, or another `kage rpc`. The latest
//! user turn is forwarded as a `session/prompt`; the agent's
//! `session/update` stream is translated back into [`ProviderEvent`]s
//! so kage's loop can stack on top of another agent.
//!
//! ## v1 scope and limitations
//!
//! The upstream agent runs its *own* tool loop; only its assistant
//! text and thinking are surfaced (`supports_tool_use` is `false`).
//! kage advertises **no** `fs`/`terminal` client capabilities, so a
//! conformant agent will not ask kage to touch the filesystem. An
//! upstream `session/request_permission` is routed to the injected
//! [`PermissionResolver`] (the host backs it with
//! `kage.on_acp_permission`); with no resolver, or on deny, the call
//! is rejected. kage never auto-approves an upstream agent's tools.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use kage_core::{CancelFlag, Content, Message, Role, TokenUsage};
use kage_jsonrpc::{Inbound, Peer, RpcError, connect};
use kage_provider::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, StopReason,
    StreamRequest,
};

use crate::acp::{
    ClientCapabilities, ContentBlock, Implementation, InitializeRequest, NewSessionRequest,
    NewSessionResponse, PROTOCOL_VERSION, PermissionOption, PermissionOptionKind,
    PermissionOutcome, PromptRequest, RequestPermissionRequest, RequestPermissionResponse,
    SelectedOption, SessionNotification, SessionUpdate,
};
use crate::agent::PermissionDecision;

/// Decides whether an upstream agent's tool call is permitted. Called
/// on the client's drain thread (`Send + Sync`); must not block on
/// the agent loop. The default (no resolver) denies - kage never
/// auto-approves an upstream agent's tools.
pub type PermissionResolver =
    Arc<dyn Fn(&RequestPermissionRequest) -> PermissionDecision + Send + Sync>;

/// Supplies additional agents not in static config (e.g. ones a
/// plugin declared via `kage.acp.add_agent`). Consulted at
/// `stream()` time so it can see runtime-registered agents. The host
/// injects it; `None` means config is the only source.
pub type AgentSource = Arc<dyn Fn() -> Vec<(String, kage_core::config::AcpAgent)> + Send + Sync>;

/// A [`Provider`] that multiplexes over user-configured external ACP
/// agents. The model id selects the agent: `acp:<name>` resolves to
/// `req.model == "<name>"`, looked up in the configured agents map.
pub struct AcpProvider {
    agents: std::collections::BTreeMap<String, kage_core::config::AcpAgent>,
    metadata: ProviderMetadata,
    permission: Option<PermissionResolver>,
    agent_source: Option<AgentSource>,
}

impl std::fmt::Debug for AcpProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpProvider")
            .field("agents", &self.agents)
            .field("metadata", &self.metadata)
            .field("permission", &self.permission.as_ref().map(|_| "set"))
            .field("agent_source", &self.agent_source.as_ref().map(|_| "set"))
            .finish()
    }
}

impl AcpProvider {
    /// Build the provider from `[acp.agents.*]` config.
    #[must_use]
    pub fn from_config(acp: &kage_core::config::AcpConfig) -> Self {
        Self {
            agents: acp.agents.clone(),
            metadata: ProviderMetadata {
                id: "acp".to_owned(),
                display_name: "ACP agent".to_owned(),
                supports_caching: false,
                supports_thinking: true,
                supports_tool_use: false,
            },
            permission: None,
            agent_source: None,
        }
    }

    /// Set the resolver consulted when an upstream agent asks to run
    /// a tool. Without one, every upstream permission ask is denied.
    #[must_use]
    pub fn with_permission(mut self, resolver: PermissionResolver) -> Self {
        self.permission = Some(resolver);
        self
    }

    /// Set a source of additional agents resolved at `stream()` time
    /// (e.g. plugin-declared agents). Static config still takes
    /// precedence on a name clash.
    #[must_use]
    pub fn with_agent_source(mut self, source: AgentSource) -> Self {
        self.agent_source = Some(source);
        self
    }

    fn resolve_agent(&self, name: &str) -> Option<kage_core::config::AcpAgent> {
        if let Some(a) = self.agents.get(name) {
            return Some(a.clone());
        }
        self.agent_source
            .as_ref()
            .and_then(|src| src().into_iter().find(|(n, _)| n == name).map(|(_, a)| a))
    }
}

fn last_user_text(messages: &[Message]) -> Option<String> {
    let msg = messages.iter().rev().find(|m| m.role == Role::User)?;
    Some(
        msg.content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn rpc_to_provider(e: RpcError) -> ProviderError {
    let RpcError { code, message } = e;
    if code == -32800 {
        ProviderError::Cancelled
    } else {
        ProviderError::Transport(format!("acp: {message}"))
    }
}

/// Translate one `session/update` payload into a provider event.
/// `None` for updates kage's loop has no slot for (the upstream
/// agent's own tool calls, plans, mode changes).
fn translate(update: &SessionUpdate) -> Option<ProviderEvent> {
    let chunk = match update {
        SessionUpdate::AgentMessageChunk(c) => {
            return c.content.as_text().map(|t| ProviderEvent::TextDelta {
                delta: t.to_owned(),
            });
        }
        SessionUpdate::AgentThoughtChunk(c) => Some(c),
        _ => None,
    };
    chunk.and_then(|c| {
        c.content.as_text().map(|t| ProviderEvent::ThinkingDelta {
            delta: t.to_owned(),
        })
    })
}

fn option_id(req: &RequestPermissionRequest, allow: bool) -> Option<String> {
    let want = |o: &&PermissionOption| {
        let is_allow = matches!(
            o.kind,
            PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
        );
        is_allow == allow
    };
    req.options.iter().find(want).map(|o| o.option_id.clone())
}

/// Resolve an upstream `session/request_permission` into the JSON-RPC
/// result value. Without a resolver, or on `Deny`, a reject option is
/// selected (kage never auto-approves an upstream agent's tools).
fn permission_result(
    resolver: Option<&PermissionResolver>,
    req: &RequestPermissionRequest,
) -> serde_json::Value {
    let allow = matches!(resolver.map(|r| r(req)), Some(PermissionDecision::Allow));
    let outcome = match option_id(req, allow) {
        Some(option_id) => PermissionOutcome::Selected(SelectedOption { option_id }),
        None => PermissionOutcome::Cancelled,
    };
    serde_json::to_value(RequestPermissionResponse { outcome }).unwrap_or(serde_json::Value::Null)
}

/// The translating iterator returned by [`AcpProvider::stream`].
struct AcpClientStream {
    rx: Receiver<Result<ProviderEvent, ProviderError>>,
    child: Option<Child>,
    finished: bool,
    shutdown: Arc<AtomicBool>,
}

impl Iterator for AcpClientStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let Ok(item) = self.rx.recv() else {
            self.finished = true;
            return None;
        };
        if matches!(item, Ok(ProviderEvent::MessageEnd { .. }) | Err(_)) {
            self.finished = true;
        }
        Some(item)
    }
}

impl Drop for AcpClientStream {
    fn drop(&mut self) {
        // Tear the turn down: the drain/prompt threads observe this
        // and unwind even when there is no child to kill.
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Slot the prompt thread parks its terminal event in; the drain
/// thread emits it only once `inbound` is empty, preserving wire
/// order (the upstream sends every `session/update` before it
/// answers `session/prompt`).
type Terminal = Arc<std::sync::Mutex<Option<Result<ProviderEvent, ProviderError>>>>;

type Events = mpsc::Sender<Result<ProviderEvent, ProviderError>>;

/// Drain inbound: translate notifications to events, reject upstream
/// permission asks, and - once the prompt has resolved and every
/// buffered notification is forwarded - emit the terminal event.
fn spawn_drain(
    inbound: Receiver<Inbound>,
    peer: Peer,
    tx: Events,
    shutdown: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    terminal: Terminal,
    resolver: Option<PermissionResolver>,
) {
    thread::spawn(move || {
        loop {
            let message = match inbound.recv_timeout(Duration::from_millis(50)) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => {
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    if done.load(Ordering::SeqCst) {
                        if let Some(item) =
                            terminal.lock().expect("acp terminal mutex poisoned").take()
                        {
                            let _ = tx.send(item);
                        }
                        break;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    if let Some(item) = terminal.lock().expect("acp terminal mutex poisoned").take()
                    {
                        let _ = tx.send(item);
                    }
                    break;
                }
            };
            match message {
                Inbound::Notification { method, params } if method == "session/update" => {
                    if let Ok(note) = serde_json::from_value::<SessionNotification>(params)
                        && let Some(ev) = translate(&note.update)
                        && tx.send(Ok(ev)).is_err()
                    {
                        break;
                    }
                }
                Inbound::Notification { .. } => {}
                Inbound::Request { id, method, params } => {
                    let reply = if method == "session/request_permission" {
                        match serde_json::from_value::<RequestPermissionRequest>(params) {
                            Ok(req) => Ok(permission_result(resolver.as_ref(), &req)),
                            Err(e) => Err(RpcError::new(-32602, format!("bad params: {e}"))),
                        }
                    } else {
                        Err(RpcError::method_not_found(&method))
                    };
                    let _ = peer.respond(&id, reply);
                }
            }
        }
    });
}

/// Drive `session/prompt`; park its outcome for the drain thread to
/// emit in order, then mark the turn done.
fn spawn_prompt(
    peer: Peer,
    params: serde_json::Value,
    cancel: CancelFlag,
    shutdown: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    terminal: Terminal,
) {
    thread::spawn(move || {
        let poll_shutdown = Arc::clone(&shutdown);
        let outcome = peer.request_cancellable("session/prompt", params, &move || {
            cancel.is_cancelled() || poll_shutdown.load(Ordering::SeqCst)
        });
        let item = match outcome {
            Ok(_) => Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
            Err(e) => Err(rpc_to_provider(e)),
        };
        *terminal.lock().expect("acp terminal mutex poisoned") = Some(item);
        done.store(true, Ordering::SeqCst);
    });
}

/// Run a full ACP client turn over an established transport: hand
/// shake, open a session, forward the prompt, and stream the agent's
/// reply back as provider events. Generic over the transport so it is
/// unit-testable against an in-process [`crate::agent::serve_agent`].
fn run_turn<R, W>(
    reader: R,
    writer: W,
    prompt: String,
    cwd: String,
    cancel: &CancelFlag,
    resolver: Option<PermissionResolver>,
) -> Result<AcpClientStream, ProviderError>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let (peer, inbound, _rh) = connect(reader, writer);

    let init = InitializeRequest {
        protocol_version: PROTOCOL_VERSION,
        client_capabilities: ClientCapabilities::default(),
        client_info: Some(Implementation {
            name: "kage".to_owned(),
            title: None,
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }),
    };
    peer.request(
        "initialize",
        serde_json::to_value(&init).map_err(|e| ProviderError::Decode(e.to_string()))?,
    )
    .map_err(rpc_to_provider)?;

    let new_session = NewSessionRequest {
        cwd,
        mcp_servers: vec![],
    };
    let session: NewSessionResponse = serde_json::from_value(
        peer.request(
            "session/new",
            serde_json::to_value(&new_session).map_err(|e| ProviderError::Decode(e.to_string()))?,
        )
        .map_err(rpc_to_provider)?,
    )
    .map_err(|e| ProviderError::Decode(format!("session/new: {e}")))?;

    let (tx, rx) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let terminal: Terminal = Arc::new(std::sync::Mutex::new(None));

    let prompt_req = PromptRequest {
        session_id: session.session_id,
        prompt: vec![ContentBlock::text(prompt)],
    };
    let params =
        serde_json::to_value(&prompt_req).map_err(|e| ProviderError::Decode(e.to_string()))?;

    spawn_drain(
        inbound,
        peer.clone(),
        tx,
        Arc::clone(&shutdown),
        Arc::clone(&done),
        Arc::clone(&terminal),
        resolver,
    );
    spawn_prompt(
        peer,
        params,
        cancel.clone(),
        Arc::clone(&shutdown),
        done,
        terminal,
    );

    Ok(AcpClientStream {
        rx,
        child: None,
        finished: false,
        shutdown,
    })
}

impl Provider for AcpProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn stream(
        &self,
        req: StreamRequest,
        cancel: &CancelFlag,
    ) -> Result<EventStream, ProviderError> {
        let prompt = last_user_text(&req.messages)
            .ok_or_else(|| ProviderError::Decode("acp: no user message to forward".to_owned()))?;
        let agent = self
            .resolve_agent(&req.model)
            .ok_or_else(|| ProviderError::UnknownModel(format!("acp:{}", req.model)))?;
        let mut child = Command::new(&agent.command)
            .args(&agent.args)
            .envs(&agent.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| ProviderError::Transport(format!("acp: spawn {}: {e}", agent.command)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProviderError::Transport("acp: no child stdin".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::Transport("acp: no child stdout".to_owned()))?;
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        let mut stream = run_turn(
            BufReader::new(stdout),
            stdin,
            prompt,
            cwd,
            cancel,
            self.permission.clone(),
        )
        .inspect_err(|_| {
            let _ = child.kill();
            let _ = child.wait();
        })?;
        stream.child = Some(child);
        Ok(Box::new(stream))
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use super::*;
    use crate::acp::{
        AgentCapabilities, InitializeResponse, MessageChunk, NewSessionResponse, PromptResponse,
    };
    use crate::agent::{Agent, PromptContext, serve_agent};

    struct EchoAgent;

    impl Agent for EchoAgent {
        fn initialize(&mut self, _req: InitializeRequest) -> InitializeResponse {
            InitializeResponse {
                protocol_version: PROTOCOL_VERSION,
                agent_capabilities: AgentCapabilities::default(),
                agent_info: None,
                auth_methods: vec![],
            }
        }

        fn new_session(&mut self, _req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            Ok(NewSessionResponse {
                session_id: "s1".to_owned(),
            })
        }

        fn prompt(
            &mut self,
            req: PromptRequest,
            ctx: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            let text = req
                .prompt
                .first()
                .and_then(ContentBlock::as_text)
                .unwrap_or_default()
                .to_owned();
            ctx.update(SessionUpdate::AgentThoughtChunk(MessageChunk {
                content: ContentBlock::text("thinking"),
            }));
            ctx.update(SessionUpdate::AgentMessageChunk(MessageChunk {
                content: ContentBlock::text(format!("echo: {text}")),
            }));
            Ok(PromptResponse {
                stop_reason: crate::acp::StopReason::EndTurn,
            })
        }
    }

    #[test]
    fn consumes_an_upstream_acp_agent() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server = thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, EchoAgent));

        let cancel = CancelFlag::new();
        let stream = run_turn(
            BufReader::new(cli_r),
            cli_w,
            "hi".to_owned(),
            "/tmp".to_owned(),
            &cancel,
            None,
        )
        .expect("turn starts");

        let events: Vec<_> = stream.take(3).map(Result::unwrap).collect();
        assert!(matches!(
            &events[0],
            ProviderEvent::ThinkingDelta { delta } if delta == "thinking"
        ));
        assert!(matches!(
            &events[1],
            ProviderEvent::TextDelta { delta } if delta == "echo: hi"
        ));
        assert!(matches!(
            events[2],
            ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ));
        server.join().unwrap().unwrap();
    }

    /// The agent asks for permission; the kage ACP client always
    /// rejects (never auto-approves). Exercises `session/request_
    /// permission` flowing agent -> client over the real wire.
    struct PermissionAgent;

    impl Agent for PermissionAgent {
        fn initialize(&mut self, _req: InitializeRequest) -> InitializeResponse {
            InitializeResponse {
                protocol_version: PROTOCOL_VERSION,
                agent_capabilities: AgentCapabilities::default(),
                agent_info: None,
                auth_methods: vec![],
            }
        }

        fn new_session(&mut self, _req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            Ok(NewSessionResponse {
                session_id: "s1".to_owned(),
            })
        }

        fn prompt(
            &mut self,
            _req: PromptRequest,
            ctx: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            let decision = ctx
                .permission()
                .request("bash", serde_json::json!({"cmd": "ls"}));
            let verdict = match decision {
                crate::agent::PermissionDecision::Allow => "allowed",
                crate::agent::PermissionDecision::Deny(_) => "denied",
            };
            ctx.update(SessionUpdate::AgentMessageChunk(MessageChunk {
                content: ContentBlock::text(verdict),
            }));
            Ok(PromptResponse {
                stop_reason: crate::acp::StopReason::EndTurn,
            })
        }
    }

    #[test]
    fn client_rejects_upstream_permission_requests() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, PermissionAgent));

        let cancel = CancelFlag::new();
        let stream = run_turn(
            BufReader::new(cli_r),
            cli_w,
            "do it".to_owned(),
            "/tmp".to_owned(),
            &cancel,
            None,
        )
        .expect("turn starts");

        let events: Vec<_> = stream.take(2).map(Result::unwrap).collect();
        assert!(
            matches!(&events[0], ProviderEvent::TextDelta { delta } if delta == "denied"),
            "no resolver must deny the upstream permission ask"
        );
        assert!(matches!(events[1], ProviderEvent::MessageEnd { .. }));
        server.join().unwrap().unwrap();
    }

    #[test]
    fn resolver_allow_lets_the_upstream_proceed() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, PermissionAgent));

        let resolver: PermissionResolver = Arc::new(|_req| PermissionDecision::Allow);
        let cancel = CancelFlag::new();
        let stream = run_turn(
            BufReader::new(cli_r),
            cli_w,
            "do it".to_owned(),
            "/tmp".to_owned(),
            &cancel,
            Some(resolver),
        )
        .expect("turn starts");

        let events: Vec<_> = stream.take(2).map(Result::unwrap).collect();
        assert!(
            matches!(&events[0], ProviderEvent::TextDelta { delta } if delta == "allowed"),
            "an Allow resolver must let the upstream tool run"
        );
        assert!(matches!(events[1], ProviderEvent::MessageEnd { .. }));
        server.join().unwrap().unwrap();
    }
}
