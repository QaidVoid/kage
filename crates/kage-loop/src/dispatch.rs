//! Dispatch of tool calls produced by one assistant turn.
//!
//! Walks the [`PendingToolCall`] list from [`crate::stream::collect_turn`],
//! consults [`Hooks::before_tool_call`] for short-circuit, emits a
//! [`LoopEvent::ToolExecutionStart`] and executes the tool through the
//! registry, runs the result through [`Hooks::after_tool_call`],
//! emits a [`LoopEvent::ToolCallEnd`], and produces one tool-result message
//! per call to append to history. Calls run one at a time or, for a
//! parallel batch, concurrently. A parallel call is finished as soon as it
//! completes, so its `ToolCallEnd` follows completion order, while the
//! result messages always keep input order.
//!
//! Dispatch is infallible: a call that never produced an output (cancel, or
//! a tool failure the loop cannot recover from) gets a synthesized
//! `is_error` result. The assistant message's `tool_use` blocks are
//! therefore always answered, in memory and in the persisted session, so a
//! resumed run never sends a provider a dangling `tool_use`.

use std::path::Path;
use std::sync::{Arc, mpsc};

use kage_core::event::TOOL_CANCELLED_TEXT;
use kage_core::{
    CancelFlag, Content, LoopError, LoopEvent, Message, MessageId, Role, ToolCallId, ToolOutput,
    ToolUpdate,
};
use kage_tools::{ProgressSink, ToolContext, ToolError, ToolRegistry};

use crate::Hooks;
use crate::stream::PendingToolCall;

/// Message from a tool thread to the dispatching loop thread.
enum Progress {
    Update(ToolCallId, ToolUpdate),
    Done(usize, Result<ToolOutput, LoopError>),
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

/// Reports the result of the call at `index`. When dropped without a
/// report, as on panic, it reports an error for that call instead.
struct DoneOnDrop {
    index: usize,
    tx: mpsc::Sender<Progress>,
    reported: bool,
}

impl DoneOnDrop {
    fn report(mut self, result: Result<ToolOutput, LoopError>) {
        self.reported = true;
        let _ = self.tx.send(Progress::Done(self.index, result));
    }
}

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        if !self.reported {
            let _ = self.tx.send(Progress::Done(
                self.index,
                Err(LoopError::Other {
                    message: "tool thread panicked".into(),
                }),
            ));
        }
    }
}

/// Execute `calls` on scoped threads and emit their progress live.
///
/// Emits a [`LoopEvent::ToolExecutionStart`] per call first. The calling
/// thread forwards every update as a [`LoopEvent::ToolUpdate`] and hands
/// each result to `done` with the call's index in `calls` as soon as that
/// call completes, so `emit` and `done` stay on the loop thread. A
/// panicking tool yields an error for its own call.
#[allow(clippy::too_many_arguments)]
fn execute_live<F, D>(
    calls: &[&PendingToolCall],
    tools: &ToolRegistry,
    workdir: &Path,
    cancel: &CancelFlag,
    confine_paths: bool,
    emit: &mut F,
    mut done: D,
) where
    F: FnMut(LoopEvent),
    D: FnMut(&mut F, usize, Result<ToolOutput, LoopError>),
{
    for call in calls {
        emit(LoopEvent::ToolExecutionStart {
            id: call.id.clone(),
        });
    }
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let handles: Vec<_> = calls
            .iter()
            .enumerate()
            .map(|(index, &call)| {
                let sink = Arc::new(ChannelSink {
                    id: call.id.clone(),
                    tx: tx.clone(),
                });
                let reporter = DoneOnDrop {
                    index,
                    tx: tx.clone(),
                    reported: false,
                };
                scope.spawn(move || {
                    reporter.report(execute(
                        tools,
                        call,
                        workdir,
                        cancel,
                        confine_paths,
                        Some(sink),
                    ));
                })
            })
            .collect();

        let mut running = handles.len();
        while running > 0 {
            match rx.recv() {
                Ok(Progress::Update(id, update)) => {
                    emit(LoopEvent::ToolUpdate { id, update });
                }
                Ok(Progress::Done(index, result)) => {
                    running -= 1;
                    done(emit, index, result);
                }
                Err(_) => break,
            }
        }

        for handle in handles {
            let _ = handle.join();
        }
    });
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
        LoopError::Cancelled => TOOL_CANCELLED_TEXT.to_owned(),
        other => format!("tool did not run: {other}"),
    };
    ToolOutput {
        is_error: true,
        text,
        structured: None,
        terminate: false,
    }
}

/// Answer every call in `pending` without running it, so history never
/// carries a dangling tool use when the run stops after a turn.
pub(crate) fn unrun_results<F: FnMut(LoopEvent)>(
    pending: Vec<PendingToolCall>,
    parent: MessageId,
    emit: &mut F,
) -> Vec<Message> {
    pending
        .into_iter()
        .map(|call| {
            let output = ToolOutput {
                is_error: true,
                text: "tool did not run: the run stopped after this turn".to_owned(),
                structured: None,
                terminate: false,
            };
            emit(LoopEvent::ToolCallEnd {
                id: call.id.clone(),
                output: output.clone(),
            });
            Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: call.id,
                    output: output.text,
                    is_error: true,
                }],
                Some(parent),
            )
        })
        .collect()
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
            let pre = hooks.before_tool_call(&call.id, &call.name, &call.input);
            if let Some(out) = pre {
                Some(out)
            } else {
                let mut result = None;
                execute_live(
                    &[&call],
                    tools,
                    workdir,
                    cancel,
                    confine_paths,
                    emit,
                    |_, _, done| result = Some(done),
                );
                match result.expect("one call yields one result") {
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

        emit(LoopEvent::ToolCallEnd {
            id: call.id.clone(),
            output: output.clone(),
        });

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
/// only the tool's `execute` runs concurrently. Each call is finished on
/// the calling thread as soon as it completes: `after_tool_call` runs and
/// its [`LoopEvent::ToolCallEnd`] is emitted in completion order. Result
/// message order is preserved to match the input order, regardless of
/// completion order.
///
/// Calls that get short-circuited by `before_tool_call` skip thread
/// dispatch entirely and are finished before the others start. The
/// remaining calls all run on dedicated threads inside one
/// [`std::thread::scope`] block; the function blocks until the last one
/// completes.
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
    // Resolve hook short-circuits up front, single-threaded. Skipped
    // entirely when the batch is already cancelled: nothing will run.
    let entry_cancelled = cancel.is_cancelled();
    let mut slots: Vec<Slot> = Vec::with_capacity(pending.len());
    if !entry_cancelled {
        for call in &pending {
            match hooks.before_tool_call(&call.id, &call.name, &call.input) {
                Some(out) => slots.push(Slot::Short(out)),
                None => slots.push(Slot::Run),
            }
        }
    }

    let mut error = entry_cancelled.then_some(LoopError::Cancelled);
    let mut all_terminate = !pending.is_empty();
    let mut outputs: Vec<Option<ToolOutput>> = std::iter::repeat_with(|| None)
        .take(pending.len())
        .collect();
    let mut finish = |emit: &mut F, index: usize, raw: Result<ToolOutput, LoopError>| {
        let call = &pending[index];
        let output = match raw {
            Ok(out) => hooks.after_tool_call(&call.name, out),
            Err(kind) => {
                record_batch_error(&mut error, &kind);
                hooks.after_tool_call(&call.name, synthesized_output(&kind))
            }
        };
        all_terminate &= output.terminate;
        emit(LoopEvent::ToolCallEnd {
            id: call.id.clone(),
            output: output.clone(),
        });
        outputs[index] = Some(output);
    };

    if entry_cancelled {
        for index in 0..pending.len() {
            finish(emit, index, Err(LoopError::Cancelled));
        }
    } else {
        let mut to_run = Vec::new();
        for (index, slot) in slots.into_iter().enumerate() {
            match slot {
                Slot::Short(out) => finish(emit, index, Ok(out)),
                Slot::Run => to_run.push(index),
            }
        }
        let calls: Vec<&PendingToolCall> = to_run.iter().map(|&index| &pending[index]).collect();
        execute_live(
            &calls,
            tools,
            workdir,
            cancel,
            confine_paths,
            emit,
            |emit, index, raw| finish(emit, to_run[index], raw),
        );
    }

    let results = pending
        .into_iter()
        .zip(outputs)
        .map(|(call, output)| {
            let output = output.expect("every call is finished");
            Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: call.id,
                    output: output.text,
                    is_error: output.is_error,
                }],
                Some(parent),
            )
        })
        .collect();
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

    let mut cx = ToolContext::new(workdir, cancel).with_call_id(&call.id);
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
