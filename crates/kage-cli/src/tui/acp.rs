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
//! "client"`. A Lua call the pump serves runs synchronously on the
//! pump thread, so frames from the agent stall until it returns; the
//! engine path pays the same serialization around its Lua mutex. A
//! resumed session has no such `_meta` on its wire yet, so it runs
//! without the client's plugins and tools until the load learns to
//! carry them.
//!
//! Some commands the engine path serves have no ACP counterpart yet:
//! the pump refuses them with a notice instead of dropping them
//! silently.

use std::collections::{BTreeMap, HashSet};
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
    PluginsSource, SessionOpt, TextContent, UnconfiguredTools,
};
use kage_client::{Change, DialogAsk, Frame, PermissionDecision, RequestId, RpcError};
use kage_core::keymap::{Keymap, MappingData};
use kage_core::options::OptionValue;
use kage_core::protocol::{
    Delivery, Envelope, HostEvent, NoticeLevel, Question, QuestionOption, RunOutcome, SessionId,
    SessionState,
};
use kage_core::{CancelFlag, LoopEvent, MediaSource, Message, Role, ThinkingLevel, ToolCallId};
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
}

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
        Self::stream(BufReader::new(reader), stream, None, events)
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
        let (client_read, client_write, host_read, host_write) =
            private_pair().map_err(|error| format!("cannot open the private pipe: {error}"))?;
        let spawned = thread::Builder::new()
            .name("kage-acp-private-host".to_owned())
            .spawn(move || {
                let _ = host.serve_local(host_read, host_write);
            });
        spawned.map_err(|error| format!("cannot start the private host: {error}"))?;
        Self::stream(BufReader::new(client_read), client_write, None, events)
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
/// reads the first and writes the second, the host the other two. A
/// Unix stream pair on Unix, a channel pipe elsewhere.
type PrivateRead = Box<dyn BufRead + Send>;
type PrivateWrite = Box<dyn Write + Send>;

#[cfg(unix)]
fn private_pair() -> std::io::Result<(PrivateRead, PrivateWrite, PrivateRead, PrivateWrite)> {
    use std::os::unix::net::UnixStream;
    let (client, host_end) = UnixStream::pair()?;
    let host_reader = host_end.try_clone()?;
    let client_reader = client.try_clone()?;
    Ok((
        Box::new(BufReader::new(client_reader)),
        Box::new(client),
        Box::new(BufReader::new(host_reader)),
        Box::new(host_end),
    ))
}

#[cfg(not(unix))]
fn private_pair() -> std::io::Result<(PrivateRead, PrivateWrite, PrivateRead, PrivateWrite)> {
    let (client_write, host_read) = super::pipe::channel_pair();
    let (host_write, client_read) = super::pipe::channel_pair();
    Ok((
        Box::new(BufReader::new(client_read)),
        Box::new(client_write),
        Box::new(BufReader::new(host_read)),
        Box::new(host_write),
    ))
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

/// The `_kage/plugin/*` surface of the session whose plugins this
/// process hosts: the local runtime answering for it. One surface per
/// connection, whatever session the agent names, like the engine
/// serves every session from one runtime.
struct PluginSurface {
    runtime: Arc<PluginRuntime>,
    /// Where the runtime's tools execute.
    workdir: PathBuf,
    /// Raised when the connection ends, so a tool call the agent
    /// abandoned does not start. One already running keeps the pump
    /// thread until it returns.
    cancel: CancelFlag,
}

impl PluginSurface {
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
        let cx = kage_tools::ToolContext::new(&self.workdir, &self.cancel);
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
                Ok(Event::Frame(frame)) => self.absorb(frame),
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

    /// Feeds one frame in. The plugin reverse requests never touch the
    /// store: the local surface answers them at once, because the
    /// client would otherwise fail them as methods it does not know.
    /// Hosting nothing leaves them to the store, which fails them the
    /// same way.
    fn absorb(&mut self, frame: Frame) {
        if let Frame::Request { id, method, params } = &frame
            && method.starts_with("_kage/plugin/")
        {
            let answered = self
                .plugins
                .as_ref()
                .map(|surface| surface.answer(method, params));
            if let Some(answered) = answered {
                match answered {
                    Ok(result) => self.store.client().reply(id.clone(), result),
                    Err(error) => self.store.client().fail(id.clone(), error),
                }
                return;
            }
        }
        for change in self.store.absorb(frame) {
            self.react(change);
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
            let servers = self.mcp_servers();
            self.store
                .client()
                .new_session(&self.cwd, &servers, Some(meta));
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
            self.plugins.is_some(),
        )
    }

    /// The MCP servers the session brings: the hosted plugins' own
    /// registrations, disabled ones left out, like the engine path.
    /// The agent spawns them, so none of this process's MCP runs
    /// beside the pump.
    fn mcp_servers(&self) -> Vec<McpServer> {
        let Some(surface) = self.plugins.as_ref() else {
            return Vec::new();
        };
        surface
            .runtime
            .registered_mcp_servers()
            .into_iter()
            .filter_map(|(name, server)| {
                if server.disabled {
                    return None;
                }
                match (server.command, server.url) {
                    (Some(command), _) => Some(McpServer::Stdio(McpServerStdio {
                        name,
                        command,
                        args: server.args,
                        env: server
                            .env
                            .into_iter()
                            .map(|(name, value)| EnvVariable { name, value })
                            .collect(),
                    })),
                    (None, Some(url)) => Some(McpServer::Http(McpServerHttp {
                        name,
                        url,
                        headers: server
                            .headers
                            .into_iter()
                            .map(|(name, value)| HttpHeader { name, value })
                            .collect(),
                    })),
                    // Neither transport: the config validator rejects
                    // such a server before it can get here.
                    (None, None) => None,
                    // Fields the wire type cannot carry, such as
                    // `disabled_tools` and OAuth, drop here.
                }
            })
            .collect()
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
                    let state = self.state_snapshot();
                    self.update_ui_slot(|ui| ui.state = state.clone());
                    self.publish(HostEvent::StateChanged { state });
                    let usage = self.usage_snapshot();
                    self.update_ui_slot(|ui| ui.usage = Some(usage));
                    self.publish(HostEvent::UsageUpdated { usage });
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
            // What the link protocol's bus subscriber records: the
            // model a completed run used, and the run itself.
            let _ = crate::state::record_last_model(&self.model);
            let _ = crate::state::record_run_outcome(&self.model, &RunOutcome::Completed);
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
        self.warned_model = None;
        self.handed = None;
        self.hand_buffer();
        self.update_ui_slot(|ui| {
            ui.session_id.clone_from(&self.wire);
            ui.session_title = None;
        });
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
                self.invoke_plugin_command(&name, &args);
            }
            RunRequest::InvokeKeymap { id } => self.invoke_keymap(id),
            RunRequest::NewSession => {
                self.pending_new = true;
                let meta = self.session_meta();
                let servers = self.mcp_servers();
                self.store
                    .client()
                    .new_session(&self.cwd, &servers, Some(meta));
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
        // A tool call the agent left running must not run on: the
        // surface's next execute sees the flag at once.
        if let Some(surface) = &self.plugins {
            surface.cancel.cancel();
        }
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
        let event = event.into();
        let warning = if let kage_core::protocol::Event::Host(HostEvent::StateChanged { state }) =
            &event
            && self.warned_model.as_deref() != Some(state.model.as_str())
        {
            self.warned_model = Some(state.model.clone());
            support::legacy_acp_warning(&state.model)
        } else {
            None
        };
        self.seq += 1;
        let _ = self.events.send(Envelope {
            session: self.app_session,
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

/// Which agent the TUI drives: the shared host of this user unless
/// asked or unable to use it. `--standalone`, `[host] share = false`,
/// client configuration overrides, a host of another kage version, one that cannot be started, and
/// platforms without the socket all fall back to a private host in
/// this process, like `kage rpc` falls back to its private engine.
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
    let surface = plugins.as_ref().map(|rt| PluginSurface {
        runtime: Arc::clone(rt),
        workdir: workdir.clone(),
        cancel: CancelFlag::new(),
    });
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
        warned_model: None,
        options: Arc::clone(&options),
        registry: Arc::clone(&registry),
        plugins: surface,
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
    let mirror = Arc::new(Mutex::new(Mirror::new(None)));
    let model_choices = available_model_items(&registry, &shown_model);
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
        requests: requests_tx,
        events: events_rx,
        dialogs: dialog_rx,
        refresh: refresh_rx,
        acp_feed: Some(feed),
        model: shown_model,
        model_choices,
        plugin_commands,
        skills,
        notices,
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
            warned_model: None,
            options: Arc::new(Mutex::new(kage_core::options::OptionStore::default())),
            registry: Arc::new(ProviderRegistry::default()),
            plugins: None,
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
            next_ask: 0,
            pending_export: None,
            unknown: HashSet::new(),
            closed: false,
        };
        (pump, app, dialog_rx, feed, dir)
    }

    #[test]
    fn a_resumed_legacy_model_warns_once_and_switching_back_warns_again() {
        let (mut pump, app, _dialogs, _feed, dir) = test_pump();
        pump.pending_open = Some("resumed".to_owned());
        pump.store.client().load_session("resumed", "/w", &[]);
        let request = pump.store.take_outgoing().pop().unwrap();
        let Frame::Request { id, .. } = request else {
            panic!("load sends a request");
        };
        let changes = pump.store.absorb(Frame::Success {
            id,
            result: serde_json::json!({"configOptions": [{
                "id": "model", "name": "Model", "category": "model",
                "type": "select", "currentValue": "acp:reviewer", "options": []
            }]}),
        });
        for change in changes {
            pump.react(change);
        }
        pump.session_moved("resumed".to_owned());
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
            let changes = pump.store.absorb(Frame::Notification {
                method: "session/update".into(),
                params: serde_json::json!({"sessionId": "resumed", "update": {
                    "sessionUpdate": "config_option_update", "configOptions": [{
                        "id": "model", "name": "Model", "category": "model",
                        "type": "select", "currentValue": model, "options": []
                    }]
                }}),
            });
            for change in changes {
                pump.react(change);
            }
        }
        assert_eq!(warnings(), 1);
        pump.pending_open = Some("another".to_owned());
        pump.store.client().load_session("another", "/w", &[]);
        let request = pump.store.take_outgoing().pop().unwrap();
        let Frame::Request { id, .. } = request else {
            panic!("load sends a request");
        };
        let changes = pump.store.absorb(Frame::Success {
            id,
            result: serde_json::json!({"configOptions": [{
                "id": "model", "name": "Model", "category": "model",
                "type": "select", "currentValue": "acp:reviewer", "options": []
            }]}),
        });
        for change in changes {
            pump.react(change);
        }
        pump.session_moved("another".to_owned());
        assert_eq!(
            warnings(),
            1,
            "a different transcript needs its own warning"
        );
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
        let registry = crate::build_provider_registry().unwrap();
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

    /// The Lua fixture the hosted-pump tests load: an echo tool and
    /// an override of it, a failing tool, and notification, transform
    /// and predicate handlers.
    const FIXTURE: &str = r"
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
                vec!["context".to_owned()],
            )]))
            .build()
            .unwrap();
        kage_plugin::load_all(Some(&dir), &runtime).unwrap();
        pump.plugins = Some(PluginSurface {
            runtime: Arc::new(runtime),
            workdir: dir.clone(),
            cancel: CancelFlag::new(),
        });
        (pump, app, dir)
    }

    /// A session id the wire shape demands: a real ULID, since the
    /// surface scopes dispatches by it.
    fn fixture_session() -> String {
        ulid::Ulid::generate().to_string()
    }

    /// Feed one request in and return the reply the surface queued.
    fn plugin_reply(pump: &mut Pump, id: u64, method: &str, params: Value) -> Frame {
        pump.absorb(Frame::Request {
            id: RequestId::Number(id),
            method: method.to_owned(),
            params,
        });
        pump.store
            .take_outgoing()
            .into_iter()
            .next()
            .expect("the surface answers")
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
}
