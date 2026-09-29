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

/// Identifies one subscription, from [`Bus::subscribe`].
pub(crate) type SubscriptionId = u64;

/// Stamps each event with its session's next sequence number and hands it
/// to every subscriber.
pub(crate) struct Bus {
    inner: Mutex<Inner>,
}

struct Inner {
    next: SubscriptionId,
    seqs: HashMap<SessionId, u64>,
    subscribers: Vec<(SubscriptionId, Subscriber)>,
}

impl Bus {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                next: 0,
                seqs: HashMap::new(),
                subscribers: Vec::new(),
            }),
        }
    }

    /// Deliver every event published from now on to `subscriber`, and
    /// return the id [`Bus::unsubscribe`] takes.
    pub(crate) fn subscribe(&self, subscriber: Subscriber) -> SubscriptionId {
        let mut inner = lock(&self.inner);
        let id = inner.next;
        inner.next += 1;
        inner.subscribers.push((id, subscriber));
        id
    }

    /// Remove the subscription `id`, so it receives nothing more.
    /// Unknown ids are ignored. Must not be called from inside a
    /// subscriber: `publish` holds the bus lock while it runs
    /// subscribers, so that deadlocks.
    pub(crate) fn unsubscribe(&self, id: SubscriptionId) {
        lock(&self.inner)
            .subscribers
            .retain(|(other, _)| *other != id);
    }

    /// Run `f` while the bus is locked, so no envelope is published
    /// during it. Like a subscriber, `f` must not publish, subscribe,
    /// or unsubscribe, or it deadlocks on the same lock.
    pub(crate) fn hold<R>(&self, f: impl FnOnce() -> R) -> R {
        let _held = lock(&self.inner);
        f()
    }

    pub(crate) fn publish(&self, session: SessionId, event: impl Into<Event>) {
        let mut inner = lock(&self.inner);
        let Inner {
            seqs, subscribers, ..
        } = &mut *inner;
        let seq = seqs.entry(session).or_default();
        *seq += 1;
        let envelope = Envelope {
            session,
            seq: *seq,
            event: event.into(),
        };
        for (_, subscriber) in subscribers {
            subscriber(&envelope);
        }
    }
}
