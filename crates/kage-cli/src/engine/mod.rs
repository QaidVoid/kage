//! The session host every frontend drives.
//!
//! Clients send [`Command`]s and observe [`kage_core::protocol::Envelope`]s
//! through subscribers. A dispatcher thread owns the sessions and never
//! blocks on a run: each run executes on its own thread and hands its
//! context back when it ends. MCP restarts and list reloads run off the
//! dispatcher too: at the start of a run on its thread, or on a worker
//! thread for a restart of an idle session. Permission questions travel
//! over the same channels: the engine publishes `PermissionRequested` and
//! a client answers with `ResolvePermission`.

mod agent_tool;
mod agents;
mod bus;
mod mailbox_tool;
mod mcp;
mod plan_tool;
mod plugin_tools;
mod recorder;
mod runner;
mod sessions;
mod shell;
mod swarm_tool;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use kage_core::agents::AgentDefs;
use kage_core::config::Config;
use kage_core::options::{OptionStore, OptionValue};
use kage_core::protocol::{
    Command, CommandKind, Delivery, HostEvent, NoticeLevel, PermissionDecision, RequestId,
    RunOutcome, SessionState, Usage,
};
use kage_core::sync::lock;
use kage_core::{
    CancelFlag, Content, LoopError, Message, Role, SessionId, ThinkingLevel, TokenUsage,
    ToolCallId, ToolOutput,
};
use kage_loop::{AgentContext, LoopConfig};
use kage_mcp::{McpError, McpManager};
use kage_plugin::PluginRuntime;
use kage_provider::ProviderRegistry;
use kage_tools::ToolRegistry;

use self::swarm_tool::SwarmInfo;

pub(crate) use bus::{Subscriber, SubscriptionId};
pub(crate) use recorder::Recorder;
#[cfg(test)]
pub(crate) use sessions::render_session_markdown;

use agent_tool::{AgentTool, Spawn};
use agents::{AgentLink, depth_of};
use bus::Bus;
use mailbox_tool::MailboxTool;
use mcp::{McpDone, restart_failed};
use plan_tool::ExitPlanTool;
use plugin_tools::PluginTools;
use runner::{Finished, McpLease, Run, Steering, Work};
use shell::ShellDone;
use swarm_tool::SwarmTool;

use crate::permissions::{Asker, PermissionGate, PermissionPrompt};

/// A session to host, with the resources its runs use.
pub(crate) struct SessionSpec {
    pub id: SessionId,
    /// Provider-qualified model id.
    pub model: String,
    pub cx: AgentContext,
    pub recorder: Option<Recorder>,
    pub tools: ToolRegistry,
    pub plugins: Option<Arc<PluginRuntime>>,
    pub gate: PermissionGate,
    pub loop_cfg: LoopConfig,
    /// MCP servers whose tools are in `tools`. Restarts and list changes
    /// are applied at the start of each run, on the run thread.
    pub mcp: Option<McpManager>,
    /// Whether a client answers permission requests. Without one, `ask`
    /// verdicts are refused.
    pub interactive: bool,
    /// Program user `!` commands and the shell tool run with, from
    /// `[shell] program`. `None` uses the platform default.
    pub shell: Option<String>,
    /// Generate and record a title after the first completed exchange.
    pub title: bool,
    /// Agent definitions and limits. `None` means no `agent` tool.
    pub agents: Option<AgentSetup>,
}

/// What the `agent` and `swarm` tools may start, shared by a whole
/// session tree.
#[derive(Clone)]
pub(crate) struct AgentSetup {
    pub defs: Arc<AgentDefs>,
    /// How deep agents may nest. 0 turns the `agent` tool off.
    pub max_depth: u8,
    /// How many agents run at once. Further agents wait their turn.
    pub max_running: usize,
    /// Most items one `swarm` call may start.
    pub swarm_max_items: usize,
    /// Overall deadline for one `swarm` call, in milliseconds.
    pub swarm_timeout_ms: u64,
}

impl AgentSetup {
    /// `defs` with the limits of `config`'s `[agents]` table, where an
    /// out-of-range value falls back to its default.
    pub(crate) fn from_config(defs: AgentDefs, config: &Config) -> Self {
        let (options, _) = OptionStore::from_config(config);
        let int = |name: &str| options.get(name).and_then(OptionValue::as_int).unwrap_or(0);
        Self {
            defs: Arc::new(defs),
            max_depth: u8::try_from(int("agent_max_depth")).unwrap_or(0),
            max_running: usize::try_from(int("agent_max_running")).unwrap_or(1),
            swarm_max_items: usize::try_from(int("swarm_max_items")).unwrap_or(32),
            swarm_timeout_ms: u64::try_from(int("swarm_timeout_ms")).unwrap_or(7_200_000),
        }
    }
}

/// Handle to a running engine. Dropping it shuts the engine down.
pub(crate) struct Engine {
    commander: Commander,
    bus: Arc<Bus>,
    thread: Option<thread::JoinHandle<()>>,
}

/// Cloneable handle for sending commands to an engine.
#[derive(Clone)]
pub(crate) struct Commander(mpsc::Sender<Input>);

impl Commander {
    pub(crate) fn send(&self, command: Command) {
        let _ = self.0.send(Input::Command(command));
    }

    /// Publish a host event, such as a notice, on the active session.
    pub(crate) fn publish(&self, event: HostEvent) {
        let _ = self.0.send(Input::Publish(event));
    }

    /// Resolve models against `registry` from the next run on.
    pub(crate) fn set_registry(&self, registry: Arc<ProviderRegistry>) {
        let _ = self.0.send(Input::SetRegistry(registry));
    }

    /// Replace every session's plugin tools with what its plugin runtime
    /// registers now, after a plugin reload.
    pub(crate) fn reload_plugin_tools(&self) {
        let _ = self.0.send(Input::ReloadPluginTools);
    }
}

enum Input {
    Command(Command),
    Open(Box<SessionSpec>),
    Spawn(Box<Spawn>),
    /// Check that every id names a swarm child of `parent` before the
    /// caller attaches to any of them. Replies with each child's
    /// marker facts, or the reason the whole call is refused.
    VerifyResume {
        parent: SessionId,
        ids: Vec<SessionId>,
        reply: crossbeam_channel::Sender<Result<Vec<ResumeChild>, String>>,
    },
    Attach(Box<Attach>),
    /// Drop a message into a live session's mailbox. The engine
    /// resolves the target, wraps the message so the target knows the
    /// sender, and prompts it with [`Delivery::Queue`]. Replies with
    /// an ack or the reason the delivery was refused.
    Deliver {
        from: SessionId,
        /// `None` addresses the sender's parent.
        to: Option<SessionId>,
        message: String,
        reply: crossbeam_channel::Sender<Result<String, String>>,
    },
    /// Re-prompt a swarm child that was rate limited and is waiting
    /// out its backoff. Sent by the backoff timer the engine armed
    /// when the child's run failed.
    RequeueChild {
        id: SessionId,
    },
    Finished(Box<Finished>),
    McpDone(Box<McpDone>),
    ShellDone(Box<ShellDone>),
    Title {
        session: SessionId,
        title: String,
    },
    /// A session's finished turn met its goal. Published as a success
    /// notice.
    GoalMet {
        session: SessionId,
        goal: String,
    },
    Publish(HostEvent),
    SetRegistry(Arc<ProviderRegistry>),
    ReloadPluginTools,
    /// The user approved a session's plan and `exit_plan` turned plan
    /// mode off in the gate. The engine records and announces it.
    PlanApproved(SessionId),
    /// Test-only: report the hosted session ids.
    #[cfg(test)]
    HostedSessions(crossbeam_channel::Sender<Vec<(SessionId, Option<usize>)>>),
}

/// One verified resume target, from its session marker.
pub(super) struct ResumeChild {
    pub id: SessionId,
    /// The item the child was first spawned for.
    pub item: String,
    /// The agent definition the child runs.
    pub agent: String,
    /// The description its first spawn showed.
    pub description: String,
}

/// A request from a `swarm` call to re-prompt an existing child
/// session with a follow-up.
pub(super) struct Attach {
    pub parent: SessionId,
    pub id: SessionId,
    /// The agent definition the child originally ran.
    pub agent: String,
    /// The description its first spawn showed.
    pub description: String,
    /// The resume call's batch, stamped on the link.
    pub batch_id: ToolCallId,
    pub prompt: String,
    pub reply: crossbeam_channel::Sender<ToolOutput>,
    /// Batch membership for the resumed child, so its card keeps its
    /// place in the new batch.
    pub swarm: Option<SwarmInfo>,
}

impl Engine {
    pub(crate) fn start(registry: Arc<ProviderRegistry>) -> Self {
        let (tx, rx) = mpsc::channel();
        let bus = Arc::new(Bus::new());
        let dispatcher = Dispatcher {
            bus: Arc::clone(&bus),
            registry,
            sessions: HashMap::new(),
            active: None,
            tx: tx.clone(),
            asks: Arc::default(),
            next_request: Arc::default(),
            waiting: VecDeque::new(),
            shutting_down: false,
            engine_shutdown: Arc::new(AtomicBool::new(false)),
            watchdogs: HashMap::new(),
            swarm_requeues: HashMap::new(),
        };
        let thread = thread::spawn(move || dispatcher.run(&rx));
        Self {
            commander: Commander(tx),
            bus,
            thread: Some(thread),
        }
    }

    /// Deliver every event published from now on to `subscriber`, and
    /// return the id [`Engine::unsubscribe`] takes.
    pub(crate) fn subscribe(&self, subscriber: Subscriber) -> SubscriptionId {
        self.bus.subscribe(subscriber)
    }

    /// Stop delivering events to the subscription `id`. Must not be
    /// called from inside a subscriber: `publish` holds the bus lock
    /// while it runs subscribers, so that deadlocks.
    pub(crate) fn unsubscribe(&self, id: SubscriptionId) {
        self.bus.unsubscribe(id);
    }

    /// Run `f` while the bus is locked, so no envelope is published
    /// during it: a publish from another thread waits until `f`
    /// returns. Like a subscriber, `f` must not publish, subscribe, or
    /// unsubscribe, or it deadlocks on the same lock.
    pub(crate) fn hold_events<R>(&self, f: impl FnOnce() -> R) -> R {
        self.bus.hold(f)
    }

    pub(crate) fn commander(&self) -> Commander {
        self.commander.clone()
    }

    /// Host `spec`. The first session opened becomes the active one.
    pub(crate) fn open(&self, spec: SessionSpec) {
        let _ = self.commander.0.send(Input::Open(Box::new(spec)));
    }

    pub(crate) fn send(&self, command: Command) {
        self.commander.send(command);
    }

    /// Test-only: the hosted sessions with the length of each idle
    /// context history, `None` while a run owns the context.
    #[cfg(test)]
    pub(crate) fn hosted_sessions(&self) -> Vec<(SessionId, Option<usize>)> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = self.commander.0.send(Input::HostedSessions(tx));
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default()
    }

    /// Cancel every run and wait for the engine to stop.
    pub(crate) fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.send(Command::active(CommandKind::Shutdown));
        let _ = thread.join();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent per-session flags"
)]
struct Session {
    idle: Option<Idle>,
    state: SessionState,
    /// A thinking level chosen during a run, recorded at the next run
    /// start.
    thinking_changed: bool,
    /// A model switched to during a run, recorded at the next run start.
    model_changed: bool,
    usage: Usage,
    cancel: CancelFlag,
    steering: Steering,
    queued: VecDeque<Vec<Content>>,
    tools: ToolRegistry,
    plugins: Option<Arc<PluginRuntime>>,
    gate: PermissionGate,
    loop_cfg: LoopConfig,
    /// `None` while a run start, an idle restart or the first bring-up
    /// of unstarted servers holds it.
    mcp: Option<McpManager>,
    /// `RestartMcp` names that wait for the next run start.
    mcp_restarts: Vec<String>,
    interactive: bool,
    /// Session file, when the session is recorded.
    path: Option<PathBuf>,
    workdir: PathBuf,
    /// Messages to add to history once the session is idle, such as
    /// shell output that arrived while a run was in flight.
    pending_history: Vec<Message>,
    /// Custom entries to append once the recorder is back, so a swarm
    /// mode toggle during a run is still persisted.
    pending_entries: Vec<kage_session::Custom>,
    /// User shell commands still running.
    shells: usize,
    /// Program shell commands run with (`[shell] program`).
    shell: Option<String>,
    title: bool,
    title_pending: bool,
    /// A generated title that arrived while a run or an idle restart held
    /// the recorder, written when the recorder comes back.
    late_title: Option<String>,
    plugin_tools: PluginTools,
    agents: Option<AgentSetup>,
    /// Present on sessions an `agent` call started.
    link: Option<AgentLink>,
    /// Read at spawn, while the context is out with a run.
    confine_paths: bool,
    /// Whether the session delegates repeated work through `swarm`.
    /// Drives the workflow block injected once per state change.
    swarm_mode: bool,
}

impl Session {
    /// Copy the swarm and plan modes and the background shell count
    /// into the state snapshot clients see, right before one is
    /// published.
    fn sync_state(&mut self) {
        self.state.swarm = self.swarm_mode;
        self.state.plan = self.gate.plan();
        self.state.shells = u32::try_from(self.shells).unwrap_or(u32::MAX);
    }
}

/// What a session holds while no run owns it.
struct Idle {
    cx: AgentContext,
    recorder: Option<Recorder>,
}

/// Open permission requests with the session that asked.
type Asks =
    Arc<Mutex<HashMap<RequestId, (SessionId, crossbeam_channel::Sender<PermissionDecision>)>>>;

struct Dispatcher {
    bus: Arc<Bus>,
    registry: Arc<ProviderRegistry>,
    sessions: HashMap<SessionId, Session>,
    active: Option<SessionId>,
    tx: mpsc::Sender<Input>,
    asks: Asks,
    next_request: Arc<AtomicU64>,
    /// Agents over the running limit, in spawn order.
    waiting: VecDeque<SessionId>,
    shutting_down: bool,
    /// Shared with the swarm timeout watchdogs so they stop polling
    /// once the engine is shutting down.
    engine_shutdown: Arc<AtomicBool>,
    /// Per running swarm member, the flag its watchdog polls. Set
    /// false when the run ends, so a late fire cannot cancel the
    /// member's next run.
    watchdogs: HashMap<SessionId, Arc<AtomicBool>>,
    /// Swarm children requeued after a rate limit: attempts so far
    /// and when their shared timeout budget started.
    swarm_requeues: HashMap<SessionId, RequeueState>,
}

impl Dispatcher {
    fn run(mut self, rx: &mpsc::Receiver<Input>) {
        while let Ok(input) = rx.recv() {
            match input {
                Input::Command(command) => self.command(command),
                Input::Open(spec) => self.open(*spec, CancelFlag::new(), None),
                Input::Spawn(spawn) => self.spawn(*spawn),
                Input::VerifyResume { parent, ids, reply } => {
                    let _ = reply.send(self.verify_resume(parent, &ids));
                }
                Input::Attach(attach) => self.attach(*attach),
                Input::Deliver {
                    from,
                    to,
                    message,
                    reply,
                } => {
                    let _ = reply.send(self.deliver_message(from, to, &message));
                }
                Input::Finished(finished) => self.finish(*finished),
                Input::RequeueChild { id } => self.requeue_child(id),
                #[cfg(test)]
                Input::HostedSessions(reply) => {
                    let _ = reply.send(
                        self.sessions
                            .iter()
                            .map(|(id, s)| (*id, s.idle.as_ref().map(|idle| idle.cx.history.len())))
                            .collect(),
                    );
                }
                Input::McpDone(done) => self.mcp_done(*done),
                Input::ShellDone(done) => self.shell_done(*done),
                Input::Title { session, title } => self.record_title(session, title),
                Input::GoalMet { session, goal } => notice(
                    &self.bus,
                    session,
                    NoticeLevel::Success,
                    format!("goal met: {goal}"),
                ),
                Input::Publish(event) => {
                    if let Some(id) = self.active {
                        self.bus.publish(id, event);
                    }
                }
                Input::SetRegistry(registry) => self.registry = registry,
                Input::ReloadPluginTools => {
                    let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
                    for id in ids {
                        self.apply_plugin_tools(id);
                    }
                }
                Input::PlanApproved(id) => self.plan_mode_changed(id, false, false),
            }
            if self.shutting_down
                && self
                    .sessions
                    .values()
                    .all(|s| s.idle.is_some() && s.shells == 0)
            {
                return;
            }
        }
    }

    fn open(&mut self, spec: SessionSpec, cancel: CancelFlag, link: Option<AgentLink>) {
        let SessionSpec {
            id,
            model,
            cx,
            recorder,
            tools,
            plugins,
            gate,
            loop_cfg,
            mcp,
            interactive,
            title,
            agents,
            shell,
        } = spec;
        let usage = usage_of(&cx);
        let mut state = SessionState {
            model,
            thinking: cx.thinking_level,
            permission_mode: gate.mode(),
            ..SessionState::default()
        };
        fit_to_model(&mut state, &self.registry);
        self.bus.publish(
            id,
            HostEvent::StateChanged {
                state: state.clone(),
            },
        );
        self.bus.publish(id, HostEvent::UsageUpdated { usage });
        if link.is_none() {
            let servers = mcp.as_ref().map(McpManager::catalog).unwrap_or_default();
            self.bus.publish(id, HostEvent::McpServers { servers });
        }
        let path = recorder.as_ref().map(|r| r.path().to_path_buf());
        let workdir = cx.workdir.clone();
        let confine_paths = cx.confine_paths;
        let title_pending = title && !has_reply(&cx);
        let starting_mcp = mcp
            .as_ref()
            .is_some_and(|m| m.starting_names().next().is_some());
        self.sessions.insert(
            id,
            Session {
                idle: Some(Idle { cx, recorder }),
                state,
                thinking_changed: false,
                model_changed: false,
                usage,
                cancel,
                steering: Arc::default(),
                queued: VecDeque::new(),
                tools,
                plugins,
                gate,
                loop_cfg,
                mcp,
                mcp_restarts: Vec::new(),
                interactive,
                shell,
                path,
                workdir,
                pending_history: Vec::new(),
                pending_entries: Vec::new(),
                shells: 0,
                title,
                title_pending,
                late_title: None,
                plugin_tools: PluginTools::default(),
                agents,
                link,
                confine_paths,
                swarm_mode: false,
            },
        );
        self.active.get_or_insert(id);
        self.apply_plugin_tools(id);
        if starting_mcp {
            self.start_mcp(id);
        }
    }

    /// Register the session's plugin tools, replacing earlier ones.
    fn apply_plugin_tools(&mut self, id: SessionId) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let Some(rt) = session.plugins.clone() else {
            return;
        };
        for name in session.plugin_tools.apply(&mut session.tools, &rt) {
            notice(
                &self.bus,
                id,
                NoticeLevel::Warning,
                format!(
                    "override_tool: no tool named `{name}` to override; treating as new registration"
                ),
            );
        }
    }

    fn command(&mut self, command: Command) {
        match command.kind {
            CommandKind::Shutdown => {
                self.shutting_down = true;
                self.engine_shutdown.store(true, Ordering::Relaxed);
                for session in self.sessions.values() {
                    session.cancel.cancel();
                }
                while let Some(&id) = self.waiting.front() {
                    self.end_waiting(id);
                }
                return;
            }
            CommandKind::ResolvePermission {
                request_id,
                decision,
            } => {
                self.resolve_permission(command.session, request_id, decision);
                return;
            }
            _ => {}
        }
        let Some(id) = command.session.or(self.active) else {
            return;
        };
        if !self.sessions.contains_key(&id) {
            notice(
                &self.bus,
                id,
                NoticeLevel::Error,
                format!("unknown session {id}"),
            );
            return;
        }
        match command.kind {
            CommandKind::Prompt { content, delivery } => self.prompt(id, content, delivery),
            CommandKind::WithdrawPrompt { delivery } => self.withdraw_prompt(id, delivery),
            CommandKind::Cancel if self.waiting.contains(&id) => self.end_waiting(id),
            CommandKind::Cancel => self.sessions[&id].cancel.cancel(),
            CommandKind::Compact => {
                if self.ensure_idle(id, "compact") {
                    self.start_run(id, Work::Compact);
                }
            }
            CommandKind::Shell { command } => self.shell(id, command),
            CommandKind::NewSession => self.new_session(id),
            CommandKind::LoadSession { path } => self.load_session(id, &path),
            CommandKind::Fork { at, switch } => self.fork(id, at.as_deref(), switch),
            CommandKind::ForkFile { path } => self.fork_file(id, &path),
            CommandKind::Clone => self.clone_session(id),
            CommandKind::DeleteSession { path } => self.delete_session(id, &path),
            CommandKind::Export { path } => self.export(id, path),
            CommandKind::SetModel { model } => self.set_model(id, model),
            CommandKind::SetThinking { level } => self.set_thinking(id, level),
            CommandKind::SetPermissionMode { mode } => self.update_state(id, |s| {
                s.gate.set_mode(mode);
                s.state.permission_mode = mode;
            }),
            CommandKind::RestartMcp { server } => self.restart_mcp(id, server),
            CommandKind::Close => self.close(id),
            CommandKind::SwarmMode { on } => self.set_swarm_mode(id, on),
            CommandKind::SetGoal { goal } => self.set_goal(id, goal),
            CommandKind::SwarmResume { members } => self.resume_members(id, &members),
            CommandKind::PlanMode { on } => {
                let session = self.sessions.get_mut(&id).expect("session checked");
                if session.gate.plan() != on {
                    session.gate.set_plan(on);
                    self.plan_mode_changed(id, on, true);
                }
            }
            CommandKind::Shutdown | CommandKind::ResolvePermission { .. } => {}
        }
    }

    /// Record and apply a thinking level now when idle, or at the next
    /// run start otherwise. `None` returns to the automatic level.
    fn set_thinking(&mut self, id: SessionId, level: Option<ThinkingLevel>) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        session.state.thinking = level;
        fit_to_model(&mut session.state, &self.registry);
        match session.idle.as_mut() {
            Some(idle) => {
                idle.cx.thinking_level = level;
                if let Some(recorder) = idle.recorder.as_mut() {
                    report_write(&self.bus, id, recorder.append(&thinking_entry(level)));
                }
            }
            None => session.thinking_changed = true,
        }
        let state = session.state.clone();
        self.bus.publish(id, HostEvent::StateChanged { state });
    }

    /// Switch models and record the switch now when idle, or at the next
    /// run start otherwise.
    fn set_model(&mut self, id: SessionId, model: String) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        if session.state.model != model {
            session.state.model = model;
            fit_to_model(&mut session.state, &self.registry);
            match session.idle.as_mut() {
                Some(idle) => {
                    if let Some(recorder) = idle.recorder.as_mut() {
                        report_write(&self.bus, id, recorder.set_model(&session.state.model));
                    }
                }
                None => session.model_changed = true,
            }
        }
        let state = session.state.clone();
        self.bus.publish(id, HostEvent::StateChanged { state });
    }

    /// Turn the session's swarm mode on or off. Only a change injects
    /// the workflow block or the exit note, as a user message that
    /// lands in the history like shell output does, plus a custom
    /// entry that persists the state across restarts.
    fn set_swarm_mode(&mut self, id: SessionId, on: bool) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        if session.swarm_mode == on {
            return;
        }
        session.swarm_mode = on;
        session.sync_state();
        let state = session.state.clone();
        self.bus.publish(id, HostEvent::StateChanged { state });
        let text = if on {
            swarm_tool::SWARM_MODE_ON.to_owned()
        } else {
            swarm_tool::SWARM_MODE_OFF.to_owned()
        };
        session
            .pending_history
            .push(Message::new(Role::User, vec![Content::Text { text }], None));
        record_mode(
            &self.bus,
            id,
            session,
            kage_session::list::SWARM_MODE_ENTRY_KIND,
            on,
        );
        notice(
            &self.bus,
            id,
            NoticeLevel::Info,
            format!("swarm mode {}", if on { "on" } else { "off" }),
        );
    }

    /// Set the goal the session works toward, or clear it. Publishing
    /// the state is all a change needs: the check runs at the end of
    /// every completed turn while a goal is set.
    fn set_goal(&mut self, id: SessionId, goal: Option<String>) {
        let goal = goal.filter(|g| !g.trim().is_empty());
        self.update_state(id, |s| {
            s.state.goal = goal;
        });
    }

    /// Announce and persist a plan mode change the gate already holds.
    /// `remind` injects the reminder for the model as a user message;
    /// an approved plan skips it, since `exit_plan`'s result tells the
    /// model instead.
    fn plan_mode_changed(&mut self, id: SessionId, on: bool, remind: bool) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        session.sync_state();
        let state = session.state.clone();
        self.bus.publish(id, HostEvent::StateChanged { state });
        if remind {
            let text = if on {
                plan_tool::PLAN_MODE_ON
            } else {
                plan_tool::PLAN_MODE_OFF
            };
            session.pending_history.push(Message::new(
                Role::User,
                vec![Content::Text {
                    text: text.to_owned(),
                }],
                None,
            ));
        }
        record_mode(
            &self.bus,
            id,
            session,
            kage_session::list::PLAN_MODE_ENTRY_KIND,
            on,
        );
        let text = match (on, remind) {
            (true, _) => "plan mode on: nothing changes until you approve a plan",
            (false, true) => "plan mode off",
            (false, false) => "plan approved; plan mode off",
        };
        notice(&self.bus, id, NoticeLevel::Info, text.to_owned());
    }

    fn record_title(&mut self, id: SessionId, title: String) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        match session.idle.as_mut() {
            Some(idle) => {
                if let Some(recorder) = idle.recorder.as_mut() {
                    report_write(&self.bus, id, recorder.append(&title_entry(title.clone())));
                }
            }
            None => session.late_title = Some(title.clone()),
        }
        self.bus.publish(id, HostEvent::TitleChanged { title });
    }

    fn resolve_permission(
        &self,
        session: Option<SessionId>,
        request_id: RequestId,
        decision: PermissionDecision,
    ) {
        let asker = lock(&self.asks).remove(&request_id).map(|(asker, reply)| {
            let _ = reply.send(decision);
            asker
        });
        if let Some(id) = asker.or(session).or(self.active) {
            self.bus
                .publish(id, HostEvent::PermissionResolved { request_id });
        }
    }

    fn update_state(&mut self, id: SessionId, apply: impl FnOnce(&mut Session)) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        apply(session);
        let state = session.state.clone();
        self.bus.publish(id, HostEvent::StateChanged { state });
    }

    fn prompt(&mut self, id: SessionId, mut content: Vec<Content>, delivery: Delivery) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        let is_image = |c: &Content| matches!(c, Content::Image { .. });
        if session.state.input.lacks(kage_core::Input::Image) && content.iter().any(is_image) {
            content.retain(|c| !is_image(c));
            let model = &session.state.model;
            let text = if content.is_empty() {
                format!("{model} does not accept images; nothing to send")
            } else {
                format!("{model} does not accept images; sent the prompt without them")
            };
            notice(&self.bus, id, NoticeLevel::Warning, text.clone());
            if content.is_empty() {
                self.bus.publish(
                    id,
                    HostEvent::RunEnded {
                        outcome: RunOutcome::Failed {
                            error: LoopError::InvalidPrompt { message: text },
                        },
                    },
                );
                return;
            }
        }
        if session.idle.is_some() && !self.waiting.contains(&id) {
            let prompt = Message::new(Role::User, content, None);
            self.start_run(id, Work::Prompt(prompt));
            return;
        }
        match (delivery, text_only(&content)) {
            (Delivery::Steer, Some(text)) => lock(&session.steering).push_back(text),
            _ => session.queued.push_back(content),
        }
    }

    /// Take the newest pending prompt for `delivery` back out of `id`'s
    /// queues and report it, so the sender can edit and resubmit it.
    /// `content` is `None` when that queue held nothing.
    fn withdraw_prompt(&mut self, id: SessionId, delivery: Delivery) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        let content = match delivery {
            Delivery::Steer => lock(&session.steering)
                .pop_back()
                .map(|text| vec![Content::Text { text }]),
            Delivery::Queue => session.queued.pop_back(),
        };
        self.bus
            .publish(id, HostEvent::PromptWithdrawn { delivery, content });
    }

    fn start_run(&mut self, id: SessionId, work: Work) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        let model = session.state.model.clone();
        let (provider, bare_model) = match self.registry.resolve(&model) {
            Ok(resolved) => (Arc::clone(resolved.provider), resolved.model),
            Err(err) => {
                let message = format!("model {model} unavailable: {err}");
                notice(&self.bus, id, NoticeLevel::Error, message.clone());
                let outcome = RunOutcome::Failed {
                    error: LoopError::Provider { message },
                };
                self.bus.publish(
                    id,
                    HostEvent::RunEnded {
                        outcome: outcome.clone(),
                    },
                );
                self.deliver(id, &outcome, &[], Usage::default(), Duration::ZERO);
                return;
            }
        };
        flush_pending(&self.bus, id, session);
        let Some(Idle {
            mut cx,
            mut recorder,
        }) = session.idle.take()
        else {
            return;
        };
        if let Some(path) = session.link.as_mut().and_then(|l| l.lazy_history.take()) {
            apply_forked_snapshot(&self.bus, id, &path, &mut cx);
        }
        cx.model = bare_model;
        fit_context(&mut cx, &self.registry, &model);
        if std::mem::take(&mut session.thinking_changed) {
            cx.thinking_level = session.state.thinking;
            if let Some(recorder) = recorder.as_mut() {
                report_write(
                    &self.bus,
                    id,
                    recorder.append(&thinking_entry(cx.thinking_level)),
                );
            }
        }
        if std::mem::take(&mut session.model_changed)
            && let Some(recorder) = recorder.as_mut()
        {
            report_write(&self.bus, id, recorder.set_model(&model));
        }
        let mcp = if let Some(manager) = session.mcp.take() {
            let mut restarts = session
                .plugins
                .as_ref()
                .map(|rt| rt.take_mcp_restarts())
                .unwrap_or_default();
            restarts.append(&mut session.mcp_restarts);
            Some(McpLease { manager, restarts })
        } else {
            for name in session.mcp_restarts.drain(..) {
                restart_failed(&self.bus, id, &name, &McpError::Unknown(name.clone()));
            }
            None
        };

        session.usage.context_window = cx.context_window;
        session.cancel.reset();
        session.state.working = true;
        let mut gate = session.gate.clone().with_cancel(session.cancel.clone());
        if session.interactive {
            gate = gate.with_asker(asker(&self.bus, &self.asks, &self.next_request, id));
        }
        let work = match work {
            Work::Prompt(prompt) => Work::Prompt(Message {
                parent: cx.history.last().map(|m| m.id),
                ..prompt
            }),
            Work::Compact => Work::Compact,
        };
        let tools = run_tools(session, id, &self.tx);
        let run = Run {
            session: id,
            work,
            provider,
            tools,
            cx,
            recorder,
            usage: session.usage,
            loop_cfg: session.loop_cfg,
            cancel: session.cancel.clone(),
            gate,
            steering: Arc::clone(&session.steering),
            plugins: session.plugins.clone(),
            mcp,
            bus: Arc::clone(&self.bus),
        };
        let state = session.state.clone();
        self.bus.publish(id, HostEvent::StateChanged { state });
        self.arm_swarm_watchdog(id);
        run.spawn(self.tx.clone());
    }

    /// Cancel a running swarm member when its per-run deadline of
    /// `swarm_timeout_ms` passes. Armed at run start and disarmed at
    /// run end, so children queued behind the running limit burn none
    /// of their budget and a late fire cannot cancel the member's next
    /// run. Deadline and user cancellation both surface as
    /// `Cancelled`.
    fn arm_swarm_watchdog(&mut self, id: SessionId) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        let is_swarm_member = session
            .link
            .as_ref()
            .is_some_and(|link| link.batch_id.is_some());
        let timeout = if is_swarm_member {
            session.agents.as_ref().map(|setup| {
                let budget = Duration::from_millis(setup.swarm_timeout_ms);
                self.swarm_requeues
                    .get(&id)
                    .map_or(budget, |state| budget.saturating_sub(state.since.elapsed()))
            })
        } else {
            None
        };
        let Some(timeout) = timeout else {
            return;
        };
        let armed = Arc::new(AtomicBool::new(true));
        if let Some(previous) = self.watchdogs.insert(id, Arc::clone(&armed)) {
            previous.store(false, Ordering::Relaxed);
        }
        let cancel = session.cancel.clone();
        let shutdown = Arc::clone(&self.engine_shutdown);
        thread::spawn(move || {
            let mut elapsed = Duration::ZERO;
            while elapsed < timeout {
                if shutdown.load(Ordering::Relaxed) || !armed.load(Ordering::Relaxed) {
                    return;
                }
                let step = timeout
                    .checked_sub(elapsed)
                    .map_or(WATCHDOG_SLICE, |rest| WATCHDOG_SLICE.min(rest));
                thread::sleep(step);
                elapsed += step;
            }
            if armed.load(Ordering::Relaxed) && !shutdown.load(Ordering::Relaxed) {
                cancel.cancel();
            }
        });
    }

    fn finish(&mut self, finished: Finished) {
        let Finished {
            session: id,
            mut cx,
            mut recorder,
            usage,
            outcome,
            run_time,
        } = finished;
        if let Some(armed) = self.watchdogs.remove(&id) {
            armed.store(false, Ordering::Relaxed);
        }
        // Read before the session borrow goes out for the bookkeeping
        // below; the check itself runs after it.
        let goal = (outcome == RunOutcome::Completed)
            .then(|| {
                self.sessions
                    .get(&id)
                    .and_then(|s| s.state.goal.clone())
                    .filter(|g| !g.trim().is_empty())
            })
            .flatten();
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        append_history(
            &self.bus,
            id,
            &mut cx,
            recorder.as_mut(),
            session.pending_history.drain(..),
        );
        flush_entries(
            &self.bus,
            id,
            recorder.as_mut(),
            session.pending_entries.drain(..),
        );
        if outcome == RunOutcome::Completed && session.title_pending {
            session.title_pending = false;
            let model = session.state.model.clone();
            self.generate_title(id, &cx, &model);
        }
        if let Some(goal) = goal {
            let model = self.sessions[&id].state.model.clone();
            self.check_goal(id, &cx, &model, &goal);
        }
        let requeued = self.requeue_rate_limited(id, &outcome);
        let reply = if requeued {
            None
        } else {
            // The runner's totals carry the price-adjusted cost and
            // the context fill, unlike the budget's raw counters.
            self.take_reply(id, &outcome, &cx.history, usage, run_time)
        };
        // Children may not have seen the cancel yet, and this session's
        // own flag resets below, so they get their own.
        if outcome == RunOutcome::Cancelled {
            for child in self.sessions.values() {
                if child.link.as_ref().is_some_and(|l| l.parent == id) && child.idle.is_none() {
                    child.cancel.cancel();
                }
            }
        }
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        session.idle = Some(Idle { cx, recorder });
        record_late_title(&self.bus, id, session);
        session.usage = usage;
        session.state.working = session.shells > 0;
        // A set flag on an idle session would cancel any run its agents
        // start, through the tree.
        session.cancel.reset();
        let leftover: Vec<String> = lock(&session.steering).drain(..).collect();
        for text in leftover.into_iter().rev() {
            session.queued.push_front(vec![Content::Text { text }]);
        }
        session.sync_state();
        let state = session.state.clone();
        let next = if self.shutting_down {
            None
        } else {
            session.queued.pop_front()
        };
        self.bus.publish(id, HostEvent::RunEnded { outcome });
        self.deny_asks_of(id);
        self.bus.publish(id, HostEvent::StateChanged { state });
        if let Some((reply, output)) = reply {
            let _ = reply.send(output);
        }
        if let Some(content) = next {
            self.start_run(id, Work::Prompt(Message::new(Role::User, content, None)));
        }
        let orphans: Vec<SessionId> = self
            .waiting
            .iter()
            .copied()
            .filter(|w| self.parent_of(*w) == Some(id))
            .collect();
        for orphan in orphans {
            self.end_waiting(orphan);
        }
        self.start_waiting();
        self.reap_swarm_child(id);
    }

    /// Deny every permission ask the ended run of `session` still has
    /// parked. A run that ends mid-ask, such as a cancelled child,
    /// otherwise leaves the ask and its reply channel in `asks`
    /// forever, because only [`Self::resolve_permission`] removes
    /// entries.
    fn deny_asks_of(&self, session: SessionId) {
        let stale: Vec<RequestId> = lock(&self.asks)
            .iter()
            .filter_map(|(id, (asker, _))| (*asker == session).then_some(*id))
            .collect();
        for request_id in stale {
            self.resolve_permission(Some(session), request_id, PermissionDecision::Deny);
        }
    }

    /// Drop a finished swarm child whose result was already delivered.
    ///
    /// The child's transcript stays in its session file and a later
    /// `swarm resume` reopens the file (`attach`), so hosting the idle
    /// session only pins its whole history in RAM for the engine's
    /// life. Plain `agent` children stay hosted: they can still be
    /// re-prompted in place, and their own agents keep finding them.
    fn reap_swarm_child(&mut self, id: SessionId) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        let delivered = session
            .link
            .as_ref()
            .is_some_and(|link| link.batch_id.is_some() && link.reply.is_none());
        let quiet = session.idle.is_some()
            && session.queued.is_empty()
            && session.pending_history.is_empty()
            && session.pending_entries.is_empty()
            && session.shells == 0
            && session.late_title.is_none()
            && !self.swarm_requeues.contains_key(&id)
            && !self.waiting.contains(&id)
            && !self
                .sessions
                .values()
                .any(|s| s.idle.is_none() && s.link.as_ref().is_some_and(|l| l.parent == id));
        if !(delivered && quiet) {
            return;
        }
        self.deny_asks_of(id);
        self.watchdogs.remove(&id);
        self.swarm_requeues.remove(&id);
        self.sessions.remove(&id);
    }

    /// Re-prompt a swarm child whose run failed on a rate limit, or
    /// report why not. The child's reply stays pending, so the swarm
    /// call keeps waiting; a timer re-prompts it after a backoff.
    /// Returns false when the child is out of requeues or out of
    /// budget, leaving `finish` to deliver the failure.
    fn requeue_rate_limited(&mut self, id: SessionId, outcome: &RunOutcome) -> bool {
        let RunOutcome::Failed {
            error: LoopError::RateLimited {
                retry_after_secs, ..
            },
        } = outcome
        else {
            return false;
        };
        if self.shutting_down {
            return false;
        }
        let Some(session) = self.sessions.get(&id) else {
            return false;
        };
        let is_pending_swarm_child = session
            .link
            .as_ref()
            .is_some_and(|link| link.batch_id.is_some() && link.reply.is_some());
        if !is_pending_swarm_child {
            return false;
        }
        let budget = session.agents.as_ref().map_or(Duration::ZERO, |setup| {
            Duration::from_millis(setup.swarm_timeout_ms)
        });
        let attempts = self
            .swarm_requeues
            .get(&id)
            .map_or(0, |state| state.attempts);
        let since = self
            .swarm_requeues
            .get(&id)
            .map_or(Instant::now(), |state| state.since);
        let Some(backoff) = requeue_backoff(attempts, since.elapsed(), budget, *retry_after_secs)
        else {
            return false;
        };
        self.swarm_requeues.insert(
            id,
            RequeueState {
                attempts: attempts + 1,
                since,
            },
        );
        let reason = format!(
            "rate limited; retrying in {}s (attempt {} of {MAX_REQUEUES})",
            backoff.as_secs().max(1),
            attempts + 1
        );
        notice(&self.bus, id, NoticeLevel::Warning, reason.clone());
        self.bus.publish(id, HostEvent::AgentPaused { reason });
        let engine = self.tx.clone();
        let shutdown = Arc::clone(&self.engine_shutdown);
        thread::spawn(move || {
            let mut left = backoff;
            while !left.is_zero() {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                let step = WATCHDOG_SLICE.min(left);
                thread::sleep(step);
                left = left.saturating_sub(step);
            }
            let _ = engine.send(Input::RequeueChild { id });
        });
        true
    }

    /// Answer a requeue timer: prompt the child to continue its task.
    /// If the child is busy or has queued work, the prompt waits in
    /// line like any other.
    fn requeue_child(&mut self, id: SessionId) {
        if self.shutting_down {
            return;
        }
        let pending = self
            .sessions
            .get(&id)
            .and_then(|s| s.link.as_ref())
            .is_some_and(|link| link.batch_id.is_some() && link.reply.is_some());
        if !pending {
            self.swarm_requeues.remove(&id);
            return;
        }
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let idle = session.idle.is_some()
            && session.queued.is_empty()
            && lock(&session.steering).is_empty();
        let content = vec![Content::Text {
            text: CONTINUE_PROMPT.to_owned(),
        }];
        if idle {
            self.start_run(id, Work::Prompt(Message::new(Role::User, content, None)));
        } else {
            session.queued.push_back(content);
        }
    }

    /// Ask the model for a short title for the session's first exchange,
    /// off the dispatcher thread.
    /// Ask the model, off the run thread, whether the finished turn
    /// met the session's goal, and publish a success notice when it
    /// did. A failed or unreadable check stays silent.
    fn check_goal(&self, id: SessionId, cx: &AgentContext, model: &str, goal: &str) {
        let Ok(resolved) = self.registry.resolve(model) else {
            return;
        };
        let provider = Arc::clone(resolved.provider);
        let bare_model = resolved.model;
        let last_text = |role: Role| {
            cx.history
                .iter()
                .rev()
                .find(|m| m.role == role)
                .map(|m| crate::cli_loop_run::first_user_text(m))
                .unwrap_or_default()
        };
        let (user, reply) = (last_text(Role::User), last_text(Role::Assistant));
        let tx = self.tx.clone();
        let goal = goal.to_owned();
        thread::spawn(move || {
            if crate::goal::met(
                provider.as_ref(),
                &bare_model,
                &goal,
                &user,
                &reply,
                &CancelFlag::new(),
            ) {
                let _ = tx.send(Input::GoalMet { session: id, goal });
            }
        });
    }

    fn generate_title(&self, id: SessionId, cx: &AgentContext, model: &str) {
        let Ok(resolved) = self.registry.resolve(model) else {
            return;
        };
        let provider = Arc::clone(resolved.provider);
        let bare_model = resolved.model;
        let first_text = |role: Role| {
            cx.history
                .iter()
                .find(|m| m.role == role)
                .map(|m| crate::cli_loop_run::first_user_text(m))
                .unwrap_or_default()
        };
        let (user, reply) = (first_text(Role::User), first_text(Role::Assistant));
        let tx = self.tx.clone();
        thread::spawn(move || {
            let title = crate::title::generate(
                provider.as_ref(),
                &bare_model,
                &user,
                &reply,
                &CancelFlag::new(),
            );
            let _ = tx.send(Input::Title { session: id, title });
        });
    }
}

/// The tools one run of `session` may call: its own, the delegation
/// and mailbox tools its agent setup allows, and `exit_plan` in plan
/// mode. Records their risks in the gate for plan mode to judge.
fn run_tools(session: &Session, id: SessionId, tx: &mpsc::Sender<Input>) -> ToolRegistry {
    let mut tools = session.tools.clone();
    if let Some(setup) = &session.agents {
        if depth_of(session) < setup.max_depth {
            register_delegation_tools(&mut tools, id, tx, setup);
        }
        // Mailboxing does not nest, so every agent-enabled session
        // gets it whatever its depth.
        tools.register(Arc::new(MailboxTool::new(id, tx.clone())));
    }
    if session.gate.plan() {
        tools.register(Arc::new(ExitPlanTool::new(
            id,
            tx.clone(),
            session.gate.clone(),
        )));
    }
    session.gate.set_risks(
        tools
            .names()
            .filter_map(|name| Some((name.to_owned(), tools.get(name)?.risk())))
            .collect(),
    );
    tools
}

/// Register the delegation tools a session may call while its depth
/// is under the limit: the single `agent` tool and the `swarm` tool.
fn register_delegation_tools(
    tools: &mut ToolRegistry,
    id: SessionId,
    tx: &mpsc::Sender<Input>,
    setup: &AgentSetup,
) {
    tools.register(Arc::new(AgentTool::new(id, tx.clone(), &setup.defs)));
    tools.register(Arc::new(SwarmTool::new(
        id,
        tx.clone(),
        &setup.defs,
        setup.swarm_max_items,
        Duration::from_millis(setup.swarm_timeout_ms),
    )));
}

/// Route a run's permission questions onto the bus. The answer arrives
/// through [`CommandKind::ResolvePermission`].
fn asker(bus: &Arc<Bus>, asks: &Asks, next: &Arc<AtomicU64>, session: SessionId) -> Asker {
    let bus = Arc::clone(bus);
    let asks = Arc::clone(asks);
    let next = Arc::clone(next);
    Arc::new(move |prompt: PermissionPrompt| {
        let request_id = RequestId(next.fetch_add(1, Ordering::Relaxed));
        let (reply, answer) = crossbeam_channel::bounded(1);
        lock(&asks).insert(request_id, (session, reply));
        bus.publish(
            session,
            HostEvent::PermissionRequested {
                request_id,
                tool_call_id: Some(prompt.call_id),
                tool: prompt.tool,
                subject: prompt.subject,
                input: prompt.input,
            },
        );
        Some(answer)
    })
}

/// Load a forked child's copied conversation into `cx` at its first
/// run start. The snapshot sits in the child's own session file from
/// spawn on, so a child queued behind the running limit holds
/// kilobytes instead of the whole parent transcript.
fn apply_forked_snapshot(bus: &Bus, id: SessionId, path: &Path, cx: &mut AgentContext) {
    match kage_session::replay(path) {
        Ok(replay) => {
            cx.history = replay.history.into_iter().map(Arc::new).collect();
            cx.budget = kage_loop::TokenBudget {
                used_input: replay.usage_total.input,
                used_output: replay.usage_total.output,
                used_cache_read: replay.usage_total.cache_read,
                used_cache_write: replay.usage_total.cache_write,
                current_context: replay.usage_total.last_context,
            };
        }
        Err(err) => notice(
            bus,
            id,
            NoticeLevel::Warning,
            format!("cannot load the forked conversation: {err}"),
        ),
    }
}

/// Tell the user when writing to the session file failed.
fn report_write(bus: &Bus, id: SessionId, result: Result<(), kage_session::SessionError>) {
    if let Err(err) = result {
        notice(
            bus,
            id,
            NoticeLevel::Error,
            format!("session write failed: {err}"),
        );
    }
}

fn notice(bus: &Bus, id: SessionId, level: NoticeLevel, text: String) {
    bus.publish(
        id,
        HostEvent::Notice {
            level,
            text,
            transient: false,
        },
    );
}

/// Usage totals carried by a context's token budget.
fn usage_of(cx: &AgentContext) -> Usage {
    Usage {
        total: TokenUsage {
            input: cx.budget.used_input,
            output: cx.budget.used_output,
            cache_read: cx.budget.used_cache_read,
            cache_write: cx.budget.used_cache_write,
        },
        context_used: cx.budget.current_context,
        context_window: cx.context_window,
        cost: 0.0,
    }
}

/// Whether the conversation already has an assistant reply.
fn has_reply(cx: &AgentContext) -> bool {
    cx.history.iter().any(|m| m.role == Role::Assistant)
}

/// Clone `cx` with an empty history, for commands that replace the
/// conversation wholesale right after.
fn clone_shallow(cx: &AgentContext) -> AgentContext {
    AgentContext {
        history: Vec::new(),
        model: cx.model.clone(),
        system_prompt: cx.system_prompt.clone(),
        workdir: cx.workdir.clone(),
        context_window: cx.context_window,
        max_output_tokens: cx.max_output_tokens,
        thinking_level: cx.thinking_level,
        reasoning: cx.reasoning,
        confine_paths: cx.confine_paths,
        budget: cx.budget,
    }
}

fn title_entry(title: String) -> kage_session::SessionEntry {
    kage_session::SessionEntry::Title(kage_session::SessionTitle {
        id: kage_session::EntryId::new(),
        ts: chrono::Utc::now(),
        title,
    })
}

/// Write the title that arrived while `session` was busy, now that its
/// recorder is back.
fn record_late_title(bus: &Bus, id: SessionId, session: &mut Session) {
    let Some(title) = session.late_title.take() else {
        return;
    };
    if let Some(recorder) = session.idle.as_mut().and_then(|i| i.recorder.as_mut()) {
        report_write(bus, id, recorder.append(&title_entry(title)));
    }
}

/// Append `messages` to `cx`'s history, each after the one before, and
/// record them.
fn append_history(
    bus: &Bus,
    id: SessionId,
    cx: &mut AgentContext,
    mut recorder: Option<&mut Recorder>,
    messages: impl IntoIterator<Item = Message>,
) {
    for message in messages {
        let message = Message {
            parent: cx.history.last().map(|m| m.id),
            ..message
        };
        if let Some(recorder) = recorder.as_deref_mut() {
            report_write(bus, id, recorder.message(&message));
        }
        cx.history.push(Arc::new(message));
    }
}

/// Append the swarm mode markers that queued while the session was
/// busy. Callers hold the recorder.
fn flush_entries(
    bus: &Bus,
    id: SessionId,
    recorder: Option<&mut Recorder>,
    entries: impl IntoIterator<Item = kage_session::Custom>,
) {
    let Some(recorder) = recorder else {
        return;
    };
    for entry in entries {
        let append = recorder.append(&kage_session::SessionEntry::Custom(entry));
        if let Err(err) = append {
            notice(
                bus,
                id,
                NoticeLevel::Error,
                format!("session write failed: {err}"),
            );
        }
    }
}

/// Move the messages that arrived while `session` was busy into its
/// history, once it is idle.
/// Persist a session mode toggle as a `kind` custom entry: now when
/// the recorder is idle, else once the run hands it back.
fn record_mode(bus: &Bus, id: SessionId, session: &mut Session, kind: &str, on: bool) {
    let entry = kage_session::Custom {
        id: kage_session::EntryId::new(),
        ts: chrono::Utc::now(),
        kind: kind.to_owned(),
        data: serde_json::json!({ "on": on }),
    };
    match session
        .idle
        .as_mut()
        .and_then(|idle| idle.recorder.as_mut())
    {
        Some(recorder) => {
            let append = recorder.append(&kage_session::SessionEntry::Custom(entry));
            if let Err(err) = append {
                notice(
                    bus,
                    id,
                    NoticeLevel::Error,
                    format!("session write failed: {err}"),
                );
            }
        }
        None => session.pending_entries.push(entry),
    }
}

fn flush_pending(bus: &Bus, id: SessionId, session: &mut Session) {
    if let Some(Idle { cx, recorder }) = session.idle.as_mut() {
        append_history(
            bus,
            id,
            cx,
            recorder.as_mut(),
            session.pending_history.drain(..),
        );
        flush_entries(
            bus,
            id,
            recorder.as_mut(),
            session.pending_entries.drain(..),
        );
    }
}

/// Clear the working flag of an idle session once no shell command runs
/// any more, and publish the change.
fn settle_working(bus: &Bus, id: SessionId, session: &mut Session) {
    let working = session.idle.is_none() || session.shells > 0;
    session.state.working = working;
    session.sync_state();
    let state = session.state.clone();
    bus.publish(id, HostEvent::StateChanged { state });
}

/// Session entry recording `level`, written as [`AUTO_THINKING`] when
/// the level is automatic.
fn thinking_entry(level: Option<ThinkingLevel>) -> kage_session::SessionEntry {
    kage_session::SessionEntry::ThinkingLevelChange(kage_session::ThinkingLevelChange {
        id: kage_session::EntryId::new(),
        ts: chrono::Utc::now(),
        level: level
            .map_or(AUTO_THINKING, ThinkingLevel::as_str)
            .to_owned(),
    })
}

/// How the automatic thinking level is named in session entries, the
/// ACP thinking option and the TUI.
/// How often a swarm timeout watchdog rechecks its run-end flag, so a
/// disarmed or shutdown watchdog stops within this slice.
const WATCHDOG_SLICE: Duration = Duration::from_millis(250);

/// Most times a swarm child may be requeued after a rate limit
/// before its next failure is delivered to the swarm call.
const MAX_REQUEUES: u32 = 5;

/// Base of the requeue backoff: 3s doubled per attempt, capped below.
const REQUEUE_BASE: Duration = Duration::from_secs(3);

/// Longest the requeue backoff ever waits, provider hint included.
const REQUEUE_CAP: Duration = Duration::from_secs(60);

/// What a requeued swarm child is prompted with: the failed run left
/// its task in the child's history, so this is enough to go on.
const CONTINUE_PROMPT: &str = "continue";

/// Requeue bookkeeping for a rate-limited swarm child.
struct RequeueState {
    attempts: u32,
    /// When the child's shared timeout budget started counting.
    since: Instant,
}

/// How long a rate-limited swarm child waits before its next try, or
/// `None` when it gets none: it has used [`MAX_REQUEUES`] already or
/// burned its share of the timeout budget. The wait is the provider's
/// hint or the doubled base, whichever is longer, capped by
/// [`REQUEUE_CAP`] and by what is left of the budget.
fn requeue_backoff(
    attempts: u32,
    elapsed: Duration,
    budget: Duration,
    retry_after_secs: Option<u64>,
) -> Option<Duration> {
    if attempts >= MAX_REQUEUES {
        return None;
    }
    // Zero or negative budget is spent: nothing left to wait within.
    let left = budget.checked_sub(elapsed).filter(|left| !left.is_zero())?;
    let hinted = retry_after_secs.map_or(REQUEUE_BASE, |secs| {
        Duration::from_secs(secs).max(REQUEUE_BASE)
    });
    let doubled = REQUEUE_BASE * (1 << attempts);
    Some(hinted.max(doubled).min(REQUEUE_CAP).min(left))
}

pub(crate) const AUTO_THINKING: &str = "default";

/// Set what `cx` takes from `model` (`provider:model`): its prompt
/// window, output cap and thinking settings.
fn fit_context(cx: &mut AgentContext, registry: &ProviderRegistry, model: &str) {
    if let Some(window) = crate::runtime_env::context_window_for(registry, model) {
        cx.context_window = window;
    }
    cx.max_output_tokens = crate::runtime_env::max_output_tokens_for(registry, model);
    cx.reasoning = crate::runtime_env::reasoning_for(registry, model);
}

/// Refresh the parts of `state` that follow its model and chosen
/// thinking level: the level the next run sends, the levels the model
/// accepts, and its inputs.
pub(crate) fn fit_to_model(state: &mut SessionState, registry: &ProviderRegistry) {
    let reasoning = crate::runtime_env::reasoning_for(registry, &state.model);
    state.thinking_effective = reasoning.resolve(state.thinking);
    state.thinking_levels = reasoning.levels();
    state.input = crate::runtime_env::input_for(registry, &state.model);
}

/// The session id encoded in a session file name, `<id>.jsonl`.
pub(crate) fn session_id_of(path: &std::path::Path) -> Option<SessionId> {
    let stem = path.file_stem()?.to_str()?;
    ulid::Ulid::from_string(stem).ok().map(SessionId)
}

/// The prompt's text when it has no other content.
fn text_only(content: &[Content]) -> Option<String> {
    match content {
        [Content::Text { text }] => Some(text.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
