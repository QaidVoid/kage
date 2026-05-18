//! Adapter that makes any [`EventStream`] cancellable mid-blocking-read.
//!
//! The per-provider streams (`OpenAiStream`, `AnthropicStream`,
//! `GeminiStream`) check the cancel flag *between* SSE chunks, but the
//! `read_chunk` call between checks is a blocking I/O read. If a model
//! is slow or stops sending bytes mid-turn, the cancel flag is not
//! observed until the next chunk arrives, which can be many seconds.
//!
//! [`make_cancelable`] wraps an inner stream so the foreground iterator
//! observes the cancel flag within the polling interval (25ms)
//! regardless of how long the underlying network read takes. The inner
//! stream runs on a worker thread and forwards events through a bounded
//! channel; the outer iterator polls the channel with `recv_timeout` and
//! returns [`ProviderError::Cancelled`] as soon as the flag flips.
//!
//! The worker thread keeps running the inner stream until it produces an
//! event whose send fails (because the consumer dropped the channel),
//! the inner stream finishes, or the underlying connection closes.
//! Bounded resource leak: at most one worker thread per cancelled turn,
//! reclaimed when the next chunk arrives or the HTTP connection times
//! out at the OS level.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use kage_core::CancelFlag;

use crate::{EventStream, ProviderError, ProviderEvent};

/// Time the foreground iterator waits between cancel-flag checks.
///
/// Trades responsiveness vs CPU. 25ms is below the human "instant"
/// perception threshold: a user hitting `Esc` sees the cancel land
/// effectively immediately. The idle iterator wakes 40 times a
/// second checking a single atomic load, which is invisible in `top`.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Bounded backlog between worker thread and consumer. Bounded so a
/// fast-streaming provider cannot run away if the consumer is slow.
const CHANNEL_BUFFER: usize = 32;

/// Run a blocking, uncancellable closure on a worker thread and poll
/// `cancel` from the foreground so the caller can return
/// `ProviderError::Cancelled` long before `f` finishes.
///
/// Used by each provider's `stream` impl to wrap the synchronous
/// `ureq` request-and-headers call: that part of the round-trip
/// happens before we ever get a Reader to wrap, so without this the
/// cancel flag would not be observed until the HTTP server replies
/// (potentially many seconds for slow providers).
///
/// On cancel the spawned thread is detached - it continues until `f`
/// returns naturally and the result is discarded. The connection it
/// holds is reclaimed at OS-level HTTP keepalive timeout.
///
/// # Errors
///
/// - Whatever `f` returns when it completes first.
/// - [`ProviderError::Cancelled`] when the flag is observed first or
///   the worker thread panics.
pub fn cancellable_call<F, T>(cancel: &CancelFlag, f: F) -> Result<T, ProviderError>
where
    F: FnOnce() -> Result<T, ProviderError> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    loop {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(r) => return r,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(ProviderError::Cancelled),
        }
    }
}

/// Wrap `inner` so that `cancel` is observed within [`POLL_INTERVAL`]
/// regardless of how long the underlying read blocks.
///
/// The inner iterator is moved onto a dedicated worker thread; the
/// returned iterator pulls events from a channel, polling the cancel
/// flag between recv timeouts. Once the cancel flag is set, the next
/// `next()` call returns `Err(ProviderError::Cancelled)` and the
/// iterator is fused; the worker thread continues until its next send
/// fails (when the channel receiver is dropped) and then exits.
#[must_use]
pub fn make_cancelable(inner: EventStream, cancel: CancelFlag) -> EventStream {
    let (tx, rx) = mpsc::sync_channel(CHANNEL_BUFFER);
    std::thread::spawn(move || {
        for item in inner {
            if tx.send(item).is_err() {
                break;
            }
        }
    });
    Box::new(CancelableStream {
        rx,
        cancel,
        done: false,
    })
}

struct CancelableStream {
    rx: mpsc::Receiver<Result<ProviderEvent, ProviderError>>,
    cancel: CancelFlag,
    done: bool,
}

impl Iterator for CancelableStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            if self.cancel.is_cancelled() {
                self.done = true;
                return Some(Err(ProviderError::Cancelled));
            }
            match self.rx.recv_timeout(POLL_INTERVAL) {
                Ok(item) => {
                    if matches!(&item, Err(ProviderError::Cancelled)) {
                        self.done = true;
                    }
                    return Some(item);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.done = true;
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

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
        let mut s = make_cancelable(inner, cancel);
        assert!(matches!(s.next(), Some(Ok(ProviderEvent::MessageStart))));
        assert!(matches!(
            s.next(),
            Some(Ok(ProviderEvent::TextDelta { ref delta })) if delta == "hello"
        ));
        assert!(s.next().is_none());
    }

    #[test]
    fn returns_cancelled_quickly_when_inner_is_blocked() {
        struct Slow;
        impl Iterator for Slow {
            type Item = Result<ProviderEvent, ProviderError>;
            fn next(&mut self) -> Option<Self::Item> {
                std::thread::sleep(Duration::from_secs(60));
                Some(Ok(ProviderEvent::MessageStart))
            }
        }

        let cancel = CancelFlag::new();
        let mut s = make_cancelable(Box::new(Slow), cancel.clone());
        let watcher = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            watcher.cancel();
        });
        let start = Instant::now();
        let item = s.next().expect("an item should arrive");
        let elapsed = start.elapsed();
        assert!(
            matches!(item, Err(ProviderError::Cancelled)),
            "expected Cancelled, got {item:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "cancel observation took {elapsed:?}, expected < 500ms"
        );
    }

    #[test]
    fn fuses_after_cancelled_so_next_returns_none() {
        struct Slow;
        impl Iterator for Slow {
            type Item = Result<ProviderEvent, ProviderError>;
            fn next(&mut self) -> Option<Self::Item> {
                std::thread::sleep(Duration::from_secs(60));
                None
            }
        }

        let cancel = CancelFlag::new();
        cancel.cancel();
        let mut s = make_cancelable(Box::new(Slow), cancel);
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
        let mut s = make_cancelable(inner, cancel);
        assert!(matches!(s.next(), Some(Ok(ProviderEvent::MessageStart))));
        assert!(matches!(s.next(), Some(Err(ProviderError::Cancelled))));
        assert!(s.next().is_none());
    }
}
