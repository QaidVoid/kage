//! Streaming session reader.
//!
//! [`SessionReader::iter`] opens a session file and yields one
//! [`SessionEntry`] per line that is not empty or whitespace-only.
//! Non-final corrupt lines are reported and iteration continues. An
//! unterminated final fragment is forgiven only if it is incomplete.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek};
use std::path::{Path, PathBuf};

use std::sync::Arc;

use crate::blobs::BlobStore;
use crate::entry::{MessageEntry, SessionEntry};
use crate::error::SessionError;

/// Streaming reader over a session file.
#[derive(Debug)]
pub struct SessionReader {
    path: PathBuf,
    inner: BufReader<File>,
    done: bool,
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
        Self::from_file(path, file)
    }

    /// Prepare to stream entries from an already open file.
    pub(crate) fn from_file(path: PathBuf, mut file: File) -> Result<Self, SessionError> {
        file.rewind().map_err(|source| SessionError::Io {
            path: path.clone(),
            source,
        })?;
        let inner = BufReader::new(file);
        Ok(Self {
            path,
            inner,
            done: false,
            line_no: 0,
            torn_trailing: false,
        })
    }

    /// True if iteration ended on an incomplete unterminated fragment.
    #[must_use]
    pub fn torn_trailing(&self) -> bool {
        self.torn_trailing
    }

    /// Path of the file being read.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `entry` with the media its message keeps in the blob store put
    /// back inline.
    fn hydrated(&self, entry: SessionEntry) -> SessionEntry {
        let SessionEntry::Message(message) = &entry else {
            return entry;
        };
        match BlobStore::beside(&self.path).hydrate(&message.message) {
            Some(restored) => SessionEntry::Message(MessageEntry {
                message: Arc::new(restored),
                ..message.clone()
            }),
            None => entry,
        }
    }
}

impl Iterator for SessionReader {
    type Item = Result<SessionEntry, SessionError>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            let mut bytes = Vec::new();
            match self.inner.read_until(b'\n', &mut bytes) {
                Ok(0) => {
                    self.done = true;
                    return None;
                }
                Ok(_) => {}
                Err(source) => {
                    self.done = true;
                    return Some(Err(SessionError::Io {
                        path: self.path.clone(),
                        source,
                    }));
                }
            }
            self.line_no += 1;
            let terminated = bytes.last() == Some(&b'\n');
            match classify_line(&bytes, terminated) {
                Line::Blank => {}
                Line::Entry(entry) => return Some(Ok(self.hydrated(entry))),
                Line::Torn => {
                    self.done = true;
                    self.torn_trailing = true;
                    return None;
                }
                Line::Decode(source) => {
                    return Some(Err(SessionError::Decode {
                        path: self.path.clone(),
                        line: self.line_no,
                        source,
                    }));
                }
                Line::InvalidUtf8(source) => {
                    return Some(Err(SessionError::Io {
                        path: self.path.clone(),
                        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
                    }));
                }
            }
        }
        None
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "avoid an allocation for every session entry"
)]
pub(crate) enum Line {
    Blank,
    Entry(SessionEntry),
    Torn,
    Decode(serde_json::Error),
    InvalidUtf8(std::str::Utf8Error),
}

pub(crate) fn classify_line(bytes: &[u8], terminated: bool) -> Line {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            if !terminated && err.error_len().is_none() {
                let mut prefix = String::from_utf8(bytes[..err.valid_up_to()].to_vec())
                    .expect("valid UTF-8 prefix");
                prefix.push('x');
                if matches!(serde_json::from_str::<serde_json::Value>(&prefix), Err(ref e) if e.is_eof())
                {
                    return Line::Torn;
                }
            }
            return Line::InvalidUtf8(err);
        }
    };
    if text.trim_ascii().is_empty() {
        return Line::Blank;
    }
    match serde_json::from_str::<SessionEntry>(text) {
        Ok(entry) => Line::Entry(entry),
        Err(_err)
            if !terminated
                && matches!(serde_json::from_str::<serde_json::Value>(text), Err(ref e) if e.is_eof()) =>
        {
            Line::Torn
        }
        Err(err) => Line::Decode(err),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

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
            backend: None,
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
            message: Arc::new(Message::new(
                Role::User,
                vec![Content::Text { text: "hi".into() }],
                None,
            )),
            usage: None,
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
    fn terminated_corrupt_trailing_line_is_a_decode_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let mut w = SessionWriter::create(&path, fresh_header()).unwrap();
        w.append(&label("kept")).unwrap();
        drop(w);
        // A corrupt line that does end with `\n` is not a torn write.
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(b"{\"corrupt\": true}\n");
        std::fs::write(&path, raw).unwrap();

        let mut reader = SessionReader::iter(&path).unwrap();
        assert!(matches!(reader.next(), Some(Ok(SessionEntry::Header(_)))));
        assert!(matches!(reader.next(), Some(Ok(SessionEntry::Label(_)))));
        let err = reader.next().unwrap().unwrap_err();
        assert!(
            matches!(err, SessionError::Decode { line: 3, .. }),
            "{err:?}"
        );
        assert!(reader.next().is_none());
        assert!(!reader.torn_trailing());
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
    fn whitespace_only_line_is_skipped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = serde_json::to_string(&SessionEntry::Header(fresh_header())).unwrap();
        let valid = serde_json::to_string(&label("ok")).unwrap();
        let raw = format!("{header}\n   \n{valid}\n");
        std::fs::write(&path, raw).unwrap();

        let reader = SessionReader::iter(&path).unwrap();
        let entries: Vec<_> = reader.collect::<Result<_, _>>().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0], SessionEntry::Header(_)));
        assert!(matches!(entries[1], SessionEntry::Label(_)));
    }

    #[test]
    fn open_missing_file_errors() {
        let err = SessionReader::iter("/nonexistent/path/here.jsonl").unwrap_err();
        assert!(matches!(err, SessionError::Io { .. }));
    }
}
