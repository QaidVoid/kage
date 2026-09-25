//! Session file operations: fork, clone, export, and the engine commands
//! that replace or copy a session.

use std::path::{Path, PathBuf};

use kage_core::protocol::{HostEvent, NoticeLevel};
use kage_core::{Content, Role, SessionId};
use kage_session::{EntryId, SessionReader, SessionWriter};

/// Read the JSONL session at `path` and return the id of its final
/// non-header entry, or `None` if the file holds only a header.
fn find_last_entry(path: &std::path::Path) -> Result<Option<EntryId>, kage_session::SessionError> {
    let reader = SessionReader::iter(path)?;
    let mut last = None;
    for item in reader {
        let entry = item?;
        if !matches!(entry, kage_session::SessionEntry::Header(_)) {
            last = Some(entry.id());
        }
    }
    Ok(last)
}

/// Copy `src` up through entry `at` (an id prefix, or the latest entry)
/// into a new session file next to it. Returns the new path and id.
pub(super) fn fork_session(src: &Path, at: Option<&str>) -> Result<(PathBuf, SessionId), String> {
    if !src.exists() {
        return Err("the session has no committed entries yet".to_owned());
    }
    let entry: EntryId = match at {
        Some(prefix) => {
            kage_session::resolve_entry_prefix(src, prefix).map_err(|e| e.to_string())?
        }
        None => find_last_entry(src)
            .map_err(|e| e.to_string())?
            .ok_or("the session has no entries to fork at")?,
    };
    let dir = src.parent().ok_or("session path has no parent directory")?;
    let id = SessionId::new();
    let dst = dir.join(format!("{id}.jsonl"));
    kage_session::fork(src, &dst, id, entry).map_err(|e| e.to_string())?;
    Ok((dst, id))
}

/// Write `src` as a Markdown transcript to `dest`, or to
/// `<short-id>.md` in the working directory. Returns the written path.
pub(super) fn export_session(src: &Path, dest: Option<PathBuf>) -> Result<PathBuf, String> {
    if !src.exists() {
        return Err("the session has no committed entries yet".to_owned());
    }
    let replay = kage_session::replay(src).map_err(|e| e.to_string())?;
    let out = dest.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(format!("{}.md", short_id(replay.header.session)))
    });
    std::fs::write(&out, render_session_markdown(&replay))
        .map_err(|e| format!("write {}: {e}", out.display()))?;
    Ok(out)
}

/// First eight characters of a session id, for notices.
pub(super) fn short_id(id: SessionId) -> String {
    id.to_string().chars().take(8).collect()
}

/// Render a replayed session as a Markdown transcript. Plain text and
/// fenced code only, no HTML, so the file stays greppable and
/// ASCII-clean.
pub(crate) fn render_session_markdown(replay: &kage_session::ReplayResult) -> String {
    use std::fmt::Write as _;
    let short: String = replay.header.session.to_string().chars().take(8).collect();
    let mut md = String::new();
    let _ = writeln!(md, "# kage session {short}");
    let _ = writeln!(md);
    let _ = writeln!(md, "- model: `{}`", replay.model);
    let _ = writeln!(md, "- created: {}", replay.header.ts.to_rfc3339());
    let _ = writeln!(md, "- cwd: `{}`", replay.header.cwd.display());
    let _ = writeln!(md);
    for msg in &replay.history {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::ToolResult => "Tool",
            Role::System => "System",
        };
        let _ = writeln!(md, "## {role}");
        let _ = writeln!(md);
        for block in &msg.content {
            match block {
                Content::Text { text } => {
                    let _ = writeln!(md, "{text}");
                    let _ = writeln!(md);
                }
                Content::Thinking { text } => {
                    let _ = writeln!(md, "**thinking**");
                    let _ = writeln!(md);
                    for line in text.lines() {
                        let _ = writeln!(md, "> {line}");
                    }
                    let _ = writeln!(md);
                }
                Content::ToolCall { name, input, .. } => {
                    let pretty =
                        serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
                    let _ = writeln!(md, "**tool call: `{name}`**");
                    let _ = writeln!(md);
                    let _ = writeln!(md, "```json");
                    let _ = writeln!(md, "{pretty}");
                    let _ = writeln!(md, "```");
                    let _ = writeln!(md);
                }
                Content::ToolResultBlock {
                    output, is_error, ..
                } => {
                    let label = if *is_error {
                        "tool result (error)"
                    } else {
                        "tool result"
                    };
                    let _ = writeln!(md, "**{label}**");
                    let _ = writeln!(md);
                    let _ = writeln!(md, "```");
                    let _ = writeln!(md, "{output}");
                    let _ = writeln!(md, "```");
                    let _ = writeln!(md);
                }
                Content::Image { mime, .. } => {
                    let _ = writeln!(md, "_[image omitted: {mime}]_");
                    let _ = writeln!(md);
                }
                Content::Custom { kind, data } => {
                    let pretty =
                        serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
                    let _ = writeln!(md, "_[custom block: {kind}]_");
                    let _ = writeln!(md);
                    let _ = writeln!(md, "```json");
                    let _ = writeln!(md, "{pretty}");
                    let _ = writeln!(md, "```");
                    let _ = writeln!(md);
                }
            }
        }
    }
    md
}

impl super::Dispatcher {
    /// `true` when `id` has no run in flight; otherwise tells the user.
    pub(super) fn ensure_idle(&self, id: SessionId, what: &str) -> bool {
        if self.sessions.get(&id).is_some_and(|s| s.idle.is_some()) {
            return true;
        }
        super::notice(
            &self.bus,
            id,
            NoticeLevel::Warning,
            format!("{what}: wait for the current run to finish or cancel it"),
        );
        false
    }

    pub(super) fn new_session(&mut self, id: SessionId) {
        if !self.ensure_idle(id, "new session") {
            return;
        }
        let session = &self.sessions[&id];
        let Some(idle) = session.idle.as_ref() else {
            return;
        };
        let (path, mut header) =
            match crate::plan_session(&session.state.model, &idle.cx.system_prompt) {
                Ok(planned) => planned,
                Err(err) => {
                    self.error(id, format!("new session: {err}"));
                    return;
                }
            };
        header.cwd.clone_from(&session.workdir);
        let new_id = header.session;
        let mut cx = idle.cx.clone();
        cx.history.clear();
        cx.budget = kage_loop::TokenBudget::default();
        let recorder = super::Recorder::planned(path.clone(), header, session.plugins.clone());
        self.reseat(
            id,
            new_id,
            cx,
            recorder,
            path,
            format!("new session: {}", short_id(new_id)),
        );
    }

    pub(super) fn load_session(&mut self, id: SessionId, path: &Path) {
        if !self.ensure_idle(id, "resume") {
            return;
        }
        if self.sessions[&id].path.as_deref() == Some(path) {
            self.info(id, "that session is already open".to_owned());
            return;
        }
        let replay = match kage_session::replay(path) {
            Ok(replay) => replay,
            Err(err) => {
                self.error(id, format!("resume {}: {err}", path.display()));
                return;
            }
        };
        let writer = match SessionWriter::open(path) {
            Ok(writer) => writer,
            Err(err) => {
                self.error(id, format!("resume {}: {err}", path.display()));
                return;
            }
        };
        let session = self.sessions.get_mut(&id).expect("session checked");
        let Some(idle) = session.idle.as_ref() else {
            return;
        };
        let mut cx = idle.cx.clone();
        let mut fallback = None;
        if self.registry.resolve(&replay.model).is_ok() {
            session.state.model.clone_from(&replay.model);
        } else {
            fallback = Some(format!(
                "session model {} unavailable; using {} instead",
                replay.model, session.state.model
            ));
        }
        cx.history = replay.history;
        cx.budget = kage_loop::TokenBudget {
            used_input: replay.usage_total.input,
            used_output: replay.usage_total.output,
            used_cache_read: replay.usage_total.cache_read,
            used_cache_write: replay.usage_total.cache_write,
            current_context: replay.usage_total.last_context,
        };
        cx.thinking_level = replay
            .thinking_level
            .as_deref()
            .and_then(kage_core::ThinkingLevel::parse);
        let new_id = replay.header.session;
        let message = format!(
            "resumed session {} on {}",
            short_id(new_id),
            session.state.model
        );
        let recorder = super::Recorder::new(writer, session.plugins.clone());
        self.reseat(id, new_id, cx, recorder, path.to_path_buf(), message);
        if let Some(note) = fallback {
            self.info(new_id, note);
        }
    }

    pub(super) fn clone_session(&mut self, id: SessionId) {
        if !self.ensure_idle(id, "clone") {
            return;
        }
        let Some(src) = self.sessions[&id].path.clone() else {
            self.error(id, "clone: the session is not recorded".to_owned());
            return;
        };
        let (dst, new_id) = match fork_session(&src, None) {
            Ok(forked) => forked,
            Err(err) => {
                self.error(id, format!("clone: {err}"));
                return;
            }
        };
        let writer = match SessionWriter::open(&dst) {
            Ok(writer) => writer,
            Err(err) => {
                self.error(id, format!("clone: {err}"));
                return;
            }
        };
        let session = &self.sessions[&id];
        let Some(idle) = session.idle.as_ref() else {
            return;
        };
        let cx = idle.cx.clone();
        let recorder = super::Recorder::new(writer, session.plugins.clone());
        self.reseat(
            id,
            new_id,
            cx,
            recorder,
            dst,
            format!("cloned session: {}", short_id(new_id)),
        );
    }

    pub(super) fn fork(&mut self, id: SessionId, at: Option<&str>, switch: bool) {
        let Some(src) = self.sessions[&id].path.clone() else {
            self.error(id, "fork: the session is not recorded".to_owned());
            return;
        };
        match fork_session(&src, at) {
            Ok((dst, _)) if switch => self.load_session(id, &dst),
            Ok((_, new_id)) => self.info(id, format!("forked session: {}", short_id(new_id))),
            Err(err) => self.error(id, format!("fork: {err}")),
        }
    }

    pub(super) fn fork_file(&self, id: SessionId, path: &Path) {
        match fork_session(path, None) {
            Ok((_, new_id)) => self.info(id, format!("forked session: {}", short_id(new_id))),
            Err(err) => self.error(id, format!("fork: {err}")),
        }
    }

    pub(super) fn delete_session(&self, id: SessionId, path: &Path) {
        if self
            .sessions
            .values()
            .any(|s| s.path.as_deref() == Some(path))
        {
            self.info(id, "cannot delete an open session".to_owned());
            return;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {
                let short: String = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.chars().take(8).collect())
                    .unwrap_or_default();
                self.info(id, format!("deleted session: {short}"));
            }
            Err(err) => self.error(id, format!("delete failed: {err}")),
        }
    }

    pub(super) fn export(&self, id: SessionId, dest: Option<PathBuf>) {
        let Some(src) = self.sessions[&id].path.clone() else {
            self.error(id, "export: the session is not recorded".to_owned());
            return;
        };
        match export_session(&src, dest) {
            Ok(out) => self.info(id, format!("exported to {}", out.display())),
            Err(err) => self.error(id, format!("export: {err}")),
        }
    }

    /// Replace session `old` with a session `new` recorded at `path`,
    /// keeping its tools, plugins and settings but not its permission
    /// mode or approvals, and tell clients.
    fn reseat(
        &mut self,
        old: SessionId,
        new: SessionId,
        cx: kage_loop::AgentContext,
        recorder: super::Recorder,
        path: PathBuf,
        message: String,
    ) {
        let mut session = self.sessions.remove(&old).expect("session checked");
        let messages = cx.history.clone();
        session.usage = super::usage_of(&cx);
        session.state.thinking = cx.thinking_level.unwrap_or_default();
        session.thinking = None;
        session.title_pending = session.title && !super::has_reply(&cx);
        session.pending_history.clear();
        session.queued.clear();
        session.gate.reset_session();
        session.state.permission_mode = None;
        session.path = Some(path.clone());
        session.idle = Some(super::Idle {
            cx,
            recorder: Some(recorder),
        });
        let state = session.state.clone();
        let usage = session.usage;
        self.sessions.insert(new, session);
        if self.active == Some(old) {
            self.active = Some(new);
        }
        self.bus.publish(
            new,
            HostEvent::SessionChanged {
                path,
                title: None,
                messages,
            },
        );
        self.bus.publish(new, HostEvent::StateChanged { state });
        self.bus.publish(new, HostEvent::UsageUpdated { usage });
        self.info(new, message);
    }

    fn info(&self, id: SessionId, text: String) {
        self.bus.publish(
            id,
            HostEvent::Notice {
                level: NoticeLevel::Info,
                text,
                transient: true,
            },
        );
    }

    fn error(&self, id: SessionId, text: String) {
        super::notice(&self.bus, id, NoticeLevel::Error, text);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use kage_core::Message;
    use kage_session::{FORMAT_VERSION, Header, MessageEntry, SessionEntry};

    use super::*;

    fn write_session(dir: &Path) -> (PathBuf, SessionId) {
        let id = SessionId::new();
        let path = dir.join(format!("{id}.jsonl"));
        let header = Header {
            version: FORMAT_VERSION,
            session: id,
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: String::new(),
            parent_session: None,
            parent_entry: None,
        };
        let mut writer = SessionWriter::create(&path, header).unwrap();
        writer
            .append(&SessionEntry::Message(MessageEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                message: Message::new(
                    Role::User,
                    vec![Content::Text {
                        text: "hello".to_owned(),
                    }],
                    None,
                ),
                usage: None,
            }))
            .unwrap();
        (path, id)
    }

    #[test]
    fn export_writes_markdown_to_the_given_path() {
        let dir = tempfile::tempdir().unwrap();
        let (src, _) = write_session(dir.path());
        let out = dir.path().join("out.md");
        assert_eq!(export_session(&src, Some(out.clone())).unwrap(), out);
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(body.contains("# kage session "));
        assert!(body.contains("## User"));
        assert!(body.contains("hello"));
    }

    #[test]
    fn fork_creates_a_parent_linked_session() {
        let dir = tempfile::tempdir().unwrap();
        let (src, src_id) = write_session(dir.path());
        let (forked, _) = fork_session(&src, None).unwrap();
        let mut reader = SessionReader::iter(&forked).unwrap();
        match reader.next().unwrap().unwrap() {
            SessionEntry::Header(h) => assert_eq!(h.parent_session, Some(src_id)),
            other => panic!("expected header, got {other:?}"),
        }
    }
}
