//! Session-local notification history and short-lived floating acknowledgements.
//!
//! History preserves full messages independently of the bounded active queue.
//! Matching bursts share one history entry and never extend an acknowledgement.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kage_core::sync::lock;

/// Default display lifetime, excluding time spent waiting or hovered.
pub const DEFAULT_TOAST_DURATION: Duration = Duration::from_secs(5);

/// Maximum number of acknowledgements, including the current one and those waiting.
pub const MAX_VISIBLE_TOASTS: usize = 4;

/// Maximum number of grouped notifications retained for this session.
pub const MAX_NOTIFICATION_HISTORY: usize = 100;

const DEDUP_WINDOW: Duration = Duration::from_secs(5);

/// Severity or intent of a notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToastKind {
    /// Neutral informational feedback.
    Info,
    /// A successfully completed action.
    Success,
    /// A non-fatal issue requiring attention.
    Warning,
    /// An error requiring attention.
    Error,
}

impl ToastKind {
    /// Return the default kind for built-in notifications.
    #[must_use]
    pub const fn default_kind() -> Self {
        Self::Info
    }
}

/// One short-lived floating acknowledgement.
#[derive(Clone, Debug)]
pub struct Toast {
    /// Stable acknowledgement identifier assigned when recorded, or zero beforehand.
    pub id: u64,
    /// Full message text, retained without truncation in history.
    pub text: String,
    /// Severity or intent of the message.
    pub kind: ToastKind,
    /// Component or plugin that produced the message.
    pub source: String,
    /// Monotonic expiry when current, extended by elapsed hover time when resumed.
    ///
    /// Waiting acknowledgements receive a fresh expiry when promoted.
    pub expires_at: Instant,
    duration: Duration,
    pending: bool,
    paused_at: Option<Instant>,
}

impl Toast {
    /// Build an informational acknowledgement with the default lifetime.
    #[must_use]
    pub fn info(text: impl Into<String>) -> Self {
        Self::with_kind(text, ToastKind::Info, DEFAULT_TOAST_DURATION)
    }

    /// Build an acknowledgement with explicit severity and lifetime.
    #[must_use]
    pub fn with_kind(text: impl Into<String>, kind: ToastKind, duration: Duration) -> Self {
        Self {
            id: 0,
            text: text.into(),
            kind,
            source: "kage".into(),
            expires_at: Instant::now() + duration,
            duration,
            pending: false,
            paused_at: None,
        }
    }

    /// Set the component or plugin that produced this message.
    #[must_use]
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// Whether this acknowledgement is waiting, hovered, or unexpired at `now`.
    #[must_use]
    pub fn is_live(&self, now: Instant) -> bool {
        self.pending || self.paused_at.is_some() || now < self.expires_at
    }

    /// Return the expiry deadline, or `None` while waiting or hovered.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        (!self.pending && self.paused_at.is_none()).then_some(self.expires_at)
    }
}

/// A group of identical notifications retained in session history.
#[derive(Clone, Debug)]
pub struct Notification {
    /// Complete, untruncated message text.
    pub text: String,
    /// Severity or intent of the message.
    pub kind: ToastKind,
    /// Component or plugin that produced the message.
    pub source: String,
    /// Number of events represented by this group.
    pub count: u64,
    /// Monotonic time of the first event in this group.
    pub first_seen: Instant,
    /// Monotonic time of the most recent event in this group.
    pub last_seen: Instant,
    /// Whether the user has reviewed this group in notification history.
    pub reviewed: bool,
}

/// Bounded active acknowledgements and independently retained history.
#[derive(Debug, Default)]
pub struct NotificationState {
    /// Current acknowledgement first, followed by waiting acknowledgements in arrival order.
    pub active: VecDeque<Toast>,
    /// Notification groups ordered by their most recent occurrence.
    pub history: VecDeque<Notification>,
    revision: u64,
}

impl NotificationState {
    /// Revision of recorded messages, repeat counts, review, hover, dismissal, and expiry.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Count unreviewed warning and error groups, not individual repeats.
    #[must_use]
    pub fn unread_count(&self) -> usize {
        self.history
            .iter()
            .filter(|n| !n.reviewed && needs_attention(n.kind))
            .count()
    }

    /// Pause the current live acknowledgement when its identifier matches `id`.
    ///
    /// Passing `None`, a waiting identifier, or a stale identifier resumes the
    /// current acknowledgement. Resuming preserves its remaining lifetime.
    /// Expired entries are never revived, and waiting lifetimes never start here.
    /// The revision changes only when the current acknowledgement pauses or resumes.
    pub fn hover(&mut self, id: Option<u64>, now: Instant) {
        let Some(toast) = self.active.front_mut() else {
            return;
        };
        let pause = id == Some(toast.id) && toast.is_live(now);
        match (toast.paused_at, pause) {
            (Some(paused_at), false) => {
                toast.expires_at += now.saturating_duration_since(paused_at);
                toast.paused_at = None;
            }
            (None, true) => toast.paused_at = Some(now),
            _ => return,
        }
        self.revision = self.revision.wrapping_add(1);
    }

    /// Dismiss an active acknowledgement without reviewing or removing its history.
    ///
    /// Return whether an acknowledgement was removed, advancing the revision only
    /// when the active queue changes. Dismissing the current acknowledgement starts
    /// the next waiting acknowledgement's full display lifetime immediately.
    pub fn dismiss(&mut self, id: u64) -> bool {
        let Some(index) = self.active.iter().position(|toast| toast.id == id) else {
            return false;
        };
        self.active.remove(index);
        if index == 0 {
            self.promote_current(Instant::now());
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn promote_current(&mut self, now: Instant) {
        if let Some(toast) = self.active.front_mut()
            && toast.pending
        {
            toast.pending = false;
            toast.expires_at = now + toast.duration;
        }
    }

    fn advance(&mut self, now: Instant) {
        if self.active.front().is_some_and(|toast| !toast.is_live(now)) {
            self.active.pop_front();
            self.promote_current(now);
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Clear history attention without removing messages or acknowledgements.
    pub fn mark_reviewed(&mut self) {
        if self.history.iter().any(|entry| !entry.reviewed) {
            self.revision = self.revision.wrapping_add(1);
        }
        for notification in &mut self.history {
            notification.reviewed = true;
        }
    }
}

/// Notification state shared by the application, renderer, and plugin sink.
pub type SharedToasts = Arc<Mutex<NotificationState>>;

/// Construct empty shared notification state.
#[must_use]
pub fn shared_toasts() -> SharedToasts {
    Arc::new(Mutex::new(NotificationState::default()))
}

/// Record a notification and request a short-lived floating acknowledgement.
///
/// Repeated messages share history and do not renew active deadlines.
pub fn push_toast(toasts: &SharedToasts, toast: Toast) {
    record_notification(toasts, toast, true);
}

/// Retain a notification, optionally requesting a floating acknowledgement.
///
/// Identical source, severity, and text coalesce within five seconds. Unreviewed
/// warnings and errors keep coalescing until reviewed, without re-alerting.
/// After review, a repeated warning or error creates a fresh group immediately.
/// Silent events (`interrupt == false`) and already-expired inputs only enter
/// history. Acknowledgements display in arrival order, with waiting lifetimes
/// starting only when promoted. Overflow evicts waiting entries, preferring to
/// retain errors, but never interrupts the current acknowledgement. Active entries
/// and history are bounded independently, and full message text is preserved.
pub fn record_notification(toasts: &SharedToasts, toast: Toast, interrupt: bool) {
    let now = Instant::now();
    let mut state = lock(toasts);
    state.revision = state.revision.wrapping_add(1);
    let mut toast = toast;
    toast.id = state.revision;
    toast.pending = false;
    toast.paused_at = None;
    let duplicate = state.history.iter().rposition(|notification| {
        if notification.text != toast.text
            || notification.kind != toast.kind
            || notification.source != toast.source
        {
            return false;
        }
        if needs_attention(notification.kind) {
            !notification.reviewed
        } else {
            now.saturating_duration_since(notification.last_seen) <= DEDUP_WINDOW
        }
    });
    if let Some(index) = duplicate {
        let mut notification = state.history.remove(index).expect("existing notification");
        notification.count = notification.count.saturating_add(1);
        notification.last_seen = now;
        state.history.push_back(notification);
        return;
    }

    while state.history.len() >= MAX_NOTIFICATION_HISTORY {
        state.history.pop_front();
    }
    state.history.push_back(Notification {
        text: toast.text.clone(),
        kind: toast.kind,
        source: toast.source.clone(),
        count: 1,
        first_seen: now,
        last_seen: now,
        reviewed: false,
    });

    if interrupt {
        state.advance(now);
        if !toast.is_live(now) {
            return;
        }
        let current_id = state.active.front().map(|active| active.id);
        state.active.retain(|active| {
            Some(active.id) == current_id
                || !(active.text == toast.text
                    && active.kind == toast.kind
                    && active.source == toast.source)
        });
        while state.active.len() >= MAX_VISIBLE_TOASTS {
            let evict = state
                .active
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, active)| active.kind != ToastKind::Error)
                .map_or(1, |(index, _)| index);
            state.active.remove(evict);
        }
        toast.pending = !state.active.is_empty();
        state.active.push_back(toast);
    }
}

/// Expire the current acknowledgement and return the current display deadline.
///
/// The next waiting acknowledgement receives its full lifetime starting at `now`.
/// Only one acknowledgement expires per call, even after a long idle period.
/// Hovered and waiting acknowledgements contribute no deadline. History is
/// unaffected. A poisoned mutex is recovered consistently with recording.
pub fn prune_expired(toasts: &SharedToasts, now: Instant) -> Option<Instant> {
    let mut state = lock(toasts);
    state.advance(now);
    state.active.front().and_then(Toast::deadline)
}

fn needs_attention(kind: ToastKind) -> bool {
    matches!(kind, ToastKind::Warning | ToastKind::Error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warning(text: &str) -> Toast {
        Toast::with_kind(text, ToastKind::Warning, DEFAULT_TOAST_DURATION)
    }

    #[test]
    fn pending_toasts_receive_full_lifetime_when_promoted_in_arrival_order() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("first"));
        push_toast(&toasts, warning("second"));
        push_toast(&toasts, Toast::info("third"));
        let future = Instant::now() + Duration::from_secs(30);
        {
            let state = lock(&toasts);
            assert_eq!(state.active[0].text, "first");
            assert_eq!(state.active[1].deadline(), None);
            assert!(state.active[1].is_live(future));
        }
        let revision = lock(&toasts).revision();
        assert_eq!(
            prune_expired(&toasts, future),
            Some(future + DEFAULT_TOAST_DURATION)
        );
        {
            let state = lock(&toasts);
            assert_ne!(state.revision(), revision);
            assert_eq!(state.active.len(), 2);
            assert_eq!(state.active[0].text, "second");
            assert_eq!(state.active[1].text, "third");
            assert_eq!(state.active[1].deadline(), None);
        }
        let next = future + DEFAULT_TOAST_DURATION;
        assert_eq!(
            prune_expired(&toasts, next),
            Some(next + DEFAULT_TOAST_DURATION)
        );
        assert_eq!(lock(&toasts).active[0].text, "third");
    }

    #[test]
    fn waiting_dedup_preserves_id_order_and_full_custom_lifetime() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("first"));
        let duration = Duration::from_secs(9);
        push_toast(
            &toasts,
            Toast::with_kind("waiting", ToastKind::Warning, duration),
        );
        let (id, expiry) = {
            let state = lock(&toasts);
            (state.active[1].id, state.active[1].expires_at)
        };
        push_toast(&toasts, warning("waiting"));
        {
            let state = lock(&toasts);
            assert_eq!(state.active.len(), 2);
            assert_eq!(state.active[1].id, id);
            assert_eq!(state.active[1].expires_at, expiry);
            assert_eq!(state.active[1].deadline(), None);
            assert_eq!(state.history.len(), 2);
            assert_eq!(state.history[1].count, 2);
        }
        let future = Instant::now() + Duration::from_secs(30);
        assert_eq!(prune_expired(&toasts, future), Some(future + duration));
        let state = lock(&toasts);
        assert_eq!(state.active[0].id, id);
        assert_eq!(state.active[0].text, "waiting");
    }

    #[test]
    fn dismissing_current_starts_next_full_lifetime_without_reviewing_history() {
        let toasts = shared_toasts();
        push_toast(&toasts, warning("current"));
        let duration = Duration::from_secs(12);
        push_toast(&toasts, Toast::with_kind("next", ToastKind::Info, duration));
        push_toast(&toasts, Toast::info("last"));
        let mut state = lock(&toasts);
        let current = state.active[0].id;
        let next = state.active[1].id;
        let last = state.active[2].id;
        let before = Instant::now();
        assert!(state.dismiss(current));
        let after = Instant::now();
        assert_eq!(state.active[0].id, next);
        let deadline = state.active[0].deadline().unwrap();
        assert!(deadline >= before + duration);
        assert!(deadline <= after + duration);
        assert_eq!(state.active[1].id, last);
        assert_eq!(state.active[1].deadline(), None);
        assert_eq!(state.history.len(), 3);
        assert_eq!(state.unread_count(), 1);
        assert!(
            state
                .history
                .iter()
                .all(|notification| !notification.reviewed)
        );
        let revision = state.revision();
        assert!(!state.dismiss(current));
        assert_eq!(state.revision(), revision);
    }

    #[test]
    fn dismissing_pending_preserves_current_deadline_and_survivor_order() {
        let toasts = shared_toasts();
        for message in ["current", "remove", "keep", "last"] {
            push_toast(&toasts, Toast::info(message));
        }
        let mut state = lock(&toasts);
        let deadline = state.active[0].deadline();
        let current = state.active[0].id;
        let removed = state.active[1].id;
        let revision = state.revision();
        assert!(state.dismiss(removed));
        assert_ne!(state.revision(), revision);
        assert_eq!(state.active[0].id, current);
        assert_eq!(state.active[0].deadline(), deadline);
        let messages: Vec<_> = state
            .active
            .iter()
            .map(|toast| toast.text.as_str())
            .collect();
        assert_eq!(messages, ["current", "keep", "last"]);
        assert_eq!(state.active[1].deadline(), None);
        assert_eq!(state.active[2].deadline(), None);
        assert_eq!(state.history.len(), 4);
    }

    #[test]
    fn overflow_prefers_pending_errors_without_preempting_current_info() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("current"));
        push_toast(
            &toasts,
            Toast::with_kind("first error", ToastKind::Error, DEFAULT_TOAST_DURATION),
        );
        push_toast(&toasts, Toast::info("evicted"));
        push_toast(
            &toasts,
            Toast::with_kind("second error", ToastKind::Error, DEFAULT_TOAST_DURATION),
        );
        let (id, deadline) = {
            let state = lock(&toasts);
            (state.active[0].id, state.active[0].deadline())
        };
        push_toast(
            &toasts,
            Toast::with_kind("third error", ToastKind::Error, DEFAULT_TOAST_DURATION),
        );
        let state = lock(&toasts);
        assert_eq!(state.active[0].id, id);
        assert_eq!(state.active[0].deadline(), deadline);
        let messages: Vec<_> = state
            .active
            .iter()
            .map(|toast| toast.text.as_str())
            .collect();
        assert_eq!(
            messages,
            ["current", "first error", "second error", "third error"]
        );
    }

    #[test]
    fn zero_duration_and_expired_inputs_remain_history_only_while_queue_waits() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("current"));
        push_toast(
            &toasts,
            Toast::with_kind("zero", ToastKind::Warning, Duration::ZERO),
        );
        let mut expired = Toast::info("expired");
        expired.expires_at = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        push_toast(&toasts, expired);
        push_toast(&toasts, Toast::info("next"));
        {
            let state = lock(&toasts);
            assert_eq!(state.active.len(), 2);
            assert_eq!(state.active[0].text, "current");
            assert_eq!(state.active[1].text, "next");
            assert_eq!(state.history.len(), 4);
            assert_eq!(state.unread_count(), 1);
        }
        let future = Instant::now() + Duration::from_secs(30);
        assert_eq!(
            prune_expired(&toasts, future),
            Some(future + DEFAULT_TOAST_DURATION)
        );
        assert_eq!(lock(&toasts).active[0].text, "next");
    }

    #[test]
    fn silent_updates_and_repeats_invalidate_history_without_new_acknowledgements() {
        let toasts = shared_toasts();
        let before = lock(&toasts).revision();
        record_notification(&toasts, warning("same issue"), false);
        let recorded = lock(&toasts).revision();
        assert_ne!(recorded, before);
        record_notification(&toasts, warning("same issue"), false);
        let mut state = lock(&toasts);
        assert_ne!(state.revision(), recorded);
        assert!(state.active.is_empty());
        assert_eq!(state.history[0].count, 2);
        let repeated = state.revision();
        state.mark_reviewed();
        assert_ne!(state.revision(), repeated);
    }

    #[test]
    fn info_toast_uses_default_duration_kind_and_source() {
        let before = Instant::now();
        let toast = Toast::info("hi");
        assert_eq!(toast.kind, ToastKind::Info);
        assert_eq!(toast.source, "kage");
        assert!(toast.expires_at >= before + DEFAULT_TOAST_DURATION);
        assert_eq!(toast.with_source("plugin").source, "plugin");
    }

    #[test]
    fn expiration_does_not_remove_full_history() {
        let toasts = shared_toasts();
        let text = format!("{}\nsecond line", "a".repeat(500));
        let toast = Toast::with_kind(&text, ToastKind::Warning, Duration::ZERO);
        assert!(!toast.is_live(Instant::now()));
        push_toast(&toasts, toast);
        assert_eq!(prune_expired(&toasts, Instant::now()), None);
        let state = lock(&toasts);
        assert!(state.active.is_empty());
        assert_eq!(state.history[0].text, text);
        assert_eq!(state.unread_count(), 1);
    }

    #[test]
    fn active_and_history_have_independent_bounds() {
        let toasts = shared_toasts();
        for i in 0..MAX_NOTIFICATION_HISTORY + 2 {
            push_toast(&toasts, Toast::info(format!("t{i}")));
        }
        let state = lock(&toasts);
        assert_eq!(state.active.len(), MAX_VISIBLE_TOASTS);
        assert_eq!(state.history.len(), MAX_NOTIFICATION_HISTORY);
        assert_eq!(state.history.front().unwrap().text, "t2");
        assert_eq!(state.active.front().unwrap().text, "t0");
        assert_eq!(state.active[1].text, "t99");
        assert_eq!(state.active[2].text, "t100");
        assert_eq!(state.active[3].text, "t101");
    }

    #[test]
    fn errors_survive_non_error_chatter() {
        let toasts = shared_toasts();
        for i in 0..3 {
            push_toast(
                &toasts,
                Toast::with_kind(format!("e{i}"), ToastKind::Error, DEFAULT_TOAST_DURATION),
            );
        }
        for i in 0..4 {
            push_toast(&toasts, Toast::info(format!("i{i}")));
        }
        let state = lock(&toasts);
        assert_eq!(state.active.len(), MAX_VISIBLE_TOASTS);
        assert_eq!(state.active[0].text, "e0");
        assert_eq!(state.active[2].text, "e2");
        assert_eq!(state.active[3].text, "i3");
    }

    #[test]
    fn all_error_queue_evicts_oldest_pending_error() {
        let toasts = shared_toasts();
        for i in 0..=MAX_VISIBLE_TOASTS {
            push_toast(
                &toasts,
                Toast::with_kind(format!("e{i}"), ToastKind::Error, DEFAULT_TOAST_DURATION),
            );
        }
        let state = lock(&toasts);
        let messages: Vec<_> = state
            .active
            .iter()
            .map(|toast| toast.text.as_str())
            .collect();
        assert_eq!(messages, ["e0", "e2", "e3", "e4"]);
    }

    #[test]
    fn burst_dedup_does_not_stack_or_extend_lifetime() {
        let toasts = shared_toasts();
        let toast = Toast::info("saved");
        let deadline = toast.expires_at;
        push_toast(&toasts, toast);
        push_toast(&toasts, Toast::info("saved"));
        let state = lock(&toasts);
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.active[0].expires_at, deadline);
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].count, 2);
        assert!(state.history[0].last_seen >= state.history[0].first_seen);
    }

    #[test]
    fn dedup_distinguishes_source_severity_and_exact_text() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("same"));
        push_toast(&toasts, Toast::info("same").with_source("plugin"));
        push_toast(&toasts, warning("same"));
        push_toast(&toasts, Toast::info("same\n"));
        assert_eq!(lock(&toasts).history.len(), 4);
    }

    #[test]
    fn routine_actions_outside_burst_get_fresh_acknowledgements() {
        for kind in [ToastKind::Info, ToastKind::Success] {
            let toasts = shared_toasts();
            let toast = Toast::with_kind("saved", kind, DEFAULT_TOAST_DURATION);
            push_toast(&toasts, toast.clone());
            lock(&toasts).history[0].last_seen =
                Instant::now().checked_sub(DEDUP_WINDOW * 2).unwrap();
            prune_expired(&toasts, toast.expires_at);
            push_toast(
                &toasts,
                Toast::with_kind("saved", kind, DEFAULT_TOAST_DURATION),
            );
            let state = lock(&toasts);
            assert_eq!(state.history.len(), 2);
            assert_eq!(state.active.len(), 1);
        }
    }

    #[test]
    fn unreviewed_issue_coalesces_after_burst_without_realerting() {
        let toasts = shared_toasts();
        let toast = warning("offline");
        push_toast(&toasts, toast.clone());
        lock(&toasts).history[0].last_seen = Instant::now().checked_sub(DEDUP_WINDOW * 2).unwrap();
        prune_expired(&toasts, toast.expires_at);
        push_toast(&toasts, warning("offline"));
        let state = lock(&toasts);
        assert!(state.active.is_empty());
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].count, 2);
        assert_eq!(state.unread_count(), 1);
    }

    #[test]
    fn review_clears_attention_and_allows_immediate_issue_realert() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("saved"));
        push_toast(&toasts, warning("offline"));
        push_toast(
            &toasts,
            Toast::with_kind("failed", ToastKind::Error, DEFAULT_TOAST_DURATION),
        );
        {
            let mut state = lock(&toasts);
            assert_eq!(state.unread_count(), 2);
            state.mark_reviewed();
            assert_eq!(state.unread_count(), 0);
            assert_eq!(state.history.len(), 3);
        }
        push_toast(&toasts, warning("offline"));
        let state = lock(&toasts);
        assert_eq!(state.history.len(), 4);
        assert_eq!(state.unread_count(), 1);
        assert!(!state.history.back().unwrap().reviewed);
    }

    #[test]
    fn reviewed_current_realerts_in_fifo_without_preemption() {
        let toasts = shared_toasts();
        push_toast(&toasts, warning("offline"));
        push_toast(&toasts, Toast::info("waiting"));
        let current = {
            let mut state = lock(&toasts);
            let current = state.active[0].id;
            state.mark_reviewed();
            current
        };
        push_toast(&toasts, warning("offline"));
        {
            let state = lock(&toasts);
            assert_eq!(state.active.len(), 3);
            assert_eq!(state.active[0].id, current);
            assert_eq!(state.active[1].text, "waiting");
            assert_eq!(state.active[2].text, "offline");
            assert_ne!(state.active[2].id, current);
            assert_eq!(state.active[2].deadline(), None);
            assert_eq!(state.history.len(), 3);
            assert_eq!(state.unread_count(), 1);
        }
        push_toast(&toasts, warning("offline"));
        let state = lock(&toasts);
        assert_eq!(state.active.len(), 3);
        assert_eq!(state.history.back().unwrap().count, 2);
    }

    #[test]
    fn reviewed_pending_realert_replaces_only_matching_pending_entry() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("current"));
        push_toast(&toasts, warning("offline"));
        push_toast(&toasts, Toast::info("waiting"));
        let (current, replaced) = {
            let mut state = lock(&toasts);
            let ids = (state.active[0].id, state.active[1].id);
            state.mark_reviewed();
            ids
        };
        push_toast(&toasts, warning("offline"));
        let state = lock(&toasts);
        assert_eq!(state.active.len(), 3);
        assert_eq!(state.active[0].id, current);
        assert_eq!(state.active[1].text, "waiting");
        assert_eq!(state.active[2].text, "offline");
        assert_ne!(state.active[2].id, replaced);
        assert_eq!(state.active[2].deadline(), None);
        assert_eq!(state.history.len(), 4);
        assert_eq!(state.unread_count(), 1);
    }

    #[test]
    fn silent_notifications_only_enter_history() {
        let toasts = shared_toasts();
        record_notification(&toasts, Toast::info("trace detail"), false);
        let state = lock(&toasts);
        assert!(state.active.is_empty());
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.unread_count(), 0);
    }

    #[test]
    fn hover_preserves_remaining_lifetime_through_expiry_and_resume() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("paused"));
        let (id, expires_at) = {
            let state = lock(&toasts);
            (state.active[0].id, state.active[0].expires_at)
        };
        let remaining = Duration::from_secs(2);
        let pause_at = expires_at.checked_sub(remaining).unwrap();
        lock(&toasts).hover(Some(id), pause_at);
        let resume_at = expires_at + Duration::from_secs(30);
        assert_eq!(prune_expired(&toasts, resume_at), None);
        {
            let mut state = lock(&toasts);
            assert_eq!(state.active.len(), 1);
            assert!(state.active[0].is_live(resume_at));
            assert_eq!(state.active[0].deadline(), None);
            state.hover(None, resume_at);
            assert_eq!(state.active[0].deadline(), Some(resume_at + remaining));
        }
        assert_eq!(
            prune_expired(
                &toasts,
                (resume_at + remaining)
                    .checked_sub(Duration::from_nanos(1))
                    .unwrap(),
            ),
            Some(resume_at + remaining)
        );
        assert_eq!(prune_expired(&toasts, resume_at + remaining), None);
        assert!(lock(&toasts).active.is_empty());
    }

    #[test]
    fn hovering_current_preserves_waiting_toasts_without_deadlines() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("paused"));
        push_toast(&toasts, Toast::info("waiting"));
        let (id, expiry) = {
            let mut state = lock(&toasts);
            let id = state.active[0].id;
            state.hover(Some(id), Instant::now());
            (id, state.active[0].expires_at)
        };
        let later = expiry + Duration::from_secs(30);
        assert_eq!(prune_expired(&toasts, later), None);
        let state = lock(&toasts);
        assert_eq!(state.active.len(), 2);
        assert_eq!(state.active[0].id, id);
        assert!(state.active[1].is_live(later));
        assert_eq!(state.active[1].deadline(), None);
    }

    #[test]
    fn pending_and_stale_hover_ids_never_start_waiting_lifetimes() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("hovered"));
        let now = Instant::now();
        let id = lock(&toasts).active[0].id;
        lock(&toasts).hover(Some(id), now);
        push_toast(&toasts, Toast::info("arrival"));
        let mut state = lock(&toasts);
        assert_eq!(state.active[0].deadline(), None);
        assert_eq!(state.active[1].deadline(), None);
        let original_deadline = state.active[0].expires_at;
        let pending_id = state.active[1].id;
        let pending_expiry = state.active[1].expires_at;
        let later = pending_expiry + Duration::from_secs(30);
        state.hover(Some(pending_id), later);
        assert_eq!(
            state.active[0].expires_at,
            original_deadline + (later - now)
        );
        assert!(state.active[0].deadline().is_some());
        assert!(state.active[1].is_live(later));
        assert_eq!(state.active[1].deadline(), None);
        assert_eq!(state.active[1].expires_at, pending_expiry);
        let revision = state.revision();
        let expired_at = state.active[0].expires_at;
        state.hover(Some(id), expired_at);
        state.hover(Some(u64::MAX), expired_at);
        assert_eq!(state.revision(), revision);
        assert!(!state.active[0].is_live(expired_at));
        assert!(state.active[1].is_live(expired_at));
    }

    #[test]
    fn overflow_never_evicts_or_preempts_hovered_current() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("current"));
        let id = lock(&toasts).active[0].id;
        lock(&toasts).hover(Some(id), Instant::now());
        for i in 0..MAX_VISIBLE_TOASTS + 2 {
            push_toast(
                &toasts,
                Toast::with_kind(
                    format!("error {i}"),
                    ToastKind::Error,
                    DEFAULT_TOAST_DURATION,
                ),
            );
        }
        let later = Instant::now() + Duration::from_secs(30);
        assert_eq!(prune_expired(&toasts, later), None);
        let state = lock(&toasts);
        assert_eq!(state.active.len(), MAX_VISIBLE_TOASTS);
        assert_eq!(state.active[0].id, id);
        assert_eq!(state.active[0].deadline(), None);
        let messages: Vec<_> = state
            .active
            .iter()
            .map(|toast| toast.text.as_str())
            .collect();
        assert_eq!(messages, ["current", "error 3", "error 4", "error 5"]);
        assert!(state.active.iter().all(|toast| toast.is_live(later)));
    }

    #[test]
    fn dismissal_leaves_history_unread_and_dedup_intact() {
        let toasts = shared_toasts();
        push_toast(&toasts, warning("offline"));
        push_toast(&toasts, Toast::info("saved"));
        {
            let mut state = lock(&toasts);
            let dismissed = state.active[0].id;
            let retained = state.active[1].id;
            let before = state.revision();
            assert!(state.dismiss(dismissed));
            assert_ne!(state.revision(), before);
            let revision = state.revision();
            assert!(!state.dismiss(dismissed));
            assert_eq!(state.revision(), revision);
            assert_eq!(state.active.len(), 1);
            assert_eq!(state.active[0].id, retained);
            assert_eq!(state.history.len(), 2);
            assert_eq!(state.unread_count(), 1);
            assert!(!state.history[0].reviewed);
        }
        push_toast(&toasts, warning("offline"));
        let state = lock(&toasts);
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history.back().unwrap().count, 2);
        assert_eq!(state.unread_count(), 1);
    }

    #[test]
    fn repeated_hover_only_changes_revision_on_pause_transitions() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("first"));
        push_toast(&toasts, Toast::info("second"));
        let mut state = lock(&toasts);
        let first = state.active[0].id;
        let second = state.active[1].id;
        let now = Instant::now();
        let initial = state.revision();
        state.hover(None, now);
        assert_eq!(state.revision(), initial);
        state.hover(Some(first), now);
        assert_eq!(state.revision(), initial + 1);
        state.hover(Some(first), now + Duration::from_millis(100));
        assert_eq!(state.revision(), initial + 1);
        state.hover(Some(second), now + Duration::from_millis(200));
        assert_eq!(state.revision(), initial + 2);
        assert!(state.active[0].deadline().is_some());
        assert_eq!(state.active[1].deadline(), None);
        state.hover(None, now + Duration::from_millis(300));
        assert_eq!(state.revision(), initial + 2);
        state.hover(None, now + Duration::from_millis(400));
        assert_eq!(state.revision(), initial + 2);
    }

    #[test]
    fn recorded_ids_stay_stable_across_mutation_and_are_not_reused() {
        let toasts = shared_toasts();
        push_toast(&toasts, Toast::info("first"));
        push_toast(&toasts, Toast::info("second"));
        let (first, second) = {
            let state = lock(&toasts);
            (state.active[0].id, state.active[1].id)
        };
        assert_ne!(first, 0);
        assert_ne!(first, second);
        push_toast(&toasts, Toast::info("second"));
        let clone = {
            let mut state = lock(&toasts);
            state.dismiss(first);
            state.mark_reviewed();
            state.hover(Some(second), Instant::now());
            assert_eq!(state.active[0].id, second);
            state.active[0].clone()
        };
        push_toast(&toasts, Toast::info("third"));
        let mut replacement = clone;
        replacement.text = "fourth".into();
        push_toast(&toasts, replacement);
        let state = lock(&toasts);
        assert_eq!(state.active[0].id, second);
        assert_ne!(state.active[1].id, first);
        assert_ne!(state.active[1].id, second);
        assert_ne!(state.active[2].id, second);
        assert_ne!(state.active[2].id, state.active[1].id);
        assert_eq!(state.active[2].deadline(), None);
    }

    #[test]
    fn prune_returns_earliest_live_deadline_without_touching_history() {
        let toasts = shared_toasts();
        let early = Toast::with_kind("early", ToastKind::Info, Duration::from_secs(30));
        let late = Toast::with_kind("late", ToastKind::Info, Duration::from_secs(60));
        let deadline = early.expires_at;
        push_toast(&toasts, early);
        push_toast(&toasts, late);
        assert_eq!(prune_expired(&toasts, Instant::now()), Some(deadline));
        assert_eq!(lock(&toasts).history.len(), 2);
    }
}
