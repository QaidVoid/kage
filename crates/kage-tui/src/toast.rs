//! Ephemeral toast notifications: short messages that appear in the
//! top-right of the buffer area, auto-expire after a duration, and
//! never occupy buffer scroll space.
//!
//! Replaces the previous `kage:notify` Custom-block path so the
//! conversation pane stays focused on actual conversation content.
//! `kage:log` and `kage:error` blocks remain inline because the user
//! wants to scroll back to them; toasts are for "thing happened, ack
//! it, move on" feedback.
//!
//! Plugins customize toasts in three places (PE.A wiring lands later):
//!
//! 1. **Source** - `kage.notify(text)` already pushes a toast via the
//!    plugin host log sink.
//! 2. **Style** - the per-kind theme colors live in the `Theme` and
//!    can be overridden by a plugin's theme TOML in PT.9.
//! 3. **Renderer** - swap the implementation behind
//!    [`ToastRenderer`] via [`crate::App::set_toast_renderer`] for
//!    full layout control. The default renderer is bundled.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default lifetime for a freshly-pushed toast. Long enough to read
/// a short sentence, short enough not to clutter when actions chain.
pub const DEFAULT_TOAST_DURATION: Duration = Duration::from_secs(3);

/// Cap on simultaneous on-screen toasts. New toasts past this cap
/// drop the oldest. Keeps the overlay from drowning the buffer when
/// a plugin spams notifications.
pub const MAX_VISIBLE_TOASTS: usize = 4;

/// Severity / intent of a toast. Drives the left-edge accent color
/// and (when configured) the icon glyph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToastKind {
    /// Neutral info: "switched to anthropic:claude-sonnet-4-6".
    Info,
    /// Successful action: "yanked 234 chars to clipboard".
    Success,
    /// Non-fatal heads-up: "mouse capture off - drag selects via the
    /// terminal".
    Warning,
    /// Error feedback that the user should see immediately but not
    /// scroll back to (errors that *do* need history go through
    /// `kage:error` blocks instead).
    Error,
}

impl ToastKind {
    /// Convenience: most builtin notifications are informational.
    #[must_use]
    pub const fn default_kind() -> Self {
        Self::Info
    }
}

/// One toast entry in the overlay queue.
#[derive(Clone, Debug)]
pub struct Toast {
    /// Plain message text. The renderer wraps to the toast's width.
    pub text: String,
    /// Severity / intent driving the accent color.
    pub kind: ToastKind,
    /// Wall-clock instant after which the toast disappears.
    pub expires_at: Instant,
}

impl Toast {
    /// Build an info-kind toast that lives [`DEFAULT_TOAST_DURATION`]
    /// from now.
    #[must_use]
    pub fn info(text: impl Into<String>) -> Self {
        Self::with_kind(text, ToastKind::Info, DEFAULT_TOAST_DURATION)
    }

    /// Build a toast with explicit kind and duration.
    #[must_use]
    pub fn with_kind(text: impl Into<String>, kind: ToastKind, duration: Duration) -> Self {
        Self {
            text: text.into(),
            kind,
            expires_at: Instant::now() + duration,
        }
    }

    /// Whether the toast should still be painted at `now`.
    #[must_use]
    pub fn is_live(&self, now: Instant) -> bool {
        now < self.expires_at
    }
}

/// Shared queue of live toasts. The renderer reads from it; the App
/// and host plugin sink push into it. Bounded by
/// [`MAX_VISIBLE_TOASTS`] - the oldest entry is dropped on overflow.
pub type SharedToasts = Arc<Mutex<VecDeque<Toast>>>;

/// Construct an empty shared toast queue.
#[must_use]
pub fn shared_toasts() -> SharedToasts {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Push a toast onto a shared queue, dropping the oldest if past the
/// visible cap. Best-effort: a poisoned mutex silently skips - toasts
/// are decorative, never load-bearing.
pub fn push_toast(toasts: &SharedToasts, toast: Toast) {
    let Ok(mut q) = toasts.lock() else { return };
    while q.len() >= MAX_VISIBLE_TOASTS {
        q.pop_front();
    }
    q.push_back(toast);
}

/// Drop expired toasts from `toasts` and return the next live
/// expiration deadline (used by the App's poll-loop to wake up just
/// in time to remove a toast).
///
/// Best-effort on a poisoned mutex: returns `None` and lets the next
/// successful frame clean up.
pub fn prune_expired(toasts: &SharedToasts, now: Instant) -> Option<Instant> {
    let mut q = toasts.lock().ok()?;
    q.retain(|t| t.is_live(now));
    q.iter().map(|t| t.expires_at).min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_toast_uses_default_duration_and_kind() {
        let before = Instant::now();
        let t = Toast::info("hi");
        assert_eq!(t.kind, ToastKind::Info);
        assert!(t.expires_at >= before + DEFAULT_TOAST_DURATION);
    }

    #[test]
    fn is_live_returns_false_after_expiration() {
        let t = Toast::with_kind("x", ToastKind::Info, Duration::from_millis(0));
        std::thread::sleep(Duration::from_millis(1));
        assert!(!t.is_live(Instant::now()));
    }

    #[test]
    fn push_drops_oldest_past_cap() {
        let q = shared_toasts();
        for i in 0..(MAX_VISIBLE_TOASTS + 2) {
            push_toast(&q, Toast::info(format!("t{i}")));
        }
        let q = q.lock().unwrap();
        assert_eq!(q.len(), MAX_VISIBLE_TOASTS);
        // The two oldest were dropped; the survivors start with t2.
        assert_eq!(q.front().unwrap().text, "t2");
    }

    #[test]
    fn prune_removes_expired_and_reports_next_deadline() {
        let q = shared_toasts();
        push_toast(
            &q,
            Toast::with_kind("a", ToastKind::Info, Duration::from_millis(0)),
        );
        push_toast(
            &q,
            Toast::with_kind("b", ToastKind::Info, Duration::from_secs(60)),
        );
        std::thread::sleep(Duration::from_millis(2));
        let next = prune_expired(&q, Instant::now()).expect("survivor's deadline");
        let q = q.lock().unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q.front().unwrap().text, "b");
        assert!(next > Instant::now());
    }
}
