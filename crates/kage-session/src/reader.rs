//! Streaming session reader.
//!
//! [`SessionReader::iter`] opens a session file and yields one
//! [`SessionEntry`] per non-empty line. A non-final line that fails to parse
//! is yielded as `Err` and iteration continues; the trailing line is given
//! the benefit of the doubt: if it fails to parse, it is treated as a torn
//! write from a crashed appender, iteration ends silently, and
//! [`SessionReader::torn_trailing`] returns true.

use std::fs::File;
use std::io::{BufRead, BufReader, Lines};
use std::path::{Path, PathBuf};

use crate::entry::SessionEntry;
use crate::error::SessionError;

/// Streaming reader over a session file.
#[derive(Debug)]
pub struct SessionReader {
    path: PathBuf,
    inner: Lines<BufReader<File>>,
    next_line: Option<std::io::Result<String>>,
    line_no: usize,
    torn_trailing: bool,
}

impl SessionReader {
    /// Open a session file and prepare to stream entries.
    pub fn iter(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let file = File::open(&path).map_err(|err| SessionError::Io {
            path: path.clone(),
            source: err,
        })?;
        let mut inner = BufReader::new(file).lines();
        let next_line = inner.next();
        Ok(Self {
            path,
            inner,
            next_line,
            line_no: 0,
            torn_trailing: false,
        })
    }

    /// True if iteration ended on a trailing line that failed to parse.
    ///
    /// Indicates a crashed appender: the last write made it to disk only
    /// partially. The session up to that point is still valid.
    #[must_use]
    pub fn torn_trailing(&self) -> bool {
        self.torn_trailing
    }

    /// Path of the file being read.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Iterator for SessionReader {
    type Item = Result<SessionEntry, SessionError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let current = self.next_line.take()?;
            self.line_no += 1;
            let line_no = self.line_no;
            self.next_line = self.inner.next();
            let is_trailing = self.next_line.is_none();

            let line = match current {
                Ok(s) => s,
                Err(err) => {
                    return Some(Err(SessionError::Io {
                        path: self.path.clone(),
                        source: err,
                    }));
                }
            };

            if line.is_empty() {
                if is_trailing {
                    return None;
                }
                continue;
            }

            return match serde_json::from_str::<SessionEntry>(&line) {
                Ok(entry) => Some(Ok(entry)),
                Err(err) => {
                    if is_trailing {
                        self.torn_trailing = true;
                        None
                    } else {
                        Some(Err(SessionError::Decode {
                            path: self.path.clone(),
                            line: line_no,
                            source: err,
                        }))
                    }
                }
            };
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
    use crate::writer::SessionWriter;

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

    fn label(text: &str) -> SessionEntry {
        SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: text.into(),
            anchor: EntryId::new(),
        })
    }

    #[test]
    fn round_trips_writer_to_reader() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let mut w = SessionWriter::create(&path, fresh_header()).unwrap();
        w.append(&label("a")).unwrap();
        w.append(&SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Message::new(Role::User, vec![Content::Text { text: "hi".into() }], None),
        }))
        .unwrap();
        w.append(&label("b")).unwrap();
        drop(w);

        let reader = SessionReader::iter(&path).unwrap();
        let entries: Vec<_> = reader.collect::<Result<_, _>>().unwrap();
        assert_eq!(entries.len(), 4);
        assert!(matches!(entries[0], SessionEntry::Header(_)));
        assert!(matches!(entries[1], SessionEntry::Label(_)));
        assert!(matches!(entries[2], SessionEntry::Message(_)));
        assert!(matches!(entries[3], SessionEntry::Label(_)));
    }

    #[test]
    fn torn_trailing_line_is_silently_dropped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let mut w = SessionWriter::create(&path, fresh_header()).unwrap();
        w.append(&label("kept")).unwrap();
        drop(w);
        // Simulate a crashed appender: append half a line with no terminator.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(b"{\"type\":\"label\",\"id\":\"01");
        std::fs::write(&path, raw).unwrap();

        let mut reader = SessionReader::iter(&path).unwrap();
        let mut count = 0;
        for item in reader.by_ref() {
            item.expect("interior entries must parse");
            count += 1;
        }
        assert_eq!(count, 2);
        assert!(reader.torn_trailing());
    }

    #[test]
    fn interior_decode_failure_is_an_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        // Header line, then an obviously bogus interior line, then a valid line.
        let header = serde_json::to_string(&SessionEntry::Header(fresh_header())).unwrap();
        let valid = serde_json::to_string(&label("ok")).unwrap();
        let bogus = "{\"type\":\"label\",\"id\":\"oops\"}";
        let raw = format!("{header}\n{bogus}\n{valid}\n");
        std::fs::write(&path, raw).unwrap();

        let mut reader = SessionReader::iter(&path).unwrap();
        assert!(matches!(reader.next(), Some(Ok(SessionEntry::Header(_)))));
        let err = reader.next().unwrap().unwrap_err();
        assert!(matches!(err, SessionError::Decode { line: 2, .. }));
        assert!(matches!(reader.next(), Some(Ok(SessionEntry::Label(_)))));
        assert!(reader.next().is_none());
        assert!(!reader.torn_trailing());
    }

    #[test]
    fn empty_interior_line_is_skipped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = serde_json::to_string(&SessionEntry::Header(fresh_header())).unwrap();
        let valid = serde_json::to_string(&label("ok")).unwrap();
        let raw = format!("{header}\n\n{valid}\n");
        std::fs::write(&path, raw).unwrap();

        let reader = SessionReader::iter(&path).unwrap();
        let entries: Vec<_> = reader.collect::<Result<_, _>>().unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn open_missing_file_errors() {
        let err = SessionReader::iter("/nonexistent/path/here.jsonl").unwrap_err();
        assert!(matches!(err, SessionError::Io { .. }));
    }
}
