//! Persists a session's durable loop events to its JSONL file.

use std::sync::Arc;

use chrono::Utc;
use kage_core::{LoopEvent, Role, TokenUsage};
use kage_plugin::PluginRuntime;
use kage_session::{Compaction, EntryId, MessageEntry, SessionEntry, SessionError, SessionWriter};

/// Writes every appended message and compaction of one session.
///
/// Assistant messages carry the usage of the `MessageEnd` that preceded
/// them. Plugin session operations queued during a turn are written when
/// the turn ends.
pub(crate) struct Recorder {
    writer: SessionWriter,
    plugins: Option<Arc<PluginRuntime>>,
    turn_usage: Option<TokenUsage>,
}

impl Recorder {
    pub(crate) fn new(writer: SessionWriter, plugins: Option<Arc<PluginRuntime>>) -> Self {
        Self {
            writer,
            plugins,
            turn_usage: None,
        }
    }

    pub(crate) fn observe(&mut self, event: &LoopEvent) -> Result<(), SessionError> {
        match event {
            LoopEvent::MessageEnd { usage, .. } => {
                self.turn_usage = Some(*usage);
                Ok(())
            }
            LoopEvent::MessageAppended { message } => {
                let usage = if message.role == Role::Assistant {
                    self.turn_usage.take()
                } else {
                    None
                };
                self.writer.append(&SessionEntry::Message(MessageEntry {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    message: message.clone(),
                    usage,
                }))
            }
            LoopEvent::Compaction {
                kept,
                summarized,
                summary,
            } => self.writer.append(&SessionEntry::Compaction(Compaction {
                id: EntryId::new(),
                ts: Utc::now(),
                kept: *kept,
                summarized: *summarized,
                summary: summary.clone(),
            })),
            LoopEvent::TurnEnded { .. } => self.write_plugin_ops(),
            _ => Ok(()),
        }
    }

    fn write_plugin_ops(&mut self) -> Result<(), SessionError> {
        let Some(plugins) = &self.plugins else {
            return Ok(());
        };
        for op in plugins.take_pending_session_ops() {
            if let Some(entry) = crate::session::plugin_op_entry(op) {
                self.writer.append(&entry)?;
            }
        }
        Ok(())
    }
}
