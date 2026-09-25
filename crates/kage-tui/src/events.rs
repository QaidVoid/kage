//! Engine events applied to the conversation buffer.
//!
//! [`apply_loop_event`] folds streamed [`kage_core::LoopEvent`]s into the
//! buffer's block model and [`populate_from_history`] rebuilds a buffer
//! from a stored conversation. The renderer reads the same buffer each
//! frame, so painting is decoupled from event arrival.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kage_core::resource_block::{self, ResourceRef};
use kage_core::{Content, LoopError, LoopEvent, Message, MessageId, Role, StopReason};

use crate::buffer::Buffer;
use crate::view::tool_view::ToolPhase;

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
        | LoopEvent::MessageAppended { .. }
        | LoopEvent::TurnStarted { .. }
        | LoopEvent::TurnEnded { .. } => {
            // The buffer lazily begins an Assistant block on the first
            // text/thinking delta, so MessageStart is a no-op here.
        }
        LoopEvent::TextDelta { delta, .. } => buf.append_assistant_delta(delta),
        LoopEvent::ThinkingDelta { delta, .. } => buf.append_thinking_delta(delta),
        LoopEvent::ToolCallArgsDelta {
            id,
            name,
            input_partial,
        } => buf.upsert_tool_call(id.to_string(), name, input_partial.clone()),
        LoopEvent::ToolCallStart {
            id,
            name,
            input_partial,
        } => {
            let id = id.to_string();
            buf.upsert_tool_call(id.clone(), name, input_partial.clone());
            buf.set_tool_phase(&id, ToolPhase::Queued);
        }
        LoopEvent::ToolExecutionStart { id } => {
            buf.set_tool_phase(&id.to_string(), ToolPhase::Running);
        }
        LoopEvent::ToolUpdate { id, update } => {
            buf.set_tool_progress(&id.to_string(), update.content.clone());
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
                format!("Compacted history (kept {kept}, summarized {summarized})\n{summary}"),
                true,
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
            buf.replace_or_push_custom("kage:retry", msg);
        }
        LoopEvent::Error { kind } => {
            buf.finish_streaming();
            match kind {
                LoopError::Cancelled => buf.push_custom("kage:notify", "Interrupted", false),
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
    if let Some(text) = user_text(message) {
        buf.push_user(text);
    }
    for block in &message.content {
        if let Content::Image { mime, .. } = block {
            buf.push_custom("kage:image", format!("[image: {mime}]"), false);
        }
    }
}

/// How long tool calls took in milliseconds, keyed by the message that
/// carries each result and the call id. Providers may reuse call ids
/// across turns, so the id alone does not name one call.
pub type ToolDurations = HashMap<(MessageId, String), u64>;

/// How long each tool call took, recovered from the timestamps of the
/// assistant message that made the call and the message carrying its
/// result.
#[must_use]
pub fn tool_durations(messages: &[Message]) -> ToolDurations {
    let mut started = HashMap::new();
    let mut durations = HashMap::new();
    for message in messages {
        for block in &message.content {
            match block {
                Content::ToolCall { id, .. } => {
                    started.insert(id.to_string(), message.ts);
                }
                Content::ToolResultBlock { call_id, .. } => {
                    if let Some(start) = started.remove(&call_id.to_string()) {
                        let ms = (message.ts - start).num_milliseconds();
                        durations.insert(
                            (message.id, call_id.to_string()),
                            u64::try_from(ms).unwrap_or(0),
                        );
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
/// `tool_durations` to recover real durations from session entry
/// timestamps; a call missing from the map shows none. Calls left
/// without a result read as interrupted.
pub fn populate_from_history(
    buf: &mut Buffer,
    messages: &[Message],
    tool_durations: &ToolDurations,
) {
    for msg in messages {
        match msg.role {
            Role::User => {
                if let Some(text) = user_text(msg) {
                    if is_compaction_summary(&text) {
                        buf.push_custom("kage:compaction", text, true);
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
                                buf.push_custom("kage:compaction", text.clone(), true);
                            } else {
                                buf.append_assistant_delta(text);
                                buf.finish_streaming();
                            }
                        }
                        Content::Thinking { text } => buf.push_thinking(text.clone()),
                        Content::ToolCall { id, name, input } => {
                            buf.push_tool_call(id.to_string(), name, input.clone());
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
                        // shown without a duration.
                        let duration = tool_durations.get(&(msg.id, call_id.to_string())).copied();
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
    buf.interrupt_running_tools();
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

/// The text of a user message as its bubble shows it: every text
/// block, with each attached resource shortened to one `attached` line.
/// The contents stay in the message the model receives.
fn user_text(message: &Message) -> Option<String> {
    let text: Vec<Cow<'_, str>> = message
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(match resource_block::parse(text) {
                Some(resource) => Cow::Owned(attached_line(&resource)),
                None => Cow::Borrowed(text.as_str()),
            }),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

fn attached_line(resource: &ResourceRef) -> String {
    let size = crate::image::human_bytes(resource.bytes);
    match &resource.server {
        Some(server) => format!("attached {server}:{} ({size})", resource.uri),
        None => format!("attached {} ({size})", resource.uri),
    }
}

/// One-line summary of a tool's input: the
/// [`crate::view::tool_view::describe`] target, such as the path for
/// `read` or the command for `bash`.
pub(crate) fn summarize_input(name: &str, input: &serde_json::Value) -> String {
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

    fn bash_output(text: &str, is_error: bool) -> ToolOutput {
        ToolOutput {
            is_error,
            text: text.into(),
            structured: None,
            terminate: false,
        }
    }

    #[test]
    fn tool_events_walk_the_phases() {
        let (buf, mut hooks) = fresh();
        let cid = ToolCallId::new("c1");
        let phase = |buf: &SharedBuffer| match &lock(buf).blocks()[0] {
            Block::ToolCall { phase, .. } => *phase,
            other => panic!("expected ToolCall, got {other:?}"),
        };
        hooks.on_event(&LoopEvent::ToolCallArgsDelta {
            id: cid.clone(),
            name: "bash".into(),
            input_partial: json!({}),
        });
        assert_eq!(phase(&buf), ToolPhase::Streaming);
        hooks.on_event(&LoopEvent::ToolCallStart {
            id: cid.clone(),
            name: "bash".into(),
            input_partial: json!({"command": "make"}),
        });
        assert_eq!(phase(&buf), ToolPhase::Queued);
        hooks.on_event(&LoopEvent::ToolExecutionStart { id: cid.clone() });
        assert_eq!(phase(&buf), ToolPhase::Running);
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: cid,
            output: bash_output("stderr:\nno\nexit: 2", true),
        });
        assert_eq!(phase(&buf), ToolPhase::Failed);
    }

    fn bash_start(id: &str, command: &str) -> LoopEvent {
        LoopEvent::ToolCallStart {
            id: ToolCallId::new(id),
            name: "bash".into(),
            input_partial: json!({ "command": command }),
        }
    }

    fn phases(buf: &SharedBuffer) -> Vec<ToolPhase> {
        phases_of(&lock(buf))
    }

    #[test]
    fn only_the_executing_call_runs_and_its_time_excludes_the_queue() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&bash_start("c1", "echo one"));
        hooks.on_event(&bash_start("c2", "echo two"));
        assert_eq!(phases(&buf), [ToolPhase::Queued, ToolPhase::Queued]);

        std::thread::sleep(std::time::Duration::from_millis(80));
        hooks.on_event(&LoopEvent::ToolExecutionStart {
            id: ToolCallId::new("c1"),
        });
        assert_eq!(phases(&buf), [ToolPhase::Running, ToolPhase::Queued]);
        assert!(!lock(&buf).is_timed(1), "a queued call has no timer");

        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: ToolCallId::new("c1"),
            output: bash_output("stdout:\none\nexit: 0", false),
        });
        let durations: Vec<Option<u64>> = lock(&buf)
            .blocks()
            .iter()
            .filter_map(|b| match b {
                Block::ToolResult { duration_ms, .. } => Some(*duration_ms),
                _ => None,
            })
            .collect();
        assert!(
            matches!(durations[..], [Some(ms)] if ms < 80),
            "{durations:?}"
        );
    }

    #[test]
    fn a_cancelled_call_reads_interrupted_and_keeps_its_progress() {
        let (buf, mut hooks) = fresh();
        let cid = ToolCallId::new("c1");
        hooks.on_event(&bash_start("c1", "for i in 1 2 3"));
        hooks.on_event(&LoopEvent::ToolExecutionStart { id: cid.clone() });
        hooks.on_event(&LoopEvent::ToolUpdate {
            id: cid.clone(),
            update: kage_core::ToolUpdate {
                content: "1\n2".into(),
                structured: None,
            },
        });
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: cid,
            output: bash_output(kage_core::event::TOOL_CANCELLED_TEXT, true),
        });
        assert_eq!(phases(&buf), [ToolPhase::Interrupted]);
        let mut buf = lock(&buf);
        let topo = buf.tool_topology();
        let registry = kage_core::sync::read(crate::view::registry::global());
        let lines = crate::view::build_block_lines(
            &buf,
            0,
            60,
            &topo,
            crate::view::Emphasis::None,
            &registry,
            None,
        );
        let rows: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(rows[0].contains("\u{2298} Run for i in 1 2 3"), "{rows:?}");
        assert!(rows[0].trim_end().ends_with("interrupted"), "{rows:?}");
        assert!(rows.iter().any(|r| r.trim_end().ends_with('2')), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("cancelled")), "{rows:?}");
    }

    #[test]
    fn a_cancelled_approval_reads_interrupted_without_a_duration() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&bash_start("c1", "ls"));
        lock(&buf).set_tool_phase("c1", ToolPhase::Waiting);
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: ToolCallId::new("c1"),
            output: bash_output("`bash`: permission prompt cancelled", true),
        });
        assert_eq!(phases(&buf), [ToolPhase::Interrupted]);
        assert!(matches!(
            lock(&buf).blocks()[1],
            Block::ToolResult {
                duration_ms: None,
                ..
            }
        ));
    }

    #[test]
    fn a_reused_call_id_opens_a_new_block() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&bash_start("call_0", "echo first"));
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: ToolCallId::new("call_0"),
            output: bash_output("stdout:\nfirst\nexit: 0", false),
        });
        hooks.on_event(&LoopEvent::MessageAppended {
            message: Message::new(
                Role::User,
                vec![Content::Text {
                    text: "again".into(),
                }],
                None,
            ),
        });
        hooks.on_event(&LoopEvent::ToolCallArgsDelta {
            id: ToolCallId::new("call_0"),
            name: "bash".into(),
            input_partial: json!({}),
        });
        hooks.on_event(&bash_start("call_0", "echo second"));
        hooks.on_event(&LoopEvent::ToolExecutionStart {
            id: ToolCallId::new("call_0"),
        });
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: ToolCallId::new("call_0"),
            output: bash_output("stdout:\nsecond\nexit: 0", false),
        });
        let mut buf = lock(&buf);
        let commands: Vec<String> = buf
            .blocks()
            .iter()
            .filter_map(|b| match b {
                Block::ToolCall { input, .. } => Some(input["command"].to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(commands, ["\"echo first\"", "\"echo second\""]);
        assert_eq!(phases_of(&buf), [ToolPhase::Done, ToolPhase::Done]);
        let topo = buf.tool_topology();
        assert_eq!(topo.result_of_call.get(&0), Some(&1));
        assert_eq!(topo.result_of_call.get(&3), Some(&4));
    }

    fn phases_of(buf: &Buffer) -> Vec<ToolPhase> {
        buf.blocks()
            .iter()
            .filter_map(|b| match b {
                Block::ToolCall { phase, .. } => Some(*phase),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tool_updates_show_while_running_and_the_result_replaces_them() {
        let (buf, mut hooks) = fresh();
        let cid = ToolCallId::new("c1");
        hooks.on_event(&LoopEvent::ToolCallStart {
            id: cid.clone(),
            name: "bash".into(),
            input_partial: json!({"command": "make"}),
        });
        for content in ["compiling a", "compiling a\ncompiling b"] {
            hooks.on_event(&LoopEvent::ToolUpdate {
                id: cid.clone(),
                update: kage_core::ToolUpdate {
                    content: content.into(),
                    structured: None,
                },
            });
        }
        let rendered = |buf: &SharedBuffer| {
            let mut buf = lock(buf);
            let backend = ratatui::backend::TestBackend::new(60, 12);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let input = crate::input::InputState::new();
            terminal
                .draw(|frame| {
                    let regions = crate::layout::split(
                        frame.area(),
                        crate::layout::Heights {
                            header: 1,
                            input: crate::layout::INPUT_MIN_LINES,
                            ..crate::layout::Heights::default()
                        },
                    );
                    crate::view::render(
                        frame,
                        regions,
                        &mut buf,
                        &input,
                        None,
                        &crate::view::StatusCtx::default(),
                        None,
                        &mut std::collections::BTreeMap::new(),
                        None,
                        &[],
                    );
                })
                .unwrap();
            let screen = terminal.backend().buffer().clone();
            (0..screen.area.height)
                .map(|y| {
                    (0..screen.area.width)
                        .map(|x| screen[(x, y)].symbol().to_owned())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let live = rendered(&buf);
        assert!(
            live.contains("compiling a") && live.contains("compiling b"),
            "{live}"
        );
        assert_eq!(
            live.matches("compiling a").count(),
            1,
            "replaced, not appended: {live}"
        );
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: cid,
            output: bash_output("stdout:\nall done\nexit: 0", false),
        });
        let done = rendered(&buf);
        assert!(
            done.contains("all done") && !done.contains("compiling"),
            "{done}"
        );
    }

    #[test]
    fn consecutive_provider_retries_replace_one_notice() {
        let (buf, mut hooks) = fresh();
        for attempt in 1..=3 {
            hooks.on_event(&LoopEvent::ProviderRetry {
                attempt,
                max_attempts: 5,
                wait_secs: 1,
                requested_secs: None,
                error: "overloaded".into(),
            });
        }
        let buf = lock(&buf);
        assert_eq!(buf.blocks().len(), 1);
        assert!(matches!(
            &buf.blocks()[0],
            Block::Custom { text, .. } if text.contains("retrying 3/5")
        ));
    }

    #[test]
    fn text_after_thinking_finishes_the_thinking_block() {
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::ThinkingDelta {
            id: id(),
            delta: "hmm".into(),
        });
        hooks.on_event(&LoopEvent::TextDelta {
            id: id(),
            delta: "ok".into(),
        });
        let buf = lock(&buf);
        assert!(matches!(
            buf.blocks()[0],
            Block::Thinking {
                live: false,
                folded: true,
                duration_ms: Some(_),
                ..
            }
        ));
        assert!(!buf.is_timed(0));
    }

    #[test]
    fn replayed_thinking_has_no_timing_and_orphan_calls_are_interrupted() {
        let mut buf = Buffer::new();
        let history = vec![Message::new(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: "plan".into(),
                },
                Content::ToolCall {
                    id: ToolCallId::new("c1"),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                },
            ],
            None,
        )];
        populate_from_history(&mut buf, &history, &HashMap::new());
        assert!(matches!(
            buf.blocks()[0],
            Block::Thinking {
                duration_ms: None,
                folded: true,
                ..
            }
        ));
        assert!(matches!(
            buf.blocks()[1],
            Block::ToolCall {
                phase: ToolPhase::Interrupted,
                ..
            }
        ));
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
                assert_eq!(text, "Interrupted");
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
    fn resource_blocks_show_as_attached_lines_live_and_on_replay() {
        let message = Message::new(
            Role::User,
            vec![
                Content::Text {
                    text: "compare @docs:file:///a and the context".into(),
                },
                Content::Text {
                    text: resource_block::render("file:///a", Some("docs"), None, "secret body"),
                },
                Content::Text {
                    text: resource_block::render("file:///b.rs", None, Some("text/x-rust"), ""),
                },
            ],
            None,
        );
        let want = "compare @docs:file:///a and the context\n\
                    attached docs:file:///a (11 B)\n\
                    attached file:///b.rs (0 B)";
        let (buf, mut hooks) = fresh();
        hooks.on_event(&LoopEvent::MessageAppended {
            message: message.clone(),
        });
        assert!(matches!(&lock(&buf).blocks()[0], Block::User { text } if text == want));
        let mut replayed = Buffer::new();
        populate_from_history(&mut replayed, &[message], &HashMap::new());
        assert!(matches!(&replayed.blocks()[0], Block::User { text } if text == want));
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
        let key = (result.id, "c1".to_owned());
        assert_eq!(tool_durations(&[call, result])[&key], 250);
    }

    #[test]
    fn a_call_id_reused_in_a_later_turn_keeps_each_duration() {
        let turn = |ms: i64| {
            let call = Message::new(
                Role::Assistant,
                vec![Content::ToolCall {
                    id: ToolCallId::new("call_0"),
                    name: "agent".into(),
                    input: json!({}),
                }],
                None,
            );
            let mut result = Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new("call_0"),
                    output: "ok".into(),
                    is_error: false,
                }],
                None,
            );
            result.ts = call.ts + chrono::Duration::milliseconds(ms);
            [call, result]
        };
        let history: Vec<Message> = turn(8_000).into_iter().chain(turn(2_000)).collect();
        let mut buf = Buffer::new();
        populate_from_history(&mut buf, &history, &tool_durations(&history));
        let durations: Vec<Option<u64>> = buf
            .blocks()
            .iter()
            .filter_map(|b| match b {
                Block::ToolResult { duration_ms, .. } => Some(*duration_ms),
                _ => None,
            })
            .collect();
        assert_eq!(durations, [Some(8_000), Some(2_000)]);
    }
}
