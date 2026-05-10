//! [`HostLog`] sink that routes plugin notifications and log lines into
//! the TUI's overlay (notify) and conversation buffer (log).
//!
//! While the TUI owns the alt screen, anything written directly to
//! stderr corrupts the rendered frame (the OS happily mixes raw
//! escape-free bytes with our cursor-positioned redraws). Plugin code
//! that calls `kage.notify` or `kage.log` therefore needs a sink that
//! pushes through the same render path as everything else.
//!
//! `notify` lands as an ephemeral toast (top-right overlay, auto-
//! expires) so plugin chatter does not pollute the conversation
//! pane. `log` (especially error level) keeps the inline path because
//! the user wants to scroll back and review.

use kage_plugin::{HostLog, LogLevel, SharedHostLog};

use crate::events::SharedBuffer;
use crate::toast::{self, SharedToasts, Toast, ToastKind};

/// Build a [`SharedHostLog`] that pushes plugin `notify` calls onto
/// `toasts` and `log` calls into `buffer` as `kage:log` custom blocks.
#[must_use]
pub fn buffer_host_log(buffer: SharedBuffer, toasts: SharedToasts) -> SharedHostLog {
    use std::sync::{Arc, Mutex};
    Arc::new(Mutex::new(
        Box::new(BufferHostLog { buffer, toasts }) as Box<dyn HostLog + Send>
    ))
}

struct BufferHostLog {
    buffer: SharedBuffer,
    toasts: SharedToasts,
}

impl HostLog for BufferHostLog {
    fn notify(&mut self, message: &str) {
        toast::push_toast(
            &self.toasts,
            Toast::with_kind(
                message.to_owned(),
                ToastKind::Info,
                toast::DEFAULT_TOAST_DURATION,
            ),
        );
    }
    fn log(&mut self, level: LogLevel, message: &str) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom(
                "kage:log",
                format!("[{level:?}] {message}"),
                level != LogLevel::Error,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Block;
    use crate::events::shared_buffer;
    use crate::toast::shared_toasts;

    #[test]
    fn notify_pushes_a_toast_and_does_not_touch_the_buffer() {
        let buffer = shared_buffer();
        let toasts = shared_toasts();
        let sink = buffer_host_log(buffer.clone(), toasts.clone());
        sink.lock().unwrap().notify("plugin loaded");
        assert!(
            buffer.lock().unwrap().blocks().is_empty(),
            "notify must not append a buffer block"
        );
        let q = toasts.lock().unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q.front().unwrap().text, "plugin loaded");
    }

    #[test]
    fn log_error_block_is_unfolded_so_failures_are_visible() {
        let buffer = shared_buffer();
        let toasts = shared_toasts();
        let sink = buffer_host_log(buffer.clone(), toasts);
        sink.lock().unwrap().log(LogLevel::Error, "boom");
        let buf = buffer.lock().unwrap();
        match &buf.blocks()[0] {
            Block::Custom { kind, folded, .. } => {
                assert_eq!(kind, "kage:log");
                assert!(!folded);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn log_info_block_is_folded_to_keep_chrome_quiet() {
        let buffer = shared_buffer();
        let toasts = shared_toasts();
        let sink = buffer_host_log(buffer.clone(), toasts);
        sink.lock().unwrap().log(LogLevel::Info, "ok");
        let buf = buffer.lock().unwrap();
        match &buf.blocks()[0] {
            Block::Custom { folded, .. } => assert!(folded),
            _ => panic!(),
        }
    }
}
