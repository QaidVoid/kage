use super::*;

const REJECTED: TokenUsage = TokenUsage {
    input: 30_000,
    output: 5_000,
    cache_read: 40,
    cache_write: 20,
};

fn invisible_turn(
    stop_reason: StopReason,
) -> Vec<Result<ProviderEvent, kage_provider::ProviderError>> {
    vec![
        Ok(ProviderEvent::ThinkingDelta {
            delta: "Let me read a file </parameter></invoke></calls>".into(),
        }),
        Ok(ProviderEvent::MessageEnd {
            usage: REJECTED,
            stop_reason,
        }),
    ]
}

fn context() -> AgentContext {
    let mut cx = AgentContext::new("mock:m", "")
        .with_context_window(40_000)
        .with_max_output_tokens(32_000);
    cx.history = vec![
        Arc::new(user_msg("initial")),
        Arc::new(assistant_msg(vec![Content::Text {
            text: "previous reply".into(),
        }])),
        Arc::new(user_msg("next")),
    ];
    cx.budget.current_context = 1_000;
    cx
}

#[test]
fn reasoning_only_attempt_retries_then_dispatches_only_structured_calls() {
    let mock = MockProvider::sequence(vec![
        invisible_turn(StopReason::ToolUse),
        tool_call_turn(&["counting"]),
        good_turn(),
    ]);
    let tool = Arc::new(CountingTool::default());
    let tools = ToolRegistry::new().with(tool.clone());
    let mut cx = context();
    let original = cx.history.clone();
    let mut events = Vec::new();
    run(
        &mock,
        &tools,
        &mut cx,
        LoopConfig {
            max_provider_retries: 1,
            compaction_threshold: 0.0,
            ..LoopConfig::default()
        },
        &mut NoopHooks,
        &CancelFlag::new(),
        |event| events.push(event),
    )
    .unwrap();
    assert_eq!(tool.calls.lock().unwrap().len(), 1);
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].messages, original);
    assert_eq!(requests[0].messages, requests[1].messages);
    assert_eq!(requests[0].max_output_tokens, requests[1].max_output_tokens);
    assert_eq!(cx.history.len(), original.len() + 3);
    assert_eq!(&cx.history[..original.len()], original.as_slice());
    assert!(!cx.history.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, Content::Thinking { .. }))
    }));
    assert_eq!(cx.budget.used_input, REJECTED.input);
    assert_eq!(cx.budget.used_output, REJECTED.output);
    assert_eq!(cx.budget.used_cache_read, REJECTED.cache_read);
    assert_eq!(cx.budget.used_cache_write, REJECTED.cache_write);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, LoopEvent::MessageEnd { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, LoopEvent::TurnEnded { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, LoopEvent::RejectedUsage { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, LoopEvent::ProviderRetry { .. }))
            .count(),
        1
    );
}

#[test]
fn rejected_attempts_fail_visibly_and_charge_usage_without_changing_context() {
    for (stop_reason, retries, attempts) in [
        (StopReason::EndTurn, 0, 1),
        (StopReason::ToolUse, 1, 2),
        (StopReason::MaxTokens, 1, 1),
        (StopReason::StopSequence, 1, 1),
        (StopReason::Other, 1, 1),
    ] {
        let mock = MockProvider::replaying(invisible_turn(stop_reason));
        let mut cx = context();
        let original = cx.history.clone();
        let mut events = Vec::new();
        let outcome = run(
            &mock,
            &ToolRegistry::new(),
            &mut cx,
            LoopConfig {
                max_provider_retries: retries,
                compaction_threshold: 0.0,
                ..LoopConfig::default()
            },
            &mut NoopHooks,
            &CancelFlag::new(),
            |event| events.push(event),
        );
        assert!(
            matches!(outcome, Err(LoopError::Provider { .. })),
            "{stop_reason:?}: {outcome:?}"
        );
        assert_eq!(mock.call_count(), attempts);
        assert_eq!(cx.history, original);
        assert_eq!(cx.budget.current_context, 1_000);
        let attempts = u64::try_from(attempts).unwrap();
        assert_eq!(cx.budget.used_input, REJECTED.input * attempts);
        assert_eq!(cx.budget.used_output, REJECTED.output * attempts);
        assert_eq!(cx.budget.used_cache_read, REJECTED.cache_read * attempts);
        assert_eq!(cx.budget.used_cache_write, REJECTED.cache_write * attempts);
        assert!(!events.iter().any(|event| matches!(
            event,
            LoopEvent::MessageEnd { .. }
                | LoopEvent::MessageAppended { .. }
                | LoopEvent::TurnEnded { .. }
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LoopEvent::Error { .. }))
        );
    }
}

#[test]
fn cancellation_after_rejected_usage_keeps_the_charge_without_retrying() {
    let mock = MockProvider::replaying(invisible_turn(StopReason::EndTurn));
    let mut cx = context();
    let original = cx.history.clone();
    let cancel = CancelFlag::new();
    let mut events = Vec::new();
    let outcome = run(
        &mock,
        &ToolRegistry::new(),
        &mut cx,
        LoopConfig::default(),
        &mut NoopHooks,
        &cancel,
        |event| {
            if matches!(event, LoopEvent::RejectedUsage { .. }) {
                cancel.cancel();
            }
            events.push(event);
        },
    );
    assert_eq!(outcome, Err(LoopError::Cancelled));
    assert_eq!(mock.call_count(), 1);
    assert_eq!(cx.history, original);
    assert_eq!(cx.budget.used_input, REJECTED.input);
    assert_eq!(cx.budget.used_output, REJECTED.output);
    assert_eq!(cx.budget.current_context, 1_000);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LoopEvent::ProviderRetry { .. }))
    );
}

#[test]
fn empty_end_turn_after_tool_results_is_retried_instead_of_completed() {
    let mock = MockProvider::sequence(vec![
        tool_call_turn(&["counting"]),
        vec![Ok(ProviderEvent::MessageEnd {
            usage: REJECTED,
            stop_reason: StopReason::EndTurn,
        })],
        good_turn(),
    ]);
    let tool = Arc::new(CountingTool::default());
    let mut cx = context();
    run(
        &mock,
        &ToolRegistry::new().with(tool.clone()),
        &mut cx,
        LoopConfig {
            max_provider_retries: 1,
            compaction_threshold: 0.0,
            ..LoopConfig::default()
        },
        &mut NoopHooks,
        &CancelFlag::new(),
        |_| {},
    )
    .unwrap();
    assert_eq!(mock.call_count(), 3);
    assert_eq!(tool.calls.lock().unwrap().len(), 1);
    assert_eq!(
        cx.history.last().unwrap().content,
        [Content::Text { text: "hi".into() }]
    );
}
