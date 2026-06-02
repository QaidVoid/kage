//! Test helpers for plugin authors.
//!
//! Available under `#[cfg(test)]` automatically, or for downstream
//! crates by enabling the `testing` feature on `kage-plugin`. A plugin
//! author builds a runtime with a [`RecordingSink`], evaluates their
//! plugin, fires events or invokes registered surfaces, and asserts on
//! the notifications and log lines the plugin emitted, mirroring how the
//! in-repo tests exercise the runtime.
//!
//! ```no_run
//! let (rec, rt) = kage_plugin::testing::runtime_with_recording(".".into());
//! rt.eval_plugin("my_plugin", "kage.notify('hello')").unwrap();
//! assert_eq!(rec.snapshot().notifications, ["hello"]);
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::api::{HostLog, LogLevel, SharedHostLog};
use crate::runtime::PluginRuntime;

/// Notifications and log lines a plugin emitted through the host sink.
#[derive(Debug, Default, Clone)]
pub struct RecordedOutput {
    /// `kage.ui.notify` / `kage.notify` messages, in order.
    pub notifications: Vec<String>,
    /// `kage.log` lines as `(level, message)`, in order.
    pub logs: Vec<(LogLevel, String)>,
}

/// A shareable [`HostLog`] that records everything a plugin emits.
///
/// Clones share the same buffer, so the handle returned alongside the
/// runtime reads what the plugin emitted after evaluation.
#[derive(Clone, Default)]
pub struct RecordingSink(Arc<Mutex<RecordedOutput>>);

impl RecordingSink {
    /// Snapshot what the plugin has emitted so far.
    #[must_use]
    pub fn snapshot(&self) -> RecordedOutput {
        self.0.lock().expect("recording sink poisoned").clone()
    }
}

impl HostLog for RecordingSink {
    fn notify(&mut self, message: &str) {
        self.0
            .lock()
            .expect("recording sink poisoned")
            .notifications
            .push(message.to_owned());
    }

    fn log(&mut self, level: LogLevel, message: &str) {
        self.0
            .lock()
            .expect("recording sink poisoned")
            .logs
            .push((level, message.to_owned()));
    }
}

/// Build a [`SharedHostLog`] backed by a [`RecordingSink`], returning the
/// readable handle and the sink to hand to
/// [`crate::PluginRuntimeBuilder::sink`].
#[must_use]
pub fn recording_sink() -> (RecordingSink, SharedHostLog) {
    let sink = RecordingSink::default();
    let shared: SharedHostLog =
        Arc::new(Mutex::new(Box::new(sink.clone()) as Box<dyn HostLog + Send>));
    (sink, shared)
}

/// Build a [`PluginRuntime`] wired to a fresh [`RecordingSink`], rooted at
/// `workdir`. Returns the recording handle and the runtime.
///
/// # Panics
///
/// Panics if the runtime fails to build, which for a default
/// configuration indicates a bug in the host rather than the plugin.
#[must_use]
pub fn runtime_with_recording(workdir: PathBuf) -> (RecordingSink, PluginRuntime) {
    let (rec, sink) = recording_sink();
    let rt = PluginRuntime::builder()
        .sink(sink)
        .workdir(workdir)
        .build()
        .expect("plugin runtime builds with default configuration");
    (rec, rt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_notifications_and_logs_from_a_plugin() {
        let (rec, rt) = runtime_with_recording(PathBuf::from("."));
        rt.eval_plugin("demo", "kage.notify('hello'); kage.log('warn', 'careful')")
            .unwrap();
        let out = rec.snapshot();
        assert_eq!(out.notifications, ["hello"]);
        assert_eq!(out.logs, [(LogLevel::Warn, "careful".to_owned())]);
    }
}
