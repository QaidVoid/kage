//! Who plugin work runs for.
//!
//! One runtime can serve several sessions and clients, so every piece of
//! Lua work carries an [`Invocation`]: the session an engine hook or event
//! belongs to, and the client that invoked a command or keybinding. A
//! caller sets it with [`Invocation::scope`] around its call into the
//! runtime; the job it queues runs with it on the owner thread, and so do
//! the coroutines and timers that job starts. The APIs whose effect depends
//! on a session, such as `kage.exec` and `kage.env`, read
//! [`Invocation::current`].

use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use kage_core::SessionId;
use kage_core::environment::Environment;
use kage_core::sync::lock;

/// The session and client a piece of plugin work runs for. Both are
/// `None` for work nothing in particular asked for, such as loading a
/// plugin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Invocation {
    /// The session an engine hook or event runs for, or the invoking
    /// client's active session.
    pub session: Option<SessionId>,
    /// The connection that invoked a command or keybinding.
    pub client: Option<u64>,
}

thread_local! {
    static CURRENT: Cell<Invocation> = const {
        Cell::new(Invocation {
            session: None,
            client: None,
        })
    };
}

impl Invocation {
    /// The work for `session`, invoked by no client.
    #[must_use]
    pub fn session(session: SessionId) -> Self {
        Self {
            session: Some(session),
            client: None,
        }
    }

    /// The invocation in effect on this thread.
    #[must_use]
    pub fn current() -> Self {
        CURRENT.with(Cell::get)
    }

    /// Run `f` with this invocation in effect on this thread, restoring the
    /// previous one afterwards. Work `f` queues on the runtime runs with it.
    pub fn scope<R>(self, f: impl FnOnce() -> R) -> R {
        let _entered = Entered::new(self);
        f()
    }
}

/// Puts an invocation in effect until dropped, on the thread that made it.
pub(crate) struct Entered(Invocation, std::marker::PhantomData<*const ()>);

impl Entered {
    pub(crate) fn new(invocation: Invocation) -> Self {
        Self(
            CURRENT.with(|current| current.replace(invocation)),
            std::marker::PhantomData,
        )
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(self.0));
    }
}

/// Queued items, each with the session whose work queued it.
pub type Tagged<T> = Vec<(Option<SessionId>, T)>;

/// Take from `queue` the items `session`'s work queued and those no
/// session's work did, in order; with no session, take them all. Items
/// another session queued stay for that session.
pub(crate) fn drain_for<T>(queue: &mut Tagged<T>, session: Option<SessionId>) -> Vec<T> {
    let (mine, others) = std::mem::take(queue)
        .into_iter()
        .partition(|(owner, _)| session.is_none() || owner.is_none() || *owner == session);
    *queue = others;
    mine.into_iter().map(|(_, item)| item).collect()
}

/// The environment each session's processes start with, and the one for
/// work no known session asked for.
#[derive(Clone, Default)]
pub(crate) struct Environments {
    default: Environment,
    sessions: Arc<Mutex<HashMap<SessionId, Environment>>>,
}

impl Environments {
    pub(crate) fn new(default: Environment) -> Self {
        Self {
            default,
            sessions: Arc::default(),
        }
    }

    /// The environment of the session the current work runs for.
    pub(crate) fn current(&self) -> Environment {
        Invocation::current()
            .session
            .and_then(|session| lock(&self.sessions).get(&session).cloned())
            .unwrap_or_else(|| self.default.clone())
    }

    pub(crate) fn set(&self, session: SessionId, env: Environment) {
        lock(&self.sessions).insert(session, env);
    }

    pub(crate) fn forget(&self, session: SessionId) {
        lock(&self.sessions).remove(&session);
    }
}

/// A future that runs every poll with the invocation it was created under,
/// so a coroutine a job starts keeps that job's session and client.
pub(crate) struct InContext<F> {
    invocation: Invocation,
    inner: Pin<Box<F>>,
}

impl<F> InContext<F> {
    /// Wrap `inner` with the invocation in effect now.
    pub(crate) fn new(inner: F) -> Self {
        Self {
            invocation: Invocation::current(),
            inner: Box::pin(inner),
        }
    }
}

impl<F: Future> Future for InContext<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let _entered = Entered::new(self.invocation);
        self.inner.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_restores_the_outer_invocation() {
        let outer = Invocation::session(SessionId::new());
        let inner = Invocation {
            session: None,
            client: Some(3),
        };
        outer.scope(|| {
            inner.scope(|| assert_eq!(Invocation::current(), inner));
            assert_eq!(Invocation::current(), outer);
        });
        assert_eq!(Invocation::current(), Invocation::default());
    }

    #[test]
    fn a_session_drains_its_own_items_and_those_of_none() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let mut queue: Tagged<&str> = vec![(Some(a), "a"), (Some(b), "b"), (None, "any")];
        assert_eq!(drain_for(&mut queue, Some(a)), ["a", "any"]);
        assert_eq!(drain_for(&mut queue, None), ["b"]);
        assert!(queue.is_empty());
    }
}
