use kage_core::{CancelFlag, StopReason};

use super::*;

const USAGE: TokenUsage = TokenUsage {
    input: 137_000,
    output: 300,
    cache_read: 40,
    cache_write: 20,
};

fn collect(events: Vec<ProviderEvent>) -> (Result<TurnResult, TurnFailure>, Vec<LoopEvent>) {
    let mut emitted = Vec::new();
    let result = collect_turn(
        None,
        "mock:m",
        Box::new(events.into_iter().map(Ok)),
        &CancelFlag::new(),
        &mut |event| emitted.push(event),
    );
    (result, emitted)
}

#[test]
fn invisible_terminal_turns_fail_before_completion_and_report_usage() {
    for stop_reason in [
        StopReason::EndTurn,
        StopReason::ToolUse,
        StopReason::MaxTokens,
        StopReason::StopSequence,
        StopReason::Other,
    ] {
        for mut events in [
            vec![],
            vec![ProviderEvent::TextDelta {
                delta: " \n\t".into(),
            }],
            vec![ProviderEvent::ThinkingDelta {
                delta: "Let me read that file </parameter></invoke></calls>".into(),
            }],
            vec![ProviderEvent::ThinkingSignature {
                data: "opaque".into(),
            }],
            vec![ProviderEvent::RedactedThinking {
                data: "redacted".into(),
            }],
        ] {
            events.push(ProviderEvent::MessageEnd {
                usage: USAGE,
                stop_reason,
            });
            let (result, emitted) = collect(events);
            let Err(TurnFailure::Provider(error)) = result else {
                panic!("expected an unusable-turn failure for {stop_reason:?}, got {result:?}");
            };
            assert_eq!(
                error.is_transient(),
                matches!(stop_reason, StopReason::EndTurn | StopReason::ToolUse),
                "{stop_reason:?}: {error}"
            );
            assert!(
                error
                    .to_string()
                    .contains("no visible output or tool calls")
            );
            assert!(
                !emitted
                    .iter()
                    .any(|event| matches!(event, LoopEvent::MessageEnd { .. }))
            );
            assert_eq!(
                emitted
                    .iter()
                    .filter_map(|event| match event {
                        LoopEvent::RejectedUsage { usage } => Some(*usage),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                [USAGE]
            );
        }
    }
}

#[test]
fn visible_text_and_media_remain_valid_terminal_responses() {
    for stop_reason in [
        StopReason::EndTurn,
        StopReason::ToolUse,
        StopReason::MaxTokens,
        StopReason::StopSequence,
        StopReason::Other,
    ] {
        for event in [
            ProviderEvent::TextDelta {
                delta: "I cannot help with that.".into(),
            },
            ProviderEvent::Media {
                kind: Modality::Image,
                mime: "image/png".into(),
                data: "image".into(),
            },
            ProviderEvent::Media {
                kind: Modality::Audio,
                mime: "audio/wav".into(),
                data: "audio".into(),
            },
        ] {
            let (result, emitted) = collect(vec![
                event,
                ProviderEvent::MessageEnd {
                    usage: USAGE,
                    stop_reason,
                },
            ]);
            assert!(result.is_ok(), "{stop_reason:?}: {result:?}");
            assert!(
                emitted
                    .iter()
                    .any(|event| matches!(event, LoopEvent::MessageEnd { .. }))
            );
            assert!(
                !emitted
                    .iter()
                    .any(|event| matches!(event, LoopEvent::RejectedUsage { .. }))
            );
        }
    }
}

#[test]
fn cancellation_at_the_terminal_event_keeps_reported_usage() {
    let cancel = CancelFlag::new();
    let cancelling = cancel.clone();
    let stream = vec![Ok(ProviderEvent::MessageEnd {
        usage: USAGE,
        stop_reason: StopReason::EndTurn,
    })]
    .into_iter()
    .inspect(move |_| cancelling.cancel());
    let mut emitted = Vec::new();
    let result = collect_turn(None, "mock:m", Box::new(stream), &cancel, &mut |event| {
        emitted.push(event);
    });
    assert!(matches!(
        result,
        Err(TurnFailure::Fatal(LoopError::Cancelled))
    ));
    assert_eq!(emitted, [LoopEvent::RejectedUsage { usage: USAGE }]);
}

#[test]
fn completed_tool_calls_do_not_need_visible_text() {
    let id = ToolCallId::new("call");
    let (result, emitted) = collect(vec![
        ProviderEvent::ThinkingDelta {
            delta: "I should read a file.".into(),
        },
        ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: "read".into(),
        },
        ProviderEvent::ToolCallEnd {
            id,
            input: serde_json::json!({"path": "README.md"}),
        },
        ProviderEvent::MessageEnd {
            usage: USAGE,
            stop_reason: StopReason::ToolUse,
        },
    ]);
    assert_eq!(result.unwrap().tool_calls.len(), 1);
    assert!(
        !emitted
            .iter()
            .any(|event| matches!(event, LoopEvent::RejectedUsage { .. }))
    );
}
