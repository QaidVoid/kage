use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use kage_core::permissions::{PermissionAction, PermissionsConfig};
use kage_core::protocol::{Envelope, Event, RunOutcome};
use kage_core::{LoopEvent, StopReason, TokenUsage, ToolCallId, ToolOutput};
use kage_provider::testing::MockProvider;
use kage_provider::{ProviderError, ProviderEvent};
use kage_session::{EntryId, FORMAT_VERSION, Header, SessionWriter};
use kage_tools::{Tool, ToolContext, ToolError};

use super::*;

const WAIT: Duration = Duration::from_secs(5);

fn text_turn(text: &str) -> Vec<Result<ProviderEvent, ProviderError>> {
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::TextDelta { delta: text.into() }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input: 10,
                output: 2,
                ..TokenUsage::default()
            },
        }),
    ]
}

fn tool_turn(tool: &str) -> Vec<Result<ProviderEvent, ProviderError>> {
    let id = ToolCallId::new(format!("call_{tool}"));
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: tool.into(),
        }),
        Ok(ProviderEvent::ToolCallEnd {
            id,
            input: serde_json::json!({}),
        }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }),
    ]
}

/// Blocks until the test releases it or the run is cancelled.
#[derive(Debug)]
struct Gate {
    release: Mutex<Receiver<()>>,
}

impl Tool for Gate {
    fn name(&self) -> &'static str {
        "gate"
    }
    fn description(&self) -> &'static str {
        "waits for the test"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self) -> kage_core::Risk {
        kage_core::Risk::Read
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let release = lock(&self.release);
        loop {
            if cx.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            if release.recv_timeout(Duration::from_millis(10)).is_ok() {
                return Ok(ToolOutput {
                    text: "released".into(),
                    ..ToolOutput::default()
                });
            }
        }
    }
}

struct Harness {
    engine: Engine,
    events: Receiver<Envelope>,
    release: mpsc::Sender<()>,
    tools: ToolRegistry,
}

fn harness(mock: MockProvider) -> Harness {
    let (release, release_rx) = channel();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Gate {
        release: Mutex::new(release_rx),
    }));
    let (tx, events) = channel();
    let collector: Subscriber = Box::new(move |envelope| {
        let _ = tx.send(envelope.clone());
    });
    let engine = Engine::start(Arc::new(ProviderRegistry::new().with(Arc::new(mock))));
    engine.subscribe(collector);
    Harness {
        engine,
        events,
        release,
        tools,
    }
}

impl Harness {
    fn open(&self, id: SessionId, recorder: Option<Recorder>) {
        self.open_with(
            id,
            recorder,
            PermissionGate::new(PermissionsConfig::default()),
        );
    }

    fn open_with(&self, id: SessionId, recorder: Option<Recorder>, gate: PermissionGate) {
        self.engine.open(SessionSpec {
            id,
            model: "mock:m".into(),
            cx: AgentContext::new("m", "").with_workdir("/tmp"),
            recorder,
            tools: self.tools.clone(),
            plugins: None,
            gate,
            loop_cfg: LoopConfig::default(),
            mcp: None,
            interactive: true,
            title: false,
        });
    }
}

fn prompt(engine: &Engine, session: SessionId, text: &str, delivery: Delivery) {
    engine.send(Command::to(
        session,
        CommandKind::Prompt {
            content: vec![Content::Text { text: text.into() }],
            delivery,
        },
    ));
}

/// Collect envelopes until `count` runs have ended.
fn until_runs_end(events: &Receiver<Envelope>, count: usize) -> Vec<Envelope> {
    let mut seen = Vec::new();
    let mut ended = 0;
    while ended < count {
        let envelope = events.recv_timeout(WAIT).expect("engine stalled");
        if matches!(envelope.event, Event::Host(HostEvent::RunEnded { .. })) {
            ended += 1;
        }
        seen.push(envelope);
    }
    seen
}

fn wait_for(events: &Receiver<Envelope>, pred: impl Fn(&Envelope) -> bool) -> Vec<Envelope> {
    let mut seen = Vec::new();
    loop {
        let envelope = events.recv_timeout(WAIT).expect("engine stalled");
        let done = pred(&envelope);
        seen.push(envelope);
        if done {
            return seen;
        }
    }
}

fn appended_texts(events: &[Envelope]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Loop(LoopEvent::MessageAppended { message }) => {
                Some(crate::cli_loop_run::first_user_text(message))
            }
            _ => None,
        })
        .collect()
}

fn outcomes(events: &[Envelope]) -> Vec<RunOutcome> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Host(HostEvent::RunEnded { outcome }) => Some(outcome.clone()),
            _ => None,
        })
        .collect()
}

fn is_tool_start(envelope: &Envelope) -> bool {
    matches!(envelope.event, Event::Loop(LoopEvent::ToolCallStart { .. }))
}

/// A recorder writing `<dir>/<id>.jsonl`.
fn recorder_in(dir: &std::path::Path, id: SessionId) -> (Recorder, std::path::PathBuf) {
    let path = dir.join(format!("{id}.jsonl"));
    let writer = SessionWriter::create(
        &path,
        Header {
            version: FORMAT_VERSION,
            session: id,
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            cwd: "/tmp".into(),
            model: "mock:m".into(),
            system_prompt: String::new(),
            parent_session: None,
            parent_entry: None,
        },
    )
    .unwrap();
    (Recorder::new(writer, None), path)
}

fn host_events(events: &[Envelope]) -> Vec<&HostEvent> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Host(host) => Some(host),
            Event::Loop(_) => None,
        })
        .collect()
}

fn notices(events: &[Envelope]) -> Vec<String> {
    host_events(events)
        .into_iter()
        .filter_map(|e| match e {
            HostEvent::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn is_notice(envelope: &Envelope) -> bool {
    matches!(envelope.event, Event::Host(HostEvent::Notice { .. }))
}

fn is_session_changed(envelope: &Envelope) -> bool {
    matches!(
        envelope.event,
        Event::Host(HostEvent::SessionChanged { .. })
    )
}

#[test]
fn prompt_runs_and_records_the_history() {
    let dir = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    let h = harness(MockProvider::replaying(text_turn("hello")));
    h.open(id, Some(recorder));
    prompt(&h.engine, id, "hi", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    h.engine.shutdown();

    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
    let usage = events.iter().rev().find_map(|e| match &e.event {
        Event::Host(HostEvent::UsageUpdated { usage }) => Some(*usage),
        _ => None,
    });
    assert_eq!(usage.unwrap().total.input, 10);

    let replay = kage_session::replay(&path).unwrap();
    assert_eq!(replay.history.len(), 2);
    assert_eq!(replay.history[1].parent, Some(replay.history[0].id));
    assert_eq!(replay.usage_total.input, 10);
}

#[test]
fn steered_prompt_is_delivered_at_the_next_turn() {
    let h = harness(MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("done"),
    ]));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "start", Delivery::Steer);
    wait_for(&h.events, is_tool_start);
    prompt(&h.engine, id, "also this", Delivery::Steer);
    std::thread::sleep(Duration::from_millis(50));
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 1);

    let roles: Vec<(Role, String)> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Loop(LoopEvent::MessageAppended { message }) => {
                Some((message.role, crate::cli_loop_run::first_user_text(message)))
            }
            _ => None,
        })
        .collect();
    let tool_result = roles.iter().position(|(r, _)| *r == Role::ToolResult);
    let steered = roles.iter().position(|(_, t)| t == "also this");
    assert!(
        steered > tool_result,
        "steer lands after the tool result: {roles:?}"
    );
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
}

#[test]
fn queued_prompt_starts_a_new_run() {
    let h = harness(MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("first"),
        text_turn("second"),
    ]));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "one", Delivery::Steer);
    wait_for(&h.events, is_tool_start);
    prompt(&h.engine, id, "two", Delivery::Queue);
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 2);

    assert_eq!(
        outcomes(&events),
        [RunOutcome::Completed, RunOutcome::Completed]
    );
    let run_ends = events
        .iter()
        .position(|e| matches!(e.event, Event::Host(HostEvent::RunEnded { .. })))
        .unwrap();
    assert!(appended_texts(&events[run_ends..]).contains(&"two".to_owned()));
}

#[test]
fn cancel_ends_the_run_as_cancelled() {
    let h = harness(MockProvider::replaying(tool_turn("gate")));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start);
    h.engine.send(Command::to(id, CommandKind::Cancel));
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcomes(&events), [RunOutcome::Cancelled]);
}

#[test]
fn shutdown_stops_a_running_session() {
    let h = harness(MockProvider::replaying(tool_turn("gate")));
    let id = SessionId::new();
    h.open(id, None);
    prompt(&h.engine, id, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start);

    let (done_tx, done_rx) = channel();
    std::thread::spawn(move || {
        h.engine.shutdown();
        let _ = done_tx.send(());
    });
    done_rx.recv_timeout(WAIT).expect("shutdown hung");
}

#[test]
fn sessions_are_sequenced_independently() {
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let (a, b) = (SessionId::new(), SessionId::new());
    h.open(a, None);
    h.open(b, None);
    prompt(&h.engine, a, "a", Delivery::Steer);
    prompt(&h.engine, b, "b", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);

    for session in [a, b] {
        let seqs: Vec<u64> = events
            .iter()
            .filter(|e| e.session == session)
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    }
}

fn ask_for_gate() -> PermissionGate {
    let mut rules = PermissionsConfig::default();
    rules.tools.insert(
        "gate".into(),
        kage_core::permissions::ToolPermissionRules {
            default: PermissionAction::Ask,
            allow: Vec::new(),
            deny: Vec::new(),
        },
    );
    PermissionGate::new(rules)
}

fn permission_request(events: &Receiver<Envelope>) -> (RequestId, Option<ToolCallId>) {
    let seen = wait_for(events, |e| {
        matches!(e.event, Event::Host(HostEvent::PermissionRequested { .. }))
    });
    match &seen.last().unwrap().event {
        Event::Host(HostEvent::PermissionRequested {
            request_id,
            tool_call_id,
            ..
        }) => (*request_id, tool_call_id.clone()),
        _ => unreachable!(),
    }
}

#[test]
fn permission_asks_travel_over_the_bus() {
    let h = harness(MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("done"),
    ]));
    let id = SessionId::new();
    h.open_with(id, None, ask_for_gate());
    prompt(&h.engine, id, "go", Delivery::Steer);

    let (request_id, call_id) = permission_request(&h.events);
    assert_eq!(call_id, Some(ToolCallId::new("call_gate")));
    h.engine.send(Command::to(
        id,
        CommandKind::ResolvePermission {
            request_id,
            decision: PermissionDecision::AllowOnce,
        },
    ));
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 1);

    let output = events.iter().find_map(|e| match &e.event {
        Event::Loop(LoopEvent::ToolCallEnd { output, .. }) => Some(output.clone()),
        _ => None,
    });
    assert_eq!(output.unwrap().text, "released");
}

#[test]
fn denied_permission_refuses_the_tool() {
    let h = harness(MockProvider::sequence(vec![
        tool_turn("gate"),
        text_turn("done"),
    ]));
    let id = SessionId::new();
    h.open_with(id, None, ask_for_gate());
    prompt(&h.engine, id, "go", Delivery::Steer);

    let (request_id, _) = permission_request(&h.events);
    h.engine.send(Command::to(
        id,
        CommandKind::ResolvePermission {
            request_id,
            decision: PermissionDecision::Deny,
        },
    ));
    let events = until_runs_end(&h.events, 1);

    let output = events.iter().find_map(|e| match &e.event {
        Event::Loop(LoopEvent::ToolCallEnd { output, .. }) => Some(output.clone()),
        _ => None,
    });
    assert!(output.unwrap().is_error);
}

#[test]
fn run_shell_capture_combines_streams_and_exit_code() {
    let dir = std::env::temp_dir();
    let (code, out) = run_shell("echo out; echo err >&2", &dir);
    assert_eq!(code, Some(0));
    assert!(out.contains("out"), "{out}");
    assert!(out.contains("err"), "{out}");
}

#[test]
fn run_shell_capture_reports_failure_and_signal() {
    let dir = std::env::temp_dir();
    let (code, out) = run_shell("exit 3", &dir);
    assert_eq!(code, Some(3));
    assert_eq!(out, "");
    let (code, _) = run_shell("kill -9 $$", &dir);
    assert_eq!(code, None);
}

#[test]
fn run_shell_capture_truncates_large_output() {
    let dir = std::env::temp_dir();
    let (_, out) = run_shell("yes | head -c 100000", &dir);
    assert!(
        out.chars().count() <= 8 * 1024 + 64,
        "truncated, len {}",
        out.chars().count()
    );
    assert!(
        out.contains("output truncated"),
        "{:?}",
        &out[..out.len().min(200)]
    );
}

#[test]
fn run_shell_capture_runs_in_the_given_workdir() {
    let dir = std::env::temp_dir();
    let (_, out) = run_shell("pwd", &dir);
    assert!(out.trim().starts_with(dir.to_str().unwrap()), "{out}");
}

/// A session with one recorded exchange in `dir`.
fn recorded_session(h: &Harness, dir: &std::path::Path) -> (SessionId, std::path::PathBuf) {
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir, id);
    h.open(id, Some(recorder));
    prompt(&h.engine, id, "hi", Delivery::Steer);
    until_runs_end(&h.events, 1);
    (id, path)
}

#[test]
fn load_session_switches_to_the_stored_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let first = harness(MockProvider::replaying(text_turn("hello")));
    let (_, path) = recorded_session(&first, dir.path());
    first.engine.shutdown();

    let h = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::LoadSession { path: path.clone() },
    ));
    let seen = wait_for(&h.events, is_session_changed);
    match &seen.last().unwrap().event {
        Event::Host(HostEvent::SessionChanged {
            messages, path: p, ..
        }) => {
            assert_eq!(p, &path);
            assert_eq!(messages.len(), 2);
        }
        _ => unreachable!(),
    }
    prompt(
        &h.engine,
        seen.last().unwrap().session,
        "more",
        Delivery::Steer,
    );
    until_runs_end(&h.events, 1);
    assert_eq!(kage_session::replay(&path).unwrap().history.len(), 4);
}

#[test]
fn clone_continues_on_a_copy() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let (id, path) = recorded_session(&h, dir.path());

    h.engine.send(Command::to(id, CommandKind::Clone));
    let seen = wait_for(&h.events, is_session_changed);
    let Event::Host(HostEvent::SessionChanged {
        path: copy,
        messages,
        ..
    }) = &seen.last().unwrap().event
    else {
        unreachable!()
    };
    assert_ne!(copy, &path);
    assert!(copy.exists());
    assert_eq!(messages.len(), 2);
}

fn state_of(envelope: &Envelope) -> Option<&SessionState> {
    match &envelope.event {
        Event::Host(HostEvent::StateChanged { state }) => Some(state),
        _ => None,
    }
}

#[test]
fn switching_sessions_drops_the_permission_mode() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    let (recorder, _) = recorder_in(dir.path(), id);
    let gate = PermissionGate::new(PermissionsConfig::default());
    h.open_with(id, Some(recorder), gate.clone());
    prompt(&h.engine, id, "hi", Delivery::Steer);
    until_runs_end(&h.events, 1);

    h.engine.send(Command::to(
        id,
        CommandKind::SetPermissionMode {
            mode: Some(PermissionAction::Ask),
        },
    ));
    h.engine.send(Command::to(id, CommandKind::Clone));
    let before = wait_for(&h.events, is_session_changed);
    assert!(
        before
            .iter()
            .filter_map(state_of)
            .any(|s| s.permission_mode == Some(PermissionAction::Ask))
    );
    let after = wait_for(&h.events, |e| state_of(e).is_some());
    assert_eq!(
        state_of(after.last().unwrap()).unwrap().permission_mode,
        None
    );
    assert_eq!(gate.mode(), None);
}

#[test]
fn fork_export_and_delete_report_through_notices() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let (id, path) = recorded_session(&h, dir.path());

    h.engine.send(Command::to(
        id,
        CommandKind::Fork {
            at: None,
            switch: false,
        },
    ));
    let fork = wait_for(&h.events, is_notice);
    assert!(notices(&fork)[0].starts_with("forked session: "));

    let out = dir.path().join("out.md");
    h.engine.send(Command::to(
        id,
        CommandKind::Export {
            path: Some(out.clone()),
        },
    ));
    wait_for(&h.events, is_notice);
    assert!(
        std::fs::read_to_string(&out)
            .unwrap()
            .contains("## Assistant")
    );

    h.engine.send(Command::to(
        id,
        CommandKind::DeleteSession { path: path.clone() },
    ));
    let refused = wait_for(&h.events, is_notice);
    assert_eq!(notices(&refused), ["cannot delete an open session"]);
    assert!(path.exists());
}

#[test]
fn shell_output_reaches_the_next_request() {
    let mock = MockProvider::replaying(text_turn("ok"));
    let h = harness(mock.clone());
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(
        id,
        CommandKind::Shell {
            command: "echo from-shell".into(),
        },
    ));
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::ShellFinished { .. }))
    });
    let Event::Host(HostEvent::ShellFinished {
        output, exit_code, ..
    }) = &seen.last().unwrap().event
    else {
        unreachable!()
    };
    assert_eq!(output.trim(), "from-shell");
    assert_eq!(*exit_code, Some(0));

    prompt(&h.engine, id, "what did it print?", Delivery::Steer);
    until_runs_end(&h.events, 1);
    let request = mock.requests().pop().unwrap();
    let texts: Vec<String> = request
        .messages
        .iter()
        .map(crate::cli_loop_run::first_user_text)
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("[shell] ran `echo from-shell`"))
    );
}

#[test]
fn compact_without_history_is_a_no_op() {
    let h = harness(MockProvider::replaying(text_turn("ok")));
    let id = SessionId::new();
    h.open(id, None);
    h.engine.send(Command::to(id, CommandKind::Compact));
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
    assert!(notices(&events).contains(&"compact: not enough history yet".to_owned()));
}

#[test]
fn first_exchange_records_a_title() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    let (recorder, path) = recorder_in(dir.path(), id);
    h.engine.open(SessionSpec {
        id,
        model: "mock:m".into(),
        cx: AgentContext::new("m", "").with_workdir("/tmp"),
        recorder: Some(recorder),
        tools: h.tools.clone(),
        plugins: None,
        gate: PermissionGate::new(PermissionsConfig::default()),
        loop_cfg: LoopConfig::default(),
        mcp: None,
        interactive: true,
        title: true,
    });
    prompt(&h.engine, id, "hi", Delivery::Steer);
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::TitleChanged { .. }))
    });
    let Event::Host(HostEvent::TitleChanged { title }) = &seen.last().unwrap().event else {
        unreachable!()
    };
    assert_eq!(title, "hello");
    h.engine.shutdown();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("\"type\":\"title\""), "{file}");
}

#[test]
fn plugin_turn_end_entries_land_in_the_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(PluginRuntime::new().unwrap());
    runtime
        .eval(
            "kage.on('turn_end', function() \
                kage.session.append_entry('plugin:mark', { ok = true }) \
            end)",
        )
        .unwrap();
    let h = harness(MockProvider::replaying(text_turn("hello")));
    let id = SessionId::new();
    let (_, path) = recorder_in(dir.path(), id);
    let writer = SessionWriter::open(&path).unwrap();
    h.engine.open(SessionSpec {
        id,
        model: "mock:m".into(),
        cx: AgentContext::new("m", "").with_workdir("/tmp"),
        recorder: Some(Recorder::new(writer, Some(Arc::clone(&runtime)))),
        tools: h.tools.clone(),
        plugins: Some(runtime),
        gate: PermissionGate::new(PermissionsConfig::default()),
        loop_cfg: LoopConfig::default(),
        mcp: None,
        interactive: true,
        title: false,
    });
    prompt(&h.engine, id, "hi", Delivery::Steer);
    until_runs_end(&h.events, 1);
    h.engine.shutdown();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("plugin:mark"), "{file}");
}
