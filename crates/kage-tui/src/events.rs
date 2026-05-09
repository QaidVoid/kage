//! Hooks adapter: drain agent-loop events into the conversation buffer.
//!
//! [`TuiHooks`] reassembles streamed [`kage_core::LoopEvent`]s into the
//! buffer's block model. The renderer reads from the same buffer each
//! frame, so painting is decoupled from event arrival.
//!
//! Buffer access is mediated by an `Arc<Mutex<Buffer>>` so the agent
//! loop's worker thread can push events while the main thread renders.
//! The lock is held only as long as one event takes to apply, never
//! during `Hooks` re-entry.

use std::sync::{Arc, Mutex};

use kage_core::{LoopEvent, ToolOutput};
use kage_loop::Hooks;

use crate::buffer::Buffer;

/// Cloneable handle to the conversation buffer shared between the agent
/// loop's worker thread and the TUI renderer.
pub type SharedBuffer = Arc<Mutex<Buffer>>;

/// Construct an empty shared buffer.
#[must_use]
pub fn shared_buffer() -> SharedBuffer {
    Arc::new(Mutex::new(Buffer::new()))
}

/// Hooks impl that mirrors [`LoopEvent`]s into the [`SharedBuffer`].
///
/// Wraps another `Hooks` so a host can chain (TUI display + session
/// recording + plugin dispatch) without interleaving wrappers manually.
pub struct TuiHooks<H: Hooks> {
    inner: H,
    buffer: SharedBuffer,
}

impl<H: Hooks> TuiHooks<H> {
    /// Wrap `inner` so its `on_event` flow also paints into `buffer`.
    pub fn new(inner: H, buffer: SharedBuffer) -> Self {
        Self { inner, buffer }
    }

    /// Append a user-typed prompt to the buffer. The agent loop never
    /// emits user messages as events, so the host calls this directly.
    pub fn record_user_input(&self, text: impl Into<String>) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_user(text);
        }
    }
}

impl<H: Hooks> Hooks for TuiHooks<H> {
    fn before_tool_call(&mut self, name: &str, input: &serde_json::Value) -> Option<ToolOutput> {
        self.inner.before_tool_call(name, input)
    }

    fn after_tool_call(&mut self, name: &str, output: ToolOutput) -> ToolOutput {
        self.inner.after_tool_call(name, output)
    }

    fn on_event(&mut self, event: &LoopEvent) {
        if let Ok(mut buf) = self.buffer.lock() {
            apply_event(&mut buf, event);
        }
        self.inner.on_event(event);
    }

    fn get_steering(&mut self) -> Option<String> {
        self.inner.get_steering()
    }

    fn get_followup(&mut self) -> Option<String> {
        self.inner.get_followup()
    }
}

fn apply_event(buf: &mut Buffer, event: &LoopEvent) {
    match event {
        LoopEvent::MessageStart { .. } => {
            // The buffer lazily begins an Assistant block on the first
            // text/thinking delta, so MessageStart is a no-op here.
        }
        LoopEvent::TextDelta { delta, .. } => buf.append_assistant_delta(delta),
        LoopEvent::ThinkingDelta { delta, .. } => buf.append_thinking_delta(delta),
        LoopEvent::ToolCallStart {
            id,
            name,
            input_partial,
        } => {
            let summary = summarize_input(input_partial);
            let pretty = serde_json::to_string_pretty(input_partial)
                .unwrap_or_else(|_| input_partial.to_string());
            buf.push_tool_call(id.to_string(), name, summary, pretty);
        }
        LoopEvent::ToolCallEnd { id, output } => {
            buf.push_tool_result(id.to_string(), output.text.clone(), output.is_error);
        }
        LoopEvent::MessageEnd { .. } => buf.finish_streaming(),
        LoopEvent::Compaction {
            kept,
            summarized,
            summary,
        } => {
            buf.push_custom(
                "kage:compaction",
                format!("[compacted: kept {kept}, summarized {summarized}]\n{summary}"),
                true,
            );
        }
        LoopEvent::Error { kind } => {
            buf.push_custom("kage:error", format!("[error] {kind}"), false);
        }
    }
}

fn summarize_input(input: &serde_json::Value) -> String {
    if matches!(input, serde_json::Value::Null) {
        return String::new();
    }
    let mut s = input.to_string();
    if s.len() > 60 {
        s.truncate(57);
        s.push_str("...");
    }
    s
}

#[cfg(test)]
mod tests {
    use kage_core::{LoopError, MessageId, TokenUsage, ToolCallId};
    use kage_loop::NoopHooks;
    use serde_json::json;

    use super::*;
    use crate::buffer::Block;

    fn fresh() -> (SharedBuffer, TuiHooks<NoopHooks>) {
        let buf = shared_buffer();
        (buf.clone(), TuiHooks::new(NoopHooks, buf))
    }

    fn id() -> MessageId {
        MessageId::new()
    }

    #[test]
    fn text_delta_appends_to_assistant_block() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::MessageStart { id: id() });
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "hello ".into(),
        });
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "world".into(),
        });
        hooks.on_event(&LoopEvent::MessageEnd {
            id: id(),
            usage: TokenUsage::default(),
        });
        let buf = buf.lock().unwrap();
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Assistant { text, live } => {
                assert_eq!(text, "hello world");
                assert!(!*live);
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn thinking_delta_appends_to_separate_block() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::ThinkingDelta {
            id: id(),
            delta: "let me think".into(),
        });
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "ok".into(),
        });
        let buf = buf.lock().unwrap();
        assert_eq!(buf.blocks().len(), 2);
        assert!(matches!(buf.blocks()[0], Block::Thinking { .. }));
        assert!(matches!(buf.blocks()[1], Block::Assistant { .. }));
    }

    #[test]
    fn tool_call_and_result_pair_into_blocks() {
        let (buf, mut hooks) = fresh();
        let cid = ToolCallId::new("c1");
        hooks.on_event(&LoopEvent::ToolCallStart {
            id: cid.clone(),
            name: "bash".into(),
            input_partial: json!({"cmd": "ls"}),
        });
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: cid,
            output: ToolOutput {
                is_error: false,
                text: "file1\nfile2".into(),
                structured: None,
            },
        });
        let buf = buf.lock().unwrap();
        assert_eq!(buf.blocks().len(), 2);
        match &buf.blocks()[0] {
            Block::ToolCall { name, .. } => assert_eq!(name, "bash"),
            other => panic!("expected ToolCall, got {other:?}"),
        }
        match &buf.blocks()[1] {
            Block::ToolResult {
                output, is_error, ..
            } => {
                assert_eq!(output, "file1\nfile2");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn compaction_pushes_custom_block_with_summary() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::Compaction {
            kept: 4,
            summarized: 12,
            summary: "everyone agrees".into(),
        });
        let buf = buf.lock().unwrap();
        match &buf.blocks()[0] {
            Block::Custom { kind, text, .. } => {
                assert_eq!(kind, "kage:compaction");
                assert!(text.contains("kept 4"));
                assert!(text.contains("everyone agrees"));
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn error_pushes_custom_unfolded_error_block() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::Error {
            kind: LoopError::Cancelled,
        });
        let buf = buf.lock().unwrap();
        match &buf.blocks()[0] {
            Block::Custom { kind, folded, .. } => {
                assert_eq!(kind, "kage:error");
                assert!(!folded, "errors should be visible by default");
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn record_user_input_pushes_user_block() {
        let (buf, hooks) = fresh();
        hooks.record_user_input("hello");
        let buf = buf.lock().unwrap();
        match &buf.blocks()[0] {
            Block::User { text } => assert_eq!(text, "hello"),
            _ => panic!(),
        }
    }
}
