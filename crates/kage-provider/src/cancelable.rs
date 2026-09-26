//! Adapter that makes any [`EventStream`] cancellable mid-blocking-read.
//!
//! The per-provider streams (`OpenAiStream`, `AnthropicStream`,
//! `GeminiStream`) check the cancel flag *between* SSE chunks, but the
//! `read_chunk` call between checks is a blocking I/O read. If a model
//! is slow or stops sending bytes mid-turn, the cancel flag is not
//! observed until the next chunk arrives, which can be many seconds.
//!
//! [`make_cancelable`] wraps an inner stream so the foreground iterator
//! returns [`ProviderError::Cancelled`] the moment the flag flips,
//! regardless of how long the underlying network read takes. The inner
//! stream runs on a worker thread and forwards events through a bounded
//! channel. The outer iterator blocks on that channel and a
//! [`CancelWatch`] at once, so it never wakes while nothing happens.
//!
//! The worker thread keeps running the inner stream until it produces
//! an event whose send fails (because the consumer dropped the channel),
//! the inner stream finishes, or the underlying connection closes.
//! Cancelling does not leave that worker holding a live connection: the
//! consumer shuts the request's sockets down through
//! [`KillRegistry`](crate::interrupt::KillRegistry), so a worker
//! blocked in a read wakes with a connection error and exits instead of
//! draining toward the idle deadline.
use std::sync::Arc;

use crossbeam_channel::{Receiver, select_biased};
use kage_core::{CancelFlag, CancelWatch};

use crate::interrupt::KillRegistry;
use crate::{EventStream, ProviderError, ProviderEvent};

/// Bounded backlog between worker thread and consumer. Bounded so a
/// fast-streaming provider cannot run away if the consumer is slow.
const CHANNEL_BUFFER: usize = 32;

/// Run a blocking, uncancellable closure on a worker thread and wait
/// for either its result or `cancel`, so the caller can return
/// `ProviderError::Cancelled` long before `f` finishes.
///
/// Used by each provider's `stream` impl to wrap the synchronous
/// `ureq` request-and-headers call: that part of the round-trip
/// happens before we ever get a Reader to wrap, so without this the
/// cancel flag would not be observed until the HTTP server replies
/// (potentially many seconds for slow providers).
///
/// On cancel the request's sockets are shut down through `kill`, so the
/// detached thread's blocked connect or read fails promptly and the
/// thread exits instead of holding a half-open connection.
///
/// # Errors
///
/// - Whatever `f` returns when it completes first.
/// - [`ProviderError::Cancelled`] when the flag is set first or the
///   worker thread panics.
pub fn cancellable_call<F, T>(
    cancel: &CancelFlag,
    kill: &KillRegistry,
    f: F,
) -> Result<T, ProviderError>
where
    F: FnOnce() -> Result<T, ProviderError> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    if cancel.is_cancelled() {
        kill.shutdown_all();
        return Err(ProviderError::Cancelled);
    }
    let watch = cancel.watch();
    select_biased! {
        recv(watch.receiver()) -> _ => {
            kill.shutdown_all();
            Err(ProviderError::Cancelled)
        }
        recv(rx) -> reply => reply.unwrap_or(Err(ProviderError::Cancelled)),
    }
}

/// Wrap `inner` so that `cancel` is observed as soon as it is set,
/// regardless of how long the underlying read blocks.
///
/// The inner iterator is moved onto a dedicated worker thread; the
/// returned iterator waits on its channel and a watch on the cancel
/// flag together. Once the cancel flag is set, the next `next()` call
/// returns `Err(ProviderError::Cancelled)` and the iterator is fused;
/// the request's sockets are shut down through `kill`, so the worker
/// thread's blocked read fails and it exits rather than holding the
/// connection open.
#[must_use]
pub fn make_cancelable(
    inner: EventStream,
    cancel: CancelFlag,
    kill: Arc<KillRegistry>,
) -> EventStream {
    let (tx, rx) = crossbeam_channel::bounded(CHANNEL_BUFFER);
    std::thread::spawn(move || {
        for item in inner {
            if tx.send(item).is_err() {
                break;
            }
        }
    });
    let watch = cancel.watch();
    Box::new(CancelableStream {
        rx,
        cancel,
        watch,
        kill,
        done: false,
    })
}

struct CancelableStream {
    rx: Receiver<Result<ProviderEvent, ProviderError>>,
    cancel: CancelFlag,
    watch: CancelWatch,
    kill: Arc<KillRegistry>,
    done: bool,
}

impl Iterator for CancelableStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.cancel.is_cancelled() {
            self.kill.shutdown_all();
            self.done = true;
            return Some(Err(ProviderError::Cancelled));
        }
        select_biased! {
            recv(self.watch.receiver()) -> _ => {
                self.kill.shutdown_all();
                self.done = true;
                Some(Err(ProviderError::Cancelled))
            }
            recv(self.rx) -> item => {
                let item = item.ok();
                self.done = matches!(item, None | Some(Err(ProviderError::Cancelled)));
                item
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    /// Tests stream in-memory iterators; there is no socket to tear
    /// down, so the empty registry's shutdown is a no-op.
    fn no_kill() -> Arc<KillRegistry> {
        Arc::new(KillRegistry::new())
    }

    fn boxed(events: Vec<Result<ProviderEvent, ProviderError>>) -> EventStream {
        Box::new(events.into_iter())
    }

    #[test]
    fn forwards_inner_events_in_order_when_not_cancelled() {
        let cancel = CancelFlag::new();
        let inner = boxed(vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: "hello".into(),
            }),
        ]);
        let mut s = make_cancelable(inner, cancel, no_kill());
        assert!(matches!(s.next(), Some(Ok(ProviderEvent::MessageStart))));
        assert!(matches!(
            s.next(),
            Some(Ok(ProviderEvent::TextDelta { ref delta })) if delta == "hello"
        ));
        assert!(s.next().is_none());
    }

    /// A read that never finishes on its own: `next` reports on
    /// `entered`, then blocks until the test drops the release sender
    /// (or five seconds pass) and yields `MessageStart`.
    struct Stalled {
        entered: crossbeam_channel::Sender<()>,
        release: crossbeam_channel::Receiver<()>,
    }

    impl Iterator for Stalled {
        type Item = Result<ProviderEvent, ProviderError>;
        fn next(&mut self) -> Option<Self::Item> {
            let _ = self.entered.send(());
            let _ = self.release.recv_timeout(Duration::from_secs(5));
            Some(Ok(ProviderEvent::MessageStart))
        }
    }

    fn stalled() -> (
        Stalled,
        crossbeam_channel::Receiver<()>,
        crossbeam_channel::Sender<()>,
    ) {
        let (entered_tx, entered) = crossbeam_channel::unbounded();
        let (release, release_rx) = crossbeam_channel::bounded(0);
        let inner = Stalled {
            entered: entered_tx,
            release: release_rx,
        };
        (inner, entered, release)
    }

    #[test]
    fn returns_cancelled_while_inner_is_blocked() {
        let (inner, entered, _release) = stalled();
        let cancel = CancelFlag::new();
        let mut s = make_cancelable(Box::new(inner), cancel.clone(), no_kill());
        std::thread::spawn(move || {
            let _ = entered.recv();
            cancel.cancel();
        });
        let item = s.next().expect("an item should arrive");
        assert!(
            matches!(item, Err(ProviderError::Cancelled)),
            "expected Cancelled, got {item:?}"
        );
    }

    #[test]
    fn cancellable_call_returns_cancelled_while_the_closure_blocks() {
        let (mut inner, entered, _release) = stalled();
        let cancel = CancelFlag::new();
        let flag = cancel.clone();
        std::thread::spawn(move || {
            let _ = entered.recv();
            flag.cancel();
        });
        let result = cancellable_call(&cancel, &no_kill(), move || {
            inner.next();
            Ok(())
        });
        assert!(matches!(result, Err(ProviderError::Cancelled)));
    }

    #[test]
    fn fuses_after_cancelled_so_next_returns_none() {
        let (inner, _entered, _release) = stalled();
        let cancel = CancelFlag::new();
        cancel.cancel();
        let mut s = make_cancelable(Box::new(inner), cancel, no_kill());
        assert!(matches!(s.next(), Some(Err(ProviderError::Cancelled))));
        assert!(s.next().is_none());
    }

    #[test]
    fn forwards_inner_cancelled_and_fuses() {
        let cancel = CancelFlag::new();
        let inner = boxed(vec![
            Ok(ProviderEvent::MessageStart),
            Err(ProviderError::Cancelled),
            Ok(ProviderEvent::TextDelta {
                delta: "after".into(),
            }),
        ]);
        let mut s = make_cancelable(inner, cancel, no_kill());
        assert!(matches!(s.next(), Some(Ok(ProviderEvent::MessageStart))));
        assert!(matches!(s.next(), Some(Err(ProviderError::Cancelled))));
        assert!(s.next().is_none());
    }
}
