//! Replay a session file back into an in-memory history.
//!
//! [`replay`] walks every entry of a session and reconstructs the
//! [`Vec<Message>`] that the loop's `cx.history` would hold immediately
//! after the last recorded entry. Compactions are applied exactly as the
//! loop applied them at write time: the named number of leading messages
//! is dropped and replaced by a synthetic assistant message carrying the
//! recorded summary text.
//!
//! [`find_by_prefix`] and [`find_last`] resolve a session file path from a
//! directory either by id prefix or by most-recent header timestamp.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use kage_core::{Content, Message, MessageId, Role};

use crate::entry::{Header, SessionEntry};
use crate::error::SessionError;
use crate::list::list;
use crate::reader::SessionReader;

/// Result of replaying a session: enough state to seed an `AgentContext`
/// and resume from the last recorded turn.
#[derive(Debug)]
pub struct ReplayResult {
    /// The session's header, carrying provenance and the original model.
    pub header: Header,
    /// Reconstructed conversation history, post-compaction.
    pub history: Vec<Message>,
    /// Active model id at the end of the session, taking any
    /// [`SessionEntry::ModelChange`] entries into account.
    pub model: String,
    /// Tool-call durations recovered from the entry timestamps. Key is
    /// the `ToolCallId` as a string; value is milliseconds from the
    /// call's `MessageEntry.ts` to the matching result's `ts`.
    pub tool_durations: HashMap<String, u64>,
    /// Sum of every persisted [`MessageEntry::usage`] across the
    /// session, post-compaction. Returned as four scalars instead of
    /// a `TokenUsage` to keep this crate's public surface free of
    /// `kage-core` types in result-only positions; the host folds it
    /// into `AgentContext::budget` on resume.
    pub usage_total: ReplayUsage,
}

/// Cumulative token totals replayed from a session file, plus the
/// most recent turn's full prompt size for context-fill display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayUsage {
    /// Sum of `usage.input` across all assistant turns.
    pub input: u64,
    /// Sum of `usage.output` across all assistant turns.
    pub output: u64,
    /// Sum of `usage.cache_read` across all assistant turns.
    pub cache_read: u64,
    /// Sum of `usage.cache_write` across all assistant turns.
    pub cache_write: u64,
    /// Sum of `input + output + cache_read + cache_write` of the
    /// last assistant turn that recorded usage. Compared to the
    /// model's context window for the modeline percentage on
    /// resume; `0` until a turn with usage is found.
    pub last_context: u64,
}

/// Replay every entry of `path`, returning the final history.
pub fn replay(path: &Path) -> Result<ReplayResult, SessionError> {
    let mut reader = SessionReader::iter(path)?;
    let first = reader
        .next()
        .ok_or_else(|| empty_file_error(path))?
        .map_err(|e| match e {
            SessionError::Decode { .. } | SessionError::Io { .. } => e,
            SessionError::Encode { .. } => unreachable!("reader does not produce Encode"),
        })?;
    let SessionEntry::Header(header) = first else {
        return Err(missing_header_error(path));
    };

    let mut model = header.model.clone();
    let mut history: Vec<Message> = Vec::new();
    // `call_starts` tracks the wall-clock time each ToolCall was
    // appended; on a matching ToolResult we compute the elapsed
    // duration so resumed sessions can show real `Took Xms` instead
    // of `Took 0ms` (which the renderer would otherwise compute from
    // back-to-back replay pushes).
    let mut call_starts: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut tool_durations: HashMap<String, u64> = HashMap::new();
    let mut usage_total = ReplayUsage::default();
    for item in reader {
        let entry = item?;
        match entry {
            SessionEntry::Header(_) => {
                return Err(SessionError::Decode {
                    path: path.to_path_buf(),
                    line: 0,
                    source: serde_json::from_str::<SessionEntry>(
                        "{\"err\":\"second header in file\"}",
                    )
                    .unwrap_err(),
                });
            }
            SessionEntry::Message(m) => {
                if let Some(u) = m.usage {
                    usage_total.input = usage_total.input.saturating_add(u.input);
                    usage_total.output = usage_total.output.saturating_add(u.output);
                    usage_total.cache_read = usage_total.cache_read.saturating_add(u.cache_read);
                    usage_total.cache_write = usage_total.cache_write.saturating_add(u.cache_write);
                    usage_total.last_context = u
                        .input
                        .saturating_add(u.output)
                        .saturating_add(u.cache_read)
                        .saturating_add(u.cache_write);
                }
                let ts = m.ts;
                for block in &m.message.content {
                    match block {
                        Content::ToolCall { id, .. } => {
                            call_starts.insert(id.to_string(), ts);
                        }
                        Content::ToolResultBlock { call_id, .. } => {
                            if let Some(start) = call_starts.remove(&call_id.to_string()) {
                                let delta = ts.signed_duration_since(start).num_milliseconds();
                                if delta >= 0 {
                                    tool_durations.insert(
                                        call_id.to_string(),
                                        u64::try_from(delta).unwrap_or(u64::MAX),
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
                history.push(m.message);
            }
            SessionEntry::Compaction(c) => {
                let split = c.summarized.min(history.len());
                history.drain(..split);
                history.insert(
                    0,
                    Message {
                        role: Role::Assistant,
                        content: vec![Content::Text { text: c.summary }],
                        id: MessageId::new(),
                        parent: None,
                        ts: c.ts,
                    },
                );
            }
            SessionEntry::ModelChange(mc) => model = mc.model,
            SessionEntry::ThinkingLevelChange(_)
            | SessionEntry::Label(_)
            | SessionEntry::Custom(_) => {}
        }
    }
    Ok(ReplayResult {
        header,
        history,
        model,
        tool_durations,
        usage_total,
    })
}

fn empty_file_error(path: &Path) -> SessionError {
    SessionError::Decode {
        path: path.to_path_buf(),
        line: 0,
        source: serde_json::from_str::<SessionEntry>("").unwrap_err(),
    }
}

fn missing_header_error(path: &Path) -> SessionError {
    SessionError::Decode {
        path: path.to_path_buf(),
        line: 1,
        source: serde_json::from_str::<SessionEntry>("{\"err\":\"first entry not a header\"}")
            .unwrap_err(),
    }
}

/// Find the session file in `dir` whose id starts with `prefix`. Returns
/// `Ok(None)` if no match exists; an error if the prefix is ambiguous.
pub fn find_by_prefix(dir: &Path, prefix: &str) -> Result<Option<PathBuf>, SessionError> {
    let summaries = list(dir)?;
    let mut matches = summaries
        .into_iter()
        .filter(|s| s.id.to_string().starts_with(prefix));
    let Some(first) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(SessionError::Io {
            path: dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("session id prefix '{prefix}' is ambiguous"),
            ),
        });
    }
    Ok(Some(first.path))
}

/// Find the most recently created session in `dir`.
pub fn find_last(dir: &Path) -> Result<Option<PathBuf>, SessionError> {
    Ok(list(dir)?.into_iter().next().map(|s| s.path))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use kage_core::{Content, Message, Role, ToolCallId};
    use tempfile::tempdir;

    use super::*;
    use crate::entry::{
        Compaction, EntryId, FORMAT_VERSION, Header, MessageEntry, ModelChange, SessionEntry,
        SessionId,
    };
    use crate::writer::SessionWriter;

    fn fresh_header() -> Header {
        Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "be helpful".into(),
            parent_session: None,
            parent_entry: None,
        }
    }

    fn message_entry(role: Role, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Message::new(
                role,
                vec![Content::Text {
                    text: text.to_owned(),
                }],
                None,
            ),
            usage: None,
        })
    }

    fn write(path: &Path, header: Header, entries: &[SessionEntry]) {
        let mut w = SessionWriter::create(path, header).unwrap();
        for e in entries {
            w.append(e).unwrap();
        }
    }

    #[test]
    fn replay_reconstructs_basic_history() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "hello"),
                message_entry(Role::Assistant, "hi there"),
                message_entry(Role::User, "what is 2+2"),
                SessionEntry::Message(MessageEntry {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    message: Message::new(
                        Role::ToolResult,
                        vec![Content::ToolResultBlock {
                            call_id: ToolCallId::new("c1"),
                            output: "4".into(),
                            is_error: false,
                        }],
                        None,
                    ),
                    usage: None,
                }),
                message_entry(Role::Assistant, "4"),
            ],
        );

        let result = replay(&path).unwrap();
        assert_eq!(result.history.len(), 5);
        assert_eq!(result.history[0].role, Role::User);
        assert_eq!(result.history[1].role, Role::Assistant);
        assert_eq!(result.history[3].role, Role::ToolResult);
        assert_eq!(result.model, "anthropic:claude");
    }

    #[test]
    fn replay_applies_compaction() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "old 1"),
                message_entry(Role::Assistant, "old 2"),
                message_entry(Role::User, "old 3"),
                message_entry(Role::Assistant, "old 4"),
                message_entry(Role::User, "kept 1"),
                message_entry(Role::Assistant, "kept 2"),
                SessionEntry::Compaction(Compaction {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    kept: 2,
                    summarized: 4,
                    summary: "[summary of 4 earlier turns]\nthey did stuff".into(),
                }),
                message_entry(Role::User, "post-compact"),
            ],
        );

        let result = replay(&path).unwrap();
        // 1 synthetic + 2 kept + 1 post-compact
        assert_eq!(result.history.len(), 4);
        assert_eq!(result.history[0].role, Role::Assistant);
        match &result.history[0].content[0] {
            Content::Text { text } => assert!(text.contains("they did stuff")),
            other => panic!("expected text, got {other:?}"),
        }
        // Second-to-last kept message comes from the original "kept 1".
        match &result.history[1].content[0] {
            Content::Text { text } => assert_eq!(text, "kept 1"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn replay_applies_model_change() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write(
            &path,
            fresh_header(),
            &[
                message_entry(Role::User, "go"),
                SessionEntry::ModelChange(ModelChange {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    model: "openai:gpt-4o".into(),
                }),
                message_entry(Role::Assistant, "ok"),
            ],
        );
        let result = replay(&path).unwrap();
        assert_eq!(result.model, "openai:gpt-4o");
        assert_eq!(result.history.len(), 2);
    }

    #[test]
    fn find_by_prefix_resolves_unique_match() {
        let dir = tempdir().unwrap();
        let path_a = dir.path().join("01AAAAAAAAAAAAAAAAAAAAAAAA.jsonl");
        let path_b = dir.path().join("01BBBBBBBBBBBBBBBBBBBBBBBB.jsonl");
        let mut header_a = fresh_header();
        let mut header_b = fresh_header();
        header_a.session =
            SessionId(ulid::Ulid::from_string("01AAAAAAAAAAAAAAAAAAAAAAAA").unwrap());
        header_b.session =
            SessionId(ulid::Ulid::from_string("01BBBBBBBBBBBBBBBBBBBBBBBB").unwrap());
        write(&path_a, header_a, &[]);
        write(&path_b, header_b, &[]);

        let found = find_by_prefix(dir.path(), "01A").unwrap().unwrap();
        assert_eq!(found, path_a);
    }

    #[test]
    fn find_by_prefix_errors_on_ambiguity() {
        let dir = tempdir().unwrap();
        let path_a = dir.path().join("01AAAAAAAAAAAAAAAAAAAAAAAA.jsonl");
        let path_b = dir.path().join("01ABBBBBBBBBBBBBBBBBBBBBBB.jsonl");
        let mut header_a = fresh_header();
        let mut header_b = fresh_header();
        header_a.session =
            SessionId(ulid::Ulid::from_string("01AAAAAAAAAAAAAAAAAAAAAAAA").unwrap());
        header_b.session =
            SessionId(ulid::Ulid::from_string("01ABBBBBBBBBBBBBBBBBBBBBBB").unwrap());
        write(&path_a, header_a, &[]);
        write(&path_b, header_b, &[]);

        let err = find_by_prefix(dir.path(), "01A").unwrap_err();
        assert!(matches!(err, SessionError::Io { .. }));
    }

    #[test]
    fn find_last_returns_newest() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("a.jsonl"), fresh_header(), &[]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let path_b = dir.path().join("b.jsonl");
        write(&path_b, fresh_header(), &[]);

        let last = find_last(dir.path()).unwrap().unwrap();
        assert_eq!(last, path_b);
    }
}
