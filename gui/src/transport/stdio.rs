//! The stdio transport: `kage rpc` as a child process, ACP frames as
//! newline-delimited JSON over its pipes.
//!
//! A reader thread turns the child's stdout lines into [`Event`]s; a
//! writer thread drains a channel of outgoing frames into the child's
//! stdin. The child is killed when the transport drops, so closing the
//! window never leaves an engine behind. `Config::program` may be a
//! path or a bare name resolved on `PATH`; the shell passes an
//! override for tests and for running a locally built engine.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use kage_client::{Frame, unquote_and_trim};

use super::{Event, EventSender, Link, State, Transport};

/// What to spawn for `kage rpc`.
#[derive(Debug, Clone)]
pub struct Config {
    /// A path, or a bare name resolved on `PATH`.
    pub program: String,
    /// The child's arguments.
    pub args: Vec<String>,
}

impl Config {
    /// The default engine launch: `kage rpc`.
    #[must_use]
    pub fn engine() -> Self {
        Self {
            program: "kage".to_owned(),
            args: vec!["rpc".to_owned()],
        }
    }
}

/// Everything the threads and the handle share.
#[derive(Default)]
struct Shared {
    /// The writer thread's queue, absent before spawn and after the
    /// child is gone.
    writer: Mutex<Option<std::sync::mpsc::Sender<Frame>>>,
    /// The child, so a drop can kill it.
    child: Mutex<Option<Child>>,
}

/// The stdio transport to a spawned `kage rpc`.
#[derive(Clone)]
pub struct StdioTransport {
    config: Config,
    shared: Arc<Shared>,
}

impl StdioTransport {
    /// A transport for `config`; nothing spawns until
    /// [`Transport::start`].
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            shared: Arc::default(),
        }
    }

    /// Kills the child and releases the writer queue.
    fn shutdown(&self) {
        *self.shared.writer.lock().unwrap() = None;
        if let Some(mut child) = self.shared.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Resolves `program` to an executable path: quotes and spaces from
/// a pasted value go first, then a program containing a path
/// separator is used as is, and a bare name is looked up on `PATH`
/// with the executable bit required.
#[cfg(unix)]
fn resolve(program: &str) -> Option<PathBuf> {
    let program = unquote_and_trim(program);
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// The Windows form of [`resolve`]: both separators mark a path, and
/// a bare name probes the plain file, `.exe`, `.bat` and `.cmd`
/// against every `PATH` directory, one directory at a time.
#[cfg(windows)]
fn resolve(program: &str) -> Option<PathBuf> {
    let program = unquote_and_trim(program);
    if program.is_empty() {
        return None;
    }
    if program.contains('\\') || program.contains('/') {
        return Some(PathBuf::from(program));
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for extension in ["", ".exe", ".bat", ".cmd"] {
            let candidate = dir.join(format!("{program}{extension}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// The refusal for a program [`resolve`] could not find: a path
/// input names the missing path, a bare name the `PATH` search. Both
/// point at `--rpc-bin`.
fn refusal(program: &str) -> String {
    let program = unquote_and_trim(program);
    if program.contains('/') || program.contains('\\') {
        format!("{program} does not exist; pass --rpc-bin to point at the engine")
    } else {
        format!("no {program} on PATH; pass --rpc-bin to point at the engine")
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The next line of a child's stdout, trailing newlines trimmed.
/// `None` on end of stream or a read error.
fn next_line(reader: &mut BufReader<std::process::ChildStdout>) -> Option<String> {
    let mut line = String::new();
    loop {
        match reader.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => {
                while line.ends_with('\n') || line.ends_with('\r') {
                    line.pop();
                }
                return Some(line);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}

impl Transport for StdioTransport {
    fn link(&self) -> Link {
        Link {
            name: "kage rpc",
            detail: "stdio \u{b7} local".to_owned(),
        }
    }

    fn start(&mut self, events: EventSender) {
        let Some(program) = resolve(&self.config.program) else {
            let _ =
                events.send_blocking(Event::State(State::Refused(refusal(&self.config.program))));
            return;
        };
        let mut child = Err(std::io::Error::other("not spawned"));
        for attempt in 0..5 {
            child = Command::new(&program)
                .args(&self.config.args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn();
            match &child {
                Ok(_) => break,
                // A binary written moments ago can still be held open
                // for write by another thread; ETXTBSY clears at once.
                Err(error) if error.raw_os_error() == Some(26) && attempt < 4 => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                let _ = events.send_blocking(Event::State(State::Refused(format!(
                    "cannot spawn {}: {error}",
                    program.display()
                ))));
                return;
            }
        };
        let stdin = child.stdin.take().expect("spawned with piped stdin");
        let stdout = child.stdout.take().expect("spawned with piped stdout");
        *self.shared.child.lock().unwrap() = Some(child);

        let (writer, reader) = std::sync::mpsc::channel::<Frame>();
        *self.shared.writer.lock().unwrap() = Some(writer);

        let _ = events.send_blocking(Event::State(State::Connecting));
        thread::Builder::new()
            .name("kage-stdio-writer".to_owned())
            .spawn(move || {
                let mut stdin = stdin;
                for frame in reader {
                    let line = serde_json::to_string(&frame.to_value()).expect("frame serializes");
                    if writeln!(stdin, "{line}").is_err() || stdin.flush().is_err() {
                        break;
                    }
                }
            })
            .expect("writer thread spawns");

        thread::Builder::new()
            .name("kage-stdio-reader".to_owned())
            .spawn(move || {
                let _ = events.send_blocking(Event::State(State::Connected));
                let mut reader = BufReader::new(stdout);
                while let Some(line) = next_line(&mut reader) {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    let Some(frame) = Frame::parse(&value) else {
                        continue;
                    };
                    if events.send_blocking(Event::Frame(frame)).is_err() {
                        break;
                    }
                }
                let _ = events.send_blocking(Event::State(State::Closed));
            })
            .expect("reader thread spawns");
    }

    fn send(&self, frame: Frame) {
        let writer = self.shared.writer.lock().unwrap().clone();
        if let Some(writer) = writer {
            let _ = writer.send(frame);
        }
    }

    fn close(&self) {
        self.shutdown();
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use kage_client::{Client, Frame, PromptOutcome};

    use super::{Config, StdioTransport, refusal, resolve};
    use crate::transport::{Event, State, Transport};

    /// A stub engine: answers whatever request arrives first with an
    /// initialize result, the next with a session id, the next with a
    /// prompt stop reason, always echoing the id it read.
    const STUB: &str = r#"#!/bin/sh
read line
id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"steer":true},"agentInfo":{"name":"stub","version":"0.1.0"}}}\n' "$id"
read line
id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"stub-1"}}\n' "$id"
read line
id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
"#;

    /// Writes the stub script and returns its path.
    fn stub_script() -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.subsec_nanos())
            .unwrap_or(0);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "kage-desktop-stub-{}-{unique}.sh",
            std::process::id()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(STUB.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// The next frame event, skipping state changes, with a deadline.
    fn next_frame(rx: &async_channel::Receiver<Event>) -> Frame {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "the stub never answered");
            match rx.try_recv() {
                Ok(Event::Frame(frame)) => return frame,
                Ok(Event::State(_)) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(other) => panic!("event stream broke: {other}"),
            }
        }
    }

    /// The next state event, skipping frames, with a deadline.
    fn next_state(rx: &async_channel::Receiver<Event>) -> State {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "no state event arrived");
            match rx.try_recv() {
                Ok(Event::State(state)) => return state,
                Ok(Event::Frame(_)) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(other) => panic!("event stream broke: {other}"),
            }
        }
    }

    #[test]
    fn a_prompt_reaches_the_stub_child_and_its_reply_lands_in_the_client() {
        let script = stub_script();
        let (events, rx) = async_channel::unbounded();
        let mut transport = StdioTransport::new(Config {
            program: script.display().to_string(),
            args: vec![],
        });
        transport.start(events);

        // The link comes up before any frame can flow; a refusal here
        // means the child could not spawn at all.
        assert_eq!(
            next_state(&rx),
            State::Connecting,
            "spawn refused: check stderr"
        );
        assert_eq!(next_state(&rx), State::Connected);

        let mut client = Client::new();
        client.initialize(Default::default(), None);
        for frame in client.take_outgoing() {
            transport.send(frame);
        }
        client.handle(next_frame(&rx));
        assert_eq!(
            client
                .state()
                .agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("stub"),
            "the initialize answer came through the pipes"
        );

        client.new_session("/w", &[]);
        for frame in client.take_outgoing() {
            transport.send(frame);
        }
        client.handle(next_frame(&rx));
        let session_id = client
            .state()
            .sessions
            .keys()
            .next()
            .cloned()
            .expect("the stub created a session");

        assert_eq!(
            client.prompt(
                &session_id,
                vec![kage_client::wire::ContentBlock::text("fix it")]
            ),
            PromptOutcome::Sent { request_id: 3 }
        );
        for frame in client.take_outgoing() {
            transport.send(frame);
        }
        client.handle(next_frame(&rx));

        let session = client.state().session(&session_id).unwrap();
        assert!(!session.running, "the prompt answer ended the run");
        assert_eq!(
            session.last_stop,
            Some(kage_client::wire::StopReason::EndTurn),
            "the stub echoed the prompt's id back, so the prompt frame went out"
        );

        transport.close();
        assert_eq!(next_state(&rx), State::Closed);
        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn a_missing_program_is_refused_not_retried() {
        let (events, rx) = async_channel::unbounded();
        let mut transport = StdioTransport::new(Config {
            program: "/nonexistent/kage-desktop-nothing".to_owned(),
            args: vec![],
        });
        transport.start(events);
        assert!(matches!(
            next_state(&rx),
            State::Refused(reason) if reason.contains("cannot spawn")
        ));
    }

    #[test]
    fn the_refusal_tells_a_missing_path_from_a_missing_bare_name() {
        assert!(
            refusal("\"C:\\tools\\kage.exe\"").contains("does not exist"),
            "a quoted paste is a path, not a PATH search"
        );
        assert!(refusal("  /opt/kage  ").contains("does not exist"));
        assert!(refusal("kage").contains("on PATH"));
    }

    /// A fresh directory under the temp dir.
    fn unique_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "kage-desktop-resolve-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn resolve_unquotes_a_pasted_path_before_the_lookup() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_dir("unix");
        let program = dir.join("käge");
        std::fs::write(&program, b"").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pasted = format!("\"{}\"", program.display());
        assert_eq!(resolve(&pasted), Some(program.clone()));
        assert_eq!(
            resolve(&program.display().to_string()),
            Some(program.clone())
        );
        assert_eq!(
            resolve("\"/nonexistent/kage-desktop-nothing\""),
            Some(PathBuf::from("/nonexistent/kage-desktop-nothing")),
            "a quoted path routes to the spawn refusal, not the PATH one"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn resolve_accepts_quoted_backslash_and_forward_slash_paths() {
        let dir = unique_dir("windows");
        let program = dir.join("kage.exe");
        std::fs::write(&program, b"MZ").unwrap();
        let pasted = format!("\"{}\"", program.display());
        assert_eq!(resolve(&pasted), Some(program.clone()));
        assert_eq!(
            resolve(&program.display().to_string()),
            Some(program.clone())
        );
        let forward = format!("{}/kage.exe", dir.display().to_string().replace('\\', "/"));
        assert_eq!(resolve(&forward), Some(program));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn resolve_probes_the_executable_extensions_for_a_bare_name() {
        let dir = unique_dir("windows-path");
        std::fs::write(dir.join("kage.exe"), b"MZ").unwrap();
        let previous = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", &dir) };
        let found = resolve("kage");
        let missing = resolve("kage-desktop-nothing");
        match previous {
            Some(previous) => unsafe { std::env::set_var("PATH", previous) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        assert_eq!(found, Some(dir.join("kage.exe")));
        assert_eq!(missing, None, "an absent bare name stays unresolved");
        let _ = std::fs::remove_dir_all(dir);
    }
}
