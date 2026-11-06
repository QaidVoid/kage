//! Bounded recording of an external session's presentation history.
//!
//! The upstream event path only enqueues, within an item and byte budget. A
//! worker owned by the session slot creates the file and appends records, so
//! a slow disk never blocks upstream draining. Overflow stops recording for
//! the rest of the session, and the worker marks the gap after the records
//! already queued. A failed write also stops recording, leaving the prefix
//! that reached the disk. A turn without its end therefore means the session
//! was interrupted. Records are display history, never upstream state.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use kage_jsonrpc::RpcError;
use kage_session::SessionWriter;
use kage_session::entry::{
    EXTERNAL_ACP_TEXT_LIMIT, EntryId, ExternalAcpPresentation, Header, PresentationText,
    PresentationToolStatus,
};
use serde_json::Value;

use super::lifecycle::Slot;

const MAX_ITEMS: usize = 1024;
const MAX_BYTES: usize = 4 * 1024 * 1024;
// Items written with one fsync.
const BATCH: usize = 64;

const RECORDING: u8 = 0;
const GAP: u8 = 1;
const STOPPED: u8 = 2;

// `None` wakes the worker to mark a gap.
type Item = Option<(ExternalAcpPresentation, usize)>;

/// Records presentation events of one external session.
pub(crate) struct Recorder {
    sender: mpsc::SyncSender<Item>,
    bytes: Arc<AtomicUsize>,
    state: Arc<AtomicU8>,
    discard: Arc<AtomicBool>,
}

/// Whether to create the session file or append to an existing one.
pub(crate) enum Target {
    Create(Box<Header>),
    Append,
}

impl Recorder {
    /// Open the session file on a slot worker and wait up to `timeout` for
    /// it. The session must not be offered as persistent if this fails. A
    /// file created after the wait gave up is removed.
    pub(crate) fn start(
        slot: &Arc<Slot>,
        path: PathBuf,
        target: Target,
        timeout: Duration,
    ) -> Result<Self, RpcError> {
        let (sender, items) = mpsc::sync_channel::<Item>(MAX_ITEMS);
        let (opened_tx, opened) = mpsc::channel();
        let bytes = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(AtomicU8::new(RECORDING));
        let discard = Arc::new(AtomicBool::new(false));
        let worker = (Arc::clone(&bytes), Arc::clone(&state), Arc::clone(&discard));
        slot.spawn("kage-external-recorder", move || {
            let (bytes, state, discard) = worker;
            let created = matches!(target, Target::Create(_));
            let opened = match target {
                Target::Create(header) => SessionWriter::create(&path, *header),
                Target::Append => SessionWriter::open(&path),
            };
            let writer = match opened {
                Ok(writer) => writer,
                Err(error) => {
                    state.store(STOPPED, Ordering::SeqCst);
                    let _ = opened_tx.send(Err(error.to_string()));
                    return;
                }
            };
            if opened_tx.send(Ok(())).is_err() {
                discard.store(true, Ordering::SeqCst);
            }
            write_all(writer, &items, &bytes, &state);
            if created && discard.load(Ordering::SeqCst) {
                let _ = std::fs::remove_file(&path);
            }
        })
        .map_err(|e| RpcError::internal(format!("cannot start external recording: {e}")))?;
        match opened.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(Self {
                sender,
                bytes,
                state,
                discard,
            }),
            Ok(Err(error)) => Err(RpcError::internal(format!(
                "cannot open external session file: {error}"
            ))),
            Err(_) => {
                discard.store(true, Ordering::SeqCst);
                Err(RpcError::internal(
                    "external session file opening timed out",
                ))
            }
        }
    }

    /// Queue `event` without blocking. Call only under the session lock, so
    /// records keep the order in which the session observed them.
    pub(crate) fn record(&self, event: ExternalAcpPresentation) {
        if self.state.load(Ordering::SeqCst) != RECORDING {
            return;
        }
        let size = event_size(&event);
        let admitted = self
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(size).filter(|total| *total <= MAX_BYTES)
            })
            .is_ok();
        if !admitted {
            self.stop_with_gap();
            return;
        }
        if self.sender.try_send(Some((event, size))).is_err() {
            self.bytes.fetch_sub(size, Ordering::AcqRel);
            self.stop_with_gap();
        }
    }

    /// Whether every event so far was queued and written or is pending.
    pub(crate) fn complete(&self) -> bool {
        self.state.load(Ordering::SeqCst) == RECORDING
    }

    /// Remove a file this recorder created once its worker finishes, because
    /// the session it was created for never started.
    pub(crate) fn discard(self) {
        self.discard.store(true, Ordering::SeqCst);
    }

    // A full queue means the worker is still draining and will see the gap.
    fn stop_with_gap(&self) {
        if self
            .state
            .compare_exchange(RECORDING, GAP, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let _ = self.sender.try_send(None);
        }
    }
}

// Writes queued events in batches. Recording stops for good at the first
// failed write. After an overflow, the gap is marked as soon as the events
// queued before it are written, so it survives a later crash.
fn write_all(
    mut writer: SessionWriter,
    items: &mpsc::Receiver<Item>,
    bytes: &AtomicUsize,
    state: &AtomicU8,
) {
    let mut broken = false;
    let mut marked = false;
    while let Ok(first) = items.recv() {
        let mut batch: Vec<_> = first.into_iter().collect();
        let mut drained = true;
        loop {
            if batch.len() >= BATCH {
                drained = false;
                break;
            }
            match items.try_recv() {
                Ok(item) => batch.extend(item),
                Err(_) => break,
            }
        }
        let size: usize = batch.iter().map(|(_, size)| size).sum();
        let mut events: Vec<_> = batch.into_iter().map(|(event, _)| event).collect();
        if drained && !marked && state.load(Ordering::SeqCst) == GAP {
            events.push(ExternalAcpPresentation::Gap);
            marked = true;
        }
        if !broken && !events.is_empty() && !append(&mut writer, events) {
            broken = true;
            state.store(STOPPED, Ordering::SeqCst);
        }
        bytes.fetch_sub(size, Ordering::AcqRel);
    }
    if !broken && !marked && state.load(Ordering::SeqCst) == GAP {
        append(&mut writer, vec![ExternalAcpPresentation::Gap]);
    }
}

fn append(writer: &mut SessionWriter, events: Vec<ExternalAcpPresentation>) -> bool {
    let mut lines = Vec::new();
    for event in events {
        let entry = event.into_entry(EntryId::new(), chrono::Utc::now());
        if serde_json::to_writer(&mut lines, &entry).is_err() {
            return false;
        }
        lines.push(b'\n');
    }
    writer.append_lines(&lines).is_ok()
}

fn event_size(event: &ExternalAcpPresentation) -> usize {
    let len = |text: &PresentationText| text.as_str().len();
    let optional = |text: &Option<PresentationText>| text.as_ref().map_or(0, len);
    256 + match event {
        ExternalAcpPresentation::Prompt { text } => len(text),
        ExternalAcpPresentation::AgentMessage { item_id, text }
        | ExternalAcpPresentation::AgentThought { item_id, text } => len(item_id) + len(text),
        ExternalAcpPresentation::ToolCall {
            tool_call_id,
            title,
            ..
        } => len(tool_call_id) + len(title),
        ExternalAcpPresentation::ToolCallUpdate {
            tool_call_id,
            title,
            text,
            ..
        } => len(tool_call_id) + optional(title) + optional(text),
        ExternalAcpPresentation::TurnEnd { .. } | ExternalAcpPresentation::Gap => 0,
    }
}

/// Bounded fragments of `text`, split on character boundaries.
fn fragments(text: &str) -> Vec<PresentationText> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(EXTERNAL_ACP_TEXT_LIMIT);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (head, tail) = rest.split_at(end);
        out.push(PresentationText::new(head.to_owned()).expect("bounded fragment"));
        rest = tail;
    }
    out
}

/// `text`, cut to the presentation limit on a character boundary.
pub(crate) fn bounded(text: &str) -> PresentationText {
    fragments(text)
        .into_iter()
        .next()
        .unwrap_or_else(|| PresentationText::new(String::new()).expect("empty text"))
}

fn status(update: &Value) -> Option<PresentationToolStatus> {
    Some(match update.get("status")?.as_str()? {
        "pending" => PresentationToolStatus::Pending,
        "in_progress" => PresentationToolStatus::InProgress,
        "completed" => PresentationToolStatus::Completed,
        "failed" => PresentationToolStatus::Failed,
        _ => PresentationToolStatus::Unknown,
    })
}

fn content_text(content: &Value) -> Option<&str> {
    (content.get("type")?.as_str()? == "text")
        .then(|| content.get("text")?.as_str())
        .flatten()
}

/// The display events of one upstream `session/update`, in `turn`.
///
/// Only message and thought text and tool call titles and statuses are kept.
/// Tool output, raw tool input, other content and metadata are omitted,
/// because they can carry secrets. Titles can still name a command.
pub(crate) fn presentation(params: &Value, turn: u64) -> Vec<ExternalAcpPresentation> {
    let Some(update) = params.get("update") else {
        return Vec::new();
    };
    let item_id = || bounded(&format!("turn-{turn}"));
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some(kind @ ("agent_message_chunk" | "agent_thought_chunk")) => update
            .get("content")
            .and_then(content_text)
            .map(fragments)
            .unwrap_or_default()
            .into_iter()
            .map(|text| {
                if kind == "agent_message_chunk" {
                    ExternalAcpPresentation::AgentMessage {
                        item_id: item_id(),
                        text,
                    }
                } else {
                    ExternalAcpPresentation::AgentThought {
                        item_id: item_id(),
                        text,
                    }
                }
            })
            .collect(),
        Some("tool_call") => {
            let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
                return Vec::new();
            };
            vec![ExternalAcpPresentation::ToolCall {
                tool_call_id: bounded(id),
                title: bounded(update.get("title").and_then(Value::as_str).unwrap_or("")),
                status: status(update).unwrap_or(PresentationToolStatus::Pending),
            }]
        }
        Some("tool_call_update") => {
            let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
                return Vec::new();
            };
            vec![ExternalAcpPresentation::ToolCallUpdate {
                tool_call_id: bounded(id),
                title: update.get("title").and_then(Value::as_str).map(bounded),
                status: status(update),
                text: None,
            }]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::external::lifecycle::Lifecycles;
    use kage_session::entry::{ExternalAcpSession, SessionBackend};
    use serde_json::json;

    const WAIT: Duration = Duration::from_secs(5);

    fn header(dir: &std::path::Path) -> Header {
        let (_, mut header) = crate::plan_session_in(dir, "external", "");
        header.cwd = dir.to_path_buf();
        header.backend = Some(SessionBackend::ExternalAcp(ExternalAcpSession {
            agent_name: "fake".into(),
            protocol_version: 1,
            upstream_session_id: "up".into(),
        }));
        header
    }

    fn text(event: &ExternalAcpPresentation) -> Option<&str> {
        match event {
            ExternalAcpPresentation::Prompt { text }
            | ExternalAcpPresentation::AgentMessage { text, .. } => Some(text.as_str()),
            _ => None,
        }
    }

    #[test]
    fn records_in_order_and_replays_for_display() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let slot = Lifecycles::new(1).reserve().unwrap().commit();
        let recorder = Recorder::start(
            &slot,
            path.clone(),
            Target::Create(Box::new(header(dir.path()))),
            WAIT,
        )
        .unwrap();
        recorder.record(ExternalAcpPresentation::Prompt {
            text: bounded("hi"),
        });
        let update = json!({"sessionId": "up", "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hello"}
        }});
        for event in presentation(&update, 1) {
            recorder.record(event);
        }
        recorder.record(ExternalAcpPresentation::TurnEnd {
            outcome: kage_session::entry::PresentationTurnOutcome::EndTurn,
        });
        assert!(recorder.complete());
        drop(recorder);
        slot.terminate();
        assert!(slot.wait_finished(WAIT));
        let replay = kage_session::external_replay::replay_external(&path).unwrap();
        let texts: Vec<_> = replay.events.iter().filter_map(text).collect();
        assert_eq!(texts, ["hi", "hello"]);
        assert!(matches!(
            replay.events.last(),
            Some(ExternalAcpPresentation::TurnEnd { .. })
        ));
    }

    #[test]
    fn overflow_stops_recording_and_marks_a_gap_after_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let slot = Lifecycles::new(1).reserve().unwrap().commit();
        let recorder = Recorder::start(
            &slot,
            path.clone(),
            Target::Create(Box::new(header(dir.path()))),
            WAIT,
        )
        .unwrap();
        recorder.record(ExternalAcpPresentation::Prompt {
            text: bounded("kept"),
        });
        // The worker is idle once this is on disk, which is when a gap used
        // to wait for the session to close.
        let until = std::time::Instant::now() + WAIT;
        while kage_session::external_replay::replay_external(&path)
            .unwrap()
            .events
            .is_empty()
        {
            assert!(
                std::time::Instant::now() < until,
                "first event was not written"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        recorder.bytes.fetch_add(MAX_BYTES, Ordering::AcqRel);
        recorder.record(ExternalAcpPresentation::Prompt {
            text: bounded("lost"),
        });
        recorder.bytes.fetch_sub(MAX_BYTES, Ordering::AcqRel);
        recorder.record(ExternalAcpPresentation::Prompt {
            text: bounded("after"),
        });
        assert!(!recorder.complete());
        // The gap reaches the disk while the session is still recording, so a
        // crash cannot hide it.
        let until = std::time::Instant::now() + WAIT;
        while !kage_session::external_replay::replay_external(&path)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event, ExternalAcpPresentation::Gap))
        {
            assert!(std::time::Instant::now() < until, "gap was not written");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(recorder);
        slot.terminate();
        assert!(slot.wait_finished(WAIT));
        let replay = kage_session::external_replay::replay_external(&path).unwrap();
        assert_eq!(replay.events.len(), 2);
        assert_eq!(text(&replay.events[0]), Some("kept"));
        assert!(matches!(replay.events[1], ExternalAcpPresentation::Gap));
    }

    #[test]
    fn discarded_recording_removes_the_file_it_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let slot = Lifecycles::new(1).reserve().unwrap().commit();
        let recorder = Recorder::start(
            &slot,
            path.clone(),
            Target::Create(Box::new(header(dir.path()))),
            WAIT,
        )
        .unwrap();
        assert!(path.exists());
        recorder.discard();
        slot.terminate();
        assert!(slot.wait_finished(WAIT));
        assert!(!path.exists());
    }

    #[test]
    fn appending_keeps_the_existing_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let slot = Lifecycles::new(2).reserve().unwrap().commit();
        let first = Recorder::start(
            &slot,
            path.clone(),
            Target::Create(Box::new(header(dir.path()))),
            WAIT,
        )
        .unwrap();
        first.record(ExternalAcpPresentation::Prompt {
            text: bounded("before"),
        });
        drop(first);
        let reopened = loop {
            match Recorder::start(&slot, path.clone(), Target::Append, WAIT) {
                Ok(recorder) => break recorder,
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        reopened.record(ExternalAcpPresentation::Prompt {
            text: bounded("after"),
        });
        reopened.discard();
        slot.terminate();
        assert!(slot.wait_finished(WAIT));
        let replay = kage_session::external_replay::replay_external(&path).unwrap();
        let texts: Vec<_> = replay.events.iter().filter_map(text).collect();
        assert_eq!(texts, ["before", "after"]);
    }

    #[test]
    fn failed_file_creation_is_reported_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "taken").unwrap();
        let slot = Lifecycles::new(1).reserve().unwrap().commit();
        assert!(
            Recorder::start(
                &slot,
                path.clone(),
                Target::Create(Box::new(header(dir.path()))),
                WAIT
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "taken");
        slot.terminate();
        assert!(slot.wait_finished(WAIT));
    }

    #[test]
    fn presentation_keeps_display_text_only() {
        let call = json!({"sessionId": "up", "update": {
            "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed",
            "rawOutput": {"secret": true},
            "content": [{"type": "content", "content": {"type": "text", "text": "ok"}},
                        {"type": "diff", "path": "/x", "newText": "y"}]
        }});
        let events = presentation(&call, 1);
        assert_eq!(events.len(), 1);
        let ExternalAcpPresentation::ToolCallUpdate {
            text: shown,
            status,
            ..
        } = &events[0]
        else {
            panic!("expected a tool call update");
        };
        assert!(shown.is_none());
        assert_eq!(*status, Some(PresentationToolStatus::Completed));
        let long = "\u{e9}".repeat(EXTERNAL_ACP_TEXT_LIMIT);
        let chunk = json!({"update": {"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": long}}});
        let joined: String = presentation(&chunk, 1).iter().filter_map(text).collect();
        assert_eq!(joined, long);
        assert!(presentation(&json!({"update": {"sessionUpdate": "plan"}}), 1).is_empty());
    }
}
