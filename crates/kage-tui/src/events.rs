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

use kage_core::{Content, LoopEvent, Message, Role, ToolOutput};
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
            let summary = summarize_input(name, input_partial);
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

/// Pour a replayed `Vec<Message>` into a fresh [`Buffer`] so the user
/// sees the prior conversation rendered with the current TUI styling.
///
/// The translator walks each message in order: user prompts become user
/// bubbles, assistant text/thinking blocks become their respective
/// streamed-and-finished blocks, tool calls and tool results become the
/// merged tool composites the renderer pairs at draw time. Timing
/// information from the original session is not preserved (replay
/// blocks render `Took --`).
pub fn populate_from_history(buf: &mut Buffer, messages: &[Message]) {
    for msg in messages {
        match msg.role {
            Role::User => {
                if let Some(text) = first_text(msg) {
                    buf.push_user(text);
                }
            }
            Role::Assistant => {
                for block in &msg.content {
                    match block {
                        Content::Text { text } => {
                            buf.append_assistant_delta(text);
                            buf.finish_streaming();
                        }
                        Content::Thinking { text } => {
                            buf.append_thinking_delta(text);
                            buf.finish_streaming();
                        }
                        Content::ToolCall { id, name, input } => {
                            let summary = summarize_input(name, input);
                            let pretty = serde_json::to_string_pretty(input)
                                .unwrap_or_else(|_| input.to_string());
                            buf.push_tool_call(id.to_string(), name, summary, pretty);
                        }
                        Content::ToolResultBlock { .. }
                        | Content::Image { .. }
                        | Content::Custom { .. } => {}
                    }
                }
            }
            Role::ToolResult => {
                for block in &msg.content {
                    if let Content::ToolResultBlock {
                        call_id,
                        output,
                        is_error,
                    } = block
                    {
                        // Replay: timing is not persisted in the
                        // session format, so pass `None` rather than
                        // computing a misleading sub-millisecond
                        // duration from the just-replayed call.
                        buf.push_tool_result_with_duration(
                            call_id.to_string(),
                            output.clone(),
                            *is_error,
                            None,
                        );
                    }
                }
            }
            Role::System => {}
        }
    }
}

fn first_text(msg: &Message) -> Option<String> {
    msg.content.iter().find_map(|c| match c {
        Content::Text { text } => Some(text.clone()),
        _ => None,
    })
}

/// One-line summary of a tool's input shown in the folded header.
///
/// Built-in tools each get a tailored projection of their JSON input
/// (the path for `read`/`write`/`edit`, the pattern for `find`/`grep`,
/// the command for `bash`, the URL for `web_fetch`, etc.) so the header
/// reads like `read README.md` instead of `read({"path":"README.md"})`.
/// Unknown tools fall back to the previous compact-JSON representation.
fn summarize_input(name: &str, input: &serde_json::Value) -> String {
    if matches!(input, serde_json::Value::Null) {
        return String::new();
    }
    let summary = match name {
        "read" | "view" | "write" | "ls" => string_field(input, "path"),
        "edit" => edit_summary(input),
        "find" | "glob" => string_field(input, "pattern"),
        "grep" => grep_summary(input),
        "bash" | "shell" => string_field(input, "cmd").or_else(|| string_field(input, "command")),
        "web_fetch" | "fetch" => string_field(input, "url"),
        _ => None,
    };
    let raw = summary.unwrap_or_else(|| input.to_string());
    truncate(&raw, 60)
}

fn string_field(input: &serde_json::Value, key: &str) -> Option<String> {
    input.get(key)?.as_str().map(str::to_owned)
}

fn edit_summary(input: &serde_json::Value) -> Option<String> {
    let path = string_field(input, "path")?;
    if let (Some(start), Some(end)) = (
        input.get("start_line").and_then(serde_json::Value::as_i64),
        input.get("end_line").and_then(serde_json::Value::as_i64),
    ) {
        return Some(format!("{path}:{start}-{end}"));
    }
    Some(path)
}

fn grep_summary(input: &serde_json::Value) -> Option<String> {
    let pattern = string_field(input, "pattern")?;
    match string_field(input, "path") {
        Some(path) if path != "." => Some(format!("{pattern} in {path}")),
        _ => Some(pattern),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let cut: String = s.chars().take(max.saturating_sub(3)).collect();
    format!("{cut}...")
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
    fn summarize_read_returns_just_the_path() {
        assert_eq!(
            summarize_input("read", &json!({"path": "README.md"})),
            "README.md"
        );
    }

    #[test]
    fn summarize_edit_includes_line_range_when_present() {
        assert_eq!(
            summarize_input(
                "edit",
                &json!({"path": "src/lib.rs", "start_line": 10, "end_line": 20})
            ),
            "src/lib.rs:10-20"
        );
        assert_eq!(
            summarize_input("edit", &json!({"path": "src/lib.rs"})),
            "src/lib.rs"
        );
    }

    #[test]
    fn summarize_grep_combines_pattern_and_path() {
        assert_eq!(
            summarize_input("grep", &json!({"pattern": "foo", "path": "src"})),
            "foo in src"
        );
        assert_eq!(
            summarize_input("grep", &json!({"pattern": "foo", "path": "."})),
            "foo"
        );
    }

    #[test]
    fn summarize_bash_uses_command_field() {
        assert_eq!(summarize_input("bash", &json!({"cmd": "ls -la"})), "ls -la");
        assert_eq!(
            summarize_input("bash", &json!({"command": "ls -la"})),
            "ls -la"
        );
    }

    #[test]
    fn summarize_unknown_tool_falls_back_to_compact_json() {
        assert_eq!(
            summarize_input("custom_tool", &json!({"foo": "bar"})),
            "{\"foo\":\"bar\"}"
        );
    }

    #[test]
    fn summarize_truncates_long_summaries() {
        let path = "a".repeat(80);
        let out = summarize_input("read", &json!({"path": path}));
        assert!(out.ends_with("..."));
        assert!(out.chars().count() <= 60);
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
    fn populate_from_history_translates_user_assistant_and_tools() {
        use kage_core::ToolCallId;
        let mut buf = Buffer::new();
        let history = vec![
            Message::new(
                Role::User,
                vec![Content::Text {
                    text: "list files".into(),
                }],
                None,
            ),
            Message::new(
                Role::Assistant,
                vec![
                    Content::Thinking {
                        text: "use ls".into(),
                    },
                    Content::Text {
                        text: "looking now".into(),
                    },
                    Content::ToolCall {
                        id: ToolCallId::new("c1"),
                        name: "ls".into(),
                        input: json!({"path": "."}),
                    },
                ],
                None,
            ),
            Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new("c1"),
                    output: "a.rs\nb.rs".into(),
                    is_error: false,
                }],
                None,
            ),
        ];
        populate_from_history(&mut buf, &history);
        let blocks = buf.blocks();
        assert!(matches!(blocks[0], Block::User { .. }));
        assert!(matches!(blocks[1], Block::Thinking { .. }));
        assert!(matches!(blocks[2], Block::Assistant { .. }));
        assert!(matches!(
            &blocks[3],
            Block::ToolCall { name, .. } if name == "ls"
        ));
        assert!(matches!(
            &blocks[4],
            Block::ToolResult { output, .. } if output == "a.rs\nb.rs"
        ));
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
