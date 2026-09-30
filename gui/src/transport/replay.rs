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
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(not(target_arch = "wasm32"))]
use std::thread;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

use kage_client::Frame;

use super::{Event, EventSender, State, Transport};

/// The golden transcript the replay transport plays, in wire order.
const TRANSCRIPT: &str = include_str!("../../../crates/kage-client/tests/fixtures/fix-tools.jsonl");

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
}

impl ReplayTransport {
    /// A transport nobody has started yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Transport for ReplayTransport {
    fn start(&mut self, events: EventSender) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let transport = self.clone();
            thread::Builder::new()
                .name("kage-replay".to_owned())
                .spawn(move || play_blocking(transport, events))
                .expect("replay thread spawns");
        }
        #[cfg(target_arch = "wasm32")]
        {
            let transport = self.clone();
            wasm_bindgen_futures::spawn_local(async move {
                play_timed(transport, events).await;
            });
        }
    }

    fn send(&self, _frame: Frame) {
        // Outgoing frames are drained and dropped: the recording has
        // already decided what it answers.
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
    for frame in transcript() {
        if transport.stop.load(Ordering::SeqCst) {
            return;
        }
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
    for frame in transcript() {
        if transport.stop.load(Ordering::SeqCst) {
            return;
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
    use kage_client::{Client, Frame, PromptOutcome, TranscriptItem};

    use super::transcript;

    /// Drives the transcript into a client the way the shell does:
    /// handshake first, a session, then the prompt the fixture was
    /// recorded with.
    fn replayed_client() -> Client {
        let mut client = Client::new();
        client.initialize(ClientCapabilities::default(), None);
        client.new_session("/w", &[]);
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
        assert!(matches!(frames[0], Frame::Success { id: 1, .. }));
        assert!(matches!(frames[22], Frame::Success { id: 3, .. }));
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
                TranscriptItem::Thinking { .. } => "thinking",
                TranscriptItem::ToolCall(_) => "tool",
                TranscriptItem::TurnEnd { .. } => "turn-end",
                TranscriptItem::Notice { .. } => "notice",
                TranscriptItem::Compaction { .. } => "compaction",
                TranscriptItem::Plan { .. } => "plan",
            })
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
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
