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
mod mcp;
mod plugin_tools;
mod recorder;
mod runner;
mod sessions;
mod shell;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

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
};
use kage_loop::{AgentContext, LoopConfig};
use kage_mcp::{McpError, McpManager};
use kage_plugin::PluginRuntime;
use kage_provider::ProviderRegistry;
use kage_tools::ToolRegistry;

pub(crate) use bus::Subscriber;
pub(crate) use recorder::Recorder;
#[cfg(test)]
pub(crate) use sessions::render_session_markdown;

use agent_tool::{AgentTool, Spawn};
use agents::{AgentLink, depth_of};
use bus::Bus;
use mcp::{McpDone, restart_failed};
use plugin_tools::PluginTools;
use runner::{Finished, McpLease, Run, Steering, Work};
use shell::ShellDone;

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
    /// Generate and record a title after the first completed exchange.
    pub title: bool,
    /// Agent definitions and limits. `None` means no `agent` tool.
    pub agents: Option<AgentSetup>,
}

/// What the `agent` tool may start, shared by a whole session tree.
#[derive(Clone)]
pub(crate) struct AgentSetup {
    pub defs: Arc<AgentDefs>,
    /// How deep agents may nest. 0 turns the `agent` tool off.
    pub max_depth: u8,
    /// How many agents run at once. Further agents wait their turn.
    pub max_running: usize,
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
    Finished(Box<Finished>),
    McpDone(Box<McpDone>),
    ShellDone(Box<ShellDone>),
    Title { session: SessionId, title: String },
    Publish(HostEvent),
    SetRegistry(Arc<ProviderRegistry>),
    ReloadPluginTools,
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
        };
        let thread = thread::spawn(move || dispatcher.run(&rx));
        Self {
            commander: Commander(tx),
            bus,
            thread: Some(thread),
        }
    }

    /// Deliver every event published from now on to `subscriber`.
    pub(crate) fn subscribe(&self, subscriber: Subscriber) {
        self.bus.subscribe(subscriber);
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
    /// `None` while a run start or an idle restart holds it.
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
    /// User shell commands still running.
    shells: usize,
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
}

impl Dispatcher {
    fn run(mut self, rx: &mpsc::Receiver<Input>) {
        while let Ok(input) = rx.recv() {
            match input {
                Input::Command(command) => self.command(command),
                Input::Open(spec) => self.open(*spec, CancelFlag::new(), None),
                Input::Spawn(spawn) => self.spawn(*spawn),
                Input::Finished(finished) => self.finish(*finished),
                Input::McpDone(done) => self.mcp_done(*done),
                Input::ShellDone(done) => self.shell_done(*done),
                Input::Title { session, title } => self.record_title(session, title),
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
                path,
                workdir,
                pending_history: Vec::new(),
                shells: 0,
                title,
                title_pending,
                late_title: None,
                plugin_tools: PluginTools::default(),
                agents,
                link,
                confine_paths,
            },
        );
        self.active.get_or_insert(id);
        self.apply_plugin_tools(id);
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
            notice(&self.bus, id, NoticeLevel::Warning, text);
            if content.is_empty() {
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
                self.deliver(id, &outcome, &[]);
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
        let mut tools = session.tools.clone();
        if let Some(setup) = &session.agents
            && depth_of(session) < setup.max_depth
        {
            tools.register(Arc::new(AgentTool::new(id, self.tx.clone(), &setup.defs)));
        }
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
        run.spawn(self.tx.clone());
    }

    fn finish(&mut self, finished: Finished) {
        let Finished {
            session: id,
            mut cx,
            mut recorder,
            usage,
            outcome,
        } = finished;
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
        if outcome == RunOutcome::Completed && session.title_pending {
            session.title_pending = false;
            let model = session.state.model.clone();
            self.generate_title(id, &cx, &model);
        }
        let reply = self.take_reply(id, &outcome, &cx.history);
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
        let state = session.state.clone();
        let next = if self.shutting_down {
            None
        } else {
            session.queued.pop_front()
        };
        self.bus.publish(id, HostEvent::RunEnded { outcome });
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
    }

    /// Ask the model for a short title for the session's first exchange,
    /// off the dispatcher thread.
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
                .map(crate::cli_loop_run::first_user_text)
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
        cx.history.push(message);
    }
}

/// Move the messages that arrived while `session` was busy into its
/// history, once it is idle.
fn flush_pending(bus: &Bus, id: SessionId, session: &mut Session) {
    if let Some(Idle { cx, recorder }) = session.idle.as_mut() {
        append_history(
            bus,
            id,
            cx,
            recorder.as_mut(),
            session.pending_history.drain(..),
        );
    }
}

/// Clear the working flag of an idle session once no shell command runs
/// any more, and publish the change.
fn settle_working(bus: &Bus, id: SessionId, session: &mut Session) {
    let working = session.idle.is_none() || session.shells > 0;
    if session.state.working != working {
        session.state.working = working;
        let state = session.state.clone();
        bus.publish(id, HostEvent::StateChanged { state });
    }
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
