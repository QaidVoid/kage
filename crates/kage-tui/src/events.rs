//! Engine events applied to the conversation buffer.
//!
//! [`apply_loop_event`] folds streamed [`kage_core::LoopEvent`]s into the
//! buffer's block model and [`populate_from_history`] rebuilds a buffer
//! from a stored conversation. The renderer reads the same buffer each
//! frame, so painting is decoupled from event arrival.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kage_core::{Content, LoopError, LoopEvent, Message, Role, StopReason};

use crate::buffer::Buffer;

/// Cloneable handle to the conversation buffer shared between the App
/// and its renderer.
pub type SharedBuffer = Arc<Mutex<Buffer>>;

/// Construct an empty shared buffer.
#[must_use]
pub fn shared_buffer() -> SharedBuffer {
    Arc::new(Mutex::new(Buffer::new()))
}

/// Fold one loop event into `buf`. User prompts arrive as
/// [`LoopEvent::MessageAppended`], so a steered prompt appears when the
/// agent actually receives it.
pub fn apply_loop_event(buf: &mut Buffer, event: &LoopEvent) {
    match event {
        LoopEvent::MessageAppended { message } if message.role == Role::User => {
            push_user_message(buf, message);
        }
        LoopEvent::MessageStart { .. }
        | LoopEvent::ToolUpdate { .. }
        | LoopEvent::MessageAppended { .. }
        | LoopEvent::TurnStarted { .. }
        | LoopEvent::TurnEnded { .. } => {
            // The buffer lazily begins an Assistant block on the first
            // text/thinking delta, so MessageStart is a no-op here.
            // Mid-execution tool progress is consumed by plugin event
            // handlers; the conversation buffer shows only the final
            // tool result.
        }
        LoopEvent::TextDelta { delta, .. } => buf.append_assistant_delta(delta),
        LoopEvent::ThinkingDelta { delta, .. } => buf.append_thinking_delta(delta),
        LoopEvent::ToolCallStart {
            id,
            name,
            input_partial,
        }
        | LoopEvent::ToolCallArgsDelta {
            id,
            name,
            input_partial,
        } => {
            let summary = summarize_input(name, input_partial);
            let pretty = serde_json::to_string_pretty(input_partial)
                .unwrap_or_else(|_| input_partial.to_string());
            buf.upsert_tool_call(id.to_string(), name, summary, pretty);
        }
        LoopEvent::ToolCallEnd { id, output } => {
            buf.push_tool_result(id.to_string(), output.text.clone(), output.is_error);
        }
        LoopEvent::MessageEnd {
            stop_reason: StopReason::MaxTokens,
            ..
        } => {
            // Finalize the live assistant bubble first: finish_streaming
            // targets the last block, so the notice must land after it.
            buf.finish_streaming();
            buf.push_custom(
                "kage:truncated",
                "reply hit the max output token limit",
                false,
            );
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
                false,
            );
        }
        LoopEvent::ProviderRetry {
            attempt,
            max_attempts,
            wait_secs,
            requested_secs,
            error,
        } => {
            let mut msg = format!(
                "provider error ({error}); retrying {attempt}/{max_attempts} in {wait_secs}s"
            );
            if let Some(req) = requested_secs
                && *req > *wait_secs
            {
                use std::fmt::Write as _;
                let _ = write!(msg, " (server asked for {req}s)");
            }
            buf.push_custom("kage:notify", msg, false);
        }
        LoopEvent::Error { kind } => {
            buf.finish_streaming();
            match kind {
                LoopError::Cancelled => buf.push_custom("kage:notify", "interrupted", false),
                LoopError::Auth { message } => buf.push_custom(
                    "kage:error",
                    format!(
                        "authentication failed: {}. Run /login to re-authenticate.",
                        message.trim_end_matches('.')
                    ),
                    false,
                ),
                // The block's `error` chrome already names the severity;
                // the payload adds nothing but the message.
                other => buf.push_custom("kage:error", other.to_string(), false),
            }
        }
    }
}

/// Paint a user prompt: its text as a user bubble, then one placeholder
/// per attached image.
fn push_user_message(buf: &mut Buffer, message: &Message) {
    let text: Vec<&str> = message
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if !text.is_empty() {
        buf.push_user(text.join("\n"));
    }
    for block in &message.content {
        if let Content::Image { mime, .. } = block {
            buf.push_custom("kage:image", format!("[image: {mime}]"), false);
        }
    }
}

/// How long each tool call took, keyed by call id, recovered from the
/// timestamps of the assistant message that made the call and the
/// message carrying its result.
#[must_use]
pub fn tool_durations(messages: &[Message]) -> HashMap<String, u64> {
    let mut started = HashMap::new();
    let mut durations = HashMap::new();
    for message in messages {
        for block in &message.content {
            match block {
                Content::ToolCall { id, .. } => {
                    started.insert(id.to_string(), message.ts);
                }
                Content::ToolResultBlock { call_id, .. } => {
                    if let Some(start) = started.get(&call_id.to_string()) {
                        let ms = (message.ts - *start).num_milliseconds();
                        durations.insert(call_id.to_string(), u64::try_from(ms).unwrap_or(0));
                    }
                }
                _ => {}
            }
        }
    }
    durations
}

/// Pour a replayed `Vec<Message>` into a fresh [`Buffer`] so the user
/// sees the prior conversation rendered with the current TUI styling.
///
/// The translator walks each message in order: user prompts become user
/// bubbles, assistant text/thinking blocks become their respective
/// streamed-and-finished blocks, tool calls and tool results become the
/// merged tool composites the renderer pairs at draw time. Pass
/// `tool_durations` to recover real `Took Xms` values from session
/// entry timestamps; an empty map renders as `Took --`.
#[allow(clippy::implicit_hasher)]
pub fn populate_from_history(
    buf: &mut Buffer,
    messages: &[Message],
    tool_durations: &HashMap<String, u64>,
) {
    for msg in messages {
        match msg.role {
            Role::User => {
                if let Some(text) = first_text(msg) {
                    if is_compaction_summary(&text) {
                        buf.push_custom("kage:compaction", text, false);
                    } else {
                        buf.push_user(text);
                    }
                }
            }
            Role::Assistant => {
                for block in &msg.content {
                    match block {
                        Content::Text { text } => {
                            if is_compaction_summary(text) {
                                buf.push_custom("kage:compaction", text.clone(), false);
                            } else {
                                buf.append_assistant_delta(text);
                                buf.finish_streaming();
                            }
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
                        // Replay: real timing is recovered from the
                        // session's per-entry `ts` deltas via
                        // `tool_durations`. A miss yields `None`,
                        // rendered as `Took --`.
                        let duration = tool_durations.get(&call_id.to_string()).copied();
                        buf.push_tool_result_with_duration(
                            call_id.to_string(),
                            output.clone(),
                            *is_error,
                            duration,
                        );
                    }
                }
            }
            Role::System => {}
        }
    }
}

/// True when `text` looks like the synthetic compaction-summary
/// message the loop inserts in place of drained history. Detection
/// matches the framing constants in [`kage_core::message`] so resumed
/// sessions route the summary through the compaction widget instead
/// of rendering it as a plain user / assistant bubble.
fn is_compaction_summary(text: &str) -> bool {
    text.starts_with(kage_core::message::COMPACTION_SUMMARY_PREFIX)
        || text.contains("<summary>") && text.contains("</summary>")
}

fn first_text(msg: &Message) -> Option<String> {
    msg.content.iter().find_map(|c| match c {
        Content::Text { text } => Some(text.clone()),
        _ => None,
    })
}

/// One-line summary of a tool's input shown in the folded header: the
/// [`crate::view::tool_view::describe`] target, such as the path for
/// `read` or the command for `bash`.
fn summarize_input(name: &str, input: &serde_json::Value) -> String {
    if matches!(input, serde_json::Value::Null) {
        return String::new();
    }
    let target = crate::view::tool_view::describe(name, input).target;
    crate::view::truncate_to_width(&target, 60, "...")
}

#[cfg(test)]
mod tests {
    use kage_core::sync::lock;
    use kage_core::{LoopError, MessageId, StopReason, TokenUsage, ToolCallId, ToolOutput};
    use serde_json::json;

    use super::*;
    use crate::buffer::Block;

    struct Apply(SharedBuffer);

    impl Apply {
        fn on_event(&mut self, event: &LoopEvent) {
            apply_loop_event(&mut lock(&self.0), event);
        }
    }

    fn fresh() -> (SharedBuffer, Apply) {
        let buf = shared_buffer();
        (buf.clone(), Apply(buf))
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
    fn summarize_edit_is_the_path() {
        assert_eq!(
            summarize_input("edit", &json!({"path":"a.rs","old_str":"x","new_str":"y"})),
            "a.rs"
        );
    }

    #[test]
    fn summarize_grep_combines_pattern_and_path() {
        assert_eq!(
            summarize_input("grep", &json!({"pattern": "foo", "path": "src"})),
            "\"foo\" in src"
        );
        assert_eq!(
            summarize_input("grep", &json!({"pattern": "foo", "path": "."})),
            "\"foo\""
        );
    }

    #[test]
    fn summarize_bash_uses_command_field() {
        assert_eq!(
            summarize_input("bash", &json!({"command": "ls -la"})),
            "ls -la"
        );
    }

    #[test]
    fn summarize_unknown_tool_is_its_name() {
        assert_eq!(
            summarize_input("custom_tool", &json!({"foo": "bar"})),
            "custom_tool"
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
            stop_reason: StopReason::EndTurn,
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
    fn max_tokens_message_end_pushes_truncated_notice() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "partial ans".into(),
        });
        hooks.on_event(&LoopEvent::MessageEnd {
            id: id(),
            usage: TokenUsage::default(),
            stop_reason: StopReason::MaxTokens,
        });
        let buf = buf.lock().unwrap();
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            Block::Assistant { text, live } => {
                assert_eq!(text, "partial ans");
                assert!(!live, "assistant block must be finished");
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        match &blocks[1] {
            Block::Custom { kind, text, folded } => {
                assert_eq!(kind, "kage:truncated");
                assert_eq!(text, "reply hit the max output token limit");
                assert!(!folded);
            }
            other => panic!("expected Custom, got {other:?}"),
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
                terminate: false,
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
            kind: LoopError::ContextOverflow,
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
    fn cancel_is_a_quiet_notice_and_finishes_the_live_reply() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "half an ans".into(),
        });
        hooks.on_event(&LoopEvent::Error {
            kind: LoopError::Cancelled,
        });
        let buf = buf.lock().unwrap();
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0], Block::Assistant { live: false, .. }));
        match &blocks[1] {
            Block::Custom { kind, text, .. } => {
                assert_eq!(kind, "kage:notify");
                assert_eq!(text, "interrupted");
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn auth_error_points_at_login() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::Error {
            kind: LoopError::Auth {
                message: "token expired.".into(),
            },
        });
        let buf = buf.lock().unwrap();
        match &buf.blocks()[0] {
            Block::Custom { kind, text, .. } => {
                assert_eq!(kind, "kage:error");
                assert_eq!(
                    text,
                    "authentication failed: token expired. Run /login to re-authenticate."
                );
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
        populate_from_history(&mut buf, &history, &std::collections::HashMap::new());
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
    fn populate_routes_compaction_summary_to_custom_block() {
        let framed = format!(
            "{}{}{}",
            kage_core::message::COMPACTION_SUMMARY_PREFIX,
            "the actual summary content",
            kage_core::message::COMPACTION_SUMMARY_SUFFIX
        );
        let mut buf = Buffer::new();
        let history = vec![Message::new(
            Role::User,
            vec![Content::Text { text: framed }],
            None,
        )];
        populate_from_history(&mut buf, &history, &std::collections::HashMap::new());
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Custom { kind, .. } => assert_eq!(kind, "kage:compaction"),
            other => panic!("expected Custom compaction block, got {other:?}"),
        }
    }

    #[test]
    fn populate_keeps_regular_user_message_as_user_block() {
        let mut buf = Buffer::new();
        let history = vec![Message::new(
            Role::User,
            vec![Content::Text {
                text: "just a normal message".into(),
            }],
            None,
        )];
        populate_from_history(&mut buf, &history, &std::collections::HashMap::new());
        let blocks = buf.blocks();
        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0], Block::User { .. }));
    }

    #[test]
    fn appended_user_messages_paint_text_then_images() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::MessageAppended {
            message: Message::new(
                Role::User,
                vec![
                    Content::Text {
                        text: "hello".into(),
                    },
                    Content::Image {
                        source: kage_core::ImageSource::Base64 { data: "AA".into() },
                        mime: "image/png".into(),
                    },
                ],
                None,
            ),
        });
        hooks.on_event(&LoopEvent::MessageAppended {
            message: Message::new(Role::Assistant, Vec::new(), None),
        });
        let buf = buf.lock().unwrap();
        assert_eq!(buf.blocks().len(), 2);
        assert!(matches!(&buf.blocks()[0], Block::User { text } if text == "hello"));
    }

    #[test]
    fn tool_durations_come_from_message_timestamps() {
        let call = Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("c1"),
                name: "bash".into(),
                input: json!({}),
            }],
            None,
        );
        let mut result = Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("c1"),
                output: String::new(),
                is_error: false,
            }],
            None,
        );
        result.ts = call.ts + chrono::Duration::milliseconds(250);
        assert_eq!(tool_durations(&[call, result])["c1"], 250);
    }
}
