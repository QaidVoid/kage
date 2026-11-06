//! The shared host process: a single instance, its ready record, idle exit
//! and graceful shutdown, run as the real binary in isolated directories.

#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(20);

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
