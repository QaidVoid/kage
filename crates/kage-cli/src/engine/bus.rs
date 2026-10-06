//! Synchronous event fan-out with per-session sequence numbers.

use std::cell::Cell;
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
        assert_not_reentrant();
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
        assert_not_reentrant();
        lock(&self.inner)
            .subscribers
            .retain(|(other, _)| *other != id);
    }

    /// Run `f` while the bus is locked, so no envelope is published
    /// during it. Like a subscriber, `f` must not publish, subscribe,
    /// or unsubscribe, or it deadlocks on the same lock.
    pub(crate) fn hold<R>(&self, f: impl FnOnce() -> R) -> R {
        assert_not_reentrant();
        let _held = lock(&self.inner);
        let _guard = SubscriberGuard::enter();
        f()
    }

    /// Drop the sequence count of a session the dispatcher removed.
    /// Ids are ULIDs and never reused, so the map would otherwise grow
    /// with every session the engine ever hosted for its whole life.
    pub(crate) fn forget(&self, session: SessionId) {
        lock(&self.inner).seqs.remove(&session);
    }

    pub(crate) fn publish(&self, session: SessionId, event: impl Into<Event>) {
        assert_not_reentrant();
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
        let _guard = SubscriberGuard::enter();
        for (_, subscriber) in subscribers {
            subscriber(&envelope);
        }
    }

    /// The sequence count of `session`, for tests.
    #[cfg(test)]
    pub(crate) fn seq_of(&self, session: SessionId) -> Option<u64> {
        lock(&self.inner).seqs.get(&session).copied()
    }
}

// Set on the thread running bus subscribers, so the bus entry points
// can refuse the re-entrancy their docs forbid: a subscriber that
// publishes, subscribes or unsubscribes deadlocks on the bus lock the
// entry point already holds.
thread_local! {
    static IN_SUBSCRIBER: Cell<bool> = const { Cell::new(false) };
}

/// Marks the thread as running subscribers until dropped.
struct SubscriberGuard;

impl SubscriberGuard {
    fn enter() -> Self {
        IN_SUBSCRIBER.with(|active| active.set(true));
        Self
    }
}

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        IN_SUBSCRIBER.with(|active| active.set(false));
    }
}

/// Refuses a call from inside a subscriber in debug builds, where it
/// would deadlock instead of failing loudly.
fn assert_not_reentrant() {
    IN_SUBSCRIBER.with(|active| {
        debug_assert!(
            !active.get(),
            "re-entered the bus from a subscriber, which deadlocks on the bus lock"
        );
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::protocol::HostEvent;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn sequences_continue_per_session_and_forget_restarts_them() {
        let bus = Bus::new();
        let (tx, rx) = crossbeam_channel::unbounded::<Envelope>();
        bus.subscribe(Box::new(move |envelope| {
            let _ = tx.send(envelope.clone());
        }));
        let id = SessionId::new();
        bus.publish(id, HostEvent::RunStarted);
        bus.publish(id, HostEvent::RunStarted);
        assert_eq!(bus.seq_of(id), Some(2));
        assert_eq!(rx.recv_timeout(WAIT).unwrap().seq, 1);
        assert_eq!(rx.recv_timeout(WAIT).unwrap().seq, 2);
        bus.forget(id);
        assert_eq!(bus.seq_of(id), None);
        bus.publish(id, HostEvent::RunStarted);
        assert_eq!(rx.recv_timeout(WAIT).unwrap().seq, 1);
    }

    #[test]
    fn one_session_gets_highest_seq_ever_seen() {
        let bus = Bus::new();
        let (tx, rx) = crossbeam_channel::unbounded::<Envelope>();
        bus.subscribe(Box::new(move |envelope| {
            let _ = tx.send(envelope.clone());
        }));
        let a = SessionId::new();
        let b = SessionId::new();
        bus.publish(a, HostEvent::RunStarted);
        bus.publish(b, HostEvent::RunStarted);
        bus.publish(a, HostEvent::RunStarted);
        assert_eq!(bus.seq_of(a), Some(2));
        assert_eq!(bus.seq_of(b), Some(1));
        assert_eq!(rx.recv_timeout(WAIT).unwrap().session, a);
        assert_eq!(rx.recv_timeout(WAIT).unwrap().session, b);
        assert_eq!(rx.recv_timeout(WAIT).unwrap().session, a);
    }

    /// The re-entrancy the docs forbid must fail loudly instead of
    /// deadlocking, so a future subscriber that re-enters is caught by
    /// every debug run.
    #[test]
    #[cfg(debug_assertions)]
    fn a_subscriber_may_not_re_enter_the_bus() {
        fn reenter<F>(what: &str, action: F)
        where
            F: Fn(&Bus) + Send + Sync + 'static,
        {
            let bus = Bus::new();
            let reentered = Bus::new();
            let subscriber =
                Box::new(move |_: &Envelope| action(&reentered)) as Box<dyn Fn(&Envelope) + Send>;
            bus.subscribe(subscriber);
            let published = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                bus.publish(SessionId::new(), HostEvent::RunStarted);
            }));
            assert!(
                published.is_err(),
                "a subscriber {what} the bus must hit the debug assert"
            );
        }
        reenter("publishing", |bus| {
            bus.publish(SessionId::new(), HostEvent::RunStarted);
        });
        reenter("subscribes to", |bus| {
            bus.subscribe(Box::new(|_| {}));
        });
        reenter("unsubscribes from", |bus| bus.unsubscribe(0));
        reenter("holds", |bus| {
            bus.hold(|| {});
        });
    }
}
