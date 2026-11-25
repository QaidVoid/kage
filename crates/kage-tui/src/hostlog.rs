//! User-visible runtime diagnostics retained outside the conversation.
//!
//! Plugins and embedded hosts share this sink, so startup and background
//! messages use the same severity-aware notification history as UI actions.

use std::sync::{Arc, Mutex};

use kage_plugin::{HostLog, LogLevel, SharedHostLog};

use crate::toast::{self, SharedToasts, Toast, ToastKind};

/// Build a host sink that retains notifications and logs without adding
/// conversation blocks. Trace and debug entries are retained silently.
#[must_use]
pub fn notification_host_log(toasts: SharedToasts) -> SharedHostLog {
    Arc::new(Mutex::new(Box::new(NotificationHostLog { toasts })))
}

struct NotificationHostLog {
    toasts: SharedToasts,
}

impl NotificationHostLog {
    fn record(&self, level: LogLevel, message: &str) {
        let kind = match level {
            LogLevel::Error => ToastKind::Error,
            LogLevel::Warn => ToastKind::Warning,
            _ => ToastKind::Info,
        };
        let text = if level == LogLevel::Error {
            tidy_error(message)
        } else {
            message.to_owned()
        };
        let toast =
            Toast::with_kind(text, kind, toast::DEFAULT_TOAST_DURATION).with_source("runtime");
        toast::record_notification(
            &self.toasts,
            toast,
            !matches!(level, LogLevel::Trace | LogLevel::Debug),
        );
    }
}

impl HostLog for NotificationHostLog {
    fn notify(&mut self, message: &str) {
        self.record(LogLevel::Info, message);
    }

    fn notify_level(&mut self, level: LogLevel, message: &str) {
        self.record(level, message);
    }

    fn log(&mut self, level: LogLevel, message: &str) {
        self.record(level, message);
    }
}

/// An error log line as the user reads it: the Lua traceback and the
/// `lua error: ` wrapper dropped, and for `init.lua` how to retry.
fn tidy_error(message: &str) -> String {
    let message = message
        .split_once("\nstack traceback:")
        .map_or(message, |(head, _)| head)
        .trim_end();
    match message.strip_prefix("init.lua: ") {
        Some(rest) => {
            let rest = rest.strip_prefix("lua error: ").unwrap_or(rest);
            format!("init.lua: {rest}\nFix init.lua; saving it reloads the plugins.")
        }
        None => message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toast::shared_toasts;
    use kage_core::sync::lock;

    #[test]
    fn warnings_are_retained_once_with_their_severity() {
        let notifications = shared_toasts();
        let sink = notification_host_log(notifications.clone());
        for _ in 0..8 {
            lock(&sink).notify_level(LogLevel::Warn, "plugin needs capabilities");
        }
        let state = lock(&notifications);
        assert_eq!(state.history.len(), 1);
        let entry = state.history.front().unwrap();
        assert_eq!(entry.text, "plugin needs capabilities");
        assert_eq!(entry.kind, ToastKind::Warning);
        assert_eq!(entry.count, 8);
        assert_eq!(state.active.len(), 1);
    }

    #[test]
    fn trace_and_debug_logs_are_reviewable_without_interrupting() {
        let notifications = shared_toasts();
        let sink = notification_host_log(notifications.clone());
        lock(&sink).log(LogLevel::Trace, "trace detail");
        lock(&sink).log(LogLevel::Debug, "debug detail");
        let state = lock(&notifications);
        assert_eq!(state.history.len(), 2);
        assert!(state.active.is_empty());
        assert_eq!(state.unread_count(), 0);
    }

    #[test]
    fn startup_and_live_logs_use_the_same_notification_route() {
        let notifications = shared_toasts();
        let sink = notification_host_log(notifications.clone());
        lock(&sink).log(LogLevel::Warn, "startup warning");
        lock(&sink).log(LogLevel::Error, "live error");
        let state = lock(&notifications);
        assert_eq!(state.history[0].kind, ToastKind::Warning);
        assert_eq!(state.history[1].kind, ToastKind::Error);
        assert_eq!(state.unread_count(), 2);
    }

    #[test]
    fn an_init_lua_error_names_how_to_retry() {
        let notifications = shared_toasts();
        let sink = notification_host_log(notifications.clone());
        lock(&sink).log(
            LogLevel::Error,
            "init.lua: lua error: runtime error: init.lua:3: boom\nstack traceback:\n details",
        );
        let state = lock(&notifications);
        let error = &state.history[0];
        assert_eq!(error.kind, ToastKind::Error);
        assert!(error.text.contains("Fix init.lua"));
        assert!(!error.text.contains("stack traceback"));
    }
}
