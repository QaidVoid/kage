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
use std::net::Shutdown;
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
use crate::serve::Log;

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
pub(crate) fn run(idle: Duration, detached: bool) -> ExitCode {
    if detached {
        let _ = nix::unistd::setsid();
    }
    let log: Log = Arc::new(|line| eprintln!("kage host: {line}"));
    match serve(idle, &log) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: host: {e}");
            ExitCode::from(1)
        }
    }
}

fn serve(idle: Duration, log: &Log) -> Result<(), String> {
    // Before anything is published, so a signal always shuts down cleanly.
    let stop = Arc::new(AtomicBool::new(false));
    crate::serve::install_signals(&stop, log);
    let dir = crate::paths::runtime_dir()?;
    prepare(&dir)?;
    let Some(lock) = acquire(&dir)? else {
        log("another host is already running");
        return Ok(());
    };
    let (listener, mut published) = bind(&dir)?;
    kage_core::reaper::start();
    let host = Host::start(None, "", false)?;
    published.record(&dir)?;
    log(&format!(
        "accepting clients on {}",
        dir.join(SOCKET).display()
    ));
    let connections = Arc::new(AtomicUsize::new(0));
    let mut listener = listener;
    loop {
        serve_until_idle(&listener, &host, &connections, &stop, idle, log);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Unlink first, so a client either connected before and is served
        // now, or connects after and starts a new host.
        let _ = fs::remove_file(&published.socket);
        if !accept_all(&listener, &host, &connections, log) {
            log("idle; exiting");
            break;
        }
        listener = bind_socket(&published.socket)?;
    }
    // New clients now find no socket and start a host, which waits for
    // this one's lock and then takes over.
    drop(published);
    log("shutting down; cancelling runs");
    host.shutdown();
    thread::sleep(SHUTDOWN_GRACE);
    drop(lock);
    Ok(())
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
/// down. Returns `None` when a live host is serving, or when the lock stays
/// held without one for [`TAKEOVER`].
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
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(Some(lock)),
            Err((held, nix::errno::Errno::EWOULDBLOCK)) => file = held,
            Err((_, e)) => return Err(format!("lock {}: {e}", path.display())),
        }
        if UnixStream::connect(dir.join(SOCKET)).is_ok() || Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(50));
    }
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
    connections: &Arc<AtomicUsize>,
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
        let accepted = accept_all(listener, host, connections, log);
        if ready.is_err() {
            thread::sleep(TICK);
        }
        if accepted || connections.load(Ordering::SeqCst) > 0 || host.busy() {
            quiet_since = Instant::now();
        } else if quiet_since.elapsed() >= idle {
            return;
        }
    }
}

// Accepts every queued client. Returns whether there was any.
fn accept_all(
    listener: &UnixListener,
    host: &Arc<Host>,
    connections: &Arc<AtomicUsize>,
    log: &Log,
) -> bool {
    let mut any = false;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                any = true;
                accept(stream, host, connections, log);
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

fn accept(stream: UnixStream, host: &Arc<Host>, connections: &Arc<AtomicUsize>, log: &Log) {
    if !crate::rpc::link::peer_is_us(&stream) {
        log("refused a client running as another user");
        return;
    }
    if connections.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
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
    connections.fetch_add(1, Ordering::SeqCst);
    let guard = Connection(Arc::clone(connections));
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

// Counts a client from accept until its thread ends.
struct Connection(Arc<AtomicUsize>);

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
