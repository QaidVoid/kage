//! Tests for tool-call dispatch.

use std::sync::Arc;

use kage_core::{Risk, ToolCallId};
use kage_tools::Tool;

use super::*;
use crate::NoopHooks;

#[derive(Debug)]
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echo input back"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            is_error: false,
            text: input.to_string(),
            structured: None,
            terminate: false,
        })
    }
}

#[derive(Debug)]
struct ErrTool;

impl Tool for ErrTool {
    fn name(&self) -> &'static str {
        "err"
    }
    fn description(&self) -> &'static str {
        "always fail"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Other("planned failure".into()))
    }
}

#[derive(Debug)]
struct ProgressTool;

impl Tool for ProgressTool {
    fn name(&self) -> &'static str {
        "progress"
    }
    fn description(&self) -> &'static str {
        "emits two progress updates before returning"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        cx.update(ToolUpdate {
            content: "step 1".into(),
            structured: None,
        });
        cx.update(ToolUpdate {
            content: "step 2".into(),
            structured: Some(serde_json::json!({"phase": "done"})),
        });
        Ok(ToolOutput {
            is_error: false,
            text: "ok".into(),
            structured: None,
            terminate: false,
        })
    }
}

/// Emits one update, then waits until the test signals that the dispatcher
/// delivered an event. Fails if the signal only comes after it returns.
#[derive(Debug)]
struct WaitsForDeliveryTool {
    delivered: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl Tool for WaitsForDeliveryTool {
    fn name(&self) -> &'static str {
        "waits"
    }
    fn description(&self) -> &'static str {
        "blocks until its progress update was emitted"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        cx.update(ToolUpdate {
            content: "working".into(),
            structured: None,
        });
        let delivered = self
            .delivered
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok();
        Ok(ToolOutput {
            is_error: !delivered,
            text: if delivered { "live" } else { "buffered" }.into(),
            structured: None,
            terminate: false,
        })
    }
}

/// Replies with the call id its context carries.
#[derive(Debug)]
struct CallIdTool;

impl Tool for CallIdTool {
    fn name(&self) -> &'static str {
        "call_id"
    }
    fn description(&self) -> &'static str {
        "reply with the call id"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            is_error: false,
            text: cx.call_id().map(ToString::to_string).unwrap_or_default(),
            structured: None,
            terminate: false,
        })
    }
}

fn registry_with_echo() -> ToolRegistry {
    ToolRegistry::new()
        .with(Arc::new(EchoTool))
        .with(Arc::new(ErrTool))
        .with(Arc::new(ProgressTool))
}

fn pending(name: &str, input: serde_json::Value) -> PendingToolCall {
    PendingToolCall {
        id: ToolCallId::new(format!("call_{name}")),
        name: name.to_owned(),
        input,
    }
}

#[test]
fn tools_see_their_own_call_id() {
    let tools = ToolRegistry::new().with(Arc::new(CallIdTool));
    let calls = || {
        ["first", "second"].map(|id| PendingToolCall {
            id: ToolCallId::new(id),
            name: "call_id".to_owned(),
            input: serde_json::json!({}),
        })
    };
    let outputs = |outcome: DispatchOutcome| -> Vec<String> {
        outcome
            .results
            .iter()
            .filter_map(|m| match &m.content[0] {
                Content::ToolResultBlock { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect()
    };
    let cancel = CancelFlag::new();
    let workdir = std::path::Path::new("/tmp");

    let sequential = dispatch_tool_calls(
        calls().into(),
        &tools,
        workdir,
        &cancel,
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |_| {},
    );
    assert_eq!(outputs(sequential), ["first", "second"]);

    let parallel = dispatch_tool_calls_parallel(
        calls().into(),
        &tools,
        workdir,
        &cancel,
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |_| {},
    );
    assert_eq!(outputs(parallel), ["first", "second"]);
}

#[test]
fn tool_updates_are_emitted_before_tool_call_end() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls(
        vec![pending("progress", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(outcome.results.len(), 1);

    let updates: Vec<_> = emitted
        .iter()
        .filter_map(|e| match e {
            LoopEvent::ToolUpdate { update, .. } => Some(update.content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(updates, vec!["step 1".to_owned(), "step 2".to_owned()]);

    let update_idx = emitted
        .iter()
        .position(|e| matches!(e, LoopEvent::ToolUpdate { .. }))
        .unwrap();
    let end_idx = emitted
        .iter()
        .position(|e| matches!(e, LoopEvent::ToolCallEnd { .. }))
        .unwrap();
    assert!(
        update_idx < end_idx,
        "ToolUpdate must fire before ToolCallEnd"
    );
}

#[test]
fn tool_updates_are_emitted_while_the_tool_runs() {
    let (tx, rx) = std::sync::mpsc::channel();
    let tools = ToolRegistry::new().with(Arc::new(WaitsForDeliveryTool {
        delivered: std::sync::Mutex::new(rx),
    }));
    let cancel = CancelFlag::new();
    let mut hooks = NoopHooks;

    let outcome = dispatch_tool_calls(
        vec![pending("waits", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        MessageId::new(),
        &mut hooks,
        &mut |ev| {
            if matches!(ev, LoopEvent::ToolUpdate { .. }) {
                let _ = tx.send(());
            }
        },
    );

    assert!(matches!(
        outcome.results[0].content[0],
        Content::ToolResultBlock {
            is_error: false,
            ..
        }
    ));
}

#[test]
fn parallel_dispatch_emits_tool_updates_per_call() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls_parallel(
        vec![
            pending("progress", serde_json::json!({})),
            pending("progress", serde_json::json!({})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(outcome.results.len(), 2);

    let updates = emitted
        .iter()
        .filter(|e| matches!(e, LoopEvent::ToolUpdate { .. }))
        .count();
    assert_eq!(updates, 4, "two tools x two updates each");
}

#[test]
fn dispatches_in_input_order_and_appends_results() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let pendings = vec![
        pending("echo", serde_json::json!({"n": 1})),
        pending("echo", serde_json::json!({"n": 2})),
    ];
    let mut hooks = NoopHooks;
    let mut emitted = Vec::new();

    let results = dispatch_tool_calls(
        pendings,
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    )
    .results;
    assert_eq!(results.len(), 2);
    for result in &results {
        assert_eq!(result.role, Role::ToolResult);
        assert_eq!(result.parent, Some(parent));
    }
    // Order: result 1 corresponds to first pending.
    if let Content::ToolResultBlock { output, .. } = &results[0].content[0] {
        assert!(output.contains("\"n\":1"));
    }
    // Two ToolCallEnd events emitted.
    let ends = emitted
        .iter()
        .filter(|e| matches!(e, LoopEvent::ToolCallEnd { .. }))
        .count();
    assert_eq!(ends, 2);
}

#[test]
fn unknown_tool_yields_error_output_not_loop_failure() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let results = dispatch_tool_calls(
        vec![pending("does_not_exist", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    assert_eq!(results.len(), 1);
    match &results[0].content[0] {
        Content::ToolResultBlock {
            is_error, output, ..
        } => {
            assert!(*is_error);
            assert!(output.contains("does_not_exist"));
        }
        other => panic!("unexpected content: {other:?}"),
    }
}

#[test]
fn tool_error_converts_to_error_output() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let results = dispatch_tool_calls(
        vec![pending("err", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    assert_eq!(results.len(), 1);
    if let Content::ToolResultBlock {
        is_error, output, ..
    } = &results[0].content[0]
    {
        assert!(*is_error);
        assert!(output.contains("planned failure"));
    }
}

#[test]
fn before_tool_call_can_short_circuit_execution() {
    struct Allowlist;
    impl Hooks for Allowlist {
        fn before_tool_call(
            &mut self,
            _id: &kage_core::ToolCallId,
            name: &str,
            _input: &serde_json::Value,
        ) -> Option<ToolOutput> {
            if name == "err" {
                Some(ToolOutput {
                    is_error: true,
                    text: "blocked by host policy".into(),
                    structured: None,
                    terminate: false,
                })
            } else {
                None
            }
        }
    }

    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = Allowlist;

    let results = dispatch_tool_calls(
        vec![pending("err", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    assert_eq!(results.len(), 1);
    match &results[0].content[0] {
        Content::ToolResultBlock {
            output, is_error, ..
        } => {
            assert!(*is_error);
            assert_eq!(output, "blocked by host policy");
        }
        other => panic!("unexpected content: {other:?}"),
    }
}

#[test]
fn after_tool_call_can_rewrite_output() {
    struct Redact;
    impl Hooks for Redact {
        fn after_tool_call(&mut self, _name: &str, mut output: ToolOutput) -> ToolOutput {
            output.text = format!("[redacted] {}", output.text);
            output
        }
    }

    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = Redact;

    let results = dispatch_tool_calls(
        vec![pending("echo", serde_json::json!({"x": 1}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    match &results[0].content[0] {
        Content::ToolResultBlock { output, .. } => {
            assert!(output.starts_with("[redacted] "));
        }
        other => panic!("unexpected content: {other:?}"),
    }
}

#[derive(Debug)]
struct SleepTool {
    millis: u64,
}

impl Tool for SleepTool {
    fn name(&self) -> &'static str {
        "sleep"
    }
    fn description(&self) -> &'static str {
        "sleep for a fixed duration"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        std::thread::sleep(std::time::Duration::from_millis(self.millis));
        Ok(ToolOutput {
            is_error: false,
            text: format!("slept_{}", self.millis),
            structured: None,
            terminate: false,
        })
    }
}

#[test]
fn parallel_dispatch_preserves_input_order() {
    let tools = ToolRegistry::new()
        .with(Arc::new(SleepTool { millis: 50 }))
        .with(Arc::new(EchoTool));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let pendings = vec![
        pending("sleep", serde_json::json!({})),
        pending("echo", serde_json::json!({"x": 1})),
        pending("sleep", serde_json::json!({})),
    ];

    let results = dispatch_tool_calls_parallel(
        pendings,
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    assert_eq!(results.len(), 3);
    assert!(matches!(
        &results[0].content[0],
        Content::ToolResultBlock { output, .. } if output == "slept_50"
    ));
    assert!(matches!(
        &results[1].content[0],
        Content::ToolResultBlock { output, .. } if output.contains("\"x\":1")
    ));
    assert!(matches!(
        &results[2].content[0],
        Content::ToolResultBlock { output, .. } if output == "slept_50"
    ));
}

#[test]
fn parallel_dispatch_actually_runs_concurrently() {
    let tools = ToolRegistry::new().with(Arc::new(SleepTool { millis: 100 }));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let pendings: Vec<_> = (0..3)
        .map(|i| pending(&format!("call_{i}"), serde_json::json!({})))
        .map(|mut c| {
            c.name = "sleep".to_owned();
            c
        })
        .collect();

    let start = std::time::Instant::now();
    let results = dispatch_tool_calls_parallel(
        pendings,
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    let elapsed = start.elapsed();

    assert_eq!(results.len(), 3);
    // 3 tools sleeping 100ms each: parallel <= ~150ms; serial >= 300ms.
    assert!(
        elapsed.as_millis() < 250,
        "expected parallel execution under 250ms, took {}ms",
        elapsed.as_millis(),
    );
}

#[test]
fn parallel_dispatch_honors_before_tool_call_short_circuit() {
    struct BlockSecond {
        seen: u32,
    }
    impl Hooks for BlockSecond {
        fn before_tool_call(
            &mut self,
            _id: &kage_core::ToolCallId,
            _name: &str,
            _input: &serde_json::Value,
        ) -> Option<ToolOutput> {
            self.seen = self.seen.saturating_add(1);
            if self.seen == 2 {
                Some(ToolOutput {
                    is_error: true,
                    text: "blocked".into(),
                    structured: None,
                    terminate: false,
                })
            } else {
                None
            }
        }
    }

    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = BlockSecond { seen: 0 };

    let pendings = vec![
        pending("echo", serde_json::json!({"i": 0})),
        pending("echo", serde_json::json!({"i": 1})),
        pending("echo", serde_json::json!({"i": 2})),
    ];

    let results = dispatch_tool_calls_parallel(
        pendings,
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    assert_eq!(results.len(), 3);
    if let Content::ToolResultBlock {
        output, is_error, ..
    } = &results[1].content[0]
    {
        assert!(*is_error);
        assert_eq!(output, "blocked");
    }
}

#[test]
fn cancelled_batch_synthesizes_error_results() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    cancel.cancel();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls(
        vec![pending("echo", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(outcome.error, Some(LoopError::Cancelled));
    assert_eq!(outcome.results.len(), 1);
    match &outcome.results[0].content[0] {
        Content::ToolResultBlock {
            is_error,
            output,
            call_id,
        } => {
            assert!(*is_error);
            assert!(output.contains("cancelled"));
            assert_eq!(*call_id, ToolCallId::new("call_echo"));
        }
        other => panic!("unexpected content: {other:?}"),
    }
    let ends = emitted
        .iter()
        .filter(|e| matches!(e, LoopEvent::ToolCallEnd { .. }))
        .count();
    assert_eq!(ends, 1, "ToolCallEnd must still fire for the aborted call");
}

/// Tool that always reports cancellation, like a host interrupt mid-run.
#[derive(Debug)]
struct CancelTool;

impl Tool for CancelTool {
    fn name(&self) -> &'static str {
        "cancel"
    }
    fn description(&self) -> &'static str {
        "always reports cancellation"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Cancelled)
    }
}

#[test]
fn mid_batch_cancel_synthesizes_remaining_results() {
    let tools = ToolRegistry::new()
        .with(Arc::new(EchoTool))
        .with(Arc::new(CancelTool));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls(
        vec![
            pending("echo", serde_json::json!({"i": 0})),
            pending("cancel", serde_json::json!({})),
            pending("echo", serde_json::json!({"i": 2})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(outcome.error, Some(LoopError::Cancelled));
    assert_eq!(outcome.results.len(), 3);

    let as_block = |msg: &Message| match &msg.content[0] {
        Content::ToolResultBlock {
            call_id,
            output,
            is_error,
        } => (call_id.clone(), output.clone(), *is_error),
        other => panic!("unexpected content: {other:?}"),
    };
    let (id0, out0, err0) = as_block(&outcome.results[0]);
    assert!(!err0, "call before the cancel keeps its real output");
    assert!(out0.contains("\"i\":0"));
    let (_, out1, err1) = as_block(&outcome.results[1]);
    assert!(err1 && out1.contains("cancelled"));
    let (_, out2, err2) = as_block(&outcome.results[2]);
    assert!(err2 && out2.contains("cancelled"));
    assert_eq!(id0, ToolCallId::new("call_echo"));

    let ends = emitted
        .iter()
        .filter(|e| matches!(e, LoopEvent::ToolCallEnd { .. }))
        .count();
    assert_eq!(ends, 3, "every call in the batch gets a ToolCallEnd");
}

/// Tool that panics, standing in for a tool bug blowing up its thread.
#[derive(Debug)]
struct PanicTool;

impl Tool for PanicTool {
    fn name(&self) -> &'static str {
        "panic"
    }
    fn description(&self) -> &'static str {
        "always panics"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        panic!("boom");
    }
}

#[test]
fn parallel_batch_keeps_successful_outputs_when_one_panics() {
    let tools = ToolRegistry::new()
        .with(Arc::new(EchoTool))
        .with(Arc::new(PanicTool));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let outcome = dispatch_tool_calls_parallel(
        vec![
            pending("echo", serde_json::json!({"i": 0})),
            pending("panic", serde_json::json!({})),
            pending("echo", serde_json::json!({"i": 2})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    );
    assert!(matches!(outcome.error, Some(LoopError::Other { .. })));
    assert_eq!(outcome.results.len(), 3);
    let as_block = |msg: &Message| match &msg.content[0] {
        Content::ToolResultBlock {
            output, is_error, ..
        } => (output.clone(), *is_error),
        other => panic!("unexpected content: {other:?}"),
    };
    let (out0, err0) = as_block(&outcome.results[0]);
    assert!(
        !err0 && out0.contains("\"i\":0"),
        "panic must not discard sibling outputs"
    );
    let (_, err1) = as_block(&outcome.results[1]);
    assert!(err1);
    let (out2, err2) = as_block(&outcome.results[2]);
    assert!(!err2 && out2.contains("\"i\":2"));
}

#[test]
fn parallel_cancelled_call_synthesizes_and_keeps_others() {
    let tools = ToolRegistry::new()
        .with(Arc::new(EchoTool))
        .with(Arc::new(CancelTool));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let outcome = dispatch_tool_calls_parallel(
        vec![
            pending("echo", serde_json::json!({"i": 0})),
            pending("cancel", serde_json::json!({})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    );
    assert_eq!(outcome.error, Some(LoopError::Cancelled));
    assert_eq!(outcome.results.len(), 2);
    let as_block = |msg: &Message| match &msg.content[0] {
        Content::ToolResultBlock {
            output, is_error, ..
        } => (output.clone(), *is_error),
        other => panic!("unexpected content: {other:?}"),
    };
    let (out0, err0) = as_block(&outcome.results[0]);
    assert!(!err0 && out0.contains("\"i\":0"));
    let (out1, err1) = as_block(&outcome.results[1]);
    assert!(err1 && out1.contains("cancelled"));
}

/// Tool that reports whether its context is confined, standing in for
/// the path-confinement plumbing the dispatcher must propagate.
#[derive(Debug)]
struct ConfineProbe;

impl Tool for ConfineProbe {
    fn name(&self) -> &'static str {
        "confine_probe"
    }
    fn description(&self) -> &'static str {
        "reports context confinement"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> Risk {
        Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            is_error: false,
            text: cx.is_confined().to_string(),
            structured: None,
            terminate: false,
        })
    }
}

#[test]
fn dispatch_propagates_confine_flag_to_tool_context() {
    let tools = ToolRegistry::new().with(Arc::new(ConfineProbe));
    let cancel = CancelFlag::new();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let results = dispatch_tool_calls(
        vec![pending("confine_probe", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        false,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    match &results[0].content[0] {
        Content::ToolResultBlock { output, .. } => assert_eq!(output, "false"),
        other => panic!("unexpected content: {other:?}"),
    }

    let results = dispatch_tool_calls_parallel(
        vec![pending("confine_probe", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        true,
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .results;
    match &results[0].content[0] {
        Content::ToolResultBlock { output, .. } => assert_eq!(output, "true"),
        other => panic!("unexpected content: {other:?}"),
    }
}

/// Short-circuits every `err` call.
struct BlockErr;

impl Hooks for BlockErr {
    fn before_tool_call(
        &mut self,
        _id: &kage_core::ToolCallId,
        name: &str,
        _input: &serde_json::Value,
    ) -> Option<ToolOutput> {
        (name == "err").then(|| ToolOutput {
            is_error: true,
            text: "blocked".into(),
            structured: None,
            terminate: false,
        })
    }
}

/// The ids of `ToolExecutionStart` and `ToolCallEnd` events, tagged.
fn execution_trace(events: &[LoopEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::ToolExecutionStart { id } => Some(format!("start {id}")),
            LoopEvent::ToolCallEnd { id, .. } => Some(format!("end {id}")),
            _ => None,
        })
        .collect()
}

#[test]
fn execution_start_marks_each_call_right_before_it_runs() {
    let tools = registry_with_echo();
    let mut emitted = Vec::new();
    dispatch_tool_calls(
        vec![
            pending("echo", serde_json::json!({})),
            pending("err", serde_json::json!({})),
            pending("progress", serde_json::json!({})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut BlockErr,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(
        execution_trace(&emitted),
        [
            "start call_echo",
            "end call_echo",
            "end call_err",
            "start call_progress",
            "end call_progress",
        ]
    );
}

#[test]
fn parallel_execution_start_skips_short_circuited_calls() {
    let tools = registry_with_echo();
    let mut emitted = Vec::new();
    dispatch_tool_calls_parallel(
        vec![
            pending("echo", serde_json::json!({})),
            pending("err", serde_json::json!({})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut BlockErr,
        &mut |ev| emitted.push(ev),
    );
    assert_eq!(
        execution_trace(&emitted),
        ["end call_err", "start call_echo", "end call_echo"]
    );
}

#[test]
fn parallel_dispatch_ends_each_call_when_it_finishes() {
    let (tx, rx) = std::sync::mpsc::channel();
    let tools = ToolRegistry::new()
        .with(Arc::new(WaitsForDeliveryTool {
            delivered: std::sync::Mutex::new(rx),
        }))
        .with(Arc::new(EchoTool));
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls_parallel(
        vec![
            pending("waits", serde_json::json!({})),
            pending("echo", serde_json::json!({"i": 1})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut NoopHooks,
        &mut |ev| {
            if matches!(&ev, LoopEvent::ToolCallEnd { id, .. } if id.to_string() == "call_echo") {
                let _ = tx.send(());
            }
            emitted.push(ev);
        },
    );

    assert_eq!(
        execution_trace(&emitted),
        [
            "start call_waits",
            "start call_echo",
            "end call_echo",
            "end call_waits",
        ]
    );
    let blocks: Vec<_> = outcome
        .results
        .iter()
        .map(|m| match &m.content[0] {
            Content::ToolResultBlock {
                call_id,
                output,
                is_error,
            } => (call_id.to_string(), output.clone(), *is_error),
            other => panic!("unexpected content: {other:?}"),
        })
        .collect();
    assert_eq!(blocks[0], ("call_waits".into(), "live".into(), false));
    assert_eq!(blocks[1].0, "call_echo");
    assert!(!blocks[1].2 && blocks[1].1.contains("\"i\":1"));
    assert!(
        outcome.results[1].ts < outcome.results[0].ts,
        "each result is stamped when its call finishes"
    );
}

#[test]
fn parallel_panic_yields_an_error_for_its_own_call() {
    let tools = ToolRegistry::new()
        .with(Arc::new(EchoTool))
        .with(Arc::new(ErrTool))
        .with(Arc::new(PanicTool));
    let mut emitted = Vec::new();

    let outcome = dispatch_tool_calls_parallel(
        vec![
            pending("err", serde_json::json!({})),
            pending("panic", serde_json::json!({})),
            pending("echo", serde_json::json!({"i": 2})),
        ],
        &tools,
        std::path::Path::new("/tmp"),
        &CancelFlag::new(),
        false,
        MessageId::new(),
        &mut BlockErr,
        &mut |ev| emitted.push(ev),
    );

    assert!(matches!(outcome.error, Some(LoopError::Other { .. })));
    let blocks: Vec<_> = outcome
        .results
        .iter()
        .map(|m| match &m.content[0] {
            Content::ToolResultBlock {
                call_id,
                output,
                is_error,
            } => (call_id.to_string(), output.clone(), *is_error),
            other => panic!("unexpected content: {other:?}"),
        })
        .collect();
    assert_eq!(blocks[0], ("call_err".into(), "blocked".into(), true));
    assert_eq!(blocks[1].0, "call_panic");
    assert!(blocks[1].2 && blocks[1].1.contains("panicked"));
    assert_eq!(blocks[2].0, "call_echo");
    assert!(!blocks[2].2 && blocks[2].1.contains("\"i\":2"));

    let panic_end = emitted.iter().find_map(|e| match e {
        LoopEvent::ToolCallEnd { id, output } if id.to_string() == "call_panic" => Some(output),
        _ => None,
    });
    assert!(panic_end.is_some_and(|output| output.is_error));
}
