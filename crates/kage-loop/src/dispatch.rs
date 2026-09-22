//! Sequential dispatch of tool calls produced by one assistant turn.
//!
//! Walks the [`PendingToolCall`] list from [`crate::stream::collect_turn`],
//! consults [`Hooks::before_tool_call`] for short-circuit, executes the tool
//! through the registry, runs the result through [`Hooks::after_tool_call`],
//! emits a [`LoopEvent::ToolCallEnd`], and produces one tool-result message
//! per call to append to history.
//!
//! Dispatch is infallible: a call that never produced an output (cancel, or
//! a tool failure the loop cannot recover from) gets a synthesized
//! `is_error` result. The assistant message's `tool_use` blocks are
//! therefore always answered, in memory and in the persisted session, so a
//! resumed run never sends a provider a dangling `tool_use`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use kage_core::{
    CancelFlag, Content, LoopError, LoopEvent, Message, MessageId, Role, ToolCallId, ToolOutput,
    ToolUpdate, sync::lock,
};
use kage_tools::{ProgressSink, ToolContext, ToolError, ToolRegistry};

use crate::Hooks;
use crate::run::emit_one;
use crate::stream::PendingToolCall;

/// Per-call sink that buffers [`ToolUpdate`]s in a mutex-protected vec so
/// the dispatcher can drain and emit them after the tool returns (sequential)
/// or after all threads join (parallel).
struct BufferingSink {
    updates: Mutex<Vec<ToolUpdate>>,
}

impl BufferingSink {
    fn new() -> Self {
        Self {
            updates: Mutex::new(Vec::new()),
        }
    }

    fn drain(&self) -> Vec<ToolUpdate> {
        std::mem::take(&mut lock(&self.updates))
    }
}

impl ProgressSink for BufferingSink {
    fn emit(&self, update: ToolUpdate) {
        let mut v = lock(&self.updates);
        v.push(update);
    }
}

/// Emit every buffered update for one tool call as a `LoopEvent::ToolUpdate`.
fn flush_updates<F: FnMut(LoopEvent)>(
    sink: &BufferingSink,
    id: &ToolCallId,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) {
    for update in sink.drain() {
        emit_one(
            hooks,
            emit,
            LoopEvent::ToolUpdate {
                id: id.clone(),
                update,
            },
        );
    }
}

/// Outcome of [`Hooks::before_tool_call`] for one entry: either a
/// short-circuit output the host produced, or run the real tool.
enum Slot {
    Short(ToolOutput),
    Run,
}

/// Result of one batch of tool dispatch.
///
/// `results` is the message list to append to history, one per call, real
/// or synthesized; `all_terminate` is `true` when every tool in the batch
/// returned `ToolOutput::terminate` so the loop can stop cleanly after
/// appending the results. `error` carries the first unrecoverable failure
/// (cancel, or a tool error the loop cannot convert to an output); when it
/// is set, the trailing synthesized results explain the abort.
pub(crate) struct DispatchOutcome {
    pub results: Vec<Message>,
    pub all_terminate: bool,
    pub error: Option<LoopError>,
}

/// Tool output standing in for a call that never produced one because the
/// batch aborted (cancel or unrecoverable error). `is_error` so the model
/// sees the call did not run; the text says why.
fn synthesized_output(error: &LoopError) -> ToolOutput {
    let text = match error {
        LoopError::Cancelled => "tool call cancelled before completion".to_owned(),
        other => format!("tool did not run: {other}"),
    };
    ToolOutput {
        is_error: true,
        text,
        structured: None,
        terminate: false,
    }
}

/// Record the batch-level failure worth surfacing to the caller.
///
/// `Cancelled` wins because the run is aborting by user request; otherwise
/// the first error sticks.
fn record_batch_error(slot: &mut Option<LoopError>, kind: &LoopError) {
    let cancelled = matches!(kind, LoopError::Cancelled);
    if matches!(slot, Some(LoopError::Cancelled)) && !cancelled {
        return;
    }
    if slot.is_none() || cancelled {
        *slot = Some(kind.clone());
    }
}

/// Dispatch every pending tool call sequentially.
///
/// Returns one tool-result [`Message`] per call, in input order. The caller
/// appends them to history before continuing the inner loop.
///
/// Cancellation: polled before every call. On cancel, or on a tool error
/// the loop cannot recover from, the failing call and every remaining call
/// get synthesized `is_error` results, the completed results are kept, and
/// the failure is carried in [`DispatchOutcome::error`].
pub(crate) fn dispatch_tool_calls<F: FnMut(LoopEvent)>(
    pending: Vec<PendingToolCall>,
    tools: &ToolRegistry,
    workdir: &Path,
    cancel: &CancelFlag,
    parent: MessageId,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> DispatchOutcome {
    let mut results = Vec::with_capacity(pending.len());
    let mut all_terminate = !pending.is_empty();
    let mut error: Option<LoopError> = None;
    for call in pending {
        let sink = Arc::new(BufferingSink::new());
        let raw = if error.is_none() && !cancel.is_cancelled() {
            let pre = hooks.before_tool_call(&call.name, &call.input);
            if let Some(out) = pre {
                Some(out)
            } else {
                let sink_dyn = Arc::clone(&sink) as Arc<dyn ProgressSink>;
                match execute(tools, &call, workdir, cancel, Some(sink_dyn)) {
                    Ok(out) => Some(out),
                    Err(kind) => {
                        record_batch_error(&mut error, &kind);
                        None
                    }
                }
            }
        } else {
            record_batch_error(&mut error, &LoopError::Cancelled);
            None
        };

        let output = match raw {
            Some(out) => hooks.after_tool_call(&call.name, out),
            None => hooks.after_tool_call(
                &call.name,
                synthesized_output(error.as_ref().unwrap_or(&LoopError::Cancelled)),
            ),
        };
        all_terminate &= output.terminate;

        flush_updates(&sink, &call.id, hooks, emit);

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
    DispatchOutcome {
        results,
        all_terminate,
        error,
    }
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
///
/// A call whose thread reports cancel or panic gets a synthesized
/// `is_error` result; every call that did produce an output keeps it. The
/// batch-level failure (cancel preferred over panic, first otherwise) is
/// carried in [`DispatchOutcome::error`].
pub(crate) fn dispatch_tool_calls_parallel<F: FnMut(LoopEvent)>(
    pending: Vec<PendingToolCall>,
    tools: &ToolRegistry,
    workdir: &Path,
    cancel: &CancelFlag,
    parent: MessageId,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> DispatchOutcome {
    let mut error: Option<LoopError> = None;

    // Resolve hook short-circuits up front, single-threaded. Skipped
    // entirely when the batch is already cancelled: nothing will run.
    let entry_cancelled = cancel.is_cancelled();
    if entry_cancelled {
        error = Some(LoopError::Cancelled);
    }
    let mut slots: Vec<Slot> = Vec::with_capacity(pending.len());
    if !entry_cancelled {
        for call in &pending {
            match hooks.before_tool_call(&call.name, &call.input) {
                Some(out) => slots.push(Slot::Short(out)),
                None => slots.push(Slot::Run),
            }
        }
    }

    let sinks: Vec<Arc<BufferingSink>> = (0..pending.len())
        .map(|_| Arc::new(BufferingSink::new()))
        .collect();

    // Per-call outcome: `None` when the call never ran (entry cancel),
    // otherwise the tool's output or its unrecoverable error.
    let raw_outputs: Vec<Option<Result<ToolOutput, LoopError>>> = if entry_cancelled {
        std::iter::repeat_n(None, pending.len()).collect()
    } else {
        std::thread::scope(|scope| {
            let mut handles: Vec<Option<std::thread::ScopedJoinHandle<'_, _>>> =
                Vec::with_capacity(pending.len());
            for ((call, slot), sink) in pending.iter().zip(&slots).zip(&sinks) {
                match slot {
                    Slot::Short(_) => handles.push(None),
                    Slot::Run => {
                        let call = call.clone();
                        let sink = Arc::clone(sink);
                        let handle =
                            scope.spawn(move || execute(tools, &call, workdir, cancel, Some(sink)));
                        handles.push(Some(handle));
                    }
                }
            }
            handles
                .into_iter()
                .zip(slots)
                .map(|(handle, slot)| match slot {
                    Slot::Short(out) => Some(Ok(out)),
                    Slot::Run => {
                        let res = handle.expect("Run slot spawned a handle").join();
                        Some(match res {
                            Ok(res) => res,
                            Err(_) => Err(LoopError::Other {
                                message: "tool thread panicked".into(),
                            }),
                        })
                    }
                })
                .collect()
        })
    };

    let mut results = Vec::with_capacity(pending.len());
    let mut all_terminate = !pending.is_empty();
    for ((call, raw), sink) in pending.into_iter().zip(raw_outputs).zip(&sinks) {
        let output = match raw {
            Some(Ok(out)) => hooks.after_tool_call(&call.name, out),
            Some(Err(kind)) => {
                record_batch_error(&mut error, &kind);
                hooks.after_tool_call(&call.name, synthesized_output(&kind))
            }
            None => hooks.after_tool_call(&call.name, synthesized_output(&LoopError::Cancelled)),
        };
        all_terminate &= output.terminate;
        flush_updates(sink, &call.id, hooks, emit);
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
    DispatchOutcome {
        results,
        all_terminate,
        error,
    }
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
    progress: Option<Arc<dyn ProgressSink>>,
) -> Result<ToolOutput, LoopError> {
    let Some(tool) = tools.get(&call.name) else {
        return Ok(ToolOutput {
            is_error: true,
            text: format!("tool '{}' is not registered", call.name),
            structured: None,
            terminate: false,
        });
    };

    let mut cx = ToolContext::new(workdir, cancel);
    if let Some(sink) = progress {
        cx = cx.with_progress(sink);
    }
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
mod tests;
