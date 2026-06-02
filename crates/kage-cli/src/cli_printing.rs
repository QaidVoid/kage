//! Streaming-event stdout printers and their tests.

#[allow(clippy::wildcard_imports)] // split out of main.rs; shares the crate-root scope
use super::*;

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
        LoopEvent::Error { kind } => {
            let _ = writeln!(out, "\n[error] {kind}");
            let _ = out.flush();
        }
        _ => {}
    }
}

/// Emit `event` as one JSONL row on `out`. Skips the trailing newline
/// the text-mode path adds because each event already terminates with
/// `\n`, so consumers can split on `\n` and run `serde_json::from_str`
/// on each line. Serialization can only fail on cycle errors, which
/// our event types can't produce; we still flush so streaming
/// consumers see the row immediately.
pub(crate) fn print_event_json<W: Write>(out: &mut W, event: &LoopEvent) {
    match serde_json::to_string(event) {
        Ok(line) => {
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
        Err(err) => {
            let _ = writeln!(
                out,
                r#"{{"type":"error","kind":{{"kind":"other","message":"encode: {err}"}}}}"#
            );
            let _ = out.flush();
        }
    }
}

#[cfg(test)]
mod json_print_tests {
    use kage_core::{MessageId, TokenUsage};

    use super::*;

    #[test]
    fn text_delta_renders_as_single_jsonl_row() {
        let mut buf = Vec::new();
        print_event_json(
            &mut buf,
            &LoopEvent::TextDelta {
                id: MessageId::new(),
                delta: "hi".into(),
            },
        );
        let line = String::from_utf8(buf).unwrap();
        assert!(line.ends_with('\n'));
        let trimmed = line.trim_end();
        // Body is one JSON value per line.
        let parsed: serde_json::Value = serde_json::from_str(trimmed).unwrap();
        assert_eq!(parsed["type"], "text_delta");
        assert_eq!(parsed["delta"], "hi");
    }

    #[test]
    fn message_end_carries_usage_through_jsonl() {
        let mut buf = Vec::new();
        print_event_json(
            &mut buf,
            &LoopEvent::MessageEnd {
                id: MessageId::new(),
                usage: TokenUsage {
                    input: 12,
                    output: 7,
                    ..TokenUsage::default()
                },
            },
        );
        let line = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["type"], "message_end");
        assert_eq!(parsed["usage"]["input"], 12);
        assert_eq!(parsed["usage"]["output"], 7);
    }
}
