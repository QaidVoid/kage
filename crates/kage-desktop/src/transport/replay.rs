//! The replay transport: a recorded golden transcript played back
//! with realistic pacing.
//!
//! The transcript is the client crate's fix-with-tools fixture,
//! embedded at build time, so the shell is fully usable with no
//! engine and no `kage` binary: run with `--replay` and the session
//! plays out on its own, prompts and answers included, because the
//! fixture's answer ids line up with the handshake and prompt the
//! shell sends.
//!
//! Native runs pace the playback on a thread; the browser build has
//! no threads, so it paces the same frames from a timer task
//! instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(not(target_arch = "wasm32"))]
use std::thread;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

use kage_client::Frame;

use super::{Event, EventSender, Link, State, Transport};

/// The golden transcript the replay transport plays, in wire order. A
/// copy of the kage-client fixture, so the published crate carries it.
const TRANSCRIPT: &str = include_str!("../../assets/replay/fix-tools.jsonl");

/// The pause between two played frames, in milliseconds.
const BEAT_MILLIS: u32 = 100;

/// The pause between two played frames.
#[cfg(not(target_arch = "wasm32"))]
const BEAT: Duration = Duration::from_millis(BEAT_MILLIS as u64);

/// The transcript as frames, in file order. Every line must parse.
///
/// # Panics
///
/// Panics when the embedded fixture stopped parsing, which is a bug
/// in the crate that records it.
#[must_use]
pub fn transcript() -> Vec<Frame> {
    TRANSCRIPT
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("fixture line is not JSON: {error}"));
            Frame::parse(&value)
                .unwrap_or_else(|| panic!("fixture line is not a JSON-RPC frame: {line}"))
        })
        .collect()
}

/// The replay transport.
#[derive(Clone, Default)]
pub struct ReplayTransport {
    stop: Arc<AtomicBool>,
    /// The highest outgoing request id the shell sent, so the
    /// playback never hands an answer over before the request it
    /// answers is in flight: a slow frame makes the pump deliver the
    /// boot answer and the session answer in one drain, and an
    /// answer with no request waiting for it is dropped unread.
    asked: Arc<AtomicU64>,
}

impl ReplayTransport {
    /// A transport nobody has started yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The wire id of an answer frame, `None` for the rest.
    fn answer_id(frame: &Frame) -> Option<u64> {
        match frame {
            Frame::Success { id, .. } | Frame::Failure { id, .. } => id.as_number(),
            _ => None,
        }
    }

    /// The request id `frame` must wait for before it plays, given the
    /// last answer played so far, which it updates. An answer waits for
    /// its own request; anything after an answer waits for the request
    /// that follows it, so a turn's updates never land before the
    /// prompt that started the turn.
    fn gate(frame: &Frame, answered: &mut u64) -> u64 {
        match Self::answer_id(frame) {
            Some(id) => {
                *answered = id;
                id
            }
            None if *answered > 0 => *answered + 1,
            None => 0,
        }
    }

    /// Waits until the shell has sent requests up to `id`, so the
    /// answer about to play lands on the request it answers.
    #[cfg(not(target_arch = "wasm32"))]
    fn wait_for(&self, id: u64) {
        while self.asked.load(Ordering::SeqCst) < id && !self.stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Transport for ReplayTransport {
    fn link(&self) -> Link {
        Link {
            name: "replay",
            detail: "recorded transcript".to_owned(),
        }
    }

    fn start(&mut self, events: EventSender) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let transport = self.clone();
            let spawned = thread::Builder::new()
                .name("kage-replay".to_owned())
                .spawn({
                    let events = events.clone();
                    move || play_blocking(transport, events)
                });
            if let Err(error) = spawned {
                let _ = events.send_blocking(Event::State(State::Refused(format!(
                    "cannot start the replay thread: {error}"
                ))));
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let transport = self.clone();
            wasm_bindgen_futures::spawn_local(async move {
                play_timed(transport, events).await;
            });
        }
    }

    fn send(&self, frame: Frame) {
        // Outgoing frames are drained and dropped: the recording has
        // already decided what it answers. Their ids gate the
        // playback, so an answer never outruns its request.
        if let Frame::Request { id, .. } = &frame
            && let Some(n) = id.as_number()
        {
            self.asked.fetch_max(n, Ordering::SeqCst);
        }
    }

    fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Plays the recording on a thread, blocking between frames. The
/// transcript ends but the link stays up: the shell remains usable
/// against the recording.
#[cfg(not(target_arch = "wasm32"))]
fn play_blocking(transport: ReplayTransport, events: EventSender) {
    let _ = events.send_blocking(Event::State(State::Connecting));
    let _ = events.send_blocking(Event::State(State::Connected));
    let mut answered = 0;
    for frame in transcript() {
        if transport.stop.load(Ordering::SeqCst) {
            return;
        }
        transport.wait_for(ReplayTransport::gate(&frame, &mut answered));
        thread::sleep(BEAT);
        if events.send_blocking(Event::Frame(frame)).is_err() {
            return;
        }
    }
}

/// Plays the recording from a timer task, awaiting between frames.
/// The transcript ends but the link stays up: the shell remains
/// usable against the recording.
#[cfg(target_arch = "wasm32")]
async fn play_timed(transport: ReplayTransport, events: EventSender) {
    let _ = events.send(Event::State(State::Connecting)).await;
    let _ = events.send(Event::State(State::Connected)).await;
    let mut answered = 0;
    for frame in transcript() {
        if transport.stop.load(Ordering::SeqCst) {
            return;
        }
        let id = ReplayTransport::gate(&frame, &mut answered);
        while transport.asked.load(Ordering::SeqCst) < id {
            crate::transport::web::sleep(5).await;
        }
        crate::transport::web::sleep(BEAT_MILLIS).await;
        if events.send(Event::Frame(frame)).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use kage_client::wire::{ClientCapabilities, ContentBlock, StopReason};
    use kage_client::{Client, Frame, PromptOutcome, RequestId, TranscriptItem};

    use super::ReplayTransport;
    use super::transcript;
    use crate::transport::{Event, Transport};

    #[test]
    fn the_transcript_matches_the_client_fixture() {
        assert_eq!(
            super::TRANSCRIPT,
            include_str!("../../../kage-client/tests/fixtures/fix-tools.jsonl"),
            "copy the kage-client fixture to assets/replay again"
        );
    }

    /// The replay holds an answer back until the shell's request with
    /// the same id is in flight: a slow frame would otherwise make
    /// the pump deliver the boot answer and the session answer in
    /// one drain, and an answer with no request waiting for it is
    /// dropped unread, so the shell never follows the recording's
    /// session.
    #[test]
    fn the_playback_waits_for_the_request_each_answer_answers() {
        let (events, incoming) = async_channel::unbounded();
        let mut transport = ReplayTransport::new();
        transport.start(events);
        // The connect states lead, then the boot answer waits for the
        // initialize request.
        let _ = incoming.recv_blocking().expect("the states lead");
        let _ = incoming.recv_blocking().expect("the states lead");
        for _ in 0..40 {
            if incoming.try_recv().is_ok() {
                panic!("the playback answered a request nobody sent");
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        // The shell's requests let the matching answers through, in
        // the recording's order.
        transport.send(Frame::Request {
            id: RequestId::Number(1),
            method: "initialize".to_owned(),
            params: serde_json::json!({}),
        });
        let boot = incoming.recv_blocking().expect("the boot answer plays");
        assert!(
            matches!(&boot, Event::Frame(_)),
            "the first playback frame is the boot answer"
        );
        transport.send(Frame::Request {
            id: RequestId::Number(2),
            method: "session/new".to_owned(),
            params: serde_json::json!({}),
        });
        let session = incoming.recv_blocking().expect("the session answer plays");
        assert!(
            matches!(&session, Event::Frame(_)),
            "the second playback frame is the session answer"
        );
        transport.close();
    }

    /// A turn's updates wait for the prompt after the session answer,
    /// so they never land ahead of the user message that started it.
    #[test]
    fn updates_wait_for_the_request_after_the_last_answer() {
        let mut answered = 0;
        let gates: Vec<u64> = transcript()
            .iter()
            .map(|frame| ReplayTransport::gate(frame, &mut answered))
            .collect();
        assert_eq!(
            &gates[..3],
            &[1, 2, 3],
            "the boot, the session, then the turn"
        );
        assert!(
            gates[2..gates.len() - 1].iter().all(|gate| *gate == 3),
            "every update of the turn waits for the prompt"
        );
        assert_eq!(
            gates.last(),
            Some(&3),
            "the prompt's answer waits for the prompt"
        );
    }

    /// Drives the transcript into a client the way the shell does:
    /// handshake first, a session, then the prompt the fixture was
    /// recorded with.
    fn replayed_client() -> Client {
        let mut client = Client::new();
        client.initialize(ClientCapabilities::default(), None);
        client.new_session("/w", &[], None);
        let _ = client.take_outgoing();
        let frames = transcript();
        client.handle(frames[0].clone());
        client.handle(frames[1].clone());

        let session_id = client
            .state()
            .sessions
            .keys()
            .next()
            .cloned()
            .expect("the transcript opened a session");
        assert_eq!(
            client.prompt(&session_id, vec![ContentBlock::text("fix the null check")]),
            PromptOutcome::Sent { request_id: 3 }
        );
        let _ = client.take_outgoing();
        for frame in &frames[2..] {
            client.handle(frame.clone());
        }
        client
    }

    #[test]
    fn the_transcript_parses_whole() {
        let frames = transcript();
        assert_eq!(frames.len(), 23);
        assert!(matches!(
            frames[0],
            Frame::Success {
                id: RequestId::Number(1),
                ..
            }
        ));
        assert!(matches!(
            frames[22],
            Frame::Success {
                id: RequestId::Number(3),
                ..
            }
        ));
    }

    #[test]
    fn the_transcript_leaves_the_asserted_state_behind() {
        let client = replayed_client();
        let state = client.state();
        assert_eq!(state.protocol_version, Some(1));
        assert_eq!(
            state.agent.as_ref().map(|agent| agent.name.as_str()),
            Some("kage")
        );
        assert_eq!(state.sessions.len(), 1);

        let session = state.sessions.values().next().unwrap();
        assert!(session.opened);
        assert_eq!(
            session.title.as_deref(),
            Some("Fix the null dereference in main")
        );
        assert_eq!(session.last_stop, Some(StopReason::EndTurn));
        assert!(!session.running);
        assert_eq!((session.usage.used, session.usage.size), (12_480, 200_000));
        assert_eq!(session.config_options.len(), 3);

        let kinds = session
            .items
            .iter()
            .map(|item| match item {
                TranscriptItem::User { .. } => "user",
                TranscriptItem::Assistant { .. } => "assistant",
                TranscriptItem::Media { .. } => "media",
                TranscriptItem::Thinking { .. } => "thinking",
                TranscriptItem::ToolCall(_) => "tool",
                TranscriptItem::TurnEnd { .. } => "turn-end",
                TranscriptItem::Notice { .. } => "notice",
                TranscriptItem::Compaction { .. } => "compaction",
                TranscriptItem::Plan { .. } => "plan",
                TranscriptItem::Decision { .. } => "decision",
            })
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                "user",
                "tool",
                "turn-end",
                "thinking",
                "assistant",
                "tool",
                "tool",
                "plan",
                "turn-end",
                "assistant",
                "turn-end",
            ]
        );
    }
}
