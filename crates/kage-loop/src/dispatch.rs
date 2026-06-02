//! Sequential dispatch of tool calls produced by one assistant turn.
//!
//! Walks the [`PendingToolCall`] list from [`crate::stream::collect_turn`],
//! consults [`Hooks::before_tool_call`] for short-circuit, executes the tool
//! through the registry, runs the result through [`Hooks::after_tool_call`],
//! emits a [`LoopEvent::ToolCallEnd`], and produces one tool-result message
//! per call to append to history.

use std::path::Path;
use std::sync::{Arc, Mutex};

use kage_core::{
    CancelFlag, Content, LoopError, LoopEvent, Message, MessageId, Role, ToolCallId, ToolOutput,
    ToolUpdate,
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
        std::mem::take(&mut self.updates.lock().expect("buffering sink poisoned"))
    }
}

impl ProgressSink for BufferingSink {
    fn emit(&self, update: ToolUpdate) {
        if let Ok(mut v) = self.updates.lock() {
            v.push(update);
        }
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
        let sink = Arc::new(BufferingSink::new());
        let raw_output = if let Some(out) = pre {
            out
        } else {
            let sink_dyn: Arc<dyn ProgressSink> = Arc::clone(&sink) as Arc<dyn ProgressSink>;
            execute(tools, &call, workdir, cancel, Some(sink_dyn))?
        };
        let output = hooks.after_tool_call(&call.name, raw_output);
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

    let sinks: Vec<Arc<BufferingSink>> = (0..pending.len())
        .map(|_| Arc::new(BufferingSink::new()))
        .collect();

    let raw_outputs: Vec<Result<ToolOutput, LoopError>> = std::thread::scope(|scope| {
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
    for ((call, raw), sink) in pending.into_iter().zip(raw_outputs).zip(&sinks) {
        let raw = raw?;
        let output = hooks.after_tool_call(&call.name, raw);
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
