//! One agent run on its own thread.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::select_biased;
use kage_core::agent_report::AgentLimit;

use kage_core::protocol::{HostEvent, McpServerInfo, NoticeLevel, RunOutcome, Usage};
use kage_core::sync::lock;
use kage_core::{
    CancelFlag, LoopError, LoopEvent, Message, ModelCost, SessionId, TokenCost, TokenUsage,
    ToolOutput,
};
use kage_loop::{AgentContext, Hooks, LoopConfig};
use kage_mcp::expand::ExpandError;
use kage_mcp::{McpConnection, McpManager};
use kage_plugin::PluginRuntime;
use kage_provider::Provider;
use kage_tools::ToolRegistry;

use super::Input;
use super::bus::Bus;
use super::mcp::{McpDone, ToolDelta};
use super::recorder::Recorder;
use crate::permissions::PermissionGate;
use crate::plugins::PluginEventHooks;

/// Prompts submitted while a run is in flight, delivered at the next turn
/// boundary.
pub(super) type Steering = Arc<Mutex<VecDeque<String>>>;

/// Told to an agent at its turn limit, which then has one more turn.
const TURN_LIMIT_WARNING: &str =
    "Turn limit reached. Do not call more tools. Reply now with what you have and what is left.";

/// Bounds on an agent's run. A main session's runs have none.
#[derive(Default)]
pub(super) struct Limits {
    /// Turns with tool calls the run may take before its warning turn.
    pub max_turns: Option<u32>,
    /// How long the run may take before it is cancelled.
    pub timeout: Option<Duration>,
    /// The token budget the run counts against.
    pub budget: Option<Budget>,
}

/// A share of the agent budget of one main session.
pub(super) struct Budget {
    pub spend: Arc<Spend>,
    /// Tokens the agents may use between two user prompts.
    pub tokens: u64,
    /// The main session the agents hang under.
    pub root: SessionId,
}

/// What the agents of one main session used since its last user
/// prompt.
#[derive(Debug, Default)]
pub(super) struct Spend {
    tokens: AtomicU64,
    spent: AtomicBool,
}

impl Spend {
    /// Whether the agents used their budget.
    pub(super) fn is_spent(&self) -> bool {
        self.spent.load(Ordering::Relaxed)
    }

    /// Start counting again, at a user prompt.
    pub(super) fn reset(&self) {
        self.tokens.store(0, Ordering::Relaxed);
        self.spent.store(false, Ordering::Relaxed);
    }
}

impl Budget {
    /// Count `tokens` more. True only for the call that crosses the
    /// budget.
    fn add(&self, tokens: u64) -> bool {
        let used = self.spend.tokens.fetch_add(tokens, Ordering::Relaxed) + tokens;
        used >= self.tokens && !self.spend.spent.swap(true, Ordering::Relaxed)
    }
}

/// What a run does.
pub(super) enum Work {
    /// Answer a user prompt.
    Prompt(Message),
    /// Summarize older turns now.
    Compact,
}

/// The session's MCP servers, lent to a run so their restarts and list
/// reloads happen on the run thread. The run hands the manager back in
/// [`McpDone`] before the loop starts.
pub(super) struct McpLease {
    pub manager: McpManager,
    /// Servers to restart first.
    pub restarts: Vec<String>,
}

impl McpLease {
    /// Apply the restarts and announced list changes to `tools`, hand the
    /// manager back to the dispatcher, and return the live connections
    /// and the catalog that expansion reads.
    fn refresh(
        self,
        session: SessionId,
        bus: &Bus,
        tools: &mut ToolRegistry,
        done: &mpsc::Sender<Input>,
    ) -> (Vec<(String, Arc<McpConnection>)>, Vec<McpServerInfo>) {
        let Self {
            mut manager,
            restarts,
        } = self;
        let before = tools.clone();
        let catalog = super::mcp::refresh_mcp(bus, session, &mut manager, &restarts, tools);
        let clients = manager.clients();
        let _ = done.send(Input::McpDone(Box::new(McpDone {
            session,
            manager,
            tools: ToolDelta::between(&before, tools),
            idle: None,
        })));
        (clients, catalog)
    }
}

/// Everything one run needs. The run owns the context and recorder until
/// it hands them back in [`Finished`].
pub(super) struct Run {
    pub session: SessionId,
    pub work: Work,
    pub provider: Arc<dyn Provider>,
    pub tools: ToolRegistry,
    pub cx: AgentContext,
    pub recorder: Option<Recorder>,
    pub usage: Usage,
    pub loop_cfg: LoopConfig,
    pub cancel: CancelFlag,
    pub gate: PermissionGate,
    pub steering: Steering,
    pub inbox: Steering,
    pub plugins: Option<Arc<PluginRuntime>>,
    pub mcp: Option<McpLease>,
    pub bus: Arc<Bus>,
    pub limits: Limits,
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
    /// Wall-clock time the run was in flight, matching the run span
    /// the clients see between `RunStarted` and `RunEnded`.
    pub run_time: Duration,
    /// The turn limit or the timeout, when one ended the run.
    pub limit: Option<AgentLimit>,
}

impl Run {
    pub(super) fn spawn(self, done: mpsc::Sender<Input>) {
        let (ended, timed_out) = match self.limits.timeout {
            Some(limit) => {
                let (ended, timed_out) = watch_time(limit, self.cancel.clone());
                (Some(ended), Some(timed_out))
            }
            None => (None, None),
        };
        thread::spawn(move || {
            let mut finished = self.execute(&done);
            drop(ended);
            if finished.outcome == RunOutcome::Cancelled
                && timed_out.is_some_and(|flag| flag.load(Ordering::Relaxed))
            {
                finished.limit = Some(AgentLimit::Time);
            }
            let _ = done.send(Input::Finished(Box::new(finished)));
        });
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one run reads top to bottom: setup, prompt, loop, finish"
    )]
    fn execute(self, done: &mpsc::Sender<Input>) -> Finished {
        let Self {
            session,
            work,
            provider,
            mut tools,
            mut cx,
            mut recorder,
            mut usage,
            loop_cfg,
            cancel,
            gate,
            steering,
            inbox,
            plugins,
            mcp,
            bus,
            limits,
        } = self;

        let (clients, catalog) = mcp
            .map(|lease| lease.refresh(session, &bus, &mut tools, done))
            .unwrap_or_default();
        bus.publish(session, HostEvent::RunStarted);
        let started_at = Instant::now();
        let price = kage_provider::model_cost(provider.as_ref(), &cx.model);
        let mut tool_names = HashMap::new();
        let mut emit = |event: LoopEvent| {
            if let Some(rt) = &plugins {
                crate::plugins::forward_event(rt, &event, &mut tool_names);
            }
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
                add_turn(&mut usage, &turn, price);
                bus.publish(session, HostEvent::UsageUpdated { usage });
                if let Some(budget) = &limits.budget
                    && budget.add(turn.input + turn.output)
                {
                    let _ = done.send(Input::BudgetSpent { root: budget.root });
                }
            }
        };

        let turn_limited = Arc::new(AtomicBool::new(false));
        let base = RunHooks {
            gate,
            steering,
            inbox,
            turns: limits.max_turns.map(|max| TurnLimit {
                max,
                warning: None,
                warned: false,
                hit: Arc::clone(&turn_limited),
            }),
        };
        let mut hooks: Box<dyn Hooks> = match &plugins {
            Some(rt) => Box::new(PluginEventHooks::new(base, Arc::clone(rt))),
            None => Box::new(base),
        };
        let result = match work {
            Work::Prompt(prompt) => {
                expand(&bus, session, prompt, &clients, &catalog).and_then(|prompt| {
                    let first_text = crate::cli_loop_run::first_user_text(&prompt);
                    let message = Arc::new(prompt);
                    cx.history.push(Arc::clone(&message));
                    emit(LoopEvent::MessageAppended { message });
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
                })
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
            run_time: started_at.elapsed(),
            limit: turn_limited
                .load(Ordering::Relaxed)
                .then_some(AgentLimit::Turns),
        }
    }
}

/// Cancel a run through `cancel` once `limit` passes, unless the
/// returned sender is dropped first, as the run does when it ends.
/// The flag tells whether the time ran out.
fn watch_time(
    limit: Duration,
    cancel: CancelFlag,
) -> (crossbeam_channel::Sender<()>, Arc<AtomicBool>) {
    let (ended, watch) = crossbeam_channel::bounded::<()>(0);
    let timed_out = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&timed_out);
    thread::spawn(move || {
        select_biased! {
            recv(watch) -> _ => {}
            recv(crossbeam_channel::after(limit)) -> _ => {
                flag.store(true, Ordering::Relaxed);
                cancel.cancel();
            }
        }
    });
    (ended, timed_out)
}

/// `prompt` with its MCP prompt command and resource mentions expanded.
/// A failure is published as a notice and fails the run, as an invalid
/// prompt when the text itself is at fault.
fn expand(
    bus: &Bus,
    session: SessionId,
    mut prompt: Message,
    clients: &[(String, Arc<McpConnection>)],
    catalog: &[McpServerInfo],
) -> Result<Message, LoopError> {
    let content = std::mem::take(&mut prompt.content);
    match kage_mcp::expand::expand(content, clients, catalog) {
        Ok(content) => Ok(Message { content, ..prompt }),
        Err(err) => {
            let message = err.to_string();
            notice(bus, session, message.clone());
            Err(match err {
                ExpandError::MissingArgument { .. } | ExpandError::Placeholder { .. } => {
                    LoopError::InvalidPrompt { message }
                }
                _ => LoopError::Other { message },
            })
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

/// Fold one turn's usage into the session totals, priced at `price`
/// when the model has a known price.
fn add_turn(usage: &mut Usage, turn: &TokenUsage, price: Option<ModelCost>) {
    let total = &mut usage.total;
    total.input += turn.input;
    total.output += turn.output;
    total.cache_read += turn.cache_read;
    total.cache_write += turn.cache_write;
    usage.context_used = turn.input + turn.output + turn.cache_read + turn.cache_write;
    if let Some(rate) = price {
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

/// Control hooks every run starts from: the permission gate, the
/// session's steering queue, its inbox of agent reports and an agent's
/// turn limit.
struct RunHooks {
    gate: PermissionGate,
    steering: Steering,
    inbox: Steering,
    turns: Option<TurnLimit>,
}

/// An agent run's turn limit: at the limit the agent is warned and gets
/// one more turn, and a further turn with tool calls ends the run.
struct TurnLimit {
    max: u32,
    /// The warning, until the next turn reads it.
    warning: Option<&'static str>,
    warned: bool,
    /// Set once the limit is reached, for the run's report.
    hit: Arc<AtomicBool>,
}

impl RunHooks {
    /// Every inbox entry, joined with blank lines, so a burst of
    /// reports lands in one message.
    fn drain_inbox(&self) -> Option<String> {
        let entries: Vec<String> = lock(&self.inbox).drain(..).collect();
        (!entries.is_empty()).then(|| entries.join("\n\n"))
    }
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
        if let Some(warning) = self.turns.as_mut().and_then(|t| t.warning.take()) {
            return Some(warning.to_owned());
        }
        lock(&self.steering)
            .pop_front()
            .or_else(|| self.drain_inbox())
    }

    fn should_stop_after_turn(&mut self, summary: &kage_loop::TurnSummary) -> bool {
        let Some(limit) = self.turns.as_mut() else {
            return false;
        };
        if !summary.had_tool_calls || summary.index + 1 < limit.max {
            return false;
        }
        limit.hit.store(true, Ordering::Relaxed);
        if limit.warned {
            return true;
        }
        limit.warned = true;
        limit.warning = Some(TURN_LIMIT_WARNING);
        false
    }

    fn get_followup(&mut self) -> Option<String> {
        self.drain_inbox()
    }
}
