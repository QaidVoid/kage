//! The shared kage host: one engine per user that local clients connect to.
//!
//! The host holds an exclusive lock on `host.lock` in the runtime directory
//! ([`crate::paths::runtime_dir`], mode 0700) for its whole life, so only
//! one runs at a time. It binds `host.sock` (mode 0600) at once, so clients
//! queue while its engine starts, and records itself in `host.json` once it
//! accepts them. Only processes of the same user are served. It exits on
//! SIGINT or SIGTERM, or once it has had no clients and no work for its idle
//! timeout.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, ErrorKind, Write};
use std::net::{Shutdown, TcpListener};
use std::os::fd::AsFd as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use nix::fcntl::{Flock, FlockArg};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use serde::{Deserialize, Serialize};

use crate::rpc::host::Host;
use crate::serve::{Budget, Log};

/// The socket clients connect to, in the runtime directory.
pub(crate) const SOCKET: &str = "host.sock";
/// The record describing the running host, in the runtime directory.
pub(crate) const RECORD: &str = "host.json";
const LOCK: &str = "host.lock";

/// The version of what a client and the host speak over the socket.
pub(crate) const PROTOCOL: u32 = 1;

const MAX_CONNECTIONS: usize = 64;
// How often the accept loop checks for a stop signal and for idleness.
const TICK: Duration = Duration::from_millis(250);
// How long a new host waits for an old one that is shutting down.
const TAKEOVER: Duration = Duration::from_secs(10);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
// How long `kage host stop` waits after SIGTERM, and then after SIGKILL.
const STOP_WAIT: Duration = Duration::from_secs(10);
// How long an ask may hold the host after its last client left.
const ASK_HOLD: Duration = Duration::from_secs(12 * 60 * 60);
const KILL_WAIT: Duration = Duration::from_secs(5);
// Output queued for one client before it is disconnected. A client that is
// only paused, such as a suspended terminal editor, keeps its connection
// until this fills.
const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;

/// What `host.json` records about the running host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The host's process id.
    pub(crate) pid: u32,
    /// The kage version the host runs.
    pub(crate) version: String,
    /// See [`PROTOCOL`].
    pub(crate) protocol: u32,
    /// The socket clients connect to.
    pub(crate) socket: PathBuf,
    /// When the host started accepting clients.
    pub(crate) started: DateTime<Utc>,
}

/// Entry point for `kage host run`. A detached host, started by a client,
/// leaves the client's session so the client's terminal does not stop it.
pub(crate) fn run(
    idle: Duration,
    detached: bool,
    host_cfg: &kage_core::config::HostConfig,
) -> ExitCode {
    if detached {
        let _ = nix::unistd::setsid();
    }
    let log: Log = Arc::new(|line| eprintln!("kage host: {line}"));
    match serve(idle, host_cfg, &log) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: host: {e}");
            ExitCode::from(1)
        }
    }
}

fn serve(
    idle: Duration,
    host_cfg: &kage_core::config::HostConfig,
    log: &Log,
) -> Result<(), String> {
    // Before anything is published, so a signal always shuts down cleanly.
    let stop = Arc::new(AtomicBool::new(false));
    crate::serve::install_signals(&stop, log);
    let dir = crate::paths::runtime_dir()?;
    prepare(&dir)?;
    let Some(lock) = acquire(&dir)? else {
        log("another kage host or serve is already running");
        return Ok(());
    };
    let (listener, mut published) = bind(&dir)?;
    kage_core::reaper::start();
    let host = Host::start(None, "", false)?;
    let budget = Budget::new(MAX_CONNECTIONS);
    let mut web = if host_cfg.web {
        Some(start_web(host_cfg, &host, budget.clone(), log)?)
    } else {
        None
    };
    published.record(&dir)?;
    log(&format!(
        "accepting clients on {}",
        dir.join(SOCKET).display()
    ));
    let mut listener = listener;
    loop {
        serve_until_idle(&listener, &host, &budget, &stop, idle, log);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Unlink first, so a client either connected before and is served
        // now, or connects after and starts a new host.
        let _ = fs::remove_file(&published.socket);
        if !accept_all(&listener, &host, &budget, log) && budget.live() == 0 {
            log("idle; exiting");
            break;
        }
        listener = bind_socket(&published.socket)?;
    }
    // The web endpoint stops accepting first: a web client that dials
    // from here on is refused, and a local one starts a new host with
    // the rest. The socket goes
    // next: a client either connected before and is served now, or
    // connects after and starts a new host. The record goes last, once
    // the lock is about to go: a host holds the lock only while its
    // socket or its record is present, so a starting instance can tell
    // a host from a serve, which publishes neither.
    if let Some(web) = web.as_mut() {
        web.stop();
    }
    let _ = fs::remove_file(&published.socket);
    log("shutting down; cancelling runs");
    host.shutdown();
    thread::sleep(SHUTDOWN_GRACE);
    published.retire();
    drop(lock);
    drop(published);
    Ok(())
}

/// Entry point for `kage host status`.
pub(crate) fn status() -> ExitCode {
    let dir = match crate::paths::runtime_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("kage: host: {e}");
            return ExitCode::from(1);
        }
    };
    if !lock_held(&dir) {
        println!("no shared host is running");
        return ExitCode::from(1);
    }
    let Some(record) = read_record(&dir) else {
        if dir.join(SOCKET).exists() {
            println!("a shared host is starting or stopping");
        } else {
            println!(
                "another kage instance holds the lock with no host: a kage serve is \
                 probably running; stop it in its own terminal"
            );
        }
        return ExitCode::SUCCESS;
    };
    println!("pid      {}", record.pid);
    println!("version  {}", record.version);
    println!(
        "started  {}",
        record
            .started
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
    );
    println!("socket   {}", record.socket.display());
    if let Ok(log) = crate::host_client::log_path() {
        println!("log      {} (a host clients started)", log.display());
    }
    ExitCode::SUCCESS
}

/// Entry point for `kage host stop`.
pub(crate) fn stop() -> ExitCode {
    match stop_host() {
        Ok(Some(message)) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("no shared host is running");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kage: host: {e}");
            ExitCode::from(1)
        }
    }
}

fn stop_host() -> Result<Option<String>, String> {
    use nix::sys::signal::{Signal, kill};
    let dir = crate::paths::runtime_dir()?;
    if !lock_held(&dir) {
        return Ok(None);
    }
    let record = match read_record(&dir) {
        Some(record) => record,
        None if dir.join(SOCKET).exists() => {
            return Err(
                "the shared host is starting or stopping and has no process id; try again"
                    .to_owned(),
            );
        }
        None => {
            return Err(
                "another kage instance holds the lock with no host: a kage serve is \
                 probably running; stop it in its own terminal"
                    .to_owned(),
            );
        }
    };
    let pid = i32::try_from(record.pid)
        .map(nix::unistd::Pid::from_raw)
        .map_err(|e| e.to_string())?;
    let stopped = Ok(Some(format!("stopped the shared host (pid {pid})")));
    match kill(pid, Signal::SIGTERM) {
        Err(nix::errno::Errno::ESRCH) => return stopped,
        Err(e) => return Err(format!("cannot stop pid {pid}: {e}")),
        Ok(()) => {}
    }
    if gone(&dir, &record, STOP_WAIT) {
        return stopped;
    }
    match kill(pid, Signal::SIGKILL) {
        Err(nix::errno::Errno::ESRCH) => return stopped,
        Err(e) => return Err(format!("cannot kill pid {pid}: {e}")),
        Ok(()) => {}
    }
    if gone(&dir, &record, KILL_WAIT) {
        return Ok(Some(format!("killed the shared host (pid {pid})")));
    }
    Err(format!("pid {pid} still holds the host lock"))
}

// Waits up to `limit` for the host `record` describes to be gone: the lock
// is free, its process no longer exists, or another host has taken over.
// A client may start a new host while the old one shuts down, and that one
// takes the lock the moment it is released.
fn gone(dir: &Path, record: &Record, limit: Duration) -> bool {
    let exited = || {
        i32::try_from(record.pid).is_ok_and(|pid| {
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
                == Err(nix::errno::Errno::ESRCH)
        })
    };
    let replaced = || read_record(dir).is_some_and(|now| now.pid != record.pid);
    let deadline = Instant::now() + limit;
    while lock_held(dir) && !exited() && !replaced() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
    true
}

// Whether a host holds the single-instance lock in `dir`.
fn lock_held(dir: &Path) -> bool {
    let Ok(file) = OpenOptions::new().write(true).open(dir.join(LOCK)) else {
        return false;
    };
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(_unlocked_on_drop) => false,
        Err((_, errno)) => errno == nix::errno::Errno::EWOULDBLOCK,
    }
}

/// The record of the host in `dir`, once it has written one.
pub(crate) fn read_record(dir: &Path) -> Option<Record> {
    serde_json::from_slice(&fs::read(dir.join(RECORD)).ok()?).ok()
}

fn prepare(dir: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| fs::set_permissions(dir, fs::Permissions::from_mode(0o700)))
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// Takes the single-instance lock, waiting for a host that is shutting
/// down. Returns `None` when a live host is serving, when the holder
/// has neither a socket nor a record across a poll (a serve), or when
/// the lock stays held for [`TAKEOVER`].
fn acquire(dir: &Path) -> Result<Option<Flock<File>>, String> {
    let path = dir.join(LOCK);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let deadline = Instant::now() + TAKEOVER;
    // A holder with neither socket nor record across a full poll is a
    // serve, which publishes neither and will not let go of the lock.
    // A host crosses the same look for instants far shorter than the
    // poll, once between taking its lock and binding its socket and
    // once between retiring its record and letting go of the lock
    // while shutting down, so the first sight of the tell only arms
    // the refusal, and the lock is tried again first on every round.
    let mut told = false;
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(Some(lock)),
            Err((held, nix::errno::Errno::EWOULDBLOCK)) => file = held,
            Err((_, e)) => return Err(format!("lock {}: {e}", path.display())),
        }
        if UnixStream::connect(dir.join(SOCKET)).is_ok() {
            return Ok(None);
        }
        let serve = !dir.join(SOCKET).exists() && !dir.join(RECORD).exists();
        if serve && told {
            return Ok(None);
        }
        told = serve;
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Takes the single-instance lock for `kage serve`, which serves
/// remote clients instead of local ones but still must not run
/// alongside a shared host. Returns `None` when another instance
/// holds it, whether a live host or another serve, so the caller
/// exits quietly.
pub(crate) fn acquire_for_serve() -> Result<Option<Flock<File>>, String> {
    let dir = crate::paths::runtime_dir()?;
    prepare(&dir)?;
    acquire(&dir)
}

/// Removes the socket and the record when the host stops.
struct Published {
    socket: PathBuf,
    record: Option<PathBuf>,
}

impl Published {
    // The record tells clients the host is ready.
    fn record(&mut self, dir: &Path) -> Result<(), String> {
        let path = dir.join(RECORD);
        let entry = Record {
            pid: std::process::id(),
            version: kage_core::build::version().to_owned(),
            protocol: PROTOCOL,
            socket: self.socket.clone(),
            started: Utc::now(),
        };
        let bytes = serde_json::to_vec(&entry).map_err(|e| e.to_string())?;
        self.record = Some(path.clone());
        kage_core::fsutil::atomic_write_private(&path, &bytes)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Removes the record while the lock is still held, as the last
    /// thing a host publishes.
    fn retire(&mut self) {
        if let Some(record) = self.record.take() {
            let _ = fs::remove_file(record);
        }
    }
}

impl Drop for Published {
    fn drop(&mut self) {
        if let Some(record) = &self.record {
            let _ = fs::remove_file(record);
        }
        let _ = fs::remove_file(&self.socket);
    }
}

// Holding the lock means no live host owns an existing socket or record, so
// both are left from a host that died and are removed.
fn bind(dir: &Path) -> Result<(UnixListener, Published), String> {
    let socket = dir.join(SOCKET);
    let _ = fs::remove_file(dir.join(RECORD));
    let published = Published {
        socket: socket.clone(),
        record: None,
    };
    let listener = bind_socket(&socket)?;
    Ok((listener, published))
}

fn bind_socket(socket: &Path) -> Result<UnixListener, String> {
    let _ = fs::remove_file(socket);
    let listener = UnixListener::bind(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
        .and_then(|()| listener.set_nonblocking(true))
        .map_err(|e| format!("{}: {e}", socket.display()))?;
    Ok(listener)
}

// Accepts clients until `stop` is set, or until the host has had no client
// and no work for `idle`.
fn serve_until_idle(
    listener: &UnixListener,
    host: &Arc<Host>,
    budget: &Budget,
    stop: &AtomicBool,
    idle: Duration,
    log: &Log,
) {
    let mut quiet_since = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        let mut fds = [PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
        let ready = poll(
            &mut fds,
            PollTimeout::try_from(TICK).unwrap_or(PollTimeout::NONE),
        );
        let accepted = accept_all(listener, host, budget, log);
        if ready.is_err() {
            thread::sleep(TICK);
        }
        host.prune_projects();
        host.decline_expired_asks(ASK_HOLD);
        if accepted || budget.live() > 0 || host.busy() {
            quiet_since = Instant::now();
        } else if quiet_since.elapsed() >= idle {
            return;
        }
    }
}

// Accepts every queued client. Returns whether there was any.
fn accept_all(listener: &UnixListener, host: &Arc<Host>, budget: &Budget, log: &Log) -> bool {
    let mut any = false;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                any = true;
                accept(stream, host, budget, log);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return any,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                log(&format!("accept: {e}"));
                return any;
            }
        }
    }
}

fn accept(stream: UnixStream, host: &Arc<Host>, budget: &Budget, log: &Log) {
    if !peer_is_us(&stream) {
        log("refused a client running as another user");
        return;
    }
    if budget.spent() {
        log("refused a client: too many connections");
        return;
    }
    let setup = stream
        .set_nonblocking(false)
        .and_then(|()| stream.try_clone());
    let reader = match setup {
        Ok(reader) => reader,
        Err(e) => {
            log(&format!("cannot set up a client: {e}"));
            return;
        }
    };
    let writer = match Outgoing::start(stream) {
        Ok(writer) => writer,
        Err(e) => {
            log(&format!("cannot set up a client: {e}"));
            return;
        }
    };
    let guard = budget.admit();
    let host = Arc::clone(host);
    let spawned = thread::Builder::new()
        .name("kage-host-client".into())
        .spawn(move || {
            let _guard = guard;
            let _ = host.serve_local(BufReader::new(reader), writer);
        });
    if let Err(e) = spawned {
        log(&format!("cannot serve a client: {e}"));
    }
}

/// Whether the process at the other end of `stream` runs as this user.
fn peer_is_us(stream: &UnixStream) -> bool {
    peer_uid(stream).is_some_and(|uid| uid == nix::unistd::geteuid().as_raw())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    getsockopt(stream, PeerCredentials)
        .ok()
        .map(|creds| creds.uid())
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    nix::unistd::getpeereid(stream)
        .ok()
        .map(|(uid, _)| uid.as_raw())
}

/// No way to tell the peer's user here, so every client is refused.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn peer_uid(_stream: &UnixStream) -> Option<u32> {
    None
}

/// The host's web endpoint, served on its own thread.
struct WebListener {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl WebListener {
    /// Stops accepting web clients and waits for the accept loop to
    /// finish. Live connections end with the process, as the local
    /// clients' do during the drain.
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Binds the web endpoint `[host] web` asks for, the same listener `kage
/// serve` runs with the same token and assets, and serves it on its own
/// thread until stopped, against the same host and the same connection
/// budget as the local socket, so a web client holds the host exactly
/// like a local one.
fn start_web(
    host_cfg: &kage_core::config::HostConfig,
    host: &Arc<Host>,
    budget: Budget,
    log: &Log,
) -> Result<WebListener, String> {
    let (token_path, token) = crate::serve::load_token(false)?;
    let bundle_dir = crate::serve::default_web_dir()?;
    let web = crate::serve::open_web(
        &bundle_dir,
        "run kage serve --fetch-web while no host or serve runs, to install it",
        "run kage serve --fetch-web while no host or serve runs, to update it",
        log,
    );
    let listener = TcpListener::bind((host_cfg.bind.as_str(), host_cfg.port)).map_err(|e| {
        format!(
            "cannot bind {}:{}: {e}; set [host] port to a free port",
            host_cfg.bind, host_cfg.port
        )
    })?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("web endpoint: {e}"))?;
    log(&format!("token file {}", token_path.display()));
    crate::serve::log_connect(addr, &token, &token_path, log);
    if web.available() {
        log(&format!("web UI: http://{addr}/"));
    }
    crate::serve::warn_non_loopback(&host_cfg.bind, log);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_host = Arc::clone(host);
    let thread_log = Arc::clone(log);
    let handle = thread::Builder::new()
        .name("kage-host-web".to_owned())
        .spawn(move || {
            crate::serve::accept_until(
                &listener,
                &thread_host,
                &token,
                &web,
                &budget,
                &thread_stop,
                &thread_log,
            );
        })
        .map_err(|e| format!("cannot start the web listener: {e}"))?;
    Ok(WebListener {
        stop,
        handle: Some(handle),
    })
}

/// A client's output, queued for its own writer thread, so a client that
/// stops reading delays only itself. Each flush queues one message. A
/// client whose queue passes its byte bound, or whose write fails, is
/// disconnected, which also ends its reader.
struct Outgoing {
    message: Vec<u8>,
    queue: mpsc::Sender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    stream: Arc<UnixStream>,
}

impl Outgoing {
    fn start(stream: UnixStream) -> io::Result<Self> {
        let stream = Arc::new(stream);
        let (queue, messages) = mpsc::channel::<Vec<u8>>();
        let queued = Arc::new(AtomicUsize::new(0));
        let (socket, sent) = (Arc::clone(&stream), Arc::clone(&queued));
        thread::Builder::new()
            .name("kage-host-output".into())
            .spawn(move || {
                for message in messages {
                    let written = (&*socket).write_all(&message);
                    sent.fetch_sub(message.len(), Ordering::SeqCst);
                    if written.is_err() {
                        let _ = socket.shutdown(Shutdown::Both);
                        return;
                    }
                }
            })?;
        Ok(Self {
            message: Vec::new(),
            queue,
            queued,
            stream,
        })
    }

    fn disconnect(&self, why: &str) -> io::Error {
        let _ = self.stream.shutdown(Shutdown::Both);
        io::Error::new(ErrorKind::BrokenPipe, why)
    }
}

impl Write for Outgoing {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.message.len() + bytes.len() > MAX_QUEUED_BYTES {
            return Err(self.disconnect("message too large"));
        }
        self.message.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.message.is_empty() {
            return Ok(());
        }
        let message = std::mem::take(&mut self.message);
        let size = message.len();
        let fits = self
            .queued
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |queued| {
                queued
                    .checked_add(size)
                    .filter(|total| *total <= MAX_QUEUED_BYTES)
            })
            .is_ok();
        if !fits {
            return Err(self.disconnect("client is not reading"));
        }
        if self.queue.send(message).is_err() {
            self.queued.fetch_sub(size, Ordering::SeqCst);
            return Err(self.disconnect("client is not reading"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn a_client_that_stops_reading_is_cut_off_without_blocking_the_writer() {
        let (host_end, client_end) = UnixStream::pair().unwrap();
        let mut outgoing = Outgoing::start(host_end).unwrap();
        let started = Instant::now();
        let line = vec![b'x'; 64 * 1024];
        let mut refused = false;
        for _ in 0..1024 {
            if outgoing
                .write_all(&line)
                .and_then(|()| outgoing.flush())
                .is_err()
            {
                refused = true;
                break;
            }
        }
        assert!(refused, "an unread client was never cut off");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "queueing blocked on the unread client"
        );
        let mut buffer = vec![0; 1024 * 1024];
        let mut read = 0;
        loop {
            match (&client_end).read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => read += n,
            }
        }
        assert!(read > 0, "the client never saw its earlier output");
    }

    #[test]
    fn a_reading_client_receives_every_message_in_order() {
        let (host_end, client_end) = UnixStream::pair().unwrap();
        let mut outgoing = Outgoing::start(host_end).unwrap();
        for n in 0..100 {
            writeln!(outgoing, "{n}").unwrap();
            outgoing.flush().unwrap();
        }
        drop(outgoing);
        let mut text = String::new();
        BufReader::new(client_end)
            .read_to_string(&mut text)
            .unwrap();
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let expected: Vec<String> = (0..100).map(|n| n.to_string()).collect();
        assert_eq!(lines, expected);
    }
}
