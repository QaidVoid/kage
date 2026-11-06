//! Append-only session writer.
//!
//! [`SessionWriter::create`] starts a new file by writing the header line.
//! [`SessionWriter::open`] reopens an existing file for further appends.
//! Each [`SessionWriter::append`] writes one JSON line and `fsync`s the file
//! before returning, so a successful return implies the entry has reached
//! disk.
//!
//! On Unix writers hold exclusive advisory locks on both the session inode
//! and a sidecar. The inode lock prevents aliases from bypassing the lock.
//! Windows currently uses only the sidecar, so aliased session paths do not
//! share a lock there. Other unsupported platforms fail closed.
//!
//! Entries written by this writer end with `\n`. On reopening, a valid
//! unterminated final entry is preserved and terminated, while an invalid
//! trailing fragment is truncated only when incomplete. Corrupt or incompatible
//! tails and an invalid header are refused.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::blobs::BlobStore;
use crate::entry::{FORMAT_VERSION, Header, MessageEntry, SessionEntry};
use crate::error::SessionError;
use crate::reader::{Line, classify_line};

/// Writes one [`SessionEntry`] per line, fsyncing on every append.
#[derive(Debug)]
pub struct SessionWriter {
    path: PathBuf,
    inner: BufWriter<File>,
    /// Held for the writer's lifetime; released when the writer drops.
    /// The lock lives on the sidecar file's own handle.
    #[cfg(unix)]
    #[expect(
        dead_code,
        reason = "held only so the lock lives as long as the writer"
    )]
    lock: Option<Lock>,
    /// The sidecar lock file's handle; the lock is released when the
    /// handle closes.
    #[cfg(windows)]
    #[expect(
        dead_code,
        reason = "held only so the lock lives as long as the writer"
    )]
    lock: Option<File>,
}

impl SessionWriter {
    /// Create a new session file at `path` and write the header line.
    ///
    /// Fails if the file already exists; callers that want to append to an
    /// existing session should use [`Self::open`]. For an external backend,
    /// canonicalizes the working directory before creating any destination
    /// paths. The directory must exist and be accessible. Versions outside
    /// the supported range are rejected before creating destination paths.
    /// Supported headers are written with the current format version because
    /// appended entries may use the current format. This also prevents older
    /// readers from treating external sessions as native sessions.
    pub fn create(path: impl Into<PathBuf>, mut header: Header) -> Result<Self, SessionError> {
        let path = path.into();
        if header.version == 0 || header.version > FORMAT_VERSION {
            return Err(SessionError::UnsupportedVersion {
                path,
                found: header.version,
                supported: FORMAT_VERSION,
            });
        }
        header.version = FORMAT_VERSION;
        if header.backend.is_some() {
            header.cwd = std::fs::canonicalize(&header.cwd).map_err(|source| SessionError::Io {
                path: header.cwd.clone(),
                source,
            })?;
        }
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
        let lock = acquire_lock(&path)?;
        let file = opts.open(&path).map_err(|err| SessionError::Io {
            path: path.clone(),
            source: err,
        })?;
        let lock = lock_inode(
            file.try_clone().map_err(|source| SessionError::Io {
                path: path.clone(),
                source,
            })?,
            &path,
            lock,
        )?;
        let mut writer = Self {
            path,
            inner: BufWriter::new(file),
            #[cfg(any(unix, windows))]
            lock,
        };
        writer.append(&SessionEntry::Header(header))?;
        // Entries are fsynced per append, but the file's own directory
        // entry needs a dir fsync or a power cut can take the whole
        // newly created session with it.
        #[cfg(unix)]
        kage_core::fsutil::sync_parent_entry(&writer.path);
        Ok(writer)
    }

    /// Reopen an existing session file for further appends.
    ///
    /// Writes seek to the end while the exclusive lock is held. A valid
    /// unterminated final entry is terminated first, while a torn trailing
    /// line is truncated away. The advisory lock is taken before repair so
    /// a second appender never truncates a concurrent writer's in-flight entry.
    /// An empty file or a file whose first line is not a parseable header is
    /// refused rather than repaired into a headerless file.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let (file, lock) = open_locked(&path, true)?;
        let mut writer = Self {
            path,
            inner: BufWriter::new(file),
            #[cfg(any(unix, windows))]
            lock,
        };
        check_version(writer.inner.get_ref(), &writer.path)?;
        repair_torn_tail(writer.inner.get_mut(), &writer.path)?;
        Ok(writer)
    }

    /// Open and replay a native session under one exclusive lock.
    ///
    /// Returns `None` for an external session without modifying its file.
    /// The returned writer appends to the same open file that was replayed.
    pub fn open_native_replay(
        path: impl Into<PathBuf>,
    ) -> Result<Option<(Self, crate::resume::ReplayResult)>, SessionError> {
        let path = path.into();
        let (mut file, lock) = open_locked(&path, true)?;
        let header = check_version(&file, &path)?;
        if header.backend.is_some() {
            return Ok(None);
        }
        file.rewind().map_err(|source| SessionError::Io {
            path: path.clone(),
            source,
        })?;
        let replay = crate::resume::replay_file(
            &path,
            file.try_clone().map_err(|source| SessionError::Io {
                path: path.clone(),
                source,
            })?,
        )?;
        repair_torn_tail(&mut file, &path)?;
        Ok(Some((
            Self {
                path,
                inner: BufWriter::new(file),
                #[cfg(any(unix, windows))]
                lock,
            },
            replay,
        )))
    }

    /// Append one entry: serialize, write `<json>\n`, flush, fsync. A
    /// message's large media go to the blob store first and the line
    /// names them (see [`crate::blobs`]).
    pub fn append(&mut self, entry: &SessionEntry) -> Result<(), SessionError> {
        self.inner
            .seek(std::io::SeekFrom::End(0))
            .map_err(|err| self.io_err(err))?;
        let stored = match entry {
            SessionEntry::Message(message) => BlobStore::beside(&self.path)
                .externalize(&message.message)
                .map_err(|err| self.io_err(err))?
                .map(|moved| {
                    SessionEntry::Message(MessageEntry {
                        message: Arc::new(moved),
                        ..message.clone()
                    })
                }),
            _ => None,
        };
        let entry = stored.as_ref().unwrap_or(entry);
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

    /// Append entry lines already encoded by another writer, each
    /// ending in `\n`, with one flush and one fsync for all of them.
    /// Copies a session's entries without decoding and re-encoding
    /// them one by one.
    pub fn append_lines(&mut self, lines: &[u8]) -> Result<(), SessionError> {
        self.inner
            .seek(std::io::SeekFrom::End(0))
            .map_err(|err| self.io_err(err))?;
        self.inner
            .write_all(lines)
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

#[cfg(any(unix, windows))]
fn open_locked(path: &Path, writable: bool) -> Result<(File, Option<Lock>), SessionError> {
    let lock = acquire_lock(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(writable)
        .open(path)
        .map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let lock = lock_inode(
        file.try_clone().map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?,
        path,
        lock,
    )?;
    Ok((file, lock))
}

#[cfg(unix)]
#[derive(Debug)]
struct Lock {
    _sidecar: nix::fcntl::Flock<File>,
    _inode: nix::fcntl::Flock<File>,
}

#[cfg(unix)]
fn lock_inode(
    file: File,
    path: &Path,
    sidecar: Option<nix::fcntl::Flock<File>>,
) -> Result<Option<Lock>, SessionError> {
    use nix::fcntl::{Flock, FlockArg};
    let inode = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, err)| {
        if err == nix::errno::Errno::EWOULDBLOCK {
            SessionError::Locked {
                path: path.to_path_buf(),
            }
        } else {
            SessionError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::from_raw_os_error(err as i32),
            }
        }
    })?;
    Ok(Some(Lock {
        _sidecar: sidecar.expect("Unix sidecar lock"),
        _inode: inode,
    }))
}

#[cfg(windows)]
fn lock_inode(
    _file: File,
    _path: &Path,
    sidecar: Option<File>,
) -> Result<Option<Lock>, SessionError> {
    Ok(sidecar)
}

#[cfg(not(any(unix, windows)))]
fn lock_inode(_file: File, path: &Path, _sidesidecar: ()) -> Result<(), SessionError> {
    Err(SessionError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "session locking is unsupported",
        ),
    })
}

#[cfg(windows)]
type Lock = File;

#[cfg(not(any(unix, windows)))]
fn acquire_lock(path: &Path) -> Result<(), SessionError> {
    Err(SessionError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "session locking is unsupported",
        ),
    })
}
#[cfg(not(any(unix, windows)))]
fn open_locked(path: &Path, _writable: bool) -> Result<(File, ()), SessionError> {
    acquire_lock(path)?;
    unreachable!()
}

/// Refuse to append to a file that is not a session this build can
/// extend. An empty file has no header to append behind; a first line
/// that does not parse as a header can only be a torn or foreign
/// line, and appending behind it would orphan everything this writer
/// adds. A parseable header must declare a format version this build
/// interprets: any version whose entries it all knows, so a version 1
/// file stays appendable.
fn check_version(file: &File, path: &Path) -> Result<Header, SessionError> {
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .map_err(|err| SessionError::Io {
            path: path.to_path_buf(),
            source: err,
        })?;
    if read == 0 {
        return Err(SessionError::Empty {
            path: path.to_path_buf(),
        });
    }
    match serde_json::from_str::<SessionEntry>(&line) {
        Ok(SessionEntry::Header(header)) => {
            if header.version == 0 || header.version > FORMAT_VERSION {
                return Err(SessionError::UnsupportedVersion {
                    path: path.to_path_buf(),
                    found: header.version,
                    supported: FORMAT_VERSION,
                });
            }
            Ok(header)
        }
        _ => Err(SessionError::MissingHeader {
            path: path.to_path_buf(),
        }),
    }
}

/// Terminate a valid final line or remove an incomplete trailing fragment.
fn repair_torn_tail(file: &mut File, path: &Path) -> Result<(), SessionError> {
    let io = |err: std::io::Error| SessionError::Io {
        path: path.to_path_buf(),
        source: err,
    };
    file.rewind().map_err(io)?;
    let len = file.metadata().map_err(io)?;
    if len.len() == 0 {
        return Ok(());
    }
    let mut last_newline: Option<u64> = None;
    let mut offset = 0u64;
    let mut line = 0usize;
    let mut reader = std::io::BufReader::new(&mut *file);
    let mut chunk = Vec::new();
    loop {
        chunk.clear();
        let n = reader.read_until(b'\n', &mut chunk).map_err(io)?;
        if n == 0 {
            break;
        }
        offset += u64::try_from(n).unwrap_or(u64::MAX);
        line += 1;
        if chunk.last() == Some(&b'\n') {
            last_newline = Some(offset - 1);
        } else {
            break;
        }
    }
    let ends_with_newline = last_newline.is_some_and(|pos| pos + 1 == offset);
    if ends_with_newline {
        return Ok(());
    }
    match classify_line(&chunk, false) {
        Line::Entry(_) | Line::Blank => {
            file.write_all(b"\n").map_err(io)?;
            return file.sync_all().map_err(io);
        }
        Line::Torn => {}
        Line::Decode(source) => {
            return Err(SessionError::Decode {
                path: path.to_path_buf(),
                line,
                source,
            });
        }
        Line::InvalidUtf8(source) => {
            return Err(io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                source,
            )));
        }
    }
    let Some(last_newline) = last_newline else {
        return Err(SessionError::TornHeader {
            path: path.to_path_buf(),
        });
    };
    file.set_len(last_newline + 1).map_err(io)?;
    file.sync_all().map_err(io)
}

/// Delete a session while holding its exclusive locks.
///
/// The file is opened but not repaired. Lock acquisition failures prevent
/// deletion, and the sidecar remains in place for future lock holders.
/// On Windows aliases can bypass the sidecar lock.
pub fn delete_locked(path: &Path) -> Result<(), SessionError> {
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("lock"))
    {
        return Err(SessionError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to delete a lock sidecar",
            ),
        });
    }
    let (_file, _lock) = open_locked(path, false)?;
    std::fs::remove_file(path).map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Whether a session appears to be locked by another writer.
///
/// Unix probes the inode, while Windows probes the sidecar. This is only a display hint. Errors return false, and callers must use
/// [`delete_locked`] rather than relying on this probe for synchronization.
#[must_use]
pub fn is_locked(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use nix::fcntl::{Flock, FlockArg};
        let Ok(file) = File::open(path) else {
            return false;
        };
        matches!(
            Flock::lock(file, FlockArg::LockExclusiveNonblock),
            Err((_, nix::errno::Errno::EWOULDBLOCK))
        )
    }
    #[cfg(windows)]
    {
        use std::fs::TryLockError;
        let Some(file) = lock_file(path) else {
            return false;
        };
        match file.try_lock() {
            Ok(()) => {
                let _ = file.unlock();
                false
            }
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Error(_)) => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        false
    }
}

/// Open or create the `<session>.lock` sidecar that carries the
/// writer's advisory lock. `None` when it cannot be opened.
#[cfg(any(unix, windows))]
fn lock_file(path: &Path) -> Option<File> {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(".lock");
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(PathBuf::from(sidecar))
        .ok()
}

/// Take an exclusive non-blocking advisory lock on the session's
/// sidecar lock file.
///
/// `EWOULDBLOCK` means another appender holds the lock, reported as
/// [`SessionError::Locked`]. Any other `flock` failure (filesystems
/// without lock support) is returned as an I/O error.
#[cfg(unix)]
fn acquire_lock(path: &Path) -> Result<Option<nix::fcntl::Flock<File>>, SessionError> {
    use nix::fcntl::{Flock, FlockArg};
    let file = lock_file(path).ok_or_else(|| SessionError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other("cannot open session lock"),
    })?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(Some(lock)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Err(SessionError::Locked {
            path: path.to_path_buf(),
        }),
        Err((_, err)) => Err(SessionError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::from_raw_os_error(err as i32),
        }),
    }
}

/// Take an exclusive non-blocking advisory lock with `LockFileEx` over
/// the whole sidecar file, mirroring the Unix `flock` shape.
///
/// A held lock is reported as [`SessionError::Locked`]. Any other lock
/// failure (including unsupported locks) is returned as an I/O error. Unlike `flock`, Windows byte-range locks belong to the whole
/// process: closing any other handle to the sidecar in this process
/// releases them, so the lock is only meaningful against other
/// processes.
#[cfg(windows)]
fn acquire_lock(path: &Path) -> Result<Option<File>, SessionError> {
    use std::fs::TryLockError;
    let file = lock_file(path).ok_or_else(|| SessionError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other("cannot open session lock"),
    })?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Err(SessionError::Locked {
            path: path.to_path_buf(),
        }),
        Err(TryLockError::Error(source)) => Err(SessionError::Io {
            path: path.to_path_buf(),
            source,
        }),
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
            backend: None,
            parent_entry: None,
        }
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn locked_delete_refuses_active_writer_and_preserves_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let writer = SessionWriter::create(&path, fresh_header()).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            delete_locked(&path),
            Err(SessionError::Locked { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(writer);
        delete_locked(&path).unwrap();
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn locked_delete_accepts_a_read_only_session() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, b"not a session").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        delete_locked(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn locked_delete_does_not_open_or_repair_session() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, b"not a session").unwrap();
        delete_locked(&path).unwrap();
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_and_inode_aliases_cannot_bypass_live_writer() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = SessionWriter::create(&path, fresh_header()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let sidecar = dir.path().join("session.jsonl.lock");
        assert!(delete_locked(&sidecar).is_err());
        assert!(sidecar.exists());
        let sidecar_alias = dir.path().join("sidecar-alias");
        std::fs::hard_link(&sidecar, &sidecar_alias).unwrap();
        assert!(delete_locked(&sidecar_alias).is_err());
        for alias in [
            dir.path().join("symlink.jsonl"),
            dir.path().join("hardlink.jsonl"),
        ] {
            if alias.file_name().unwrap() == "symlink.jsonl" {
                symlink(&path, &alias).unwrap();
            } else {
                std::fs::hard_link(&path, &alias).unwrap();
            }
            assert!(matches!(
                SessionWriter::open(&alias),
                Err(SessionError::Locked { .. })
            ));
            assert!(matches!(
                SessionWriter::open_native_replay(&alias),
                Err(SessionError::Locked { .. })
            ));
            assert!(matches!(
                delete_locked(&alias),
                Err(SessionError::Locked { .. })
            ));
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(is_locked(&alias));
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(writer);
    }

    #[test]
    fn locked_delete_fails_closed_when_sidecar_cannot_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, b"contents").unwrap();
        std::fs::create_dir(dir.path().join("s.jsonl.lock")).unwrap();
        assert!(delete_locked(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"contents");
    }

    #[test]
    fn native_replay_locks_and_includes_latest_history() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut first = SessionWriter::create(&path, fresh_header()).unwrap();
        first
            .append(&SessionEntry::Message(MessageEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                message: Arc::new(Message::new(
                    Role::User,
                    vec![Content::Text {
                        text: "latest".into(),
                    }],
                    None,
                )),
                usage: None,
            }))
            .unwrap();
        assert!(matches!(
            SessionWriter::open_native_replay(&path),
            Err(SessionError::Locked { .. })
        ));
        drop(first);
        let (mut writer, replay) = SessionWriter::open_native_replay(&path).unwrap().unwrap();
        assert_eq!(replay.history.len(), 1);
        assert!(
            matches!(&replay.history[0].content[0], Content::Text { text } if text == "latest")
        );
        assert!(matches!(
            SessionWriter::open(&path),
            Err(SessionError::Locked { .. })
        ));
        writer
            .append(&SessionEntry::Label(Label {
                id: EntryId::new(),
                ts: Utc::now(),
                text: "after".into(),
                anchor: replay.header.id,
            }))
            .unwrap();
        drop(writer);
        assert!(std::fs::read_to_string(path).unwrap().contains("after"));
    }

    #[test]
    fn native_replay_reads_multiple_entries_and_repairs_torn_tail() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut writer = SessionWriter::create(&path, fresh_header()).unwrap();
        for text in ["one", "two", "three"] {
            writer
                .append(&SessionEntry::Message(MessageEntry {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    message: Arc::new(Message::new(
                        Role::User,
                        vec![Content::Text { text: text.into() }],
                        None,
                    )),
                    usage: None,
                }))
                .unwrap();
        }
        drop(writer);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"broken");
        std::fs::write(&path, &bytes).unwrap();
        let (writer, replay) = SessionWriter::open_native_replay(&path).unwrap().unwrap();
        assert_eq!(replay.history.len(), 3);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("broken"));
        drop(writer);
    }

    #[cfg(unix)]
    #[test]
    fn native_replay_appends_to_replayed_inode_after_replacement() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let old = dir.path().join("old.jsonl");
        drop(SessionWriter::create(&path, fresh_header()).unwrap());
        let (mut writer, replay) = SessionWriter::open_native_replay(&path).unwrap().unwrap();
        std::fs::rename(&path, &old).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        writer
            .append(&SessionEntry::Label(Label {
                id: EntryId::new(),
                ts: Utc::now(),
                text: "original".into(),
                anchor: replay.header.id,
            }))
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        assert!(std::fs::read_to_string(old).unwrap().contains("original"));
    }

    #[test]
    fn native_replay_refuses_external_before_repair() {
        use crate::entry::{ExternalAcpSession, SessionBackend};
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut header = fresh_header();
        header.cwd = dir.path().into();
        header.backend = Some(SessionBackend::ExternalAcp(ExternalAcpSession {
            agent_name: "agent".into(),
            protocol_version: 1,
            upstream_session_id: "secret".into(),
        }));
        drop(SessionWriter::create(&path, header).unwrap());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{broken");
        std::fs::write(&path, &bytes).unwrap();
        assert!(SessionWriter::open_native_replay(&path).unwrap().is_none());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn native_replay_fails_if_lock_cannot_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        drop(SessionWriter::create(&path, fresh_header()).unwrap());
        let mut lock = path.as_os_str().to_os_string();
        lock.push(".lock");
        std::fs::remove_file(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();
        assert!(SessionWriter::open_native_replay(&path).is_err());
    }

    #[test]
    fn create_writes_header_then_appends_one_line_per_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let mut w = SessionWriter::create(&path, fresh_header()).unwrap();
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
    fn external_header_persists_canonical_cwd_and_minimal_identity() {
        use crate::entry::{ExternalAcpSession, SessionBackend};
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut header = fresh_header();
        header.cwd = dir.path().join(".");
        header.version = 3;
        header.model = String::new();
        header.system_prompt = String::new();
        header.backend = Some(SessionBackend::ExternalAcp(ExternalAcpSession {
            agent_name: "configured".into(),
            protocol_version: 1,
            upstream_session_id: "id".into(),
        }));
        drop(SessionWriter::create(&path, header).unwrap());
        let raw = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["version"], 4);
        // A version 3 reader rejects this header before replay or fork.
        assert!(value["version"].as_u64().unwrap() > 3);
        assert_eq!(
            value["cwd"],
            std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap()
        );
        assert_eq!(
            value["backend"],
            serde_json::json!({"kind": "external_acp", "agent_name": "configured", "protocol_version": 1, "upstream_session_id": "id"})
        );
        assert!(!raw.contains("command") && !raw.contains("env") && !raw.contains("secret"));
        drop(SessionWriter::open(&path).unwrap());
        let header = crate::reader::SessionReader::iter(&path)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert!(matches!(
            header,
            SessionEntry::Header(Header {
                backend: Some(SessionBackend::ExternalAcp(_)),
                ..
            })
        ));
    }

    #[test]
    fn ambiguous_legacy_external_header_is_rejected_on_read_and_open() {
        use crate::entry::{ExternalAcpSession, SessionBackend};
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut header = fresh_header();
        header.version = 3;
        header.backend = Some(SessionBackend::ExternalAcp(ExternalAcpSession {
            agent_name: "agent".into(),
            protocol_version: 1,
            upstream_session_id: "id".into(),
        }));
        let bytes = format!(
            "{}\n",
            serde_json::to_string(&SessionEntry::Header(header)).unwrap()
        );
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            crate::reader::SessionReader::iter(&path)
                .unwrap()
                .next()
                .unwrap()
                .is_err()
        );
        assert!(SessionWriter::open(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
    }

    #[test]
    fn external_header_with_missing_cwd_creates_nothing() {
        use crate::entry::{ExternalAcpSession, SessionBackend};
        let dir = tempdir().unwrap();
        let path = dir.path().join("new/session.jsonl");
        let mut header = fresh_header();
        header.cwd = dir.path().join("missing");
        header.backend = Some(SessionBackend::ExternalAcp(ExternalAcpSession {
            agent_name: "agent".into(),
            protocol_version: 1,
            upstream_session_id: "id".into(),
        }));
        assert!(SessionWriter::create(&path, header).is_err());
        assert!(!path.parent().unwrap().exists());
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
        std::fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::to_string(&SessionEntry::Header(header)).unwrap()
            ),
        )
        .unwrap();
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
    fn open_appends_to_a_version_one_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("old.jsonl");
        let header = Header {
            version: 1,
            ..fresh_header()
        };
        std::fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::to_string(&SessionEntry::Header(header)).unwrap()
            ),
        )
        .unwrap();
        let mut w = SessionWriter::open(&path).unwrap();
        w.append(&SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "appended".into(),
            anchor: EntryId::new(),
        }))
        .unwrap();
        drop(w);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.split_terminator('\n').count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn open_fails_while_another_writer_holds_the_lock() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let w = SessionWriter::create(&path, fresh_header()).unwrap();
        let err = SessionWriter::open(&path).unwrap_err();
        assert!(matches!(err, SessionError::Locked { .. }));
        drop(w);
        SessionWriter::open(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn is_locked_while_a_writer_holds_the_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        assert!(!is_locked(&path), "a missing file is not locked");
        let w = SessionWriter::create(&path, fresh_header()).unwrap();
        assert!(is_locked(&path));
        assert!(is_locked(&path), "the probe must not keep a lock");
        drop(w);
        assert!(!is_locked(&path));
        let _w = SessionWriter::open(&path).unwrap();
    }

    /// Windows mirror of
    /// `open_fails_while_another_writer_holds_the_lock`. Run on a
    /// Windows CI leg.
    #[cfg(windows)]
    #[test]
    fn open_fails_while_another_writer_holds_the_lock_windows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let w = SessionWriter::create(&path, fresh_header()).unwrap();
        let err = SessionWriter::open(&path).unwrap_err();
        assert!(matches!(err, SessionError::Locked { .. }));
        drop(w);
        SessionWriter::open(&path).unwrap();
    }

    /// Windows mirror of `is_locked_while_a_writer_holds_the_file`.
    /// Unlike the Unix twin this does not re-probe while the writer
    /// holds the lock: Windows releases a process's byte-range locks
    /// when the process closes any handle to the sidecar, so the first
    /// probe's close can end the writer's lock. Run on a Windows CI
    /// leg.
    #[cfg(windows)]
    #[test]
    fn is_locked_while_a_writer_holds_the_file_windows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        assert!(!is_locked(&path), "a missing file is not locked");
        let w = SessionWriter::create(&path, fresh_header()).unwrap();
        assert!(is_locked(&path));
        drop(w);
        assert!(!is_locked(&path));
        let _w = SessionWriter::open(&path).unwrap();
    }

    /// The lock must be taken before the torn-tail repair: a second
    /// opener that loses the lock must leave the tail alone. Run on a
    /// Windows CI leg.
    #[cfg(windows)]
    #[test]
    fn lock_is_taken_before_repair_on_windows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = serde_json::to_string(&SessionEntry::Header(fresh_header())).unwrap();
        let kept = serde_json::to_string(&SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "kept".into(),
            anchor: EntryId::new(),
        }))
        .unwrap();
        let torn = format!("{header}\n{kept}\n{{\"type\":\"label\",\"id\":\"01");
        std::fs::write(&path, torn).unwrap();

        // A foreign writer holding the lock, exactly as another kage
        // process would: the lock lives on the sidecar, not on the
        // data file.
        let holder = lock_file(&path).unwrap();
        holder.try_lock().unwrap();
        let err = SessionWriter::open(&path).unwrap_err();
        assert!(matches!(err, SessionError::Locked { .. }), "{err:?}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with("\"id\":\"01"),
            "the loser truncated while a writer held the lock"
        );
        drop(holder);

        let mut w = SessionWriter::open(&path).unwrap();
        w.append(&SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "after".into(),
            anchor: EntryId::new(),
        }))
        .unwrap();
        drop(w);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("01{"), "torn fragment was not truncated");
    }

    /// The lock must outlive the torn-tail repair: the repair's
    /// second truncate handle touches only the data file, never the
    /// sidecar that carries the lock. Run on a Windows CI leg.
    #[cfg(windows)]
    #[test]
    fn lock_survives_the_repair_on_windows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = serde_json::to_string(&SessionEntry::Header(fresh_header())).unwrap();
        let torn = format!("{header}\n{{\"type\":\"label\",\"id\":\"01");
        std::fs::write(&path, torn).unwrap();
        let _w = SessionWriter::open(&path).unwrap();
        let err = SessionWriter::open(&path).unwrap_err();
        assert!(matches!(err, SessionError::Locked { .. }), "{err:?}");
    }

    #[test]
    fn open_refuses_an_empty_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        std::fs::write(&path, b"").unwrap();
        let err = SessionWriter::open(&path).unwrap_err();
        assert!(matches!(err, SessionError::Empty { .. }), "{err:?}");
    }

    #[test]
    fn open_refuses_a_partial_header_and_keeps_the_bytes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let partial = b"{\"type\":\"header\",\"vers";
        std::fs::write(&path, partial).unwrap();
        let err = SessionWriter::open(&path).unwrap_err();
        assert!(matches!(err, SessionError::MissingHeader { .. }), "{err:?}");
        assert_eq!(std::fs::read(&path).unwrap(), partial, "bytes stay put");
    }

    #[test]
    fn open_preserves_a_header_that_never_ended() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = serde_json::to_string(&SessionEntry::Header(fresh_header())).unwrap();
        std::fs::write(&path, &header).unwrap();
        let mut writer = SessionWriter::open(&path).unwrap();
        writer.append_lines(b"\n").unwrap();
        drop(writer);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{header}\n\n")
        );
    }

    #[test]
    fn open_preserves_valid_unterminated_entry_before_append() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = serde_json::to_string(&SessionEntry::Header(fresh_header())).unwrap();
        let entry = SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "preserved".into(),
            anchor: EntryId::new(),
        });
        let tail = serde_json::to_string(&entry).unwrap();
        std::fs::write(&path, format!("{header}\n{tail}")).unwrap();
        assert_eq!(
            crate::reader::SessionReader::iter(&path).unwrap().count(),
            2
        );
        let mut writer = SessionWriter::open(&path).unwrap();
        writer.append(&entry).unwrap();
        drop(writer);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, format!("{header}\n{tail}\n{tail}\n"));
        assert_eq!(
            crate::reader::SessionReader::iter(&path)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn open_repairs_invalid_tails_and_preserves_complete_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = serde_json::to_string(&SessionEntry::Header(fresh_header())).unwrap();
        for tail in [&b"{\"type\":\"label\""[..], &b"{\"text\":\"\xe2\x82"[..]] {
            let mut bytes = format!("{header}\n").into_bytes();
            bytes.extend_from_slice(tail);
            std::fs::write(&path, bytes).unwrap();
            drop(SessionWriter::open(&path).unwrap());
            assert_eq!(
                std::fs::read(&path).unwrap(),
                format!("{header}\n").as_bytes()
            );
        }
        let complete = format!("{header}\nnot json\n");
        std::fs::write(&path, &complete).unwrap();
        drop(SessionWriter::open(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), complete);
    }

    #[test]
    fn open_preserves_blank_tail_and_rejects_invalid_utf8() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = serde_json::to_string(&SessionEntry::Header(fresh_header())).unwrap();
        let blank = format!("{header}\n \t\r");
        std::fs::write(&path, &blank).unwrap();
        drop(SessionWriter::open(&path).unwrap());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            format!("{blank}\n").as_bytes()
        );

        for tail in [&b"\xff"[..], &b"{\"text\":\"\xff"[..], &b"null\xe2\x82"[..]] {
            let mut bytes = format!("{header}\n").into_bytes();
            bytes.extend_from_slice(tail);
            std::fs::write(&path, &bytes).unwrap();
            let err = SessionWriter::open(&path).unwrap_err();
            assert!(
                matches!(err, SessionError::Io { ref source, .. } if source.kind() == std::io::ErrorKind::InvalidData),
                "{err:?}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn open_rejects_complete_incompatible_unterminated_json_without_changes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = serde_json::to_string(&SessionEntry::Header(fresh_header())).unwrap();
        for tail in [
            r#"{"type":"future_entry","data":1}"#,
            r#"{"type":"label","id":42}"#,
            r#"{"type":]"#,
            r#"{"type":"label",}"#,
            "null",
        ] {
            let bytes = format!("{header}\n{tail}");
            std::fs::write(&path, &bytes).unwrap();
            let err = SessionWriter::open(&path).unwrap_err();
            assert!(
                matches!(err, SessionError::Decode { line: 2, .. }),
                "{err:?}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes.as_bytes());
        }
    }

    #[test]
    fn create_checks_version_boundaries_before_touching_paths() {
        use crate::entry::{ExternalAcpSession, SessionBackend};

        let dir = tempdir().unwrap();
        for external in [false, true] {
            for version in [0, FORMAT_VERSION + 1] {
                let path = dir
                    .path()
                    .join(format!("{external}-{version}/session.jsonl"));
                let mut header = fresh_header();
                header.version = version;
                if external {
                    header.backend = Some(SessionBackend::ExternalAcp(ExternalAcpSession {
                        agent_name: "agent".into(),
                        protocol_version: 1,
                        upstream_session_id: "id".into(),
                    }));
                    header.cwd = dir.path().join("missing-cwd");
                }
                let err = SessionWriter::create(&path, header).unwrap_err();
                assert!(
                    matches!(err, SessionError::UnsupportedVersion { path: ref error_path, found, supported } if error_path == &path && found == version && supported == FORMAT_VERSION)
                );
                assert!(!path.parent().unwrap().exists());
            }
        }
    }

    #[test]
    fn create_accepts_supported_version_boundaries() {
        use crate::entry::{ExternalAcpSession, SessionBackend};

        let dir = tempdir().unwrap();
        for external in [false, true] {
            for version in [1, FORMAT_VERSION] {
                let path = dir.path().join(format!("{external}-{version}.jsonl"));
                let mut header = fresh_header();
                header.version = version;
                header.cwd = dir.path().to_path_buf();
                if external {
                    header.backend = Some(SessionBackend::ExternalAcp(ExternalAcpSession {
                        agent_name: "agent".into(),
                        protocol_version: 1,
                        upstream_session_id: "id".into(),
                    }));
                }
                drop(SessionWriter::create(&path, header).unwrap());
                let file = File::open(&path).unwrap();
                let written = check_version(&file, &path).unwrap();
                assert_eq!(written.version, FORMAT_VERSION);
                drop(file);
                drop(SessionWriter::open(&path).unwrap());
            }
        }
    }

    #[test]
    fn create_rejects_future_external_version_without_changes() {
        use crate::entry::{ExternalAcpSession, SessionBackend};
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let bytes = b"existing data";
        std::fs::write(&path, bytes).unwrap();
        let mut header = fresh_header();
        header.version = FORMAT_VERSION + 1;
        header.backend = Some(SessionBackend::ExternalAcp(ExternalAcpSession {
            agent_name: "agent".into(),
            protocol_version: 1,
            upstream_session_id: "id".into(),
        }));
        let err = SessionWriter::create(&path, header).unwrap_err();
        assert!(
            matches!(err, SessionError::UnsupportedVersion { found, supported, .. } if found == FORMAT_VERSION + 1 && supported == FORMAT_VERSION)
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn append_lines_after_valid_unterminated_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let header = serde_json::to_string(&SessionEntry::Header(fresh_header())).unwrap();
        let line = format!("{header}\n");
        std::fs::write(&path, &header).unwrap();
        let mut writer = SessionWriter::open(&path).unwrap();
        writer.append_lines(line.as_bytes()).unwrap();
        drop(writer);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{header}\n{line}")
        );
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
