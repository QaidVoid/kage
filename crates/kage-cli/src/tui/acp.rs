//! The ACP transport: `kage rpc` as a child process, ACP frames as
//! newline-delimited JSON over its pipes.
//!
//! A reader thread turns the child's stdout lines into [`Event`]s and
//! a writer thread drains [`Frame`]s into its stdin, so the [`App`]
//! treats the connection as one more event source. The child's stderr
//! is dropped: it would scribble over the raw terminal. The child is
//! this
//! same `kage` binary, so the TUI and its engine share a build;
//! [`spawn`] runs another program instead, which the tests use. On
//! Unix `kage rpc` relays to the shared per-user host, so sessions and
//! running turns outlive the TUI until the host goes idle. The child
//! is killed when the transport drops.
//!
//! The wiring into the App comes with the host-by-default switch; the
//! tests drive the transport until then.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use kage_client::Frame;

use super::mpsc;

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
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot spawn {}: {error}", program.display()))?;
    let stdin = child.stdin.take().expect("spawned with piped stdin");
    let stdout = child.stdout.take().expect("spawned with piped stdout");
    let (queue, reader) = mpsc::channel::<Frame>();
    let shared = Arc::new(Shared {
        writer: Mutex::new(Some(queue)),
        child: Mutex::new(Some(child)),
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
    Ok(transport)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Duration;

    use kage_client::RequestId;

    /// A child that echoes every line back and then ignores stdin
    /// EOF, standing in for `kage rpc`. Hanging past EOF is the point:
    /// the close the test sees after a drop can only come from the
    /// drop killing the child.
    fn echo_child(dir: &Path) -> PathBuf {
        let script = dir.join("echo-kage-rpc");
        std::fs::write(
            &script,
            "#!/bin/sh\nwhile read -r line; do printf '%s\\n' \"$line\"; done\nwhile :; do sleep 0.1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// A fresh directory for the stand-in child.
    fn scratch() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("kage-acp-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn frames_round_trip_through_the_child() {
        let dir = scratch();
        let (events, received) = mpsc::channel();
        let transport = spawn(&echo_child(&dir), &[], events).unwrap();
        transport.send(Frame::Request {
            id: RequestId::Number(1),
            method: "initialize".to_owned(),
            params: serde_json::json!({"protocolVersion": 1}),
        });
        match received.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::Frame(Frame::Request { method, .. })) => {
                assert_eq!(method, "initialize");
            }
            other => panic!("expected the echo, got {other:?}"),
        }
        drop(transport);
        // The child ignores stdin EOF, so this close can only be the
        // drop's kill.
        loop {
            match received.recv_timeout(Duration::from_secs(5)) {
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
}
