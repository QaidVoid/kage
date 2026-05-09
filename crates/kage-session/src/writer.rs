//! Append-only session writer.
//!
//! [`SessionWriter::create`] starts a new file by writing the header line.
//! [`SessionWriter::open`] reopens an existing file for further appends.
//! Each [`SessionWriter::append`] writes one JSON line and `fsync`s the file
//! before returning, so a successful return implies the entry has reached
//! disk.
//!
//! Crash safety is "newline-only": entries are always terminated by a single
//! `\n`. A process killed mid-append leaves a partial trailing line which the
//! reader detects by failed JSON parse and skips.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::entry::{Header, SessionEntry};
use crate::error::SessionError;

/// Writes one [`SessionEntry`] per line, fsyncing on every append.
#[derive(Debug)]
pub struct SessionWriter {
    path: PathBuf,
    inner: BufWriter<File>,
}

impl SessionWriter {
    /// Create a new session file at `path` and write the header line.
    ///
    /// Fails if the file already exists; callers that want to append to an
    /// existing session should use [`Self::open`].
    pub fn create(path: impl Into<PathBuf>, header: Header) -> Result<Self, SessionError> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| SessionError::Io {
                path: parent.to_path_buf(),
                source: err,
            })?;
        }
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|err| SessionError::Io {
                path: path.clone(),
                source: err,
            })?;
        let mut writer = Self {
            path,
            inner: BufWriter::new(file),
        };
        writer.append(&SessionEntry::Header(header))?;
        Ok(writer)
    }

    /// Reopen an existing session file for further appends.
    ///
    /// The file is opened in append mode so writes always land at the end
    /// regardless of what other process may have written in between. No
    /// validation of the header is performed here; readers detect malformed
    /// files.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|err| SessionError::Io {
                path: path.clone(),
                source: err,
            })?;
        Ok(Self {
            path,
            inner: BufWriter::new(file),
        })
    }

    /// Append one entry: serialize, write `<json>\n`, flush, fsync.
    pub fn append(&mut self, entry: &SessionEntry) -> Result<(), SessionError> {
        let line = serde_json::to_vec(entry).map_err(|err| SessionError::Encode {
            path: self.path.clone(),
            source: err,
        })?;
        self.inner
            .write_all(&line)
            .map_err(|err| self.io_err(err))?;
        self.inner
            .write_all(b"\n")
            .map_err(|err| self.io_err(err))?;
        self.inner.flush().map_err(|err| self.io_err(err))?;
        self.inner
            .get_ref()
            .sync_all()
            .map_err(|err| self.io_err(err))?;
        Ok(())
    }

    /// Path of the file this writer is appending to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn io_err(&self, source: std::io::Error) -> SessionError {
        SessionError::Io {
            path: self.path.clone(),
            source,
        }
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

    fn fresh_header() -> Header {
        Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/tmp"),
            model: "anthropic:claude".into(),
            system_prompt: "sys".into(),
            parent_session: None,
            parent_entry: None,
        }
    }

    #[test]
    fn create_writes_header_then_appends_one_line_per_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let mut w = SessionWriter::create(&path, fresh_header()).unwrap();
        w.append(&SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Message::new(Role::User, vec![Content::Text { text: "hi".into() }], None),
        }))
        .unwrap();
        drop(w);

        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = raw.split_terminator('\n').collect();
        assert_eq!(lines.len(), 2);
        let first: SessionEntry = serde_json::from_str(lines[0]).unwrap();
        assert!(matches!(first, SessionEntry::Header(_)));
        let second: SessionEntry = serde_json::from_str(lines[1]).unwrap();
        assert!(matches!(second, SessionEntry::Message(_)));
    }

    #[test]
    fn create_refuses_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        std::fs::write(&path, b"").unwrap();
        let err = SessionWriter::create(&path, fresh_header()).unwrap_err();
        assert!(matches!(err, SessionError::Io { .. }));
    }

    #[test]
    fn open_appends_after_existing_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let mut w = SessionWriter::create(&path, fresh_header()).unwrap();
        w.append(&SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "first".into(),
            anchor: EntryId::new(),
        }))
        .unwrap();
        drop(w);

        let mut w = SessionWriter::open(&path).unwrap();
        w.append(&SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "second".into(),
            anchor: EntryId::new(),
        }))
        .unwrap();
        drop(w);

        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.split_terminator('\n').count(), 3);
        assert!(raw.contains("\"first\""));
        assert!(raw.contains("\"second\""));
    }

    #[test]
    fn open_fails_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let err = SessionWriter::open(&path).unwrap_err();
        assert!(matches!(err, SessionError::Io { .. }));
    }

    #[test]
    fn each_line_ends_with_newline() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let mut w = SessionWriter::create(&path, fresh_header()).unwrap();
        w.append(&SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "x".into(),
            anchor: EntryId::new(),
        }))
        .unwrap();
        drop(w);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.ends_with('\n'));
    }
}
