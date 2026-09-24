//! Streaming-event stdout printers and their tests.

#[allow(clippy::wildcard_imports)] // split out of main.rs; shares the crate-root scope
use super::*;
use kage_core::LoopError;

/// Render one streaming event to stdout. Only text-bearing events produce
/// visible output; tool calls render a single bracketed status line.
pub(crate) fn print_event<W: Write>(out: &mut W, event: &LoopEvent) {
    match event {
        LoopEvent::TextDelta { delta, .. } => {
            let _ = out.write_all(delta.as_bytes());
            let _ = out.flush();
        }
        LoopEvent::ToolCallStart { name, .. } => {
            let _ = writeln!(out, "\n[tool: {name}]");
            let _ = out.flush();
        }
        LoopEvent::ToolCallEnd { output, .. } => {
            if output.is_error {
                let _ = writeln!(out, "[tool error] {}", output.text);
            }
            let _ = out.flush();
        }
        LoopEvent::Compaction {
            kept, summarized, ..
        } => {
            let _ = writeln!(out, "\n[compacted: kept {kept}, summarized {summarized}]");
            let _ = out.flush();
        }
        LoopEvent::Error {
            kind: LoopError::Auth { message },
        } => {
            let _ = writeln!(
                out,
                "\n[error] authentication failed: {}. Run `kage auth login` to re-authenticate.",
                message.trim_end_matches('.')
            );
            let _ = out.flush();
        }
        LoopEvent::Error { kind } => {
            let _ = writeln!(out, "\n[error] {kind}");
            let _ = out.flush();
        }
        _ => {}
    }
}

/// Emit `envelope` as one JSONL row on `out` and flush, so streaming
/// consumers can split on `\n` and parse each line on arrival.
pub(crate) fn print_envelope_json<W: Write>(out: &mut W, envelope: &kage_core::protocol::Envelope) {
    match serde_json::to_string(envelope) {
        Ok(line) => {
            let _ = writeln!(out, "{line}");
        }
        Err(err) => {
            let _ = writeln!(
                out,
                r#"{{"type":"error","kind":{{"kind":"other","message":"encode: {err}"}}}}"#
            );
        }
    }
    let _ = out.flush();
}

#[cfg(test)]
mod text_print_tests {
    use super::*;

    #[test]
    fn auth_error_names_the_login_command() {
        let mut buf = Vec::new();
        print_event(
            &mut buf,
            &LoopEvent::Error {
                kind: LoopError::Auth {
                    message: "token expired".into(),
                },
            },
        );
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("authentication failed: token expired"),
            "{text:?}"
        );
        assert!(text.contains("kage auth login"), "{text:?}");
    }
}

#[cfg(test)]
mod json_print_tests {
    use kage_core::protocol::Envelope;
    use kage_core::{MessageId, SessionId, StopReason, TokenUsage};

    use super::*;

    fn json_row(event: LoopEvent) -> serde_json::Value {
        let mut buf = Vec::new();
        print_envelope_json(
            &mut buf,
            &Envelope {
                session: SessionId::new(),
                seq: 1,
                event: event.into(),
            },
        );
        let line = String::from_utf8(buf).unwrap();
        assert!(line.ends_with('\n'));
        serde_json::from_str(line.trim_end()).unwrap()
    }

    #[test]
    fn text_delta_renders_as_single_jsonl_row() {
        let parsed = json_row(LoopEvent::TextDelta {
            id: MessageId::new(),
            delta: "hi".into(),
        });
        assert_eq!(parsed["seq"], 1);
        assert_eq!(parsed["type"], "text_delta");
        assert_eq!(parsed["delta"], "hi");
    }

    #[test]
    fn message_end_carries_usage_through_jsonl() {
        let parsed = json_row(LoopEvent::MessageEnd {
            id: MessageId::new(),
            usage: TokenUsage {
                input: 12,
                output: 7,
                ..TokenUsage::default()
            },
            stop_reason: StopReason::EndTurn,
        });
        assert_eq!(parsed["type"], "message_end");
        assert_eq!(parsed["usage"]["input"], 12);
        assert_eq!(parsed["usage"]["output"], 7);
    }
}
