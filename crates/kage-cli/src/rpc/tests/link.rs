//! Link clients attached to sessions the host serves.

use std::os::unix::net::UnixStream;

use kage_core::protocol::{Envelope, Event, HostEvent, NoticeLevel, PermissionDecision};

use super::*;
use crate::rpc::link::{self, Hello, LINK_VERSION, Reply};

/// The client end of a link on a test host.
struct LinkClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl LinkClient {
    /// Attaches to `session` on `host`, or returns why serve refused.
    fn attach(host: &Arc<Host>, session: SessionId) -> Result<Self, String> {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let host = Arc::clone(host);
        let log: crate::serve::Log = Arc::new(|_| {});
        std::thread::spawn(move || link::serve(&host, theirs, &log));
        ours.set_read_timeout(Some(WAIT)).unwrap();
        let mut writer = ours.try_clone().unwrap();
        let hello = Hello::Attach {
            session,
            v: LINK_VERSION,
        };
        link::write_line(&mut writer, &hello).unwrap();
        let mut reader = BufReader::new(ours);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        match serde_json::from_str(&line).unwrap() {
            Reply::Attached { pid } => {
                assert_eq!(pid, std::process::id());
                Ok(Self { reader, writer })
            }
            Reply::Refused(reason) => Err(reason),
        }
    }

    fn send(&mut self, session: Option<SessionId>, kind: CommandKind) {
        link::write_line(&mut self.writer, &Command { session, kind }).unwrap();
    }

    fn next(&mut self) -> Envelope {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).expect("no envelope");
        assert!(read > 0, "the link closed");
        serde_json::from_str(&line).unwrap()
    }

    /// Envelopes up to and including the first one `done` accepts.
    fn until(&mut self, done: impl Fn(&Envelope) -> bool) -> Vec<Envelope> {
        let mut seen = Vec::new();
        loop {
            let envelope = self.next();
            let last = done(&envelope);
            seen.push(envelope);
            if last {
                return seen;
            }
        }
    }
}

fn host_event(envelope: &Envelope) -> Option<&HostEvent> {
    match &envelope.event {
        Event::Host(event) => Some(event),
        Event::Loop(_) => None,
    }
}

fn notice_with(envelope: &Envelope, part: &str) -> bool {
    matches!(
        host_event(envelope),
        Some(HostEvent::Notice { level: NoticeLevel::Error, text, .. }) if text.contains(part)
    )
}

fn text_of(envelopes: &[Envelope], session: SessionId) -> String {
    envelopes
        .iter()
        .filter(|envelope| envelope.session == session)
        .filter_map(|envelope| match &envelope.event {
            Event::Loop(LoopEvent::TextDelta { delta, .. }) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

fn hosts(host: &Host, id: SessionId) -> bool {
    host.engine
        .hosted_sessions()
        .iter()
        .any(|(hosted, _)| *hosted == id)
}

#[test]
fn a_link_attaching_mid_turn_sees_the_flight_once_and_its_answer_wins() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().display().to_string();
    let h = serve_paused(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "Hel".into(),
            }),
            Ok(ProviderEvent::ToolCallStart {
                id: ToolCallId::new("call_1"),
                name: "ls".into(),
            }),
        ],
        vec![
            Ok(ProviderEvent::ToolCallEnd {
                id: ToolCallId::new("call_1"),
                input: serde_json::json!({ "path": cwd }),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: CoreStopReason::ToolUse,
                usage: TokenUsage::default(),
            }),
        ],
        vec![text_turn("done"), text_turn("titled")],
        dir.path(),
        dir.path(),
    );
    let prompt_end = prompt_async(&h.client, &h.session, "go");
    until(|| h.paused.is_parked());

    let mut link = LinkClient::attach(&h.host, h.id).unwrap();
    let call_1 = |envelope: &Envelope| matches!(&envelope.event, Event::Loop(LoopEvent::ToolCallStart { id, .. }) if id.0 == "call_1");
    let mut seen = link.until(call_1);
    let Some(HostEvent::SessionChanged { messages, .. }) = host_event(&seen[0]) else {
        panic!("the link opened with {:?}", seen[0]);
    };
    assert_eq!(messages.len(), 1, "the recorded prompt");
    assert!(seen.iter().any(|envelope| matches!(
        host_event(envelope),
        Some(HostEvent::StateChanged { state }) if state.working
    )));

    link.send(
        None,
        CommandKind::Prompt {
            content: vec![text("me too")],
            delivery: Delivery::Queue,
        },
    );
    link.until(|envelope| notice_with(envelope, "busy"));

    h.release.send(()).unwrap();
    let (ask, _) = until_ask(&h.inbox, &mut Vec::new());
    let asked = link.until(|envelope| {
        matches!(
            host_event(envelope),
            Some(HostEvent::PermissionRequested { .. })
        )
    });
    let Some(HostEvent::PermissionRequested { request_id, .. }) = host_event(asked.last().unwrap())
    else {
        unreachable!();
    };
    link.send(
        None,
        CommandKind::ResolvePermission {
            request_id: *request_id,
            decision: PermissionDecision::AllowOnce,
        },
    );
    wait_cancel(&h.inbox, &ask);
    seen.extend(asked);
    seen.extend(link.until(|envelope| {
        envelope.session == h.id && matches!(host_event(envelope), Some(HostEvent::RunEnded { .. }))
    }));
    let response = prompt_end.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(response["stopReason"], "end_turn");
    assert_eq!(text_of(&seen, h.id), "Heldone");
}

#[test]
fn a_prompt_from_another_client_waits_for_the_links_run() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_paused(
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta { delta: "a".into() }),
        ],
        vec![Ok(ProviderEvent::MessageEnd {
            stop_reason: CoreStopReason::EndTurn,
            usage: TokenUsage::default(),
        })],
        vec![text_turn("titled")],
        dir.path(),
        dir.path(),
    );
    let mut link = LinkClient::attach(&h.host, h.id).unwrap();
    link.send(
        None,
        CommandKind::Prompt {
            content: vec![text("go")],
            delivery: Delivery::Queue,
        },
    );
    until(|| h.paused.is_parked());
    let refused = prompt_async(&h.client, &h.session, "me too")
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap_err();
    assert!(refused.message.contains("busy"), "{}", refused.message);
    h.release.send(()).unwrap();
    let seen = link.until(|envelope| {
        envelope.session == h.id && matches!(host_event(envelope), Some(HostEvent::RunEnded { .. }))
    });
    assert_eq!(text_of(&seen, h.id), "a");
}

#[test]
fn a_link_holds_the_session_until_it_disconnects() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_paused(Vec::new(), Vec::new(), Vec::new(), dir.path(), dir.path());
    let link = LinkClient::attach(&h.host, h.id).unwrap();
    h.client
        .request("session/close", serde_json::json!({"sessionId": h.session}))
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(hosts(&h.host, h.id), "the link still holds the session");
    drop(link);
    until(|| !hosts(&h.host, h.id));
}

#[test]
fn a_link_is_refused_what_it_may_not_touch() {
    let dir = tempfile::tempdir().unwrap();
    let h = serve_paused(Vec::new(), Vec::new(), Vec::new(), dir.path(), dir.path());
    let stranger = SessionId::new();
    let refused = LinkClient::attach(&h.host, stranger).err().unwrap();
    assert!(refused.contains("does not host"), "{refused}");

    let mut link = LinkClient::attach(&h.host, h.id).unwrap();
    link.send(None, CommandKind::NewSession);
    link.until(|envelope| notice_with(envelope, "not available"));
    link.send(Some(stranger), CommandKind::Cancel);
    link.until(|envelope| notice_with(envelope, "not part of the attached session"));
    assert!(hosts(&h.host, h.id));
}
