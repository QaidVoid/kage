//! The TUI on an ACP link: `kage rpc` as a child process, ACP frames
//! as newline-delimited JSON over its pipes.
//!
//! A reader thread turns the child's stdout lines into [`Event`]s and
//! a writer thread drains [`Frame`]s into its stdin. A pump thread
//! owns the [`Store`]: it feeds the agent's frames in, sends what the
//! client produces, and fabricates the signals the wire cannot carry
//! as [`Envelope`]s for the App's engine channel, because the agent
//! echoes a prompt only to its other clients and reports the run's
//! shape only through this side's own bookkeeping. What needs
//! `&mut App` waits in a [`FeedState`] the App drains as its
//! [`kage_tui::AcpFeed`]. The child's stderr is captured to a short
//! tail rather than let loose on the raw terminal, and the tail rides
//! the notice when the child dies. The child is this same `kage`
//! binary, so the TUI and its engine share a build; [`spawn`] runs
//! another program instead, which the tests use. On Unix `kage rpc`
//! relays to the shared per-user host, so sessions and running turns
//! outlive the TUI until the host goes idle. The child is killed when
//! the transport drops, which happens when the user quits.
//!
//! Some commands the engine path serves have no ACP counterpart yet:
//! the pump refuses them with a notice instead of dropping them
//! silently.

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use kage_client::wire::{
    BlobContent, ClientCapabilities, ClientMeta, ContentBlock, Implementation,
    KageClientCapabilities, NewSessionKage, NewSessionMeta, PermissionOptionKind, SessionOpt,
    TextContent, UnconfiguredTools,
};
use kage_client::{Change, DialogAsk, Frame, PermissionDecision, RequestId};
use kage_core::keymap::{Keymap, MappingData};
use kage_core::options::OptionValue;
use kage_core::protocol::{
    Delivery, Envelope, HostEvent, NoticeLevel, Question, QuestionOption, RunOutcome, SessionId,
    SessionState,
};
use kage_core::{LoopEvent, MediaSource, Message, Role, ThinkingLevel, ToolCallId};
use kage_tui::command::PluginCommand;
use kage_tui::{AcpUpdate, SharedBuffer, Store};
use serde::Deserialize;
use serde_json::Value;

use super::entry::{Frontend, startup_config};
use super::*;
use kage_tui::hostlog::LogPublisher;

/// What the connection delivered.
#[derive(Debug)]
pub(crate) enum Event {
    /// A frame from the agent.
    Frame(Frame),
    /// The child's stdout ended: it is gone.
    Closed,
}

/// Everything the threads and the handle share.
#[derive(Default)]
struct Shared {
    /// The writer thread's queue, absent once the child is gone.
    writer: Mutex<Option<mpsc::Sender<Frame>>>,
    /// The child, so a drop can kill it.
    child: Mutex<Option<Child>>,
    /// The child's last stderr bytes, kept by the stderr thread.
    stderr: Mutex<Vec<u8>>,
}

/// The stdio connection to the spawned `kage rpc`. Its drop ends the
/// child, so it is passed by ownership, never cloned.
pub(crate) struct Transport {
    shared: Arc<Shared>,
}

impl Transport {
    /// Spawns `kage rpc` beside this binary and starts its io
    /// threads.
    pub(crate) fn start(events: mpsc::Sender<Event>) -> Result<Transport, String> {
        let program = program()?;
        spawn(&program, &["rpc".to_owned()], events)
    }

    /// Queues `frame` for the child. A frame the child cannot take is
    /// dropped; [`Event::Closed`] is how the connection learns the
    /// child is gone.
    pub(crate) fn send(&self, frame: Frame) {
        let writer = self.shared.writer.lock().unwrap().clone();
        if let Some(writer) = writer {
            let _ = writer.send(frame);
        }
    }

    /// The child's last stderr bytes, lossily decoded. Empty while the
    /// child said nothing on stderr.
    pub(crate) fn stderr_tail(&self) -> String {
        let tail = self.shared.stderr.lock().unwrap();
        String::from_utf8_lossy(&tail).into_owned()
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        *self.shared.writer.lock().unwrap() = None;
        if let Some(mut child) = self.shared.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The program to run for `kage rpc`: this same binary, so the TUI
/// and its engine share a build. `KAGE_RPC_BIN` names another, which
/// tests use to stand in for the engine.
fn program() -> Result<PathBuf, String> {
    if let Some(named) = std::env::var_os("KAGE_RPC_BIN") {
        return Ok(PathBuf::from(named));
    }
    std::env::current_exe().map_err(|error| format!("cannot find this kage: {error}"))
}

/// Spawns `program args` with piped stdio and starts the reader and
/// writer threads. A thread that would not start leaves the transport
/// to drop, which kills the child.
fn spawn(
    program: &Path,
    args: &[String],
    events: mpsc::Sender<Event>,
) -> Result<Transport, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot spawn {}: {error}", program.display()))?;
    let stdin = child.stdin.take().expect("spawned with piped stdin");
    let stdout = child.stdout.take().expect("spawned with piped stdout");
    let stderr = child.stderr.take().expect("spawned with piped stderr");
    let (queue, reader) = mpsc::channel::<Frame>();
    let shared = Arc::new(Shared {
        writer: Mutex::new(Some(queue)),
        child: Mutex::new(Some(child)),
        stderr: Mutex::new(Vec::new()),
    });
    let transport = Transport {
        shared: Arc::clone(&shared),
    };
    let shared_for_writer = Arc::clone(&shared);
    let spawned = thread::Builder::new()
        .name("kage-acp-writer".to_owned())
        .spawn(move || {
            let mut stdin = stdin;
            for frame in reader {
                let line = serde_json::to_string(&frame.to_value()).expect("frame serializes");
                if writeln!(stdin, "{line}").is_err() || stdin.flush().is_err() {
                    // The child is gone; senders learn at once.
                    *shared_for_writer.writer.lock().unwrap() = None;
                    break;
                }
            }
        });
    spawned.map_err(|error| format!("cannot start the io threads: {error}"))?;
    let spawned = thread::Builder::new()
        .name("kage-acp-reader".to_owned())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        while line.ends_with('\n') || line.ends_with('\r') {
                            line.pop();
                        }
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                            continue;
                        };
                        let Some(frame) = Frame::parse(&value) else {
                            continue;
                        };
                        if events.send(Event::Frame(frame)).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = events.send(Event::Closed);
        });
    spawned.map_err(|error| format!("cannot start the io threads: {error}"))?;
    let shared_for_stderr = Arc::clone(&shared);
    let spawned = thread::Builder::new()
        .name("kage-acp-stderr".to_owned())
        .spawn(move || {
            let mut stderr = stderr;
            let mut chunk = [0u8; 512];
            while let Ok(read) = stderr.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                let mut tail = shared_for_stderr.stderr.lock().unwrap();
                tail.extend_from_slice(&chunk[..read]);
                let excess = tail.len().saturating_sub(STDERR_TAIL);
                tail.drain(..excess);
            }
        });
    spawned.map_err(|error| format!("cannot start the io threads: {error}"))?;
    Ok(transport)
}

/// How long the pump waits on a silent connection before it looks at
/// the channels it shares with the App again.
const TICK: Duration = Duration::from_millis(20);

/// What the App sees once the agent is gone.
const DETACHED: &str = "the kage agent stopped; this TUI is detached";

/// How many stderr bytes the child's last words are kept in, for the
/// notice when it dies.
const STDERR_TAIL: usize = 2000;

/// What the pump holds back for the App's `&mut App` work, shared
/// through `kage_tui::AcpFeed`. Every field is a one-shot handover:
/// `take` empties it, so the App applies exactly what is waiting.
#[derive(Default)]
struct FeedState {
    commands: Mutex<Option<Vec<PluginCommand>>>,
    keymap: Mutex<Option<Keymap>>,
    buffer: Mutex<Option<SharedBuffer>>,
    status: Mutex<Option<Vec<(String, String)>>>,
}

impl FeedState {
    fn send_commands(&self, commands: Vec<PluginCommand>) {
        *self.commands.lock().unwrap() = Some(commands);
    }

    fn send_keymap(&self, keymap: Option<Keymap>) {
        *self.keymap.lock().unwrap() = keymap;
    }

    fn send_buffer(&self, buffer: SharedBuffer) {
        *self.buffer.lock().unwrap() = Some(buffer);
    }

    fn send_status(&self, status: Vec<(String, String)>) {
        *self.status.lock().unwrap() = Some(status);
    }
}

impl kage_tui::AcpFeed for FeedState {
    fn take(&self) -> AcpUpdate {
        AcpUpdate {
            commands: self.commands.lock().unwrap().take(),
            keymap: self.keymap.lock().unwrap().take(),
            root_buffer: self.buffer.lock().unwrap().take(),
            status: self.status.lock().unwrap().take(),
        }
    }
}

/// The `_meta.kage` the TUI asks of the session it opens: what this
/// process would have set up on the engine path, phrased for the
/// agent to apply instead. Only what the user actually set is sent.
fn session_meta(
    options: &kage_plugin::SharedOptions,
    registry: &ProviderRegistry,
    model: &str,
    system: &str,
    yolo: bool,
) -> NewSessionMeta {
    let store = lock(options);
    // A fresh store pre-seeds every option with its default, so only
    // a value the user or the config actually set is sent: the host
    // keeps its own defaults otherwise.
    let set = |name: &str| {
        store
            .source(name)
            .is_some_and(|source| source != kage_core::options::OptionSource::Default)
    };
    let threshold = set("compaction_threshold")
        .then(|| store.get("compaction_threshold"))
        .flatten()
        .and_then(OptionValue::as_float);
    let retries = set("max_provider_retries")
        .then(|| store.get("max_provider_retries"))
        .flatten()
        .and_then(OptionValue::as_int);
    let thinking = set("thinking_level")
        .then(|| store.get("thinking_level"))
        .flatten()
        .and_then(OptionValue::as_str)
        .and_then(ThinkingLevel::parse)
        .map(|level| level.as_str().to_owned());
    let opt =
        (threshold.is_some() || retries.is_some() || thinking.is_some()).then_some(SessionOpt {
            compaction_threshold: threshold,
            max_provider_retries: retries,
            thinking_level: thinking,
        });
    NewSessionMeta {
        kage: Some(NewSessionKage {
            plugins: None,
            background: None,
            yolo,
            system_prompt: (!system.is_empty()).then(|| system.to_owned()),
            opt,
            max_output_tokens: crate::runtime_env::max_output_tokens_for(registry, model),
        }),
    }
}

/// The `session/prompt` params of a frame this client sent, for
/// watching its own prompts go by.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutgoingPrompt {
    session_id: String,
    prompt: Vec<ContentBlock>,
}

/// The thread that owns the connection. It turns the store's view of
/// the wire into the App's engine vocabulary: changes into
/// [`HostEvent`]s, its own prompts into user bubbles, and everything
/// the App asks into client calls. One connection, one pump.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the flags answer unrelated questions: a session setting, \
              the bootstrap, the run edge and the lifecycle"
)]
struct Pump {
    store: Store,
    transport: Transport,
    frames: mpsc::Receiver<Event>,
    events: mpsc::Sender<Envelope>,
    requests: mpsc::Receiver<RunRequest>,
    dialog_tx: mpsc::Sender<PluginDialog>,
    /// Dialogs the App is showing, by wire request id, with the
    /// channel its answer lands on.
    dialogs: Vec<(RequestId, mpsc::Receiver<Option<Value>>)>,
    feed: Arc<FeedState>,
    /// The session id every envelope names: the App sees one session
    /// per connection, whatever the agent calls its sessions.
    app_session: SessionId,
    /// The wire id of the session the App is on.
    wire: String,
    /// The session being opened, whose answer adopts the wire.
    pending_open: Option<String>,
    /// A session/new is in flight: its answer's session adopts.
    pending_new: bool,
    cwd: String,
    /// The model the session runs, as the config option carries it.
    model: String,
    /// The option store the session settings are read from at each
    /// session's opening, so a `:set` since startup reaches the next
    /// session too.
    options: kage_plugin::SharedOptions,
    /// The provider registry, for the model's output token cap.
    registry: Arc<ProviderRegistry>,
    /// The run-wide session settings that do not come from options.
    yolo: bool,
    system_prompt: String,
    seq: u64,
    /// Whether the wire session's run is in flight, for the
    /// run-started and run-ended edges.
    running: bool,
    /// The buffer the App was last handed for the wire session. A
    /// reset on the store side (a replaced session) swaps the object,
    /// which is when the App must be handed the new one.
    handed: Option<SharedBuffer>,
    /// The keymap data last offered to the App, so a snapshot that
    /// moved for other reasons does not clear a half-typed chord.
    last_keymap: Option<Vec<MappingData>>,
    /// Ask ids the App has been shown, by wire id. An ask that leaves
    /// the state closes its card.
    forwarded: HashSet<RequestId>,
    /// Dialog ids the App has been shown.
    dialog_shown: HashSet<RequestId>,
    /// App-facing ask id to wire ask id, for the answers.
    ask_ids: BTreeMap<u64, RequestId>,
    next_ask: u64,
    /// Where an export writes once the agent answers.
    pending_export: Option<PathBuf>,
    /// Update kinds this build cannot show, warned of once each.
    unknown: HashSet<String>,
    closed: bool,
}

impl Pump {
    /// Serves the connection until the App quits or the agent does.
    fn serve(mut self) {
        loop {
            match self.frames.recv_timeout(TICK) {
                Ok(Event::Frame(frame)) => {
                    for change in self.store.absorb(frame) {
                        self.react(change);
                    }
                }
                Ok(Event::Closed)
                // The reader sends Closed before it drops, so this is
                // its late echo.
                | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => self.close(),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            loop {
                match self.requests.try_recv() {
                    Ok(request) => self.handle(request),
                    // The App is gone: stop, which ends the child with
                    // the transport.
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                }
            }
            self.poll_dialogs();
            self.flush();
        }
    }

    /// Opens the connection before the App starts: handshake, session
    /// and plugin UI, with the frames these produce sent at once.
    /// Resuming replays on a fresh buffer; otherwise the new session's
    /// answer lands while the App runs and adopts the wire then.
    fn bootstrap(&mut self, resume: Option<&Path>) {
        let capabilities = ClientCapabilities {
            meta: Some(ClientMeta {
                kage: Some(KageClientCapabilities {
                    // Built-in tools run without asking, like the
                    // engine path of a default configuration.
                    unconfigured_tools: Some(UnconfiguredTools::Allow),
                    plugin_host: false,
                }),
            }),
            ..ClientCapabilities::default()
        };
        let info = Implementation {
            name: "kage".to_owned(),
            title: Some("TUI".to_owned()),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        };
        self.store.client().initialize(capabilities, Some(info));
        if let Some(id) = resume.and_then(|path| path.file_stem()?.to_str().map(str::to_owned)) {
            self.wire.clone_from(&id);
            let fresh = self.store.reset_session(&id);
            self.handed = Some(Arc::clone(&fresh));
            self.feed.send_buffer(fresh);
            self.store.client().load_session(&id, &self.cwd, &[]);
        } else {
            self.pending_new = true;
            let meta = self.session_meta();
            self.store.client().new_session(&self.cwd, &[], Some(meta));
        }
        self.store.client().ui_attach(&self.cwd, None);
        self.flush();
    }

    /// The `_meta.kage` a session of this connection opens with: what
    /// this process would have set up on the engine path, phrased for
    /// the agent to apply. Read fresh, so a `:set` since the last
    /// session reaches the next one.
    fn session_meta(&self) -> NewSessionMeta {
        session_meta(
            &self.options,
            &self.registry,
            &self.model,
            &self.system_prompt,
            self.yolo,
        )
    }

    /// Sends what the client produced, watching its own prompts go by
    /// so the App hears of the runs they start.
    fn flush(&mut self) {
        for frame in self.store.take_outgoing() {
            self.observe_outgoing(&frame);
            self.transport.send(frame);
        }
    }

    fn observe_outgoing(&mut self, frame: &kage_client::Frame) {
        let kage_client::Frame::Request { method, params, .. } = frame else {
            return;
        };
        if method != "session/prompt" {
            return;
        }
        let Ok(prompt) = serde_json::from_value::<OutgoingPrompt>(params.clone()) else {
            return;
        };
        if prompt.session_id != self.wire {
            return;
        }
        if !self.running {
            self.running = true;
            self.publish(HostEvent::RunStarted);
        }
        self.publish(HostEvent::StateChanged {
            state: self.state_snapshot(),
        });
        // The agent echoes a prompt only to the other clients, so the
        // bubble is this side's own copy.
        let message = Message::new(Role::User, content_of(&prompt.prompt), None);
        self.publish(LoopEvent::MessageAppended {
            message: Arc::new(message),
        });
    }

    /// Turns one handled frame's changes into the App's signals.
    fn react(&mut self, change: Change) {
        match change {
            Change::Session { id } => self.session_moved(id),
            Change::Transcript { id } => {
                if id == self.wire {
                    self.publish(HostEvent::StateChanged {
                        state: self.state_snapshot(),
                    });
                    self.publish(HostEvent::UsageUpdated {
                        usage: self.usage_snapshot(),
                    });
                    self.hand_buffer();
                }
            }
            Change::Permission { id } => self.asks_moved(&id),
            Change::AnsweredElsewhere { id, request_id } => {
                if id == self.wire {
                    self.forwarded.remove(&request_id);
                    self.publish_resolved(&request_id);
                }
            }
            Change::Unknown { method, .. } => {
                if self.unknown.insert(method.clone()) {
                    self.notice(
                        NoticeLevel::Warning,
                        format!("the agent sent {method}, which this kage cannot show"),
                    );
                }
            }
            Change::Ui => self.ui_moved(),
            Change::UiOutput { output, cwd: _ } => {
                let level = if output.is_error {
                    NoticeLevel::Error
                } else {
                    NoticeLevel::Info
                };
                self.notice(level, output.text);
            }
            Change::Dialog => self.dialogs_moved(),
            Change::Failed { error, .. } => {
                self.notice(NoticeLevel::Error, format!("kage agent: {}", error.message));
            }
            Change::Exported { markdown, .. } => self.export(markdown),
            Change::Agents { .. }
            | Change::Config { .. }
            | Change::Connection
            | Change::Directory
            | Change::Folders { .. }
            | Change::Forked { .. }
            | Change::Fs { .. }
            | Change::KeySaved { .. }
            | Change::Models { .. }
            | Change::Options { .. }
            | Change::Plugins { .. }
            | Change::ProviderDirectory { .. }
            | Change::Registry { .. }
            | Change::RegistryInstalled { .. }
            | Change::Tested { .. }
            | Change::Themes { .. } => {}
        }
    }

    /// A session moved. A session whose answer was waited for adopts
    /// the wire; the active one publishes its state, with the run
    /// edges its running flag crosses.
    fn session_moved(&mut self, id: String) {
        if let Some(open) = self.pending_open.take_if(|pending| *pending == id) {
            self.adopt(open, false);
            return;
        }
        if self.pending_new && id != self.wire {
            self.pending_new = false;
            let old = std::mem::replace(&mut self.wire, id);
            // A cold start adopts its first session off an empty wire;
            // there is nothing to close.
            if !old.is_empty() {
                self.store.client().close_session(&old);
            }
            self.adopt(self.wire.clone(), true);
            return;
        }
        if id != self.wire {
            return;
        }
        let working = self
            .store
            .state()
            .session(&self.wire)
            .is_some_and(|session| session.running || session.compacting);
        if working && !self.running {
            self.running = true;
            self.publish(HostEvent::RunStarted);
        }
        if !working && self.running {
            self.running = false;
            self.publish(HostEvent::RunEnded {
                outcome: RunOutcome::Completed,
            });
        }
        if let Some(model) = self
            .store
            .state()
            .session(&self.wire)
            .and_then(|session| {
                session
                    .config_options
                    .iter()
                    .find(|option| option.id == "model")
                    .map(|option| option.current_value.clone())
            })
            .filter(|model| !model.is_empty())
        {
            self.model = model;
        }
        self.publish(HostEvent::StateChanged {
            state: self.state_snapshot(),
        });
        self.publish(HostEvent::UsageUpdated {
            usage: self.usage_snapshot(),
        });
        self.hand_buffer();
    }

    /// Hands the App the wire session's current buffer when the store
    /// swapped it, as a replaced session does: the replay of the
    /// replacement folds into the new object, so the old one the App
    /// still draws has to go.
    fn hand_buffer(&mut self) {
        let buffer = self.store.shared_buffer(&self.wire);
        let same = self
            .handed
            .as_ref()
            .is_some_and(|handed| Arc::ptr_eq(handed, &buffer));
        if !same {
            self.handed = Some(Arc::clone(&buffer));
            self.feed.send_buffer(buffer);
        }
    }

    /// Draws from the freshly opened session from now on: the buffer
    /// it folds into, no stale cards, and on a brand-new session the
    /// model carried over. A resumed session keeps the model it was
    /// recorded with, as the engine path does.
    fn adopt(&mut self, id: String, carry_model: bool) {
        self.close_ask_cards();
        self.running = false;
        self.wire = id;
        self.handed = None;
        self.hand_buffer();
        if carry_model {
            self.store
                .client()
                .set_config_option(&self.wire, "model", &self.model);
        }
        self.publish(HostEvent::StateChanged {
            state: self.state_snapshot(),
        });
        self.publish(HostEvent::UsageUpdated {
            usage: self.usage_snapshot(),
        });
    }

    fn asks_moved(&mut self, id: &str) {
        if id != self.wire {
            return;
        }
        let (open, asks) = {
            let Some(session) = self.store.state().session(&self.wire) else {
                return;
            };
            let open: HashSet<RequestId> = session
                .permissions
                .iter()
                .map(|ask| ask.request_id.clone())
                .collect();
            (open, session.permissions.clone())
        };
        let closed: Vec<RequestId> = self
            .forwarded
            .iter()
            .filter(|wire_id| !open.contains(*wire_id))
            .cloned()
            .collect();
        self.forwarded.retain(|wire_id| open.contains(wire_id));
        for wire_id in closed {
            self.publish_resolved(&wire_id);
        }
        for ask in asks {
            if !self.forwarded.insert(ask.request_id.clone()) {
                continue;
            }
            let app_id = self.next_ask;
            self.next_ask += 1;
            self.ask_ids.insert(app_id, ask.request_id.clone());
            let request_id = kage_core::protocol::RequestId(app_id);
            let tool_call_id = Some(ToolCallId::new(&ask.tool_call.tool_call_id));
            if let Some(prompt) = ask.question.as_ref() {
                self.publish(HostEvent::QuestionAsked {
                    request_id,
                    tool_call_id,
                    questions: vec![Question {
                        header: prompt.header.clone(),
                        question: prompt.question.clone(),
                        options: prompt
                            .options
                            .iter()
                            .map(|choice| QuestionOption {
                                label: choice.label.clone(),
                                description: choice.description.clone(),
                            })
                            .collect(),
                        multi_select: prompt.multi_select,
                    }],
                });
            } else {
                self.publish(HostEvent::PermissionRequested {
                    request_id,
                    tool_call_id,
                    tool: ask.tool_call.title.clone().unwrap_or_else(|| ask.subject()),
                    subject: ask.subject(),
                    input: ask.tool_call.raw_input.clone().unwrap_or(Value::Null),
                });
            }
        }
    }

    fn publish_resolved(&mut self, wire_id: &RequestId) {
        let app_id = self
            .ask_ids
            .iter()
            .find(|(_, wire)| *wire == wire_id)
            .map(|(app, _)| *app);
        if let Some(app) = app_id {
            self.publish(HostEvent::PermissionResolved {
                request_id: kage_core::protocol::RequestId(app),
            });
        }
    }

    /// Closes every ask card the App still shows: the conversation
    /// moved elsewhere, so the asks cannot be answered here.
    fn close_ask_cards(&mut self) {
        for wire_id in std::mem::take(&mut self.forwarded) {
            self.publish_resolved(&wire_id);
        }
    }

    fn ui_moved(&mut self) {
        let commands = self.store.plugin_commands();
        let keymap = self.store.plugin_keymap();
        let status = self.store.state().plugin_ui.as_ref().map(|ui| {
            ui.status
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        });
        self.feed.send_commands(commands);
        // A snapshot that moved for other reasons must not rebuild the
        // keymap the App routes with: a rebuild clears a half-typed
        // chord, so only one whose mappings actually changed is sent.
        let data = keymap.as_ref().map(kage_core::keymap::Keymap::to_data);
        if self.last_keymap != data {
            self.last_keymap = data;
            self.feed.send_keymap(keymap);
        }
        if let Some(status) = status {
            self.feed.send_status(status);
        }
    }

    /// Shows the dialogs the agent asked of this client. One this
    /// build cannot show is dismissed at once, so the parked plugin
    /// call does not hang.
    fn dialogs_moved(&mut self) {
        let asks = self.store.state().dialogs.clone();
        for ask in asks {
            if !self.dialog_shown.insert(ask.request_id.clone()) {
                continue;
            }
            let (reply_tx, reply_rx) = mpsc::channel();
            if let Some(dialog) = plugin_dialog(&ask, reply_tx) {
                let _ = self.dialog_tx.send(dialog);
                self.dialogs.push((ask.request_id.clone(), reply_rx));
            } else {
                self.dialog_shown.remove(&ask.request_id);
                self.store
                    .client()
                    .reply_dialog(ask.request_id, Value::Null);
            }
        }
        let open: HashSet<RequestId> = self
            .store
            .state()
            .dialogs
            .iter()
            .map(|ask| ask.request_id.clone())
            .collect();
        self.dialog_shown.retain(|id| open.contains(id));
        self.dialogs.retain(|(id, _)| open.contains(id));
    }

    /// Forwards the answers the App's overlays produced.
    fn poll_dialogs(&mut self) {
        let mut answered = Vec::new();
        for (index, (id, reply)) in self.dialogs.iter().enumerate() {
            if let Ok(answer) = reply.try_recv() {
                self.store
                    .client()
                    .reply_dialog(id.clone(), answer.unwrap_or(Value::Null));
                answered.push(index);
            }
        }
        for index in answered.into_iter().rev() {
            self.dialogs.swap_remove(index);
        }
    }

    fn export(&mut self, markdown: String) {
        let path = self.pending_export.take().unwrap_or_else(|| {
            PathBuf::from(format!("{}.md", self.wire.get(..8).unwrap_or("kage")))
        });
        match std::fs::write(&path, markdown) {
            Ok(()) => self.notice(NoticeLevel::Success, format!("wrote {}", path.display())),
            Err(error) => self.notice(NoticeLevel::Error, format!("export: {error}")),
        }
    }

    fn state_snapshot(&self) -> SessionState {
        let Some(session) = self.store.state().session(&self.wire) else {
            return SessionState::default();
        };
        let option = |id: &str| {
            session
                .config_options
                .iter()
                .find(|option| option.id == id)
                .map(|option| option.current_value.clone())
        };
        let thinking_value = option("thinking");
        let thinking = thinking_value.as_deref().and_then(ThinkingLevel::parse);
        let thinking_levels = session
            .config_options
            .iter()
            .find(|option| option.id == "thinking")
            .map(|option| {
                option
                    .options
                    .iter()
                    .filter_map(|choice| ThinkingLevel::parse(&choice.value))
                    .collect()
            })
            .unwrap_or_default();
        let mode = option("mode");
        let permission_mode = mode.as_deref().and_then(|mode| match mode {
            "allow" => Some(kage_core::permissions::PermissionAction::Allow),
            "ask" => Some(kage_core::permissions::PermissionAction::Ask),
            "deny" => Some(kage_core::permissions::PermissionAction::Deny),
            _ => None,
        });
        SessionState {
            model: option("model").unwrap_or_default(),
            thinking,
            thinking_effective: thinking,
            thinking_levels,
            input: kage_core::Modalities::default(),
            permission_mode,
            working: session.running || session.compacting,
            swarm: option("swarm").as_deref() == Some("on"),
            goal: option("goal").filter(|goal| !goal.is_empty()),
            plan: mode.as_deref() == Some("plan"),
            shells: 0,
        }
    }

    fn usage_snapshot(&self) -> kage_core::protocol::Usage {
        let Some(session) = self.store.state().session(&self.wire) else {
            return kage_core::protocol::Usage::default();
        };
        let tokens = session.usage.tokens.unwrap_or_default();
        kage_core::protocol::Usage {
            total: kage_core::TokenUsage {
                input: tokens.input,
                output: tokens.output,
                cache_read: tokens.cache_read,
                cache_write: tokens.cache_write,
            },
            context_used: session.usage.used,
            context_window: session.usage.size,
            cost: session.usage.cost.as_ref().map_or(0.0, |cost| cost.amount),
            context_estimated: false,
        }
    }

    fn handle(&mut self, request: RunRequest) {
        if self.closed {
            self.notice(NoticeLevel::Error, DETACHED.to_owned());
            return;
        }
        match request {
            RunRequest::Submit {
                text,
                images,
                queue,
                ..
            } => self.submit(text, images, queue),
            RunRequest::Cancel { .. } => self.store.client().cancel(&self.wire),
            RunRequest::RecallPrompt { delivery, .. } => self.recall(delivery),
            RunRequest::SwitchModel(model) => {
                self.model.clone_from(&model);
                self.store
                    .client()
                    .set_config_option(&self.wire, "model", &model);
            }
            RunRequest::ResolvePermission {
                request_id,
                decision,
            } => self.resolve_permission(request_id, decision),
            RunRequest::AnswerQuestion {
                request_id,
                answers,
            } => self.answer_question(request_id, answers),
            RunRequest::CycleThinkingLevel => self.cycle_thinking(),
            RunRequest::SetThinkingLevel(level) => {
                let value: &str = if level.is_empty() { "auto" } else { &level };
                self.store
                    .client()
                    .set_config_option(&self.wire, "thinking", value);
            }
            RunRequest::SetPermissionMode(mode) => {
                let value = match mode {
                    Some(kage_core::permissions::PermissionAction::Ask) => "ask",
                    Some(kage_core::permissions::PermissionAction::Deny) => "deny",
                    Some(kage_core::permissions::PermissionAction::Allow) => "allow",
                    None => "default",
                };
                self.store
                    .client()
                    .set_config_option(&self.wire, "mode", value);
            }
            RunRequest::PlanMode { on } => {
                self.store.client().set_config_option(
                    &self.wire,
                    "mode",
                    if on { "plan" } else { "default" },
                );
            }
            RunRequest::SwarmMode { on } => {
                self.store.client().set_config_option(
                    &self.wire,
                    "swarm",
                    if on { "on" } else { "off" },
                );
            }
            RunRequest::SetGoal { goal } => {
                self.store.client().set_config_option(
                    &self.wire,
                    "goal",
                    goal.as_deref().unwrap_or(""),
                );
            }
            RunRequest::CompactNow => {
                self.store.client().compact_session(&self.wire);
            }
            RunRequest::ExportSession(path) => {
                self.pending_export = path;
                self.store.client().export_session(&self.wire);
            }
            RunRequest::InvokePluginCommand { name, args } => {
                self.store
                    .client()
                    .ui_command(&self.cwd, &name, &args, Some(&self.wire));
            }
            RunRequest::InvokeKeymap { id } => {
                self.store
                    .client()
                    .ui_keymap(&self.cwd, id, Some(&self.wire));
            }
            RunRequest::NewSession => {
                self.pending_new = true;
                let meta = self.session_meta();
                self.store.client().new_session(&self.cwd, &[], Some(meta));
            }
            RunRequest::ResumeSession(path) => self.open_recorded(&path),
            RunRequest::SwitchSession(SwitchTarget::Session(target)) => {
                match super::support::resolve_switch_target(&target) {
                    Ok(path) => self.open_recorded(&path),
                    Err(error) => self.notice(NoticeLevel::Error, error),
                }
            }
            other => self.refuse(&other),
        }
    }

    fn submit(&mut self, text: String, images: Vec<kage_tui::image::AttachedImage>, queue: bool) {
        let _ = crate::history::append(&text);
        let mut blocks = Vec::new();
        if !text.is_empty() || images.is_empty() {
            blocks.push(ContentBlock::Text(TextContent { text }));
        }
        let mut dropped = 0usize;
        for image in images {
            match &image.source {
                MediaSource::Base64 { data } => blocks.push(ContentBlock::Image(BlobContent {
                    data: data.clone(),
                    mime_type: image.mime.clone(),
                    uri: None,
                })),
                MediaSource::Url { url } => blocks.push(ContentBlock::Image(BlobContent {
                    data: String::new(),
                    mime_type: image.mime.clone(),
                    uri: Some(url.clone()),
                })),
                // Bytes the session blob store holds: this side has no
                // store, so the attachment cannot ride the link.
                MediaSource::Blob { .. } => dropped += 1,
            }
        }
        if dropped > 0 {
            let (noun, verb) = if dropped == 1 {
                ("attachment", "was")
            } else {
                ("attachments", "were")
            };
            self.notice(
                NoticeLevel::Warning,
                format!("{dropped} {noun} cannot ride the ACP link and {verb} dropped"),
            );
        }
        if queue {
            self.store.client().prompt(&self.wire, blocks);
            return;
        }
        // Steering joins the run in flight; anything else goes plain,
        // which queues while a run is in flight and starts one
        // otherwise.
        let running = self
            .store
            .state()
            .session(&self.wire)
            .is_some_and(|session| session.running);
        if running
            && self
                .store
                .client()
                .steer(&self.wire, blocks.clone())
                .is_ok()
        {
            return;
        }
        self.store.client().prompt(&self.wire, blocks);
    }

    fn recall(&mut self, delivery: Delivery) {
        match delivery {
            Delivery::Queue => {
                let held = self
                    .store
                    .state()
                    .session(&self.wire)
                    .and_then(|session| session.queue.last())
                    .map(|queued| queued.prompt.clone());
                if let Some(blocks) = &held {
                    let index = self
                        .store
                        .state()
                        .session(&self.wire)
                        .map_or(0, |session| session.queue.len().saturating_sub(1));
                    self.store.client().withdraw_queued(&self.wire, index);
                    self.publish(HostEvent::PromptWithdrawn {
                        delivery,
                        content: Some(content_of(blocks)),
                    });
                } else {
                    self.publish(HostEvent::PromptWithdrawn {
                        delivery,
                        content: None,
                    });
                }
            }
            // A steered prompt is already on the wire: there is
            // nothing to take back.
            Delivery::Steer => self.publish(HostEvent::PromptWithdrawn {
                delivery,
                content: None,
            }),
        }
    }

    fn resolve_permission(
        &mut self,
        request_id: kage_core::protocol::RequestId,
        decision: kage_tui::PermissionDecision,
    ) {
        let Some(wire_id) = self.ask_ids.get(&request_id.0).cloned() else {
            return;
        };
        let answer = match decision {
            kage_tui::PermissionDecision::AllowOnce => PermissionDecision::Allow,
            kage_tui::PermissionDecision::AllowSession
            | kage_tui::PermissionDecision::AllowAlways => {
                let has_always = self
                    .store
                    .state()
                    .session(&self.wire)
                    .and_then(|session| {
                        session
                            .permissions
                            .iter()
                            .find(|ask| ask.request_id == wire_id)
                    })
                    .is_some_and(|ask| ask.option_of(PermissionOptionKind::AllowAlways).is_some());
                if has_always {
                    PermissionDecision::AllowAlways
                } else {
                    PermissionDecision::Allow
                }
            }
            kage_tui::PermissionDecision::Deny => PermissionDecision::Reject,
        };
        self.store
            .client()
            .reply_permission(&self.wire, wire_id, &answer);
    }

    /// Answers the one question each wire ask carries. A row picks the
    /// matching `choice-<n>` option; the written answer rides the
    /// `_meta.kage.question` channel either way, and an empty row
    /// cancels.
    fn answer_question(
        &mut self,
        request_id: kage_core::protocol::RequestId,
        answers: Option<Vec<Vec<String>>>,
    ) {
        let Some(wire_id) = self.ask_ids.get(&request_id.0).cloned() else {
            return;
        };
        let row = answers.and_then(|rows| rows.into_iter().next());
        let Some(row) = row.filter(|row| !row.is_empty()) else {
            self.store
                .client()
                .reply_permission(&self.wire, wire_id, &PermissionDecision::Cancel);
            return;
        };
        let option_id = self
            .store
            .state()
            .session(&self.wire)
            .and_then(|session| {
                session
                    .permissions
                    .iter()
                    .find(|ask| ask.request_id == wire_id)
            })
            .and_then(|ask| ask.question.as_ref())
            .and_then(|prompt| {
                row.iter()
                    .find_map(|label| {
                        prompt
                            .options
                            .iter()
                            .position(|choice| &choice.label == label)
                    })
                    .map(|index| format!("choice-{index}"))
            })
            .unwrap_or_else(|| "skip".to_owned());
        let answer = PermissionDecision::Answer {
            option_id,
            answer: row,
        };
        self.store
            .client()
            .reply_permission(&self.wire, wire_id, &answer);
    }

    fn cycle_thinking(&mut self) {
        let current = {
            let Some(session) = self.store.state().session(&self.wire) else {
                return;
            };
            let Some(option) = session
                .config_options
                .iter()
                .find(|option| option.id == "thinking")
            else {
                self.notice(
                    NoticeLevel::Warning,
                    "this model has no thinking levels".to_owned(),
                );
                return;
            };
            let ladder: Vec<String> = option
                .options
                .iter()
                .map(|choice| choice.value.clone())
                .filter(|value| value != "auto")
                .collect();
            if ladder.is_empty() {
                self.notice(
                    NoticeLevel::Warning,
                    "this model has no thinking levels".to_owned(),
                );
                return;
            }
            Some(match option.current_value.as_str() {
                "" | "auto" => ladder[0].clone(),
                value => ladder.iter().position(|level| level == value).map_or_else(
                    || ladder[0].clone(),
                    |index| ladder[(index + 1) % ladder.len()].clone(),
                ),
            })
        };
        if let Some(next) = current {
            self.store
                .client()
                .set_config_option(&self.wire, "thinking", &next);
        }
    }

    /// Replays a recorded session: the wire id is the file stem, the
    /// App gets a fresh buffer, and the replay folds into it once the
    /// agent answers.
    fn open_recorded(&mut self, path: &Path) {
        let Some(id) = path
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .map(str::to_owned)
        else {
            self.notice(
                NoticeLevel::Error,
                "that session has no id to open".to_owned(),
            );
            return;
        };
        if self.pending_open.as_ref().is_some_and(|open| *open == id) || id == self.wire {
            return;
        }
        if self.running {
            self.store.client().cancel(&self.wire);
            self.running = false;
            self.publish(HostEvent::RunEnded {
                outcome: RunOutcome::Cancelled,
            });
        }
        // The session the App leaves is closed on the agent side, so
        // the shared host does not keep it loaded for a TUI that moved
        // on. The switch is already committed to `id` by now.
        if !self.wire.is_empty() {
            self.store.client().close_session(&self.wire);
        }
        let fresh = self.store.reset_session(&id);
        self.handed = Some(Arc::clone(&fresh));
        self.feed.send_buffer(fresh);
        self.close_ask_cards();
        self.pending_open = Some(id);
        let opening = self.pending_open.clone().expect("just set");
        self.store.client().load_session(&opening, &self.cwd, &[]);
    }

    fn refuse(&mut self, request: &RunRequest) {
        let what = match request {
            RunRequest::RunShell(_) => "shell escapes",
            RunRequest::ForkSession { .. }
            | RunRequest::Rewind { .. }
            | RunRequest::UndoSession
            | RunRequest::ListPrompts => "rewinding and forking the live session",
            RunRequest::CloneSession => "cloning the session",
            RunRequest::Image { .. } => "opening or saving images",
            RunRequest::Media { .. } => "opening or saving media",
            RunRequest::RefreshProviders
            | RunRequest::ReloadPlugins
            | RunRequest::RestartMcp(_) => "reloading providers, plugins and MCP servers",
            RunRequest::ForkSessionFile(_) | RunRequest::DeleteSession(_) => {
                "forking or deleting session files"
            }
            RunRequest::SwitchSession(SwitchTarget::PendingFork(_)) => "that switch",
            _ => "that command",
        };
        self.notice(
            NoticeLevel::Error,
            format!("{what} is not available over the ACP link yet"),
        );
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.store.interrupt_running();
        if self.running {
            self.running = false;
            self.publish(HostEvent::RunEnded {
                outcome: RunOutcome::Cancelled,
            });
        }
        // No card survives a dead agent: the asks can never be
        // answered now.
        self.close_ask_cards();
        self.dialogs.clear();
        self.dialog_shown.clear();
        // The agent's last stderr lines say why it died, when it said
        // anything.
        let mut text = DETACHED.to_owned();
        let stderr = self.transport.stderr_tail();
        if !stderr.trim().is_empty() {
            text.push_str(": ");
            text.push_str(stderr.trim_end());
        }
        self.notice(NoticeLevel::Error, text);
    }

    fn publish(&mut self, event: impl Into<kage_core::protocol::Event>) {
        self.seq += 1;
        let _ = self.events.send(Envelope {
            session: self.app_session,
            seq: self.seq,
            event: event.into(),
        });
    }

    fn notice(&mut self, level: NoticeLevel, text: String) {
        self.publish(HostEvent::Notice {
            level,
            text,
            transient: false,
        });
    }
}

/// The wire blocks of a prompt as the App's messages: text and the
/// media it can draw.
fn content_of(blocks: &[ContentBlock]) -> Vec<Content> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(Content::Text {
                text: text.text.clone(),
            }),
            ContentBlock::Image(blob) => Some(Content::Image {
                source: media_source(blob),
                mime: blob.mime_type.clone(),
            }),
            ContentBlock::Audio(blob) => Some(Content::Audio {
                source: media_source(blob),
                mime: blob.mime_type.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn media_source(blob: &BlobContent) -> MediaSource {
    match blob.uri.as_deref() {
        Some(url) => MediaSource::Url {
            url: url.to_owned(),
        },
        None => MediaSource::Base64 {
            data: blob.data.clone(),
        },
    }
}

/// The dialog the App shows for a `_kage/ui/dialog` ask, or `None`
/// when the kind is one this build cannot show.
fn plugin_dialog(ask: &DialogAsk, reply: mpsc::Sender<Option<Value>>) -> Option<PluginDialog> {
    match ask.kind.as_str() {
        "ui.select" => {
            let select = kage_plugin::ui::SelectRequest::from_payload(&ask.payload).ok()?;
            Some(PluginDialog::Select {
                title: select.title,
                items: select.items,
                reply,
            })
        }
        "ui.confirm" => {
            let confirm = kage_plugin::ui::ConfirmRequest::from_payload(&ask.payload).ok()?;
            Some(PluginDialog::Confirm {
                title: confirm.title,
                message: confirm.message,
                reply,
            })
        }
        "ui.input" => {
            let input = kage_plugin::ui::InputRequest::from_payload(&ask.payload).ok()?;
            Some(PluginDialog::Input {
                title: input.title,
                placeholder: input.placeholder,
                reply,
            })
        }
        "ui.editor" => {
            let editor = kage_plugin::ui::EditorRequest::from_payload(&ask.payload).ok()?;
            Some(PluginDialog::Editor {
                title: editor.title,
                prefill: editor.prefill,
                reply,
            })
        }
        _ => None,
    }
}

/// Runs the TUI over an ACP link to `kage rpc`, on the recorded
/// session at `resume` when given. Returns the exit code once the user
/// quits.
#[expect(clippy::too_many_lines, reason = "one linear startup sequence")]
pub(crate) fn run(
    model: Option<&str>,
    system: &str,
    resume: Option<&Path>,
    yolo: bool,
) -> ExitCode {
    let registry = match crate::build_provider_registry() {
        Ok(registry) => Arc::new(registry),
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let buffer = shared_buffer();
    let toasts = shared_toasts();
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    kage_tui::theme::detect_terminal_background();
    let Some((config, options)) = startup_config(&workdir, &buffer) else {
        return ExitCode::from(1);
    };
    let shown_model = model.map_or_else(|| crate::default_model(&registry), str::to_owned);
    // No plugins load here: Lua runs in the agent, so keybindings,
    // options and themes work but plugin commands come over the wire.
    let log_publisher: Arc<OnceLock<LogPublisher>> = Arc::default();
    let plugins = match setup_user_runtime(
        None,
        None,
        config.plugins.clone(),
        config.keybindings.clone(),
        Arc::clone(&options),
        &workdir,
        &shown_model,
        "",
        buffer_host_log(buffer.clone(), toasts.clone(), Arc::clone(&log_publisher)),
        &kage_core::environment::Environment::inherit(),
    ) {
        Ok(rt) => Some(rt),
        Err(e) => {
            lock(&buffer).push_custom("kage:error", e, false);
            None
        }
    };
    let app_session = SessionId::new();
    let store = Store::new();
    let (frames_tx, frames_rx) = mpsc::channel();
    let transport = match Transport::start(frames_tx) {
        Ok(transport) => transport,
        Err(error) => {
            eprintln!("kage: {error}");
            return ExitCode::from(1);
        }
    };
    let (events_tx, events_rx) = mpsc::channel();
    let (requests_tx, requests_rx) = mpsc::channel::<RunRequest>();
    let (dialog_tx, dialog_rx) = mpsc::channel::<PluginDialog>();
    // Plugin hot reload never fires here: there is no local runtime.
    let (refresh_tx, refresh_rx) = mpsc::channel::<PluginRefresh>();
    drop(refresh_tx);
    let feed = Arc::new(FeedState::default());
    let mut pump = Pump {
        store,
        transport,
        frames: frames_rx,
        events: events_tx.clone(),
        requests: requests_rx,
        dialog_tx,
        dialogs: Vec::new(),
        feed: Arc::clone(&feed),
        app_session,
        wire: String::new(),
        pending_open: None,
        pending_new: false,
        cwd: workdir.display().to_string(),
        model: shown_model.clone(),
        options: Arc::clone(&options),
        registry: Arc::clone(&registry),
        yolo,
        system_prompt: system.to_owned(),
        seq: 0,
        running: false,
        handed: None,
        last_keymap: None,
        forwarded: HashSet::new(),
        dialog_shown: HashSet::new(),
        ask_ids: BTreeMap::new(),
        next_ask: 0,
        pending_export: None,
        unknown: HashSet::new(),
        closed: false,
    };
    pump.bootstrap(resume);
    // The buffer the App draws first: a resumed session's replay
    // target, or a placeholder until the new session's answer adopts
    // the wire and the feed swaps the store's buffer in.
    let root = if pump.wire.is_empty() {
        shared_buffer()
    } else {
        pump.store.shared_buffer(&pump.wire)
    };
    let log_events = events_tx.clone();
    let _ = log_publisher.set(Box::new(move |level, message| {
        let level = match level {
            kage_plugin::LogLevel::Error => NoticeLevel::Error,
            _ => NoticeLevel::Info,
        };
        let _ = log_events.send(Envelope {
            session: app_session,
            seq: 0,
            event: HostEvent::Notice {
                level,
                text: message.to_owned(),
                transient: false,
            }
            .into(),
        });
    }));
    if let Err(error) = thread::Builder::new()
        .name("kage-acp-pump".to_owned())
        .spawn(move || pump.serve())
    {
        // The transport drops here and kills the child.
        eprintln!("kage: cannot start the pump thread: {error}");
        return ExitCode::from(1);
    }
    let mirror = Arc::new(Mutex::new(host::Mirror::new(None)));
    let model_choices = available_model_items(&registry, &shown_model);
    Frontend {
        buffer: root,
        toasts,
        options,
        config,
        workdir,
        plugins,
        mirror,
        requests: requests_tx,
        events: events_rx,
        dialogs: dialog_rx,
        refresh: refresh_rx,
        acp_feed: Some(feed),
        model: shown_model,
        model_choices,
        plugin_commands: Vec::new(),
        skills: Vec::new(),
        notices: Vec::new(),
        session: app_session,
    }
    .run(|| {})
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Duration;

    use kage_client::RequestId;

    /// Writes a child that echoes every line back and then ignores
    /// stdin EOF, standing in for `kage rpc`. Hanging past EOF is the
    /// point: the close the test sees after a drop can only come from
    /// the drop killing the child. Run it through
    /// [`echo_child_program`] rather than exec'ing the script: a
    /// just-written file is briefly still open for writing somewhere
    /// in a forking process, and an exec of it loses that race as
    /// text file busy.
    fn echo_child(dir: &Path) -> String {
        let script = dir.join("echo-kage-rpc");
        std::fs::write(
            &script,
            "#!/bin/sh\nwhile read -r line; do printf '%s\\n' \"$line\"; done\nwhile :; do sleep 0.1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script.display().to_string()
    }

    /// The interpreter the stand-in child runs under.
    fn echo_child_program() -> std::path::PathBuf {
        PathBuf::from("sh")
    }

    /// A fresh directory for the stand-in child, unique per call so
    /// parallel tests never share one.
    fn scratch() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kage-acp-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn frames_round_trip_through_the_child() {
        let dir = scratch();
        let (events, received) = mpsc::channel();
        let transport = spawn(&echo_child_program(), &[echo_child(&dir)], events).unwrap();
        transport.send(Frame::Request {
            id: RequestId::Number(1),
            method: "initialize".to_owned(),
            params: serde_json::json!({"protocolVersion": 1}),
        });
        // Generous waits: the workspace suite runs this beside
        // hundreds of other tests, and a slow machine must not fail
        // it.
        let wait = Duration::from_secs(30);
        match received.recv_timeout(wait) {
            Ok(Event::Frame(Frame::Request { method, .. })) => {
                assert_eq!(method, "initialize");
            }
            other => panic!("expected the echo, got {other:?}"),
        }
        drop(transport);
        // The child ignores stdin EOF, so this close can only be the
        // drop's kill.
        loop {
            match received.recv_timeout(wait) {
                Ok(Event::Frame(_)) => {}
                Ok(Event::Closed) => break,
                other => panic!("expected the close, got {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_program_refuses_without_events() {
        let (events, received) = mpsc::channel();
        let Err(error) = spawn(Path::new("/nonexistent/kage-acp-missing"), &[], events) else {
            panic!("a missing program does not start")
        };
        assert!(error.contains("cannot spawn"), "{error}");
        assert!(
            received.recv_timeout(Duration::from_secs(1)).is_err(),
            "a refusal sends no events"
        );
    }

    /// A pump on a stand-in child, with the channels the App would
    /// hold. The pump is driven method by method, so the child never
    /// speaks.
    fn test_pump() -> (
        Pump,
        mpsc::Receiver<Envelope>,
        mpsc::Receiver<PluginDialog>,
        Arc<FeedState>,
        PathBuf,
    ) {
        let dir = scratch();
        let (frames_tx, frames_rx) = mpsc::channel();
        let transport = spawn(&echo_child_program(), &[echo_child(&dir)], frames_tx).unwrap();
        let (events, app) = mpsc::channel();
        let (_requests, requests_rx) = mpsc::channel::<RunRequest>();
        let (dialog_tx, dialog_rx) = mpsc::channel::<PluginDialog>();
        let feed = Arc::new(FeedState::default());
        let pump = Pump {
            store: Store::new(),
            transport,
            frames: frames_rx,
            events,
            requests: requests_rx,
            dialog_tx,
            dialogs: Vec::new(),
            feed: Arc::clone(&feed),
            app_session: SessionId::new(),
            wire: "s1".to_owned(),
            pending_open: None,
            pending_new: false,
            cwd: "/w".to_owned(),
            model: "test:model".to_owned(),
            options: Arc::new(Mutex::new(kage_core::options::OptionStore::default())),
            registry: Arc::new(ProviderRegistry::default()),
            yolo: false,
            system_prompt: String::new(),
            seq: 0,
            running: false,
            handed: None,
            last_keymap: None,
            forwarded: HashSet::new(),
            dialog_shown: HashSet::new(),
            ask_ids: BTreeMap::new(),
            next_ask: 0,
            pending_export: None,
            unknown: HashSet::new(),
            closed: false,
        };
        (pump, app, dialog_rx, feed, dir)
    }

    /// The question ask the agent sends for an `ask_user_question`
    /// call.
    fn question_frame() -> Frame {
        Frame::Request {
            id: RequestId::Number(9),
            method: "session/request_permission".into(),
            params: serde_json::json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "call-q", "title": "ask_user_question",
                    "kind": "other", "status": "pending",
                    "rawInput": {"question": "Which auth?"}},
                "options": [
                    {"optionId": "choice-0", "name": "API key", "kind": "allow_once"},
                    {"optionId": "skip", "name": "Skip", "kind": "reject_once"},
                ],
                "_meta": {"kage": {"question": {"prompt": {
                    "header": "Auth",
                    "question": "How should the agent sign in?",
                    "options": [
                        {"label": "API key"},
                        {"label": "OAuth", "description": "opens a browser"},
                    ],
                    "multiSelect": false,
                }}}},
            }),
        }
    }

    #[test]
    fn a_question_ask_becomes_a_question_panel() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let changes = pump.store.absorb(question_frame());
        for change in changes {
            pump.react(change);
        }
        let envelope = app.recv_timeout(Duration::from_secs(5)).unwrap();
        match envelope.event {
            kage_core::protocol::Event::Host(HostEvent::QuestionAsked {
                request_id,
                questions,
                ..
            }) => {
                assert_eq!(request_id, kage_core::protocol::RequestId(0));
                assert_eq!(questions.len(), 1);
                assert_eq!(questions[0].header, "Auth");
                assert_eq!(questions[0].options.len(), 2);
                assert_eq!(questions[0].options[1].label, "OAuth");
            }
            other => panic!("expected the question, got {other:?}"),
        }
        // The panel's answer maps the picked label back to the wire's
        // `choice-<n>` option and rides the written answer.
        pump.handle(RunRequest::AnswerQuestion {
            request_id: kage_core::protocol::RequestId(0),
            answers: Some(vec![vec!["API key".to_owned()]]),
        });
        let outgoing = pump.store.take_outgoing();
        let Frame::Success { id, result } = &outgoing[0] else {
            panic!("the answer replies, got {:?}", outgoing[0])
        };
        assert_eq!(*id, RequestId::Number(9));
        assert_eq!(result["outcome"]["optionId"], "choice-0", "{result}");
        assert_eq!(
            result["_meta"]["kage"]["question"]["answer"][0], "API key",
            "{result}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ask_shows_a_card_and_allow_session_falls_back_to_allow() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let changes = pump.store.absorb(Frame::Request {
            id: RequestId::Number(7),
            method: "session/request_permission".into(),
            params: serde_json::json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "call-sh", "title": "shell",
                    "kind": "execute", "status": "pending",
                    "rawInput": {"command": "cargo test"}},
                "options": [
                    {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                ],
            }),
        });
        for change in changes {
            pump.react(change);
        }
        let envelope = app.recv_timeout(Duration::from_secs(5)).unwrap();
        match envelope.event {
            kage_core::protocol::Event::Host(HostEvent::PermissionRequested {
                request_id,
                tool,
                subject,
                ..
            }) => {
                assert_eq!(request_id, kage_core::protocol::RequestId(0));
                assert_eq!(tool, "shell");
                assert_eq!(subject, "cargo test");
            }
            other => panic!("expected the ask, got {other:?}"),
        }
        // The session offers no allow-always option, so a session-long
        // verdict answers the allow-once one instead of vanishing.
        pump.handle(RunRequest::ResolvePermission {
            request_id: kage_core::protocol::RequestId(0),
            decision: kage_tui::PermissionDecision::AllowSession,
        });
        let outgoing = pump.store.take_outgoing();
        let Frame::Success { result, .. } = &outgoing[0] else {
            panic!("the verdict replies, got {:?}", outgoing[0])
        };
        assert_eq!(result["outcome"]["optionId"], "allow", "{result}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dialog_ask_reaches_the_app_and_its_answer_replies() {
        let (mut pump, _app, dialogs, _feed, dir) = test_pump();
        let changes = pump.store.absorb(Frame::Request {
            id: RequestId::Number(11),
            method: "_kage/ui/dialog".into(),
            params: serde_json::json!({
                "kind": "ui.select",
                "payload": {"title": "Pick one", "items": ["a", "b"]},
            }),
        });
        for change in changes {
            pump.react(change);
        }
        match dialogs.recv_timeout(Duration::from_secs(5)).unwrap() {
            PluginDialog::Select {
                title,
                items,
                reply,
            } => {
                assert_eq!(title, "Pick one");
                assert_eq!(items.len(), 2);
                let _ = reply.send(Some(serde_json::json!("b")));
            }
            PluginDialog::Confirm { .. }
            | PluginDialog::Input { .. }
            | PluginDialog::Editor { .. } => {
                panic!("expected the select")
            }
        }
        pump.poll_dialogs();
        let outgoing = pump.store.take_outgoing();
        let Frame::Success { id, result } = &outgoing[0] else {
            panic!("the answer replies, got {:?}", outgoing[0])
        };
        assert_eq!(*id, RequestId::Number(11));
        assert_eq!(result, "b", "{result}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_settings_ride_the_new_session_meta() {
        let options: kage_plugin::SharedOptions =
            Arc::new(Mutex::new(kage_core::options::OptionStore::default()));
        lock(&options)
            .set(
                "compaction_threshold",
                // Off the default of 0.8: a value the default already
                // holds never changes its source.
                OptionValue::Float(0.6),
                kage_core::options::OptionSource::Lua,
            )
            .unwrap();
        lock(&options)
            .set(
                "max_provider_retries",
                OptionValue::Int(2),
                kage_core::options::OptionSource::Lua,
            )
            .unwrap();
        lock(&options)
            .set(
                "thinking_level",
                OptionValue::Str("high".to_owned()),
                kage_core::options::OptionSource::Lua,
            )
            .unwrap();
        let registry = crate::build_provider_registry().unwrap();
        let meta = session_meta(&options, &registry, "test:model", "", false);
        let kage = meta.kage.expect("the settings are set");
        assert!(!kage.yolo);
        assert!(kage.system_prompt.is_none(), "an empty system adds nothing");
        let opt = kage.opt.expect("the options are set");
        assert_eq!(opt.compaction_threshold, Some(0.6));
        assert_eq!(opt.max_provider_retries, Some(2));
        assert_eq!(
            opt.thinking_level.as_deref(),
            Some("high"),
            "the level keeps its ladder string"
        );
        // A clean store sends nothing, and a system prompt rides along.
        let clean: kage_plugin::SharedOptions =
            Arc::new(Mutex::new(kage_core::options::OptionStore::default()));
        let meta = session_meta(&clean, &registry, "test:model", "be brief", true);
        let kage = meta.kage.expect("yolo and the system prompt are set");
        assert!(kage.yolo);
        assert_eq!(kage.system_prompt.as_deref(), Some("be brief"));
        assert!(kage.opt.is_none(), "nothing set sends nothing");
    }

    #[test]
    fn prompts_map_to_the_apps_content() {
        let blocks = vec![
            ContentBlock::Text(TextContent {
                text: "look".to_owned(),
            }),
            ContentBlock::Image(BlobContent {
                data: "aGk=".to_owned(),
                mime_type: "image/png".to_owned(),
                uri: None,
            }),
            ContentBlock::Image(BlobContent {
                data: String::new(),
                mime_type: "image/jpeg".to_owned(),
                uri: Some("https://example.test/pic.jpg".to_owned()),
            }),
            ContentBlock::Audio(BlobContent {
                data: "c28=".to_owned(),
                mime_type: "audio/mpeg".to_owned(),
                uri: None,
            }),
            ContentBlock::Unknown,
        ];
        let content = content_of(&blocks);
        assert_eq!(content.len(), 4, "an unknown block contributes nothing");
        assert_eq!(
            content[0],
            Content::Text {
                text: "look".to_owned(),
            }
        );
        assert_eq!(
            content[1],
            Content::Image {
                source: MediaSource::Base64 {
                    data: "aGk=".to_owned(),
                },
                mime: "image/png".to_owned(),
            }
        );
        assert_eq!(
            content[2],
            Content::Image {
                source: MediaSource::Url {
                    url: "https://example.test/pic.jpg".to_owned(),
                },
                mime: "image/jpeg".to_owned(),
            }
        );
        assert!(matches!(
            content[3],
            Content::Audio {
                source: MediaSource::Base64 { .. },
                ..
            }
        ));
    }

    #[test]
    fn dialogs_parse_the_four_kinds_and_refuse_others() {
        let (reply, _answer) = mpsc::channel();
        let select = plugin_dialog(
            &DialogAsk {
                request_id: RequestId::Number(1),
                kind: "ui.select".to_owned(),
                payload: serde_json::json!({"title": "t", "items": ["a"]}),
            },
            reply.clone(),
        );
        assert!(matches!(select, Some(PluginDialog::Select { .. })));
        let confirm = plugin_dialog(
            &DialogAsk {
                request_id: RequestId::Number(2),
                kind: "ui.confirm".to_owned(),
                payload: serde_json::json!({"title": "t", "message": "sure?"}),
            },
            reply.clone(),
        );
        assert!(matches!(confirm, Some(PluginDialog::Confirm { .. })));
        let input = plugin_dialog(
            &DialogAsk {
                request_id: RequestId::Number(3),
                kind: "ui.input".to_owned(),
                payload: serde_json::json!({"title": "t", "placeholder": "p"}),
            },
            reply.clone(),
        );
        assert!(matches!(input, Some(PluginDialog::Input { .. })));
        let editor = plugin_dialog(
            &DialogAsk {
                request_id: RequestId::Number(4),
                kind: "ui.editor".to_owned(),
                payload: serde_json::json!({"title": "t", "prefill": "hi"}),
            },
            reply,
        );
        assert!(matches!(editor, Some(PluginDialog::Editor { .. })));
        let unknown = plugin_dialog(
            &DialogAsk {
                request_id: RequestId::Number(5),
                kind: "ui.progress".to_owned(),
                payload: serde_json::json!({}),
            },
            mpsc::channel().0,
        );
        assert!(unknown.is_none(), "an unknown kind is dismissed");
    }

    #[test]
    fn an_idle_submit_sends_a_plain_prompt() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        pump.handle(RunRequest::Submit {
            text: "hello".to_owned(),
            images: Vec::new(),
            queue: false,
            session: None,
        });
        let outgoing = pump.store.take_outgoing();
        let Frame::Request { method, params, .. } = &outgoing[0] else {
            panic!("the submit sends a request, got {:?}", outgoing[0])
        };
        assert_eq!(method, "session/prompt");
        assert_eq!(params["sessionId"], "s1");
        assert_eq!(params["prompt"][0]["text"], "hello");
        assert!(app.try_recv().is_err(), "an idle submit is never refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_close_ends_the_run_and_closes_the_cards() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        pump.handle(RunRequest::Submit {
            text: "go".to_owned(),
            images: Vec::new(),
            queue: false,
            session: None,
        });
        pump.flush();
        let changes = pump.store.absorb(Frame::Request {
            id: RequestId::Number(7),
            method: "session/request_permission".into(),
            params: serde_json::json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "call-sh", "title": "shell",
                    "kind": "execute", "status": "pending",
                    "rawInput": {"command": "cargo test"}},
                "options": [
                    {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                ],
            }),
        });
        for change in changes {
            pump.react(change);
        }
        pump.close();
        let mut ended = false;
        let mut resolved = false;
        // The run start and the ask ride their own envelopes on the
        // way; the close must add the end, the card's resolution and
        // the detached notice.
        while let Ok(envelope) = app.recv_timeout(Duration::from_secs(5)) {
            match envelope.event {
                kage_core::protocol::Event::Host(HostEvent::RunEnded { outcome }) => {
                    assert_eq!(outcome, RunOutcome::Cancelled);
                    ended = true;
                }
                kage_core::protocol::Event::Host(HostEvent::PermissionResolved { .. }) => {
                    resolved = true;
                }
                kage_core::protocol::Event::Host(HostEvent::Notice { text, .. }) => {
                    assert!(text.starts_with(DETACHED), "{text}");
                    break;
                }
                _ => {}
            }
        }
        assert!(ended, "the run in flight ends as cancelled");
        assert!(resolved, "the open card is resolved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cold_start_adopts_its_first_session_without_closing_anything() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        // The state run() hands the pump on a cold start: no session
        // adopted yet, nothing on the wire.
        pump.wire.clear();
        pump.pending_new = true;
        let meta = pump.session_meta();
        pump.store.client().new_session(&pump.cwd, &[], Some(meta));
        let outgoing = pump.store.take_outgoing();
        let new_id = outgoing
            .iter()
            .find_map(|frame| match frame {
                Frame::Request { id, method, .. } if method == "session/new" => Some(id.clone()),
                _ => None,
            })
            .expect("bootstrap asks for a new session");
        let changes = pump.store.absorb(Frame::Success {
            id: new_id,
            result: serde_json::json!({"sessionId": "agent-1"}),
        });
        for change in changes {
            pump.react(change);
        }
        assert_eq!(pump.wire, "agent-1");
        assert!(!pump.pending_new);
        let sent = pump.store.take_outgoing();
        assert!(
            !sent.iter().any(|frame| matches!(
                frame,
                Frame::Request { method, .. } if method == "session/close"
            )),
            "an empty wire is never closed"
        );
        while let Ok(envelope) = app.try_recv() {
            assert!(
                !matches!(
                    envelope.event,
                    kage_core::protocol::Event::Host(HostEvent::Notice { .. })
                ),
                "a cold start raises no error, got {:?}",
                envelope.event
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
