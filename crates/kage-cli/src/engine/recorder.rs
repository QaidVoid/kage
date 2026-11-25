//! Persists a session's durable loop events to its JSONL file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use kage_core::{LoopEvent, Message, Role, TokenUsage, sync::lock};
use kage_plugin::{LogLevel, PendingSessionOp, SharedHostLog, session_plugins::SessionPlugins};
use kage_session::{
    Compaction, Custom, EntryId, Header, Label, MessageEntry, ModelChange, RejectedUsage,
    SessionEntry, SessionError, SessionWriter, UsageCost,
};

/// Writes appended messages, compactions, and per-attempt accounting for one session.
///
/// Assistant messages carry the usage and captured cost of the `MessageEnd`
/// that preceded them. Plugin session operations queued during a turn are
/// written when the turn ends.
pub(crate) struct Recorder {
    target: Target,
    plugins: Option<Arc<dyn SessionPlugins>>,
    turn_usage: Option<PendingUsage>,
}

struct PendingUsage {
    usage: TokenUsage,
    model: String,
    amount: Option<f64>,
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
    pub(crate) fn new(writer: SessionWriter, plugins: Option<Arc<dyn SessionPlugins>>) -> Self {
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
        plugins: Option<Arc<dyn SessionPlugins>>,
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
            message: Arc::new(message.clone()),
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

    /// Persist durable events with their captured qualified model and USD cost.
    pub(crate) fn observe(
        &mut self,
        event: &LoopEvent,
        model: &str,
        amount: Option<f64>,
    ) -> Result<(), SessionError> {
        match event {
            LoopEvent::RejectedUsage { usage } => {
                let id = EntryId::new();
                let ts = Utc::now();
                let cost = UsageCost {
                    entry: id,
                    model: model.to_owned(),
                    amount,
                }
                .into_entry(EntryId::new(), ts)
                .map_err(|source| SessionError::Encode {
                    path: self.path().to_path_buf(),
                    source,
                })?;
                self.writer()?
                    .append_many(&[cost, RejectedUsage { usage: *usage }.into_entry(id, ts)])
            }
            LoopEvent::MessageEnd { usage, .. } => {
                self.turn_usage = Some(PendingUsage {
                    usage: *usage,
                    model: model.to_owned(),
                    amount,
                });
                Ok(())
            }
            LoopEvent::MessageAppended { message } => {
                let pending = if message.role == Role::Assistant {
                    self.turn_usage.take()
                } else {
                    None
                };
                let id = EntryId::new();
                let ts = Utc::now();
                let entry = SessionEntry::Message(MessageEntry {
                    id,
                    ts,
                    message: message.clone(),
                    usage: pending.as_ref().map(|pending| pending.usage),
                });
                if let Some(pending) = pending {
                    let cost = UsageCost {
                        entry: id,
                        model: pending.model,
                        amount: pending.amount,
                    }
                    .into_entry(EntryId::new(), ts)
                    .map_err(|source| SessionError::Encode {
                        path: self.path().to_path_buf(),
                        source,
                    })?;
                    self.writer()?.append_many(&[cost, entry])
                } else {
                    self.writer()?.append(&entry)
                }
            }
            LoopEvent::Compaction {
                kept,
                summarized,
                summary,
                before,
                ..
            } => self.writer()?.append(&SessionEntry::Compaction(Compaction {
                id: EntryId::new(),
                ts: Utc::now(),
                kept: *kept,
                summarized: *summarized,
                summary: summary.clone(),
                before: Some(*before),
            })),
            LoopEvent::TurnEnded { .. } => self.write_plugin_ops(),
            _ => Ok(()),
        }
    }

    fn write_plugin_ops(&mut self) -> Result<(), SessionError> {
        let Some(plugins) = &self.plugins else {
            return Ok(());
        };
        // The run thread names its session, so another session's ops stay
        // for that session's recorder.
        let ops = match kage_plugin::Invocation::current().session {
            Some(session) => plugins.take_pending_session_ops_for(session),
            None => plugins.take_pending_session_ops(),
        };
        let sink = plugins.sink();
        for op in ops {
            if let Some(entry) = plugin_op_entry(op, &sink) {
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
pub(crate) fn plugin_op_entry(op: PendingSessionOp, sink: &SharedHostLog) -> Option<SessionEntry> {
    match op {
        PendingSessionOp::AppendCustom { kind, data } => Some(SessionEntry::Custom(Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind,
            data,
        })),
        PendingSessionOp::SetLabel { anchor, text } => {
            let Ok(parsed) = ulid::Ulid::from_string(&anchor) else {
                lock(sink).log(
                    LogLevel::Warn,
                    &format!("set_label: invalid entry id '{anchor}', dropping"),
                );
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
    use kage_plugin::PluginRuntime;
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
            backend: None,
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
            message: Arc::new(Message::new(
                role,
                vec![Content::Text { text: text.into() }],
                None,
            )),
        }
    }

    #[test]
    fn planned_file_appears_with_the_first_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (path, header) = header(dir.path());
        let mut recorder = Recorder::planned(path.clone(), header, None);
        recorder
            .observe(
                &LoopEvent::TurnEnded {
                    index: 0,
                    had_tool_calls: false,
                },
                "mock/m",
                None,
            )
            .unwrap();
        assert!(!path.exists());
        recorder
            .observe(&appended(Role::User, "hi"), "mock/m", None)
            .unwrap();
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
        recorder
            .observe(&appended(Role::User, "q"), "mock/m", None)
            .unwrap();
        recorder
            .observe(
                &LoopEvent::MessageEnd {
                    id: MessageId::new(),
                    usage,
                    stop_reason: StopReason::EndTurn,
                },
                "mock/m",
                None,
            )
            .unwrap();
        recorder
            .observe(&appended(Role::Assistant, "a"), "mock/m", None)
            .unwrap();
        recorder
            .observe(
                &LoopEvent::Compaction {
                    kept: 1,
                    summarized: 2,
                    summary: "s".into(),
                    before: 9_000,
                    estimate: 0,
                },
                "mock/m",
                None,
            )
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
    fn assistant_cost_is_staged_until_append_with_its_original_model_and_cost() {
        let dir = tempfile::tempdir().unwrap();
        let (path, header) = header(dir.path());
        let mut recorder = Recorder::planned(path.clone(), header, None);
        let usage = TokenUsage {
            input: 7,
            output: 3,
            cache_read: 11,
            cache_write: 13,
        };
        recorder
            .observe(
                &LoopEvent::MessageEnd {
                    id: MessageId::new(),
                    usage,
                    stop_reason: StopReason::EndTurn,
                },
                "actual/scoped/model",
                Some(0.00641),
            )
            .unwrap();
        assert!(!path.exists());
        recorder
            .observe(&appended(Role::User, "q"), "other/model", None)
            .unwrap();
        assert_eq!(entries(&path).len(), 2);
        recorder
            .observe(&appended(Role::Assistant, "a"), "other/model", None)
            .unwrap();

        let written = entries(&path);
        assert_eq!(written.len(), 4);
        let SessionEntry::Custom(custom) = &written[2] else {
            panic!("cost must immediately precede its message");
        };
        let snapshot = UsageCost::from_custom(custom).unwrap().unwrap();
        let SessionEntry::Message(message) = &written[3] else {
            panic!("cost must target an accepted message");
        };
        assert_eq!(snapshot.entry, message.id);
        assert_eq!(snapshot.model, "actual/scoped/model");
        assert!((snapshot.amount.unwrap() - 0.00641).abs() < 1e-12);
        assert_eq!(message.usage, Some(usage));
    }

    #[test]
    fn invalid_cost_returns_an_encoding_error_without_creating_a_session() {
        for amount in [f64::NAN, f64::INFINITY, -1.0] {
            for accepted in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let (path, header) = header(dir.path());
                let mut recorder = Recorder::planned(path.clone(), header, None);
                let usage = TokenUsage::default();
                let result = if accepted {
                    recorder
                        .observe(
                            &LoopEvent::MessageEnd {
                                id: MessageId::new(),
                                usage,
                                stop_reason: StopReason::EndTurn,
                            },
                            "mock/m",
                            Some(amount),
                        )
                        .unwrap();
                    recorder.observe(&appended(Role::Assistant, "a"), "mock/m", None)
                } else {
                    recorder.observe(&LoopEvent::RejectedUsage { usage }, "mock/m", Some(amount))
                };
                assert!(matches!(result, Err(SessionError::Encode { .. })));
                assert!(!path.exists());
            }
        }
    }

    #[test]
    fn accepted_usage_batch_still_externalizes_message_media() {
        use base64::Engine as _;
        use kage_core::MediaSource;

        let dir = tempfile::tempdir().unwrap();
        let (path, header) = header(dir.path());
        let mut recorder = Recorder::planned(path.clone(), header, None);
        let bytes = vec![7_u8; kage_session::blobs::INLINE_MAX + 1];
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let message = Arc::new(Message::new(
            Role::Assistant,
            vec![Content::Image {
                source: MediaSource::Base64 { data: data.clone() },
                mime: "image/png".into(),
            }],
            None,
        ));
        recorder
            .observe(
                &LoopEvent::MessageEnd {
                    id: message.id,
                    usage: TokenUsage::default(),
                    stop_reason: StopReason::EndTurn,
                },
                "mock/m",
                None,
            )
            .unwrap();
        recorder
            .observe(&LoopEvent::MessageAppended { message }, "mock/m", None)
            .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(&data));
        let sha256 = kage_session::BlobStore::digest(&bytes);
        assert!(raw.contains(&sha256));
        assert_eq!(
            kage_session::BlobStore::beside(&path).get(&sha256),
            Some(bytes)
        );
        let written = entries(&path);
        assert_eq!(written.len(), 3);
        let SessionEntry::Message(message) = &written[2] else {
            panic!("cost must precede the stored message");
        };
        assert!(matches!(
            &message.message.content[0],
            Content::Image { source: MediaSource::Base64 { data: restored }, .. }
                if restored == &data
        ));
    }

    #[test]
    fn snapshots_distinguish_free_and_unknown_prices_for_every_attempt_kind() {
        for amount in [None, Some(0.0)] {
            let dir = tempfile::tempdir().unwrap();
            let (path, header) = header(dir.path());
            let mut recorder = Recorder::planned(path.clone(), header, None);
            let usage = TokenUsage {
                input: 7,
                output: 3,
                ..TokenUsage::default()
            };
            recorder
                .observe(&LoopEvent::RejectedUsage { usage }, "mock/m", amount)
                .unwrap();
            recorder
                .observe(
                    &LoopEvent::MessageEnd {
                        id: MessageId::new(),
                        usage,
                        stop_reason: StopReason::EndTurn,
                    },
                    "mock/m",
                    amount,
                )
                .unwrap();
            recorder
                .observe(&appended(Role::Assistant, "a"), "mock/m", amount)
                .unwrap();
            let written = entries(&path);
            assert_eq!(written.len(), 5);
            for pair in written[1..].chunks_exact(2) {
                let SessionEntry::Custom(custom) = &pair[0] else {
                    panic!("each attempt must have a cost annotation");
                };
                let snapshot = UsageCost::from_custom(custom).unwrap().unwrap();
                let target = match &pair[1] {
                    SessionEntry::Custom(custom) => {
                        assert!(RejectedUsage::from_custom(custom).unwrap().is_some());
                        custom.id
                    }
                    SessionEntry::Message(message) => message.id,
                    _ => panic!("cost must target usage"),
                };
                assert_eq!(snapshot.entry, target);
                assert_eq!(snapshot.model, "mock/m");
                assert_eq!(snapshot.amount, amount);
            }
            let replay = kage_session::replay(&path).unwrap();
            assert!(replay.accounting.cost.abs() < f64::EPSILON);
            assert_eq!(
                replay.accounting.cost_status,
                if amount.is_some() {
                    kage_core::CostStatus::Recorded
                } else {
                    kage_core::CostStatus::Incomplete
                }
            );
            assert!(replay.accounting.legacy_usage.is_empty());
        }
    }

    #[test]
    fn rejected_usage_is_recorded_without_becoming_assistant_usage() {
        let dir = tempfile::tempdir().unwrap();
        let (path, header) = header(dir.path());
        let mut recorder = Recorder::planned(path.clone(), header, None);
        let usage = TokenUsage {
            input: 7,
            output: 3,
            cache_read: 11,
            cache_write: 13,
        };
        recorder
            .observe(&appended(Role::User, "q"), "mock/m", None)
            .unwrap();
        recorder
            .observe(&LoopEvent::RejectedUsage { usage }, "mock/m", None)
            .unwrap();

        let written = entries(&path);
        assert_eq!(written.len(), 4);
        let SessionEntry::Custom(custom) = &written[3] else {
            panic!("rejected usage must be an accounting entry");
        };
        assert_eq!(
            RejectedUsage::from_custom(custom).unwrap().unwrap().usage,
            usage
        );
        let replay = kage_session::replay(&path).unwrap();
        assert_eq!(replay.history.len(), 1);
        assert_eq!(replay.usage_total.input, usage.input);
        assert_eq!(replay.usage_total.output, usage.output);
        assert_eq!(replay.usage_total.cache_read, usage.cache_read);
        assert_eq!(replay.usage_total.cache_write, usage.cache_write);
        assert_eq!(replay.usage_total.last_context, 0);

        recorder
            .observe(&appended(Role::Assistant, "a"), "mock/m", None)
            .unwrap();
        assert!(matches!(
            entries(&path).last(),
            Some(SessionEntry::Message(message)) if message.usage.is_none()
        ));
    }

    #[test]
    fn a_model_switch_goes_to_the_planned_header_or_an_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (path, header) = header(dir.path());
        let mut recorder = Recorder::planned(path.clone(), header, None);
        recorder.set_model("mock:a").unwrap();
        assert!(!path.exists());
        recorder
            .observe(&appended(Role::User, "hi"), "mock/m", None)
            .unwrap();
        recorder.set_model("mock:b").unwrap();

        let written = entries(&path);
        assert!(matches!(&written[0], SessionEntry::Header(h) if h.model == "mock:a"));
        assert!(matches!(&written[2], SessionEntry::ModelChange(m) if m.model == "mock:b"));
    }

    #[derive(Clone, Default)]
    struct RecordingSink(Arc<std::sync::Mutex<Vec<(kage_plugin::LogLevel, String)>>>);

    impl kage_plugin::HostLog for RecordingSink {
        fn notify(&mut self, message: &str) {
            panic!("unexpected notification: {message}");
        }

        fn log(&mut self, level: kage_plugin::LogLevel, message: &str) {
            kage_core::sync::lock(&self.0).push((level, message.to_owned()));
        }
    }

    #[test]
    fn invalid_plugin_anchor_diagnostics_stay_off_stdio() {
        const CHILD: &str = "KAGE_RECORDER_SINK_REGRESSION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            plugin_session_ops_are_written_at_turn_end();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::recorder::tests::invalid_plugin_anchor_diagnostics_stay_off_stdio",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        for bytes in [&output.stdout, &output.stderr] {
            let marker = b"not-a-ulid";
            assert!(
                !bytes.windows(marker.len()).any(|part| part == marker),
                "diagnostic leaked: {}",
                String::from_utf8_lossy(bytes),
            );
        }
    }

    #[test]
    fn plugin_session_ops_are_written_at_turn_end() {
        let dir = tempfile::tempdir().unwrap();
        let (path, header) = header(dir.path());
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("t".to_owned(), vec!["session_write".to_owned()]);
        let recording = RecordingSink::default();
        let sink: kage_plugin::SharedHostLog =
            Arc::new(std::sync::Mutex::new(Box::new(recording.clone())));
        let runtime = Arc::new(
            PluginRuntime::builder()
                .capabilities(caps)
                .sink(sink)
                .build()
                .unwrap(),
        );
        runtime
            .eval_plugin(
                "t",
                "kage.request_capabilities({'session_write'}); \
                 kage.session.append_entry('plugin:tps', { rate = 12.5 })",
            )
            .unwrap();
        let anchor = EntryId::new();
        runtime
            .eval(&format!("kage.session.set_label('{anchor}', 'milestone')"))
            .unwrap();
        runtime
            .eval("kage.session.set_label('not-a-ulid', 'dropped')")
            .unwrap();
        let mut recorder = Recorder::planned(
            path.clone(),
            header,
            Some(Arc::clone(&runtime) as Arc<dyn SessionPlugins>),
        );
        recorder
            .observe(
                &LoopEvent::TurnEnded {
                    index: 0,
                    had_tool_calls: false,
                },
                "mock/m",
                None,
            )
            .unwrap();

        let written = entries(&path);
        assert_eq!(written.len(), 3);
        assert!(matches!(&written[1], SessionEntry::Custom(c) if c.kind == "plugin:tps"));
        assert!(matches!(&written[2], SessionEntry::Label(l) if l.anchor == anchor));
        assert!(runtime.take_pending_session_ops().is_empty());
        let logs = kage_core::sync::lock(&recording.0);
        assert_eq!(
            *logs,
            [(
                kage_plugin::LogLevel::Warn,
                "set_label: invalid entry id 'not-a-ulid', dropping".to_owned()
            )],
        );
    }
}
