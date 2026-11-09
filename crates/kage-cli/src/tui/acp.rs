//! The TUI's ACP link, to one of three agents: a `kage rpc` child of
//! this binary, the user's shared host over its socket, or a private
//! host served inside this process. The default is the shared host,
//! started when it is not running; `--standalone`, a host of another
//! kage version, and platforms without the socket serve a private host
//! over an in-memory pipe instead, and `KAGE_LINK=acp` runs a child.
//!
//! A reader thread turns the connection's lines into [`Event`]s and
//! a writer thread drains [`Frame`]s onto it. A pump thread
//! owns the [`Store`]: it feeds the agent's frames in, sends what the
//! client produces, and fabricates the signals the wire cannot carry
//! as [`Envelope`]s for the App's engine channel, because the agent
//! echoes a prompt only to its other clients and reports the run's
//! shape only through this side's own bookkeeping. What needs
//! `&mut App` waits in a [`FeedState`] the App drains as its
//! [`kage_tui::AcpFeed`]. The child's stderr is captured to a short
//! tail rather than let loose on the raw terminal, and the tail rides
//! the notice when the child dies; the socket and the private pipe
//! have no stderr. The child is this same `kage` binary, so the TUI
//! and its engine share a build; [`spawn`] runs another program
//! instead, which the tests use. A session on the shared host outlives
//! the TUI: it and a running turn stay with the host until the host
//! goes idle. The child is killed when the transport drops, which
//! happens when the user quits; a socket or pipe merely hangs up.
//!
//! The plugins load in this process: the runtime the engine path would
//! build runs beside the pump, answers the agent's
//! `_kage/plugin/{tools,call,hook,event,drain}` reverse requests from
//! it, and serves the App's plugin surfaces (commands, keymap,
//! widgets) locally. The agent learns this at initialize through
//! `plugin_host` and opens sessions with `_meta.kage.plugins =
//! "client"`. The plugin requests run in order on a worker thread
//! and are answered from there, so the pump keeps serving the App and
//! the agent's frames while a Lua tool runs; `Esc` raises the
//! worker's cancel flag. A resumed or loaded session carries the same
//! `_meta.kage` a new one does, so its settings and the plugin
//! hosting survive the switch.
//!
//! Some commands the engine path serves have no ACP counterpart yet:
//! the pump refuses them with a notice instead of dropping them
//! silently.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use kage_client::wire::{
    BlobContent, ClientCapabilities, ClientMeta, ContentBlock, EnvVariable,
    EnvironmentNotification, HttpHeader, Implementation, KageClientCapabilities, McpServer,
    McpServerHttp, McpServerStdio, NewSessionKage, NewSessionMeta, PermissionOptionKind,
    PluginCallRequest, PluginCallResult, PluginDrainQueue, PluginDrainRequest, PluginDrainResult,
    PluginEventRequest, PluginHookKind, PluginHookRequest, PluginPendingMessage, PluginPendingRole,
    PluginSessionOp, PluginToolRename, PluginToolSpec, PluginToolsRequest, PluginToolsResult,
    PluginsSource, PromptDelivery, SessionNotification, SessionOpt, SessionUpdate, SubagentState,
    TextContent, ToolCallStatus, UnconfiguredTools,
};
use kage_client::{Change, DialogAsk, Frame, PermissionDecision, RequestId, RpcError};
use kage_core::keymap::{Keymap, MappingData};
use kage_core::options::OptionValue;
use kage_core::protocol::{
    Delivery, Envelope, HostEvent, NoticeLevel, Question, QuestionOption, RunOutcome, SessionId,
    SessionState, SwarmMember,
};
use kage_core::{
    CancelFlag, LoopEvent, MediaSource, Message, MessageId, Modality, Role, ThinkingLevel,
    ToolCallId, ToolOutput,
};
use kage_plugin::events::KNOWN_EVENTS;
use kage_plugin::messages::PendingRole;
use kage_plugin::sessions::PendingSessionOp;
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
    /// The connection's read side ended: the agent is gone.
    Closed,
}

/// Which agent the TUI speaks ACP to. The shared host is the default;
/// the private host takes over where it cannot be used, and the child
/// stays for `KAGE_LINK=acp`.
pub(crate) enum Upstream {
    /// A `kage rpc` child of this same binary.
    Child,
    /// The user's shared host, over the socket a client connected to.
    #[cfg(unix)]
    Host(std::os::unix::net::UnixStream),
    /// A private host served inside this process.
    Private,
    /// The shared host unless asked or unable to use it, decided after
    /// onboarding had its turn, so a host started for a first run
    /// carries the credentials that run saved.
    Decide { standalone: bool },
}

/// Everything the threads and the handle share.
#[derive(Default)]
struct Shared {
    /// The writer thread's queue, absent once the agent is gone.
    writer: Mutex<Option<mpsc::Sender<Frame>>>,
    /// The child, so a drop can kill it. Absent on the socket and the
    /// private pipe, whose agent is a process that outlives the
    /// transport or a host in this same process.
    child: Mutex<Option<Child>>,
    /// The child's last stderr bytes, kept by the stderr thread.
    stderr: Mutex<Vec<u8>>,
    /// Shuts a socket down, which a reader blocked on it needs: its
    /// own handle keeps the connection open past the writer's drop.
    hangup: Mutex<Option<Hangup>>,
}

/// What ends a socket transport's blocked read.
type Hangup = Box<dyn FnOnce() + Send>;

/// The TUI's end of the ACP connection. Its drop kills a spawned
/// child and hangs up a socket or pipe, so it is passed by ownership,
/// never cloned.
pub(crate) struct Transport {
    shared: Arc<Shared>,
}

impl Transport {
    /// Spawns `kage rpc` beside this binary and starts its io
    /// threads.
    pub(crate) fn child(events: mpsc::Sender<Event>) -> Result<Transport, String> {
        let program = program()?;
        spawn(&program, &["rpc".to_owned()], events)
    }

    /// Connects to the shared host over the client's socket. The host
    /// serves one agent per connection, so this TUI's sessions are its
    /// own, and it runs their tools with the environment the pump
    /// announces in `_kage/environment`.
    #[cfg(unix)]
    pub(crate) fn host(
        stream: std::os::unix::net::UnixStream,
        events: mpsc::Sender<Event>,
    ) -> Result<Transport, String> {
        let reader = stream
            .try_clone()
            .map_err(|error| format!("cannot split the socket: {error}"))?;
        let hangup = shutdown_on_drop(&stream)
            .map_err(|error| format!("cannot split the socket: {error}"))?;
        let transport = Self::stream(BufReader::new(reader), stream, None, events)?;
        *transport.shared.hangup.lock().unwrap() = Some(hangup);
        Ok(transport)
    }

    /// Serves a private `rpc::Host` inside this process and connects
    /// to it over an in-memory pipe: the shape `--standalone` runs and
    /// the fallback where the shared host cannot be used. The host
    /// serves this TUI alone and is dropped with the transport.
    pub(crate) fn memory(
        model: Option<&str>,
        system: &str,
        yolo: bool,
        events: mpsc::Sender<Event>,
    ) -> Result<Transport, String> {
        let host = crate::rpc::host::Host::start(model, system, yolo)?;
        let (client_read, client_write, host_read, host_write, hangup) =
            private_pair().map_err(|error| format!("cannot open the private pipe: {error}"))?;
        let spawned = thread::Builder::new()
            .name("kage-acp-private-host".to_owned())
            .spawn(move || {
                let _ = host.serve_local(host_read, host_write);
            });
        spawned.map_err(|error| format!("cannot start the private host: {error}"))?;
        let transport = Self::stream(BufReader::new(client_read), client_write, None, events)?;
        *transport.shared.hangup.lock().unwrap() = hangup;
        Ok(transport)
    }

    /// Starts the reader and writer threads on a connected byte
    /// transport. `child` rides the shared state so the transport's
    /// drop kills it.
    fn stream(
        reader: impl BufRead + Send + 'static,
        writer: impl Write + Send + 'static,
        child: Option<Child>,
        events: mpsc::Sender<Event>,
    ) -> Result<Transport, String> {
        let (queue, frames) = mpsc::channel::<Frame>();
        let shared = Arc::new(Shared {
            writer: Mutex::new(Some(queue)),
            child: Mutex::new(child),
            stderr: Mutex::new(Vec::new()),
            hangup: Mutex::new(None),
        });
        let transport = Transport {
            shared: Arc::clone(&shared),
        };
        let shared_for_writer = Arc::clone(&shared);
        let spawned = thread::Builder::new()
            .name("kage-acp-writer".to_owned())
            .spawn(move || {
                let mut writer = writer;
                for frame in frames {
                    let line = serde_json::to_string(&frame.to_value()).expect("frame serializes");
                    if writeln!(writer, "{line}").is_err() || writer.flush().is_err() {
                        // The agent is gone; senders learn at once.
                        *shared_for_writer.writer.lock().unwrap() = None;
                        break;
                    }
                }
            });
        spawned.map_err(|error| format!("cannot start the io threads: {error}"))?;
        let spawned = thread::Builder::new()
            .name("kage-acp-reader".to_owned())
            .spawn(move || {
                let mut reader = reader;
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
        Ok(transport)
    }

    /// Queues `frame` for the agent. A frame the connection cannot
    /// take is dropped; [`Event::Closed`] is how the pump learns the
    /// agent is gone.
    pub(crate) fn send(&self, frame: Frame) {
        let writer = self.shared.writer.lock().unwrap().clone();
        if let Some(writer) = writer {
            let _ = writer.send(frame);
        }
    }

    /// The child's last stderr bytes, lossily decoded. Empty while the
    /// child said nothing on stderr; always empty on the socket and
    /// the private pipe, which have none.
    pub(crate) fn stderr_tail(&self) -> String {
        let tail = self.shared.stderr.lock().unwrap();
        String::from_utf8_lossy(&tail).into_owned()
    }
}

/// The four halves of the private host's in-memory pipe: the client
/// reads the first and writes the second, the host the other two, and
/// the hangup ends the client's blocked read where the pipe needs one.
/// A Unix stream pair on Unix, a channel pipe elsewhere.
type PrivateRead = Box<dyn BufRead + Send>;
type PrivateWrite = Box<dyn Write + Send>;
type PrivatePair = (
    PrivateRead,
    PrivateWrite,
    PrivateRead,
    PrivateWrite,
    Option<Hangup>,
);

/// A hangup that stops reads on `stream`, so a reader blocked on a
/// clone of it returns end of file. Writes already queued still go
/// out: the writer thread closes its own end once its queue is empty.
#[cfg(unix)]
fn shutdown_on_drop(stream: &std::os::unix::net::UnixStream) -> std::io::Result<Hangup> {
    let handle = stream.try_clone()?;
    Ok(Box::new(move || {
        let _ = handle.shutdown(std::net::Shutdown::Read);
    }))
}

#[cfg(unix)]
fn private_pair() -> std::io::Result<PrivatePair> {
    use std::os::unix::net::UnixStream;
    let (client, host_end) = UnixStream::pair()?;
    let host_reader = host_end.try_clone()?;
    let client_reader = client.try_clone()?;
    let hangup = shutdown_on_drop(&client)?;
    Ok((
        Box::new(BufReader::new(client_reader)),
        Box::new(client),
        Box::new(BufReader::new(host_reader)),
        Box::new(host_end),
        Some(hangup),
    ))
}

#[cfg(not(unix))]
fn private_pair() -> std::io::Result<PrivatePair> {
    let (client_write, host_read) = super::pipe::channel_pair();
    let (host_write, client_read) = super::pipe::channel_pair();
    Ok((
        Box::new(BufReader::new(client_read)),
        Box::new(client_write),
        Box::new(BufReader::new(host_read)),
        Box::new(host_write),
        None,
    ))
}

impl Drop for Transport {
    fn drop(&mut self) {
        *self.shared.writer.lock().unwrap() = None;
        if let Some(hangup) = self.shared.hangup.lock().unwrap().take() {
            hangup();
        }
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
    let transport = Transport::stream(BufReader::new(stdout), stdin, Some(child), events)?;
    let shared_for_stderr = Arc::clone(&transport.shared);
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

/// Writes the runs the pump reports to the saved state, one at a time
/// and off the pump thread: the model the last run used, and the
/// auth failures the run outcomes clear or add. Ends with its queue.
fn start_run_recorder(runs: mpsc::Receiver<(String, RunOutcome)>) {
    let spawned = thread::Builder::new()
        .name("kage-acp-state".to_owned())
        .spawn(move || {
            for (model, outcome) in runs {
                let _ = crate::state::record_last_model(&model);
                let _ = crate::state::record_run_outcome(&model, &outcome);
            }
        });
    if let Err(error) = spawned {
        eprintln!("kage: cannot start the state thread: {error}");
    }
}

/// What the App sees once the agent is gone.
const DETACHED: &str = "the kage agent stopped; this TUI is detached";

/// The answer to a prompt or stop aimed at an agent whose session no
/// longer maps, such as one left open past its eviction.
const GONE_AGENT: &str = "that agent is gone";

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

/// The external ACP agent `model` selects, `acp:<name>`, or `None`
/// for an engine model. An `acp:` pick opens a persistent external
/// session through `_meta.kage.externalAgent`; the engine's `acp`
/// provider stays the one-shot path for `-p`.
fn external_agent(model: &str) -> Option<&str> {
    model.strip_prefix("acp:").filter(|name| !name.is_empty())
}

/// The `_meta.kage` the TUI asks of the session it opens: what this
/// process would have set up on the engine path, phrased for the
/// agent to apply instead. Only what the user actually set is sent.
/// `plugins` marks this process as the session's plugin host.
fn session_meta(
    options: &kage_plugin::SharedOptions,
    registry: &ProviderRegistry,
    model: &str,
    system: &str,
    yolo: bool,
    plugins: bool,
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
            plugins: plugins.then_some(PluginsSource::Client),
            background: None,
            yolo,
            system_prompt: (!system.is_empty()).then(|| system.to_owned()),
            opt,
            max_output_tokens: crate::runtime_env::max_output_tokens_for(registry, model),
            external_agent: None,
        }),
    }
}

/// The wire form of the plugins' MCP servers, and a warning for each
/// server using a setting the wire cannot carry, `disabled_tools` or
/// OAuth, which the agent then runs without. A disabled server and one
/// with neither a command nor a URL, which the config validator
/// rejects, are left out.
fn wire_mcp_servers(
    registered: Vec<(String, kage_core::config::McpServer)>,
) -> (Vec<McpServer>, Vec<String>) {
    let mut servers = Vec::new();
    let mut warnings = Vec::new();
    for (name, server) in registered {
        if server.disabled {
            continue;
        }
        let lost = match (server.disabled_tools.is_empty(), server.oauth.is_some()) {
            (true, false) => None,
            (false, false) => Some("disabled_tools"),
            (true, true) => Some("oauth"),
            (false, true) => Some("disabled_tools and oauth"),
        };
        let wire = match (server.command, server.url) {
            (Some(command), _) => McpServer::Stdio(McpServerStdio {
                name: name.clone(),
                command,
                args: server.args,
                env: server
                    .env
                    .into_iter()
                    .map(|(name, value)| EnvVariable { name, value })
                    .collect(),
            }),
            (None, Some(url)) => McpServer::Http(McpServerHttp {
                name: name.clone(),
                url,
                headers: server
                    .headers
                    .into_iter()
                    .map(|(name, value)| HttpHeader { name, value })
                    .collect(),
            }),
            (None, None) => continue,
        };
        if let Some(lost) = lost {
            warnings.push(format!(
                "plugin MCP server `{name}` sets {lost}, which the ACP link cannot carry; \
                 the agent runs it without"
            ));
        }
        servers.push(wire);
    }
    (servers, warnings)
}

/// The external agent a recorded session file names in its header,
/// or `None` for a native session or one whose header does not read.
fn recorded_external_agent(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    match serde_json::from_str::<kage_session::SessionEntry>(line.trim()).ok()? {
        kage_session::SessionEntry::Header(header) => match header.backend {
            Some(kage_session::SessionBackend::ExternalAcp(external)) => Some(external.agent_name),
            None => None,
        },
        _ => None,
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

/// The `_kage/plugin/*` surface of the session whose plugins this
/// process hosts: the local runtime answering for it. One surface per
/// connection, whatever session the agent names, like the engine
/// serves every session from one runtime.
#[derive(Clone)]
struct PluginSurface {
    runtime: Arc<PluginRuntime>,
    /// Where the runtime's tools execute.
    workdir: PathBuf,
    /// Raised when the connection ends, so no tool call runs on.
    cancel: CancelFlag,
    /// The flag tool calls run under: raised by `Esc` and cleared as
    /// each call starts, and it reads cancelled whenever `cancel` is.
    turn: CancelFlag,
}

/// One plugin reverse request waiting for the worker.
struct PluginJob {
    id: RequestId,
    method: String,
    params: Value,
}

/// What the worker answers a [`PluginJob`] with.
type PluginAnswer = (RequestId, Result<Value, RpcError>);

impl PluginSurface {
    fn new(runtime: Arc<PluginRuntime>, workdir: PathBuf) -> Self {
        let cancel = CancelFlag::new();
        let turn = cancel.child();
        Self {
            runtime,
            workdir,
            cancel,
            turn,
        }
    }

    /// Starts the thread that answers plugin requests in arrival
    /// order, which a Lua tool may hold for as long as it runs. It
    /// ends with the returned queue.
    fn start_worker(
        &self,
        answers: mpsc::Sender<PluginAnswer>,
    ) -> Result<mpsc::Sender<PluginJob>, String> {
        let surface = self.clone();
        let (jobs, queue) = mpsc::channel::<PluginJob>();
        thread::Builder::new()
            .name("kage-acp-plugins".to_owned())
            .spawn(move || {
                for job in queue {
                    surface.turn.reset();
                    let answer = surface.answer(&job.method, &job.params);
                    if answers.send((job.id, answer)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| format!("cannot start the plugin thread: {error}"))?;
        Ok(jobs)
    }

    /// Answer one reverse request. Unknown methods cannot reach here:
    /// the pump only routes the `_kage/plugin/` prefix.
    fn answer(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            crate::rpc::client_plugins::PLUGIN_TOOLS => self.tools(params),
            crate::rpc::client_plugins::PLUGIN_CALL => self.call(params),
            crate::rpc::client_plugins::PLUGIN_HOOK => self.hook(params),
            crate::rpc::client_plugins::PLUGIN_EVENT => self.event(params),
            crate::rpc::client_plugins::PLUGIN_DRAIN => self.drain(params),
            other => Err(RpcError::method_not_found(other)),
        }
    }

    /// Runs `f` with the request session's invocation in effect, the
    /// way the engine scopes its dispatches, so work the handlers
    /// queue is tagged to the session that asked for it.
    fn in_session<R>(session_id: &str, f: impl FnOnce() -> R) -> Result<R, RpcError> {
        let session = ulid::Ulid::from_string(session_id)
            .map(kage_core::SessionId)
            .map_err(|e| RpcError::invalid_params(format!("plugin session: {e}")))?;
        Ok(kage_plugin::Invocation::session(session).scope(f))
    }

    /// Drains `take` inside the request session's scope when it names
    /// one, so only that session's queued work leaves. A session-less
    /// ask drains across every session, as the wire describes.
    fn drain_scoped<R>(session_id: Option<&str>, take: impl FnOnce() -> R) -> Result<R, RpcError> {
        match session_id {
            Some(id) => Self::in_session(id, take),
            None => Ok(take()),
        }
    }

    /// The registration snapshot the host takes once at session open:
    /// the tools, overrides and renames the runtime holds, plus the
    /// per-event handler counts that let it skip round trips.
    fn tools(&self, params: &Value) -> Result<Value, RpcError> {
        serde_json::from_value::<PluginToolsRequest>(params.clone())
            .map_err(|e| RpcError::invalid_params(format!("plugin tools: {e}")))?;
        let runtime = &*self.runtime;
        let spec = |tool: &Arc<dyn kage_tools::Tool>| PluginToolSpec {
            name: tool.name().to_owned(),
            description: tool.description().to_owned(),
            schema: tool.schema(),
            risk: serde_json::to_value(tool.risk())
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned)),
            parallel: tool
                .execution_mode()
                .map(|mode| mode == kage_tools::ExecMode::Parallel),
        };
        let result = PluginToolsResult {
            tools: runtime.registered_tools().iter().map(spec).collect(),
            overrides: runtime
                .registered_tool_overrides()
                .iter()
                .map(spec)
                .collect(),
            renames: runtime
                .registered_renames()
                .into_iter()
                .map(|(from, to)| PluginToolRename { from, to })
                .collect(),
            handlers: KNOWN_EVENTS
                .iter()
                .filter_map(|(name, _, _)| {
                    let count = u32::try_from(runtime.handler_count(name)).ok()?;
                    (count > 0).then(|| (name.to_string(), count))
                })
                .collect(),
        };
        serde_json::to_value(result).map_err(|e| plugin_internal(format!("plugin tools: {e}")))
    }

    /// Run one tool call on the runtime. A tool that reports failure
    /// answers `is_error`; one that throws or was cancelled fails the
    /// request, which the host turns into a failed tool call. An
    /// override wins the name, the way the engine's registry replaces
    /// tools by name.
    fn call(&self, params: &Value) -> Result<Value, RpcError> {
        let request = serde_json::from_value::<PluginCallRequest>(params.clone())
            .map_err(|e| RpcError::invalid_params(format!("plugin call: {e}")))?;
        let runtime = &*self.runtime;
        let tool = runtime
            .registered_tool_overrides()
            .into_iter()
            .chain(runtime.registered_tools())
            .find(|tool| tool.name() == request.tool);
        let Some(tool) = tool else {
            return Err(plugin_internal(format!(
                "no plugin tool `{}`",
                request.tool
            )));
        };
        let cx = kage_tools::ToolContext::new(&self.workdir, &self.turn);
        let output = Self::in_session(&request.session_id, || tool.execute(request.input, &cx))?
            .map_err(|e| plugin_internal(format!("plugin tool `{}`: {e}", request.tool)))?;
        let result = PluginCallResult {
            is_error: output.is_error,
            text: output.text,
            structured: output.structured,
        };
        serde_json::to_value(result).map_err(|e| plugin_internal(format!("plugin call: {e}")))
    }

    /// Run one synchronous hook. A transform's reply replaces the
    /// payload, a predicate's is the decision.
    fn hook(&self, params: &Value) -> Result<Value, RpcError> {
        let request = serde_json::from_value::<PluginHookRequest>(params.clone())
            .map_err(|e| RpcError::invalid_params(format!("plugin hook: {e}")))?;
        let runtime = &*self.runtime;
        match request.kind {
            PluginHookKind::Transform => Self::in_session(&request.session_id, || {
                runtime.dispatch_transform(&request.event, request.payload)
            })?
            .map_err(|e| plugin_internal(format!("{} transform: {e}", request.event))),
            PluginHookKind::Predicate => Self::in_session(&request.session_id, || {
                runtime.dispatch_predicate(&request.event, &request.payload)
            })?
            .map(Value::Bool)
            .map_err(|e| plugin_internal(format!("{} predicate: {e}", request.event))),
        }
    }

    /// Fire one event. The runtime queues notify events and logs
    /// handler trouble itself, so a notify failure still answers.
    fn event(&self, params: &Value) -> Result<Value, RpcError> {
        let request = serde_json::from_value::<PluginEventRequest>(params.clone())
            .map_err(|e| RpcError::invalid_params(format!("plugin event: {e}")))?;
        let runtime = &*self.runtime;
        let outcome = Self::in_session(&request.session_id, || {
            if request.notify {
                runtime.notify_event(&request.event, &request.payload)
            } else {
                runtime.dispatch_event(&request.event, &request.payload)
            }
        })?;
        outcome.map_err(|e| plugin_internal(format!("{} event: {e}", request.event)))?;
        Ok(Value::Null)
    }

    /// Hand over one queued queue of plugin work. Only the requested
    /// queue's field carries anything.
    fn drain(&self, params: &Value) -> Result<Value, RpcError> {
        let request = serde_json::from_value::<PluginDrainRequest>(params.clone())
            .map_err(|e| RpcError::invalid_params(format!("plugin drain: {e}")))?;
        let runtime = &*self.runtime;
        let result = match request.queue {
            PluginDrainQueue::McpRestarts => {
                Self::drain_scoped(request.session_id.as_deref(), || PluginDrainResult {
                    restarts: runtime.take_mcp_restarts(),
                    ..PluginDrainResult::default()
                })?
            }
            PluginDrainQueue::Messages => {
                Self::drain_scoped(request.session_id.as_deref(), || PluginDrainResult {
                    messages: runtime
                        .take_pending_messages()
                        .into_iter()
                        .map(|message| PluginPendingMessage {
                            text: message.text,
                            trigger_turn: message.trigger_turn,
                            deliver_as: match message.deliver_as {
                                PendingRole::User => PluginPendingRole::User,
                                PendingRole::Assistant => PluginPendingRole::Assistant,
                                PendingRole::System => PluginPendingRole::System,
                            },
                        })
                        .collect(),
                    ..PluginDrainResult::default()
                })?
            }
            PluginDrainQueue::SessionOps => PluginDrainResult {
                ops: runtime
                    .take_pending_session_ops()
                    .into_iter()
                    .map(plugin_session_op)
                    .collect(),
                ..PluginDrainResult::default()
            },
            PluginDrainQueue::SessionOpsFor => {
                let session = request
                    .session_id
                    .as_deref()
                    .map(ulid::Ulid::from_string)
                    .transpose()
                    .map_err(|e| RpcError::invalid_params(format!("plugin drain: {e}")))?;
                let Some(session) = session else {
                    return Err(RpcError::invalid_params(
                        "plugin drain: SessionOpsFor needs a session",
                    ));
                };
                PluginDrainResult {
                    ops: runtime
                        .take_pending_session_ops_for(kage_core::SessionId(session))
                        .into_iter()
                        .map(plugin_session_op)
                        .collect(),
                    ..PluginDrainResult::default()
                }
            }
        };
        serde_json::to_value(result).map_err(|e| plugin_internal(format!("plugin drain: {e}")))
    }
}

/// The internal-error answer for a well-formed plugin request the
/// runtime could not serve.
fn plugin_internal(message: impl std::fmt::Display) -> RpcError {
    RpcError::new(-32603, message.to_string())
}

/// The plugin form of one queued session write.
fn plugin_session_op(op: PendingSessionOp) -> PluginSessionOp {
    match op {
        PendingSessionOp::AppendCustom { kind, data } => {
            PluginSessionOp::AppendCustom { kind, data }
        }
        PendingSessionOp::SetLabel { anchor, text } => PluginSessionOp::SetLabel { anchor, text },
    }
}

/// The `_kage/environment` notification carrying this process's
/// environment, or `None` when it cannot be sent and the host keeps
/// its own, like the `kage rpc` relay does.
fn environment_frame() -> Option<Frame> {
    let notification = EnvironmentNotification {
        vars: crate::rpc::environment_vars(),
    };
    let Ok(params) = serde_json::to_value(&notification) else {
        eprintln!("kage: the environment cannot be serialized; tools use the shared host's");
        return None;
    };
    if params.to_string().len() > crate::rpc::MAX_ENVIRONMENT_BYTES {
        eprintln!("kage: the environment is too large to send; tools use the shared host's");
        return None;
    }
    Some(Frame::Notification {
        method: "_kage/environment".to_owned(),
        params,
    })
}

/// A recorded session being opened over the wire.
struct PendingOpen {
    id: String,
    request: u64,
    /// A resume the App started with: when it fails there is no
    /// session to fall back to, so the pump asks for a new one.
    bootstrap: bool,
}

/// The pending request a frame answers.
#[derive(Clone, Copy)]
enum Answer {
    NewSession,
    Open,
}

/// How much of a child tool call the App has already heard of.
#[derive(Clone, Copy, PartialEq)]
enum ToolSent {
    /// Its `ToolCallStart` went out.
    Started,
    /// Its execution began, as `ToolExecutionStart`.
    Executing,
    /// Its result went out as `ToolCallEnd`.
    Ended,
}

/// The latest tool state delivered to a child view.
#[derive(Clone)]
struct ChildTool {
    phase: ToolSent,
    name: String,
    input: Value,
    progress: String,
}

/// Which kind of child message is streaming into the App.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TailKind {
    /// Reply text, carried by `TextDelta`.
    Assistant,
    /// Reasoning text, carried by `ThinkingDelta`.
    Thinking,
}

/// What the pump has already told the App about one subagent session.
struct ChildTrack {
    /// The App session the child's envelopes ride.
    app: SessionId,
    /// The wire id of the session that announced this child.
    parent: String,
    /// Transcript items fully handed over. The streaming tail stays
    /// uncommitted until it stops being the last item.
    committed: usize,
    /// Tool phases, arguments and progress already delivered to the App.
    tools: BTreeMap<String, ChildTool>,
    /// The streaming tail: its kind, transcript index and the text
    /// length already sent. The item stays uncommitted while it is
    /// still the last one.
    tail: Option<(TailKind, usize, usize)>,
    /// Whether the child's run is in flight, for the run edges.
    running: bool,
    /// Whether the last announced run has already settled.
    ended: bool,
}

/// The thread that turns the store's view of the wire into the App's
/// engine vocabulary: changes into
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
    /// The mirror the run loop and the App read, pointed at the wire
    /// session's file as sessions are adopted.
    mirror: Arc<Mutex<Mirror>>,
    /// The session id every envelope names: the App sees one session
    /// per connection, whatever the agent calls its sessions.
    app_session: SessionId,
    /// The wire id of the session the App is on.
    wire: String,
    /// The recorded session being opened. Its answer adopts it; a
    /// failed answer leaves the current session as it was.
    pending_open: Option<PendingOpen>,
    /// The request id of the `session/new` in flight, whose answer's
    /// session adopts.
    pending_new: Option<u64>,
    cwd: String,
    /// The model the session runs, as the config option carries it.
    model: String,
    warned_model: Option<String>,
    /// The option store the session settings are read from at each
    /// session's opening, so a `:set` since startup reaches the next
    /// session too.
    options: kage_plugin::SharedOptions,
    /// The provider registry, for the model's output token cap.
    registry: Arc<ProviderRegistry>,
    /// The plugins this process hosts for the wire session, answering
    /// the agent's `_kage/plugin/*` reverse requests. `None` leaves
    /// plugin loading to the agent.
    plugins: Option<PluginSurface>,
    /// Where plugin reverse requests queue for the worker.
    plugin_jobs: Option<mpsc::Sender<PluginJob>>,
    /// The worker's answers, to send on the wire.
    plugin_answers: mpsc::Receiver<PluginAnswer>,
    plugin_answers_tx: mpsc::Sender<PluginAnswer>,
    /// Where finished runs go to be written to the saved state, off
    /// this thread.
    recorder: mpsc::Sender<(String, RunOutcome)>,
    /// Sent prompts and their owning sessions, so failures can settle
    /// the main run or return a rejected child submission to its draft.
    prompts: HashMap<RequestId, (String, Vec<ContentBlock>)>,
    /// The error that ended the run being settled.
    run_error: Option<String>,
    /// The run-wide session settings that do not come from options.
    yolo: bool,
    /// Whether the agent serves sessions for this connection without
    /// this process's environment, so the pump announces it in
    /// `_kage/environment` and their tools run with the terminal's
    /// `PATH` and variables. The shared host does; a child inherits
    /// this process's environment and the private host is this
    /// process.
    environment: bool,
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
    /// App-facing ask id to wire ask id and the wire session that
    /// asked, for the answers.
    ask_ids: BTreeMap<u64, (String, RequestId)>,
    next_ask: u64,
    /// Subagent sessions the agent announced, by wire id, with what
    /// the App has already been told about each.
    children: HashMap<String, ChildTrack>,
    /// Wire ids of the sessions an external ACP agent owns, with the
    /// agent's configured name. The client marks sessions it creates
    /// itself; this map also covers the ones a recorded file names,
    /// which the wire list never describes to this connection.
    external: HashMap<String, String>,
    /// App child session id to the wire id the child's transcript is
    /// recorded under, shared with the App's agent loader: the App
    /// only knows its own minted ids, the files use the wire's.
    child_files: Arc<Mutex<HashMap<SessionId, String>>>,
    /// Where an export writes once the agent answers.
    pending_export: Option<PathBuf>,
    /// Update kinds this build cannot show, warned of once each.
    unknown: HashSet<String>,
    /// Dropped MCP server settings, warned of once each.
    mcp_warnings: HashSet<String>,
    closed: bool,
}

impl Pump {
    /// Serves the connection until the App quits.
    fn serve(mut self) {
        while self.pass() {}
    }

    /// One turn of the loop: waits up to a tick for the agent or, once
    /// it is gone, for the App, then handles what the App and the
    /// plugin worker have queued. `false` once the App is gone, which
    /// ends the child with the transport.
    fn pass(&mut self) -> bool {
        if self.closed {
            match self.requests.recv_timeout(TICK) {
                Ok(request) => self.handle(request),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        } else {
            match self.frames.recv_timeout(TICK) {
                Ok(Event::Frame(frame)) => self.absorb(frame),
                Ok(Event::Closed)
                // The reader sends Closed before it drops, so this is
                // its late echo.
                | Err(mpsc::RecvTimeoutError::Disconnected) => self.close(),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        loop {
            match self.requests.try_recv() {
                Ok(request) => self.handle(request),
                Err(mpsc::TryRecvError::Disconnected) => return false,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        while let Ok(answer) = self.plugin_answers.try_recv() {
            self.send_plugin_answer(answer);
        }
        self.poll_dialogs();
        self.flush();
        true
    }

    fn send_plugin_answer(&mut self, (id, answer): PluginAnswer) {
        match answer {
            Ok(result) => self.store.client().reply(id, result),
            Err(error) => self.store.client().fail(id, error),
        }
    }

    /// Starts answering the plugin reverse requests from `surface`.
    fn host_plugins(&mut self, surface: PluginSurface) -> Result<(), String> {
        self.plugin_jobs = Some(surface.start_worker(self.plugin_answers_tx.clone())?);
        self.plugins = Some(surface);
        Ok(())
    }

    /// Feeds one frame in. The plugin reverse requests never touch the
    /// store: the worker answers them, because the client would
    /// otherwise fail them as methods it does not know. Hosting
    /// nothing leaves them to the store, which fails them the same way.
    fn absorb(&mut self, frame: Frame) {
        if let Frame::Request { id, method, params } = &frame
            && method.starts_with("_kage/plugin/")
            && let Some(jobs) = &self.plugin_jobs
        {
            let job = PluginJob {
                id: id.clone(),
                method: method.clone(),
                params: params.clone(),
            };
            if jobs.send(job).is_err() {
                self.store
                    .client()
                    .fail(id.clone(), plugin_internal("the plugin thread stopped"));
            }
            return;
        }
        self.withdrawn_child_prompt(&frame);
        let answer = self.answer_of(&frame);
        let failed_prompt = match &frame {
            Frame::Failure { id, error } => self
                .prompts
                .remove(id)
                .map(|(wire, blocks)| (wire, blocks, error.message.clone())),
            Frame::Success { id, .. } => {
                self.prompts.remove(id);
                None
            }
            _ => None,
        };
        let replaced = match &frame {
            Frame::Notification { method, params }
                if method == "session/update"
                    && params["update"]["sessionUpdate"] == "_kage/session_replaced" =>
            {
                params["sessionId"].as_str().map(str::to_owned)
            }
            _ => None,
        };
        let changes = self.store.absorb(frame);
        if let Some(wire) = replaced {
            self.reset_child_transcript(&wire);
        }
        match answer {
            Some(answer) => self.settle(answer, changes),
            None => {
                for change in changes {
                    self.react(change);
                }
            }
        }
        // A failed prompt moves no session in the client, so the run
        // it ended is settled here.
        if let Some((wire, blocks, message)) = failed_prompt {
            if wire == self.wire {
                self.run_error = Some(message);
                self.session_moved(&wire);
                self.run_error = None;
            } else {
                self.return_prompt(self.app_of(&wire), content_of(&blocks), false, message);
            }
        }
    }

    /// Returns live steering that the child finished without consuming.
    fn withdrawn_child_prompt(&mut self, frame: &Frame) {
        let Frame::Notification { method, params } = frame else {
            return;
        };
        if method != "session/update"
            || params["update"]["sessionUpdate"] != "_kage/prompt_withdrawn"
        {
            return;
        }
        let Ok(SessionNotification {
            session_id,
            update: SessionUpdate::PromptWithdrawn(prompt),
        }) = serde_json::from_value::<SessionNotification>(params.clone())
        else {
            return;
        };
        let Some(track) = self.children.get(&session_id) else {
            return;
        };
        let app = track.app;
        let content = content_of(&prompt.content);
        self.prompts
            .retain(|_, (wire, blocks)| wire != &session_id || content_of(blocks) != content);
        self.publish_on(
            app,
            HostEvent::PromptWithdrawn {
                delivery: match prompt.delivery {
                    PromptDelivery::Steer => Delivery::Steer,
                    PromptDelivery::Queue => Delivery::Queue,
                },
                content: Some(content),
            },
        );
    }

    /// Which pending session request `frame` answers, if any.
    fn answer_of(&self, frame: &Frame) -> Option<Answer> {
        let (Frame::Success { id, .. } | Frame::Failure { id, .. }) = frame else {
            return None;
        };
        let RequestId::Number(request) = id else {
            return None;
        };
        if self.pending_new == Some(*request) {
            return Some(Answer::NewSession);
        }
        self.pending_open
            .as_ref()
            .is_some_and(|open| open.request == *request)
            .then_some(Answer::Open)
    }

    /// Reacts to the changes of the answer to a pending session
    /// request. A session that answered adopts the wire; a failed
    /// request clears its pending state and leaves the session the
    /// App is on as it was, with the failure shown. The client's
    /// restore of a failed load reports a session change that must
    /// not adopt.
    fn settle(&mut self, answer: Answer, changes: Vec<Change>) {
        let failed = changes
            .iter()
            .any(|change| matches!(change, Change::Failed { .. }));
        let adopted = match answer {
            Answer::NewSession => {
                self.pending_new = None;
                changes.iter().find_map(|change| match change {
                    Change::Session { id } if !failed => Some((id.clone(), true)),
                    _ => None,
                })
            }
            Answer::Open => {
                let open = self.pending_open.take().expect("an open answered");
                if failed && open.bootstrap {
                    // The startup resume never opened: the wire names
                    // a session the agent does not know. Fall back to
                    // a new one off an empty wire, so its adoption
                    // does not close the phantom.
                    self.wire.clear();
                    self.request_new_session();
                    None
                } else {
                    (!failed).then_some((open.id, false))
                }
            }
        };
        for change in changes {
            if !matches!(change, Change::Session { .. }) {
                self.react(change);
            }
        }
        if let Some((id, new)) = adopted {
            self.adopt(&id, new);
        }
    }

    /// Opens the connection before the App starts: handshake, session
    /// and plugin UI, with the frames these produce sent at once.
    /// Resuming replays on a fresh buffer; otherwise the new session's
    /// answer lands while the App runs and adopts the wire then.
    fn bootstrap(&mut self, resume: Option<&Path>) {
        if self.environment
            && let Some(frame) = environment_frame()
        {
            self.transport.send(frame);
        }
        let capabilities = ClientCapabilities {
            meta: Some(ClientMeta {
                kage: Some(KageClientCapabilities {
                    // Built-in tools run without asking, like the
                    // engine path of a default configuration.
                    unconfigured_tools: Some(UnconfiguredTools::Allow),
                    plugin_host: self.plugins.is_some(),
                }),
            }),
            // Subagents announce on their parent and stream on their
            // own sessions; an agent without them flattens the tree
            // into the parent transcript, as before.
            subagents: Some(serde_json::json!({})),
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
            self.note_session_file();
            let meta = self.load_meta();
            let servers = self.mcp_servers();
            let (request, _) =
                self.store
                    .client()
                    .load_session(&id, &self.cwd, &servers, Some(meta));
            self.pending_open = Some(PendingOpen {
                id,
                request,
                bootstrap: true,
            });
        } else {
            self.request_new_session();
        }
        self.store.client().ui_attach(&self.cwd, None);
        self.flush();
    }

    /// Asks the agent for a new session, whose answer adopts the wire.
    /// An `acp:<name>` model opens a persistent external session on the
    /// configured agent instead of an engine session.
    fn request_new_session(&mut self) {
        if let Some(agent) = external_agent(&self.model).map(str::to_owned) {
            self.request_external_session(&agent);
            return;
        }
        let meta = self.session_meta();
        let servers = self.mcp_servers();
        let request = self
            .store
            .client()
            .new_session(&self.cwd, &servers, Some(meta));
        self.pending_new = Some(request);
    }

    /// Opens a persistent session on the configured external ACP agent
    /// `name`: the host spawns it once and the conversation survives
    /// turns, unlike the one-shot `acp` provider path. No MCP servers
    /// ride along: external sessions refuse them.
    fn request_external_session(&mut self, name: &str) {
        let request = self
            .store
            .client()
            .new_session_with_external_agent(&self.cwd, &[], name);
        self.pending_new = Some(request);
    }

    /// The external ACP agent owning wire session `id`, when known:
    /// the client marks the sessions it created and the directory
    /// lists, and [`Self::external`] covers the ones a recorded file
    /// named before the wire ever described them.
    fn external_agent_of(&self, id: &str) -> Option<String> {
        if let Some(agent) = self.external.get(id) {
            return Some(agent.clone());
        }
        self.store
            .state()
            .session(id)
            .and_then(|session| session.external_agent.clone())
    }

    /// The agent owning the session on screen, when it is external.
    fn external(&self) -> Option<String> {
        self.external_agent_of(&self.wire)
    }

    /// Reports and returns `true` when the session on screen belongs
    /// to an external ACP agent, whose own agent manages models, modes,
    /// history and files: kage cannot change them here.
    fn refuse_external(&mut self) -> bool {
        let Some(agent) = self.external() else {
            return false;
        };
        self.notice(
            NoticeLevel::Warning,
            format!(
                "{agent} is an external agent session. Its agent manages models, modes, history and files, so kage cannot change them here."
            ),
        );
        true
    }

    /// Whether `request` asks an external session for something its
    /// agent owns: engine models and options, thinking, modes, goals,
    /// compaction, swarm and export. An `acp:` model pick is not one:
    /// it opens a session of its own. Reports the refusal and returns
    /// `true` so [`Self::handle`] skips the request.
    fn refuse_external_request(&mut self, request: &RunRequest) -> bool {
        let gated = match request {
            RunRequest::SwitchModel(model) => external_agent(model).is_none(),
            RunRequest::CycleThinkingLevel
            | RunRequest::SetThinkingLevel(_)
            | RunRequest::SetPermissionMode(_)
            | RunRequest::PlanMode { .. }
            | RunRequest::SwarmMode { .. }
            | RunRequest::SetGoal { .. }
            | RunRequest::CompactNow
            | RunRequest::ExportSession(_) => true,
            _ => false,
        };
        gated && self.refuse_external()
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
            self.plugins.is_some(),
        )
    }

    /// The `_meta.kage` a load or resume carries. A load opens on the
    /// session's recorded model, so the output cap is left out: one
    /// sized for the current model would pin the wrong limit.
    fn load_meta(&mut self) -> NewSessionMeta {
        let mut meta = self.session_meta();
        if let Some(kage) = meta.kage.as_mut() {
            kage.max_output_tokens = None;
        }
        meta
    }

    /// The MCP servers the session brings: the hosted plugins' own
    /// registrations, disabled ones left out, like the engine path.
    /// The agent spawns them, so none of this process's MCP runs
    /// beside the pump. A setting the wire cannot carry is warned of.
    fn mcp_servers(&mut self) -> Vec<McpServer> {
        let Some(surface) = self.plugins.as_ref() else {
            return Vec::new();
        };
        let (servers, warnings) = wire_mcp_servers(surface.runtime.registered_mcp_servers());
        for warning in warnings {
            // Every session open recomputes these; one toast each is
            // enough.
            if self.mcp_warnings.insert(warning.clone()) {
                self.notice(NoticeLevel::Warning, warning);
            }
        }
        servers
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
        let kage_client::Frame::Request { id, method, params } = frame else {
            return;
        };
        if method != "session/prompt" {
            return;
        }
        let Ok(prompt) = serde_json::from_value::<OutgoingPrompt>(params.clone()) else {
            return;
        };
        if prompt.session_id != self.wire && !self.children.contains_key(&prompt.session_id) {
            return;
        }
        self.prompts.insert(
            id.clone(),
            (prompt.session_id.clone(), prompt.prompt.clone()),
        );
        if prompt.session_id != self.wire {
            self.child_transcript(&prompt.session_id);
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
            Change::Session { id } => {
                if id == self.wire {
                    self.session_moved(&id);
                } else {
                    self.child_moved(&id);
                }
            }
            Change::Transcript { id } => {
                if id == self.wire {
                    let state = self.state_snapshot();
                    self.update_ui_slot(|ui| ui.state = state.clone());
                    self.publish(HostEvent::StateChanged { state });
                    let usage = self.usage_snapshot();
                    self.update_ui_slot(|ui| ui.usage = Some(usage));
                    self.publish(HostEvent::UsageUpdated { usage });
                    self.hand_buffer();
                } else {
                    self.child_transcript(&id);
                }
            }
            Change::Permission { id } => {
                if id == self.wire {
                    self.asks_moved(&id);
                } else {
                    self.child_asks(&id);
                }
            }
            Change::AnsweredElsewhere { id, request_id } => {
                if id == self.wire || self.children.contains_key(&id) {
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
            Change::Agents { id } => self.agents_moved(&id),
            Change::Config { .. }
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

    /// A session moved. Only the active one is followed: it publishes
    /// its state, with the run edges its running flag crosses.
    fn session_moved(&mut self, id: &str) {
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
            let outcome = self.run_outcome();
            let _ = self.recorder.send((self.model.clone(), outcome.clone()));
            self.publish(HostEvent::RunEnded { outcome });
        }
        if self.external_agent_of(&self.wire).is_none()
            && let Some(model) = self
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
        let state = self.state_snapshot();
        let title = self
            .store
            .state()
            .session(&self.wire)
            .and_then(|session| session.title.clone());
        self.update_ui_slot(|ui| {
            ui.state = state.clone();
            ui.session_title = title;
        });
        self.publish(HostEvent::StateChanged { state });
        self.publish(HostEvent::UsageUpdated {
            usage: self.usage_snapshot(),
        });
        self.hand_buffer();
    }

    /// How the run that just ended finished: the error its prompt
    /// answered with, else the stop reason the agent reported.
    fn run_outcome(&mut self) -> RunOutcome {
        if let Some(message) = self.run_error.take() {
            return RunOutcome::Failed {
                error: kage_core::LoopError::Provider { message },
            };
        }
        let stop = self
            .store
            .state()
            .session(&self.wire)
            .and_then(|session| session.last_stop);
        match stop {
            Some(kage_client::wire::StopReason::Cancelled) => RunOutcome::Cancelled,
            _ => RunOutcome::Completed,
        }
    }

    /// The App session a wire session's envelopes ride: the main
    /// session's for the wire itself, the child's mapping for one of
    /// its subagents.
    fn app_of(&self, wire: &str) -> SessionId {
        if wire == self.wire {
            return self.app_session;
        }
        self.children
            .get(wire)
            .map_or(self.app_session, |child| child.app)
    }

    /// The wire id an App session maps to: the wire session itself, or
    /// a subagent's own session. `None` for an id nothing owns.
    fn wire_of(&self, app: SessionId) -> Option<String> {
        if app == self.app_session {
            return Some(self.wire.clone());
        }
        self.children
            .iter()
            .find(|(_, child)| child.app == app)
            .map(|(wire, _)| wire.clone())
    }

    /// A subagent's session moved: refreshes its App view without
    /// treating provider turns as child-run boundaries.
    fn child_moved(&mut self, wire: &str) {
        if self.children.contains_key(wire) {
            self.child_sync(wire);
        }
    }

    /// Refreshes a child's model, usage and transcript. Its parent
    /// announces the run lifetime independently of provider turns.
    fn child_sync(&mut self, wire: &str) {
        let Some(track) = self.children.get(wire) else {
            return;
        };
        let (app, working) = (track.app, track.running);
        let snapshot = self.store.state().session(wire).map(|session| {
            (
                session
                    .config_options
                    .iter()
                    .find(|option| option.id == "model")
                    .map(|option| option.current_value.clone()),
                session.usage.clone(),
            )
        });
        let Some((model, usage)) = snapshot else {
            return;
        };
        if let Some(model) = model {
            self.publish_on(
                app,
                HostEvent::StateChanged {
                    state: SessionState {
                        model,
                        working,
                        ..SessionState::default()
                    },
                },
            );
        }
        self.publish_on(
            app,
            HostEvent::UsageUpdated {
                usage: Self::usage_of(&usage),
            },
        );
        self.child_transcript(wire);
    }

    /// Resets a replaced child's tracking and displayed conversation
    /// before the replacement replay arrives.
    fn reset_child_transcript(&mut self, wire: &str) {
        let Some(track) = self.children.get_mut(wire) else {
            return;
        };
        track.committed = 0;
        track.tools.clear();
        track.tail = None;
        let app = track.app;
        self.publish_on(
            app,
            HostEvent::SessionChanged {
                path: PathBuf::from(format!("{wire}.jsonl")),
                title: None,
                timeline: Vec::new(),
                compacted_agents: Vec::new(),
            },
        );
    }

    /// Settles a child exactly once, finalizing its stream before the
    /// App may evict the finished view.
    fn end_child(&mut self, wire: &str, outcome: RunOutcome) {
        let Some(track) = self.children.get_mut(wire) else {
            return;
        };
        if track.ended {
            return;
        }
        track.running = false;
        track.ended = true;
        let app = track.app;
        self.child_close_tail(wire);
        self.publish_on(app, HostEvent::RunEnded { outcome });
    }

    /// Finalizes a child's streaming bubble and counts its item as
    /// handed over, so the finished text is never sent twice.
    fn child_close_tail(&mut self, wire: &str) {
        let Some(track) = self.children.get_mut(wire) else {
            return;
        };
        if let Some((_, index, _)) = track.tail.take() {
            track.committed = track.committed.max(index + 1);
            let app = track.app;
            self.message_end(app);
        }
    }

    /// Reconciles the children announced by a mapped parent. Incoming
    /// session trees wait in the store until that session is adopted.
    fn agents_moved(&mut self, parent: &str) {
        if parent != self.wire && !self.children.contains_key(parent) {
            return;
        }
        let agents = self
            .store
            .state()
            .session(parent)
            .map(|session| session.agents.clone())
            .unwrap_or_default();
        let parent_app = self.app_of(parent);
        for (wire, sub) in &agents {
            let fresh = !self.children.contains_key(wire);
            if fresh {
                let app = SessionId::new();
                self.children.insert(
                    wire.clone(),
                    ChildTrack {
                        app,
                        parent: parent.to_owned(),
                        committed: 0,
                        tools: BTreeMap::new(),
                        tail: None,
                        running: false,
                        ended: false,
                    },
                );
                lock(&self.child_files).insert(app, wire.clone());
                self.publish_on(
                    app,
                    HostEvent::AgentSpawned {
                        parent: parent_app,
                        tool_call_id: ToolCallId::new(sub.tool_call_id.as_deref().unwrap_or(wire)),
                        agent: sub.name.clone().unwrap_or_else(|| "agent".to_owned()),
                        description: sub.task.clone().unwrap_or_default(),
                        swarm: sub.swarm.as_ref().map(|swarm| SwarmMember {
                            batch: Some(ToolCallId::new(&swarm.id)),
                            item: swarm.item.clone(),
                            index: swarm.index,
                            total: swarm.total,
                        }),
                        background: sub.background,
                    },
                );
            }
            let terminal = matches!(
                sub.state,
                Some(
                    SubagentState::Completed
                        | SubagentState::Failed
                        | SubagentState::Cancelled
                        | SubagentState::Unknown
                )
            );
            let track = self.children.get_mut(wire).expect("mapped above");
            let app = track.app;
            let starting = !track.running && (!terminal || !track.ended);
            let restarting = starting && track.ended;
            if starting {
                track.running = true;
                track.ended = false;
                self.publish_on(app, HostEvent::RunStarted);
                if restarting {
                    self.reset_child_transcript(wire);
                }
            }
            self.child_sync(wire);
            if terminal {
                let outcome = match sub.state {
                    Some(SubagentState::Failed | SubagentState::Unknown) => RunOutcome::Failed {
                        error: kage_core::LoopError::Provider {
                            message: sub
                                .reason
                                .clone()
                                .unwrap_or_else(|| "subagent failed".to_owned()),
                        },
                    },
                    Some(SubagentState::Cancelled) => RunOutcome::Cancelled,
                    _ => RunOutcome::Completed,
                };
                self.end_child(wire, outcome);
            }
            if fresh {
                self.child_asks(wire);
                self.agents_moved(wire);
            }
        }
        self.children
            .retain(|wire, track| track.parent != parent || agents.contains_key(wire));
        let live: HashSet<SessionId> = self.children.values().map(|track| track.app).collect();
        lock(&self.child_files).retain(|app, _| live.contains(app));
    }

    /// Publishes a child session's transcript moves on its App
    /// session: finished items whole, the streaming tail as deltas,
    /// tool calls by phase.
    fn child_transcript(&mut self, wire: &str) {
        let Some(items) = self
            .store
            .state()
            .session(wire)
            .map(|session| session.items.clone())
        else {
            return;
        };
        let Some(track) = self.children.get_mut(wire) else {
            return;
        };
        let app = track.app;
        let mut tools = std::mem::take(&mut track.tools);
        let mut committed = track.committed;
        let mut tail = track.tail;
        self.child_tool_phases(app, &items, &mut tools);
        // The streaming tail: the item still last grows by suffix;
        // once anything else ends the transcript, what streamed counts
        // as handed over and the tail moves on.
        let streaming = match items.last() {
            Some(kage_client::TranscriptItem::Assistant { text }) if items.len() > committed => {
                Some((TailKind::Assistant, text.as_str()))
            }
            Some(kage_client::TranscriptItem::Thinking { text, .. }) if items.len() > committed => {
                Some((TailKind::Thinking, text.as_str()))
            }
            _ => None,
        }
        .map(|(kind, text)| (kind, text, items.len() - 1));
        let live = streaming.is_some()
            && streaming.as_ref().map(|&(_, _, index)| Some(index))
                == tail.as_ref().map(|(_, index, _)| Some(*index));
        if tail.is_some() && !live {
            if let Some((_, index, _)) = tail.take() {
                committed = committed.max(index + 1);
            }
            self.message_end(app);
        }
        if let Some((kind, text, index)) = streaming {
            let mut delta = None;
            if live {
                let (_, _, sent_len) = tail.expect("live implies an open tail");
                if text.len() > sent_len {
                    delta = Some(text[sent_len..].to_owned());
                }
                tail = Some((kind, index, text.len()));
            } else {
                for item in &items[committed..index] {
                    self.commit_child_item(app, item);
                }
                if !text.is_empty() {
                    delta = Some(text.to_owned());
                }
                tail = Some((kind, index, text.len()));
            }
            if let Some(delta) = delta {
                match kind {
                    TailKind::Assistant => self.publish_on(
                        app,
                        LoopEvent::TextDelta {
                            id: MessageId::new(),
                            delta,
                        },
                    ),
                    TailKind::Thinking => self.publish_on(
                        app,
                        LoopEvent::ThinkingDelta {
                            id: MessageId::new(),
                            delta,
                        },
                    ),
                }
            }
            committed = index;
        } else {
            for item in &items[committed..] {
                self.commit_child_item(app, item);
            }
            committed = items.len();
        }
        let track = self.children.get_mut(wire).expect("checked above");
        track.tools = tools;
        track.committed = committed;
        track.tail = tail;
    }

    /// Walks a child transcript's tool calls for the phases the App
    /// has not heard of yet, since updates merge into the call they
    /// name wherever it sits.
    fn child_tool_phases(
        &mut self,
        app: SessionId,
        items: &[kage_client::TranscriptItem],
        tools: &mut BTreeMap<String, ChildTool>,
    ) {
        for item in items {
            let kage_client::TranscriptItem::ToolCall(call) = item else {
                continue;
            };
            let id = ToolCallId::new(&call.tool_call_id);
            let name = call.name.as_ref().unwrap_or(&call.title);
            let input = call.input.clone().unwrap_or_else(|| serde_json::json!({}));
            let sent = tools.entry(call.tool_call_id.clone()).or_insert_with(|| {
                self.publish_on(
                    app,
                    LoopEvent::ToolCallStart {
                        id: id.clone(),
                        name: name.clone(),
                        input_partial: input.clone(),
                    },
                );
                ChildTool {
                    phase: ToolSent::Started,
                    name: name.clone(),
                    input: input.clone(),
                    progress: String::new(),
                }
            });
            if sent.name != *name || sent.input != input {
                self.publish_on(
                    app,
                    LoopEvent::ToolCallArgsDelta {
                        id: id.clone(),
                        name: name.clone(),
                        input_partial: input.clone(),
                    },
                );
                sent.name.clone_from(name);
                sent.input = input;
            }
            let text = call.text();
            match call.status {
                ToolCallStatus::InProgress if sent.phase != ToolSent::Ended => {
                    if sent.phase == ToolSent::Started {
                        self.publish_on(app, LoopEvent::ToolExecutionStart { id: id.clone() });
                        sent.phase = ToolSent::Executing;
                    }
                    if text != sent.progress {
                        self.publish_on(
                            app,
                            LoopEvent::ToolUpdate {
                                id,
                                update: kage_core::ToolUpdate {
                                    content: text.clone(),
                                    structured: None,
                                },
                            },
                        );
                        sent.progress = text;
                    }
                }
                ToolCallStatus::Completed | ToolCallStatus::Failed
                    if sent.phase != ToolSent::Ended =>
                {
                    self.publish_on(
                        app,
                        LoopEvent::ToolCallEnd {
                            id,
                            output: ToolOutput {
                                is_error: call.status == ToolCallStatus::Failed,
                                text,
                                ..ToolOutput::default()
                            },
                        },
                    );
                    sent.phase = ToolSent::Ended;
                }
                _ => {}
            }
        }
    }

    /// Publishes one finished child transcript item whole.
    fn commit_child_item(&mut self, app: SessionId, item: &kage_client::TranscriptItem) {
        match item {
            kage_client::TranscriptItem::User { content, .. } => {
                let message = Message::new(Role::User, content_of(content), None);
                self.publish_on(
                    app,
                    LoopEvent::MessageAppended {
                        message: Arc::new(message),
                    },
                );
            }
            kage_client::TranscriptItem::Assistant { text } => {
                self.publish_on(
                    app,
                    LoopEvent::TextDelta {
                        id: MessageId::new(),
                        delta: text.clone(),
                    },
                );
                self.message_end(app);
            }
            kage_client::TranscriptItem::Thinking { text, .. } => {
                self.publish_on(
                    app,
                    LoopEvent::ThinkingDelta {
                        id: MessageId::new(),
                        delta: text.clone(),
                    },
                );
                self.message_end(app);
            }
            kage_client::TranscriptItem::Media { mime, data } => {
                self.publish_on(
                    app,
                    LoopEvent::Media {
                        id: MessageId::new(),
                        kind: media_kind(mime),
                        mime: mime.clone(),
                        data: data.clone(),
                    },
                );
            }
            kage_client::TranscriptItem::Notice { tone, text } => {
                self.publish_on(
                    app,
                    HostEvent::Notice {
                        level: notice_level(*tone),
                        text: text.clone(),
                        transient: false,
                    },
                );
            }
            kage_client::TranscriptItem::Compaction {
                kept,
                summarized,
                summary,
                ..
            } => {
                self.publish_on(
                    app,
                    LoopEvent::Compaction {
                        kept: usize::try_from(*kept).unwrap_or(0),
                        summarized: usize::try_from(summarized.unwrap_or(0)).unwrap_or(0),
                        summary: summary.clone().unwrap_or_default(),
                        before: 0,
                        estimate: 0,
                    },
                );
            }
            // Tool calls went out in the phase walk; turn boundaries,
            // plans and decision records have no agent-buffer shape.
            kage_client::TranscriptItem::ToolCall(_)
            | kage_client::TranscriptItem::TurnEnd { .. }
            | kage_client::TranscriptItem::Plan { .. }
            | kage_client::TranscriptItem::Decision { .. } => {}
        }
    }

    /// Closes a child's streaming message bubble.
    fn message_end(&mut self, app: SessionId) {
        self.publish_on(
            app,
            LoopEvent::MessageEnd {
                id: MessageId::new(),
                usage: kage_core::TokenUsage::default(),
                stop_reason: kage_core::StopReason::EndTurn,
            },
        );
    }

    /// A subagent's permission asks moved. Children ask no questions,
    /// so only tool approvals forward.
    fn child_asks(&mut self, wire: &str) {
        if !self.children.contains_key(wire) {
            return;
        }
        let (open, asks) = {
            let Some(session) = self.store.state().session(wire) else {
                return;
            };
            let open: HashSet<RequestId> = session
                .permissions
                .iter()
                .map(|ask| ask.request_id.clone())
                .collect();
            (open, session.permissions.clone())
        };
        let app = self.app_of(wire);
        let closed = self.close_stray_asks(wire, &open);
        for wire_id in closed {
            self.publish_resolved(&wire_id);
        }
        for ask in asks {
            if ask.question.is_some() || !self.forwarded.insert(ask.request_id.clone()) {
                continue;
            }
            let app_id = self.next_ask;
            self.next_ask += 1;
            self.ask_ids
                .insert(app_id, (wire.to_owned(), ask.request_id.clone()));
            self.publish_on(
                app,
                HostEvent::PermissionRequested {
                    request_id: kage_core::protocol::RequestId(app_id),
                    tool_call_id: Some(ToolCallId::new(&ask.tool_call.tool_call_id)),
                    tool: ask.tool_call.title.clone().unwrap_or_else(|| ask.subject()),
                    subject: ask.subject(),
                    input: ask.tool_call.raw_input.clone().unwrap_or(Value::Null),
                },
            );
        }
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

    /// Points the mirror at the wire session's file, which the exit
    /// summary, the session tree and the agent loader read.
    fn note_session_file(&mut self) {
        self.point_mirror(crate::paths::sessions_dir());
    }

    /// Points the mirror at `<dir>/<wire id>.jsonl`, or at nothing when
    /// the sessions directory cannot be resolved: a stale path would
    /// send the readers to another session's file.
    fn point_mirror(&mut self, dir: Result<PathBuf, String>) {
        let mut mirror = lock(&self.mirror);
        match dir {
            Ok(dir) => mirror.point_at(dir.join(format!("{}.jsonl", self.wire))),
            Err(_) => mirror.clear(),
        }
    }

    /// Draws from the freshly opened session from now on: the buffer
    /// it folds into, no stale cards, and on a brand-new session the
    /// model carried over. A resumed session keeps the model it was
    /// recorded with, as the engine path does.
    fn adopt(&mut self, id: &str, carry_model: bool) {
        if self.wire != id {
            self.close_ask_cards();
            self.children.clear();
            lock(&self.child_files).clear();
        }
        if self.running {
            self.store.client().cancel(&self.wire);
            self.running = false;
            let outcome = RunOutcome::Cancelled;
            let _ = self.recorder.send((self.model.clone(), outcome.clone()));
            self.publish(HostEvent::RunEnded { outcome });
        }
        let old = std::mem::replace(&mut self.wire, id.to_owned());
        // A cold start adopts its first session off an empty wire;
        // there is nothing to close. A bootstrap resume adopts the
        // session `wire` already names: closing it would end the very
        // session the App just moved onto. The session the App leaves
        // is closed on the agent side, so the shared host does not
        // keep it loaded for a TUI that moved on.
        if !old.is_empty() && old != id {
            self.store.client().close_session(&old);
        }
        self.warned_model = None;
        self.handed = None;
        self.hand_buffer();
        self.note_session_file();
        self.update_ui_slot(|ui| {
            ui.session_id.clone_from(&self.wire);
            ui.session_title = None;
        });
        // An external session carries no engine model option: the
        // status shows the agent pick instead, and nothing is sent.
        let external = self.external_agent_of(id);
        if let Some(agent) = &external {
            self.external.insert(id.to_owned(), agent.clone());
            self.model = format!("acp:{agent}");
        }
        if carry_model && external.is_none() {
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
        self.agents_moved(id);
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
        let closed = self.close_stray_asks(id, &open);
        for wire_id in closed {
            self.publish_resolved(&wire_id);
        }
        for ask in asks {
            if !self.forwarded.insert(ask.request_id.clone()) {
                continue;
            }
            let app_id = self.next_ask;
            self.next_ask += 1;
            self.ask_ids
                .insert(app_id, (self.wire.clone(), ask.request_id.clone()));
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
        let found = self
            .ask_ids
            .iter()
            .find(|(_, (_, wire))| wire == wire_id)
            .map(|(app, (session, _))| (*app, session.clone()));
        if let Some((app, session)) = found {
            // One resolved per ask: the entry leaves, so a second
            // notice of the same ask publishes nothing.
            self.ask_ids.remove(&app);
            self.publish_on(
                self.app_of(&session),
                HostEvent::PermissionResolved {
                    request_id: kage_core::protocol::RequestId(app),
                },
            );
        }
    }

    /// The forwarded ids of one session's asks, closing and dropping
    /// the ones that left it. Ids another session forwarded stay
    /// alone: a child's ask moving says nothing about the main
    /// session's cards.
    fn close_stray_asks(&mut self, wire: &str, open: &HashSet<RequestId>) -> Vec<RequestId> {
        let mine: HashSet<RequestId> = self
            .ask_ids
            .values()
            .filter(|(session, _)| session == wire)
            .map(|(_, wire_id)| wire_id.clone())
            .collect();
        let closed: Vec<RequestId> = self
            .forwarded
            .iter()
            .filter(|wire_id| mine.contains(*wire_id) && !open.contains(*wire_id))
            .cloned()
            .collect();
        self.forwarded
            .retain(|wire_id| !mine.contains(wire_id) || open.contains(wire_id));
        closed
    }

    /// Closes every ask card the App still shows: the conversation
    /// moved elsewhere, so the asks cannot be answered here.
    fn close_ask_cards(&mut self) {
        for wire_id in std::mem::take(&mut self.forwarded) {
            self.publish_resolved(&wire_id);
        }
    }

    fn ui_moved(&mut self) {
        // Hosting the plugins makes the local runtime authoritative
        // for its own surfaces: the agent's `_kage/ui` snapshot
        // describes the host process's project runtime, whose handler
        // ids and command set never match what this process serves.
        if self.plugins.is_some() {
            return;
        }
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
        // An external session reports no model option: the status
        // shows the agent pick the session was opened with.
        let model = option("model").unwrap_or_else(|| {
            self.external_agent_of(&self.wire)
                .map_or_else(String::new, |agent| format!("acp:{agent}"))
        });
        SessionState {
            model,
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
        self.store
            .state()
            .session(&self.wire)
            .map(|session| Self::usage_of(&session.usage))
            .unwrap_or_default()
    }

    /// The App's usage totals for a session's store usage.
    fn usage_of(usage: &kage_client::Usage) -> kage_core::protocol::Usage {
        let tokens = usage.tokens.unwrap_or_default();
        kage_core::protocol::Usage {
            total: kage_core::TokenUsage {
                input: tokens.input,
                output: tokens.output,
                cache_read: tokens.cache_read,
                cache_write: tokens.cache_write,
            },
            context_used: usage.used,
            context_window: usage.size,
            cost: usage.cost.as_ref().map_or(0.0, |cost| cost.amount),
            context_estimated: false,
        }
    }

    /// Returns submissions that raced with transport shutdown.
    fn handle_closed(&mut self, request: RunRequest) {
        if let RunRequest::Submit {
            text,
            images,
            queue,
            session,
        } = request
        {
            self.return_prompt(
                session.unwrap_or(self.app_session),
                submission_content(text, &images),
                queue,
                DETACHED.to_owned(),
            );
        } else {
            self.notice(NoticeLevel::Error, DETACHED.to_owned());
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one dispatch over every request the App sends"
    )]
    fn handle(&mut self, request: RunRequest) {
        if self.closed {
            self.handle_closed(request);
            return;
        }
        if self.refuse_external_request(&request) {
            return;
        }
        match request {
            RunRequest::Submit {
                text,
                images,
                queue,
                session,
            } => match self.route_of(session) {
                Some(wire) => self.submit_to(&wire, text, images, queue),
                None => self.return_prompt(
                    session.unwrap_or(self.app_session),
                    submission_content(text, &images),
                    queue,
                    GONE_AGENT.to_owned(),
                ),
            },
            RunRequest::Cancel { session } => self.cancel_of(session),
            RunRequest::RecallPrompt { session, delivery } => self.recall_of(session, delivery),
            RunRequest::SwitchModel(model) => {
                // An external agent is a different backend, not a
                // model: it opens as a session of its own, like the
                // desktop's new-session pick.
                if let Some(agent) = external_agent(&model).map(str::to_owned) {
                    self.model = model;
                    self.request_external_session(&agent);
                } else {
                    self.model.clone_from(&model);
                    self.store
                        .client()
                        .set_config_option(&self.wire, "model", &model);
                }
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
                self.invoke_plugin_command(&name, &args);
            }
            RunRequest::InvokeKeymap { id } => self.invoke_keymap(id),
            RunRequest::NewSession => self.request_new_session(),
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

    /// The wire id a prompt or stop aims at: the main session for
    /// `None` or the main App id, the mapped child otherwise. `None`
    /// means an id nothing owns, never a fallback to the main run.
    fn route_of(&self, session: Option<SessionId>) -> Option<String> {
        match session {
            None => Some(self.wire.clone()),
            Some(app) if app == self.app_session => Some(self.wire.clone()),
            Some(app) => self.wire_of(app),
        }
    }

    /// Cancels a child's run, or the wire session's with every agent
    /// under it.
    fn cancel_of(&mut self, session: Option<SessionId>) {
        match self.route_of(session) {
            Some(wire) if wire == self.wire => {
                if let Some(surface) = &self.plugins {
                    surface.turn.cancel();
                }
                self.store.client().cancel(&self.wire);
            }
            Some(wire) => self.store.client().cancel(&wire),
            None => self.notice(NoticeLevel::Warning, GONE_AGENT.to_owned()),
        }
    }

    /// Sends a prompt to `wire`: steering while its run is in
    /// flight, plain otherwise, and the echo published on the App
    /// session that asked.
    fn submit_to(
        &mut self,
        wire: &str,
        text: String,
        images: Vec<kage_tui::image::AttachedImage>,
        queue: bool,
    ) {
        if wire != self.wire {
            let live = self
                .children
                .get(wire)
                .is_some_and(|track| track.running && !track.ended);
            let refusal = if queue {
                Some("subagents accept live steering only; queued prompts are not supported")
            } else if !live {
                Some("that agent has finished; only a running agent can be steered")
            } else {
                None
            };
            if let Some(message) = refusal {
                self.return_prompt(
                    self.app_of(wire),
                    submission_content(text, &images),
                    queue,
                    message.to_owned(),
                );
                return;
            }
        }
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
        if wire != self.wire {
            if let Err(error) = self.store.client().steer(wire, blocks.clone()) {
                self.return_prompt(
                    self.app_of(wire),
                    content_of(&blocks),
                    false,
                    match error {
                        kage_client::SteerError::NotRunning => {
                            "that agent is no longer running".to_owned()
                        }
                        kage_client::SteerError::NotAdvertised => {
                            "the host does not support live steering".to_owned()
                        }
                    },
                );
            }
            return;
        }
        if queue {
            self.store.client().prompt(wire, blocks);
            return;
        }
        // Steering joins the run in flight; anything else goes plain,
        // which queues while a run is in flight and starts one
        // otherwise. An external session cannot be steered: its agent
        // owns the turn, so the prompt goes plain and the host decides.
        let running = self
            .store
            .state()
            .session(wire)
            .is_some_and(|session| session.running);
        if running
            && self.external_agent_of(wire).is_none()
            && self.store.client().steer(wire, blocks.clone()).is_ok()
        {
            return;
        }
        self.store.client().prompt(wire, blocks);
    }

    /// Returns a refused submission to the composer instead of silently
    /// dropping it when its child session cannot accept the prompt.
    fn return_prompt(
        &mut self,
        app: SessionId,
        content: Vec<kage_core::Content>,
        queue: bool,
        message: String,
    ) {
        self.publish_on(
            app,
            HostEvent::PromptWithdrawn {
                delivery: if queue {
                    Delivery::Queue
                } else {
                    Delivery::Steer
                },
                content: Some(content),
            },
        );
        self.notice(NoticeLevel::Warning, message);
    }

    /// Recalls only main-session prompts. Child steering is live-only
    /// and cannot withdraw an unrelated prompt from the main session.
    fn recall_of(&mut self, session: Option<SessionId>, delivery: Delivery) {
        if self.route_of(session).is_some_and(|wire| wire == self.wire) {
            self.recall(delivery);
        } else {
            self.notice(
                NoticeLevel::Warning,
                "subagent steering cannot be recalled".to_owned(),
            );
        }
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
        let Some((session, wire_id)) = self.ask_ids.get(&request_id.0).cloned() else {
            return;
        };
        let answer = match decision {
            kage_tui::PermissionDecision::AllowOnce => PermissionDecision::Allow,
            kage_tui::PermissionDecision::AllowSession
            | kage_tui::PermissionDecision::AllowAlways => {
                let has_always = self
                    .store
                    .state()
                    .session(&session)
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
            .reply_permission(&session, wire_id, &answer);
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
        let Some((session, wire_id)) = self.ask_ids.get(&request_id.0).cloned() else {
            return;
        };
        let row = answers.and_then(|rows| rows.into_iter().next());
        let Some(row) = row.filter(|row| !row.is_empty()) else {
            self.store
                .client()
                .reply_permission(&session, wire_id, &PermissionDecision::Cancel);
            return;
        };
        let option_id = self
            .store
            .state()
            .session(&session)
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
            .reply_permission(&session, wire_id, &answer);
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

    /// Replays a recorded session: the wire id is the file stem, and
    /// the replay folds into a fresh buffer once the agent answers.
    /// The App stays on its current session until that answer adopts
    /// the new one, so a failed load loses nothing.
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
        if self.pending_open.as_ref().is_some_and(|open| open.id == id) || id == self.wire {
            return;
        }
        // The wire list never describes this connection's sessions,
        // so a recorded external session is recognized by its file's
        // header before the load answers.
        if let Some(agent) = recorded_external_agent(path) {
            self.external.insert(id.clone(), agent);
        }
        self.store.reset_session(&id);
        let meta = self.load_meta();
        let servers = self.mcp_servers();
        let (request, _) = self
            .store
            .client()
            .load_session(&id, &self.cwd, &servers, Some(meta));
        self.pending_open = Some(PendingOpen {
            id,
            request,
            bootstrap: false,
        });
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
        // A tool call the agent left running must not run on: the
        // surface's next execute sees the flag at once.
        if let Some(surface) = &self.plugins {
            surface.cancel.cancel();
        }
        self.store.interrupt_running();
        let children: Vec<String> = self.children.keys().cloned().collect();
        for wire in children {
            self.end_child(&wire, RunOutcome::Cancelled);
        }
        if self.running {
            self.running = false;
            let outcome = RunOutcome::Cancelled;
            let _ = self.recorder.send((self.model.clone(), outcome.clone()));
            self.publish(HostEvent::RunEnded { outcome });
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
        self.publish_on(self.app_session, event);
    }

    /// Publishes an envelope on `session`'s App view: the main
    /// session's id for the wire session, a child's mapped id for one
    /// of its subagents.
    fn publish_on(&mut self, session: SessionId, event: impl Into<kage_core::protocol::Event>) {
        let event = event.into();
        // The one-shot warning is for engine sessions on the `acp`
        // provider; an external session's `acp:<name>` model is the
        // agent pick, not that provider.
        let warning = if session == self.app_session
            && self.external_agent_of(&self.wire).is_none()
            && let kage_core::protocol::Event::Host(HostEvent::StateChanged { state }) = &event
            && self.warned_model.as_deref() != Some(state.model.as_str())
        {
            self.warned_model = Some(state.model.clone());
            support::legacy_acp_warning(&state.model)
        } else {
            None
        };
        self.seq += 1;
        let _ = self.events.send(Envelope {
            session,
            seq: self.seq,
            event,
        });
        if let Some(warning) = warning {
            self.notice(NoticeLevel::Error, warning.to_owned());
        }
    }

    /// Feeds the runtime's UI-state slot the way the engine path's
    /// bus subscriber does, so plugin widgets and slots read the
    /// session's current shape.
    fn update_ui_slot(&self, f: impl FnOnce(&mut kage_plugin::UiState)) {
        let Some(surface) = self.plugins.as_ref() else {
            return;
        };
        let ui = surface.runtime.slots().ui_state();
        f(&mut lock(&ui));
    }

    fn notice(&mut self, level: NoticeLevel, text: String) {
        self.publish(HostEvent::Notice {
            level,
            text,
            transient: false,
        });
    }

    /// Runs a plugin command the App asked for. Hosting the plugins
    /// runs it here, where its handler's dialogs ride the App's dialog
    /// channel and the output lands as a notice; otherwise the agent,
    /// which hosts them, is asked over the wire.
    fn invoke_plugin_command(&mut self, name: &str, args: &str) {
        let Some(runtime) = self.plugins.as_ref().map(|s| Arc::clone(&s.runtime)) else {
            self.store
                .client()
                .ui_command(&self.cwd, name, args, Some(&self.wire));
            return;
        };
        let command = runtime
            .registered_command_overrides()
            .into_iter()
            .chain(runtime.registered_commands())
            .find(|command| command.name() == name);
        let Some(command) = command else {
            self.notice(NoticeLevel::Error, format!("no plugin command: {name}"));
            return;
        };
        let dialog_tx = self.dialog_tx.clone();
        let mut errors = |text| self.notice(NoticeLevel::Error, text);
        let output = run_bridged_command(&runtime, &command, args, &dialog_tx, &mut errors);
        self.plugin_output(output);
    }

    /// Runs a key mapping's handler the way
    /// [`Self::invoke_plugin_command`] runs a command.
    fn invoke_keymap(&mut self, id: u64) {
        let Some(runtime) = self.plugins.as_ref().map(|s| Arc::clone(&s.runtime)) else {
            self.store
                .client()
                .ui_keymap(&self.cwd, id, Some(&self.wire));
            return;
        };
        let dialog_tx = self.dialog_tx.clone();
        let mut errors = |text| self.notice(NoticeLevel::Error, text);
        let output = run_bridged_keymap(&runtime, id, &dialog_tx, &mut errors);
        self.plugin_output(output);
    }

    /// Publish a locally run plugin command or keymap output the way
    /// the engine path's host does: a notice, an error one when it
    /// failed, nothing when the handler produced no text.
    fn plugin_output(&mut self, output: Option<CommandOutput>) {
        let Some(output) = output.filter(|output| !output.text.is_empty()) else {
            return;
        };
        let level = if output.is_error {
            NoticeLevel::Error
        } else {
            NoticeLevel::Info
        };
        self.notice(level, output.text);
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

/// The original draft content of a submission that could not be sent.
fn submission_content(
    text: String,
    images: &[kage_tui::image::AttachedImage],
) -> Vec<kage_core::Content> {
    let mut content = Vec::new();
    if !text.is_empty() || images.is_empty() {
        content.push(kage_core::Content::Text { text });
    }
    content.extend(images.iter().map(|image| kage_core::Content::Image {
        source: image.source.clone(),
        mime: image.mime.clone(),
    }));
    content
}

/// The modality a child's media item carries, by MIME type.
fn media_kind(mime: &str) -> Modality {
    if mime.starts_with("audio/") {
        Modality::Audio
    } else if mime.starts_with("video/") {
        Modality::Video
    } else if mime == "application/pdf" {
        Modality::Pdf
    } else {
        Modality::Image
    }
}

/// The App's severity for a child notice's tone.
fn notice_level(tone: kage_client::wire::NoticeTone) -> NoticeLevel {
    match tone {
        kage_client::wire::NoticeTone::Info => NoticeLevel::Info,
        kage_client::wire::NoticeTone::Warn => NoticeLevel::Warning,
        kage_client::wire::NoticeTone::Error => NoticeLevel::Error,
        kage_client::wire::NoticeTone::Success => NoticeLevel::Success,
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

/// Which agent the TUI drives: the shared host of this user, or a
/// private host in this process when `--standalone`, `[host] share =
/// false`, a client configuration override, a host of another version,
/// a host that cannot be started, or a platform without the socket
/// rules the shared one out.
fn choose_upstream(standalone: bool, workdir: &Path) -> Upstream {
    #[cfg(not(unix))]
    {
        let _ = (standalone, workdir);
        return Upstream::Private;
    }
    #[cfg(unix)]
    {
        if standalone
            || !crate::user_host_config().share
            || kage_core::config::Config::env_overrides_active(workdir)
        {
            return Upstream::Private;
        }
        match crate::host_client::connect() {
            Ok(stream) => Upstream::Host(stream),
            Err(error) => {
                eprintln!("kage: {error}; using a private host");
                Upstream::Private
            }
        }
    }
}

/// Runs the TUI over an ACP link to `kage rpc`, to the shared host's
/// socket, or to a private host in this process, on the recorded
/// session at `resume` when given. Returns the exit code once the user
/// quits.
#[expect(clippy::too_many_lines, reason = "one linear startup sequence")]
pub(crate) fn run(
    model: Option<&str>,
    system: &str,
    resume: Option<&Path>,
    yolo: bool,
    upstream: Upstream,
) -> ExitCode {
    let mut registry = match crate::build_provider_registry() {
        Ok(registry) => registry,
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
    let mut shown_model = model.map_or_else(|| crate::default_model(&registry), str::to_owned);
    // The plugins load in this process, like the engine path builds its
    // runtime: tools, commands, handlers and the UI surfaces run beside
    // the pump, and the session names this process the plugin host.
    let log_publisher: Arc<OnceLock<LogPublisher>> = Arc::default();
    let plugins_dir_path = match crate::plugins_dir() {
        Ok(dir) => Some(dir),
        Err(e) => {
            lock(&buffer).push_custom("kage:error", e, false);
            None
        }
    };
    let user_dir = crate::config_dir().ok();
    // Built against a bare prompt, like the engine path; the session's
    // real prompt is the agent's business.
    let bare_prompt = crate::runtime_env::build_system_prompt(
        system,
        &workdir,
        &shown_model,
        &[],
        config.shell.program.as_deref(),
    );
    let plugins = match setup_user_runtime(
        plugins_dir_path.as_deref(),
        user_dir.as_deref(),
        config.plugins.clone(),
        config.keybindings.clone(),
        Arc::clone(&options),
        &workdir,
        &shown_model,
        &bare_prompt,
        buffer_host_log(buffer.clone(), toasts.clone(), Arc::clone(&log_publisher)),
        &kage_core::environment::Environment::inherit(),
    ) {
        Ok(rt) => Some(rt),
        Err(e) => {
            lock(&buffer).push_custom("kage:error", e, false);
            None
        }
    };
    if let Some(rt) = plugins.as_ref() {
        for id in crate::plugins::merge_plugin_providers(rt, &mut registry) {
            eprintln!("kage: plugin provider `{id}` shadows the built-in registration");
        }
        register_block_renderers(rt);
        let ui = rt.slots().ui_state();
        lock(&ui).cwd = workdir.display().to_string();
    }
    // First-run onboarding, like the engine path ran it: with no
    // credentials anywhere, walk the new user through `auth login`
    // (its provider picker and key prompt run in the normal terminal
    // before the TUI starts) instead of exiting with instructions.
    // Esc on the picker or an empty key falls back to the env-var
    // guidance and exits.
    if !crate::has_usable_provider(&registry) && registry.resolve(&shown_model).is_err() {
        eprintln!("kage: welcome! no provider credentials found, let's add one.");
        loop {
            let code = crate::auth::run_login(None, &config);
            if code != ExitCode::SUCCESS {
                eprintln!("{}", crate::NO_CREDENTIALS_MESSAGE);
                return ExitCode::from(1);
            }
            registry = match crate::build_provider_registry() {
                Ok(registry) => registry,
                Err(e) => {
                    eprintln!("kage: {e}");
                    return ExitCode::from(1);
                }
            };
            if let Some(rt) = plugins.as_ref() {
                for id in crate::plugins::merge_plugin_providers(rt, &mut registry) {
                    eprintln!("kage: plugin provider `{id}` shadows the built-in registration");
                }
            }
            if crate::has_usable_provider(&registry) {
                break;
            }
            eprintln!("kage: credential saved, but no provider is usable yet; add another.");
        }
    }
    // Recompute the default against the merged registry so a last-used
    // plugin-provided model resolves on restart.
    if model.is_none() {
        shown_model = crate::default_model(&registry);
    }
    if shown_model.is_empty() {
        eprintln!("{}", crate::NO_MODEL_MESSAGE);
        return ExitCode::from(1);
    }
    if let Err(err) = crate::state::record_last_model(&shown_model) {
        lock(&buffer).push_custom("kage:error", format!("state: {err}"), false);
    }
    let notices = start_notices(
        &registry,
        model.is_none().then_some(shown_model.as_str()),
        chrono::Utc::now(),
    );
    let registry = Arc::new(registry);
    let app_session = SessionId::new();
    let store = Store::new();
    let (frames_tx, frames_rx) = mpsc::channel();
    let upstream = match upstream {
        Upstream::Decide { standalone } => choose_upstream(standalone, &workdir),
        upstream => upstream,
    };
    #[cfg(unix)]
    let environment = matches!(upstream, Upstream::Host(_));
    #[cfg(not(unix))]
    let environment = false;
    let transport = match upstream {
        Upstream::Child => Transport::child(frames_tx),
        #[cfg(unix)]
        Upstream::Host(stream) => Transport::host(stream, frames_tx),
        Upstream::Private => Transport::memory(model, system, yolo, frames_tx),
        Upstream::Decide { .. } => unreachable!("resolved above"),
    };
    let transport = match transport {
        Ok(transport) => transport,
        Err(error) => {
            eprintln!("kage: {error}");
            return ExitCode::from(1);
        }
    };
    let (events_tx, events_rx) = mpsc::channel();
    let (requests_tx, requests_rx) = mpsc::channel::<RunRequest>();
    let (dialog_tx, dialog_rx) = mpsc::channel::<PluginDialog>();
    // Plugin hot reload stays off: the pump cannot reload a runtime
    // whose tools the agent already snapshotted for the session.
    let (refresh_tx, refresh_rx) = mpsc::channel::<PluginRefresh>();
    drop(refresh_tx);
    let feed = Arc::new(FeedState::default());
    let surface = plugins
        .as_ref()
        .map(|rt| PluginSurface::new(Arc::clone(rt), workdir.clone()));
    let (plugin_answers_tx, plugin_answers) = mpsc::channel();
    let (recorder, finished_runs) = mpsc::channel();
    start_run_recorder(finished_runs);
    let mirror = Arc::new(Mutex::new(Mirror::new()));
    let child_files = Arc::new(Mutex::new(HashMap::new()));
    let mut pump = Pump {
        store,
        transport,
        frames: frames_rx,
        events: events_tx.clone(),
        requests: requests_rx,
        dialog_tx,
        dialogs: Vec::new(),
        feed: Arc::clone(&feed),
        mirror: Arc::clone(&mirror),
        app_session,
        wire: String::new(),
        pending_open: None,
        pending_new: None,
        cwd: workdir.display().to_string(),
        model: shown_model.clone(),
        warned_model: None,
        options: Arc::clone(&options),
        registry: Arc::clone(&registry),
        plugins: None,
        plugin_jobs: None,
        plugin_answers,
        plugin_answers_tx,
        recorder,
        prompts: HashMap::new(),
        run_error: None,
        external: HashMap::new(),
        yolo,
        environment,
        system_prompt: system.to_owned(),
        seq: 0,
        running: false,
        handed: None,
        last_keymap: None,
        forwarded: HashSet::new(),
        dialog_shown: HashSet::new(),
        ask_ids: BTreeMap::new(),
        children: HashMap::new(),
        child_files: Arc::clone(&child_files),
        next_ask: 0,
        pending_export: None,
        unknown: HashSet::new(),
        mcp_warnings: HashSet::new(),
        closed: false,
    };
    if let Some(surface) = surface
        && let Err(error) = pump.host_plugins(surface)
    {
        eprintln!("kage: {error}");
        return ExitCode::from(1);
    }
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
    let mut model_choices = available_model_items(&registry, &shown_model);
    model_choices.extend(external_agent_items(&config, &shown_model));
    // The commands the local plugins register; the App completes them
    // and the pump runs them, never the agent.
    let plugin_commands = plugins
        .as_ref()
        .map(|rt| snapshot_plugin_commands(rt))
        .unwrap_or_default();
    let skills = crate::load_skills(&workdir, plugins.as_deref());
    Frontend {
        buffer: root,
        toasts,
        options,
        config,
        workdir,
        plugins,
        mirror,
        child_files,
        requests: requests_tx,
        events: events_rx,
        dialogs: dialog_rx,
        refresh: refresh_rx,
        acp_feed: feed,
        model: shown_model,
        model_choices,
        plugin_commands,
        skills,
        notices,
        session: app_session,
    }
    .run(|| {})
}

#[cfg(test)]
mod tests {
    use super::*;
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
    #[cfg(unix)]
    fn echo_child(dir: &Path) -> String {
        use std::os::unix::fs::PermissionsExt;
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
    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn frames_round_trip_through_a_stream_pair() {
        let (mine, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let (events, received) = mpsc::channel();
        let transport = Transport::stream(
            BufReader::new(mine.try_clone().unwrap()),
            mine,
            None,
            events,
        )
        .unwrap();
        let agent = std::thread::spawn(move || {
            let mut reader = BufReader::new(&theirs);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            (&theirs).write_all(line.as_bytes()).unwrap();
            // The end drops when the closure's return does, ending the
            // transport's read side.
            theirs
        });
        transport.send(Frame::Request {
            id: RequestId::Number(1),
            method: "initialize".to_owned(),
            params: serde_json::json!({"protocolVersion": 1}),
        });
        let wait = Duration::from_secs(30);
        match received.recv_timeout(wait) {
            Ok(Event::Frame(Frame::Request { method, .. })) => {
                assert_eq!(method, "initialize");
            }
            other => panic!("expected the echo, got {other:?}"),
        }
        drop(agent);
        loop {
            match received.recv_timeout(wait) {
                Ok(Event::Frame(_)) => {}
                Ok(Event::Closed) => break,
                other => panic!("expected the close, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_environment_notification_names_this_process_environment() {
        let Some(Frame::Notification { method, params }) = environment_frame() else {
            panic!("the environment rides a notification")
        };
        assert_eq!(method, "_kage/environment");
        assert!(params["vars"].is_object(), "{params}");
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

    /// A pump on an in-memory pipe with nobody behind it, with the
    /// channels the App would hold. The pump is driven method by
    /// method, so the agent never speaks.
    fn test_pump() -> (
        Pump,
        mpsc::Receiver<Envelope>,
        mpsc::Receiver<PluginDialog>,
        Arc<FeedState>,
        PathBuf,
    ) {
        let dir = scratch();
        let (frames_tx, frames_rx) = mpsc::channel();
        let (to_agent, unread) = crate::tui::pipe::channel_pair();
        let (unwritten, from_agent) = crate::tui::pipe::channel_pair();
        // The silent agent's ends stay open for the life of the test
        // process, so the connection never reads as closed.
        Box::leak(Box::new((unread, unwritten)));
        let transport =
            Transport::stream(BufReader::new(from_agent), to_agent, None, frames_tx).unwrap();
        let (events, app) = mpsc::channel();
        let (_requests, requests_rx) = mpsc::channel::<RunRequest>();
        let (dialog_tx, dialog_rx) = mpsc::channel::<PluginDialog>();
        let feed = Arc::new(FeedState::default());
        let (plugin_answers_tx, plugin_answers) = mpsc::channel();
        let (recorder, _recorded) = mpsc::channel();
        let pump = Pump {
            store: Store::new(),
            transport,
            frames: frames_rx,
            events,
            requests: requests_rx,
            dialog_tx,
            dialogs: Vec::new(),
            feed: Arc::clone(&feed),
            mirror: Arc::new(Mutex::new(Mirror::new())),
            app_session: SessionId::new(),
            wire: "s1".to_owned(),
            pending_open: None,
            pending_new: None,
            cwd: "/w".to_owned(),
            model: "test:model".to_owned(),
            warned_model: None,
            options: Arc::new(Mutex::new(kage_core::options::OptionStore::default())),
            registry: Arc::new(ProviderRegistry::default()),
            plugins: None,
            plugin_jobs: None,
            plugin_answers,
            plugin_answers_tx,
            recorder,
            prompts: HashMap::new(),
            run_error: None,
            yolo: false,
            environment: false,
            system_prompt: String::new(),
            seq: 0,
            running: false,
            handed: None,
            last_keymap: None,
            forwarded: HashSet::new(),
            dialog_shown: HashSet::new(),
            ask_ids: BTreeMap::new(),
            children: HashMap::new(),
            external: HashMap::new(),
            child_files: Arc::new(Mutex::new(HashMap::new())),
            next_ask: 0,
            pending_export: None,
            unknown: HashSet::new(),
            mcp_warnings: HashSet::new(),
            closed: false,
        };
        (pump, app, dialog_rx, feed, dir)
    }

    /// The model option a load or session answer carries.
    fn model_options(model: &str) -> Value {
        serde_json::json!([{
            "id": "model", "name": "Model", "category": "model",
            "type": "select", "currentValue": model, "options": []
        }])
    }

    /// The id of the first request of `method` among `frames`.
    fn request_id(frames: &[Frame], method: &str) -> RequestId {
        frames
            .iter()
            .find_map(|frame| match frame {
                Frame::Request { id, method: m, .. } if m == method => Some(id.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {method} request in {frames:?}"))
    }

    #[cfg(unix)]
    fn has_notification(frames: &[Frame], method: &str) -> bool {
        frames
            .iter()
            .any(|frame| matches!(frame, Frame::Notification { method: m, .. } if m == method))
    }

    fn has_request(frames: &[Frame], method: &str) -> bool {
        frames
            .iter()
            .any(|frame| matches!(frame, Frame::Request { method: m, .. } if m == method))
    }

    /// Opens the recorded session `id` and answers the load with
    /// `model`.
    fn resume_as(pump: &mut Pump, id: &str, model: &str) {
        pump.open_recorded(Path::new(&format!("/sessions/{id}.jsonl")));
        let load = request_id(&pump.store.take_outgoing(), "session/load");
        pump.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({"configOptions": model_options(model)}),
        });
    }

    fn notices(app: &mpsc::Receiver<Envelope>) -> Vec<(NoticeLevel, String)> {
        app.try_iter()
            .filter_map(|envelope| match envelope.event {
                kage_core::protocol::Event::Host(HostEvent::Notice { level, text, .. }) => {
                    Some((level, text))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_resumed_legacy_model_warns_once_and_switching_back_warns_again() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        resume_as(&mut pump, "resumed", "acp:reviewer");
        let warnings = || {
            app.try_iter()
                .filter(|envelope| {
                    matches!(
                        &envelope.event,
                        kage_core::protocol::Event::Host(HostEvent::Notice { text, .. })
                            if text.contains("legacy one-shot ACP")
                    )
                })
                .count()
        };
        assert_eq!(warnings(), 1);
        for model in ["test:model", "acp:reviewer", "acp:reviewer"] {
            pump.absorb(Frame::Notification {
                method: "session/update".into(),
                params: serde_json::json!({"sessionId": "resumed", "update": {
                    "sessionUpdate": "config_option_update",
                    "configOptions": model_options(model)
                }}),
            });
        }
        assert_eq!(warnings(), 1);
        resume_as(&mut pump, "another", "acp:reviewer");
        assert_eq!(
            warnings(),
            1,
            "a different transcript needs its own warning"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_opens_only_when_its_load_answers() {
        let (mut pump, app, _dialogs, feed, dir) = test_pump();
        pump.open_recorded(Path::new("/sessions/next.jsonl"));
        let sent = pump.store.take_outgoing();
        let load = request_id(&sent, "session/load");
        assert!(
            !has_request(&sent, "session/close"),
            "the old session stays open while the load is out"
        );
        assert_eq!(pump.wire, "s1");
        assert!(
            kage_tui::AcpFeed::take(&*feed).root_buffer.is_none(),
            "the App keeps its buffer until the load answers"
        );
        pump.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({"configOptions": []}),
        });
        assert_eq!(pump.wire, "next");
        assert!(pump.pending_open.is_none());
        assert!(
            kage_tui::AcpFeed::take(&*feed).root_buffer.is_some(),
            "the adopted session's buffer reaches the App"
        );
        assert!(
            has_request(&pump.store.take_outgoing(), "session/close"),
            "the session the App left is closed once"
        );
        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_load_keeps_the_session_and_a_retry_loads_again() {
        let (mut pump, app, _dialogs, feed, dir) = test_pump();
        pump.open_recorded(Path::new("/sessions/next.jsonl"));
        let load = request_id(&pump.store.take_outgoing(), "session/load");
        pump.absorb(Frame::Failure {
            id: load,
            error: RpcError::new(-32000, "no such session".to_owned()),
        });
        assert_eq!(pump.wire, "s1", "the failed switch never adopts");
        assert!(pump.pending_open.is_none(), "the failure clears the open");
        assert!(kage_tui::AcpFeed::take(&*feed).root_buffer.is_none());
        let sent = pump.store.take_outgoing();
        assert!(
            !has_request(&sent, "session/close"),
            "the old session is still in use"
        );
        let shown = notices(&app);
        assert!(
            shown
                .iter()
                .any(|(level, text)| *level == NoticeLevel::Error
                    && text.contains("no such session")),
            "{shown:?}"
        );
        pump.open_recorded(Path::new("/sessions/next.jsonl"));
        let retry = request_id(&pump.store.take_outgoing(), "session/load");
        pump.absorb(Frame::Success {
            id: retry,
            result: serde_json::json!({"configOptions": []}),
        });
        assert_eq!(pump.wire, "next", "the retry opens the session");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_new_session_clears_the_pending_request() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        pump.handle(RunRequest::NewSession);
        let new = request_id(&pump.store.take_outgoing(), "session/new");
        assert_eq!(new, RequestId::Number(pump.pending_new.expect("in flight")));
        pump.absorb(Frame::Failure {
            id: new,
            error: RpcError::new(-32000, "cannot start".to_owned()),
        });
        assert_eq!(pump.pending_new, None);
        assert_eq!(pump.wire, "s1");
        assert!(
            notices(&app)
                .iter()
                .any(|(_, text)| text.contains("cannot start"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_answer_to_session_new_adopts_a_new_session() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        pump.handle(RunRequest::NewSession);
        let new = request_id(&pump.store.take_outgoing(), "session/new");
        // A session that appears for another reason, such as another
        // client's `session/new` echo, is not the answer.
        pump.absorb(Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({"sessionId": "stray", "update": {
                "sessionUpdate": "config_option_update",
                "configOptions": model_options("test:model")
            }}),
        });
        assert_eq!(pump.wire, "s1", "an unrelated session does not adopt");
        assert!(pump.pending_new.is_some());
        pump.absorb(Frame::Success {
            id: new,
            result: serde_json::json!({"sessionId": "fresh"}),
        });
        assert_eq!(pump.wire, "fresh");
        assert_eq!(pump.pending_new, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn advertise_steering(pump: &mut Pump) {
        let request = pump
            .store
            .client()
            .initialize(ClientCapabilities::default(), None);
        pump.store.take_outgoing();
        pump.absorb(Frame::Success {
            id: RequestId::Number(request),
            result: serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": {"steer": true}
            }),
        });
    }

    /// Announces a subagent of the wire session and returns the App
    /// session its envelopes ride. `extra` folds into the update, as
    /// a terminal `state` does.
    fn announced_child(
        pump: &mut Pump,
        app: &mpsc::Receiver<Envelope>,
        extra: &Value,
    ) -> SessionId {
        let mut update = serde_json::json!({
            "sessionUpdate": "subagent_update",
            "subagentSessionId": "child",
            "name": "general",
            "task": "list files",
            "toolCallId": "call-agent",
            "capabilities": {"cancel": true}
        });
        if let (Some(update), Some(extra)) = (update.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                update.insert(key.clone(), value.clone());
            }
        }
        pump.absorb(Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({"sessionId": "s1", "update": update}),
        });
        let spawned = app
            .try_iter()
            .find_map(|envelope| match &envelope.event {
                kage_core::protocol::Event::Host(HostEvent::AgentSpawned { parent, .. }) => {
                    Some((envelope.session, *parent))
                }
                _ => None,
            })
            .expect("the announcement spawns the agent");
        assert_eq!(spawned.1, pump.app_session, "the child hangs off the main");
        assert_ne!(spawned.0, pump.app_session);
        spawned.0
    }

    /// Feeds one `session/update` frame for the child session.
    fn child_update(pump: &mut Pump, update: &Value) {
        pump.absorb(Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({"sessionId": "child", "update": update}),
        });
    }

    /// The child-agent activity of one App session, as short tags in
    /// order.
    fn agent_events(app: &mpsc::Receiver<Envelope>, session: SessionId) -> Vec<String> {
        app.try_iter()
            .filter(|envelope| envelope.session == session)
            .filter_map(|envelope| match envelope.event {
                kage_core::protocol::Event::Loop(LoopEvent::ToolCallStart { name, .. }) => {
                    Some(format!("start:{name}"))
                }
                kage_core::protocol::Event::Loop(LoopEvent::ToolCallEnd { output, .. }) => {
                    Some(format!("end:{}", output.text))
                }
                kage_core::protocol::Event::Loop(LoopEvent::TextDelta { delta, .. }) => {
                    Some(format!("text:{delta}"))
                }
                kage_core::protocol::Event::Loop(LoopEvent::MessageEnd { .. }) => {
                    Some("msgend".to_owned())
                }
                kage_core::protocol::Event::Host(HostEvent::SessionChanged { .. }) => {
                    Some("reset".to_owned())
                }
                kage_core::protocol::Event::Host(HostEvent::RunStarted) => {
                    Some("runstart".to_owned())
                }
                kage_core::protocol::Event::Host(HostEvent::RunEnded { outcome }) => {
                    Some(format!("runend:{outcome:?}"))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_announced_subagent_opens_its_own_app_view() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        announced_child(&mut pump, &app, &Value::Null);
        assert!(pump.children.contains_key("child"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_announced_child_maps_its_app_session_to_its_wire_file() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        assert_eq!(
            lock(&pump.child_files).get(&child),
            Some(&"child".to_owned())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adopting_a_session_drops_the_child_file_map() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        announced_child(&mut pump, &app, &Value::Null);
        pump.adopt("s2", false);
        assert!(lock(&pump.child_files).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_subagent_turn_streams_into_its_own_view() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        let _ = app.try_iter().count();
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-child",
                "title": "ls",
                "kind": "read",
                "status": "pending",
                "rawInput": {"path": "/w"}
            }),
        );
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "_kage/turn", "phase": "start"
            }),
        );
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "Listed."}
            }),
        );
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "_kage/turn", "phase": "end", "reason": "no_tool_calls"
            }),
        );
        assert_eq!(
            agent_events(&app, child),
            ["start:ls", "text:Listed.", "msgend"]
        );
        pump.absorb(Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({"sessionId": "s1", "update": {
                "sessionUpdate": "subagent_update", "subagentSessionId": "child", "state": "completed"
            }}),
        });
        assert_eq!(agent_events(&app, child), ["runend:Completed"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tool_calling_child_turn(pump: &mut Pump) {
        for update in [
            serde_json::json!({
                "sessionUpdate": "_kage/turn", "phase": "start"
            }),
            serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-child",
                "title": "ls",
                "kind": "read",
                "status": "pending",
                "rawInput": {"path": "/w"}
            }),
            serde_json::json!({
                "sessionUpdate": "_kage/turn", "phase": "end", "reason": "tool_calls"
            }),
        ] {
            child_update(pump, &update);
        }
    }

    #[test]
    fn a_child_stays_running_between_provider_turns() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &serde_json::json!({"state": "running"}));
        tool_calling_child_turn(&mut pump);
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-child",
                "status": "in_progress"
            }),
        );
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-child",
                "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "main.rs"}}]
            }),
        );
        assert_eq!(
            pump.store.state().session("s1").unwrap().agents["child"].state,
            Some(SubagentState::Running)
        );
        let events = agent_events(&app, child);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            events,
            ["runstart", "start:ls", "end:main.rs"],
            "a tool-calling provider turn ends before tool execution, not the child run; the parent still announces Running"
        );
    }

    #[test]
    fn a_terminal_announcement_reports_the_actual_child_failure() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &serde_json::json!({"state": "running"}));
        tool_calling_child_turn(&mut pump);
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-child",
                "status": "failed",
                "content": [{"type": "content", "content": {"type": "text", "text": "permission denied"}}]
            }),
        );
        pump.absorb(Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "subagent_update",
                    "subagentSessionId": "child",
                    "state": "failed"
                }
            }),
        });
        assert_eq!(
            pump.store.state().session("s1").unwrap().agents["child"].state,
            Some(SubagentState::Failed)
        );
        let outcomes: Vec<_> = app
            .try_iter()
            .filter(|envelope| envelope.session == child)
            .filter_map(|envelope| match envelope.event {
                kage_core::protocol::Event::Host(HostEvent::RunEnded { outcome }) => Some(outcome),
                _ => None,
            })
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            outcomes,
            [RunOutcome::Failed {
                error: kage_core::LoopError::Provider {
                    message: "subagent failed".to_owned(),
                },
            }],
            "the terminal parent announcement, not the earlier provider turn, determines the child outcome"
        );
    }

    #[test]
    fn a_bootstrap_resume_keeps_its_restored_child_mappings() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        pump.bootstrap(Some(Path::new("/sessions/s1.jsonl")));
        let load = RequestId::Number(
            pump.pending_open
                .as_ref()
                .expect("the load is pending")
                .request,
        );
        let child = announced_child(&mut pump, &app, &serde_json::json!({"state": "completed"}));
        assert_eq!(pump.wire, "s1");
        assert_eq!(pump.children["child"].app, child);
        assert_eq!(
            lock(&pump.child_files).get(&child).map(String::as_str),
            Some("child")
        );
        pump.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({"configOptions": model_options("test:model")}),
        });
        let mapping = (
            pump.children.get("child").map(|track| track.app),
            lock(&pump.child_files).get(&child).cloned(),
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            mapping,
            (Some(child), Some("child".to_owned())),
            "session/load announces restored children before its response; adopting that same session must retain their routing and file mappings"
        );
    }

    #[test]
    fn replacing_a_childs_first_streaming_message_resets_before_replay() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "old answer"}
            }),
        );
        assert_eq!(pump.children["child"].committed, 0);
        assert!(pump.children["child"].tail.is_some());
        child_update(
            &mut pump,
            &serde_json::json!({"sessionUpdate": "_kage/session_replaced"}),
        );
        assert_eq!(pump.children["child"].committed, 0);
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "new answer"}
            }),
        );
        let events = agent_events(&app, child);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            events,
            ["runstart", "text:old answer", "reset", "text:new answer"]
        );
    }

    #[test]
    fn terminal_announcements_settle_open_child_turns_once() {
        for (state, outcome) in [
            ("completed", "Completed"),
            ("cancelled", "Cancelled"),
            (
                "failed",
                "Failed { error: Provider { message: \"subagent failed\" } }",
            ),
        ] {
            let (mut pump, app, _dialogs, _feed, dir) = test_pump();
            let child = announced_child(&mut pump, &app, &Value::Null);
            let _ = app.try_iter().count();
            child_update(
                &mut pump,
                &serde_json::json!({
                    "sessionUpdate": "_kage/turn", "phase": "start"
                }),
            );
            child_update(
                &mut pump,
                &serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "partial"}
                }),
            );
            let terminal = Frame::Notification {
                method: "session/update".into(),
                params: serde_json::json!({"sessionId": "s1", "update": {
                    "sessionUpdate": "subagent_update", "subagentSessionId": "child", "state": state
                }}),
            };
            pump.absorb(terminal.clone());
            assert_eq!(
                agent_events(&app, child),
                [
                    "text:partial".to_owned(),
                    "msgend".to_owned(),
                    format!("runend:{outcome}")
                ]
            );
            pump.absorb(terminal);
            assert!(
                agent_events(&app, child).is_empty(),
                "a terminal announcement cannot replay or settle twice"
            );
            assert!(!pump.children["child"].running);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn replacing_a_first_thinking_tail_clears_tracking_before_replay() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        let _ = app.try_iter().count();
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "old reasoning"}
            }),
        );
        let _ = app.try_iter().count();
        child_update(
            &mut pump,
            &serde_json::json!({"sessionUpdate": "_kage/session_replaced"}),
        );
        assert_eq!(agent_events(&app, child), ["reset"]);
        assert!(pump.children["child"].tail.is_none());
        assert_eq!(pump.children["child"].committed, 0);
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "new answer"}
            }),
        );
        assert_eq!(agent_events(&app, child), ["text:new answer"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn child_tool_updates_deliver_arguments_and_replaced_progress() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "tool_call", "toolCallId": "call-child", "title": "read", "status": "pending", "rawInput": {}
            }),
        );
        let _ = app.try_iter().count();
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "call-child", "rawInput": {"path": "file.rs"}
            }),
        );
        for progress in ["step 1", "step 2", "step 2"] {
            child_update(
                &mut pump,
                &serde_json::json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": "call-child", "status": "in_progress",
                    "content": [{"type": "content", "content": {"type": "text", "text": progress}}]
                }),
            );
        }
        let delivered: Vec<String> = app
            .try_iter()
            .filter(|event| event.session == child)
            .filter_map(|envelope| match envelope.event {
                kage_core::protocol::Event::Loop(LoopEvent::ToolCallArgsDelta {
                    input_partial,
                    ..
                }) => Some(format!("args:{input_partial}")),
                kage_core::protocol::Event::Loop(LoopEvent::ToolExecutionStart { .. }) => {
                    Some("executing".to_owned())
                }
                kage_core::protocol::Event::Loop(LoopEvent::ToolUpdate { update, .. }) => {
                    Some(format!("progress:{}", update.content))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            delivered,
            [
                "args:{\"path\":\"file.rs\"}",
                "executing",
                "progress:step 1",
                "progress:step 2"
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn adopting_an_incoming_session_rebuilds_its_nested_child_mappings() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let old = announced_child(&mut pump, &app, &Value::Null);
        pump.open_recorded(Path::new("/sessions/next.jsonl"));
        let load = request_id(&pump.store.take_outgoing(), "session/load");
        let _ = app.try_iter().count();
        for (parent, child) in [("next", "incoming"), ("incoming", "nested")] {
            pump.absorb(Frame::Notification {
                method: "session/update".into(),
                params: serde_json::json!({"sessionId": parent, "update": {
                    "sessionUpdate": "subagent_update", "subagentSessionId": child, "toolCallId": child, "state": "running"
                }}),
            });
        }
        assert!(
            !pump.children.contains_key("incoming"),
            "the outgoing view must not adopt incoming agents early"
        );
        pump.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({"configOptions": []}),
        });
        let incoming = pump.children["incoming"].app;
        let nested = pump.children["nested"].app;
        assert_eq!(pump.children.len(), 2);
        assert_eq!(pump.children["nested"].parent, "incoming");
        let files = lock(&pump.child_files);
        assert!(!files.contains_key(&old));
        assert_eq!(files.get(&incoming).map(String::as_str), Some("incoming"));
        assert_eq!(files.get(&nested).map(String::as_str), Some("nested"));
        drop(files);
        assert!(app.try_iter().any(|envelope| envelope.session == nested && matches!(
            envelope.event, kage_core::protocol::Event::Host(HostEvent::AgentSpawned { parent, .. }) if parent == incoming
        )));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disconnect_settles_nested_children_and_closes_their_streams() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "subagent_update", "subagentSessionId": "nested", "toolCallId": "nested", "state": "running"
            }),
        );
        let nested = pump.children["nested"].app;
        let _ = app.try_iter().count();
        for wire in ["child", "nested"] {
            pump.absorb(Frame::Notification {
                method: "session/update".into(),
                params: serde_json::json!({"sessionId": wire, "update": {
                    "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "partial"}
                }}),
            });
        }
        let _ = app.try_iter().count();
        pump.close();
        let envelopes: Vec<_> = app.try_iter().collect();
        for session in [child, nested] {
            let events: Vec<_> = envelopes
                .iter()
                .filter(|event| event.session == session)
                .map(|event| &event.event)
                .collect();
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        kage_core::protocol::Event::Loop(LoopEvent::MessageEnd { .. }),
                        kage_core::protocol::Event::Host(HostEvent::RunEnded {
                            outcome: RunOutcome::Cancelled
                        })
                    ]
                ),
                "each live child closes its stream before it can be evicted: {events:?}"
            );
        }
        pump.close();
        assert!(app.try_iter().next().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refused_child_prompt_returns_the_original_draft() {
        for (state, queue) in [("running", true), ("completed", false)] {
            let (mut pump, app, _dialogs, _feed, dir) = test_pump();
            let child = announced_child(&mut pump, &app, &serde_json::json!({"state": state}));
            let _ = app.try_iter().count();
            pump.handle(RunRequest::Submit {
                text: "keep this draft".to_owned(),
                images: Vec::new(),
                queue,
                session: Some(child),
            });
            assert!(!has_request(&pump.store.take_outgoing(), "session/prompt"));
            let returned = app.try_iter().find_map(|envelope| match envelope.event {
                kage_core::protocol::Event::Host(HostEvent::PromptWithdrawn {
                    content, ..
                }) if envelope.session == child => content,
                _ => None,
            });
            assert_eq!(
                returned,
                Some(vec![kage_core::Content::Text {
                    text: "keep this draft".to_owned()
                }])
            );
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn child_steering_echoes_on_the_child_and_returns_a_rejected_prompt() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        advertise_steering(&mut pump);
        let child = announced_child(&mut pump, &app, &Value::Null);
        let _ = app.try_iter().count();
        pump.handle(RunRequest::Submit {
            text: "steer this child".to_owned(),
            images: Vec::new(),
            queue: false,
            session: Some(child),
        });
        let sent = pump.store.take_outgoing();
        let prompt = request_id(&sent, "session/prompt");
        for frame in &sent {
            pump.observe_outgoing(frame);
        }
        let echoes: Vec<_> = app
            .try_iter()
            .filter(|envelope| {
                matches!(
                    envelope.event,
                    kage_core::protocol::Event::Loop(LoopEvent::MessageAppended { .. })
                )
            })
            .collect();
        assert_eq!(echoes.len(), 1);
        assert_eq!(echoes[0].session, child);
        pump.absorb(Frame::Failure {
            id: prompt,
            error: RpcError::new(-32603, "child busy"),
        });
        let returned =
            app.try_iter().find_map(|envelope| match envelope.event {
                kage_core::protocol::Event::Host(HostEvent::PromptWithdrawn {
                    content, ..
                }) if envelope.session == child => content,
                _ => None,
            });
        assert_eq!(
            returned,
            Some(vec![kage_core::Content::Text {
                text: "steer this child".to_owned()
            }])
        );
        assert!(
            pump.children["child"].running,
            "a rejected steer must not settle the independent child run"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bootstrap_adoption_preserves_an_outstanding_child_approval() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        pump.bootstrap(Some(Path::new("/sessions/s1.jsonl")));
        let load = RequestId::Number(pump.pending_open.as_ref().unwrap().request);
        let child = announced_child(&mut pump, &app, &Value::Null);
        pump.absorb(Frame::Request {
            id: RequestId::Number(101),
            method: "session/request_permission".into(),
            params: serde_json::json!({
                "sessionId": "child",
                "toolCall": {"toolCallId": "child-call", "title": "ls", "status": "pending", "rawInput": {}},
                "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]
            }),
        });
        let ask = app
            .try_iter()
            .find_map(|event| match event.event {
                kage_core::protocol::Event::Host(HostEvent::PermissionRequested {
                    request_id,
                    ..
                }) if event.session == child => Some(request_id),
                _ => None,
            })
            .expect("the resumed child asks before the load response");
        pump.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({"configOptions": []}),
        });
        assert!(pump.ask_ids.contains_key(&ask.0));
        assert!(!app.try_iter().any(|event| matches!(event.event,
            kage_core::protocol::Event::Host(HostEvent::PermissionResolved { request_id }) if request_id == ask
        )));
        pump.resolve_permission(ask, kage_tui::PermissionDecision::AllowOnce);
        assert!(pump.store.take_outgoing().iter().any(|frame| matches!(
            frame,
            Frame::Success {
                id: RequestId::Number(101),
                ..
            }
        )));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_submission_racing_with_disconnect_returns_text_and_attachments() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        pump.close();
        let _ = app.try_iter().count();
        pump.handle(RunRequest::Submit {
            text: "not delivered".into(),
            images: vec![kage_tui::image::AttachedImage {
                source: MediaSource::Url {
                    url: "https://example.test/image.png".into(),
                },
                mime: "image/png".into(),
                label: "image".into(),
                bytes: 0,
            }],
            queue: false,
            session: Some(child),
        });
        assert!(!has_request(&pump.store.take_outgoing(), "session/prompt"));
        let content = app
            .try_iter()
            .find_map(|event| match event.event {
                kage_core::protocol::Event::Host(HostEvent::PromptWithdrawn {
                    content, ..
                }) if event.session == child => content,
                _ => None,
            })
            .expect("a disconnected submission returns to its draft");
        assert_eq!(
            content,
            vec![
                kage_core::Content::Text {
                    text: "not delivered".into()
                },
                kage_core::Content::Image {
                    source: MediaSource::Url {
                        url: "https://example.test/image.png".into()
                    },
                    mime: "image/png".into()
                },
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recalling_a_child_prompt_does_not_withdraw_from_the_main_session() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        pump.handle(RunRequest::RecallPrompt {
            session: Some(child),
            delivery: Delivery::Steer,
        });
        assert!(pump.store.take_outgoing().is_empty());
        assert!(
            notices(&app)
                .iter()
                .any(|(_, message)| message.contains("cannot be recalled"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unconsumed_child_steer_returns_once_even_if_the_run_failed() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        advertise_steering(&mut pump);
        let child = announced_child(&mut pump, &app, &Value::Null);
        pump.handle(RunRequest::Submit {
            text: "unconsumed".into(),
            images: Vec::new(),
            queue: false,
            session: Some(child),
        });
        let outgoing = pump.store.take_outgoing();
        let prompt = request_id(&outgoing, "session/prompt");
        for frame in &outgoing {
            pump.observe_outgoing(frame);
        }
        let _ = app.try_iter().count();
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "_kage/prompt_withdrawn", "delivery": "steer",
                "content": [{"type": "text", "text": "unconsumed"}]
            }),
        );
        pump.absorb(Frame::Failure {
            id: prompt,
            error: RpcError::new(-32603, "provider failed"),
        });
        let returned: Vec<_> = app
            .try_iter()
            .filter_map(|event| match event.event {
                kage_core::protocol::Event::Host(HostEvent::PromptWithdrawn {
                    content, ..
                }) if event.session == child => content,
                _ => None,
            })
            .collect();
        assert_eq!(
            returned,
            vec![vec![kage_core::Content::Text {
                text: "unconsumed".into()
            }]]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_subagent_ask_answers_on_its_own_session() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        let _ = app.try_iter().count();
        pump.absorb(Frame::Request {
            id: RequestId::Number(101),
            method: "session/request_permission".into(),
            params: serde_json::json!({
                "sessionId": "child",
                "toolCall": {
                    "toolCallId": "call-child",
                    "title": "ls",
                    "kind": "read",
                    "status": "pending",
                    "rawInput": {"path": "/w"}
                },
                "options": [
                    {"optionId": "allow", "name": "Allow ls", "kind": "allow_once"},
                    {"optionId": "allow_session", "name": "Allow ls for this session", "kind": "allow_always"},
                    {"optionId": "reject", "name": "Reject ls", "kind": "reject_once"}
                ]
            }),
        });
        let asked = app
            .try_iter()
            .filter(|envelope| envelope.session == child)
            .find_map(|envelope| match envelope.event {
                kage_core::protocol::Event::Host(HostEvent::PermissionRequested {
                    request_id,
                    tool,
                    ..
                }) => Some((request_id, tool)),
                _ => None,
            })
            .expect("the child's ask forwards");
        assert_eq!(asked.1, "ls");
        pump.resolve_permission(asked.0, kage_tui::PermissionDecision::AllowOnce);
        let sent = pump.store.take_outgoing();
        assert!(
            sent.iter().any(
                |frame| matches!(frame, Frame::Success { id, .. } if *id == RequestId::Number(101))
            ),
            "the answer rides the child's request: {sent:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_prompt_addressed_to_an_agent_reaches_its_session() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        advertise_steering(&mut pump);
        let child = announced_child(&mut pump, &app, &Value::Null);
        let _ = app.try_iter().count();
        pump.handle(RunRequest::Submit {
            text: "keep going".to_owned(),
            images: Vec::new(),
            queue: false,
            session: Some(child),
        });
        let sent = pump.store.take_outgoing();
        let to_child = sent.iter().any(|frame| match frame {
            Frame::Request { method, params, .. } if method == "session/prompt" => {
                params.get("sessionId").and_then(Value::as_str) == Some("child")
                    && params["delivery"] == "steer"
            }
            _ => false,
        });
        assert!(to_child, "the prompt rides the child session: {sent:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancel_addressed_to_an_agent_cancels_its_session() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        let _ = app.try_iter().count();
        pump.handle(RunRequest::Cancel {
            session: Some(child),
        });
        let sent = pump.store.take_outgoing();
        let to_child = sent.iter().any(|frame| match frame {
            Frame::Notification { method, params } if method == "session/cancel" => {
                params.get("sessionId").and_then(Value::as_str) == Some("child")
            }
            _ => false,
        });
        let to_main = sent.iter().any(|frame| match frame {
            Frame::Notification { method, params } if method == "session/cancel" => {
                params.get("sessionId").and_then(Value::as_str) == Some("s1")
            }
            _ => false,
        });
        assert!(to_child, "the cancel rides the child session: {sent:?}");
        assert!(!to_main, "the main run keeps going: {sent:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_agent_announced_finished_replays_and_settles() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        // The child's whole turn arrives before its announcement, as
        // on a reconnect.
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-child",
                "title": "ls",
                "kind": "read",
                "status": "completed",
                "rawInput": {"path": "/w"},
                "content": [{"type": "content", "content": {"type": "text", "text": "main.rs\nlib.rs"}}]
            }),
        );
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "Listed both."}
            }),
        );
        let _ = app.try_iter().count();
        let child = announced_child(&mut pump, &app, &serde_json::json!({"state": "completed"}));
        assert_eq!(
            agent_events(&app, child),
            [
                "runstart",
                "start:ls",
                "end:main.rs\nlib.rs",
                "text:Listed both.",
                "msgend",
                "runend:Completed",
            ],
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bootstrap_advertises_subagents() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        // A pipe whose read half this test holds: the frames bootstrap
        // flushes land here instead of the leaked ends.
        let (to_agent, mut from_pump) = crate::tui::pipe::channel_pair();
        let (_, from_agent) = crate::tui::pipe::channel_pair();
        pump.transport = Transport::stream(
            BufReader::new(from_agent),
            to_agent,
            None,
            mpsc::channel().0,
        )
        .unwrap();
        pump.bootstrap(None);
        let mut lines = Vec::new();
        for _ in 0..3 {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while !line.ends_with(b"\n") {
                if from_pump.read(&mut byte).unwrap_or(0) == 0 {
                    break;
                }
                line.push(byte[0]);
            }
            lines.push(String::from_utf8_lossy(&line).to_string());
        }
        let initialize = lines
            .iter()
            .find_map(|line| {
                serde_json::from_str::<Value>(line.trim())
                    .ok()
                    .filter(|frame| frame["method"] == "initialize")
            })
            .expect("bootstrap initializes");
        assert_eq!(
            initialize["params"]["clientCapabilities"]["subagents"],
            serde_json::json!({})
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_replaced_child_transcript_replays_from_empty() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        let _ = app.try_iter().count();
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-child",
                "title": "ls",
                "kind": "read",
                "status": "pending",
                "rawInput": {"path": "/w"}
            }),
        );
        // An undo or rewind replaces the child's conversation: the
        // tracking must reset instead of slicing past the transcript.
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "_kage/session_replaced"
            }),
        );
        child_update(
            &mut pump,
            &serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-again",
                "title": "rm",
                "kind": "edit",
                "status": "completed",
                "rawInput": {"path": "/w"},
                "content": [{"type": "content", "content": {"type": "text", "text": "done"}}]
            }),
        );
        assert_eq!(
            agent_events(&app, child),
            ["start:ls", "reset", "start:rm", "end:done"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_session_s_ask_moving_leaves_other_cards_alone() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        let _ = app.try_iter().count();
        let ask = |session: &str, id: u64| Frame::Request {
            id: RequestId::Number(id),
            method: "session/request_permission".into(),
            params: serde_json::json!({
                "sessionId": session,
                "toolCall": {
                    "toolCallId": "call",
                    "title": "ls",
                    "kind": "read",
                    "status": "pending",
                    "rawInput": {"path": "/w"}
                },
                "options": [
                    {"optionId": "allow", "name": "Allow ls", "kind": "allow_once"},
                    {"optionId": "reject", "name": "Reject ls", "kind": "reject_once"}
                ]
            }),
        };
        pump.absorb(ask("s1", 201));
        pump.absorb(ask("child", 101));
        let _ = app.try_iter().count();
        // The agent withdraws the child's ask: only its card resolves.
        pump.absorb(Frame::Notification {
            method: "$/cancel_request".into(),
            params: serde_json::json!({"requestId": 101}),
        });
        let resolved: Vec<(SessionId, u64)> = app
            .try_iter()
            .filter_map(|envelope| match envelope.event {
                kage_core::protocol::Event::Host(HostEvent::PermissionResolved { request_id }) => {
                    Some((envelope.session, request_id.0))
                }
                _ => None,
            })
            .collect();
        assert_eq!(resolved.len(), 1, "one card resolves: {resolved:?}");
        assert_eq!(resolved[0].0, child, "the child's card resolves");
        assert_ne!(resolved[0].0, pump.app_session);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_request_for_a_gone_agent_is_refused_not_aimed_at_the_main() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let child = announced_child(&mut pump, &app, &Value::Null);
        let _ = app.try_iter().count();
        pump.children.clear();
        pump.handle(RunRequest::Cancel {
            session: Some(child),
        });
        let sent = pump.store.take_outgoing();
        assert!(
            !sent.iter().any(|frame| matches!(
                frame,
                Frame::Notification { method, .. } if method == "session/cancel"
            )),
            "the main run keeps going: {sent:?}"
        );
        pump.handle(RunRequest::Submit {
            text: "hi".to_owned(),
            images: Vec::new(),
            queue: false,
            session: Some(child),
        });
        let sent = pump.store.take_outgoing();
        assert!(
            !sent.iter().any(|frame| matches!(
                frame,
                Frame::Request { method, .. } if method == "session/prompt"
            )),
            "the main agent is not prompted: {sent:?}"
        );
        assert!(
            notices(&app)
                .iter()
                .any(|(level, _)| *level == NoticeLevel::Warning),
            "the gone agent is named"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pump whose run on `s1` is in flight, with the recorder's
    /// queue to read.
    fn running_pump() -> (
        Pump,
        mpsc::Receiver<Envelope>,
        mpsc::Receiver<(String, RunOutcome)>,
        PathBuf,
    ) {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        let (recorder, finished_runs) = mpsc::channel();
        pump.recorder = recorder;
        pump.handle(RunRequest::Submit {
            text: "go".to_owned(),
            images: Vec::new(),
            queue: false,
            session: None,
        });
        pump.flush();
        let _ = app.try_iter().count();
        (pump, app, finished_runs, dir)
    }

    fn run_ended(app: &mpsc::Receiver<Envelope>) -> Vec<RunOutcome> {
        app.try_iter()
            .filter_map(|envelope| match envelope.event {
                kage_core::protocol::Event::Host(HostEvent::RunEnded { outcome }) => Some(outcome),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_run_ends_as_the_stop_reason_says() {
        for (reason, expected) in [
            ("end_turn", RunOutcome::Completed),
            ("cancelled", RunOutcome::Cancelled),
        ] {
            let (mut pump, app, recorded, dir) = running_pump();
            // The prompt is the only request the pump has sent.
            let prompt = pump
                .prompts
                .keys()
                .next()
                .cloned()
                .expect("a prompt is out");
            pump.absorb(Frame::Success {
                id: prompt,
                result: serde_json::json!({"stopReason": reason}),
            });
            assert_eq!(run_ended(&app), std::slice::from_ref(&expected), "{reason}");
            assert_eq!(
                recorded.try_recv().unwrap(),
                ("test:model".to_owned(), expected)
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_failed_prompt_ends_the_run_as_failed() {
        let (mut pump, app, recorded, dir) = running_pump();
        let prompt = pump
            .prompts
            .keys()
            .next()
            .cloned()
            .expect("a prompt is out");
        pump.absorb(Frame::Failure {
            id: prompt,
            error: RpcError::new(-32000, "provider down".to_owned()),
        });
        let ended = run_ended(&app);
        assert!(
            matches!(
                ended.as_slice(),
                [RunOutcome::Failed {
                    error: kage_core::LoopError::Provider { message }
                }] if message == "provider down"
            ),
            "{ended:?}"
        );
        assert!(matches!(
            recorded.try_recv().unwrap().1,
            RunOutcome::Failed { .. }
        ));
        assert!(!pump.running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_closed_pump_waits_for_the_app_instead_of_spinning() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        // The frames sender is gone, so every read of the channel
        // fails at once, as after the reader thread ends.
        let (frames_tx, frames_rx) = mpsc::channel();
        drop(frames_tx);
        pump.frames = frames_rx;
        pump.close();
        let (requests, requests_rx) = mpsc::channel::<RunRequest>();
        pump.requests = requests_rx;
        let start = std::time::Instant::now();
        for _ in 0..5 {
            assert!(pump.pass());
        }
        assert!(
            start.elapsed() >= TICK * 4,
            "five passes took {:?}: the pump spins on the dead connection",
            start.elapsed()
        );
        drop(requests);
        assert!(!pump.pass(), "a gone App ends the pump");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bootstrap_resume_does_not_close_the_session_it_adopts() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        pump.bootstrap(Some(Path::new("/sessions/resumed.jsonl")));
        let _ = std::fs::remove_dir_all(&dir);
        // bootstrap flushed its frames to the transport, so the load's
        // request id is read from the pump's pending open.
        let load = RequestId::Number(
            pump.pending_open
                .as_ref()
                .expect("the load is pending")
                .request,
        );
        pump.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({"configOptions": model_options("test:model")}),
        });
        assert_eq!(pump.wire, "resumed");
        let after = pump.store.take_outgoing();
        assert!(
            !after.iter().any(|frame| {
                frame.to_value()["method"] == "session/close"
                    && frame.to_value()["params"]["sessionId"] == "resumed"
            }),
            "the resume was closed on adoption: {after:?}"
        );

        // A failed startup resume falls back to a new session and
        // closes neither the phantom nor anything else.
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        pump.bootstrap(Some(Path::new("/sessions/resumed.jsonl")));
        // bootstrap flushed its frames to the transport, so the load's
        // request id is read from the pump's pending open.
        let load = RequestId::Number(
            pump.pending_open
                .as_ref()
                .expect("the load is pending")
                .request,
        );
        pump.absorb(Frame::Failure {
            id: load,
            error: kage_client::RpcError::new(-32603, "the session file is gone".to_owned()),
        });
        let after = pump.store.take_outgoing();
        assert!(
            !after
                .iter()
                .any(|frame| frame.to_value()["method"] == "session/close"),
            "the phantom resume was closed: {after:?}"
        );
        assert_eq!(pump.wire, "", "the phantom wire is dropped");
        assert!(
            after
                .iter()
                .any(|frame| frame.to_value()["method"] == "session/new"),
            "the fallback asks for a new session: {after:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_resumed_session_rides_the_same_meta_a_new_one_does() {
        let (mut pump, _app, dir) = hosted_pump();
        pump.yolo = true;
        pump.open_recorded(Path::new("/sessions/resumed.jsonl"));
        let frames = pump.store.take_outgoing();
        let load = frames
            .iter()
            .find(|frame| frame.to_value()["method"] == "session/load")
            .expect("the load went out");
        let params = &load.to_value()["params"];
        let kage = &params["_meta"]["kage"];
        assert_eq!(
            kage["plugins"], "client",
            "a hosted pump hosts the resume too"
        );
        assert_eq!(kage["yolo"], pump.yolo);
        assert_eq!(params["sessionId"], "resumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_config_override_selects_a_private_tui_host() {
        const CHILD: &str = "TEST_KAGE_PRIVATE_UPSTREAM";
        if std::env::var_os(CHILD).is_some() {
            assert!(matches!(
                choose_upstream(false, Path::new(".")),
                Upstream::Private
            ));
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tui::acp::tests::a_config_override_selects_a_private_tui_host",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env("XDG_RUNTIME_DIR", dir.path().join("runtime"))
            .env("KAGE_PERMISSIONS__TOOLS__READ__DEFAULT", "deny")
            .env("KAGE_HOST_IDLE_SECS", "1")
            .env(CHILD, "1")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.stderr.is_empty(),
            "the override must select private hosting before trying to connect: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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
        let registry = ProviderRegistry::default();
        let meta = session_meta(&options, &registry, "test:model", "", false, false);
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
        let meta = session_meta(&clean, &registry, "test:model", "be brief", true, false);
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
        pump.request_new_session();
        let new_id = request_id(&pump.store.take_outgoing(), "session/new");
        pump.absorb(Frame::Success {
            id: new_id,
            result: serde_json::json!({"sessionId": "agent-1"}),
        });
        assert_eq!(pump.wire, "agent-1");
        assert_eq!(pump.pending_new, None);
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

    #[test]
    fn the_mirror_names_the_wire_sessions_file() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        pump.wire = "agent-1".to_owned();
        pump.point_mirror(Ok(dir.clone()));
        assert_eq!(
            lock(pump.mirror.as_ref()).path(),
            Some(dir.join("agent-1.jsonl").as_path()),
            "the exit summary and the tree read the adopted session's file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unresolvable_sessions_dir_clears_the_mirror() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        pump.point_mirror(Ok(dir.clone()));
        assert!(lock(pump.mirror.as_ref()).path().is_some());
        pump.point_mirror(Err("no home directory".to_owned()));
        assert_eq!(
            lock(pump.mirror.as_ref()).path(),
            None,
            "a stale path would send the readers to another session's file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adopting_a_session_points_the_mirror_at_it() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        resume_as(&mut pump, "abc123", "test:model");
        // Whatever the environment resolves, the mirror ends on the
        // adopted session's file name, or on nothing when the
        // sessions directory cannot be resolved.
        let path = lock(pump.mirror.as_ref()).path().map(Path::to_path_buf);
        match crate::paths::sessions_dir() {
            Ok(sessions) => assert_eq!(path, Some(sessions.join("abc123.jsonl"))),
            Err(_) => assert_eq!(path, None),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Lua fixture the hosted-pump tests load: an echo tool and
    /// an override of it, a failing tool, and notification, transform
    /// and predicate handlers.
    const FIXTURE: &str = r"
kage.request_capabilities({ 'exec', 'session_write' })
kage.register_tool({
    name = 'plugin_echo',
    description = 'echoes input.msg',
    schema = { type = 'object' },
    risk = 'read',
    execute = function(input) return 'echo:' .. (input.msg or '?') end,
})
kage.override_tool({
    name = 'plugin_echo',
    description = 'shouts instead',
    schema = { type = 'object' },
    risk = 'read',
    execute = function(input) return 'override:' .. (input.msg or '?') end,
})
kage.register_tool({
    name = 'plugin_fail',
    description = 'fails the call',
    schema = { type = 'object' },
    execute = function(input) error('no can do') end,
})
kage.register_tool({
    name = 'plugin_wait',
    description = 'waits on a process',
    schema = { type = 'object' },
    execute = function(input) kage.exec({ cmd = 'sleep', args = { '30' } }) return 'late' end,
})
kage.register_tool({
    name = 'plugin_queue',
    description = 'queues a message and a session entry',
    schema = { type = 'object' },
    execute = function(input)
        kage.send_message('queued text', { trigger_turn = true })
        kage.session.append_entry('plugin:note', { n = 1 })
        return 'queued'
    end,
})
kage.register_command({
    name = 'hello',
    description = 'greets',
    handler = function(args, ctx) return 'hello ' .. args end,
})
kage.api.keymap_set('n', 'zz', function() return 'mapped' end)
kage.on('message_end', function(ev) count = (count or 0) + 1 end)
kage.on('transform_context', function(messages) return { replaced = true } end)
kage.on('should_stop_after_turn', function(ev) return true end)
";

    /// A pump hosting a runtime with [`FIXTURE`] loaded from its
    /// workdir, standing in for what `run()` builds. The context
    /// capability is granted in config, the way
    /// `[plugins.capabilities]` does.
    fn hosted_pump() -> (Pump, mpsc::Receiver<Envelope>, PathBuf) {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        std::fs::write(dir.join("fixture.lua"), FIXTURE).unwrap();
        let runtime = PluginRuntime::builder()
            .workdir(dir.clone())
            .config(serde_json::json!({"model": "mock/m"}))
            .capabilities(std::collections::BTreeMap::from([(
                "fixture".to_owned(),
                vec![
                    "context".to_owned(),
                    "exec".to_owned(),
                    "session_write".to_owned(),
                ],
            )]))
            .build()
            .unwrap();
        kage_plugin::load_all(Some(&dir), &runtime).unwrap();
        pump.host_plugins(PluginSurface::new(Arc::new(runtime), dir.clone()))
            .unwrap();
        (pump, app, dir)
    }

    /// A session id the wire shape demands: a real ULID, since the
    /// surface scopes dispatches by it.
    fn fixture_session() -> String {
        ulid::Ulid::generate().to_string()
    }

    /// Feed one request in and return the one reply the surface
    /// queued, waiting for the worker's answer when the pump hosts.
    fn plugin_reply(pump: &mut Pump, id: u64, method: &str, params: Value) -> Frame {
        pump.absorb(Frame::Request {
            id: RequestId::Number(id),
            method: method.to_owned(),
            params,
        });
        if pump.plugin_jobs.is_some() {
            let answer = pump
                .plugin_answers
                .recv_timeout(Duration::from_secs(30))
                .expect("the worker answers");
            pump.send_plugin_answer(answer);
        }
        let mut outgoing = pump.store.take_outgoing();
        assert_eq!(outgoing.len(), 1, "exactly one reply, got {outgoing:?}");
        outgoing.remove(0)
    }

    #[test]
    fn a_hosted_session_serves_the_plugin_tools() {
        let (mut pump, _app, dir) = hosted_pump();
        let reply = plugin_reply(
            &mut pump,
            7,
            "_kage/plugin/tools",
            serde_json::json!({"sessionId": "s1"}),
        );
        let Frame::Success { id, result } = reply else {
            panic!("the tools answer succeeds, got {reply:?}")
        };
        assert_eq!(id, RequestId::Number(7));
        assert_eq!(result["tools"][0]["name"], "plugin_echo");
        assert_eq!(result["tools"][0]["risk"], "read");
        assert_eq!(result["handlers"]["message_end"], 1);
        assert_eq!(result["handlers"]["transform_context"], 1);
        assert_eq!(result["handlers"]["should_stop_after_turn"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hosted_session_runs_a_plugin_tool_call() {
        let (mut pump, _app, dir) = hosted_pump();
        // The override wins the name, like the engine's registry.
        let reply = plugin_reply(
            &mut pump,
            8,
            "_kage/plugin/call",
            serde_json::json!({
                "sessionId": fixture_session(),
                "tool": "plugin_echo",
                "input": {"msg": "ping"},
            }),
        );
        let Frame::Success { result, .. } = reply else {
            panic!("the call answers, got {reply:?}")
        };
        assert_eq!(result["text"], "override:ping");
        assert_eq!(result["isError"], false);
        // A tool that raises answers is_error on a success frame, the
        // way a Lua tool's failure is an output.
        let reply = plugin_reply(
            &mut pump,
            9,
            "_kage/plugin/call",
            serde_json::json!({"sessionId": fixture_session(), "tool": "plugin_fail", "input": {}}),
        );
        let Frame::Success { result, .. } = reply else {
            panic!("the failure is an output, got {reply:?}")
        };
        assert_eq!(result["isError"], true);
        assert!(
            result["text"]
                .as_str()
                .is_some_and(|text| text.contains("no can do")),
            "{result}"
        );
        // A tool the runtime does not hold fails the request.
        let reply = plugin_reply(
            &mut pump,
            10,
            "_kage/plugin/call",
            serde_json::json!({"sessionId": fixture_session(), "tool": "nope", "input": {}}),
        );
        assert!(matches!(reply, Frame::Failure { .. }), "{reply:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hosted_session_serves_the_hooks() {
        let (mut pump, _app, dir) = hosted_pump();
        let reply = plugin_reply(
            &mut pump,
            10,
            "_kage/plugin/hook",
            serde_json::json!({
                "sessionId": fixture_session(),
                "event": "transform_context",
                "kind": "transform",
                "payload": [],
            }),
        );
        let Frame::Success { result, .. } = reply else {
            panic!("the transform answers, got {reply:?}")
        };
        assert_eq!(result["replaced"], true);
        let reply = plugin_reply(
            &mut pump,
            11,
            "_kage/plugin/hook",
            serde_json::json!({
                "sessionId": fixture_session(),
                "event": "should_stop_after_turn",
                "kind": "predicate",
                "payload": {},
            }),
        );
        let Frame::Success { result, .. } = reply else {
            panic!("the predicate answers, got {reply:?}")
        };
        assert_eq!(result, Value::Bool(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hosted_session_serves_events_and_drains() {
        let (mut pump, _app, dir) = hosted_pump();
        let reply = plugin_reply(
            &mut pump,
            12,
            "_kage/plugin/event",
            serde_json::json!({
                "sessionId": fixture_session(),
                "event": "message_end",
                "payload": {"usage": {"output": 3}},
                "notify": false,
            }),
        );
        assert!(
            matches!(&reply, Frame::Success { result, .. } if result.is_null()),
            "{reply:?}"
        );
        // Nothing queued: the drain comes back empty.
        let reply = plugin_reply(
            &mut pump,
            13,
            "_kage/plugin/drain",
            serde_json::json!({"sessionId": fixture_session(), "queue": "mcp_restarts"}),
        );
        let Frame::Success { result, .. } = reply else {
            panic!("the drain answers, got {reply:?}")
        };
        assert_eq!(result, serde_json::json!({}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pump_without_plugins_leaves_plugin_requests_to_the_store() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        let reply = plugin_reply(
            &mut pump,
            14,
            "_kage/plugin/tools",
            serde_json::json!({"sessionId": "s1"}),
        );
        let Frame::Failure { error, .. } = reply else {
            panic!("the store refuses what it cannot serve, got {reply:?}")
        };
        assert_eq!(error.code, -32601);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hosting_pump_names_itself_the_plugin_host() {
        let (pump, _app, dir) = hosted_pump();
        let meta = pump.session_meta();
        assert_eq!(
            meta.kage.expect("the settings are set").plugins,
            Some(PluginsSource::Client)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_running_plugin_tool_leaves_the_pump_free_and_esc_cancels_it() {
        let (mut pump, _app, dir) = hosted_pump();
        pump.absorb(Frame::Request {
            id: RequestId::Number(30),
            method: "_kage/plugin/call".to_owned(),
            params: serde_json::json!({
                "sessionId": fixture_session(),
                "tool": "plugin_wait",
                "input": {},
            }),
        });
        // The tool waits on a 30 second process, yet the pump is back
        // here with no answer sent.
        assert!(pump.store.take_outgoing().is_empty());
        assert!(pump.plugin_answers.try_recv().is_err());
        let (_requests, requests_rx) = mpsc::channel::<RunRequest>();
        pump.requests = requests_rx;
        assert!(pump.pass(), "the pump keeps serving while the tool runs");
        let start = std::time::Instant::now();
        pump.handle(RunRequest::Cancel { session: None });
        let (id, answer) = pump
            .plugin_answers
            .recv_timeout(Duration::from_secs(20))
            .expect("the cancel ends the tool call");
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "the tool ran on after the cancel: {:?}",
            start.elapsed()
        );
        assert_eq!(id, RequestId::Number(30));
        let error = answer.expect_err("a cancelled tool fails the request");
        assert!(error.message.contains("plugin_wait"), "{}", error.message);
        let sent = pump.store.take_outgoing();
        assert!(has_notification(&sent, "session/cancel"), "{sent:?}");
        // The next call is not poisoned by the cancel before it.
        let reply = plugin_reply(
            &mut pump,
            31,
            "_kage/plugin/call",
            serde_json::json!({
                "sessionId": fixture_session(),
                "tool": "plugin_echo",
                "input": {"msg": "again"},
            }),
        );
        let Frame::Success { result, .. } = reply else {
            panic!("the next call runs, got {reply:?}")
        };
        assert_eq!(result["text"], "override:again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_connection_cancels_the_plugin_calls() {
        let (mut pump, _app, dir) = hosted_pump();
        pump.close();
        let reply = plugin_reply(
            &mut pump,
            32,
            "_kage/plugin/call",
            serde_json::json!({
                "sessionId": fixture_session(),
                "tool": "plugin_wait",
                "input": {},
            }),
        );
        assert!(
            matches!(reply, Frame::Failure { .. }),
            "a call after the close does not start: {reply:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drain_hands_over_the_queued_messages_and_session_ops() {
        let (mut pump, _app, dir) = hosted_pump();
        let session = fixture_session();
        let queue = |pump: &mut Pump, id: u64| {
            let reply = plugin_reply(
                pump,
                id,
                "_kage/plugin/call",
                serde_json::json!({"sessionId": session, "tool": "plugin_queue", "input": {}}),
            );
            assert!(matches!(reply, Frame::Success { .. }), "{reply:?}");
        };
        let drain = |pump: &mut Pump, id: u64, queue: &str, session: Option<&str>| {
            let reply = plugin_reply(
                pump,
                id,
                "_kage/plugin/drain",
                serde_json::json!({"sessionId": session, "queue": queue}),
            );
            let Frame::Success { result, .. } = reply else {
                panic!("the drain answers, got {reply:?}")
            };
            result
        };
        queue(&mut pump, 40);
        let messages = drain(&mut pump, 41, "messages", Some(&session));
        assert_eq!(messages["messages"][0]["text"], "queued text");
        assert_eq!(messages["messages"][0]["triggerTurn"], true);
        assert_eq!(messages["messages"][0]["deliverAs"], "user");
        let again = drain(&mut pump, 42, "messages", Some(&session));
        assert_eq!(again, serde_json::json!({}), "a drain empties the queue");
        let ops = drain(&mut pump, 43, "session_ops", None);
        assert_eq!(ops["ops"][0]["kind"], "plugin:note", "{ops}");
        queue(&mut pump, 44);
        let ops = drain(&mut pump, 45, "session_ops_for", Some(&session));
        assert_eq!(ops["ops"].as_array().map(Vec::len), Some(1), "{ops}");
        assert_eq!(ops["ops"][0]["kind"], "plugin:note", "{ops}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_id_that_is_not_a_ulid_is_refused() {
        let (mut pump, _app, dir) = hosted_pump();
        for (id, method, params) in [
            (
                50,
                "_kage/plugin/call",
                serde_json::json!({"sessionId": "s1", "tool": "plugin_echo", "input": {}}),
            ),
            (
                51,
                "_kage/plugin/hook",
                serde_json::json!({
                    "sessionId": "s1", "event": "transform_context",
                    "kind": "transform", "payload": [],
                }),
            ),
            (
                52,
                "_kage/plugin/drain",
                serde_json::json!({"sessionId": "s1", "queue": "session_ops_for"}),
            ),
            (
                53,
                "_kage/plugin/drain",
                serde_json::json!({"queue": "session_ops_for"}),
            ),
        ] {
            let reply = plugin_reply(&mut pump, id, method, params);
            let Frame::Failure { error, .. } = reply else {
                panic!("{method} takes a ULID session, got {reply:?}")
            };
            assert_eq!(error.code, -32602, "{method}: {}", error.message);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hosted_pump_runs_plugin_commands_and_keymaps_itself() {
        let (mut pump, app, dir) = hosted_pump();
        let _ = app.try_iter().count();
        pump.handle(RunRequest::InvokePluginCommand {
            name: "hello".to_owned(),
            args: "world".to_owned(),
        });
        assert_eq!(
            notices(&app),
            [(NoticeLevel::Info, "hello world".to_owned())]
        );
        pump.handle(RunRequest::InvokePluginCommand {
            name: "missing".to_owned(),
            args: String::new(),
        });
        assert_eq!(
            notices(&app),
            [(NoticeLevel::Error, "no plugin command: missing".to_owned())]
        );
        let handler = {
            let surface = pump.plugins.as_ref().expect("hosted");
            let keymap = surface.runtime.keymap();
            lock(&keymap)
                .to_data()
                .into_iter()
                .find_map(|mapping| match mapping.rhs {
                    kage_core::keymap::RhsData::Lua { id } if mapping.lhs == "zz" => Some(id),
                    _ => None,
                })
                .expect("the fixture maps zz")
        };
        pump.handle(RunRequest::InvokeKeymap { id: handler });
        assert_eq!(notices(&app), [(NoticeLevel::Info, "mapped".to_owned())]);
        assert!(
            pump.store.take_outgoing().is_empty(),
            "a hosted pump never asks the agent to run them"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pump_without_plugins_asks_the_agent_to_run_commands_and_keymaps() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        pump.handle(RunRequest::InvokePluginCommand {
            name: "hello".to_owned(),
            args: "world".to_owned(),
        });
        pump.handle(RunRequest::InvokeKeymap { id: 3 });
        let sent = pump.store.take_outgoing();
        assert!(has_request(&sent, "_kage/ui/command"), "{sent:?}");
        assert!(has_request(&sent, "_kage/ui/keymap"), "{sent:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn mcp_server(json: Value) -> kage_core::config::McpServer {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn plugin_mcp_servers_map_to_the_wire_and_warn_of_what_it_drops() {
        let (servers, warnings) = wire_mcp_servers(vec![
            (
                "files".to_owned(),
                mcp_server(serde_json::json!({
                    "command": "mcp-files", "args": ["--ro"], "env": {"K": "V"},
                })),
            ),
            (
                "remote".to_owned(),
                mcp_server(serde_json::json!({
                    "url": "https://mcp.test", "headers": {"Authorization": "t"},
                })),
            ),
            (
                "off".to_owned(),
                mcp_server(serde_json::json!({"command": "x", "disabled": true})),
            ),
            ("empty".to_owned(), mcp_server(serde_json::json!({}))),
            (
                "trimmed".to_owned(),
                mcp_server(serde_json::json!({"command": "t", "disabled_tools": ["rm"]})),
            ),
            (
                "login".to_owned(),
                mcp_server(serde_json::json!({"url": "https://o.test", "oauth": {}})),
            ),
            (
                "both".to_owned(),
                mcp_server(serde_json::json!({
                    "url": "https://b.test", "oauth": {}, "disabled_tools": ["x"],
                })),
            ),
        ]);
        let names: Vec<&str> = servers
            .iter()
            .map(|server| match server {
                McpServer::Stdio(server) => server.name.as_str(),
                McpServer::Http(server) | McpServer::Sse(server) => server.name.as_str(),
            })
            .collect();
        assert_eq!(
            names,
            ["files", "remote", "trimmed", "login", "both"],
            "disabled and transport-less servers are left out"
        );
        assert!(matches!(
            &servers[0],
            McpServer::Stdio(stdio)
                if stdio.command == "mcp-files" && stdio.args == ["--ro"]
                    && stdio.env == [EnvVariable { name: "K".into(), value: "V".into() }]
        ));
        assert!(matches!(
            &servers[1],
            McpServer::Http(http)
                if http.url == "https://mcp.test"
                    && http.headers == [HttpHeader { name: "Authorization".into(), value: "t".into() }]
        ));
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("`trimmed`") && warnings[0].contains("disabled_tools"));
        assert!(warnings[1].contains("`login`") && warnings[1].contains("oauth"));
        assert!(
            warnings[2].contains("disabled_tools and oauth"),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_acp_model_opens_an_external_session() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        pump.model = "acp:reviewer".to_owned();
        pump.request_new_session();
        let outgoing = pump.store.take_outgoing();
        let Frame::Request { method, params, .. } = &outgoing[0] else {
            panic!("session/new is a request, got {:?}", outgoing[0])
        };
        assert_eq!(method, "session/new");
        assert_eq!(params["_meta"]["kage"]["externalAgent"], "reviewer");
        assert_eq!(params["mcpServers"], serde_json::json!([]));
        assert!(
            params["_meta"]["kage"].get("plugins").is_none(),
            "an external session claims no plugin host"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_external_session_adopts_without_a_model_option() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        pump.model = "acp:reviewer".to_owned();
        pump.request_new_session();
        let new = request_id(&pump.store.take_outgoing(), "session/new");
        pump.absorb(Frame::Success {
            id: new,
            result: serde_json::json!({"sessionId": "ext1", "configOptions": []}),
        });
        pump.flush();
        assert_eq!(pump.wire, "ext1");
        assert_eq!(pump.model, "acp:reviewer");
        assert_eq!(pump.external_agent_of("ext1").as_deref(), Some("reviewer"));
        assert_eq!(pump.state_snapshot().model, "acp:reviewer");
        let outgoing = pump.store.take_outgoing();
        assert!(
            !has_request(&outgoing, "session/set_config_option"),
            "no engine model is pushed onto an external session: {outgoing:?}"
        );
        assert!(
            !notices(&app)
                .iter()
                .any(|(_, text)| text.contains("legacy one-shot")),
            "the one-shot warning is for engine sessions only"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_external_session_refuses_engine_only_requests() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        pump.model = "acp:reviewer".to_owned();
        pump.request_new_session();
        let new = request_id(&pump.store.take_outgoing(), "session/new");
        pump.absorb(Frame::Success {
            id: new,
            result: serde_json::json!({"sessionId": "ext1", "configOptions": []}),
        });
        pump.handle(RunRequest::SwitchModel("anthropic/claude".to_owned()));
        pump.handle(RunRequest::CompactNow);
        pump.handle(RunRequest::SetPermissionMode(Some(
            kage_core::permissions::PermissionAction::Allow,
        )));
        let outgoing = pump.store.take_outgoing();
        assert!(
            !has_request(&outgoing, "session/set_config_option")
                && !has_request(&outgoing, "_kage/session/compact"),
            "external sessions take prompts and answers only: {outgoing:?}"
        );
        let warnings: Vec<String> = notices(&app)
            .into_iter()
            .filter(|(level, _)| *level == NoticeLevel::Warning)
            .map(|(_, text)| text)
            .collect();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().all(|text| text.contains("reviewer")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_acp_pick_in_the_model_picker_opens_an_external_session() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        pump.handle(RunRequest::SwitchModel("acp:reviewer".to_owned()));
        let outgoing = pump.store.take_outgoing();
        let Frame::Request { method, params, .. } = &outgoing[0] else {
            panic!("session/new is a request, got {:?}", outgoing[0])
        };
        assert_eq!(method, "session/new");
        assert_eq!(params["_meta"]["kage"]["externalAgent"], "reviewer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recorded_external_session_is_marked_from_its_header() {
        let (mut pump, _app, _dialogs, _feed, dir) = test_pump();
        let path = dir.join("ext-rec.jsonl");
        let header = kage_session::entry::Header {
            version: kage_session::FORMAT_VERSION,
            session: kage_core::SessionId::new(),
            id: kage_session::EntryId::new(),
            ts: chrono::Utc::now(),
            cwd: dir.clone(),
            model: "external".to_owned(),
            system_prompt: String::new(),
            backend: Some(kage_session::SessionBackend::ExternalAcp(
                kage_session::entry::ExternalAcpSession {
                    agent_name: "reviewer".to_owned(),
                    protocol_version: 1,
                    upstream_session_id: "up-1".to_owned(),
                },
            )),
            parent_session: None,
            parent_entry: None,
        };
        kage_session::SessionWriter::create(&path, header).unwrap();
        pump.open_recorded(&path);
        assert_eq!(
            pump.external.get("ext-rec").map(String::as_str),
            Some("reviewer"),
            "the file's header marks the session external before the load answers"
        );
        let load = request_id(&pump.store.take_outgoing(), "session/load");
        pump.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({"configOptions": []}),
        });
        assert_eq!(pump.wire, "ext-rec");
        assert_eq!(pump.model, "acp:reviewer");
        assert_eq!(pump.state_snapshot().model, "acp:reviewer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_socket_transport_ends_its_reader_and_the_peer_sees_eof() {
        let (mine, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let (events, received) = mpsc::channel();
        let transport = Transport::host(mine, events).unwrap();
        drop(transport);
        match received.recv_timeout(Duration::from_secs(30)) {
            Ok(Event::Closed) => {}
            other => panic!("the reader must end on the drop, got {other:?}"),
        }
        let mut line = String::new();
        let read = BufReader::new(&theirs).read_line(&mut line).unwrap();
        assert_eq!(read, 0, "the peer reads end of file, not a timeout");
    }
}
