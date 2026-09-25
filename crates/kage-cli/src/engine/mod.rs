//! The session host every frontend drives.
//!
//! Clients send [`Command`]s and observe [`kage_core::protocol::Envelope`]s
//! through subscribers. A dispatcher thread owns the sessions and never
//! blocks on a run: each run executes on its own thread and hands its
//! context back when it ends. Permission questions travel over the same
//! channels: the engine publishes `PermissionRequested` and a client
//! answers with `ResolvePermission`.

mod agent_tool;
mod bus;
mod plugin_tools;
mod recorder;
mod runner;
mod sessions;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use kage_core::agents::{AgentDef, AgentDefs};
use kage_core::config::Config;
use kage_core::options::{OptionStore, OptionValue};
use kage_core::protocol::{
    Command, CommandKind, Delivery, HostEvent, NoticeLevel, PermissionDecision, RequestId,
    RunOutcome, SessionState, Usage,
};
use kage_core::sync::lock;
use kage_core::{
    CancelFlag, Content, LoopError, Message, Role, SessionId, ThinkingLevel, TokenUsage, ToolOutput,
};
use kage_loop::{AgentContext, LoopConfig};
use kage_mcp::McpManager;
use kage_plugin::PluginRuntime;
use kage_provider::ProviderRegistry;
use kage_tools::ToolRegistry;

pub(crate) use bus::Subscriber;
pub(crate) use recorder::Recorder;
#[cfg(test)]
pub(crate) use sessions::render_session_markdown;

use agent_tool::{AGENT_TOOL, AgentTool, Spawn};
use bus::Bus;
use plugin_tools::PluginTools;
use runner::{Finished, Run, Steering, Work};

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
    /// MCP servers whose tools are in `tools`. Restarts and tool list
    /// changes are applied before each run.
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
    ShellDone {
        session: SessionId,
        command: String,
        output: String,
        exit_code: Option<i32>,
    },
    Title {
        session: SessionId,
        title: String,
    },
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

#[allow(clippy::struct_excessive_bools)]
struct Session {
    idle: Option<Idle>,
    state: SessionState,
    thinking: Option<ThinkingLevel>,
    usage: Usage,
    cancel: CancelFlag,
    steering: Steering,
    queued: VecDeque<Vec<Content>>,
    tools: ToolRegistry,
    plugins: Option<Arc<PluginRuntime>>,
    gate: PermissionGate,
    loop_cfg: LoopConfig,
    mcp: Option<McpManager>,
    interactive: bool,
    /// Session file, when the session is recorded.
    path: Option<PathBuf>,
    workdir: PathBuf,
    /// Messages to add to history before the next run, such as shell
    /// output that arrived while a run was in flight.
    pending_history: Vec<Message>,
    title: bool,
    title_pending: bool,
    plugin_tools: PluginTools,
    agents: Option<AgentSetup>,
    /// Present on sessions an `agent` call started.
    link: Option<AgentLink>,
    /// Read at spawn, while the context is out with a run.
    confine_paths: bool,
}

/// How an agent session hangs off the session that started it.
struct AgentLink {
    parent: SessionId,
    agent: String,
    /// 1 for agents of the main session, 2 for theirs, and so on.
    depth: u8,
    /// Delivers the result to the waiting `agent` call. Taken by the
    /// first run that finishes, so later runs a user starts in the
    /// agent never answer the parent twice.
    reply: Option<mpsc::Sender<ToolOutput>>,
}

/// What a session holds while no run owns it.
struct Idle {
    cx: AgentContext,
    recorder: Option<Recorder>,
}

/// Open permission requests with the session that asked.
type Asks = Arc<Mutex<HashMap<RequestId, (SessionId, mpsc::Sender<PermissionDecision>)>>>;

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
                Input::ShellDone {
                    session,
                    command,
                    output,
                    exit_code,
                } => self.shell_done(session, command, output, exit_code),
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
            if self.shutting_down && self.sessions.values().all(|s| s.idle.is_some()) {
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
        let state = SessionState {
            model,
            thinking: cx.thinking_level.unwrap_or_default(),
            permission_mode: gate.mode(),
            working: false,
        };
        self.bus.publish(
            id,
            HostEvent::StateChanged {
                state: state.clone(),
            },
        );
        self.bus.publish(id, HostEvent::UsageUpdated { usage });
        let path = recorder.as_ref().map(|r| r.path().to_path_buf());
        let workdir = cx.workdir.clone();
        let confine_paths = cx.confine_paths;
        let title_pending = title && !has_reply(&cx);
        self.sessions.insert(
            id,
            Session {
                idle: Some(Idle { cx, recorder }),
                state,
                thinking: None,
                usage,
                cancel,
                steering: Arc::default(),
                queued: VecDeque::new(),
                tools,
                plugins,
                gate,
                loop_cfg,
                mcp,
                interactive,
                path,
                workdir,
                pending_history: Vec::new(),
                title,
                title_pending,
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
            CommandKind::SetModel { model } => self.update_state(id, |s| s.state.model = model),
            CommandKind::SetThinking { level } => self.set_thinking(id, level),
            CommandKind::SetPermissionMode { mode } => self.update_state(id, |s| {
                s.gate.set_mode(mode);
                s.state.permission_mode = mode;
            }),
            CommandKind::Shutdown | CommandKind::ResolvePermission { .. } => {}
        }
    }

    /// Record and apply a thinking level now when idle, or at the next
    /// run start otherwise.
    fn set_thinking(&mut self, id: SessionId, level: ThinkingLevel) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        session.state.thinking = level;
        match session.idle.as_mut() {
            Some(idle) => {
                idle.cx.thinking_level = Some(level);
                if let Some(recorder) = idle.recorder.as_mut()
                    && let Err(err) = recorder.append(&thinking_entry(level))
                {
                    notice(
                        &self.bus,
                        id,
                        NoticeLevel::Error,
                        format!("session write failed: {err}"),
                    );
                }
            }
            None => session.thinking = Some(level),
        }
        let state = session.state.clone();
        self.bus.publish(id, HostEvent::StateChanged { state });
    }

    fn shell(&self, id: SessionId, command: String) {
        let workdir = self.sessions[&id].workdir.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let (exit_code, output) = run_shell(&command, &workdir);
            let _ = tx.send(Input::ShellDone {
                session: id,
                command,
                output,
                exit_code,
            });
        });
    }

    /// Show a finished shell command and share its output with the model
    /// on the next turn. The output is not recorded to the session file.
    fn shell_done(
        &mut self,
        id: SessionId,
        command: String,
        output: String,
        exit_code: Option<i32>,
    ) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let exit = exit_code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
        let text = format!(
            "[shell] ran `{command}` in the session working directory; exit code {exit}:\n{}",
            output.trim_end()
        );
        let message = Message::new(Role::User, vec![Content::Text { text }], None);
        match session.idle.as_mut() {
            Some(idle) => {
                let parent = idle.cx.history.last().map(|m| m.id);
                idle.cx.history.push(Message { parent, ..message });
            }
            None => session.pending_history.push(message),
        }
        self.bus.publish(
            id,
            HostEvent::ShellFinished {
                command,
                output,
                exit_code,
            },
        );
    }

    fn record_title(&mut self, id: SessionId, title: String) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if let Some(recorder) = session.idle.as_mut().and_then(|i| i.recorder.as_mut()) {
            let entry = kage_session::SessionEntry::Title(kage_session::SessionTitle {
                id: kage_session::EntryId::new(),
                ts: chrono::Utc::now(),
                title: title.clone(),
            });
            if let Err(err) = recorder.append(&entry) {
                notice(
                    &self.bus,
                    id,
                    NoticeLevel::Error,
                    format!("session title: {err}"),
                );
            }
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

    fn prompt(&mut self, id: SessionId, content: Vec<Content>, delivery: Delivery) {
        let session = self.sessions.get_mut(&id).expect("session checked");
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
                self.deliver(id, &outcome, &[]);
                self.bus.publish(id, HostEvent::RunEnded { outcome });
                return;
            }
        };
        let Some(Idle {
            mut cx,
            mut recorder,
        }) = session.idle.take()
        else {
            return;
        };
        cx.model = bare_model;
        if let Some(window) = crate::runtime_env::context_window_for(&self.registry, &model) {
            cx.context_window = window;
        }
        cx.max_output_tokens = crate::runtime_env::max_output_tokens_for(&self.registry, &model);
        cx.history.append(&mut session.pending_history);
        if let Some(level) = session.thinking.take() {
            cx.thinking_level = Some(level);
            if let Some(recorder) = recorder.as_mut()
                && let Err(err) = recorder.append(&thinking_entry(level))
            {
                notice(
                    &self.bus,
                    id,
                    NoticeLevel::Error,
                    format!("session write failed: {err}"),
                );
            }
        }
        refresh_mcp(&self.bus, id, session);

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
            model,
            tools,
            cx,
            recorder,
            usage: session.usage,
            loop_cfg: session.loop_cfg,
            cancel: session.cancel.clone(),
            gate,
            steering: Arc::clone(&session.steering),
            plugins: session.plugins.clone(),
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
            recorder,
            usage,
            outcome,
        } = finished;
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        cx.history.append(&mut session.pending_history);
        if outcome == RunOutcome::Completed && session.title_pending {
            session.title_pending = false;
            let model = session.state.model.clone();
            self.generate_title(id, &cx, &model);
        }
        self.deliver(id, &outcome, &cx.history);
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
        session.usage = usage;
        session.state.working = false;
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

    /// Open a child session for an `agent` call and start it, or queue it
    /// when the running limit is reached. Errors reply at once.
    fn spawn(&mut self, spawn: Spawn) {
        let Spawn {
            parent,
            tool_call_id,
            agent,
            description,
            prompt,
            reply,
        } = spawn;
        let fail = |text: String| {
            let _ = reply.send(agent_tool::error_output(text));
        };
        let Some(from) = self.sessions.get(&parent) else {
            return fail(format!("session {parent} is gone"));
        };
        let Some(setup) = from.agents.clone() else {
            return fail("agents are turned off".to_owned());
        };
        let depth = depth_of(from) + 1;
        if depth > setup.max_depth {
            return fail(format!(
                "agents may nest {} level(s) deep (agent_max_depth)",
                setup.max_depth
            ));
        }
        let Some(def) = setup.defs.get(&agent) else {
            let names: Vec<&str> = setup.defs.iter().map(|d| d.name.as_str()).collect();
            return fail(format!(
                "unknown agent `{agent}`. Available agents: {}",
                names.join(", ")
            ));
        };

        let id = SessionId::new();
        let (spec, missing) = agent_spec(from, parent, id, def, &setup);
        let cancel = from.cancel.child();
        let link = AgentLink {
            parent,
            agent: agent.clone(),
            depth,
            reply: Some(reply),
        };
        let marker = serde_json::json!({
            "parent": parent,
            "tool_call_id": tool_call_id,
            "agent": agent,
            "description": description,
        });

        self.bus.publish(
            id,
            HostEvent::AgentSpawned {
                parent,
                tool_call_id,
                agent,
                description: description.clone(),
            },
        );
        self.open(spec, cancel, Some(link));
        self.record_agent_entries(id, marker, description);
        for name in missing {
            notice(
                &self.bus,
                id,
                NoticeLevel::Warning,
                format!("agent tools: no tool named `{name}`"),
            );
        }
        let content = vec![Content::Text { text: prompt }];
        if self.running_agents() < setup.max_running {
            self.start_run(id, Work::Prompt(Message::new(Role::User, content, None)));
        } else {
            let session = self.sessions.get_mut(&id).expect("session opened");
            session.queued.push_back(content);
            self.waiting.push_back(id);
        }
    }

    /// Write the `kage:agent` marker and the title right after the header.
    fn record_agent_entries(&mut self, id: SessionId, marker: serde_json::Value, title: String) {
        let Some(recorder) = self
            .sessions
            .get_mut(&id)
            .and_then(|s| s.idle.as_mut())
            .and_then(|i| i.recorder.as_mut())
        else {
            return;
        };
        let ts = chrono::Utc::now();
        let entries = [
            kage_session::SessionEntry::Custom(kage_session::Custom {
                id: kage_session::EntryId::new(),
                ts,
                kind: kage_session::list::AGENT_ENTRY_KIND.to_owned(),
                data: marker,
            }),
            kage_session::SessionEntry::Title(kage_session::SessionTitle {
                id: kage_session::EntryId::new(),
                ts,
                title,
            }),
        ];
        for entry in &entries {
            if let Err(err) = recorder.append(entry) {
                notice(
                    &self.bus,
                    id,
                    NoticeLevel::Error,
                    format!("session write failed: {err}"),
                );
                return;
            }
        }
    }

    /// Send an agent's result to its `agent` call, once.
    fn deliver(&mut self, id: SessionId, outcome: &RunOutcome, history: &[Message]) {
        let Some(link) = self.sessions.get_mut(&id).and_then(|s| s.link.as_mut()) else {
            return;
        };
        if let Some(reply) = link.reply.take() {
            let _ = reply.send(agent_tool::agent_result(id, &link.agent, outcome, history));
        }
    }

    /// Agent runs in flight that hold a slot of the running limit. An
    /// agent waiting on its own agents holds none, so nesting cannot
    /// deadlock the limit.
    fn running_agents(&self) -> usize {
        let waits_on_agents = |id: &SessionId| {
            self.sessions.values().any(|s| {
                s.link
                    .as_ref()
                    .is_some_and(|l| l.parent == *id && l.reply.is_some())
            })
        };
        self.sessions
            .iter()
            .filter(|(id, s)| s.link.is_some() && s.idle.is_none() && !waits_on_agents(id))
            .count()
    }

    /// Start waiting agents while the running limit allows.
    fn start_waiting(&mut self) {
        while !self.shutting_down
            && let Some(&id) = self.waiting.front()
        {
            let max = self
                .sessions
                .get(&id)
                .and_then(|s| s.agents.as_ref())
                .map_or(usize::MAX, |a| a.max_running);
            if self.running_agents() >= max {
                return;
            }
            self.waiting.pop_front();
            let next = self
                .sessions
                .get_mut(&id)
                .and_then(|s| s.queued.pop_front());
            if let Some(content) = next {
                self.start_run(id, Work::Prompt(Message::new(Role::User, content, None)));
            }
        }
    }

    /// End a waiting agent that never started as cancelled.
    fn end_waiting(&mut self, id: SessionId) {
        self.waiting.retain(|w| *w != id);
        if let Some(session) = self.sessions.get_mut(&id) {
            session.queued.clear();
            lock(&session.steering).clear();
        }
        self.deliver(id, &RunOutcome::Cancelled, &[]);
        self.bus.publish(
            id,
            HostEvent::RunEnded {
                outcome: RunOutcome::Cancelled,
            },
        );
    }

    fn parent_of(&self, id: SessionId) -> Option<SessionId> {
        self.sessions.get(&id)?.link.as_ref().map(|l| l.parent)
    }

    /// Whether `id` is an agent somewhere below `ancestor`.
    fn descends_from(&self, id: SessionId, ancestor: SessionId) -> bool {
        let mut current = self.parent_of(id);
        while let Some(parent) = current {
            if parent == ancestor {
                return true;
            }
            current = self.parent_of(parent);
        }
        false
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
        let (reply, answer) = mpsc::channel();
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

/// Apply plugin-requested MCP restarts and announced tool list changes to
/// the session's tools.
fn refresh_mcp(bus: &Bus, id: SessionId, session: &mut Session) {
    let Some(mcp) = session.mcp.as_mut() else {
        return;
    };
    let restarts = session
        .plugins
        .as_ref()
        .map(|rt| rt.take_mcp_restarts())
        .unwrap_or_default();
    for name in restarts {
        match mcp.restart(&name, &mut session.tools) {
            Ok(()) => notice(bus, id, NoticeLevel::Info, format!("restarted `{name}`")),
            Err(err) => notice(
                bus,
                id,
                NoticeLevel::Error,
                format!("mcp restart `{name}`: {err}"),
            ),
        }
    }
    for (server, err) in mcp.refresh_into(&mut session.tools) {
        notice(
            bus,
            id,
            NoticeLevel::Error,
            format!("mcp `{server}`: {err}"),
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

/// The session an agent of `from` runs in: the definition's model,
/// thinking, role and tools over `from`'s, `from`'s gate and loop
/// settings, no plugins or MCP of its own, and a file next to `from`'s
/// when `from` records. Also returns listed tools that match nothing.
fn agent_spec(
    from: &Session,
    parent: SessionId,
    id: SessionId,
    def: &AgentDef,
    setup: &AgentSetup,
) -> (SessionSpec, Vec<String>) {
    let model = def
        .model
        .clone()
        .unwrap_or_else(|| from.state.model.clone());
    let system_prompt =
        crate::runtime_env::build_system_prompt(&def.body, &from.workdir, &model, &[]);
    let mut cx = AgentContext::new(model.clone(), &system_prompt).with_workdir(&from.workdir);
    cx.confine_paths = from.confine_paths;
    cx.thinking_level = Some(def.thinking.unwrap_or(from.state.thinking));
    let (tools, missing) = agent_tools(&from.tools, def.tools.as_deref());
    let recorder = from.path.as_deref().and_then(Path::parent).map(|dir| {
        let header = kage_session::Header {
            version: kage_session::FORMAT_VERSION,
            session: id,
            id: kage_session::EntryId::new(),
            ts: chrono::Utc::now(),
            cwd: from.workdir.clone(),
            model: model.clone(),
            system_prompt,
            parent_session: Some(parent),
            parent_entry: None,
        };
        Recorder::planned(crate::build_session_path(dir, id), header, None)
    });
    let spec = SessionSpec {
        id,
        model,
        cx,
        recorder,
        tools,
        plugins: None,
        gate: from.gate.clone(),
        loop_cfg: from.loop_cfg,
        mcp: None,
        interactive: from.interactive,
        title: false,
        agents: Some(setup.clone()),
    };
    (spec, missing)
}

/// 0 for a main session, 1 for its agents, and so on.
fn depth_of(session: &Session) -> u8 {
    session.link.as_ref().map_or(0, |l| l.depth)
}

/// The tools an agent gets: `parent`'s, narrowed to `only` when the
/// definition lists tools. Also returns listed names that match nothing.
fn agent_tools(parent: &ToolRegistry, only: Option<&[String]>) -> (ToolRegistry, Vec<String>) {
    let Some(only) = only else {
        return (parent.clone(), Vec::new());
    };
    let mut tools = ToolRegistry::new();
    let mut missing = Vec::new();
    for name in only {
        match parent.get(name) {
            Some(tool) => tools.register(Arc::clone(tool)),
            None if name == AGENT_TOOL => {}
            None => missing.push(name.clone()),
        }
    }
    (tools, missing)
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

fn thinking_entry(level: ThinkingLevel) -> kage_session::SessionEntry {
    kage_session::SessionEntry::ThinkingLevelChange(kage_session::ThinkingLevelChange {
        id: kage_session::EntryId::new(),
        ts: chrono::Utc::now(),
        level: level.as_str().to_owned(),
    })
}

/// Run `command` with `sh -c` in `workdir` and capture stdout and stderr
/// together, truncated so a chatty command cannot flood the context.
/// Returns the exit code (`None` when a signal ended the command or it
/// failed to spawn) and the output.
pub(crate) fn run_shell(command: &str, workdir: &std::path::Path) -> (Option<i32>, String) {
    const OUTPUT_CAP: usize = 8 * 1024;
    let output = match std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .output()
    {
        Ok(output) => output,
        Err(err) => return (None, format!("failed to run: {err}")),
    };
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    if combined.chars().count() > OUTPUT_CAP {
        let cut: String = combined.chars().take(OUTPUT_CAP).collect();
        combined = format!("{cut}\n... (output truncated)");
    }
    (output.status.code(), combined)
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
