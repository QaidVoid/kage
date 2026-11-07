//! The shared host process: a single instance, its ready record, idle exit
//! and graceful shutdown, run as the real binary in isolated directories.

#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(20);

/// A TCP port that was free a moment ago, for a serve that binds it
/// right after.
fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills a serve when the test ends, however it ends: a serve has no
/// idle exit, so a failing assert would otherwise orphan it.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Forwards a child's stderr lines to a channel, so a test waits for
/// one bounded instead of blocking in `read_line`.
fn stderr_lines(stderr: BufReader<std::process::ChildStderr>) -> mpsc::Receiver<String> {
    let (send, recv) = mpsc::channel();
    thread::spawn(move || {
        for line in stderr.lines() {
            if send.send(line.unwrap_or_default()).is_err() {
                break;
            }
        }
    });
    recv
}

/// Waits bounded for a stderr line carrying `needle`.
fn wait_for_line(lines: &mpsc::Receiver<String>, needle: &str, what: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        match lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) if line.contains(needle) => return,
            Ok(_) => {}
            Err(e) => panic!("{what}: {e}"),
        }
    }
}

/// Isolated XDG directories for one test's hosts.
struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn runtime(&self) -> PathBuf {
        self.dir.path().join("runtime/kage")
    }

    fn host(&self, idle_secs: u64) -> Child {
        self.host_with(&["--idle-secs", &idle_secs.to_string()])
    }

    /// `kage host run` with `args`.
    fn host_with(&self, args: &[&str]) -> Child {
        let root = self.dir.path();
        Command::new(env!("CARGO_BIN_EXE_kage"))
            .args(["host", "run"])
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root)
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// `kage serve` on `port`, with its stderr piped so a test can
    /// read its startup lines and know when it holds the instance
    /// lock.
    fn serve(&self, port: u16) -> (Child, BufReader<std::process::ChildStderr>) {
        let root = self.dir.path();
        let mut child = Command::new(env!("CARGO_BIN_EXE_kage"))
            .args(["serve", "--host", "127.0.0.1", "--port", &port.to_string()])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root)
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = BufReader::new(child.stderr.take().unwrap());
        (child, stderr)
    }

    /// `kage rpc` as an editor or the desktop runs it, with a short idle
    /// timeout for any host it starts.
    fn rpc(&self, extra: &[(&str, &str)]) -> Rpc {
        let root = self.dir.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_kage"));
        command
            .arg("rpc")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root)
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("KAGE_HOST_IDLE_SECS", "1")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in extra {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Rpc {
            child,
            output,
            next: 0,
        }
    }

    /// Runs `kage` with `args` in this environment and waits for it.
    fn kage(&self, args: &[&str]) -> std::process::Output {
        let root = self.dir.path();
        Command::new(env!("CARGO_BIN_EXE_kage"))
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root)
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// A standalone TUI under `script`, which gives it a pty so the
    /// terminal enters raw mode and the App idles for input instead of
    /// exiting at once. The returned writer feeds the pty, so a test
    /// can type into the TUI; keep it open until the TUI is done, as
    /// an end of input ends the App. `extra` carries the environment,
    /// notably a provider key so first-run onboarding does not wait on
    /// the pty.
    fn tui(&self, extra: &[(&str, &str)]) -> (Child, std::process::ChildStdin) {
        let root = self.dir.path();
        let mut child = Command::new("script")
            .arg("-qec")
            .arg(format!("{} --standalone", env!("CARGO_BIN_EXE_kage")))
            .arg("/dev/null")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("HOME", root)
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("KAGE_TEST_TUI", root)
            .envs(extra.iter().copied())
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let keys = child.stdin.take().unwrap();
        (child, keys)
    }

    /// The standalone TUI's process, found by its command line and its
    /// marker environment variable, so a parallel test's or a stray
    /// process's argv cannot match.
    fn kage_pid(&self) -> u32 {
        let mine = format!("{}\0--standalone\0", env!("CARGO_BIN_EXE_kage"));
        let marker = format!("KAGE_TEST_TUI={}", self.dir.path().display());
        let deadline = Instant::now() + WAIT;
        loop {
            let found = std::fs::read_dir("/proc").ok().and_then(|entries| {
                entries.filter_map(Result::ok).find_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    if !name.chars().all(|c| c.is_ascii_digit()) {
                        return None;
                    }
                    std::fs::read(format!("/proc/{name}/cmdline"))
                        .ok()
                        .filter(|cmdline| cmdline.starts_with(mine.as_bytes()))
                        .and_then(|_| std::fs::read(format!("/proc/{name}/environ")).ok())
                        .filter(|environ| {
                            environ
                                .split(|b| *b == 0)
                                .any(|var| var == marker.as_bytes())
                        })
                        .map(|_| name.parse().unwrap())
                })
            });
            if let Some(pid) = found {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "the standalone TUI never started"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A session file the TUI's private host recorded, once its session
    /// opened.
    fn session_file(&self) -> PathBuf {
        let dir = self.dir.path().join("data/kage/sessions");
        let deadline = Instant::now() + WAIT;
        loop {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.filter_map(Result::ok) {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "jsonl") {
                        return path;
                    }
                }
            }
            assert!(Instant::now() < deadline, "the TUI never opened a session");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn record(&self) -> Option<Value> {
        serde_json::from_slice(&std::fs::read(self.runtime().join("host.json")).ok()?).ok()
    }

    /// The record of the host with `pid`, once it is ready for clients.
    fn ready(&self, pid: u32) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(record) = self.record().filter(|record| record["pid"] == pid) {
                return record;
            }
            assert!(Instant::now() < deadline, "the host never became ready");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn connect(&self) -> UnixStream {
        UnixStream::connect(self.runtime().join("host.sock")).unwrap()
    }
}

/// Stops the TUI and its pty, without caring how: a measured or
/// probed process may leave with any status.
fn stop_tui(pid: u32, mut tui: Child) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    );
    let _ = tui.wait();
}

/// A `kage rpc` process and its stdio.
struct Rpc {
    child: Child,
    output: BufReader<std::process::ChildStdout>,
    next: u64,
}

impl Rpc {
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        message["params"] = params;
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        loop {
            let mut line = String::new();
            assert!(self.output.read_line(&mut line).unwrap() > 0, "rpc ended");
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply["id"] == id {
                return reply;
            }
        }
    }

    /// Send a request and answer each `_kage/ui/dialog` the agent asks
    /// meanwhile with `answer`, returning the reply and the dialogs seen.
    fn request_answering(
        &mut self,
        method: &str,
        params: &Value,
        answer: &Value,
    ) -> (Value, Vec<Value>) {
        self.next += 1;
        let id = self.next;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.child.stdin.as_mut().unwrap(), "{message}").unwrap();
        let mut dialogs = Vec::new();
        loop {
            let mut line = String::new();
            assert!(self.output.read_line(&mut line).unwrap() > 0, "rpc ended");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["method"] == "_kage/ui/dialog" {
                dialogs.push(message["params"].clone());
                let reply = json!({"jsonrpc": "2.0", "id": message["id"], "result": answer});
                writeln!(self.child.stdin.as_mut().unwrap(), "{reply}").unwrap();
            } else if message["id"] == id {
                return (message, dialogs);
            }
        }
    }

    /// Sends a request and returns the params of the first `_kage/ui/dialog`
    /// it asks, leaving both open.
    fn request_leaving_a_dialog(&mut self, method: &str, params: &Value) -> Value {
        self.next += 1;
        let id = self.next;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.child.stdin.as_mut().unwrap(), "{message}").unwrap();
        loop {
            let mut line = String::new();
            assert!(self.output.read_line(&mut line).unwrap() > 0, "rpc ended");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["method"] == "_kage/ui/dialog" {
                return message["params"].clone();
            }
        }
    }

    /// The params of the next notification named `method`.
    fn notification(&mut self, method: &str) -> Value {
        loop {
            let mut line = String::new();
            assert!(self.output.read_line(&mut line).unwrap() > 0, "rpc ended");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["method"] == method {
                return message["params"].clone();
            }
        }
    }

    fn close(mut self) -> std::process::ExitStatus {
        drop(self.child.stdin.take());
        exit_within(&mut self.child, WAIT)
    }
}

fn exit_within(child: &mut Child, limit: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "the host did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn terminate(child: &mut Child) -> std::process::ExitStatus {
    let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    exit_within(child, WAIT)
}

/// Replaces `path` through a sibling file, so a reload check that reads it
/// meanwhile never sees a truncated file.
fn replace(path: &std::path::Path, text: &str) {
    let staged = path.with_extension("kage-new");
    std::fs::write(&staged, text).unwrap();
    std::fs::rename(staged, path).unwrap();
}

fn initialize(stream: &UnixStream) -> Value {
    let mut writer = stream.try_clone().unwrap();
    writeln!(
        writer,
        "{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn a_host_serves_acp_clients_records_itself_and_cleans_up() {
    let env = Env::new();
    let mut host = env.host(60);
    let record = env.ready(host.id());
    assert_eq!(record["protocol"], 1);
    assert!(
        record["version"]
            .as_str()
            .is_some_and(|version| version.starts_with(env!("CARGO_PKG_VERSION"))),
        "{record}"
    );
    assert_eq!(
        record["socket"].as_str().map(PathBuf::from),
        Some(env.runtime().join("host.sock"))
    );
    let client = env.connect();
    let reply = initialize(&client);
    assert_eq!(reply["result"]["agentInfo"]["name"], "kage", "{reply}");
    drop(client);
    assert!(terminate(&mut host).success());
    assert!(env.record().is_none());
    assert!(!env.runtime().join("host.sock").exists());
}

#[test]
fn only_one_host_runs_at_a_time() {
    let env = Env::new();
    let mut first = env.host(60);
    env.ready(first.id());
    let mut second = env.host(60);
    assert!(exit_within(&mut second, WAIT).success());
    assert!(first.try_wait().unwrap().is_none());
    assert_eq!(env.record().unwrap()["pid"], first.id());
    assert_eq!(initialize(&env.connect())["id"], 1);
    assert!(terminate(&mut first).success());
}

#[test]
fn a_serve_and_a_host_do_not_run_alongside_each_other() {
    let env = Env::new();
    let (serve, stderr) = env.serve(free_port());
    let mut serve = KillOnDrop(serve);
    let lines = stderr_lines(stderr);
    // The token line comes after the lock is taken and the engine
    // started, so serve is holding the instance lock from here on.
    wait_for_line(&lines, "token file", "serve did not start in time");

    // A host run refuses at once, publishes nothing, and leaves serve
    // alone.
    let host = env.kage(&["host", "run", "--idle-secs", "1"]);
    assert!(host.status.success(), "{:?}", host.stderr);
    assert!(
        String::from_utf8_lossy(&host.stderr).contains("already running"),
        "{:?}",
        host.stderr
    );
    assert!(
        env.record().is_none(),
        "the refused host published a record"
    );
    assert!(
        serve.0.try_wait().unwrap().is_none(),
        "serve exited while holding the lock"
    );

    // A second serve refuses too, on a free port, so only the lock can
    // be the reason.
    let second = env.kage(&[
        "serve",
        "--host",
        "127.0.0.1",
        "--port",
        &free_port().to_string(),
    ]);
    assert!(!second.status.success(), "{:?}", second.stderr);
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already running"),
        "{:?}",
        second.stderr
    );
    assert!(
        serve.0.try_wait().unwrap().is_none(),
        "serve exited while holding the lock"
    );
}

#[test]
fn a_serve_refuses_fast_while_a_host_holds_the_lock() {
    let env = Env::new();
    let mut host = env.host(60);
    env.ready(host.id());
    let started = Instant::now();
    let second = env.kage(&[
        "serve",
        "--host",
        "127.0.0.1",
        "--port",
        &free_port().to_string(),
    ]);
    assert!(!second.status.success(), "{:?}", second.stderr);
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already running"),
        "{:?}",
        second.stderr
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the refusal waited out the takeover window"
    );
    assert!(terminate(&mut host).success());
}

#[test]
fn a_relay_falls_back_fast_while_a_serve_holds_the_lock() {
    let env = Env::new();
    let (serve, stderr) = env.serve(free_port());
    // Bound for the whole test: its Drop kills the serve.
    let _serve = KillOnDrop(serve);
    let lines = stderr_lines(stderr);
    wait_for_line(&lines, "token file", "serve did not start in time");

    let started = Instant::now();
    let mut relay = env.rpc(&[]);
    assert_eq!(
        relay.request("initialize", json!({"protocolVersion": 1}))["result"]["agentInfo"]["name"],
        "kage"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the relay waited out the whole takeover window"
    );
    // The private engine publishes no host record: the serve keeps
    // holding the lock.
    assert!(env.record().is_none());
}

#[test]
fn a_connected_client_keeps_an_idle_host_alive_until_it_leaves() {
    let env = Env::new();
    let mut host = env.host(1);
    env.ready(host.id());
    let client = env.connect();
    initialize(&client);
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        host.try_wait().unwrap().is_none(),
        "the host exited with a client connected"
    );
    drop(client);
    assert!(exit_within(&mut host, WAIT).success());
    assert!(env.record().is_none());
}

#[test]
fn a_stale_socket_and_record_do_not_block_a_new_host() {
    let env = Env::new();
    std::fs::create_dir_all(env.runtime()).unwrap();
    drop(std::os::unix::net::UnixListener::bind(env.runtime().join("host.sock")).unwrap());
    std::fs::write(
        env.runtime().join("host.json"),
        json!({"pid": 1, "version": "0.0.0", "protocol": 1, "socket": "/nowhere", "started": "2020-01-01T00:00:00Z"})
            .to_string(),
    )
    .unwrap();
    let mut host = env.host(60);
    env.ready(host.id());
    assert_eq!(initialize(&env.connect())["id"], 1);
    assert!(terminate(&mut host).success());
}

#[test]
fn hosts_started_together_leave_exactly_one_serving() {
    let env = Env::new();
    let mut hosts: Vec<Child> = (0..4).map(|_| env.host(60)).collect();
    let deadline = Instant::now() + WAIT;
    let serving = loop {
        if let Some(pid) = env.record().and_then(|record| record["pid"].as_u64()) {
            break pid;
        }
        assert!(Instant::now() < deadline, "no host became ready");
        std::thread::sleep(Duration::from_millis(20));
    };
    for host in &mut hosts {
        if u64::from(host.id()) != serving {
            assert!(exit_within(host, WAIT).success());
        }
    }
    assert_eq!(initialize(&env.connect())["id"], 1);
    for host in &mut hosts {
        if u64::from(host.id()) == serving {
            assert!(terminate(host).success());
        }
    }
}

#[test]
fn a_terminated_host_disconnects_its_clients() {
    let env = Env::new();
    let mut host = env.host(60);
    env.ready(host.id());
    let client = env.connect();
    initialize(&client);
    assert!(terminate(&mut host).success());
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut &client, &mut rest).unwrap();
    assert!(env.record().is_none());
}

#[test]
fn a_host_started_during_shutdown_takes_over() {
    let env = Env::new();
    let mut old = env.host(60);
    env.ready(old.id());
    let pid = nix::unistd::Pid::from_raw(i32::try_from(old.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    let deadline = Instant::now() + WAIT;
    while env.runtime().join("host.sock").exists() {
        assert!(Instant::now() < deadline, "the old host kept its socket");
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut new = env.host(60);
    env.ready(new.id());
    assert!(exit_within(&mut old, WAIT).success());
    assert_eq!(initialize(&env.connect())["id"], 1);
    assert!(terminate(&mut new).success());
}

#[test]
fn rpc_relays_share_one_host_that_exits_after_they_leave() {
    let env = Env::new();
    let mut first = env.rpc(&[]);
    assert_eq!(
        first.request("initialize", json!({"protocolVersion": 1}))["result"]["agentInfo"]["name"],
        "kage"
    );
    let host = env.record().unwrap()["pid"].as_u64().unwrap();
    assert_ne!(host, u64::from(first.child.id()));
    let created = first.request(
        "session/new",
        json!({"cwd": env.dir.path(), "mcpServers": []}),
    );
    assert!(created["result"]["sessionId"].is_string(), "{created}");
    let mut second = env.rpc(&[]);
    second.request("initialize", json!({"protocolVersion": 1}));
    assert_eq!(env.record().unwrap()["pid"], host);
    assert!(first.close().success());
    assert!(second.close().success());
    let deadline = Instant::now() + WAIT;
    while env.record().is_some() {
        assert!(Instant::now() < deadline, "the idle host did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_session_starts_its_servers_in_the_environment_of_its_client() {
    let env = Env::new();
    let out = env.dir.path().join("seen");
    let server = |name: &str| {
        json!({
            "name": name,
            "command": "sh",
            "args": ["-c", format!("printf %s \"$KAGE_TEST_CLIENT\" > {}.{name}", out.display())],
            "env": [],
        })
    };
    // Started apart from either client, so neither value is the host's.
    let mut host = env.host(60);
    env.ready(host.id());
    let mut first = env.rpc(&[("KAGE_TEST_CLIENT", "first")]);
    first.request("initialize", json!({"protocolVersion": 1}));
    let mut second = env.rpc(&[("KAGE_TEST_CLIENT", "second")]);
    second.request("initialize", json!({"protocolVersion": 1}));
    for (rpc, name) in [(&mut first, "a"), (&mut second, "b")] {
        let created = rpc.request(
            "session/new",
            json!({"cwd": env.dir.path(), "mcpServers": [server(name)]}),
        );
        assert!(created["result"]["sessionId"].is_string(), "{created}");
    }
    let seen = |name: &str| std::fs::read_to_string(format!("{}.{name}", out.display())).unwrap();
    assert_eq!(seen("a"), "first");
    assert_eq!(seen("b"), "second");
    assert!(first.close().success());
    assert!(second.close().success());
    assert!(terminate(&mut host).success());
}

#[test]
fn host_management_survives_broken_user_and_project_configs() {
    for relative in ["config/kage/config.toml", ".kage/config.toml"] {
        let env = Env::new();
        let mut host = env.host(60);
        env.ready(host.id());
        let log = env.dir.path().join("state/kage/host.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "host repair log\n").unwrap();
        let config = env.dir.path().join(relative);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "[broken\n").unwrap();

        let status = env.kage(&["host", "status"]);
        let logs = env.kage(&["host", "logs"]);
        let stop = env.kage(&["host", "stop"]);
        if stop.status.success() {
            assert!(exit_within(&mut host, WAIT).success());
        } else {
            terminate(&mut host);
        }
        assert!(
            status.status.success(),
            "{relative}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(
            String::from_utf8_lossy(&status.stdout).contains(&host.id().to_string()),
            "status must identify the running host"
        );
        assert!(logs.status.success(), "{relative}: {logs:?}");
        assert_eq!(logs.stdout, b"host repair log\n", "{relative}");
        assert!(stop.status.success(), "{relative}: {stop:?}");
        let run = env.kage(&["host", "run", "--idle-secs", "1"]);
        assert!(
            !run.status.success(),
            "host run must still validate {relative}"
        );
    }
}

#[test]
fn status_and_stop_manage_the_running_host() {
    let env = Env::new();
    let idle = env.kage(&["host", "status"]);
    assert_eq!(idle.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&idle.stdout).contains("no shared host is running"));

    let mut host = env.host(60);
    env.ready(host.id());
    let running = env.kage(&["host", "status"]);
    assert!(running.status.success());
    let shown = String::from_utf8_lossy(&running.stdout);
    assert!(
        shown.contains(&format!("pid      {}", host.id())),
        "{shown}"
    );

    let stopped = env.kage(&["host", "stop"]);
    assert!(stopped.status.success());
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("stopped the shared host"));
    assert!(exit_within(&mut host, WAIT).success());
    assert!(env.record().is_none());
    let again = env.kage(&["host", "stop"]);
    assert!(String::from_utf8_lossy(&again.stdout).contains("no shared host is running"));
}

#[test]
fn a_killed_hosts_record_is_never_signalled() {
    let env = Env::new();
    let mut host = env.host(60);
    env.ready(host.id());
    host.kill().unwrap();
    host.wait().unwrap();
    assert!(env.record().is_some(), "the record outlives a killed host");
    let status = env.kage(&["host", "status"]);
    assert_eq!(status.status.code(), Some(1));
    let stop = env.kage(&["host", "stop"]);
    assert!(stop.status.success());
    assert!(String::from_utf8_lossy(&stop.stdout).contains("no shared host is running"));
}

#[test]
fn a_host_started_by_a_client_logs_to_the_state_directory() {
    let env = Env::new();
    let mut rpc = env.rpc(&[]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    let logs = env.kage(&["host", "logs"]);
    assert!(logs.status.success());
    let text = String::from_utf8_lossy(&logs.stdout);
    assert!(text.contains("accepting clients"), "{text}");
    assert!(rpc.close().success());
}

#[test]
fn a_user_config_can_turn_sharing_off() {
    let env = Env::new();
    let config = env.dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"), "[host]\nshare = false\n").unwrap();
    let mut rpc = env.rpc(&[]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    assert!(env.record().is_none());
    assert!(rpc.close().success());
}

#[test]
fn the_idle_timeout_comes_from_the_user_config() {
    let env = Env::new();
    let config = env.dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        "[host]\nidle_timeout_secs = 1\n",
    )
    .unwrap();
    let mut host = env.host_with(&[]);
    env.ready(host.id());
    assert!(exit_within(&mut host, WAIT).success());
}

/// The host loads the user's init.lua for an editor's session, once per
/// project, as the TUI does.
#[test]
fn an_editor_session_runs_with_the_users_init_lua() {
    let env = Env::new();
    let config = env.dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "local ok, seen = pcall(kage.fs.read, 'loads')\n\
         local n = (ok and tonumber(seen) or 0) + 1\n\
         kage.fs.write('loads', tostring(n))\n",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut rpc = env.rpc(&[]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    for _ in 0..2 {
        let created = rpc.request(
            "session/new",
            json!({"cwd": project.path(), "mcpServers": []}),
        );
        assert!(created["result"]["sessionId"].is_string(), "{created}");
    }
    let log =
        std::fs::read_to_string(env.dir.path().join("state/kage/host.log")).unwrap_or_default();
    let loads = std::fs::read_to_string(project.path().join("loads"))
        .unwrap_or_else(|e| panic!("{e}: {log}"));
    assert_eq!(loads, "1", "two sessions in one project share one load");
    assert!(rpc.close().success());
}

/// A client drawing the plugin UI attaches to its project and gets what
/// the user's init.lua declared there.
#[test]
fn a_client_gets_the_plugin_ui_of_its_project() {
    let env = Env::new();
    let config = env.dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "kage.keymap.set('n', '<F9>', ':hello', { desc = 'greet' })\n\
         kage.set_status('mood', 'calm')\n\
         kage.opt.thinking_level = 'low'\n\
         kage.register_command({ name = 'hello', description = 'say hi', \
           handler = function() return 'hi' end })\n\
         kage.defer(function() kage.set_status('mood', 'busy') end, 1500)\n",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut rpc = env.rpc(&[]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    let ui = rpc.request(
        "_kage/ui/attach",
        json!({"cwd": project.path(), "attachId": 1}),
    );
    let ui = &ui["result"];
    let mapping = ui["keymap"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["lhs"] == "<F9>")
        .unwrap_or_else(|| panic!("{ui}"));
    assert_eq!(mapping["owner"], "init.lua");
    assert_eq!(mapping["rhs"]["kind"], "command");
    assert_eq!(ui["status"]["mood"], "calm");
    assert_eq!(ui["options"]["thinking_level"], "low");
    assert_eq!(ui["optionSources"]["thinking_level"], "lua");
    assert!(ui["highlights"]["theme"].is_string(), "{ui}");
    assert!(
        ui["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "hello" && c["description"] == "say hi"),
        "{ui}"
    );
    let update = rpc.notification("_kage/ui/update");
    assert_eq!(update["status"]["mood"], "busy", "{update}");
    assert!(
        update.get("keymap").is_none(),
        "only the status changed: {update}"
    );
    assert!(update["generation"].as_u64() > ui["generation"].as_u64());

    // An edited init.lua loads again, and the client hears of its keymap.
    std::thread::sleep(Duration::from_millis(1100));
    replace(
        &config.join("init.lua"),
        "kage.keymap.set('n', '<F10>', ':bye', { desc = 'part' })\n",
    );
    let reloaded = rpc.notification("_kage/ui/update");
    let keymap = reloaded["keymap"]
        .as_array()
        .unwrap_or_else(|| panic!("{reloaded}"));
    assert!(keymap.iter().any(|m| m["lhs"] == "<F10>"), "{reloaded}");
    assert!(!keymap.iter().any(|m| m["lhs"] == "<F9>"), "{reloaded}");
    assert!(rpc.close().success());
}

/// A plugin command runs on the host for the client that asked, and the
/// dialog it opens goes to that client.
#[test]
fn a_plugin_command_asks_its_client_through_a_dialog() {
    let env = Env::new();
    let config = env.dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "kage.register_command({ name = 'pick', aliases = { 'pk' }, description = 'pick one', \
           handler = function() \
             local fruit = kage.ui.select('Fruit', { 'apple', 'pear' }) \
             return 'picked:' .. tostring(fruit) \
           end })\n\
         kage.keymap.set('n', '<F8>', function() return 'mapped' end)\n",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut rpc = env.rpc(&[]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    let (reply, dialogs) = rpc.request_answering(
        "_kage/ui/command",
        &json!({"cwd": project.path(), "name": "pk"}),
        &json!("pear"),
    );
    assert_eq!(reply["result"]["text"], "picked:pear", "{reply}");
    assert_eq!(dialogs.len(), 1);
    assert_eq!(dialogs[0]["kind"], "ui.select");
    assert_eq!(dialogs[0]["payload"]["title"], "Fruit");
    let (dismissed, _) = rpc.request_answering(
        "_kage/ui/command",
        &json!({"cwd": project.path(), "name": "pick"}),
        &Value::Null,
    );
    assert_eq!(dismissed["result"]["text"], "picked:nil", "{dismissed}");
    let ui = rpc.request(
        "_kage/ui/attach",
        json!({"cwd": project.path(), "attachId": 1}),
    );
    let id = ui["result"]["keymap"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["lhs"] == "<F8>")
        .map(|m| m["rhs"]["id"].clone())
        .unwrap();
    let mapped = rpc.request("_kage/ui/keymap", json!({"cwd": project.path(), "id": id}));
    assert_eq!(mapped["result"]["text"], "mapped", "{mapped}");
    let missing = rpc.request(
        "_kage/ui/command",
        json!({"cwd": project.path(), "name": "nope"}),
    );
    assert!(missing["error"].is_object());
    assert!(rpc.close().success());
}

/// A plugin command the client runs for one of its sessions runs in that
/// session's plugins, and an unknown session id is refused.
#[test]
fn a_command_runs_for_the_session_its_client_names() {
    let env = Env::new();
    let config = env.dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "kage.register_command({ name = 'ask', description = 'ask me', \
           handler = function() \
             local fruit = kage.ui.select('Fruit', { 'apple', 'pear' }) \
             return 'picked:' .. tostring(fruit) \
           end })\n",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut rpc = env.rpc(&[]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    let created = rpc.request(
        "session/new",
        json!({"cwd": env.dir.path(), "mcpServers": []}),
    );
    let session = created["result"]["sessionId"].as_str().unwrap().to_owned();
    let (reply, dialogs) = rpc.request_answering(
        "_kage/ui/command",
        &json!({"cwd": project.path(), "name": "ask", "sessionId": session}),
        &json!("apple"),
    );
    assert_eq!(reply["result"]["text"], "picked:apple", "{reply}");
    assert_eq!(dialogs.len(), 1);
    assert_eq!(dialogs[0]["kind"], "ui.select");
    let unknown = rpc.request(
        "_kage/ui/command",
        json!({"cwd": project.path(), "name": "ask", "sessionId": "no-such-session"}),
    );
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    assert!(rpc.close().success());
}

/// Two clients can each wait on a dialog at once, and each answer reaches
/// the command that asked for it.
#[test]
fn dialogs_from_two_clients_wait_side_by_side() {
    let env = Env::new();
    let config = env.dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "kage.register_command({ name = 'pick', description = 'pick one', \
           handler = function() \
             local fruit = kage.ui.select('Fruit', { 'apple', 'pear' }) \
             return 'picked:' .. tostring(fruit) \
           end })\n",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    let cwd = project.path().to_path_buf();
    let asked = cwd.clone();
    let mut first = env.rpc(&[]);
    first.request("initialize", json!({"protocolVersion": 1}));
    let mut second = env.rpc(&[]);
    second.request("initialize", json!({"protocolVersion": 1}));
    let asking = std::thread::spawn(move || {
        let outcome = first.request_answering(
            "_kage/ui/command",
            &json!({"cwd": &asked, "name": "pick"}),
            &json!("apple"),
        );
        (outcome, first)
    });
    let (second_reply, second_dialogs) = second.request_answering(
        "_kage/ui/command",
        &json!({"cwd": &cwd, "name": "pick"}),
        &json!("pear"),
    );
    let ((first_reply, first_dialogs), first) = asking.join().unwrap();
    assert_eq!(
        first_reply["result"]["text"], "picked:apple",
        "{first_reply}"
    );
    assert_eq!(
        second_reply["result"]["text"], "picked:pear",
        "{second_reply}"
    );
    assert_eq!(first_dialogs.len(), 1);
    assert_eq!(second_dialogs.len(), 1);
    assert!(first.close().success());
    assert!(second.close().success());
}

/// A client that quits while a plugin waits on its dialog does not wedge
/// the host: the abandoned command ends, the relay exits, and the next
/// client still gets answers. The host leaves once its last client does.
#[test]
fn a_client_leaving_mid_dialog_does_not_wedge_the_host() {
    let env = Env::new();
    let config = env.dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "kage.register_command({ name = 'chain', description = 'two asks', \
           handler = function() \
             local a = kage.ui.select('First', { 'a', 'b' }) \
             local b = kage.ui.confirm('Second', 'sure?') \
             return 'got:' .. tostring(a) .. tostring(b) \
           end })\n\
         kage.register_command({ name = 'pick', description = 'pick one', \
           handler = function() \
             local fruit = kage.ui.select('Fruit', { 'apple', 'pear' }) \
             return 'picked:' .. tostring(fruit) \
           end })\n",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut leaving = env.rpc(&[]);
    leaving.request("initialize", json!({"protocolVersion": 1}));
    let dialog = leaving.request_leaving_a_dialog(
        "_kage/ui/command",
        &json!({"cwd": project.path(), "name": "chain"}),
    );
    assert_eq!(dialog["kind"], "ui.select", "{dialog}");
    assert!(leaving.close().success());
    let mut staying = env.rpc(&[]);
    staying.request("initialize", json!({"protocolVersion": 1}));
    let (reply, _) = staying.request_answering(
        "_kage/ui/command",
        &json!({"cwd": project.path(), "name": "pick"}),
        &json!("pear"),
    );
    assert_eq!(reply["result"]["text"], "picked:pear", "{reply}");
    assert!(staying.close().success());
    let deadline = Instant::now() + WAIT;
    while env.record().is_some() {
        assert!(Instant::now() < deadline, "the idle host did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A key mapping whose handler opens a dialog asks it of the client that
/// pressed the key.
#[test]
fn a_key_mapping_opens_a_dialog_on_its_client() {
    let env = Env::new();
    let config = env.dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "kage.keymap.set('n', '<F7>', function() \
           local fruit = kage.ui.select('Fruit', { 'apple', 'pear' }) \
           return 'picked:' .. tostring(fruit) \
         end)\n",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut rpc = env.rpc(&[]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    let ui = rpc.request(
        "_kage/ui/attach",
        json!({"cwd": project.path(), "attachId": 1}),
    );
    let id = ui["result"]["keymap"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["lhs"] == "<F7>")
        .map(|m| m["rhs"]["id"].clone())
        .unwrap();
    let (reply, dialogs) = rpc.request_answering(
        "_kage/ui/keymap",
        &json!({"cwd": project.path(), "id": id}),
        &json!("pear"),
    );
    assert_eq!(reply["result"]["text"], "picked:pear", "{reply}");
    assert_eq!(dialogs.len(), 1);
    assert_eq!(dialogs[0]["kind"], "ui.select");
    assert!(rpc.close().success());
}

/// A command a reloaded project registered runs, and the one it replaced
/// is gone.
#[test]
fn a_command_runs_after_a_reload() {
    let env = Env::new();
    let config = env.dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "kage.register_command({ name = 'first', description = 'old', \
           handler = function() return 'one' end })\n",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut rpc = env.rpc(&[]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    rpc.request(
        "_kage/ui/attach",
        json!({"cwd": project.path(), "attachId": 1}),
    );
    std::thread::sleep(Duration::from_millis(1100));
    replace(
        &config.join("init.lua"),
        "kage.register_command({ name = 'second', description = 'new', \
           handler = function() return 'two' end })\n",
    );
    rpc.notification("_kage/ui/update");
    let second = rpc.request(
        "_kage/ui/command",
        json!({"cwd": project.path(), "name": "second"}),
    );
    assert_eq!(second["result"]["text"], "two", "{second}");
    let first = rpc.request(
        "_kage/ui/command",
        json!({"cwd": project.path(), "name": "first"}),
    );
    assert!(first["error"].is_object(), "{first}");
    assert!(rpc.close().success());
}

/// A session-bound watcher keeps its runtime after the project reloads,
/// while a new session draws and invokes the replacement runtime.
#[test]
fn a_session_keeps_its_plugin_ui_after_a_project_reload() {
    let env = Env::new();
    let config = env.dir.path().join("config/kage");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "kage.register_command({ name = 'first', description = 'old', \
           handler = function() return 'old' end })\n\
         kage.keymap.set('n', '<F7>', function() return 'old-key' end)\n\
         kage.set_status('version', 'old')\n\
         kage.defer(function() kage.set_status('tick', 'old') end, 4000)\n",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pinned = env.rpc(&[]);
    pinned.request("initialize", json!({"protocolVersion": 1}));
    let opened = pinned.request("session/new", json!({"cwd": project.path()}));
    let session = opened["result"]["sessionId"].clone();
    let attached = pinned.request(
        "_kage/ui/attach",
        json!({"cwd": project.path(), "attachId": 1, "sessionId": session}),
    );
    let key = attached["result"]["keymap"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["lhs"] == "<F7>")
        .unwrap()["rhs"]["id"]
        .clone();

    let mut observer = env.rpc(&[]);
    observer.request("initialize", json!({"protocolVersion": 1}));
    observer.request(
        "_kage/ui/attach",
        json!({"cwd": project.path(), "attachId": 1}),
    );
    std::thread::sleep(Duration::from_millis(1100));
    replace(
        &config.join("init.lua"),
        "kage.register_command({ name = 'second', description = 'new', \
           handler = function() return 'new' end })\n\
         kage.keymap.set('n', '<F8>', function() return 'new-key' end)\n\
         kage.set_status('version', 'new')\n",
    );
    let reloaded = observer.notification("_kage/ui/update");
    assert_eq!(reloaded["status"]["version"], "new", "{reloaded}");
    let opened = observer.request("session/new", json!({"cwd": project.path()}));
    let new_session = opened["result"]["sessionId"].clone();
    let fresh = observer.request(
        "_kage/ui/attach",
        json!({"cwd": project.path(), "attachId": 2, "sessionId": new_session}),
    );
    assert!(
        fresh["result"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "second")
    );
    assert!(
        !fresh["result"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "first")
    );

    let update = pinned.notification("_kage/ui/update");
    assert_eq!(update["attachId"], 1);
    assert_eq!(update["status"]["version"], "old", "{update}");
    assert_eq!(update["status"]["tick"], "old", "{update}");
    assert!(update.get("keymap").is_none(), "{update}");
    let old_command = pinned.request(
        "_kage/ui/command",
        json!({"cwd": project.path(), "sessionId": session, "name": "first"}),
    );
    assert_eq!(old_command["result"]["text"], "old", "{old_command}");
    let old_key = pinned.request(
        "_kage/ui/keymap",
        json!({"cwd": project.path(), "sessionId": session, "id": key}),
    );
    assert_eq!(old_key["result"]["text"], "old-key", "{old_key}");
    let new_command = observer.request(
        "_kage/ui/command",
        json!({"cwd": project.path(), "sessionId": new_session, "name": "second"}),
    );
    assert_eq!(new_command["result"]["text"], "new", "{new_command}");
    assert!(pinned.close().success());
    assert!(observer.close().success());
}

/// A client whose `KAGE_*` configuration overrides the host cannot
/// honor runs an engine of its own: the host merged its own environment
/// at startup, so joining it would silently drop the overrides. A
/// variable that names no config key does not turn the client away.
#[test]
fn a_config_env_override_runs_a_private_engine() {
    let env = Env::new();
    let project = env.dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("secret.txt"), "contents").unwrap();
    let mut host = env.host(60);
    env.ready(host.id());

    let mut shared = env.rpc(&[("KAGE_DEBUG_KEYS", "1")]);
    shared.request("initialize", json!({"protocolVersion": 1}));
    assert_eq!(
        env.record().unwrap()["pid"],
        host.id(),
        "a non-config variable still shares the host"
    );
    assert!(shared.close().success());

    let mut private = env.rpc(&[("KAGE_PERMISSIONS__TOOLS__READ__DEFAULT", "deny")]);
    private.request("initialize", json!({"protocolVersion": 1}));
    assert_eq!(
        env.record().unwrap()["pid"],
        host.id(),
        "the override starts no second host"
    );
    let created = private.request(
        "session/new",
        json!({"cwd": project.display().to_string(), "mcpServers": []}),
    );
    let session = created["result"]["sessionId"].as_str().unwrap();
    let denied = private.request(
        "_kage/fs",
        json!({"sessionId": session, "op": "read", "path": "secret.txt"}),
    );
    assert_eq!(
        denied["error"]["code"], -32602,
        "the private engine denies the read: {denied}"
    );
    assert!(private.close().success());
    assert!(terminate(&mut host).success());
}

#[test]
fn a_standalone_rpc_starts_no_host() {
    let env = Env::new();
    let mut rpc = env.rpc(&[("KAGE_STANDALONE", "1")]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    assert!(env.record().is_none());
    assert!(rpc.close().success());
}

#[test]
fn a_relay_ends_when_its_host_dies() {
    let env = Env::new();
    let mut rpc = env.rpc(&[]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    let host = i32::try_from(env.record().unwrap()["pid"].as_i64().unwrap()).unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(host),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    assert!(!exit_within(&mut rpc.child, WAIT).success());
}

/// Private memory of `pid`, in KiB.
#[cfg(target_os = "linux")]
fn rss_anon_kib(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("RssAnon:"))
        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

/// An idle host with a client connected stays within twice the private
/// memory measured for it (2.1 MiB). Debug builds are far larger, so this
/// runs only on request against a release build.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "measures a release build: cargo test --release -p kage-cli --test shared_host -- --ignored"]
fn an_idle_host_stays_within_its_memory_budget() {
    const BUDGET_KIB: u64 = 4 * 1024;
    let env = Env::new();
    let mut host = env.host(60);
    env.ready(host.id());
    let client = env.connect();
    initialize(&client);
    std::thread::sleep(Duration::from_secs(1));
    let used = rss_anon_kib(host.id());
    eprintln!("idle host RssAnon: {used} KiB");
    drop(client);
    assert!(terminate(&mut host).success());
    assert!(
        used <= BUDGET_KIB,
        "idle host uses {used} KiB, over {BUDGET_KIB} KiB"
    );
}

/// A `kage rpc` relay only copies bytes, so it stays small.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "measures a release build: cargo test --release -p kage-cli --test shared_host -- --ignored"]
fn a_relay_stays_within_its_memory_budget() {
    const BUDGET_KIB: u64 = 2 * 1024;
    let env = Env::new();
    let mut rpc = env.rpc(&[]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    std::thread::sleep(Duration::from_secs(1));
    let used = rss_anon_kib(rpc.child.id());
    eprintln!("relay RssAnon: {used} KiB");
    assert!(rpc.close().success());
    assert!(
        used <= BUDGET_KIB,
        "relay uses {used} KiB, over {BUDGET_KIB} KiB"
    );
}

/// A standalone TUI runs a private host in its own process: a message
/// typed into the TUI is served by that host and recorded under this
/// environment's data directory while the TUI is up.
#[cfg(target_os = "linux")]
#[test]
fn a_standalone_tui_runs_a_private_host_in_its_process() {
    let env = Env::new();
    let (tui, mut keys) = env.tui(&[("OPENAI_API_KEY", "sk-standalone-test")]);
    let pid = env.kage_pid();
    // Let the App reach raw mode, then type a message: the host only
    // writes the session file once a first entry is recorded.
    std::thread::sleep(Duration::from_secs(2));
    keys.write_all(b"hi\r").unwrap();
    keys.flush().unwrap();
    let session = env.session_file();
    // A beat more proves the TUI stays up beside the session it opened.
    std::thread::sleep(Duration::from_secs(1));
    let header = std::fs::read_to_string(&session).unwrap();
    assert!(header.contains("\"session\""), "{header}");
    stop_tui(pid, tui);
    drop(keys);
}

/// A standalone TUI runs a private host in process, so it stays near
/// what the engine TUI used. The measured value locally was under half
/// the budget.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "measures a release build: cargo test --release -p kage-cli --test shared_host -- --ignored"]
fn a_standalone_tui_stays_within_its_memory_budget() {
    const BUDGET_KIB: u64 = 12 * 1024;
    let env = Env::new();
    let (tui, keys) = env.tui(&[("OPENAI_API_KEY", "sk-standalone-test")]);
    // The pty feed stays open for the whole run: its end would stop the App.
    let _keep_open = keys;
    let pid = env.kage_pid();
    // Measure an idle TUI that finished starting, pty raw mode included.
    std::thread::sleep(Duration::from_secs(3));
    let used = rss_anon_kib(pid);
    eprintln!("standalone TUI RssAnon: {used} KiB");
    stop_tui(pid, tui);
    assert!(
        used <= BUDGET_KIB,
        "standalone TUI uses {used} KiB, over {BUDGET_KIB} KiB"
    );
}

#[test]
fn a_host_that_cannot_start_lets_the_relay_fall_back_quickly() {
    let env = Env::new();
    std::fs::write(env.dir.path().join("not-a-dir"), "").unwrap();
    let runtime = env.dir.path().join("not-a-dir");
    let started = Instant::now();
    let mut rpc = env.rpc(&[("XDG_RUNTIME_DIR", runtime.to_str().unwrap())]);
    let reply = rpc.request("initialize", json!({"protocolVersion": 1}));
    assert_eq!(reply["result"]["agentInfo"]["name"], "kage", "{reply}");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "falling back took {:?}",
        started.elapsed()
    );
    assert!(rpc.close().success());
}

#[test]
fn a_stale_record_of_another_version_does_not_turn_clients_away() {
    let env = Env::new();
    std::fs::create_dir_all(env.runtime()).unwrap();
    std::fs::write(
        env.runtime().join("host.json"),
        json!({"pid": 1, "version": "0.0.1 (old)", "protocol": 1, "socket": "/nowhere", "started": "2020-01-01T00:00:00Z"})
            .to_string(),
    )
    .unwrap();
    let mut rpc = env.rpc(&[]);
    rpc.request("initialize", json!({"protocolVersion": 1}));
    let record = env.record().unwrap();
    assert!(
        record["version"]
            .as_str()
            .is_some_and(|version| version.starts_with(env!("CARGO_PKG_VERSION"))),
        "{record}"
    );
    assert!(rpc.close().success());
}
