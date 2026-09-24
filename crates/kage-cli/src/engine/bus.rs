//! Synchronous event fan-out with per-session sequence numbers.

use std::collections::HashMap;
use std::sync::Mutex;

use kage_core::SessionId;
use kage_core::protocol::{Envelope, Event};
use kage_core::sync::lock;

/// Receives every envelope the engine publishes, in order.
///
/// Subscribers run on the publishing thread while the bus is locked, so
/// they must return quickly and must not publish.
pub(crate) type Subscriber = Box<dyn FnMut(&Envelope) + Send>;

/// Stamps each event with its session's next sequence number and hands it
/// to every subscriber.
pub(crate) struct Bus {
    inner: Mutex<Inner>,
}

struct Inner {
    seqs: HashMap<SessionId, u64>,
    subscribers: Vec<Subscriber>,
}

impl Bus {
    pub(crate) fn new(subscribers: Vec<Subscriber>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                seqs: HashMap::new(),
                subscribers,
            }),
        }
    }

    pub(crate) fn publish(&self, session: SessionId, event: impl Into<Event>) {
        let mut inner = lock(&self.inner);
        let Inner { seqs, subscribers } = &mut *inner;
        let seq = seqs.entry(session).or_default();
        *seq += 1;
        let envelope = Envelope {
            session,
            seq: *seq,
            event: event.into(),
        };
        for subscriber in subscribers {
            subscriber(&envelope);
        }
    }
}
