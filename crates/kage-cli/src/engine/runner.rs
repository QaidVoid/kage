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

/// Everything one run needs. The run owns the context and recorder until
/// it hands them back in [`Finished`].
pub(super) struct Run {
    pub session: SessionId,
    pub prompt: Message,
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

/// State a finished run returns to its session.
pub(super) struct Finished {
    pub session: SessionId,
    pub cx: AgentContext,
    pub recorder: Option<Recorder>,
    pub usage: Usage,
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
            prompt,
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
        let first_text = crate::cli_loop_run::first_user_text(&prompt);
        cx.history.push(prompt.clone());

        let mut emit = |event: LoopEvent| {
            if let Some(rec) = recorder.as_mut()
                && let Err(err) = rec.observe(&event)
            {
                bus.publish(
                    session,
                    HostEvent::Notice {
                        level: NoticeLevel::Error,
                        text: format!("session write failed: {err}"),
                        transient: false,
                    },
                );
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
        emit(LoopEvent::MessageAppended { message: prompt });

        let base = RunHooks { gate, steering };
        let result = if let Some(rt) = plugins {
            let mut hooks = PluginEventHooks::new(base, rt);
            hooks.dispatch_before_agent_start(&cx.system_prompt, &first_text);
            hooks.dispatch_agent_start();
            let result = kage_loop::run(
                provider.as_ref(),
                &tools,
                &mut cx,
                loop_cfg,
                &mut hooks,
                &cancel,
                &mut emit,
            );
            hooks.dispatch_agent_end(result.is_ok());
            result
        } else {
            let mut hooks = base;
            kage_loop::run(
                provider.as_ref(),
                &tools,
                &mut cx,
                loop_cfg,
                &mut hooks,
                &cancel,
                &mut emit,
            )
        };

        let outcome = match result {
            Ok(()) => RunOutcome::Completed,
            Err(LoopError::Cancelled) => RunOutcome::Cancelled,
            Err(error) => RunOutcome::Failed { error },
        };
        bus.publish(session, HostEvent::RunEnded { outcome });
        Finished {
            session,
            cx,
            recorder,
            usage,
        }
    }
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
