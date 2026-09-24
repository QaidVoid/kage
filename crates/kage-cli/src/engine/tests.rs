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

#[test]
fn prompt_runs_and_records_the_history() {
    let dir = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let path = dir.path().join(format!("{id}.jsonl"));
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
    let h = harness(MockProvider::replaying(text_turn("hello")));
    h.open(id, Some(Recorder::new(writer, None)));
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
