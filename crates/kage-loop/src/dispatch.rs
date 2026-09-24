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
use std::sync::{Arc, mpsc};

use kage_core::{
    CancelFlag, Content, LoopError, LoopEvent, Message, MessageId, Role, ToolCallId, ToolOutput,
    ToolUpdate,
};
use kage_tools::{ProgressSink, ToolContext, ToolError, ToolRegistry};

use crate::Hooks;
use crate::run::emit_one;
use crate::stream::PendingToolCall;

/// Message from a tool thread to the dispatching loop thread.
enum Progress {
    Update(ToolCallId, ToolUpdate),
    Done,
}

/// Progress sink handed to one tool call. Forwards each update to the loop
/// thread, which emits it as a [`LoopEvent::ToolUpdate`] while the tool is
/// still running.
struct ChannelSink {
    id: ToolCallId,
    tx: mpsc::Sender<Progress>,
}

impl ProgressSink for ChannelSink {
    fn emit(&self, update: ToolUpdate) {
        let _ = self.tx.send(Progress::Update(self.id.clone(), update));
    }
}

/// Reports a tool thread as finished when dropped, including on panic.
struct DoneOnDrop(mpsc::Sender<Progress>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(Progress::Done);
    }
}

/// Execute `calls` on scoped threads and emit their progress live.
///
/// The calling thread forwards every update as a [`LoopEvent::ToolUpdate`]
/// until all threads finish, so hooks and `emit` stay on the loop thread.
/// Results come back in input order. A panicking tool yields an error.
#[allow(clippy::too_many_arguments)]
fn execute_live<F: FnMut(LoopEvent)>(
    calls: &[&PendingToolCall],
    tools: &ToolRegistry,
    workdir: &Path,
    cancel: &CancelFlag,
    confine_paths: bool,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Vec<Result<ToolOutput, LoopError>> {
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let handles: Vec<_> = calls
            .iter()
            .map(|&call| {
                let sink = Arc::new(ChannelSink {
                    id: call.id.clone(),
                    tx: tx.clone(),
                });
                let done = DoneOnDrop(tx.clone());
                scope.spawn(move || {
                    let _done = done;
                    execute(tools, call, workdir, cancel, confine_paths, Some(sink))
                })
            })
            .collect();

        let mut running = handles.len();
        while running > 0 {
            match rx.recv() {
                Ok(Progress::Update(id, update)) => {
                    emit_one(hooks, emit, LoopEvent::ToolUpdate { id, update });
                }
                Ok(Progress::Done) => running -= 1,
                Err(_) => break,
            }
        }

        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| {
                    Err(LoopError::Other {
                        message: "tool thread panicked".into(),
                    })
                })
            })
            .collect()
    })
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_tool_calls<F: FnMut(LoopEvent)>(
    pending: Vec<PendingToolCall>,
    tools: &ToolRegistry,
    workdir: &Path,
    cancel: &CancelFlag,
    confine_paths: bool,
    parent: MessageId,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> DispatchOutcome {
    let mut results = Vec::with_capacity(pending.len());
    let mut all_terminate = !pending.is_empty();
    let mut error: Option<LoopError> = None;
    for call in pending {
        let raw = if error.is_none() && !cancel.is_cancelled() {
            let pre = hooks.before_tool_call(&call.name, &call.input);
            if let Some(out) = pre {
                Some(out)
            } else {
                let result =
                    execute_live(&[&call], tools, workdir, cancel, confine_paths, hooks, emit)
                        .pop()
                        .expect("one call yields one result");
                match result {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_tool_calls_parallel<F: FnMut(LoopEvent)>(
    pending: Vec<PendingToolCall>,
    tools: &ToolRegistry,
    workdir: &Path,
    cancel: &CancelFlag,
    confine_paths: bool,
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

    // Per-call outcome: `None` when the call never ran (entry cancel),
    // otherwise the tool's output or its unrecoverable error.
    let raw_outputs: Vec<Option<Result<ToolOutput, LoopError>>> = if entry_cancelled {
        std::iter::repeat_n(None, pending.len()).collect()
    } else {
        let to_run: Vec<&PendingToolCall> = pending
            .iter()
            .zip(&slots)
            .filter(|(_, slot)| matches!(slot, Slot::Run))
            .map(|(call, _)| call)
            .collect();
        let mut ran =
            execute_live(&to_run, tools, workdir, cancel, confine_paths, hooks, emit).into_iter();
        slots
            .into_iter()
            .map(|slot| match slot {
                Slot::Short(out) => Some(Ok(out)),
                Slot::Run => ran.next(),
            })
            .collect()
    };

    let mut results = Vec::with_capacity(pending.len());
    let mut all_terminate = !pending.is_empty();
    for (call, raw) in pending.into_iter().zip(raw_outputs) {
        let output = match raw {
            Some(Ok(out)) => hooks.after_tool_call(&call.name, out),
            Some(Err(kind)) => {
                record_batch_error(&mut error, &kind);
                hooks.after_tool_call(&call.name, synthesized_output(&kind))
            }
            None => hooks.after_tool_call(&call.name, synthesized_output(&LoopError::Cancelled)),
        };
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
    confine_paths: bool,
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
    if confine_paths {
        cx = cx.with_confine();
    }
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
