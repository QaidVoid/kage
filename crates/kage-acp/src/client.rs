//! ACP client adapter: drive another ACP agent as a kage provider.
//!
//! [`AcpProvider`] implements [`kage_provider::Provider`] by spawning
//! an external ACP agent (anything that speaks `kage rpc`'s protocol)
//! and forwarding the latest user turn to it as a `prompt`. The
//! upstream agent's streamed text and thinking are translated back
//! into [`ProviderEvent`]s so kage's loop can consume another agent
//! as if it were a model. This is the "stacking" seam: `kage -> kage`,
//! `kage -> goose`, `kage -> claude-code`.
//!
//! ## v1 scope and limitations
//!
//! The upstream agent runs its *own* tool loop; only its assistant
//! text and thinking reach kage. Upstream tool-call events are not
//! relayed (`supports_tool_use` is `false`), and an upstream
//! `permission/request` is **denied**, never auto-approved, because a
//! provider has no seam to forward it to kage's permission hook. Use
//! this adapter with upstream agents that complete a turn from text
//! alone, or accept that the upstream cannot use tools through it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use kage_core::{CancelFlag, Content, Message, Role, TokenUsage};
use kage_provider::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, StopReason,
    StreamRequest,
};

use crate::framing;

/// JSON-RPC id used for the forwarded `prompt`.
const PROMPT_ID: i64 = 1;

/// Outcome of classifying one upstream message.
enum Step {
    /// Surface this provider event (or error) to the caller.
    Yield(Result<ProviderEvent, ProviderError>),
    /// Nothing to surface; read the next message.
    Continue,
    /// The stream is finished; no more events.
    Stop,
}

/// A [`Provider`] backed by an external ACP agent process.
#[derive(Debug)]
pub struct AcpProvider {
    command: Vec<String>,
    metadata: ProviderMetadata,
}

impl AcpProvider {
    /// Build an adapter that spawns `command` (argv; the first element
    /// is the executable) for each turn.
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
    let text = msg
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(text)
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
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProviderError::Transport("acp: no child stdin".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::Transport("acp: no child stdout".to_owned()))?;

        framing::write_message(
            &mut stdin,
            &serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "initialize"}),
        )
        .map_err(|e| ProviderError::Transport(format!("acp: {e}")))?;
        framing::write_message(
            &mut stdin,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": PROMPT_ID,
                "method": "prompt",
                "params": { "prompt": prompt },
            }),
        )
        .map_err(|e| ProviderError::Transport(format!("acp: {e}")))?;

        Ok(Box::new(AcpStream {
            reader: BufReader::new(stdout),
            stdin,
            child: Some(child),
            cancel: cancel.clone(),
            done: false,
            ended: false,
        }))
    }
}

/// The translating iterator returned by [`AcpProvider::stream`].
/// Generic over the transport so it is unit-testable with in-memory
/// streams and no child process.
struct AcpStream<R: BufRead, W: Write> {
    reader: R,
    stdin: W,
    child: Option<Child>,
    cancel: CancelFlag,
    done: bool,
    ended: bool,
}

impl<R: BufRead, W: Write> AcpStream<R, W> {
    fn finish(&mut self) {
        self.done = true;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn end_turn(&mut self) -> Step {
        self.finish();
        if self.ended {
            return Step::Stop;
        }
        self.ended = true;
        Step::Yield(Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }))
    }

    /// Classify one upstream message into a [`Step`], performing any
    /// side effect it requires (denying a permission request, killing
    /// the child on the prompt response).
    fn classify(&mut self, value: &serde_json::Value) -> Step {
        match value.get("method").and_then(serde_json::Value::as_str) {
            Some("event") => self.classify_event(value),
            Some("permission/request") => {
                if let Some(id) = value
                    .get("params")
                    .and_then(|p| p.get("id"))
                    .and_then(serde_json::Value::as_str)
                {
                    let _ = framing::write_message(
                        &mut self.stdin,
                        &serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "permission/respond",
                            "params": {
                                "id": id,
                                "allow": false,
                                "reason": "kage acp provider does not forward tool permissions",
                            },
                        }),
                    );
                }
                Step::Continue
            }
            _ => {
                if value.get("id") != Some(&serde_json::json!(PROMPT_ID)) {
                    return Step::Continue;
                }
                if let Some(err) = value.get("error") {
                    self.finish();
                    let msg = err
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("acp prompt error")
                        .to_owned();
                    return Step::Yield(Err(ProviderError::Transport(msg)));
                }
                self.end_turn()
            }
        }
    }

    fn classify_event(&mut self, value: &serde_json::Value) -> Step {
        let ev = value
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let delta = || {
            ev.get("delta")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        match ev.get("type").and_then(serde_json::Value::as_str) {
            Some("message_start") => Step::Yield(Ok(ProviderEvent::MessageStart)),
            Some("text_delta") => Step::Yield(Ok(ProviderEvent::TextDelta { delta: delta() })),
            Some("thinking_delta") => {
                Step::Yield(Ok(ProviderEvent::ThinkingDelta { delta: delta() }))
            }
            Some("message_end") => {
                self.ended = true;
                Step::Yield(Ok(ProviderEvent::MessageEnd {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                }))
            }
            Some("error") => {
                self.finish();
                let msg = ev
                    .get("kind")
                    .and_then(|k| k.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("acp upstream error")
                    .to_owned();
                Step::Yield(Err(ProviderError::Transport(msg)))
            }
            // tool_call_* and the rest are the upstream agent's
            // internal business; not relayed in v1.
            _ => Step::Continue,
        }
    }
}

impl<R: BufRead, W: Write> Drop for AcpStream<R, W> {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl<R: BufRead, W: Write> Iterator for AcpStream<R, W> {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            if self.cancel.is_cancelled() {
                self.finish();
                return Some(Err(ProviderError::Cancelled));
            }
            let value = match framing::read_message(&mut self.reader) {
                Ok(Some(v)) => v,
                Ok(None) => {
                    let ended = self.ended;
                    self.finish();
                    return if ended {
                        None
                    } else {
                        Some(Err(ProviderError::Transport(
                            "acp: upstream closed before message_end".to_owned(),
                        )))
                    };
                }
                Err(e) => {
                    self.finish();
                    return Some(Err(ProviderError::Transport(format!("acp: {e}"))));
                }
            };
            match self.classify(&value) {
                Step::Yield(item) => return Some(item),
                Step::Continue => {}
                Step::Stop => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn frames(values: &[serde_json::Value]) -> Vec<u8> {
        let mut buf = Vec::new();
        for v in values {
            framing::write_message(&mut buf, v).unwrap();
        }
        buf
    }

    fn stream_over(input: Vec<u8>) -> AcpStream<Cursor<Vec<u8>>, Vec<u8>> {
        AcpStream {
            reader: Cursor::new(input),
            stdin: Vec::new(),
            child: None,
            cancel: CancelFlag::new(),
            done: false,
            ended: false,
        }
    }

    #[test]
    fn last_user_text_joins_text_blocks_of_the_last_user_turn() {
        let msgs = vec![
            Message::new(Role::User, vec![Content::Text { text: "old".into() }], None),
            Message::new(
                Role::Assistant,
                vec![Content::Text { text: "hi".into() }],
                None,
            ),
            Message::new(
                Role::User,
                vec![
                    Content::Text { text: "a".into() },
                    Content::Text { text: "b".into() },
                ],
                None,
            ),
        ];
        assert_eq!(last_user_text(&msgs).as_deref(), Some("a\nb"));
        assert_eq!(last_user_text(&[]), None);
    }

    #[test]
    fn translates_upstream_events_into_provider_events() {
        let input = frames(&[
            serde_json::json!({"method": "event", "params": {"type": "message_start"}}),
            serde_json::json!({"method": "event", "params": {"type": "text_delta", "delta": "he"}}),
            serde_json::json!({"method": "event", "params": {"type": "thinking_delta", "delta": "mm"}}),
            serde_json::json!({"method": "event", "params": {"type": "message_end"}}),
            serde_json::json!({"id": PROMPT_ID, "result": {"status": "completed"}}),
        ]);
        let got: Vec<_> = stream_over(input).map(Result::unwrap).collect();
        assert!(matches!(got[0], ProviderEvent::MessageStart));
        assert!(matches!(&got[1], ProviderEvent::TextDelta { delta } if delta == "he"));
        assert!(matches!(&got[2], ProviderEvent::ThinkingDelta { delta } if delta == "mm"));
        assert!(matches!(
            got[3],
            ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ));
        assert_eq!(
            got.len(),
            4,
            "prompt response ends the stream, no extra event"
        );
    }

    #[test]
    fn denies_upstream_permission_requests() {
        let input = frames(&[
            serde_json::json!({
                "method": "permission/request",
                "params": {"id": "p1", "name": "bash", "input": {}}
            }),
            serde_json::json!({"method": "event", "params": {"type": "message_end"}}),
            serde_json::json!({"id": PROMPT_ID, "result": {}}),
        ]);
        let mut s = stream_over(input);
        let first = s.next().unwrap().unwrap();
        assert!(matches!(
            first,
            ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ));
        let mut cur = Cursor::new(s.stdin.clone());
        let reply = framing::read_message(&mut cur).unwrap().unwrap();
        assert_eq!(reply["method"], "permission/respond");
        assert_eq!(reply["params"]["allow"], false);
    }

    #[test]
    fn upstream_error_event_becomes_transport_error() {
        let input = frames(&[serde_json::json!({
            "method": "event",
            "params": {"type": "error", "kind": {"message": "boom"}}
        })]);
        let mut s = stream_over(input);
        assert!(matches!(
            s.next().unwrap(),
            Err(ProviderError::Transport(m)) if m == "boom"
        ));
        assert!(s.next().is_none());
    }

    #[test]
    fn early_eof_is_a_transport_error() {
        let mut s = stream_over(Vec::new());
        assert!(matches!(
            s.next().unwrap(),
            Err(ProviderError::Transport(_))
        ));
    }
}
