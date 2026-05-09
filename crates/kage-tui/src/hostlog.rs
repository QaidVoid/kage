//! [`HostLog`] sink that routes plugin notifications and log lines into
//! the TUI's conversation buffer instead of stderr.
//!
//! While the TUI owns the alt screen, anything written directly to
//! stderr corrupts the rendered frame (the OS happily mixes raw
//! escape-free bytes with our cursor-positioned redraws). Plugin code
//! that calls `kage.notify` or `kage.log` therefore needs a sink that
//! pushes through the same render path as everything else.

use kage_plugin::{HostLog, LogLevel, SharedHostLog};

use crate::events::SharedBuffer;

/// Build a [`SharedHostLog`] that pushes plugin output into `buffer`
/// as `kage:notify` / `kage:log` custom blocks.
#[must_use]
pub fn buffer_host_log(buffer: SharedBuffer) -> SharedHostLog {
    use std::sync::{Arc, Mutex};
    Arc::new(Mutex::new(
        Box::new(BufferHostLog { buffer }) as Box<dyn HostLog + Send>
    ))
}

struct BufferHostLog {
    buffer: SharedBuffer,
}

impl HostLog for BufferHostLog {
    fn notify(&mut self, message: &str) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom("kage:notify", message.to_owned(), false);
        }
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

    #[test]
    fn notify_pushes_a_custom_block_into_the_buffer() {
        let buffer = shared_buffer();
        let sink = buffer_host_log(buffer.clone());
        sink.lock().unwrap().notify("plugin loaded");
        let buf = buffer.lock().unwrap();
        match &buf.blocks()[0] {
            Block::Custom { kind, text, .. } => {
                assert_eq!(kind, "kage:notify");
                assert_eq!(text, "plugin loaded");
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn log_error_block_is_unfolded_so_failures_are_visible() {
        let buffer = shared_buffer();
        let sink = buffer_host_log(buffer.clone());
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
        let sink = buffer_host_log(buffer.clone());
        sink.lock().unwrap().log(LogLevel::Info, "ok");
        let buf = buffer.lock().unwrap();
        match &buf.blocks()[0] {
            Block::Custom { folded, .. } => assert!(folded),
            _ => panic!(),
        }
    }
}
