//! Persists a session's durable loop events to its JSONL file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use kage_core::{LoopEvent, Message, Role, TokenUsage};
use kage_plugin::{PendingSessionOp, PluginRuntime};
use kage_session::{
    Compaction, Custom, EntryId, Header, Label, MessageEntry, ModelChange, SessionEntry,
    SessionError, SessionWriter,
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

    pub(crate) fn path(&self) -> &Path {
        match &self.target {
            Target::Open(writer) => writer.path(),
            Target::Planned { path, .. } => path,
        }
    }

    /// Append an entry the loop does not produce, such as a title.
    pub(crate) fn append(&mut self, entry: &SessionEntry) -> Result<(), SessionError> {
        self.writer()?.append(entry)
    }

    /// Record a message added to history outside the loop, such as the
    /// output of a user shell command.
    pub(crate) fn message(&mut self, message: &Message) -> Result<(), SessionError> {
        self.writer()?.append(&SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: message.clone(),
            usage: None,
        }))
    }

    /// Record a switch to `model`. A file not created yet gets it in its
    /// header instead, so a model switch alone writes nothing to disk.
    pub(crate) fn set_model(&mut self, model: &str) -> Result<(), SessionError> {
        match &mut self.target {
            Target::Planned { header, .. } => {
                model.clone_into(&mut header.model);
                Ok(())
            }
            Target::Open(writer) => writer.append(&SessionEntry::ModelChange(ModelChange {
                id: EntryId::new(),
                ts: Utc::now(),
                model: model.to_owned(),
            })),
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
            if let Some(entry) = plugin_op_entry(op) {
                self.writer()?.append(&entry)?;
            }
        }
        Ok(())
    }
}

/// Turn a plugin-requested session operation into the entry to append.
///
/// A label whose anchor is not a valid entry id is dropped with a warning:
/// writing it with a fresh id would silently detach it from its target.
pub(crate) fn plugin_op_entry(op: PendingSessionOp) -> Option<SessionEntry> {
    match op {
        PendingSessionOp::AppendCustom { kind, data } => Some(SessionEntry::Custom(Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind,
            data,
        })),
        PendingSessionOp::SetLabel { anchor, text } => {
            let Ok(parsed) = ulid::Ulid::from_string(&anchor) else {
                eprintln!("kage: set_label: invalid entry id '{anchor}', dropping");
                return None;
            };
            Some(SessionEntry::Label(Label {
                id: EntryId::new(),
                ts: Utc::now(),
                text,
                anchor: EntryId(parsed),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use kage_core::{Content, MessageId, StopReason};
    use kage_session::{FORMAT_VERSION, SessionReader};

    use super::*;

    fn header(dir: &std::path::Path) -> (PathBuf, Header) {
        let session = kage_core::SessionId::new();
        let header = Header {
            version: FORMAT_VERSION,
            session,
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: dir.to_path_buf(),
            model: "mock:m".into(),
            system_prompt: String::new(),
            parent_session: None,
            parent_entry: None,
        };
        (dir.join(format!("{session}.jsonl")), header)
    }

    fn entries(path: &Path) -> Vec<SessionEntry> {
        SessionReader::iter(path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn appended(role: Role, text: &str) -> LoopEvent {
        LoopEvent::MessageAppended {
            message: Message::new(role, vec![Content::Text { text: text.into() }], None),
        }
    }

    #[test]
    fn planned_file_appears_with_the_first_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (path, header) = header(dir.path());
        let mut recorder = Recorder::planned(path.clone(), header, None);
        recorder
            .observe(&LoopEvent::TurnEnded {
                index: 0,
                had_tool_calls: false,
            })
            .unwrap();
        assert!(!path.exists());
        recorder.observe(&appended(Role::User, "hi")).unwrap();
        assert_eq!(entries(&path).len(), 2);
    }

    #[test]
    fn assistant_messages_carry_their_turn_usage() {
        let dir = tempfile::tempdir().unwrap();
        let (path, header) = header(dir.path());
        let mut recorder = Recorder::planned(path.clone(), header, None);
        let usage = TokenUsage {
            input: 7,
            ..TokenUsage::default()
        };
        recorder.observe(&appended(Role::User, "q")).unwrap();
        recorder
            .observe(&LoopEvent::MessageEnd {
                id: MessageId::new(),
                usage,
                stop_reason: StopReason::EndTurn,
            })
            .unwrap();
        recorder.observe(&appended(Role::Assistant, "a")).unwrap();
        recorder
            .observe(&LoopEvent::Compaction {
                kept: 1,
                summarized: 2,
                summary: "s".into(),
            })
            .unwrap();

        let written = entries(&path);
        let usages: Vec<Option<TokenUsage>> = written
            .iter()
            .filter_map(|e| match e {
                SessionEntry::Message(m) => Some(m.usage),
                _ => None,
            })
            .collect();
        assert_eq!(usages, [None, Some(usage)]);
        assert!(matches!(written.last(), Some(SessionEntry::Compaction(_))));
    }

    #[test]
    fn a_model_switch_goes_to_the_planned_header_or_an_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (path, header) = header(dir.path());
        let mut recorder = Recorder::planned(path.clone(), header, None);
        recorder.set_model("mock:a").unwrap();
        assert!(!path.exists());
        recorder.observe(&appended(Role::User, "hi")).unwrap();
        recorder.set_model("mock:b").unwrap();

        let written = entries(&path);
        assert!(matches!(&written[0], SessionEntry::Header(h) if h.model == "mock:a"));
        assert!(matches!(&written[2], SessionEntry::ModelChange(m) if m.model == "mock:b"));
    }

    #[test]
    fn plugin_session_ops_are_written_at_turn_end() {
        let dir = tempfile::tempdir().unwrap();
        let (path, header) = header(dir.path());
        let runtime = Arc::new(PluginRuntime::new().unwrap());
        runtime
            .eval("kage.session.append_entry('plugin:tps', { rate = 12.5 })")
            .unwrap();
        let anchor = EntryId::new();
        runtime
            .eval(&format!("kage.session.set_label('{anchor}', 'milestone')"))
            .unwrap();
        runtime
            .eval("kage.session.set_label('not-a-ulid', 'dropped')")
            .unwrap();
        let mut recorder = Recorder::planned(path.clone(), header, Some(Arc::clone(&runtime)));
        recorder
            .observe(&LoopEvent::TurnEnded {
                index: 0,
                had_tool_calls: false,
            })
            .unwrap();

        let written = entries(&path);
        assert_eq!(written.len(), 3);
        assert!(matches!(&written[1], SessionEntry::Custom(c) if c.kind == "plugin:tps"));
        assert!(matches!(&written[2], SessionEntry::Label(l) if l.anchor == anchor));
        assert!(runtime.take_pending_session_ops().is_empty());
    }
}
