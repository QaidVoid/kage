//! Reaching the shared host from a client process, starting it when it is
//! not running, and relaying a client's stdio to it.
//!
//! A client connects to `host.sock` in the runtime directory. When nothing
//! listens there it starts `kage host run --detached` and retries. Several
//! clients may start a host at once: the losers wait and defer to the
//! winner. A host of another kage version is not used, so the caller can
//! fall back to its own engine until that host goes idle and exits.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::rpc::{MAX_ENVIRONMENT_BYTES, environment_vars};
use crate::shared_host::{SOCKET, read_record};

// How long a client waits for a host it started.
const START_WAIT: Duration = Duration::from_secs(10);
// The host log is moved aside at this size when a client starts a host.
const MAX_LOG_BYTES: u64 = 1024 * 1024;
/// Why a client cannot use the shared host.
#[derive(Debug)]
pub(crate) enum Unavailable {
    /// The running host is another kage version.
    Version(String),
    /// The host could not be reached or started.
    Failed(String),
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version(version) => write!(f, "the shared host runs kage {version}"),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

/// Connects to the shared host of this kage version, starting it when none
/// is running.
pub(crate) fn connect() -> Result<UnixStream, Unavailable> {
    let dir = crate::paths::runtime_dir().map_err(Unavailable::Failed)?;
    let mut started = None;
    let stream = if let Ok(stream) = UnixStream::connect(dir.join(SOCKET)) {
        stream
    } else {
        started = Some(start()?);
        wait_for_socket(&dir, started.as_mut())?
    };
    check_version(&dir, &stream, started.as_mut())?;
    Ok(stream)
}

fn start() -> Result<Child, Unavailable> {
    let exe = std::env::current_exe()
        .map_err(|e| Unavailable::Failed(format!("cannot find kage: {e}")))?;
    let mut command = Command::new(exe);
    command
        .args(["host", "run", "--detached"])
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file().map_or_else(|_| Stdio::null(), Stdio::from));
    if let Some(idle) = std::env::var("KAGE_HOST_IDLE_SECS")
        .ok()
        .filter(|idle| idle.parse::<u64>().is_ok_and(|secs| secs > 0))
    {
        command.arg("--idle-secs").arg(idle);
    }
    command
        .spawn()
        .map_err(|e| Unavailable::Failed(format!("cannot start the shared host: {e}")))
}

// A host this client started and that failed ends the wait at once. One that
// exited cleanly lost the start race to another, so the wait goes on.
fn failed(started: Option<&mut Child>) -> Option<Unavailable> {
    let status = started?.try_wait().ok()??;
    (!status.success()).then(|| {
        Unavailable::Failed(
            "the shared host failed to start; see host.log in the kage state directory".to_owned(),
        )
    })
}

// How far apart two sightings of "no socket, no record" must be, as in
// `shared_host::acquire`.
const REFUSAL_GAP: Duration = Duration::from_millis(50);

// The host this client started refused the lock cleanly. That usually
// means it lost a start race, and the winner binds its socket at once,
// so the wait goes on while a socket or a record shows up. A serve
// holding the lock publishes neither, so with no socket and no record
// at two sightings `REFUSAL_GAP` apart the fallback is a private engine
// instead of the whole takeover wait. A winner crosses that look for an
// instant, between taking its lock and binding, so one sighting only
// arms the refusal; `told` holds when it was first made.
fn refused_the_lock(dir: &Path, started: Option<&mut Child>, told: &mut Option<Instant>) -> bool {
    let exited_cleanly = started
        .and_then(|child| child.try_wait().ok().flatten())
        .is_some_and(|status| status.success());
    if !exited_cleanly || dir.join(SOCKET).exists() || read_record(dir).is_some() {
        *told = None;
        return false;
    }
    let first = *told.get_or_insert_with(Instant::now);
    first.elapsed() >= REFUSAL_GAP
}

/// Where a host started by a client writes its output.
pub(crate) fn log_path() -> Result<PathBuf, String> {
    Ok(crate::paths::state_root()?.join("host.log"))
}

// The host's log, private to the user and moved aside when large.
fn log_file() -> io::Result<fs::File> {
    let path = log_path().map_err(io::Error::other)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
        let _ = fs::rename(&path, path.with_extension("log.1"));
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn wait_for_socket(dir: &Path, mut started: Option<&mut Child>) -> Result<UnixStream, Unavailable> {
    let deadline = Instant::now() + START_WAIT;
    let mut delay = Duration::from_millis(10);
    let mut told = None;
    loop {
        if let Ok(stream) = UnixStream::connect(dir.join(SOCKET)) {
            return Ok(stream);
        }
        if let Some(failure) = failed(started.as_deref_mut()) {
            return Err(failure);
        }
        if refused_the_lock(dir, started.as_deref_mut(), &mut told) {
            return Err(Unavailable::Failed(
                "another kage host or serve is already running".to_owned(),
            ));
        }
        if Instant::now() >= deadline {
            return Err(Unavailable::Failed(
                "the shared host did not start; see host.log in the kage state directory"
                    .to_owned(),
            ));
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(200));
    }
}

// A host binds its socket before its engine is up and records its version
// once it is, so a connected client waits for the record. A host that fails
// meanwhile hangs up.
fn check_version(
    dir: &Path,
    stream: &UnixStream,
    mut started: Option<&mut Child>,
) -> Result<(), Unavailable> {
    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Some(record) = read_record(dir) {
            let ours = kage_core::build::version();
            if record.version == ours {
                return Ok(());
            }
            return Err(Unavailable::Version(record.version));
        }
        if let Some(failure) = failed(started.as_deref_mut()) {
            return Err(failure);
        }
        if hung_up(stream) || Instant::now() >= deadline {
            return Err(Unavailable::Failed(
                "the shared host never became ready".to_owned(),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn hung_up(stream: &UnixStream) -> bool {
    use nix::sys::socket::{MsgFlags, recv};
    use std::os::fd::AsRawFd as _;
    let mut byte = [0];
    matches!(
        recv(
            stream.as_raw_fd(),
            &mut byte,
            MsgFlags::MSG_PEEK | MsgFlags::MSG_DONTWAIT
        ),
        Ok(0)
    )
}

/// Entry point for `kage host logs`: prints the host's log.
pub(crate) fn print_log() -> ExitCode {
    let path = match log_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("kage: host: {e}");
            return ExitCode::from(1);
        }
    };
    match fs::read(&path) {
        Ok(bytes) => {
            let _ = io::stdout().lock().write_all(&bytes);
            ExitCode::SUCCESS
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("kage: host: no log yet at {}", path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kage: host: {}: {e}", path.display());
            ExitCode::from(1)
        }
    }
}

/// The `_kage/environment` notification carrying this process's
/// environment, so the client's sessions run tools with its `PATH` and
/// variables rather than those of whichever client started the host.
/// Variables that are not UTF-8 are left out.
fn environment_notification() -> Option<Vec<u8>> {
    let vars: serde_json::Map<String, serde_json::Value> = environment_vars()
        .into_iter()
        .map(|(name, value)| (name, value.into()))
        .collect();
    let message = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "_kage/environment",
        "params": {"vars": vars},
    });
    let mut line = serde_json::to_vec(&message).ok()?;
    if line.len() > MAX_ENVIRONMENT_BYTES {
        eprintln!("kage: rpc: the environment is too large to send; tools use the shared host's");
        return None;
    }
    line.push(b'\n');
    Some(line)
}

/// Sends this process's environment to the host, then copies stdin to the
/// host and the host's output to stdout until the client closes stdin and
/// the host has answered, or the host goes away.
pub(crate) fn relay(stream: &UnixStream) -> ExitCode {
    let upstream = match stream.try_clone() {
        Ok(upstream) => upstream,
        Err(e) => {
            eprintln!("kage: rpc: {e}");
            return ExitCode::from(1);
        }
    };
    if let Some(line) = environment_notification()
        && let Err(e) = (&upstream).write_all(&line)
    {
        eprintln!("kage: rpc: {e}");
        return ExitCode::from(1);
    }
    let ended = Arc::new(AtomicBool::new(false));
    let input_ended = Arc::clone(&ended);
    thread::spawn(move || {
        if io::copy(&mut io::stdin().lock(), &mut &upstream).is_ok() {
            input_ended.store(true, Ordering::SeqCst);
        }
        let _ = upstream.shutdown(Shutdown::Write);
    });
    let mut stdout = io::stdout().lock();
    let mut buffer = vec![0; 64 * 1024];
    let mut reader = stream;
    let output = loop {
        match reader.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if let Err(e) = stdout.write_all(&buffer[..n]).and_then(|()| stdout.flush()) {
                    break Err(e);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => break Err(e),
        }
    };
    if output.is_err() {
        let _ = stream.shutdown(Shutdown::Both);
        return ExitCode::from(1);
    }
    // The host closes the connection only after the client's input ended,
    // unless the host itself went away.
    if ended.load(Ordering::SeqCst) {
        ExitCode::SUCCESS
    } else {
        eprintln!("kage: rpc: the shared host closed the connection");
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited(code: &str) -> Child {
        let mut child = Command::new("sh").args(["-c", code]).spawn().unwrap();
        child.wait().unwrap();
        child
    }

    #[test]
    fn a_clean_exit_with_nothing_published_is_a_refusal_only_on_the_second_sighting() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = exited("exit 0");
        let mut told = None;
        assert!(!refused_the_lock(dir.path(), Some(&mut child), &mut told));
        assert!(told.is_some());
        assert!(!refused_the_lock(dir.path(), Some(&mut child), &mut told));
        thread::sleep(REFUSAL_GAP);
        assert!(refused_the_lock(dir.path(), Some(&mut child), &mut told));
    }

    #[test]
    fn a_socket_showing_up_between_sightings_disarms_the_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = exited("exit 0");
        let mut told = None;
        assert!(!refused_the_lock(dir.path(), Some(&mut child), &mut told));
        let _listener = std::os::unix::net::UnixListener::bind(dir.path().join(SOCKET)).unwrap();
        thread::sleep(REFUSAL_GAP);
        assert!(!refused_the_lock(dir.path(), Some(&mut child), &mut told));
        assert!(told.is_none());
    }

    #[test]
    fn a_failed_or_missing_child_is_never_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let mut failed = exited("exit 3");
        let mut told = None;
        thread::sleep(REFUSAL_GAP);
        assert!(!refused_the_lock(dir.path(), Some(&mut failed), &mut told));
        assert!(!refused_the_lock(dir.path(), None, &mut told));
    }
}
