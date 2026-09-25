//! [`HostLog`] sink that routes plugin notifications and log lines into
//! the TUI's overlay (notify) and conversation buffer (log).
//!
//! While the TUI owns the alt screen, anything written directly to
//! stderr corrupts the rendered frame (the OS happily mixes raw
//! escape-free bytes with our cursor-positioned redraws). Plugin code
//! that calls `kage.notify` or `kage.log` therefore needs a sink that
//! pushes through the same render path as everything else.
//!
//! `notify` lands as an ephemeral toast (a row below the
//! conversation, auto-expires) so plugin chatter does not pollute the conversation
//! pane. `log` (especially error level) keeps the inline path because
//! the user wants to scroll back and review. Once the engine runs, `log`
//! goes through a [`LogPublisher`] instead, so a line lands after the
//! engine events already published (the prompt, the tool call). Either
//! way an error reads the same: a `kage:error` block without the Lua
//! traceback, and an `init.lua` failure says how to retry.

use std::sync::{Arc, Mutex, OnceLock};

use kage_core::sync::lock;
use kage_plugin::{HostLog, LogLevel, SharedHostLog};

use crate::events::SharedBuffer;
use crate::toast::{self, SharedToasts, Toast, ToastKind};

/// Publishes a plugin log line as an engine event.
pub type LogPublisher = Box<dyn Fn(LogLevel, &str) + Send + Sync>;

/// Build a [`SharedHostLog`] that pushes plugin `notify` calls onto
/// `toasts`. `log` calls go to `publisher` once it is set, and into
/// `buffer` as `kage:log` custom blocks before that.
#[must_use]
pub fn buffer_host_log(
    buffer: SharedBuffer,
    toasts: SharedToasts,
    publisher: Arc<OnceLock<LogPublisher>>,
) -> SharedHostLog {
    Arc::new(Mutex::new(Box::new(BufferHostLog {
        buffer,
        toasts,
        publisher,
    }) as Box<dyn HostLog + Send>))
}

struct BufferHostLog {
    buffer: SharedBuffer,
    toasts: SharedToasts,
    publisher: Arc<OnceLock<LogPublisher>>,
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
        let tidied;
        let message = if level == LogLevel::Error {
            tidied = tidy_error(message);
            tidied.as_str()
        } else {
            message
        };
        if let Some(publish) = self.publisher.get() {
            publish(level, message);
            return;
        }
        let mut buffer = lock(&self.buffer);
        match level {
            LogLevel::Error => buffer.push_custom("kage:error", message, false),
            LogLevel::Info => buffer.push_custom("kage:log", message, true),
            _ => buffer.push_custom("kage:log", format!("[{level:?}] {message}"), true),
        }
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
            format!("init.lua: {rest}\nFix init.lua, then run /reload.")
        }
        None => message.to_owned(),
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
        let sink = buffer_host_log(buffer.clone(), toasts.clone(), Arc::default());
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
    fn log_error_block_is_an_unfolded_error_like_a_published_one() {
        let buffer = shared_buffer();
        let toasts = shared_toasts();
        let sink = buffer_host_log(buffer.clone(), toasts, Arc::default());
        sink.lock().unwrap().log(LogLevel::Error, "boom");
        let buf = buffer.lock().unwrap();
        match &buf.blocks()[0] {
            Block::Custom { kind, folded, text } => {
                assert_eq!(kind, "kage:error");
                assert_eq!(text, "boom");
                assert!(!folded);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn an_init_lua_error_drops_the_traceback_and_names_the_fix() {
        let buffer = shared_buffer();
        let publisher: Arc<OnceLock<LogPublisher>> = Arc::default();
        let sink = buffer_host_log(buffer.clone(), shared_toasts(), Arc::clone(&publisher));
        let raw = "init.lua: lua error: runtime error: init.lua:3: boom\n\
                   stack traceback:\n\t[C]: in function 'error'";
        sink.lock().unwrap().log(LogLevel::Error, raw);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        assert!(
            publisher
                .set(Box::new(move |_, message| {
                    seen.lock().unwrap().push(message.to_owned());
                }))
                .is_ok()
        );
        sink.lock().unwrap().log(LogLevel::Error, raw);
        let want = "init.lua: runtime error: init.lua:3: boom\nFix init.lua, then run /reload.";
        let buf = buffer.lock().unwrap();
        assert!(matches!(&buf.blocks()[0], Block::Custom { text, .. } if text == want));
        assert_eq!(*calls.lock().unwrap(), [want]);
    }

    #[test]
    fn log_info_block_is_folded_to_keep_chrome_quiet() {
        let buffer = shared_buffer();
        let toasts = shared_toasts();
        let sink = buffer_host_log(buffer.clone(), toasts, Arc::default());
        sink.lock().unwrap().log(LogLevel::Info, "ok");
        let buf = buffer.lock().unwrap();
        match &buf.blocks()[0] {
            Block::Custom { text, folded, .. } => {
                assert_eq!(text, "ok");
                assert!(folded);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn log_goes_to_the_buffer_before_the_publisher_is_set_and_to_it_after() {
        let buffer = shared_buffer();
        let publisher: Arc<OnceLock<LogPublisher>> = Arc::default();
        let sink = buffer_host_log(buffer.clone(), shared_toasts(), Arc::clone(&publisher));
        sink.lock().unwrap().log(LogLevel::Warn, "early");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        assert!(
            publisher
                .set(Box::new(move |level, message| {
                    seen.lock().unwrap().push((level, message.to_owned()));
                }))
                .is_ok()
        );
        sink.lock().unwrap().log(LogLevel::Error, "late");
        let buf = buffer.lock().unwrap();
        assert_eq!(buf.blocks().len(), 1);
        assert!(matches!(
            &buf.blocks()[0],
            Block::Custom { text, .. } if text == "[Warn] early"
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(LogLevel::Error, "late".to_owned())]
        );
    }

    #[test]
    fn published_notice_lands_below_the_user_block() {
        use kage_core::protocol::{Envelope, HostEvent, NoticeLevel};
        use kage_core::{Content, LoopEvent, Message, Role, SessionId};

        let buffer = shared_buffer();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::App::new(buffer.clone(), tx);
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        app.set_engine_events(events_rx);
        let session = SessionId::new();
        let message = Message::new(Role::User, vec![Content::Text { text: "hi".into() }], None);
        events_tx
            .send(Envelope {
                session,
                seq: 1,
                event: LoopEvent::MessageAppended { message }.into(),
            })
            .unwrap();
        let publisher: Arc<OnceLock<LogPublisher>> = Arc::default();
        assert!(
            publisher
                .set(Box::new(move |_, message| {
                    let _ = events_tx.send(Envelope {
                        session,
                        seq: 2,
                        event: HostEvent::Notice {
                            level: NoticeLevel::Info,
                            text: message.to_owned(),
                            transient: false,
                        }
                        .into(),
                    });
                }))
                .is_ok()
        );
        let sink = buffer_host_log(buffer.clone(), shared_toasts(), publisher);
        sink.lock().unwrap().log(LogLevel::Info, "plugin says hi");
        assert!(app.drain_engine_events());
        let buf = buffer.lock().unwrap();
        let blocks = buf.blocks();
        assert!(matches!(&blocks[0], Block::User { text } if text == "hi"));
        assert!(matches!(
            &blocks[1],
            Block::Custom { text, .. } if text == "plugin says hi"
        ));
    }
}
