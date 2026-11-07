//! The TUI's store over an ACP connection.
//!
//! [`Store`] wraps the sans-IO [`kage_client::Client`]: the connection
//! feeds it the frames a kage agent sends and drains the [`Frame`]s to
//! answer, and the mirrored [`State`] is what the TUI draws. Every
//! `session/update` also folds into that session's [`SharedBuffer`],
//! the blocks the renderer walks, so live frames and the host's
//! replayed history run through one reducer that produces the same
//! shapes the engine path does in `events.rs`. A prompt echoes to the
//! other clients only, so the connection's own prompts never arrive
//! as frames: whoever submits one adds its bubble, as the engine path
//! does. The `_kage/ui` snapshot's plugin data is carried too:
//! [`Store::plugin_commands`] and [`Store::plugin_keymap`] translate
//! it into the shapes the command registry and the key router
//! consume.

use std::collections::BTreeMap;

use kage_client::wire::{
    CompactionPhase, ContentBlock, NoticeTone, SessionNotification, SessionUpdate, ToolCallContent,
    ToolCallMeta, ToolCallStatus, TurnPhase, UiCommand, UiCommandArg,
};
use kage_client::{Change, Client, Frame, State};
use kage_core::keymap::{Keymap, MappingData};
use kage_core::protocol::CompactionCounts;
use kage_core::sync::lock;
use serde::Deserialize;
use serde_json::Value;

use crate::buffer::Buffer;
use crate::command::{OwnedArgSpec, PluginCommand};
use crate::events::{
    SharedBuffer, compaction_text, produced_media_label, push_user_text, shared_buffer,
};
use crate::view::tool_view::ToolPhase;

/// The TUI's mirror of one ACP connection: the client's state, plus
/// the buffer each session folds into.
#[derive(Debug, Default)]
pub struct Store {
    client: Client,
    buffers: BTreeMap<String, SharedBuffer>,
}

impl Store {
    /// A store that has sent nothing and heard nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Handles one incoming frame: the client mirrors the agent into
    /// its state, and a `session/update` folds into its session's
    /// buffer, which a replaced session gets fresh. Reports what
    /// moved.
    pub fn absorb(&mut self, frame: Frame) -> Vec<Change> {
        if let Frame::Notification { method, params } = &frame
            && method == "session/update"
            && let Ok(note) = serde_json::from_value::<SessionNotification>(params.clone())
        {
            if matches!(note.update, SessionUpdate::SessionReplaced(_)) {
                self.reset_session(&note.session_id);
            } else {
                apply_update(
                    &mut lock(&self.shared_buffer(&note.session_id)),
                    &note.update,
                );
            }
        }
        self.client.handle(frame)
    }

    /// The frames to send, in the order they were produced, drained by
    /// the call.
    pub fn take_outgoing(&mut self) -> Vec<Frame> {
        self.client.take_outgoing()
    }

    /// The client the store mirrors, for the connection to issue its
    /// commands through: prompts, answers and the rest.
    pub fn client(&mut self) -> &mut Client {
        &mut self.client
    }

    /// The state as the handled frames left it.
    #[must_use]
    pub fn state(&self) -> &State {
        self.client.state()
    }

    /// The buffer `session_id` folds into, made on first use. Hand it
    /// to the renderer and keep absorbing: frames that land later show
    /// up in it.
    pub fn shared_buffer(&mut self, session_id: &str) -> SharedBuffer {
        self.buffers
            .entry(session_id.to_owned())
            .or_insert_with(shared_buffer)
            .clone()
    }

    /// A fresh buffer for `session_id`, dropping what it held: a
    /// replaced or reloaded session replays over a clean slate.
    /// Handles taken before keep the old blocks, so hand the returned
    /// one to the renderer.
    pub fn reset_session(&mut self, session_id: &str) -> SharedBuffer {
        let fresh = shared_buffer();
        self.buffers.insert(session_id.to_owned(), fresh.clone());
        fresh
    }

    /// Ends every still-running tool call in every buffer: the
    /// connection closed, so no update will finish them.
    pub fn interrupt_running(&mut self) {
        for buffer in self.buffers.values() {
            lock(buffer).interrupt_running_tools();
        }
    }

    /// The plugin commands the attached `_kage/ui` offers, in the
    /// shapes the command registry consumes. Empty without an
    /// attachment.
    #[must_use]
    pub fn plugin_commands(&self) -> Vec<PluginCommand> {
        let Some(ui) = self.state().plugin_ui.as_ref() else {
            return Vec::new();
        };
        ui.commands.iter().map(plugin_command).collect()
    }

    /// The keymap the attached `_kage/ui` offers, rebuilt from the
    /// snapshot's mappings. `None` without an attachment, or when the
    /// mappings do not rebuild, so a broken snapshot cannot half
    /// replace what the App routes with.
    #[must_use]
    pub fn plugin_keymap(&self) -> Option<Keymap> {
        let ui = self.state().plugin_ui.as_ref()?;
        let mappings = ui
            .keymap
            .iter()
            .map(MappingData::deserialize)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        let mut keymap = Keymap::new();
        keymap.replace_with(&mappings).ok()?;
        Some(keymap)
    }
}

/// Fold one `session/update` into `buf`, the shapes the engine path
/// produces in `events.rs`. Kinds the transcript does not draw, such
/// as command lists, leave it alone. A `tool_call_update` without
/// `raw_input` keeps the input it showed.
fn apply_update(buf: &mut Buffer, update: &SessionUpdate) {
    match update {
        SessionUpdate::UserMessageChunk(chunk) => push_chunk(buf, &chunk.content),
        SessionUpdate::AgentMessageChunk(chunk) => match chunk.content.as_text() {
            Some(text) => buf.append_assistant_delta(text),
            None => push_media(buf, &chunk.content),
        },
        SessionUpdate::AgentThoughtChunk(chunk) => {
            if let Some(text) = chunk.content.as_text() {
                buf.append_thinking_delta(text);
            }
        }
        SessionUpdate::ToolCall(call) => {
            buf.upsert_tool_call(
                call.tool_call_id.clone(),
                call.title.clone(),
                call.raw_input.clone().unwrap_or(Value::Null),
            );
            set_status(buf, &call.tool_call_id, Some(call.status));
            land_result(
                buf,
                &call.tool_call_id,
                Some(call.status),
                &call.content,
                meta_duration(call.meta.as_ref()),
            );
        }
        SessionUpdate::ToolCallUpdate(update) => {
            if let Some(input) = &update.raw_input {
                match &update.title {
                    Some(title) => buf.upsert_tool_call(
                        update.tool_call_id.clone(),
                        title.clone(),
                        input.clone(),
                    ),
                    None => {
                        buf.refresh_tool_input(update.tool_call_id.clone(), input.clone());
                    }
                }
            }
            set_status(buf, &update.tool_call_id, update.status);
            let output = content_text(&update.content);
            if !output.is_empty() {
                buf.set_tool_progress(&update.tool_call_id, output);
            }
            land_result(
                buf,
                &update.tool_call_id,
                update.status,
                &update.content,
                meta_duration(update.meta.as_ref()),
            );
        }
        SessionUpdate::Plan(plan) => {
            let lines = plan
                .entries
                .iter()
                .filter_map(|entry| entry.get("content").and_then(Value::as_str))
                .collect::<Vec<_>>();
            if !lines.is_empty() {
                buf.replace_or_push_custom("kage:plan", lines.join("\n"));
            }
        }
        SessionUpdate::Turn(turn) if turn.phase == TurnPhase::End => {
            buf.finish_streaming();
        }
        SessionUpdate::Notice(notice) => {
            if notice.tone == NoticeTone::Error {
                buf.push_custom("kage:error", notice.text.clone(), false);
            } else {
                buf.replace_or_push_custom("kage:notify", &notice.text);
            }
        }
        SessionUpdate::Compaction(compaction) => {
            if compaction.phase != CompactionPhase::Complete {
                return;
            }
            let counts = compaction.summarized.map(|summarized| CompactionCounts {
                summarized: usize::try_from(summarized).unwrap_or(0),
                kept: usize::try_from(compaction.kept.unwrap_or(0)).unwrap_or(0),
            });
            let tokens = match (compaction.before, compaction.after) {
                (Some(before), Some(after)) => Some((before, after)),
                _ => None,
            };
            buf.push_custom(
                "kage:compaction",
                compaction_text(counts, tokens, compaction.summary.as_deref().unwrap_or("")),
                true,
            );
        }
        _ => {}
    }
}

/// A user chunk: text becomes the user bubble, anything else falls to
/// [`push_media`].
fn push_chunk(buf: &mut Buffer, content: &ContentBlock) {
    match content.as_text() {
        Some(text) => push_user_text(buf, text.to_owned()),
        None => push_media(buf, content),
    }
}

/// An image or audio chunk becomes a labelled block; other non-text
/// content does not read in the transcript.
fn push_media(buf: &mut Buffer, content: &ContentBlock) {
    let (ContentBlock::Image(blob) | ContentBlock::Audio(blob)) = content else {
        return;
    };
    let label = produced_media_label(&blob.mime_type, blob.data.len() / 4 * 3);
    buf.push_custom("kage:image", label, false);
}

/// Move the call to the phase its wire status renders as. A status
/// this build does not know stays queued, so an unknown state cannot
/// look like a result.
fn set_status(buf: &mut Buffer, call_id: &str, status: Option<ToolCallStatus>) {
    let Some(status) = status else {
        return;
    };
    let phase = match status {
        ToolCallStatus::Pending | ToolCallStatus::Unknown => ToolPhase::Queued,
        ToolCallStatus::InProgress => ToolPhase::Running,
        ToolCallStatus::Completed => ToolPhase::Done,
        ToolCallStatus::Failed => ToolPhase::Failed,
    };
    buf.set_tool_phase(call_id, phase);
}

/// Push the call's result once its status says it has one, then the
/// image and audio its content carried, as the engine path does after
/// a result.
fn land_result(
    buf: &mut Buffer,
    call_id: &str,
    status: Option<ToolCallStatus>,
    content: &[ToolCallContent],
    took_ms: Option<u64>,
) {
    let Some(status) = status else {
        return;
    };
    if !matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed) {
        return;
    }
    buf.push_tool_result_with_duration(
        call_id,
        content_text(content),
        status == ToolCallStatus::Failed,
        took_ms,
    );
    for item in content {
        if let ToolCallContent::Content(chunk) = item {
            push_media(buf, &chunk.content);
        }
    }
}

/// The text across the content's chunks; diffs and terminals do not
/// read as text.
fn content_text(content: &[ToolCallContent]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Content(chunk) => chunk.content.as_text().map(str::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `durationMs` a kage agent reports about its own call, when it
/// does.
fn meta_duration(meta: Option<&ToolCallMeta>) -> Option<u64> {
    meta.and_then(|meta| meta.kage.duration_ms)
}

/// Translate one snapshot command into the shape the command registry
/// consumes.
fn plugin_command(command: &UiCommand) -> PluginCommand {
    PluginCommand {
        name: command.name.clone(),
        aliases: command.aliases.clone(),
        is_override: command.overrides,
        description: command.description.clone(),
        args: command.args.iter().map(plugin_arg).collect(),
    }
}

/// Translate one snapshot argument spec.
fn plugin_arg(arg: &UiCommandArg) -> OwnedArgSpec {
    match arg {
        UiCommandArg::Text {
            name,
            optional,
            hint,
        } => OwnedArgSpec::Text {
            name: name.clone(),
            optional: *optional,
            hint: hint.clone(),
        },
        UiCommandArg::Choice {
            name,
            values,
            optional,
        } => OwnedArgSpec::Choice {
            name: name.clone(),
            values: values.clone(),
            optional: *optional,
        },
        UiCommandArg::Path { name, optional } => OwnedArgSpec::Path {
            name: name.clone(),
            optional: *optional,
        },
        UiCommandArg::Session { name, optional } => OwnedArgSpec::Session {
            name: name.clone(),
            optional: *optional,
        },
        UiCommandArg::Flag { name } => OwnedArgSpec::Flag { name: name.clone() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Block, MAX_BLOCKS};
    use kage_client::{PermissionDecision, RequestId};
    use kage_core::keymap::RhsData;
    use serde_json::json;

    /// A `session/update` notification for `session`.
    fn update(session: &str, kind: &str, fields: &Value) -> Frame {
        let mut params = json!({"sessionId": session, "update": {"sessionUpdate": kind}});
        let object = params["update"].as_object_mut().unwrap();
        for (key, value) in fields.as_object().unwrap() {
            object.insert(key.clone(), value.clone());
        }
        Frame::Notification {
            method: "session/update".to_owned(),
            params,
        }
    }

    /// The kinds of the blocks a session folded into.
    fn kinds(buffer: &Buffer) -> Vec<String> {
        buffer
            .blocks()
            .iter()
            .map(|block| match block.as_ref() {
                Block::User { .. } => "user".to_owned(),
                Block::Assistant { .. } => "assistant".to_owned(),
                Block::Thinking { .. } => "thinking".to_owned(),
                Block::ToolCall { name, .. } => format!("call:{name}"),
                Block::ToolResult { name, .. } => format!("result:{name}"),
                Block::Custom { kind, .. } => kind.clone(),
            })
            .collect()
    }

    #[test]
    fn a_session_folds_into_the_blocks_the_engine_path_renders() {
        let mut store = Store::new();
        store.absorb(update(
            "s1",
            "agent_message_chunk",
            &json!({"content": {"type": "text", "text": "working on it"}}),
        ));
        store.absorb(update(
            "s1",
            "tool_call",
            &json!({
                "toolCallId": "c1",
                "title": "shell",
                "kind": "execute",
                "status": "in_progress",
                "rawInput": {"command": "ls -la"},
            }),
        ));
        store.absorb(update(
            "s1",
            "tool_call_update",
            &json!({
                "toolCallId": "c1",
                "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "3 files"}}],
            }),
        ));
        store.absorb(update(
            "s1",
            "user_message_chunk",
            &json!({"content": {"type": "text", "text": "thanks"}}),
        ));

        let handle = store.shared_buffer("s1");
        let buffer = lock(&handle);
        assert_eq!(
            kinds(&buffer),
            ["assistant", "call:shell", "result:shell", "user"],
            "{buffer:#?}"
        );
        let blocks = buffer.blocks();
        let Block::ToolCall { phase, .. } = blocks[1].as_ref() else {
            panic!("expected the tool call, got {:#?}", blocks[1]);
        };
        assert_eq!(*phase, ToolPhase::Done);
        let Block::ToolResult {
            output, is_error, ..
        } = blocks[2].as_ref()
        else {
            panic!("expected the tool result, got {:#?}", blocks[2]);
        };
        assert_eq!(output, "3 files");
        assert!(!*is_error);
    }

    #[test]
    fn a_failed_call_reads_as_its_error() {
        let mut store = Store::new();
        store.absorb(update(
            "s1",
            "tool_call",
            &json!({
                "toolCallId": "c1",
                "title": "shell",
                "kind": "execute",
                "status": "failed",
                "content": [{"type": "content", "content": {"type": "text", "text": "boom"}}],
            }),
        ));
        let handle = store.shared_buffer("s1");
        let buffer = lock(&handle);
        let blocks = buffer.blocks();
        let Block::ToolResult { is_error, .. } = blocks[1].as_ref() else {
            panic!("expected the tool result, got {:#?}", blocks[1]);
        };
        assert!(*is_error);
    }

    #[test]
    fn a_finished_call_reports_how_long_it_took() {
        let mut store = Store::new();
        store.absorb(update(
            "s1",
            "tool_call",
            &json!({
                "toolCallId": "c1",
                "title": "shell",
                "kind": "execute",
                "status": "in_progress",
            }),
        ));
        store.absorb(update(
            "s1",
            "tool_call_update",
            &json!({
                "toolCallId": "c1",
                "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "3 files"}}],
                "_meta": {"kage": {"durationMs": 1200}},
            }),
        ));
        let handle = store.shared_buffer("s1");
        let buffer = lock(&handle);
        let blocks = buffer.blocks();
        let Block::ToolResult { duration_ms, .. } = blocks[1].as_ref() else {
            panic!("expected the tool result, got {:#?}", blocks[1]);
        };
        assert_eq!(*duration_ms, Some(1200));
    }

    #[test]
    fn the_rest_of_the_vocabulary_lands_in_the_kinds_the_tui_knows() {
        let mut store = Store::new();
        store.absorb(update(
            "s1",
            "agent_thought_chunk",
            &json!({"content": {"type": "text", "text": "pondering"}}),
        ));
        store.absorb(update(
            "s1",
            "_kage/notice",
            &json!({"tone": "info", "text": "hooks loaded"}),
        ));
        store.absorb(update(
            "s1",
            "_kage/notice",
            &json!({"tone": "error", "text": "the run failed"}),
        ));
        store.absorb(update(
            "s1",
            "plan",
            &json!({"entries": [{"content": "read the code"}, {"content": "write the fix"}]}),
        ));
        store.absorb(update(
            "s1",
            "_kage/compaction",
            &json!({"kept": 4, "summarized": 9, "summary": "the earlier work"}),
        ));

        let handle = store.shared_buffer("s1");
        let buffer = lock(&handle);
        assert_eq!(
            kinds(&buffer),
            [
                "thinking",
                "kage:notify",
                "kage:error",
                "kage:plan",
                "kage:compaction",
            ],
            "{buffer:#?}"
        );
        let blocks = buffer.blocks();
        let Block::Custom { text, .. } = blocks[3].as_ref() else {
            panic!("expected the plan, got {:#?}", blocks[3]);
        };
        assert_eq!(text, "read the code\nwrite the fix");
        let Block::Custom { text, .. } = blocks[4].as_ref() else {
            panic!("expected the compaction, got {:#?}", blocks[4]);
        };
        assert!(text.contains("the earlier work"), "{text}");
        assert!(
            !text.contains("tokens"),
            "no usage means no token delta: {text}"
        );
    }

    #[test]
    fn a_compaction_reports_its_token_delta() {
        let mut store = Store::new();
        store.absorb(update(
            "s1",
            "_kage/compaction",
            &json!({
                "phase": "complete",
                "before": 84000,
                "after": 12000,
                "summarized": 9,
                "kept": 4,
                "summary": "the earlier work",
            }),
        ));
        let handle = store.shared_buffer("s1");
        let buffer = lock(&handle);
        let blocks = buffer.blocks();
        let Block::Custom { text, .. } = blocks[0].as_ref() else {
            panic!("expected the compaction, got {:#?}", blocks[0]);
        };
        assert!(text.contains("9 messages summarized"), "{text}");
        assert!(text.contains("84k"), "{text}");
        assert!(text.contains("12k"), "{text}");
    }

    #[test]
    fn an_answer_flows_back_out_and_leaves_the_transcript() {
        let mut store = Store::new();
        store.absorb(update(
            "s1",
            "tool_call",
            &json!({
                "toolCallId": "c1",
                "title": "shell",
                "kind": "execute",
                "status": "pending",
            }),
        ));
        store.absorb(Frame::Request {
            id: RequestId::Number(7),
            method: "session/request_permission".to_owned(),
            params: json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "c1", "title": "shell", "kind": "execute"},
                "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"}],
            }),
        });
        assert_eq!(store.state().session("s1").unwrap().permissions.len(), 1);

        store
            .client()
            .reply_permission("s1", RequestId::Number(7), &PermissionDecision::Allow);
        let outgoing = store.take_outgoing();
        assert!(
            matches!(&outgoing[..], [Frame::Success { id, .. }] if *id == RequestId::Number(7)),
            "the answer goes out as a success frame: {outgoing:#?}"
        );
        assert!(store.state().session("s1").unwrap().permissions.is_empty());

        let handle = store.shared_buffer("s1");
        let buffer = lock(&handle);
        assert_eq!(kinds(&buffer), ["call:shell"], "{buffer:#?}");
    }

    #[test]
    fn a_buffer_taken_earlier_still_sees_later_frames() {
        let mut store = Store::new();
        store.absorb(update(
            "s1",
            "agent_message_chunk",
            &json!({"content": {"type": "text", "text": "one"}}),
        ));
        let handle = store.shared_buffer("s1");
        store.absorb(update(
            "s1",
            "agent_message_chunk",
            &json!({"content": {"type": "text", "text": " two"}}),
        ));
        let buffer = lock(&handle);
        assert_eq!(kinds(&buffer), ["assistant"], "{buffer:#?}");
        let Block::Assistant { text, .. } = buffer.blocks()[0].as_ref() else {
            panic!(
                "expected the assistant block, got {:#?}",
                buffer.blocks()[0]
            );
        };
        assert_eq!(text, "one two");
    }

    #[test]
    fn a_closed_connection_ends_the_running_calls() {
        let mut store = Store::new();
        for session in ["s1", "s2"] {
            store.absorb(update(
                session,
                "tool_call",
                &json!({
                    "toolCallId": "c1",
                    "title": "shell",
                    "kind": "execute",
                    "status": "in_progress",
                }),
            ));
        }
        store.interrupt_running();
        for session in ["s1", "s2"] {
            let handle = store.shared_buffer(session);
            let buffer = lock(&handle);
            let blocks = buffer.blocks();
            let Block::ToolCall { phase, .. } = blocks[0].as_ref() else {
                panic!("expected the tool call, got {:#?}", blocks[0]);
            };
            assert_eq!(*phase, ToolPhase::Interrupted, "{session}");
        }
    }

    #[test]
    fn a_long_session_folds_every_frame() {
        let mut store = Store::new();
        for turn in 0..(3 * MAX_BLOCKS) {
            store.absorb(update(
                "s1",
                "user_message_chunk",
                &json!({"content": {"type": "text", "text": format!("turn {turn}")}}),
            ));
        }
        let handle = store.shared_buffer("s1");
        let buffer = lock(&handle);
        let rendered = buffer.blocks().len();
        assert_eq!(
            rendered,
            3 * MAX_BLOCKS,
            "every frame folds; the renderer compacts at draw, as on the engine path"
        );
    }

    #[test]
    fn a_titleless_input_update_keeps_the_tool_row() {
        let mut store = Store::new();
        store.absorb(update(
            "s1",
            "tool_call",
            &json!({
                "toolCallId": "c1",
                "title": "shell",
                "kind": "execute",
                "status": "in_progress",
                "rawInput": {"command": "echo one"},
            }),
        ));
        // kage's own updates carry input fragments without a title.
        store.absorb(update(
            "s1",
            "tool_call_update",
            &json!({
                "toolCallId": "c1",
                "rawInput": {"command": "echo one; echo two"},
            }),
        ));
        {
            let handle = store.shared_buffer("s1");
            let buffer = lock(&handle);
            let blocks = buffer.blocks();
            let Block::ToolCall { name, input, .. } = blocks[0].as_ref() else {
                panic!("expected the tool call, got {:#?}", blocks[0]);
            };
            assert_eq!(name, "shell", "a titleless update keeps the name");
            assert_eq!(input["command"], "echo one; echo two");
        }

        // A titled update renames the row.
        store.absorb(update(
            "s1",
            "tool_call_update",
            &json!({
                "toolCallId": "c1",
                "title": "shell (fast)",
                "rawInput": {"command": "echo three"},
            }),
        ));
        let handle = store.shared_buffer("s1");
        let buffer = lock(&handle);
        let blocks = buffer.blocks();
        let Block::ToolCall { name, .. } = blocks[0].as_ref() else {
            panic!("expected the tool call, got {:#?}", blocks[0]);
        };
        assert_eq!(name, "shell (fast)");
    }

    #[test]
    fn a_replaced_session_folds_over_a_clean_slate() {
        let mut store = Store::new();
        store.absorb(update(
            "s1",
            "agent_message_chunk",
            &json!({"content": {"type": "text", "text": "old"}}),
        ));
        store.absorb(update("s1", "_kage/session_replaced", &json!({})));
        store.absorb(update(
            "s1",
            "agent_message_chunk",
            &json!({"content": {"type": "text", "text": "new"}}),
        ));
        let handle = store.shared_buffer("s1");
        let buffer = lock(&handle);
        let blocks = buffer.blocks();
        assert_eq!(blocks.len(), 1, "{buffer:#?}");
        let Block::Assistant { text, .. } = blocks[0].as_ref() else {
            panic!("expected the replayed text, got {:#?}", blocks[0]);
        };
        assert_eq!(text, "new");
    }

    #[test]
    fn media_chunks_become_labelled_rows() {
        let mut store = Store::new();
        store.absorb(update(
            "s1",
            "user_message_chunk",
            &json!({"content": {"type": "audio", "data": "aGVsbG8=", "mimeType": "audio/wav"}}),
        ));
        store.absorb(update(
            "s1",
            "tool_call",
            &json!({
                "toolCallId": "c1",
                "title": "render",
                "kind": "other",
                "status": "completed",
                "content": [
                    {"type": "content", "content": {"type": "text", "text": "here"}},
                    {"type": "content", "content": {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}},
                ],
            }),
        ));
        let handle = store.shared_buffer("s1");
        let buffer = lock(&handle);
        assert_eq!(
            kinds(&buffer),
            ["kage:image", "call:render", "result:render", "kage:image"],
            "{buffer:#?}"
        );
    }

    /// An attached `_kage/ui` over `/proj`, holding `snapshot`.
    fn attached(snapshot: Value) -> Store {
        let mut store = Store::new();
        let id = store.client().ui_attach("/proj", None);
        store.absorb(Frame::Success {
            id: RequestId::Number(id),
            result: snapshot,
        });
        let _ = store.take_outgoing();
        store
    }

    /// A snapshot body: `fields` merged over the empty snapshot's
    /// fields, which the wire always writes in full.
    fn snapshot(fields: &Value) -> Value {
        let mut base = json!({
            "generation": 0,
            "keymap": [],
            "highlights": null,
            "options": {},
            "optionSources": {},
            "commands": [],
            "status": {},
            "blockKinds": [],
            "autocomplete": false,
            "terminalHooks": false,
        });
        let object = base.as_object_mut().unwrap();
        for (key, value) in fields.as_object().unwrap() {
            object.insert(key.clone(), value.clone());
        }
        base
    }

    #[test]
    fn the_snapshot_commands_translate_into_registry_shapes() {
        let store = attached(snapshot(&json!({
            "commands": [
                {
                    "name": "greet",
                    "aliases": ["hi"],
                    "description": "say hi",
                    "overrides": true,
                    "args": [
                        {"kind": "text", "name": "who", "optional": false, "hint": "a name"},
                        {"kind": "choice", "name": "tone", "values": ["warm", "cool"], "optional": true},
                        {"kind": "flag", "name": "loud"},
                    ],
                },
                {
                    "name": "open",
                    "description": "open a path",
                    "args": [
                        {"kind": "path", "name": "p", "optional": false},
                        {"kind": "session", "name": "s", "optional": true},
                    ],
                },
            ],
        })));
        assert_eq!(
            store.plugin_commands(),
            [
                PluginCommand {
                    name: "greet".to_owned(),
                    aliases: vec!["hi".to_owned()],
                    is_override: true,
                    description: "say hi".to_owned(),
                    args: vec![
                        OwnedArgSpec::Text {
                            name: "who".to_owned(),
                            optional: false,
                            hint: "a name".to_owned(),
                        },
                        OwnedArgSpec::Choice {
                            name: "tone".to_owned(),
                            values: vec!["warm".to_owned(), "cool".to_owned()],
                            optional: true,
                        },
                        OwnedArgSpec::Flag {
                            name: "loud".to_owned(),
                        },
                    ],
                },
                PluginCommand {
                    name: "open".to_owned(),
                    aliases: Vec::new(),
                    is_override: false,
                    description: "open a path".to_owned(),
                    args: vec![
                        OwnedArgSpec::Path {
                            name: "p".to_owned(),
                            optional: false,
                        },
                        OwnedArgSpec::Session {
                            name: "s".to_owned(),
                            optional: true,
                        },
                    ],
                },
            ],
        );
    }

    #[test]
    fn the_snapshot_keymap_rebuilds_into_a_routable_keymap() {
        let store = attached(snapshot(&json!({
            "keymap": [
                {
                    "mode": "n",
                    "lhs": "<C-x>g",
                    "rhs": {"kind": "command", "line": ":echo hi"},
                    "desc": "say hi",
                    "owner": "greet-plugin",
                },
                {
                    "mode": "i",
                    "lhs": "<C-r>",
                    "rhs": {"kind": "lua", "id": 7},
                    "owner": "greet-plugin",
                },
                {
                    "mode": "n",
                    "lhs": "gn",
                    "rhs": {"kind": "nop"},
                    "owner": "greet-plugin",
                },
                {
                    "mode": "n",
                    "lhs": "zc",
                    "rhs": {"kind": "action", "name": "BeginCommand"},
                    "owner": "greet-plugin",
                },
            ],
        })));
        let keymap = store.plugin_keymap().expect("the mappings rebuild");
        let data = keymap.to_data();
        assert_eq!(data.len(), 4, "{data:#?}");
        assert!(
            data.iter().any(|m| m.mode == "n"
                && m.lhs == "zc"
                && m.rhs
                    == RhsData::Action {
                        name: "BeginCommand".to_owned(),
                        arg: None,
                    }),
            "{data:#?}"
        );
        assert!(
            data.iter().any(|m| m.mode == "n"
                && m.lhs == "<C-x>g"
                && m.rhs
                    == RhsData::Command {
                        line: ":echo hi".to_owned()
                    }),
            "{data:#?}"
        );
        assert!(
            data.iter()
                .any(|m| m.mode == "i" && m.lhs == "<C-r>" && m.rhs == RhsData::Lua { id: 7 }),
            "{data:#?}"
        );
    }

    #[test]
    fn a_broken_snapshot_yields_no_keymap() {
        for keymap in [
            json!([{
                "mode": "n",
                "lhs": "zk",
                "rhs": {"kind": "action", "name": "no_such_action"},
                "owner": "p",
            }]),
            json!([{
                "mode": "q",
                "lhs": "zk",
                "rhs": {"kind": "nop"},
                "owner": "p",
            }]),
            json!([{
                "mode": "n",
                "lhs": "zk",
                "rhs": {"kind": "wat"},
                "owner": "p",
            }]),
            json!([{
                "mode": "n",
                "lhs": "zk",
                "rhs": {"kind": "nop"},
            }]),
        ] {
            let body = json!({"keymap": keymap});
            let store = attached(snapshot(&body));
            assert!(
                store.plugin_keymap().is_none(),
                "a broken mapping cannot rebuild: {body}"
            );
        }
    }

    #[test]
    fn without_an_attachment_the_plugin_data_is_empty() {
        let store = Store::new();
        assert!(store.plugin_commands().is_empty());
        assert!(store.plugin_keymap().is_none());
    }
}
