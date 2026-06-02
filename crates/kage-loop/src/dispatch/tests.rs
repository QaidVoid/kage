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
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    )
    .unwrap();
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
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    )
    .unwrap();
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
        parent,
        &mut hooks,
        &mut |ev| emitted.push(ev),
    )
    .unwrap()
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
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .unwrap()
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
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .unwrap()
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
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .unwrap()
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
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .unwrap()
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
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .unwrap()
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
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .unwrap()
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
        parent,
        &mut hooks,
        &mut |_| {},
    )
    .unwrap()
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
fn cancellation_aborts_dispatch() {
    let tools = registry_with_echo();
    let cancel = CancelFlag::new();
    cancel.cancel();
    let parent = MessageId::new();
    let mut hooks = NoopHooks;

    let res = dispatch_tool_calls(
        vec![pending("echo", serde_json::json!({}))],
        &tools,
        std::path::Path::new("/tmp"),
        &cancel,
        parent,
        &mut hooks,
        &mut |_| {},
    );
    assert!(matches!(res, Err(LoopError::Cancelled)));
}
