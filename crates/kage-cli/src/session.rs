//! Bridges the agent loop's event stream to a session writer.
//!
//! [`SessionRecordingHooks`] wraps any inner [`Hooks`] implementation,
//! delegating every callback to it while also persisting the conversation
//! to a [`SessionWriter`]. The hook reassembles streamed assistant deltas
//! back into a single [`Message`] so the on-disk session is the same shape
//! as `cx.history`, and writes one [`SessionEntry::Message`] per turn plus
//! one per tool result.
//!
//! Initial user prompts are not visible as loop events, so callers must
//! call [`SessionRecordingHooks::record_user_message`] explicitly before
//! invoking [`run`](kage_loop::run).

use std::mem;
use std::sync::Arc;

use chrono::Utc;
use kage_core::{Content, LoopEvent, Message, MessageId, Role, ToolCallId};
use kage_loop::Hooks;
use kage_plugin::{PendingSessionOp, PluginRuntime};
use kage_session::{Compaction, Custom, EntryId, Label, MessageEntry, SessionEntry, SessionWriter};

/// Wraps another [`Hooks`] and persists the conversation to a session file.
#[derive(Debug)]
pub struct SessionRecordingHooks<H: Hooks> {
    inner: H,
    writer: SessionWriter,
    pending: Pending,
    /// Optional plugin runtime whose
    /// [`PluginRuntime::take_pending_session_ops`] queue is drained
    /// after each turn. When `None` the recorder behaves exactly like
    /// pre-plugin builds; the field exists so the print and TUI
    /// hosts can both opt in without duplicating recorder logic.
    runtime: Option<Arc<PluginRuntime>>,
}

#[derive(Debug, Default)]
struct Pending {
    msg_id: Option<MessageId>,
    text: String,
    thinking: String,
    tool_calls: Vec<(ToolCallId, String, serde_json::Value)>,
}

impl Pending {
    fn start(&mut self, id: MessageId) {
        self.msg_id = Some(id);
        self.text.clear();
        self.thinking.clear();
        self.tool_calls.clear();
    }

    fn finalize(&mut self) -> Option<Message> {
        let id = self.msg_id.take()?;
        let mut content = Vec::new();
        if !self.thinking.is_empty() {
            content.push(Content::Thinking {
                text: mem::take(&mut self.thinking),
            });
        }
        if !self.text.is_empty() {
            content.push(Content::Text {
                text: mem::take(&mut self.text),
            });
        }
        for (call_id, name, input) in mem::take(&mut self.tool_calls) {
            content.push(Content::ToolCall {
                id: call_id,
                name,
                input,
            });
        }
        Some(Message {
            role: Role::Assistant,
            content,
            id,
            parent: None,
            ts: Utc::now(),
        })
    }
}

impl<H: Hooks> SessionRecordingHooks<H> {
    /// Wrap `inner` and forward all loop callbacks while persisting them.
    pub fn new(inner: H, writer: SessionWriter) -> Self {
        Self {
            inner,
            writer,
            pending: Pending::default(),
            runtime: None,
        }
    }

    /// Attach a plugin runtime so the recorder drains its
    /// `take_pending_session_ops` queue after every turn. Calling
    /// this is optional; without it the recorder behaves exactly as
    /// it did pre-PE.D.
    #[must_use]
    pub fn with_plugin_runtime(mut self, runtime: Arc<PluginRuntime>) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Apply one plugin-requested session op to the writer.
    fn apply_plugin_op(&mut self, op: PendingSessionOp) {
        let entry = match op {
            PendingSessionOp::AppendCustom { kind, data } => SessionEntry::Custom(Custom {
                id: EntryId::new(),
                ts: Utc::now(),
                kind,
                data,
            }),
            PendingSessionOp::SetLabel { anchor, text } => {
                // Parse the plugin-supplied anchor id back into a
                // ULID. A malformed id is logged and the label is
                // dropped: writing a label with a fresh id would
                // silently detach from its intended target.
                let Ok(parsed) = ulid::Ulid::from_string(&anchor) else {
                    eprintln!("kage: set_label: invalid entry id '{anchor}', dropping");
                    return;
                };
                SessionEntry::Label(Label {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    text,
                    anchor: EntryId(parsed),
                })
            }
        };
        self.append(&entry);
    }

    /// Drain every queued plugin session op and write each as a
    /// session entry. Called at turn boundaries so a plugin's
    /// `append_entry` lands next to the surrounding messages.
    fn drain_plugin_session_ops(&mut self) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        let ops = runtime.take_pending_session_ops();
        for op in ops {
            self.apply_plugin_op(op);
        }
    }

    /// Persist a user message to the session file.
    ///
    /// The agent loop never emits user messages as events, so the host is
    /// responsible for recording them before calling `run`.
    pub fn record_user_message(&mut self, message: &Message) {
        self.append(&SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: message.clone(),
            usage: None,
        }));
    }

    /// Tear the recorder down and return its parts. Used by tests; future
    /// callers may also use it to append closing entries before drop.
    #[cfg(test)]
    pub fn into_parts(self) -> (H, SessionWriter) {
        (self.inner, self.writer)
    }

    fn append(&mut self, entry: &SessionEntry) {
        if let Err(err) = self.writer.append(entry) {
            eprintln!("kage: session write failed: {err}");
        }
    }
}

impl<H: Hooks> Hooks for SessionRecordingHooks<H> {
    fn before_tool_call(
        &mut self,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<kage_core::ToolOutput> {
        self.inner.before_tool_call(name, input)
    }

    fn after_tool_call(
        &mut self,
        name: &str,
        output: kage_core::ToolOutput,
    ) -> kage_core::ToolOutput {
        self.inner.after_tool_call(name, output)
    }

    fn on_event(&mut self, event: &LoopEvent) {
        match event {
            LoopEvent::MessageStart { id } => self.pending.start(*id),
            LoopEvent::TextDelta { delta, .. } => self.pending.text.push_str(delta),
            LoopEvent::ThinkingDelta { delta, .. } => self.pending.thinking.push_str(delta),
            LoopEvent::ToolCallStart {
                id,
                name,
                input_partial,
            } => {
                self.pending
                    .tool_calls
                    .push((id.clone(), name.clone(), input_partial.clone()));
            }
            LoopEvent::ToolCallEnd { id, output } => {
                let msg = Message {
                    role: Role::ToolResult,
                    content: vec![Content::ToolResultBlock {
                        call_id: id.clone(),
                        output: output.text.clone(),
                        is_error: output.is_error,
                    }],
                    id: MessageId::new(),
                    parent: None,
                    ts: Utc::now(),
                };
                self.append(&SessionEntry::Message(MessageEntry {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    message: msg,
                    usage: None,
                }));
            }
            LoopEvent::MessageEnd { usage, .. } => {
                if let Some(msg) = self.pending.finalize() {
                    self.append(&SessionEntry::Message(MessageEntry {
                        id: EntryId::new(),
                        ts: Utc::now(),
                        message: msg,
                        usage: Some(*usage),
                    }));
                }
            }
            LoopEvent::Compaction {
                kept,
                summarized,
                summary,
            } => {
                self.append(&SessionEntry::Compaction(Compaction {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    kept: *kept,
                    summarized: *summarized,
                    summary: summary.clone(),
                }));
            }
            // `ToolCallArgsDelta` is a transient UI hint; the
            // authoritative call is recorded from `ToolCallStart`.
            // `ProviderRetry` is a transient UI signal, never part of
            // the transcript.
            LoopEvent::ToolCallArgsDelta { .. }
            | LoopEvent::ToolUpdate { .. }
            | LoopEvent::ProviderRetry { .. }
            | LoopEvent::Error { .. } => {}
        }
        self.inner.on_event(event);
    }

    fn transform_context(&mut self, messages: &mut Vec<Message>) -> Result<(), String> {
        self.inner.transform_context(messages)
    }

    fn transform_provider_request(
        &mut self,
        req: &mut kage_loop::StreamRequest,
    ) -> Result<(), String> {
        self.inner.transform_provider_request(req)
    }

    fn on_turn_start(&mut self, index: u32) {
        self.inner.on_turn_start(index);
    }

    fn on_turn_end(&mut self, index: u32, had_tool_calls: bool) {
        // The outer `PluginEventHooks::on_turn_end` dispatched the
        // `turn_end` Lua event before delegating to us, so any
        // `kage.session.append_entry` / `kage.session.set_label`
        // call from a plugin handler is already in the runtime queue
        // by the time we run.
        self.drain_plugin_session_ops();
        self.inner.on_turn_end(index, had_tool_calls);
    }

    fn should_stop_after_turn(&mut self, summary: &kage_loop::TurnSummary) -> bool {
        self.inner.should_stop_after_turn(summary)
    }

    fn get_steering(&mut self) -> Option<String> {
        self.inner.get_steering()
    }

    fn get_followup(&mut self) -> Option<String> {
        self.inner.get_followup()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kage_core::{TokenUsage, ToolOutput};
    use kage_loop::NoopHooks;
    use kage_session::{FORMAT_VERSION, Header, SessionId, SessionReader};

    use super::*;

    fn temp_session() -> (tempfile::TempDir, SessionWriter, Header) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/tmp"),
            model: "mock:m".into(),
            system_prompt: "be helpful".into(),
            parent_session: None,
            parent_entry: None,
        };
        let writer = SessionWriter::create(&path, header.clone()).unwrap();
        (dir, writer, header)
    }

    #[test]
    fn records_user_message() {
        let (_dir, writer, _) = temp_session();
        let path = writer.path().to_path_buf();
        let mut hooks = SessionRecordingHooks::new(NoopHooks, writer);
        hooks.record_user_message(&Message::new(
            Role::User,
            vec![Content::Text { text: "hi".into() }],
            None,
        ));
        drop(hooks);

        let entries: Vec<_> = SessionReader::iter(&path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(entries.len(), 2); // header + user message
        assert!(matches!(entries[0], SessionEntry::Header(_)));
        match &entries[1] {
            SessionEntry::Message(m) => assert_eq!(m.message.role, Role::User),
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn reassembles_and_persists_assistant_message_with_tool_call() {
        let (_dir, writer, _) = temp_session();
        let path = writer.path().to_path_buf();
        let mut hooks = SessionRecordingHooks::new(NoopHooks, writer);

        let msg_id = MessageId::new();
        let call_id = ToolCallId::new("call_1");
        hooks.on_event(&LoopEvent::MessageStart { id: msg_id });
        hooks.on_event(&LoopEvent::ThinkingDelta {
            id: msg_id,
            delta: "let me ".into(),
        });
        hooks.on_event(&LoopEvent::ThinkingDelta {
            id: msg_id,
            delta: "think".into(),
        });
        hooks.on_event(&LoopEvent::TextDelta {
            id: msg_id,
            delta: "calling tool".into(),
        });
        hooks.on_event(&LoopEvent::ToolCallStart {
            id: call_id.clone(),
            name: "echo".into(),
            input_partial: serde_json::json!({"x": 1}),
        });
        hooks.on_event(&LoopEvent::MessageEnd {
            id: msg_id,
            usage: TokenUsage::default(),
        });
        drop(hooks);

        let entries: Vec<_> = SessionReader::iter(&path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // header + assistant message
        assert_eq!(entries.len(), 2);
        let SessionEntry::Message(m) = &entries[1] else {
            panic!("expected assistant message");
        };
        assert_eq!(m.message.role, Role::Assistant);
        assert_eq!(m.message.id, msg_id);
        assert_eq!(m.message.content.len(), 3);
        assert!(
            matches!(&m.message.content[0], Content::Thinking { text } if text == "let me think")
        );
        assert!(matches!(&m.message.content[1], Content::Text { text } if text == "calling tool"));
        assert!(
            matches!(&m.message.content[2], Content::ToolCall { id, name, .. } if id == &call_id && name == "echo")
        );
    }

    #[test]
    fn tool_call_end_writes_tool_result_message() {
        let (_dir, writer, _) = temp_session();
        let path = writer.path().to_path_buf();
        let mut hooks = SessionRecordingHooks::new(NoopHooks, writer);
        let call_id = ToolCallId::new("call_42");
        hooks.on_event(&LoopEvent::ToolCallEnd {
            id: call_id.clone(),
            output: ToolOutput {
                is_error: false,
                text: "OK".into(),
                structured: None,
                terminate: false,
            },
        });
        drop(hooks);

        let entries: Vec<_> = SessionReader::iter(&path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(entries.len(), 2);
        let SessionEntry::Message(m) = &entries[1] else {
            panic!("expected tool result");
        };
        assert_eq!(m.message.role, Role::ToolResult);
        match &m.message.content[0] {
            Content::ToolResultBlock {
                call_id: cid,
                output,
                is_error,
            } => {
                assert_eq!(cid, &call_id);
                assert_eq!(output, "OK");
                assert!(!is_error);
            }
            other => panic!("expected ToolResultBlock, got {other:?}"),
        }
    }

    #[test]
    fn compaction_event_writes_compaction_entry() {
        let (_dir, writer, _) = temp_session();
        let path = writer.path().to_path_buf();
        let mut hooks = SessionRecordingHooks::new(NoopHooks, writer);
        hooks.on_event(&LoopEvent::Compaction {
            kept: 4,
            summarized: 16,
            summary: "[summary of 16 earlier turns]\nthey did stuff".into(),
        });
        drop(hooks);

        let entries: Vec<_> = SessionReader::iter(&path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let SessionEntry::Compaction(c) = &entries[1] else {
            panic!("expected compaction");
        };
        assert_eq!(c.kept, 4);
        assert_eq!(c.summarized, 16);
        assert!(c.summary.contains("they did stuff"));
    }

    #[test]
    fn delegates_to_inner_hooks() {
        #[derive(Default)]
        struct CountingInner {
            before: u32,
            after: u32,
            on_event: u32,
            steering: u32,
            followup: u32,
        }
        impl Hooks for CountingInner {
            fn before_tool_call(
                &mut self,
                _name: &str,
                _input: &serde_json::Value,
            ) -> Option<ToolOutput> {
                self.before += 1;
                None
            }
            fn after_tool_call(&mut self, _name: &str, output: ToolOutput) -> ToolOutput {
                self.after += 1;
                output
            }
            fn on_event(&mut self, _event: &LoopEvent) {
                self.on_event += 1;
            }
            fn get_steering(&mut self) -> Option<String> {
                self.steering += 1;
                None
            }
            fn get_followup(&mut self) -> Option<String> {
                self.followup += 1;
                None
            }
        }
        let (_dir, writer, _) = temp_session();
        let _path = writer.path().to_path_buf();
        let mut hooks = SessionRecordingHooks::new(CountingInner::default(), writer);
        let _ = hooks.before_tool_call("t", &serde_json::Value::Null);
        let _ = hooks.after_tool_call(
            "t",
            ToolOutput {
                is_error: false,
                text: String::new(),
                structured: None,
                terminate: false,
            },
        );
        hooks.on_event(&LoopEvent::Error {
            kind: kage_core::LoopError::Cancelled,
        });
        let _ = hooks.get_steering();
        let _ = hooks.get_followup();
        let (inner, _w) = hooks.into_parts();
        assert_eq!(inner.before, 1);
        assert_eq!(inner.after, 1);
        assert_eq!(inner.on_event, 1);
        assert_eq!(inner.steering, 1);
        assert_eq!(inner.followup, 1);
    }

    #[test]
    fn turn_end_drains_plugin_session_ops_and_writes_them() {
        let (_dir, writer, _) = temp_session();
        let path = writer.path().to_path_buf();
        let runtime = Arc::new(PluginRuntime::new().unwrap());
        runtime
            .eval("kage.session.append_entry('plugin:tps', { rate = 12.5 })")
            .unwrap();
        let anchor = EntryId::new();
        runtime
            .eval(&format!("kage.session.set_label('{anchor}', 'milestone')"))
            .unwrap();

        let mut hooks =
            SessionRecordingHooks::new(NoopHooks, writer).with_plugin_runtime(Arc::clone(&runtime));
        hooks.on_turn_end(0, false);
        drop(hooks);

        let entries: Vec<_> = SessionReader::iter(&path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(entries.len(), 3); // header + custom + label
        assert!(matches!(entries[0], SessionEntry::Header(_)));
        match &entries[1] {
            SessionEntry::Custom(c) => {
                assert_eq!(c.kind, "plugin:tps");
                assert_eq!(c.data["rate"], 12.5);
            }
            other => panic!("expected custom entry, got {other:?}"),
        }
        match &entries[2] {
            SessionEntry::Label(l) => {
                assert_eq!(l.text, "milestone");
                assert_eq!(l.anchor, anchor);
            }
            other => panic!("expected label entry, got {other:?}"),
        }
        assert!(runtime.take_pending_session_ops().is_empty());
    }

    #[test]
    fn set_label_with_invalid_anchor_drops_silently() {
        let (_dir, writer, _) = temp_session();
        let path = writer.path().to_path_buf();
        let runtime = Arc::new(PluginRuntime::new().unwrap());
        runtime
            .eval("kage.session.set_label('not-a-ulid', 'oops')")
            .unwrap();
        let mut hooks =
            SessionRecordingHooks::new(NoopHooks, writer).with_plugin_runtime(Arc::clone(&runtime));
        hooks.on_turn_end(0, false);
        drop(hooks);

        // Only the header should be on disk.
        let entries: Vec<_> = SessionReader::iter(&path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], SessionEntry::Header(_)));
    }
}
