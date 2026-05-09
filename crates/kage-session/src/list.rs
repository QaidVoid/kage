//! Listing recorded sessions in a directory.
//!
//! [`list`] scans a directory for `*.jsonl` files, reads each one's header
//! plus the most recent user prompt, and returns the resulting summaries
//! sorted by creation time (newest first).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::entry::{Header, SessionEntry, SessionId};
use crate::error::SessionError;
use crate::reader::SessionReader;

/// One row in `kage list`. Reflects the persisted state of a session file
/// at the moment of listing; subsequent appends will not be visible until
/// [`list`] is called again.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    /// Session id from the header.
    pub id: SessionId,
    /// Absolute path to the session file.
    pub path: PathBuf,
    /// Header creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Timestamp of the most recently appended entry.
    pub updated_at: DateTime<Utc>,
    /// Working directory recorded in the header.
    pub cwd: PathBuf,
    /// Provider-qualified model from the header.
    pub model: String,
    /// Text of the most recent user message, if any.
    pub last_user_prompt: Option<String>,
    /// Total number of valid entries (including the header).
    pub entry_count: usize,
}

/// Scan `dir` for `*.jsonl` session files and summarize each.
///
/// Files that fail to open or whose first entry is not a header are skipped
/// silently; this lets `kage list` tolerate stray files in the sessions
/// directory without aborting on the first malformed one. Files with a
/// torn trailing line are summarized using everything that did parse.
pub fn list(dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(d) => d,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(SessionError::Io {
                path: dir.to_path_buf(),
                source: err,
            });
        }
    };

    let mut summaries = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|err| SessionError::Io {
            path: dir.to_path_buf(),
            source: err,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(summary) = summarize_one(&path) {
            summaries.push(summary);
        }
    }
    summaries.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    Ok(summaries)
}

fn summarize_one(path: &Path) -> Option<SessionSummary> {
    let mut reader = SessionReader::iter(path).ok()?;
    let first = reader.next()?.ok()?;
    let SessionEntry::Header(header) = first else {
        return None;
    };
    let mut updated_at = header.ts;
    let mut last_user_prompt = None;
    let mut entry_count = 1;
    for item in reader {
        let Ok(entry) = item else { continue };
        entry_count += 1;
        updated_at = entry.ts();
        if let SessionEntry::Message(m) = &entry
            && m.message.role == kage_core::Role::User
        {
            last_user_prompt = first_text(&m.message);
        }
    }
    Some(summary_from_header(
        header,
        path.to_path_buf(),
        updated_at,
        last_user_prompt,
        entry_count,
    ))
}

fn first_text(message: &kage_core::Message) -> Option<String> {
    for block in &message.content {
        if let kage_core::Content::Text { text } = block {
            return Some(text.clone());
        }
    }
    None
}

fn summary_from_header(
    header: Header,
    path: PathBuf,
    updated_at: DateTime<Utc>,
    last_user_prompt: Option<String>,
    entry_count: usize,
) -> SessionSummary {
    SessionSummary {
        id: header.session,
        path,
        created_at: header.ts,
        updated_at,
        cwd: header.cwd,
        model: header.model,
        last_user_prompt,
        entry_count,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use kage_core::{Content, Message, Role};
    use tempfile::tempdir;

    use super::*;
    use crate::entry::{
        EntryId, FORMAT_VERSION, Header, Label, MessageEntry, SessionEntry, SessionId,
    };
    use crate::writer::SessionWriter;

    fn write_session(dir: &Path, name: &str, prompt: &str) -> PathBuf {
        let path = dir.join(name);
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "be helpful".into(),
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
                        text: prompt.to_owned(),
                    }],
                    None,
                ),
            }))
            .unwrap();
        writer
            .append(&SessionEntry::Label(Label {
                id: EntryId::new(),
                ts: Utc::now(),
                text: "tail".into(),
                anchor: EntryId::new(),
            }))
            .unwrap();
        path
    }

    #[test]
    fn returns_empty_for_missing_dir() {
        let summaries = list(Path::new("/nonexistent/path/here")).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn returns_empty_for_empty_dir() {
        let dir = tempdir().unwrap();
        let summaries = list(dir.path()).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn summarizes_valid_sessions() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "a.jsonl", "ask one");
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_session(dir.path(), "b.jsonl", "ask two");

        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries.len(), 2);
        // Newest first.
        assert!(summaries[0].created_at >= summaries[1].created_at);
        assert!(
            summaries
                .iter()
                .any(|s| s.last_user_prompt.as_deref() == Some("ask one"))
        );
        assert!(
            summaries
                .iter()
                .any(|s| s.last_user_prompt.as_deref() == Some("ask two"))
        );
        for s in &summaries {
            assert_eq!(s.entry_count, 3);
            assert!(s.updated_at >= s.created_at);
        }
    }

    #[test]
    fn ignores_non_jsonl_files() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "real.jsonl", "hi");
        std::fs::write(dir.path().join("notes.txt"), b"random").unwrap();
        std::fs::write(dir.path().join("README"), b"random").unwrap();

        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
    }

    #[test]
    fn skips_files_without_header_first() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("garbage.jsonl"), b"{\"oops\":true}\n").unwrap();
        write_session(dir.path(), "good.jsonl", "ok");

        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].last_user_prompt.as_deref(), Some("ok"));
    }
}
