//! Translate a provider's [`ProviderEvent`] stream into a finished
//! [`Message`] plus a list of tool calls awaiting dispatch.
//!
//! Provider events are narrower than loop events: providers emit fine-grained
//! deltas and stop reasons, while the loop emits a higher-level alphabet
//! that includes executed tool outputs. This module owns the translation and
//! the message-assembly state machine.

use kage_core::{Content, LoopError, LoopEvent, Message, MessageId, Role, TokenUsage, ToolCallId};
use kage_provider::{EventStream, ProviderEvent};

use crate::Hooks;
use crate::run::emit_one;

/// Output of consuming one provider stream.
pub(crate) struct TurnResult {
    /// Fully assembled assistant message ready to append to history.
    pub(crate) message: Message,
    /// Tool calls the model emitted, in order. Awaiting dispatch.
    pub(crate) tool_calls: Vec<PendingToolCall>,
    /// Token usage reported for the turn.
    pub(crate) usage: TokenUsage,
}

/// One tool invocation requested by the model and not yet executed.
///
/// Fields are read by T4.4's tool dispatch; T4.3 only collects them.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct PendingToolCall {
    pub(crate) id: ToolCallId,
    pub(crate) name: String,
    pub(crate) input: serde_json::Value,
}

/// Drain `stream` into a finished message + tool-call manifest, emitting
/// [`LoopEvent`]s along the way.
///
/// The cancellation flag is polled between provider events. If it trips,
/// the iterator is dropped (which signals the underlying HTTP request to
/// abort via the provider implementation) and `Err(LoopError::Cancelled)`
/// is returned.
pub(crate) fn collect_turn<F: FnMut(LoopEvent)>(
    parent: Option<MessageId>,
    stream: EventStream,
    cancel: &kage_core::CancelFlag,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<TurnResult, LoopError> {
    let mut assembler = Assembler::new(parent);
    let mut started = false;

    for event in stream {
        if cancel.is_cancelled() {
            return Err(LoopError::Cancelled);
        }
        let event = event.map_err(|e| LoopError::Provider {
            message: e.to_string(),
        })?;

        if let Some(result) = handle_event(event, &mut assembler, &mut started, hooks, emit)? {
            return Ok(result);
        }
    }

    Err(LoopError::Provider {
        message: "stream ended without MessageEnd".into(),
    })
}

fn handle_event<F: FnMut(LoopEvent)>(
    event: ProviderEvent,
    assembler: &mut Assembler,
    started: &mut bool,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<Option<TurnResult>, LoopError> {
    let id = assembler.message_id;
    let mut ensure_started = |hooks: &mut dyn Hooks, emit: &mut F| {
        if !*started {
            *started = true;
            emit_one(hooks, emit, LoopEvent::MessageStart { id });
        }
    };

    match event {
        ProviderEvent::MessageStart => {
            ensure_started(hooks, emit);
        }
        ProviderEvent::TextDelta { delta } => {
            ensure_started(hooks, emit);
            assembler.push_text(&delta);
            emit_one(hooks, emit, LoopEvent::TextDelta { id, delta });
        }
        ProviderEvent::ThinkingDelta { delta } => {
            ensure_started(hooks, emit);
            assembler.push_thinking(&delta);
            emit_one(hooks, emit, LoopEvent::ThinkingDelta { id, delta });
        }
        ProviderEvent::ToolCallStart { id: call_id, name } => {
            ensure_started(hooks, emit);
            emit_one(
                hooks,
                emit,
                LoopEvent::ToolCallArgsDelta {
                    id: call_id.clone(),
                    name: name.clone(),
                    input_partial: serde_json::json!({}),
                },
            );
            assembler.begin_tool(call_id, name);
        }
        ProviderEvent::ToolCallArgsDelta {
            id: call_id,
            partial,
        } => {
            let acc = assembler.partial_args.entry(call_id.clone()).or_default();
            acc.push_str(&partial);
            // Tool-call arguments are a JSON object/array; the
            // accumulated buffer can only parse once a closing
            // delimiter has arrived. Gating on that avoids re-parsing
            // the whole growing buffer on every delta (quadratic over
            // the stream). Worst case for a value containing a literal
            // `}`/`]` is an occasional extra parse, never a missed
            // final one.
            let maybe_complete = matches!(acc.trim_end().as_bytes().last(), Some(b'}' | b']'));
            let parsed = maybe_complete
                .then(|| serde_json::from_str::<serde_json::Value>(acc.as_str()).ok())
                .flatten();
            if let Some(value) = parsed
                && let Some(name) = assembler.pending_tools.get(&call_id)
            {
                let ev = LoopEvent::ToolCallArgsDelta {
                    id: call_id.clone(),
                    name: name.clone(),
                    input_partial: value,
                };
                emit_one(hooks, emit, ev);
            }
        }
        ProviderEvent::ToolCallEnd { id: call_id, input } => {
            assembler.partial_args.remove(&call_id);
            let (call_id, name, input) = assembler.complete_tool(call_id, input)?;
            emit_one(
                hooks,
                emit,
                LoopEvent::ToolCallStart {
                    id: call_id.clone(),
                    name: name.clone(),
                    input_partial: input.clone(),
                },
            );
            assembler.tool_calls.push(PendingToolCall {
                id: call_id,
                name,
                input,
            });
        }
        ProviderEvent::MessageEnd { usage, .. } => {
            ensure_started(hooks, emit);
            emit_one(hooks, emit, LoopEvent::MessageEnd { id, usage });
            return Ok(Some(assembler.finish(usage)));
        }
    }
    Ok(None)
}

/// Mutable message-assembly state for one turn.
struct Assembler {
    message_id: MessageId,
    parent: Option<MessageId>,
    blocks: Vec<Content>,
    pending_tools: std::collections::HashMap<ToolCallId, String>,
    partial_args: std::collections::HashMap<ToolCallId, String>,
    tool_calls: Vec<PendingToolCall>,
}

impl Assembler {
    fn new(parent: Option<MessageId>) -> Self {
        Self {
            message_id: MessageId::new(),
            parent,
            blocks: Vec::new(),
            pending_tools: std::collections::HashMap::new(),
            partial_args: std::collections::HashMap::new(),
            tool_calls: Vec::new(),
        }
    }

    /// Append `delta` to the current text block, opening one if needed.
    fn push_text(&mut self, delta: &str) {
        if let Some(Content::Text { text }) = self.blocks.last_mut() {
            text.push_str(delta);
        } else {
            self.blocks.push(Content::Text {
                text: delta.to_owned(),
            });
        }
    }

    /// Append `delta` to the current thinking block, opening one if needed.
    fn push_thinking(&mut self, delta: &str) {
        if let Some(Content::Thinking { text }) = self.blocks.last_mut() {
            text.push_str(delta);
        } else {
            self.blocks.push(Content::Thinking {
                text: delta.to_owned(),
            });
        }
    }

    fn begin_tool(&mut self, id: ToolCallId, name: String) {
        self.pending_tools.insert(id, name);
    }

    fn complete_tool(
        &mut self,
        id: ToolCallId,
        input: serde_json::Value,
    ) -> Result<(ToolCallId, String, serde_json::Value), LoopError> {
        let name = self
            .pending_tools
            .remove(&id)
            .ok_or_else(|| LoopError::Provider {
                message: format!("tool_call_end without tool_call_start: {id}"),
            })?;
        self.blocks.push(Content::ToolCall {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        });
        Ok((id, name, input))
    }

    fn finish(&mut self, usage: TokenUsage) -> TurnResult {
        TurnResult {
            message: Message {
                role: Role::Assistant,
                content: std::mem::take(&mut self.blocks),
                id: self.message_id,
                parent: self.parent,
                ts: chrono::Utc::now(),
            },
            tool_calls: std::mem::take(&mut self.tool_calls),
            usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use kage_core::{CancelFlag, ToolCallId};
    use kage_provider::{Provider, StopReason, StreamRequest, testing::MockProvider};

    use super::*;
    use crate::NoopHooks;

    fn run_collect(events: Vec<Result<ProviderEvent, kage_provider::ProviderError>>) -> TurnResult {
        let mock = MockProvider::replaying(events);
        let cancel = CancelFlag::new();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let mut hooks = NoopHooks;
        let mut emitted = Vec::new();
        collect_turn(None, stream, &cancel, &mut hooks, &mut |ev| {
            emitted.push(ev);
        })
        .unwrap()
    }

    fn run_collect_with_emits(
        events: Vec<Result<ProviderEvent, kage_provider::ProviderError>>,
    ) -> (TurnResult, Vec<LoopEvent>) {
        let mock = MockProvider::replaying(events);
        let cancel = CancelFlag::new();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let mut hooks = NoopHooks;
        let mut emitted = Vec::new();
        let res = collect_turn(None, stream, &cancel, &mut hooks, &mut |ev| {
            emitted.push(ev);
        })
        .unwrap();
        (res, emitted)
    }

    fn end_event() -> ProviderEvent {
        ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input: 5,
                output: 10,
                cache_read: 0,
                cache_write: 0,
            },
        }
    }

    #[test]
    fn text_only_turn_assembles_one_text_block() {
        let result = run_collect(vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "hello ".into(),
            }),
            Ok(ProviderEvent::TextDelta {
                delta: "world".into(),
            }),
            Ok(end_event()),
        ]);
        assert_eq!(result.message.role, Role::Assistant);
        assert_eq!(result.message.content.len(), 1);
        assert!(matches!(
            &result.message.content[0],
            Content::Text { text } if text == "hello world"
        ));
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.usage.input, 5);
    }

    #[test]
    fn emits_message_lifecycle_events() {
        let (_, emitted) = run_collect_with_emits(vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta { delta: "hi".into() }),
            Ok(end_event()),
        ]);
        assert!(matches!(emitted[0], LoopEvent::MessageStart { .. }));
        assert!(matches!(
            &emitted[1],
            LoopEvent::TextDelta { delta, .. } if delta == "hi"
        ));
        assert!(matches!(
            emitted.last().unwrap(),
            LoopEvent::MessageEnd { .. }
        ));
    }

    #[test]
    fn synthesizes_message_start_when_provider_skips_it() {
        let (_, emitted) = run_collect_with_emits(vec![
            Ok(ProviderEvent::TextDelta { delta: "hi".into() }),
            Ok(end_event()),
        ]);
        assert!(matches!(emitted[0], LoopEvent::MessageStart { .. }));
    }

    #[test]
    fn thinking_block_separates_from_text() {
        let result = run_collect(vec![
            Ok(ProviderEvent::ThinkingDelta {
                delta: "ponder".into(),
            }),
            Ok(ProviderEvent::TextDelta {
                delta: "answer".into(),
            }),
            Ok(end_event()),
        ]);
        assert_eq!(result.message.content.len(), 2);
        assert!(matches!(
            &result.message.content[0],
            Content::Thinking { text } if text == "ponder"
        ));
        assert!(matches!(
            &result.message.content[1],
            Content::Text { text } if text == "answer"
        ));
    }

    #[test]
    fn tool_call_assembles_into_block_and_pending_list() {
        let id = ToolCallId::new("call_42");
        let result = run_collect(vec![
            Ok(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: "read".into(),
            }),
            Ok(ProviderEvent::ToolCallArgsDelta {
                id: id.clone(),
                partial: r#"{"path":"#.into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: id.clone(),
                input: serde_json::json!({"path": "src/lib.rs"}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ]);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "read");
        assert_eq!(result.tool_calls[0].input["path"], "src/lib.rs");
        assert_eq!(result.message.content.len(), 1);
        assert!(matches!(
            &result.message.content[0],
            Content::ToolCall { name, .. } if name == "read"
        ));
    }

    #[test]
    fn tool_call_emits_tool_call_start_with_full_input() {
        let id = ToolCallId::new("call_1");
        let (_, emitted) = run_collect_with_emits(vec![
            Ok(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: "ls".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: id.clone(),
                input: serde_json::json!({"path": "."}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ]);
        let start = emitted
            .iter()
            .find(|e| matches!(e, LoopEvent::ToolCallStart { .. }))
            .unwrap();
        match start {
            LoopEvent::ToolCallStart {
                name,
                input_partial,
                ..
            } => {
                assert_eq!(name, "ls");
                assert_eq!(input_partial["path"], ".");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn tool_call_streams_partial_args_before_final_start() {
        let id = ToolCallId::new("call_1");
        let (_, emitted) = run_collect_with_emits(vec![
            Ok(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: "ls".into(),
            }),
            Ok(ProviderEvent::ToolCallArgsDelta {
                id: id.clone(),
                partial: "{\"path\":".into(),
            }),
            Ok(ProviderEvent::ToolCallArgsDelta {
                id: id.clone(),
                partial: "\".\"}".into(),
            }),
            Ok(ProviderEvent::ToolCallEnd {
                id: id.clone(),
                input: serde_json::json!({"path": "."}),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ]);

        let first = emitted
            .iter()
            .find(|e| matches!(e, LoopEvent::ToolCallArgsDelta { .. }))
            .expect("an args-delta should fire at provider tool-call start");
        match first {
            LoopEvent::ToolCallArgsDelta {
                name,
                input_partial,
                ..
            } => {
                assert_eq!(name, "ls");
                assert_eq!(*input_partial, serde_json::json!({}));
            }
            _ => unreachable!(),
        }

        let populated = emitted
            .iter()
            .find_map(|e| match e {
                LoopEvent::ToolCallArgsDelta { input_partial, .. }
                    if input_partial.get("path").is_some() =>
                {
                    Some(input_partial.clone())
                }
                _ => None,
            })
            .expect("a populated args-delta should fire once fragments parse");
        assert_eq!(populated["path"], ".");

        let starts: Vec<&LoopEvent> = emitted
            .iter()
            .filter(|e| matches!(e, LoopEvent::ToolCallStart { .. }))
            .collect();
        assert_eq!(
            starts.len(),
            1,
            "the authoritative tool-call start must fire exactly once"
        );
        match starts[0] {
            LoopEvent::ToolCallStart {
                name,
                input_partial,
                ..
            } => {
                assert_eq!(name, "ls");
                assert_eq!(input_partial["path"], ".");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn tool_call_end_without_start_errors() {
        let mock = MockProvider::replaying(vec![
            Ok(ProviderEvent::ToolCallEnd {
                id: ToolCallId::new("orphan"),
                input: serde_json::json!({}),
            }),
            Ok(end_event()),
        ]);
        let cancel = CancelFlag::new();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let mut hooks = NoopHooks;
        let res = collect_turn(None, stream, &cancel, &mut hooks, &mut |_| {});
        assert!(matches!(res, Err(LoopError::Provider { .. })));
    }

    #[test]
    fn cancellation_aborts_mid_stream() {
        let mock = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta { delta: "a".into() }),
            Ok(end_event()),
        ]);
        let cancel = CancelFlag::new();
        cancel.cancel();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let mut hooks = NoopHooks;
        let res = collect_turn(None, stream, &cancel, &mut hooks, &mut |_| {});
        assert!(matches!(res, Err(LoopError::Cancelled)));
    }

    #[test]
    fn provider_error_translates_to_loop_provider_error() {
        let mock = MockProvider::replaying(vec![Err(kage_provider::ProviderError::Auth(
            "no key".into(),
        ))]);
        let cancel = CancelFlag::new();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let mut hooks = NoopHooks;
        let res = collect_turn(None, stream, &cancel, &mut hooks, &mut |_| {});
        assert!(matches!(res, Err(LoopError::Provider { .. })));
    }

    #[test]
    fn parent_id_is_carried_through() {
        let parent = MessageId::new();
        let mock = MockProvider::replaying(vec![Ok(end_event())]);
        let cancel = CancelFlag::new();
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let mut hooks = NoopHooks;
        let res = collect_turn(Some(parent), stream, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert_eq!(res.message.parent, Some(parent));
    }
}
