use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use kage_core::agents::AgentDefs;
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
            recorder,
            gate,
            ..self.spec(id)
        });
    }

    fn spec(&self, id: SessionId) -> SessionSpec {
        SessionSpec {
            id,
            model: "mock:m".into(),
            cx: AgentContext::new("m", "").with_workdir("/tmp"),
            recorder: None,
            tools: self.tools.clone(),
            plugins: None,
            gate: PermissionGate::new(PermissionsConfig::default()),
            loop_cfg: LoopConfig::default(),
            mcp: None,
            interactive: true,
            title: false,
            agents: None,
        }
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
        agents: None,
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
        agents: None,
    });
    prompt(&h.engine, id, "hi", Delivery::Steer);
    until_runs_end(&h.events, 1);
    h.engine.shutdown();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("plugin:mark"), "{file}");
}

fn agent_setup(max_depth: u8, max_running: usize) -> AgentSetup {
    AgentSetup {
        defs: Arc::new(AgentDefs::builtin()),
        max_depth,
        max_running,
    }
}

fn task(prompt: &str) -> serde_json::Value {
    serde_json::json!({"description": "a task", "prompt": prompt})
}

/// One assistant turn that calls `agent` once per `(call id, input)`.
fn agent_turn(calls: &[(&str, serde_json::Value)]) -> Vec<Result<ProviderEvent, ProviderError>> {
    let mut turn = vec![Ok(ProviderEvent::MessageStart)];
    for (id, input) in calls {
        let id = ToolCallId::new(*id);
        turn.push(Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: "agent".into(),
        }));
        turn.push(Ok(ProviderEvent::ToolCallEnd {
            id,
            input: input.clone(),
        }));
    }
    turn.push(Ok(ProviderEvent::MessageEnd {
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage::default(),
    }));
    turn
}

impl Harness {
    fn open_parent(
        &self,
        recorder: Option<Recorder>,
        gate: PermissionGate,
        agents: Option<AgentSetup>,
    ) -> SessionId {
        let id = SessionId::new();
        self.engine.open(SessionSpec {
            recorder,
            gate,
            agents,
            ..self.spec(id)
        });
        id
    }
}

/// Children in spawn order, with the call that started each.
fn spawned(events: &[Envelope]) -> Vec<(SessionId, ToolCallId)> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Host(HostEvent::AgentSpawned { tool_call_id, .. }) => {
                Some((e.session, tool_call_id.clone()))
            }
            _ => None,
        })
        .collect()
}

fn tool_output(events: &[Envelope], session: SessionId, call: &str) -> ToolOutput {
    events
        .iter()
        .find_map(|e| match &e.event {
            Event::Loop(LoopEvent::ToolCallEnd { id, output })
                if e.session == session && id.0 == call =>
            {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("tool call ended")
}

fn outcome_of(events: &[Envelope], session: SessionId) -> Vec<RunOutcome> {
    let mine: Vec<Envelope> = events
        .iter()
        .filter(|e| e.session == session)
        .cloned()
        .collect();
    outcomes(&mine)
}

fn is_tool_start_outside(parent: SessionId) -> impl Fn(&Envelope) -> bool {
    move |e| e.session != parent && is_tool_start(e)
}

#[test]
fn agent_call_returns_the_child_reply_and_records_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("find it"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]));
    let parent_id = SessionId::new();
    let (recorder, parent_path) = recorder_in(dir.path(), parent_id);
    h.engine.open(SessionSpec {
        recorder: Some(recorder),
        agents: Some(agent_setup(1, 1)),
        ..h.spec(parent_id)
    });
    prompt(&h.engine, parent_id, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    h.engine.shutdown();

    let [(child, call)] = &spawned(&events)[..] else {
        panic!("one child expected");
    };
    let child = *child;
    assert_eq!(call.0, "call_a");
    let first = events.iter().find(|e| e.session == child).unwrap();
    assert_eq!(first.seq, 1);
    assert!(matches!(
        &first.event,
        Event::Host(HostEvent::AgentSpawned { parent, agent, .. })
            if *parent == parent_id && agent == "general"
    ));
    let output = tool_output(&events, parent_id, "call_a");
    assert_eq!(
        output.text,
        format!(
            "<agent name=\"general\" session=\"{child}\" state=\"completed\">\nchild reply\n</agent>"
        )
    );
    assert!(!output.is_error);
    assert_eq!(outcome_of(&events, parent_id), [RunOutcome::Completed]);

    let child_path = dir.path().join(format!("{child}.jsonl"));
    let entries: Vec<kage_session::SessionEntry> = kage_session::SessionReader::iter(&child_path)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    match &entries[..3] {
        [
            kage_session::SessionEntry::Header(header),
            kage_session::SessionEntry::Custom(marker),
            kage_session::SessionEntry::Title(title),
        ] => {
            assert_eq!(header.parent_session, Some(parent_id));
            assert_eq!(marker.kind, kage_session::list::AGENT_ENTRY_KIND);
            assert_eq!(marker.data["agent"], "general");
            assert_eq!(marker.data["tool_call_id"], "call_a");
            assert_eq!(marker.data["parent"], parent_id.to_string());
            assert_eq!(title.title, "a task");
        }
        other => panic!("unexpected entries {other:?}"),
    }
    let summaries = kage_session::list(dir.path()).unwrap();
    let child_summary = summaries.iter().find(|s| s.id == child).unwrap();
    assert_eq!(child_summary.agent.as_deref(), Some("general"));
    let parent_file = std::fs::read_to_string(&parent_path).unwrap();
    assert!(
        parent_file.contains(&format!("session=\\\"{child}\\\"")),
        "{parent_file}"
    );
}

#[test]
fn agent_tool_follows_the_depth_limit() {
    let tool_names = |request: &kage_provider::StreamRequest| -> Vec<String> {
        request.tools.iter().map(|t| t.name.clone()).collect()
    };
    let mock = MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("look"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]);
    let h = harness(mock.clone());
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    until_runs_end(&h.events, 2);
    let requests = mock.requests();
    assert!(tool_names(&requests[0]).contains(&"agent".to_owned()));
    assert_eq!(tool_names(&requests[1]), ["gate"]);

    let mock = MockProvider::replaying(text_turn("ok"));
    let h = harness(mock.clone());
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(0, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    until_runs_end(&h.events, 1);
    assert_eq!(tool_names(&mock.requests()[0]), ["gate"]);
}

#[test]
fn unknown_agent_names_the_valid_ones() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[(
            "call_a",
            serde_json::json!({"agent": "nope", "description": "d", "prompt": "p"}),
        )]),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    let output = tool_output(&events, parent, "call_a");
    assert!(output.is_error);
    assert!(output.text.contains("explore, general"), "{}", output.text);
    assert!(spawned(&events).is_empty());
}

#[test]
fn cancelling_the_parent_stops_the_child() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("wait"))]),
        tool_turn("gate"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start_outside(parent));
    h.engine.send(Command::to(parent, CommandKind::Cancel));
    let events = until_runs_end(&h.events, 2);
    assert_eq!(
        outcomes(&events),
        [RunOutcome::Cancelled, RunOutcome::Cancelled]
    );
}

#[test]
fn cancelling_only_the_child_lets_the_parent_continue() {
    let mock = MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("wait"))]),
        tool_turn("gate"),
        text_turn("parent done"),
    ]);
    let h = harness(mock.clone());
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let seen = wait_for(&h.events, is_tool_start_outside(parent));
    let child = seen.last().unwrap().session;
    h.engine.send(Command::to(child, CommandKind::Cancel));
    let events = until_runs_end(&h.events, 2);

    assert_eq!(outcome_of(&events, child), [RunOutcome::Cancelled]);
    assert_eq!(outcome_of(&events, parent), [RunOutcome::Completed]);
    let output = tool_output(&events, parent, "call_a");
    assert!(output.is_error);
    assert!(
        output.text.contains("state=\"cancelled\""),
        "{}",
        output.text
    );
    assert_eq!(mock.call_count(), 3);
}

#[test]
fn running_limit_queues_agents_in_spawn_order() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("one")), ("call_b", task("two"))]),
        text_turn("first reply"),
        text_turn("second reply"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 3);

    let children = spawned(&events);
    assert_eq!(children.len(), 2);
    let (first, second) = (&children[0], &children[1]);
    let position = |pred: &dyn Fn(&Envelope) -> bool| events.iter().position(pred).unwrap();
    let second_spawned = position(&|e| {
        e.session == second.0 && matches!(e.event, Event::Host(HostEvent::AgentSpawned { .. }))
    });
    let first_ended = position(&|e| {
        e.session == first.0 && matches!(e.event, Event::Host(HostEvent::RunEnded { .. }))
    });
    let second_started = position(&|e| {
        e.session == second.0 && matches!(e.event, Event::Host(HostEvent::RunStarted))
    });
    assert!(second_spawned < first_ended && first_ended < second_started);
    assert!(
        tool_output(&events, parent, &first.1.0)
            .text
            .contains("first reply")
    );
    assert!(
        tool_output(&events, parent, &second.1.0)
            .text
            .contains("second reply")
    );
    let results: Vec<String> = events
        .iter()
        .filter(|e| e.session == parent)
        .filter_map(|e| match &e.event {
            Event::Loop(LoopEvent::MessageAppended { message }) => Some(message.content.clone()),
            _ => None,
        })
        .flatten()
        .filter_map(|c| match c {
            Content::ToolResultBlock { call_id, .. } => Some(call_id.0),
            _ => None,
        })
        .collect();
    assert_eq!(results, ["call_a", "call_b"]);
}

#[test]
fn child_asks_and_resolutions_use_the_child_session() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("ask"))]),
        tool_turn("gate"),
        text_turn("child done"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(None, ask_for_gate(), Some(agent_setup(1, 1)));
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let seen = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::PermissionRequested { .. }))
    });
    let asked = seen.last().unwrap();
    let child = asked.session;
    assert_ne!(child, parent);
    let Event::Host(HostEvent::PermissionRequested { request_id, .. }) = asked.event else {
        unreachable!()
    };
    h.engine
        .send(Command::active(CommandKind::ResolvePermission {
            request_id,
            decision: PermissionDecision::AllowOnce,
        }));
    let resolved = wait_for(&h.events, |e| {
        matches!(e.event, Event::Host(HostEvent::PermissionResolved { .. }))
    });
    assert_eq!(resolved.last().unwrap().session, child);
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 2);
    assert_eq!(outcome_of(&events, parent), [RunOutcome::Completed]);
}

#[test]
fn steering_a_running_child_reaches_it() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        tool_turn("gate"),
        text_turn("child done"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let child = wait_for(&h.events, is_tool_start_outside(parent))
        .last()
        .unwrap()
        .session;
    prompt(&h.engine, child, "also this", Delivery::Steer);
    std::thread::sleep(Duration::from_millis(50));
    h.release.send(()).unwrap();
    let events = until_runs_end(&h.events, 2);
    let child_events: Vec<Envelope> = events
        .iter()
        .filter(|e| e.session == child)
        .cloned()
        .collect();
    assert!(appended_texts(&child_events).contains(&"also this".to_owned()));
}

#[test]
fn prompting_an_idle_child_leaves_the_parent_alone() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
        text_turn("second child reply"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    let (child, _) = spawned(&events)[0].clone();
    prompt(&h.engine, child, "more", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    assert!(
        events
            .iter()
            .filter(|e| e.session == parent)
            .all(|e| state_of(e).is_some()),
        "only the parent's trailing state change: {events:?}"
    );
    assert_eq!(outcomes(&events), [RunOutcome::Completed]);
}

#[test]
fn an_idle_child_of_a_cancelled_parent_can_run_again() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("wait"))]),
        tool_turn("gate"),
        text_turn("child again"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let child = wait_for(&h.events, is_tool_start_outside(parent))
        .last()
        .unwrap()
        .session;
    h.engine.send(Command::to(parent, CommandKind::Cancel));
    until_runs_end(&h.events, 2);
    prompt(&h.engine, child, "again", Delivery::Steer);
    let events = until_runs_end(&h.events, 1);
    assert_eq!(outcome_of(&events, child), [RunOutcome::Completed]);
}

#[test]
fn new_session_waits_for_agents_then_drops_them() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
        tool_turn("gate"),
        text_turn("child again"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    let (child, _) = spawned(&events)[0].clone();
    prompt(&h.engine, child, "more", Delivery::Steer);
    wait_for(&h.events, is_tool_start);

    h.engine.send(Command::to(parent, CommandKind::NewSession));
    let refused = wait_for(&h.events, is_notice);
    assert_eq!(
        notices(&refused),
        ["new session: stop or wait for the agents first"]
    );
    h.engine.send(Command::to(child, CommandKind::NewSession));
    let refused = wait_for(&h.events, is_notice);
    assert_eq!(
        notices(&refused),
        ["new session: not available in an agent session"]
    );

    h.release.send(()).unwrap();
    until_runs_end(&h.events, 1);
    h.engine.send(Command::to(parent, CommandKind::NewSession));
    wait_for(&h.events, is_session_changed);
    prompt(&h.engine, child, "gone?", Delivery::Steer);
    let unknown = wait_for(&h.events, |e| {
        is_notice(e) && notices(std::slice::from_ref(e))[0].starts_with("unknown session")
    });
    assert_eq!(unknown.last().unwrap().session, child);
}

#[test]
fn agent_runs_skip_plugin_events_and_session_ops() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(PluginRuntime::new().unwrap());
    runtime
        .eval(
            "kage.session.append_entry('plugin:queued', {}) \
             kage.on('turn_start', function() \
                kage.session.append_entry('plugin:turn', {}) \
             end)",
        )
        .unwrap();
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]));
    let parent = SessionId::new();
    let (_, path) = recorder_in(dir.path(), parent);
    let writer = SessionWriter::open(&path).unwrap();
    h.engine.open(SessionSpec {
        recorder: Some(Recorder::new(writer, Some(Arc::clone(&runtime)))),
        plugins: Some(runtime),
        agents: Some(agent_setup(1, 1)),
        ..h.spec(parent)
    });
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);
    h.engine.shutdown();

    let (child, _) = spawned(&events)[0].clone();
    let parent_file = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        parent_file.matches("plugin:turn").count(),
        2,
        "{parent_file}"
    );
    assert!(parent_file.contains("plugin:queued"), "{parent_file}");
    let child_file = std::fs::read_to_string(dir.path().join(format!("{child}.jsonl"))).unwrap();
    assert!(!child_file.contains("plugin:"), "{child_file}");
}

#[test]
fn shutdown_with_running_and_waiting_agents_returns() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("one")), ("call_b", task("two"))]),
        tool_turn("gate"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    wait_for(&h.events, is_tool_start_outside(parent));
    let (done_tx, done_rx) = channel();
    std::thread::spawn(move || {
        h.engine.shutdown();
        let _ = done_tx.send(());
    });
    done_rx.recv_timeout(WAIT).expect("shutdown hung");
}

#[test]
fn print_mode_text_shows_only_the_main_session() {
    let h = harness(MockProvider::sequence(vec![
        agent_turn(&[("call_a", task("work"))]),
        text_turn("child reply"),
        text_turn("parent done"),
    ]));
    let parent = h.open_parent(
        None,
        PermissionGate::new(PermissionsConfig::default()),
        Some(agent_setup(1, 1)),
    );
    prompt(&h.engine, parent, "go", Delivery::Steer);
    let events = until_runs_end(&h.events, 2);

    let mut text = Vec::new();
    let mut json = Vec::new();
    for envelope in &events {
        crate::cli_loop_run::print_envelope(&mut text, envelope, parent, false);
        crate::cli_loop_run::print_envelope(&mut json, envelope, parent, true);
    }
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("parent done"), "{text}");
    assert!(!text.contains("child reply"), "{text}");
    let json = String::from_utf8(json).unwrap();
    assert!(json.contains("\"agent_spawned\""), "{json}");
    assert!(json.contains("child reply"), "{json}");
}
