//! The transports that carry ACP frames between the shell and an
//! engine.
//!
//! Every transport implements [`Transport`]: it connects in the
//! background, hands incoming [`Frame`]s to the shell through an
//! [`async_channel`], and accepts outgoing frames from anywhere. The
//! shell never sees threads, sockets or subprocesses, only events.
//!
//! Three implementations live here: [`stdio`](self::stdio) spawns
//! `kage rpc` as a child, [`ws`](self::ws) dials `kage serve` over a
//! WebSocket and reconnects with backoff, and [`replay`](self::replay)
//! plays a recorded golden transcript so the shell is fully usable
//! with no engine on the machine.

pub mod replay;
pub mod stdio;
pub mod ws;

use std::time::Duration;

use kage_client::Frame;

/// One message a transport hands the shell.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A frame arrived from the engine, ready for
    /// [`kage_client::Client::handle`].
    Frame(Frame),
    /// The connect state moved.
    State(State),
}

/// The sender half of a transport's event stream.
pub type EventSender = async_channel::Sender<Event>;

/// Where a connection stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Dialing or spawning, nothing heard yet.
    Connecting,
    /// The link is up.
    Connected,
    /// The other side said no and will keep saying no: a bad token,
    /// a wrong endpoint. Retrying is pointless until something the
    /// user changes is different.
    Refused(String),
    /// The link dropped and a retry is scheduled.
    Reconnecting {
        /// Which retry this is, counting from one per lost link.
        attempt: u32,
        /// How long the transport waits before the attempt.
        delay: Duration,
    },
    /// Shut down for good.
    Closed,
}

impl State {
    /// The short form a status bar shows.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => "connecting".to_owned(),
            Self::Connected => "connected".to_owned(),
            Self::Refused(why) => format!("refused: {why}"),
            Self::Reconnecting { attempt, delay } => {
                format!("reconnecting, attempt {} in {}s", attempt, delay.as_secs())
            }
            Self::Closed => "closed".to_owned(),
        }
    }

    /// Whether frames can flow: only a live link carries them.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// A transport behind the shell.
///
/// The shell starts it once, then only calls [`Transport::send`] and
/// drops it. Dropping must release everything the transport holds:
/// children die, sockets close, threads end.
pub trait Transport: 'static {
    /// Begins connecting and reports progress into `events`. Called
    /// once, before any [`Transport::send`].
    fn start(&mut self, events: EventSender);

    /// Hands one outgoing frame to the engine. Frames handed before a
    /// link exists are dropped; the client that produced them still
    /// holds its pending entry.
    fn send(&self, frame: Frame);

    /// Closes the link for good: no more retries, no more events
    /// after [`State::Closed`].
    fn close(&self);

    /// The engine's connection id for diagnostics, when the transport
    /// learned one.
    fn connection_id(&self) -> Option<String> {
        None
    }
}

/// The reconnect delay ladder: 1s doubling to a 30s cap.
#[derive(Debug, Clone)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    /// The delay before the first retry.
    pub const BASE: Duration = Duration::from_secs(1);
    /// The delay every retry after the fifth waits.
    pub const CAP: Duration = Duration::from_secs(30);

    /// A ladder ready for the first retry.
    #[must_use]
    pub fn new() -> Self {
        Self { attempt: 0 }
    }

    /// The next delay, one rung further up.
    pub fn next(&mut self) -> Duration {
        let shift = self.attempt.min(5);
        self.attempt += 1;
        Self::BASE.saturating_mul(1 << shift).min(Self::CAP)
    }

    /// Rewinds to the first rung, after a link came back.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Backoff, State};

    #[test]
    fn backoff_doubles_from_one_second_to_the_cap() {
        let mut backoff = Backoff::new();
        let delays: Vec<u64> = (0..8).map(|_| backoff.next().as_secs()).collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn backoff_resets_after_a_link_returns() {
        let mut backoff = Backoff::new();
        for _ in 0..4 {
            backoff.next();
        }
        backoff.reset();
        assert_eq!(backoff.next(), Duration::from_secs(1));
    }

    #[test]
    fn state_labels_read_like_a_status_bar() {
        assert_eq!(State::Connecting.label(), "connecting");
        assert_eq!(State::Connected.label(), "connected");
        assert_eq!(
            State::Refused("bad token".into()).label(),
            "refused: bad token"
        );
        assert_eq!(
            State::Reconnecting {
                attempt: 3,
                delay: Duration::from_secs(4)
            }
            .label(),
            "reconnecting, attempt 3 in 4s"
        );
        assert_eq!(State::Closed.label(), "closed");
        assert!(State::Connected.is_connected());
        assert!(
            !State::Reconnecting {
                attempt: 1,
                delay: Duration::from_secs(1)
            }
            .is_connected()
        );
    }
}
