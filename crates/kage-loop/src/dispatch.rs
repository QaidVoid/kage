//! Sequential dispatch of tool calls produced by one assistant turn.
//!
//! Walks the [`PendingToolCall`] list from [`crate::stream::collect_turn`],
//! consults [`Hooks::before_tool_call`] for short-circuit, executes the tool
//! through the registry, runs the result through [`Hooks::after_tool_call`],
//! emits a [`LoopEvent::ToolCallEnd`], and produces one tool-result message
//! per call to append to history.

use std::path::Path;

use kage_core::{CancelFlag, Content, LoopError, LoopEvent, Message, MessageId, Role, ToolOutput};
use kage_tools::{ToolContext, ToolError, ToolRegistry};

use crate::Hooks;
use crate::run::emit_one;
use crate::stream::PendingToolCall;

/// Outcome of [`Hooks::before_tool_call`] for one entry: either a
/// short-circuit output the host produced, or run the real tool.
enum Slot {
    Short(ToolOutput),
    Run,
}

/// Result of one batch of tool dispatch.
///
/// `results` is the message list to append to history; `all_terminate`
/// is `true` when every tool in the batch returned `ToolOutput::terminate`
/// so the loop can stop cleanly after appending the results.
pub(crate) struct DispatchOutcome {
    pub results: Vec<Message>,
    pub all_terminate: bool,
}

/// Dispatch every pending tool call sequentially.
///
/// Returns one tool-result [`Message`] per call, in input order. The caller
/// appends them to history before continuing the inner loop.
///
/// Cancellation: polled before every call. On cancel, stops with
/// [`LoopError::Cancelled`]; previously-completed results in this batch are
/// discarded along with the error (the loop terminates regardless).
pub(crate) fn dispatch_tool_calls<F: FnMut(LoopEvent)>(
    pending: Vec<PendingToolCall>,
    tools: &ToolRegistry,
    workdir: &Path,
    cancel: &CancelFlag,
    parent: MessageId,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<DispatchOutcome, LoopError> {
    let mut results = Vec::with_capacity(pending.len());
    let mut all_terminate = !pending.is_empty();
    for call in pending {
        if cancel.is_cancelled() {
            return Err(LoopError::Cancelled);
        }

        let pre = hooks.before_tool_call(&call.name, &call.input);
        let raw_output = match pre {
            Some(out) => out,
            None => execute(tools, &call, workdir, cancel)?,
        };
        let output = hooks.after_tool_call(&call.name, raw_output);
        all_terminate &= output.terminate;

        emit_one(
            hooks,
            emit,
            LoopEvent::ToolCallEnd {
                id: call.id.clone(),
                output: output.clone(),
            },
        );

        results.push(Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: call.id,
                output: output.text,
                is_error: output.is_error,
            }],
            Some(parent),
        ));
    }
    Ok(DispatchOutcome {
        results,
        all_terminate,
    })
}

/// Dispatch tool calls in parallel via [`std::thread::scope`].
///
/// Hooks (`before_tool_call`, `after_tool_call`) stay on the calling thread;
/// only the tool's `execute` runs concurrently. Result message order is
/// preserved to match the input order, regardless of completion order.
///
/// Calls that get short-circuited by `before_tool_call` skip thread
/// dispatch entirely. The remaining calls all run on dedicated threads
/// inside one [`std::thread::scope`] block; the function blocks until the
/// last one completes.
pub(crate) fn dispatch_tool_calls_parallel<F: FnMut(LoopEvent)>(
    pending: Vec<PendingToolCall>,
    tools: &ToolRegistry,
    workdir: &Path,
    cancel: &CancelFlag,
    parent: MessageId,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<DispatchOutcome, LoopError> {
    if cancel.is_cancelled() {
        return Err(LoopError::Cancelled);
    }

    // Resolve hook short-circuits up front, single-threaded.
    let mut slots: Vec<Slot> = Vec::with_capacity(pending.len());
    for call in &pending {
        match hooks.before_tool_call(&call.name, &call.input) {
            Some(out) => slots.push(Slot::Short(out)),
            None => slots.push(Slot::Run),
        }
    }

    let raw_outputs: Vec<Result<ToolOutput, LoopError>> = std::thread::scope(|scope| {
        let mut handles: Vec<Option<std::thread::ScopedJoinHandle<'_, _>>> =
            Vec::with_capacity(pending.len());
        for (call, slot) in pending.iter().zip(&slots) {
            match slot {
                Slot::Short(_) => handles.push(None),
                Slot::Run => {
                    let call = call.clone();
                    let handle = scope.spawn(move || execute(tools, &call, workdir, cancel));
                    handles.push(Some(handle));
                }
            }
        }
        handles
            .into_iter()
            .zip(slots)
            .map(|(handle, slot)| match slot {
                Slot::Short(out) => Ok(out),
                Slot::Run => match handle.expect("Run slot spawned a handle").join() {
                    Ok(res) => res,
                    Err(_) => Err(LoopError::Other {
                        message: "tool thread panicked".into(),
                    }),
                },
            })
            .collect()
    });

    let mut results = Vec::with_capacity(pending.len());
    let mut all_terminate = !pending.is_empty();
    for (call, raw) in pending.into_iter().zip(raw_outputs) {
        let raw = raw?;
        let output = hooks.after_tool_call(&call.name, raw);
        all_terminate &= output.terminate;
        emit_one(
            hooks,
            emit,
            LoopEvent::ToolCallEnd {
                id: call.id.clone(),
                output: output.clone(),
            },
        );
        results.push(Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: call.id,
                output: output.text,
                is_error: output.is_error,
            }],
            Some(parent),
        ));
    }
    Ok(DispatchOutcome {
        results,
        all_terminate,
    })
}

/// Execute a single tool through the registry, mapping errors to outputs.
///
/// Cancellation surfaces as `Err(LoopError::Cancelled)`; every other tool
/// error is converted to `Ok(ToolOutput { is_error: true, ... })` so the
/// model can observe the failure and adapt rather than terminating the run.
fn execute(
    tools: &ToolRegistry,
    call: &PendingToolCall,
    workdir: &Path,
    cancel: &CancelFlag,
) -> Result<ToolOutput, LoopError> {
    let Some(tool) = tools.get(&call.name) else {
        return Ok(ToolOutput {
            is_error: true,
            text: format!("tool '{}' is not registered", call.name),
            structured: None,
            terminate: false,
        });
    };

    let cx = ToolContext::new(workdir, cancel);
    match tool.execute(call.input.clone(), &cx) {
        Ok(out) => Ok(out),
        Err(ToolError::Cancelled) => Err(LoopError::Cancelled),
        Err(err) => Ok(ToolOutput {
            is_error: true,
            text: err.to_string(),
            structured: None,
            terminate: false,
        }),
    }
}

#[cfg(test)]
mod tests {
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

    fn registry_with_echo() -> ToolRegistry {
        ToolRegistry::new()
            .with(Arc::new(EchoTool))
            .with(Arc::new(ErrTool))
    }

    fn pending(name: &str, input: serde_json::Value) -> PendingToolCall {
        PendingToolCall {
            id: ToolCallId::new(format!("call_{name}")),
            name: name.to_owned(),
            input,
        }
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
}
