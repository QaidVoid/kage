//! What the app measures itself about each session's runs.
//!
//! The wire carries no times: a thinking block, a tool call and a run
//! end arrive without a clock. The store stamps them as their frames
//! arrive, so a row can say how long it took. Only what this client
//! watched live is timed; a loaded history arrives complete and shows
//! no durations rather than invented ones.

use std::collections::HashMap;
use std::time::Duration;

use kage_client::wire::{ToolCallStatus, TurnReason};
use kage_client::{Session, TranscriptItem};
use web_time::Instant;

/// One measured span: when it began and, once it ended, how long it
/// took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    start: Instant,
    took: Option<Duration>,
}

impl Span {
    /// How long the span took, once it ended.
    #[must_use]
    pub fn took(&self) -> Option<Duration> {
        self.took
    }
}

/// The end of one watched run: the wall clock it ended at and how long
/// it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunEnd {
    /// Seconds since the Unix epoch when the run ended.
    pub at: i64,
    /// How long the run took from the prompt to its end.
    pub took: Duration,
}

/// The measured times of one session.
#[derive(Debug, Default)]
pub struct SessionTimes {
    /// When the run in flight began, while one runs.
    run_started: Option<Instant>,
    /// Per thinking item index.
    thinking: HashMap<usize, Span>,
    /// Per tool call id.
    tools: HashMap<String, Span>,
    /// Per turn-end item index that closed a run.
    run_ends: HashMap<usize, RunEnd>,
    /// How many items the last observation saw.
    seen: usize,
}

impl SessionTimes {
    /// How long the thinking item at `ix` streamed, once it stopped.
    #[must_use]
    pub fn thinking(&self, ix: usize) -> Option<Duration> {
        self.thinking.get(&ix).and_then(Span::took)
    }

    /// How long the tool call `id` ran, once it ended.
    #[must_use]
    pub fn tool(&self, id: &str) -> Option<Duration> {
        self.tools.get(id).and_then(Span::took)
    }

    /// The end of the run the turn-end item at `ix` closed.
    #[must_use]
    pub fn run_end(&self, ix: usize) -> Option<RunEnd> {
        self.run_ends.get(&ix).copied()
    }

    /// Stamps what moved in `session` since the last observation, at
    /// `now` on the monotonic clock and `unix` on the wall clock.
    fn observe(&mut self, session: &Session, now: Instant, unix: i64) {
        let items = &session.items;
        if items.len() < self.seen {
            // A reload replays the history from scratch: the indices
            // name other items now.
            self.thinking.clear();
            self.run_ends.clear();
        }
        if session.running && self.run_started.is_none() {
            self.run_started = Some(now);
        }
        let last = items.len().checked_sub(1);
        for (ix, item) in items.iter().enumerate() {
            match item {
                TranscriptItem::Thinking { .. } => {
                    if Some(ix) == last && session.running {
                        self.thinking.entry(ix).or_insert(Span {
                            start: now,
                            took: None,
                        });
                    } else {
                        close(self.thinking.get_mut(&ix), now);
                    }
                }
                TranscriptItem::ToolCall(call) => match call.status {
                    ToolCallStatus::Pending | ToolCallStatus::InProgress => {
                        if session.running {
                            self.tools.entry(call.tool_call_id.clone()).or_insert(Span {
                                start: now,
                                took: None,
                            });
                        }
                    }
                    _ => close(self.tools.get_mut(&call.tool_call_id), now),
                },
                TranscriptItem::TurnEnd { reason } if ix >= self.seen => {
                    if *reason != Some(TurnReason::ToolCalls)
                        && let Some(started) = self.run_started
                    {
                        self.run_ends.insert(
                            ix,
                            RunEnd {
                                at: unix,
                                took: now.duration_since(started),
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        if !session.running {
            self.run_started = None;
        }
        self.seen = items.len();
    }
}

/// Freezes an open span at `now`.
fn close(span: Option<&mut Span>, now: Instant) {
    if let Some(span) = span
        && span.took.is_none()
    {
        span.took = Some(now.duration_since(span.start));
    }
}

/// The measured times of every session the store watched.
#[derive(Debug, Default)]
pub struct Timings {
    sessions: HashMap<String, SessionTimes>,
}

impl Timings {
    /// Stamps what moved in `session`.
    pub fn observe(&mut self, session: &Session) {
        self.sessions
            .entry(session.id.clone())
            .or_default()
            .observe(session, Instant::now(), crate::clock::unix_seconds());
    }

    /// The measured times of session `id`, when any were taken.
    #[must_use]
    pub fn session(&self, id: &str) -> Option<&SessionTimes> {
        self.sessions.get(id)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_client::wire::{ToolCallStatus, ToolKind, TurnReason};
    use kage_client::{Session, ToolCallItem, TranscriptItem};
    use web_time::Instant;

    use super::SessionTimes;

    fn call(id: &str, status: ToolCallStatus) -> TranscriptItem {
        TranscriptItem::ToolCall(ToolCallItem {
            tool_call_id: id.to_owned(),
            title: "read".to_owned(),
            kind: ToolKind::Read,
            status,
            input: None,
            swarm: None,
            content: Vec::new(),
            raw_output: None,
        })
    }

    #[test]
    fn a_watched_run_times_its_thinking_tools_and_end() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut session = Session::new("s1");
        let mut times = SessionTimes::default();

        session.running = true;
        session
            .items
            .push(TranscriptItem::Thinking { text: "hm".into() });
        times.observe(&session, at(0), 100);
        session.items.push(call("c1", ToolCallStatus::InProgress));
        times.observe(&session, at(1_500), 101);
        session.items[1] = call("c1", ToolCallStatus::Completed);
        session.items.push(TranscriptItem::TurnEnd {
            reason: Some(TurnReason::ToolCalls),
        });
        session.items.push(TranscriptItem::Assistant {
            text: "done".into(),
        });
        session.items.push(TranscriptItem::TurnEnd {
            reason: Some(TurnReason::NoToolCalls),
        });
        times.observe(&session, at(4_000), 104);
        session.running = false;
        times.observe(&session, at(4_100), 104);

        assert_eq!(times.thinking(0), Some(Duration::from_millis(1_500)));
        assert_eq!(times.tool("c1"), Some(Duration::from_millis(2_500)));
        assert_eq!(times.run_end(2), None, "tools followed that turn");
        let end = times.run_end(4).expect("the run end was watched");
        assert_eq!(end.at, 104);
        assert_eq!(end.took, Duration::from_millis(4_000));
    }

    #[test]
    fn a_loaded_history_is_not_timed() {
        let mut session = Session::new("s1");
        session.items = vec![
            TranscriptItem::Thinking { text: "old".into() },
            call("c1", ToolCallStatus::Completed),
            TranscriptItem::TurnEnd {
                reason: Some(TurnReason::NoToolCalls),
            },
        ];
        let mut times = SessionTimes::default();
        times.observe(&session, Instant::now(), 100);
        assert_eq!(times.thinking(0), None);
        assert_eq!(times.tool("c1"), None);
        assert_eq!(times.run_end(2), None);
    }
}
