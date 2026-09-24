//! Persists a session's durable loop events to its JSONL file.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use kage_core::{LoopEvent, Role, TokenUsage};
use kage_plugin::PluginRuntime;
use kage_session::{
    Compaction, EntryId, Header, MessageEntry, SessionEntry, SessionError, SessionWriter,
};

/// Writes every appended message and compaction of one session.
///
/// Assistant messages carry the usage of the `MessageEnd` that preceded
/// them. Plugin session operations queued during a turn are written when
/// the turn ends.
pub(crate) struct Recorder {
    target: Target,
    plugins: Option<Arc<PluginRuntime>>,
    turn_usage: Option<TokenUsage>,
}

enum Target {
    Open(SessionWriter),
    /// Not created yet: the file appears with the first entry, so a
    /// session nobody prompts leaves nothing on disk.
    Planned {
        path: PathBuf,
        header: Box<Header>,
    },
}

impl Recorder {
    pub(crate) fn new(writer: SessionWriter, plugins: Option<Arc<PluginRuntime>>) -> Self {
        Self {
            target: Target::Open(writer),
            plugins,
            turn_usage: None,
        }
    }

    /// Record to a new file at `path`, created on the first write.
    pub(crate) fn planned(
        path: PathBuf,
        header: Header,
        plugins: Option<Arc<PluginRuntime>>,
    ) -> Self {
        Self {
            target: Target::Planned {
                path,
                header: Box::new(header),
            },
            plugins,
            turn_usage: None,
        }
    }

    fn writer(&mut self) -> Result<&mut SessionWriter, SessionError> {
        if let Target::Planned { path, header } = &self.target {
            let writer = SessionWriter::create(path.clone(), (**header).clone())?;
            self.target = Target::Open(writer);
        }
        match &mut self.target {
            Target::Open(writer) => Ok(writer),
            Target::Planned { .. } => unreachable!("planned target was just opened"),
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
                self.writer()?.append(&SessionEntry::Message(MessageEntry {
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
            } => self.writer()?.append(&SessionEntry::Compaction(Compaction {
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
                self.writer()?.append(&entry)?;
            }
        }
        Ok(())
    }
}
