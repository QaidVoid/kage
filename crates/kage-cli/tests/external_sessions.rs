//! External ACP sessions end to end: the real host binary, `kage rpc` relays
//! as clients, and a scripted agent in a separate process.

#![cfg(target_os = "linux")]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(20);
const AGENT: &str = "tests/fixtures/external_acp_e2e.py";

/// Isolated directories for one test, with a user config that defines the
/// external agent `fake`.
struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config/kage");
        std::fs::create_dir_all(&config).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join(AGENT);
        std::fs::write(
            config.join("config.toml"),
            format!(
                "[acp.agents.fake]\ncommand = \"python3\"\nargs = [{}]\n\n\
                 [acp.agents.fake.env]\nHISTORY_FILE = {}\n",
                serde_json::to_string(script.to_str().unwrap()).unwrap(),
                serde_json::to_string(dir.path().join("history.json").to_str().unwrap()).unwrap(),
            ),
        )
        .unwrap();
        Self { dir }
    }

    fn cwd(&self) -> String {
        self.dir.path().to_str().unwrap().to_owned()
    }

    /// A `kage rpc` relay, which starts the shared host when none runs. The
    /// host exits a second after its last client leaves.
    fn client(&self) -> Client {
        let root = self.dir.path();
        let mut child = Command::new(env!("CARGO_BIN_EXE_kage"))
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
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in output.lines() {
                let Ok(line) = line else { break };
                if send
                    .send(serde_json::from_str::<Value>(&line).unwrap())
                    .is_err()
                {
                    break;
                }
            }
        });
        Client {
            child,
            stdin,
            lines,
            held: Vec::new(),
            next: 0,
        }
    }

    fn session_file(&self, id: &str) -> PathBuf {
        self.dir
            .path()
            .join("data/kage/sessions")
            .join(format!("{id}.jsonl"))
    }
}

/// One relay and the messages it delivered that no one asked for yet.
struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<Value>,
    held: Vec<Value>,
    next: u64,
}

impl Client {
    fn write(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").unwrap();
    }

    /// Sends a request and returns its id without waiting.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "call sites build params inline"
    )]
    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let id = self.next;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "call sites build params inline"
    )]
    fn notify(&mut self, method: &str, params: Value) {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// The first held or arriving message `wanted` accepts.
    fn wait_for(&mut self, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        if let Some(at) = self.held.iter().position(&wanted) {
            return self.held.remove(at);
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|e| panic!("no {what}: {e}"));
            if wanted(&message) {
                return message;
            }
            self.held.push(message);
        }
    }

    fn reply(&mut self, id: u64) -> Value {
        self.wait_for("reply", |m| m["id"] == id && m.get("method").is_none())
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        let reply = self.reply(id);
        assert!(reply.get("error").is_none(), "{method} failed: {reply}");
        reply["result"].clone()
    }

    /// Waits for an update whose text is `text`.
    fn update_text(&mut self, text: &str) {
        self.wait_for(&format!("update {text:?}"), |m| {
            m["method"] == "session/update" && m["params"]["update"]["content"]["text"] == text
        });
    }

    /// Moves what already arrived into `held`, without waiting.
    fn pull(&mut self) {
        while let Ok(message) = self.lines.try_recv() {
            self.held.push(message);
        }
    }

    /// Every text of the updates delivered so far, in order, without waiting.
    fn drain_texts(&mut self) -> Vec<String> {
        self.pull();
        self.held
            .iter()
            .filter(|m| m["method"] == "session/update")
            .filter_map(|m| m["params"]["update"]["content"]["text"].as_str())
            .map(str::to_owned)
            .collect()
    }

    fn initialize(&mut self) -> Value {
        self.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {"_meta": {"kage": {}}}}),
        )
    }

    fn prompt(&mut self, session: &str, text: &str) -> u64 {
        self.send(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        )
    }

    fn stop_reason(&mut self, id: u64) -> String {
        let reply = self.reply(id);
        reply["result"]["stopReason"]
            .as_str()
            .unwrap_or_else(|| panic!("no stop reason in {reply}"))
            .to_owned()
    }

    /// Closes the relay and waits for it.
    fn leave(mut self) {
        drop(self.stdin);
        let _ = self.child.wait();
    }
}

/// Process ids of the scripted agent started for `env`, found by the
/// history file in their environment.
fn agent_processes(env: &Env) -> Vec<u32> {
    let marker = format!(
        "HISTORY_FILE={}",
        env.dir.path().join("history.json").display()
    );
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .any(|var| var == marker.as_bytes())
            })
        })
        .collect()
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The text of every recorded agent message in `path`, in order.
fn recorded(path: &Path) -> Vec<String> {
    use kage_session::entry::ExternalAcpPresentation;
    kage_session::external_replay::replay_external(path)
        .unwrap()
        .events
        .iter()
        .filter_map(|event| match event {
            ExternalAcpPresentation::AgentMessage { text, .. } => Some(text.as_str().to_owned()),
            _ => None,
        })
        .collect()
}

#[test]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
fn an_external_session_runs_end_to_end_through_the_host() {
    let env = Env::new();
    let cwd = env.cwd();
    let mut owner = env.client();
    let init = owner.initialize();
    assert_eq!(init["_meta"]["kage"]["externalSessions"], true, "{init}");

    // Creation through a configured agent, with the first turn.
    let created = owner.request(
        "session/new",
        json!({"cwd": cwd, "mcpServers": [], "_meta": {"kage": {"externalAgent": "fake"}}}),
    );
    let session = created["sessionId"].as_str().unwrap().to_owned();
    let first = owner.prompt(&session, "one");
    assert_eq!(owner.stop_reason(first), "end_turn");
    owner.update_text("one");
    assert_eq!(agent_processes(&env).len(), 1, "one upstream process");

    // A second client attaches to the live session and shares the upstream
    // conversation.
    let mut other = env.client();
    other.initialize();
    other.request(
        "session/resume",
        json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
    );
    assert_eq!(
        agent_processes(&env).len(),
        1,
        "attaching starts no process"
    );
    let second = other.prompt(&session, "two");
    assert_eq!(other.stop_reason(second), "end_turn");
    other.update_text("one|two");
    owner.update_text("one|two");

    // The permission ask goes to the client that owns the prompt, and the
    // selected option reaches the agent.
    let ask = owner.prompt(&session, "ask");
    let request = owner.wait_for("permission request", |m| {
        m["method"] == "session/request_permission"
    });
    assert_eq!(request["params"]["sessionId"], session);
    owner.write(&json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": {"outcome": {"outcome": "selected", "optionId": "allow"}},
    }));
    assert_eq!(owner.stop_reason(ask), "end_turn");
    owner.update_text("asked:allow");
    other.update_text("asked:allow");
    other.drain_texts();
    assert!(
        other
            .held
            .iter()
            .all(|m| m["method"] != "session/request_permission"),
        "the other client was asked about a prompt it does not own"
    );

    // Only the prompt's owner cancels it.
    let waiting = owner.prompt(&session, "wait");
    owner.update_text("waiting");
    other.notify("session/cancel", json!({"sessionId": session}));
    std::thread::sleep(Duration::from_millis(500));
    owner.pull();
    assert!(
        owner.held.iter().all(|m| m["id"] != waiting),
        "a cancel from another client ended the turn"
    );
    owner.notify("session/cancel", json!({"sessionId": session}));
    assert_eq!(owner.stop_reason(waiting), "cancelled");

    // The last client leaving ends the upstream process.
    owner.request("session/close", json!({"sessionId": session}));
    assert_eq!(
        agent_processes(&env).len(),
        1,
        "another client still holds it"
    );
    other.request("session/close", json!({"sessionId": session}));
    wait_until("the agent outlived its last client", || {
        agent_processes(&env).is_empty()
    });
    owner.leave();
    other.leave();
    wait_until("the host did not retire after its last client left", || {
        !env.dir.path().join("runtime/kage/host.json").exists()
    });

    // Reopening restores the conversation from the agent and shows the
    // recorded history before the response.
    let mut reopened = env.client();
    reopened.initialize();
    let load = reopened.send(
        "session/load",
        json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
    );
    let loaded = reopened.reply(load);
    assert!(loaded.get("error").is_none(), "load failed: {loaded}");
    let history = reopened.drain_texts();
    for shown in ["one", "one|two", "asked:allow"] {
        assert!(
            history.iter().any(|text| text == shown),
            "history lacks {shown:?}: {history:?}"
        );
    }
    let third = reopened.prompt(&session, "three");
    assert_eq!(reopened.stop_reason(third), "end_turn");
    reopened.update_text("one|two|three");
    assert!(
        recorded(&env.session_file(&session))
            .iter()
            .any(|text| text == "one|two|three"),
        "the new turn is recorded in the same file"
    );
    reopened.request("session/close", json!({"sessionId": session}));
    reopened.leave();
    wait_until("the restored agent outlived its client", || {
        agent_processes(&env).is_empty()
    });
}

#[test]
fn an_unconfigured_external_agent_is_refused() {
    let env = Env::new();
    let mut client = env.client();
    client.initialize();
    let id = client.send(
        "session/new",
        json!({
            "cwd": env.cwd(),
            "mcpServers": [],
            "_meta": {"kage": {"externalAgent": "nobody"}},
        }),
    );
    let reply = client.reply(id);
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
    assert!(agent_processes(&env).is_empty());
    client.leave();
}

#[test]
fn stopping_the_host_ends_its_external_agents() {
    let env = Env::new();
    let mut client = env.client();
    client.initialize();
    client.request(
        "session/new",
        json!({"cwd": env.cwd(), "mcpServers": [], "_meta": {"kage": {"externalAgent": "fake"}}}),
    );
    assert_eq!(agent_processes(&env).len(), 1);
    let record = std::fs::read(env.dir.path().join("runtime/kage/host.json")).unwrap();
    let pid = serde_json::from_slice::<Value>(&record).unwrap()["pid"]
        .as_i64()
        .unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    wait_until("the agent outlived its host", || {
        agent_processes(&env).is_empty()
    });
    client.leave();
}
