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
) -> Result<Vec<Message>, LoopError> {
    let mut results = Vec::with_capacity(pending.len());
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
    Ok(results)
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
        .unwrap();

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
        .unwrap();

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
        .unwrap();

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
        .unwrap();

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
        .unwrap();

        match &results[0].content[0] {
            Content::ToolResultBlock { output, .. } => {
                assert!(output.starts_with("[redacted] "));
            }
            other => panic!("unexpected content: {other:?}"),
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
