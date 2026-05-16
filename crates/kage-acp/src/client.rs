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
//! upstream `session/request_permission` is **rejected** - a provider
//! has no seam to forward it to kage's own permission hook, and
//! nothing is ever auto-approved.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use kage_core::{CancelFlag, Content, Message, Role, TokenUsage};
use kage_provider::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, StopReason,
    StreamRequest,
};

use crate::acp::{
    ClientCapabilities, ContentBlock, Implementation, InitializeRequest, NewSessionRequest,
    NewSessionResponse, PROTOCOL_VERSION, PermissionOptionKind, PromptRequest,
    RequestPermissionRequest, SelectedOption, SessionNotification, SessionUpdate,
};
use crate::jsonrpc::{self, Inbound, Peer, RpcError};

/// A [`Provider`] backed by an external ACP agent process.
#[derive(Debug)]
pub struct AcpProvider {
    command: Vec<String>,
    metadata: ProviderMetadata,
}

impl AcpProvider {
    /// Build an adapter that spawns `command` (argv; first element is
    /// the executable) for each turn.
    #[must_use]
    pub fn new<I, S>(command: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            command: command.into_iter().map(Into::into).collect(),
            metadata: ProviderMetadata {
                id: "acp".to_owned(),
                display_name: "ACP agent".to_owned(),
                supports_caching: false,
                supports_thinking: true,
                supports_tool_use: false,
            },
        }
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

/// Pick a reject option's id, so an upstream permission prompt is
/// always denied (never auto-approved).
fn reject_option_id(req: &RequestPermissionRequest) -> Option<String> {
    req.options
        .iter()
        .find(|o| {
            matches!(
                o.kind,
                PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
            )
        })
        .map(|o| o.option_id.clone())
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
                        let outcome = serde_json::from_value::<RequestPermissionRequest>(params)
                            .ok()
                            .and_then(|r| reject_option_id(&r))
                            .map_or_else(
                                || serde_json::json!({"outcome": {"outcome": "cancelled"}}),
                                |id| {
                                    serde_json::json!({"outcome": serde_json::to_value(
                                        crate::acp::PermissionOutcome::Selected(
                                            SelectedOption { option_id: id },
                                        )
                                    )
                                    .unwrap_or(serde_json::Value::Null)})
                                },
                            );
                        Ok(outcome)
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
) -> Result<AcpClientStream, ProviderError>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let (peer, inbound, _rh) = jsonrpc::connect(reader, writer);

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
        let (cmd, args) = self
            .command
            .split_first()
            .ok_or_else(|| ProviderError::Transport("acp: empty command".to_owned()))?;
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| ProviderError::Transport(format!("acp: spawn {cmd}: {e}")))?;
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

        let mut stream =
            run_turn(BufReader::new(stdout), stdin, prompt, cwd, cancel).inspect_err(|_| {
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
}
