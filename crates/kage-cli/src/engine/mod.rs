//! The session host every frontend drives.
//!
//! Clients send [`Command`]s and observe [`kage_core::protocol::Envelope`]s
//! through subscribers. A dispatcher thread owns the sessions and never
//! blocks on a run: each run executes on its own thread and hands its
//! context back when it ends. Permission questions travel over the same
//! channels: the engine publishes `PermissionRequested` and a client
//! answers with `ResolvePermission`.

mod bus;
mod recorder;
mod runner;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use kage_core::protocol::{
    Command, CommandKind, Delivery, HostEvent, NoticeLevel, PermissionDecision, RequestId,
    RunOutcome, SessionState, Usage,
};
use kage_core::sync::lock;
use kage_core::{
    CancelFlag, Content, LoopError, Message, Role, SessionId, ThinkingLevel, TokenUsage,
};
use kage_loop::{AgentContext, LoopConfig};
use kage_mcp::McpManager;
use kage_plugin::PluginRuntime;
use kage_provider::ProviderRegistry;
use kage_tools::ToolRegistry;

pub(crate) use bus::Subscriber;
pub(crate) use recorder::Recorder;

use bus::Bus;
use runner::{Finished, Run, Steering};

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
}

enum Input {
    Command(Command),
    Open(Box<SessionSpec>),
    Finished(Box<Finished>),
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
}

/// What a session holds while no run owns it.
struct Idle {
    cx: AgentContext,
    recorder: Option<Recorder>,
}

type Asks = Arc<Mutex<HashMap<RequestId, mpsc::Sender<PermissionDecision>>>>;

struct Dispatcher {
    bus: Arc<Bus>,
    registry: Arc<ProviderRegistry>,
    sessions: HashMap<SessionId, Session>,
    active: Option<SessionId>,
    tx: mpsc::Sender<Input>,
    asks: Asks,
    next_request: Arc<AtomicU64>,
    shutting_down: bool,
}

impl Dispatcher {
    fn run(mut self, rx: &mpsc::Receiver<Input>) {
        while let Ok(input) = rx.recv() {
            match input {
                Input::Command(command) => self.command(command),
                Input::Open(spec) => self.open(*spec),
                Input::Finished(finished) => self.finish(*finished),
            }
            if self.shutting_down && self.sessions.values().all(|s| s.idle.is_some()) {
                return;
            }
        }
    }

    fn open(&mut self, spec: SessionSpec) {
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
        } = spec;
        let usage = Usage {
            total: TokenUsage {
                input: cx.budget.used_input,
                output: cx.budget.used_output,
                cache_read: cx.budget.used_cache_read,
                cache_write: cx.budget.used_cache_write,
            },
            context_used: cx.budget.current_context,
            context_window: cx.context_window,
            cost: 0.0,
        };
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
        self.sessions.insert(
            id,
            Session {
                idle: Some(Idle { cx, recorder }),
                state,
                thinking: None,
                usage,
                cancel: CancelFlag::new(),
                steering: Arc::default(),
                queued: VecDeque::new(),
                tools,
                plugins,
                gate,
                loop_cfg,
                mcp,
                interactive,
            },
        );
        self.active.get_or_insert(id);
    }

    fn command(&mut self, command: Command) {
        match command.kind {
            CommandKind::Shutdown => {
                self.shutting_down = true;
                for session in self.sessions.values() {
                    session.cancel.cancel();
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
            CommandKind::Cancel => self.sessions[&id].cancel.cancel(),
            CommandKind::SetModel { model } => self.update_state(id, |s| s.state.model = model),
            CommandKind::SetThinking { level } => self.update_state(id, |s| {
                s.state.thinking = level;
                s.thinking = Some(level);
            }),
            CommandKind::SetPermissionMode { mode } => self.update_state(id, |s| {
                s.gate.set_mode(mode);
                s.state.permission_mode = mode;
            }),
            other => notice(
                &self.bus,
                id,
                NoticeLevel::Error,
                format!("command not supported here: {other:?}"),
            ),
        }
    }

    fn resolve_permission(
        &self,
        session: Option<SessionId>,
        request_id: RequestId,
        decision: PermissionDecision,
    ) {
        if let Some(reply) = lock(&self.asks).remove(&request_id) {
            let _ = reply.send(decision);
        }
        if let Some(id) = session.or(self.active) {
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
        if session.idle.is_some() {
            self.start_run(id, content);
            return;
        }
        match (delivery, text_only(&content)) {
            (Delivery::Steer, Some(text)) => lock(&session.steering).push_back(text),
            _ => session.queued.push_back(content),
        }
    }

    fn start_run(&mut self, id: SessionId, content: Vec<Content>) {
        let session = self.sessions.get_mut(&id).expect("session checked");
        let model = session.state.model.clone();
        let (provider, bare_model) = match self.registry.resolve(&model) {
            Ok(resolved) => (Arc::clone(resolved.provider), resolved.model),
            Err(err) => {
                let message = format!("model {model} unavailable: {err}");
                notice(&self.bus, id, NoticeLevel::Error, message.clone());
                self.bus.publish(
                    id,
                    HostEvent::RunEnded {
                        outcome: RunOutcome::Failed {
                            error: LoopError::Provider { message },
                        },
                    },
                );
                return;
            }
        };
        let Some(Idle { mut cx, recorder }) = session.idle.take() else {
            return;
        };
        cx.model = bare_model;
        if let Some(window) = crate::runtime_env::context_window_for(&self.registry, &model) {
            cx.context_window = window;
        }
        cx.max_output_tokens = crate::runtime_env::max_output_tokens_for(&self.registry, &model);
        if let Some(level) = session.thinking {
            cx.thinking_level = Some(level);
        }
        refresh_mcp(&self.bus, id, session);

        session.usage.context_window = cx.context_window;
        session.cancel.reset();
        session.state.working = true;
        let mut gate = session.gate.clone().with_cancel(session.cancel.clone());
        if session.interactive {
            gate = gate.with_asker(asker(&self.bus, &self.asks, &self.next_request, id));
        }
        let run = Run {
            session: id,
            prompt: Message::new(Role::User, content, cx.history.last().map(|m| m.id)),
            provider,
            model,
            tools: session.tools.clone(),
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
            cx,
            recorder,
            usage,
        } = finished;
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        session.idle = Some(Idle { cx, recorder });
        session.usage = usage;
        session.state.working = false;
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
        self.bus.publish(id, HostEvent::StateChanged { state });
        if let Some(content) = next {
            self.start_run(id, content);
        }
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
        lock(&asks).insert(request_id, reply);
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
