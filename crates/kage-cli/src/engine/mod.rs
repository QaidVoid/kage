//! The session host every frontend drives.
//!
//! Clients send [`Command`]s and observe [`kage_core::protocol::Envelope`]s
//! through subscribers. A dispatcher thread owns the sessions and never
//! blocks on a run: each run executes on its own thread and hands its
//! context back when it ends.

mod bus;
mod recorder;
mod runner;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use kage_core::protocol::{
    Command, CommandKind, Delivery, HostEvent, NoticeLevel, SessionState, Usage,
};
use kage_core::sync::lock;
use kage_core::{CancelFlag, Content, Message, Role, SessionId, ThinkingLevel};
use kage_loop::{AgentContext, LoopConfig};
use kage_plugin::PluginRuntime;
use kage_provider::ProviderRegistry;
use kage_tools::ToolRegistry;

pub(crate) use bus::Subscriber;
pub(crate) use recorder::Recorder;

use bus::Bus;
use runner::{Finished, Run, Steering};

use crate::permissions::PermissionGate;

/// Shared resources every session runs with.
pub(crate) struct EngineConfig {
    pub registry: Arc<ProviderRegistry>,
    pub tools: ToolRegistry,
    pub plugins: Option<Arc<PluginRuntime>>,
    pub loop_cfg: LoopConfig,
    pub gate: PermissionGate,
}

/// A session to host.
pub(crate) struct SessionSpec {
    pub id: SessionId,
    /// Provider-qualified model id.
    pub model: String,
    pub cx: AgentContext,
    pub recorder: Option<Recorder>,
}

/// Handle to a running engine. Dropping it shuts the engine down.
pub(crate) struct Engine {
    tx: mpsc::Sender<Input>,
    thread: Option<thread::JoinHandle<()>>,
}

enum Input {
    Command(Command),
    Open(Box<SessionSpec>),
    Finished(Finished),
}

impl Engine {
    pub(crate) fn start(config: EngineConfig, subscribers: Vec<Subscriber>) -> Self {
        let (tx, rx) = mpsc::channel();
        let dispatcher = Dispatcher {
            bus: Arc::new(Bus::new(subscribers)),
            config,
            sessions: HashMap::new(),
            active: None,
            tx: tx.clone(),
            shutting_down: false,
        };
        let thread = thread::spawn(move || dispatcher.run(&rx));
        Self {
            tx,
            thread: Some(thread),
        }
    }

    /// Host `spec`. The first session opened becomes the active one.
    pub(crate) fn open(&self, spec: SessionSpec) {
        let _ = self.tx.send(Input::Open(Box::new(spec)));
    }

    pub(crate) fn send(&self, command: Command) {
        let _ = self.tx.send(Input::Command(command));
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
}

/// What a session holds while no run owns it.
struct Idle {
    cx: AgentContext,
    recorder: Option<Recorder>,
}

struct Dispatcher {
    bus: Arc<Bus>,
    config: EngineConfig,
    sessions: HashMap<SessionId, Session>,
    active: Option<SessionId>,
    tx: mpsc::Sender<Input>,
    shutting_down: bool,
}

impl Dispatcher {
    fn run(mut self, rx: &mpsc::Receiver<Input>) {
        while let Ok(input) = rx.recv() {
            match input {
                Input::Command(command) => self.command(command),
                Input::Open(spec) => self.open(*spec),
                Input::Finished(finished) => self.finish(finished),
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
        } = spec;
        let usage = Usage {
            total: kage_core::TokenUsage {
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
            permission_mode: self.config.gate.mode(),
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
                steering: Arc::new(Mutex::new(VecDeque::new())),
                queued: VecDeque::new(),
            },
        );
        self.active.get_or_insert(id);
    }

    fn command(&mut self, command: Command) {
        if let CommandKind::Shutdown = command.kind {
            self.shutting_down = true;
            for session in self.sessions.values() {
                session.cancel.cancel();
            }
            return;
        }
        let Some(id) = command.session.or(self.active) else {
            return;
        };
        if !self.sessions.contains_key(&id) {
            self.notice(id, NoticeLevel::Error, format!("unknown session {id}"));
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
            CommandKind::SetPermissionMode { mode } => {
                self.config.gate.set_mode(mode);
                self.update_state(id, |s| s.state.permission_mode = mode);
            }
            other => self.notice(
                id,
                NoticeLevel::Error,
                format!("command not supported here: {other:?}"),
            ),
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
        let provider = match self.config.registry.resolve(&model) {
            Ok(resolved) => {
                let Some(Idle { mut cx, recorder }) = session.idle.take() else {
                    return;
                };
                cx.model.clone_from(&resolved.model);
                if let Some(window) =
                    crate::runtime_env::context_window_for(&self.config.registry, &model)
                {
                    cx.context_window = window;
                }
                cx.max_output_tokens =
                    crate::runtime_env::max_output_tokens_for(&self.config.registry, &model);
                if let Some(level) = session.thinking {
                    cx.thinking_level = Some(level);
                }
                (Arc::clone(resolved.provider), cx, recorder)
            }
            Err(err) => {
                self.notice(
                    id,
                    NoticeLevel::Error,
                    format!("model {model} unavailable: {err}"),
                );
                return;
            }
        };
        let (provider, cx, recorder) = provider;
        session.usage.context_window = cx.context_window;
        session.cancel.reset();
        session.state.working = true;
        let prompt = Message::new(Role::User, content, cx.history.last().map(|m| m.id));
        let run = Run {
            session: id,
            prompt,
            provider,
            model,
            tools: self.config.tools.clone(),
            cx,
            recorder,
            usage: session.usage,
            loop_cfg: self.config.loop_cfg,
            cancel: session.cancel.clone(),
            gate: self.config.gate.clone(),
            steering: Arc::clone(&session.steering),
            plugins: self.config.plugins.clone(),
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

    fn notice(&self, id: SessionId, level: NoticeLevel, text: String) {
        self.bus.publish(
            id,
            HostEvent::Notice {
                level,
                text,
                transient: false,
            },
        );
    }
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
