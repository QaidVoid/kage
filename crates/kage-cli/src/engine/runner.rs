//! One agent run on its own thread.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use kage_core::protocol::{HostEvent, NoticeLevel, RunOutcome, Usage};
use kage_core::sync::lock;
use kage_core::{
    CancelFlag, LoopError, LoopEvent, Message, SessionId, TokenCost, TokenUsage, ToolOutput,
};
use kage_loop::{AgentContext, Hooks, LoopConfig};
use kage_plugin::PluginRuntime;
use kage_provider::Provider;
use kage_tools::ToolRegistry;

use super::Input;
use super::bus::Bus;
use super::recorder::Recorder;
use crate::permissions::PermissionGate;
use crate::plugins::PluginEventHooks;

/// Prompts submitted while a run is in flight, delivered at the next turn
/// boundary.
pub(super) type Steering = Arc<Mutex<VecDeque<String>>>;

/// What a run does.
pub(super) enum Work {
    /// Answer a user prompt.
    Prompt(Message),
    /// Summarize older turns now.
    Compact,
}

/// Everything one run needs. The run owns the context and recorder until
/// it hands them back in [`Finished`].
pub(super) struct Run {
    pub session: SessionId,
    pub work: Work,
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub tools: ToolRegistry,
    pub cx: AgentContext,
    pub recorder: Option<Recorder>,
    pub usage: Usage,
    pub loop_cfg: LoopConfig,
    pub cancel: CancelFlag,
    pub gate: PermissionGate,
    pub steering: Steering,
    pub plugins: Option<Arc<PluginRuntime>>,
    pub bus: Arc<Bus>,
}

/// State a finished run returns to its session. The dispatcher publishes
/// `RunEnded` once the session is idle again, so a client reacting to it
/// never finds the session busy.
pub(super) struct Finished {
    pub session: SessionId,
    pub cx: AgentContext,
    pub recorder: Option<Recorder>,
    pub usage: Usage,
    pub outcome: RunOutcome,
}

impl Run {
    pub(super) fn spawn(self, done: mpsc::Sender<Input>) {
        thread::spawn(move || {
            let finished = self.execute();
            let _ = done.send(Input::Finished(Box::new(finished)));
        });
    }

    fn execute(self) -> Finished {
        let Self {
            session,
            work,
            provider,
            model,
            tools,
            mut cx,
            mut recorder,
            mut usage,
            loop_cfg,
            cancel,
            gate,
            steering,
            plugins,
            bus,
        } = self;

        bus.publish(session, HostEvent::RunStarted);
        let mut emit = |event: LoopEvent| {
            if let Some(rec) = recorder.as_mut()
                && let Err(err) = rec.observe(&event)
            {
                notice(&bus, session, format!("session write failed: {err}"));
            }
            let turn_usage = match &event {
                LoopEvent::MessageEnd { usage, .. } => Some(*usage),
                _ => None,
            };
            bus.publish(session, event);
            if let Some(turn) = turn_usage {
                add_turn(&mut usage, &turn, &model);
                bus.publish(session, HostEvent::UsageUpdated { usage });
            }
        };

        let base = RunHooks { gate, steering };
        let mut hooks: Box<dyn Hooks> = match &plugins {
            Some(rt) => Box::new(PluginEventHooks::new(base, Arc::clone(rt))),
            None => Box::new(base),
        };
        let result = match work {
            Work::Prompt(prompt) => {
                let first_text = crate::cli_loop_run::first_user_text(&prompt);
                cx.history.push(prompt.clone());
                emit(LoopEvent::MessageAppended { message: prompt });
                if let Some(rt) = &plugins {
                    crate::plugins::dispatch_run_start(rt, &cx.system_prompt, &first_text);
                }
                let result = kage_loop::run(
                    provider.as_ref(),
                    &tools,
                    &mut cx,
                    loop_cfg,
                    hooks.as_mut(),
                    &cancel,
                    &mut emit,
                );
                if let Some(rt) = &plugins {
                    crate::plugins::dispatch_run_end(rt, result.is_ok());
                }
                result
            }
            Work::Compact => {
                match kage_loop::force_compact(
                    &mut cx,
                    provider.as_ref(),
                    &cancel,
                    hooks.as_mut(),
                    &mut emit,
                ) {
                    Ok(false) => {
                        notice(&bus, session, "compact: not enough history yet".to_owned());
                        Ok(())
                    }
                    other => other.map(|_| ()),
                }
            }
        };

        let outcome = match result {
            Ok(()) => RunOutcome::Completed,
            Err(LoopError::Cancelled) => RunOutcome::Cancelled,
            Err(error) => RunOutcome::Failed { error },
        };
        Finished {
            session,
            cx,
            recorder,
            usage,
            outcome,
        }
    }
}

fn notice(bus: &Bus, session: SessionId, text: String) {
    bus.publish(
        session,
        HostEvent::Notice {
            level: NoticeLevel::Error,
            text,
            transient: false,
        },
    );
}

/// Fold one turn's usage into the session totals.
fn add_turn(usage: &mut Usage, turn: &TokenUsage, model: &str) {
    let total = &mut usage.total;
    total.input += turn.input;
    total.output += turn.output;
    total.cache_read += turn.cache_read;
    total.cache_write += turn.cache_write;
    usage.context_used = turn.input + turn.output + turn.cache_read + turn.cache_write;
    if let Some((provider_id, model_id)) = model.split_once(':')
        && let Some(info) = kage_provider::catalog::model(provider_id, model_id)
        && let Some(rate) = info.cost
    {
        usage.cost += TokenCost::from_usage(
            turn,
            rate.input,
            rate.output,
            rate.cache_read,
            rate.cache_write,
        )
        .total;
    }
}

/// Control hooks every run starts from: the permission gate and the
/// session's steering queue.
struct RunHooks {
    gate: PermissionGate,
    steering: Steering,
}

impl Hooks for RunHooks {
    fn before_tool_call(
        &mut self,
        id: &kage_core::ToolCallId,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<ToolOutput> {
        self.gate.before_tool_call(id, name, input)
    }

    fn get_steering(&mut self) -> Option<String> {
        lock(&self.steering).pop_front()
    }
}
