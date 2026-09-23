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
use std::io::{BufRead, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

use crate::entry::{FORMAT_VERSION, Header, SessionEntry};
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
        let mut opts = OpenOptions::new();
        opts.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Session logs carry full conversations; keep them owner-only.
            opts.mode(0o600);
        }
        let file = opts.open(&path).map_err(|err| SessionError::Io {
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
    /// regardless of what other process may have written in between. A torn
    /// trailing line left by a crashed appender is truncated away first:
    /// appending after it would glue the next entry onto the fragment and
    /// turn a skippable partial write into a permanent decode error. No
    /// validation of the header is performed here; readers detect malformed
    /// files.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|err| SessionError::Io {
                path: path.clone(),
                source: err,
            })?;
        check_version(&file, &path)?;
        repair_torn_tail(&mut file).map_err(|err| SessionError::Io {
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

/// Refuse to append to a file whose header declares a format version this
/// build does not understand. A first line that is not a header is left for
/// readers to report, matching the previous behavior.
fn check_version(file: &File, path: &Path) -> Result<(), SessionError> {
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .map_err(|err| SessionError::Io {
            path: path.to_path_buf(),
            source: err,
        })?;
    if read == 0 {
        return Ok(());
    }
    if let Ok(SessionEntry::Header(header)) = serde_json::from_str::<SessionEntry>(&line)
        && header.version != FORMAT_VERSION
    {
        return Err(SessionError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: header.version,
            supported: FORMAT_VERSION,
        });
    }
    Ok(())
}

/// Drop an unterminated trailing line from `file`.
///
/// Scans for the final `\n`; if the file does not end with one, the bytes
/// after it are a torn write from a crashed appender and are truncated
/// away. The reader would have skipped those bytes anyway, so truncation
/// changes nothing it could see — it only keeps the next append from being
/// glued onto the fragment.
fn repair_torn_tail(file: &mut File) -> std::io::Result<()> {
    file.rewind()?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    let mut last_newline: Option<u64> = None;
    let mut offset = 0u64;
    let mut reader = std::io::BufReader::new(&mut *file);
    loop {
        let mut chunk = Vec::new();
        let n = reader.read_until(b'\n', &mut chunk)?;
        if n == 0 {
            break;
        }
        if chunk.last() == Some(&b'\n') {
            last_newline = Some(offset + u64::try_from(n).unwrap_or(u64::MAX) - 1);
        }
        offset += u64::try_from(n).unwrap_or(u64::MAX);
    }
    let ends_with_newline = last_newline.is_some_and(|pos| pos + 1 == offset);
    if ends_with_newline {
        return Ok(());
    }
    file.set_len(last_newline.map_or(0, |pos| pos + 1))?;
    file.sync_all()
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
            usage: None,
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

    #[cfg(unix)]
    #[test]
    fn created_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        SessionWriter::create(&path, fresh_header()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "group/other must have no access");
        assert_ne!(mode & 0o200, 0, "owner must be able to write");
    }

    #[test]
    fn open_rejects_unsupported_format_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = Header {
            version: FORMAT_VERSION + 1,
            ..fresh_header()
        };
        SessionWriter::create(&path, header).unwrap();
        match SessionWriter::open(&path) {
            Err(SessionError::UnsupportedVersion {
                found, supported, ..
            }) => {
                assert_eq!(found, FORMAT_VERSION + 1);
                assert_eq!(supported, FORMAT_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn open_repairs_torn_tail_before_appending() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let mut w = SessionWriter::create(&path, fresh_header()).unwrap();
        w.append(&SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "kept".into(),
            anchor: EntryId::new(),
        }))
        .unwrap();
        drop(w);
        // Simulate a crashed appender: a partial line with no terminator.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"type\":\"label\",\"id\":\"01").unwrap();
        drop(f);

        let mut w = SessionWriter::open(&path).unwrap();
        w.append(&SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "after".into(),
            anchor: EntryId::new(),
        }))
        .unwrap();
        drop(w);

        // Without repair the new entry glues onto the fragment and the
        // merged line fails to parse as an interior line.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("01{"), "torn fragment was not truncated");
        let entries: Vec<_> = crate::reader::SessionReader::iter(&path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[2], SessionEntry::Label(_)));
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
