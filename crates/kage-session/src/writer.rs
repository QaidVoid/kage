//! Append-only session writer.
//!
//! [`SessionWriter::create`] starts a new file by writing the header line.
//! [`SessionWriter::open`] reopens an existing file for further appends.
//! Each [`SessionWriter::append`] writes one JSON line and `fsync`s the file
//! before returning, so a successful return implies the entry has reached
//! disk.
//!
//! One writer at a time holds a session file. On Unix it takes an exclusive
//! advisory `flock` on the file itself, so every path to the file shares
//! the lock, and checks the path still names that file. On Windows it opens
//! the file sharing only reads: a second writer, a rename or a delete is
//! refused by the system, while readers still open it. Both belong to the
//! open file. Legacy paths also hold the `.jsonl.lock` sidecar used by
//! older builds for the entire operation. Sidecars are never unlinked, so
//! waiting processes cannot acquire different lock files for one path.
//! Other platforms fail closed.
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
    #[cfg(any(unix, windows))]
    #[expect(
        dead_code,
        reason = "held only so the lock lives as long as the writer"
    )]
    lock: Claim,
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
        let (file, lock) = claim(&mut opts, &path, Use::Create)?;
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
        let (file, lock) = open_locked(&path, true, Use::Write)?;
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
        let (mut file, lock) = open_locked(&path, true, Use::Write)?;
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

/// What a claim on a session file is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    /// Writing a file this process just created, which nobody else can
    /// be writing yet: on Unix the lock is waited for, so a brief probe
    /// by another process does not fail the creation.
    Create,
    /// Writing, or probing whether anyone writes.
    Write,
    /// Renaming or deleting the file, which on Windows must share
    /// deletion with itself. A writer shares only reads, so nothing
    /// renames or deletes a live session.
    Remove,
}

/// Opens the session file at `path` for writing as its one writer:
/// `opts` gets the share mode that claims it on Windows, and on Unix the
/// file is `flock`ed. Another writer holding it is
/// [`SessionError::Locked`].
#[cfg(any(unix, windows))]
fn claim(opts: &mut OpenOptions, path: &Path, use_: Use) -> Result<(File, Claim), SessionError> {
    let io = |source: std::io::Error| SessionError::Io {
        path: path.to_path_buf(),
        source,
    };
    let legacy = claim_legacy(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_DELETE: u32 = 0x4;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        opts.share_mode(if use_ == Use::Remove {
            FILE_SHARE_READ | FILE_SHARE_DELETE
        } else {
            FILE_SHARE_READ
        });
        let file = opts.open(path).map_err(|source| {
            if source.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
                SessionError::Locked {
                    path: path.to_path_buf(),
                }
            } else {
                io(source)
            }
        })?;
        Ok((file, legacy))
    }
    #[cfg(unix)]
    {
        use nix::fcntl::{Flock, FlockArg};
        use std::os::unix::fs::MetadataExt;
        let file = opts.open(path).map_err(io)?;
        let arg = if use_ == Use::Create {
            FlockArg::LockExclusive
        } else {
            FlockArg::LockExclusiveNonblock
        };
        let lock = Flock::lock(file.try_clone().map_err(io)?, arg).map_err(|(_, err)| {
            if err == nix::errno::Errno::EWOULDBLOCK {
                SessionError::Locked {
                    path: path.to_path_buf(),
                }
            } else {
                io(std::io::Error::from_raw_os_error(err as i32))
            }
        })?;
        // The lock is on the file the path named when it was opened. A
        // delete or move in between leaves this process holding a file
        // the path no longer names, which nothing must be written to.
        let held = file.metadata().map_err(io)?;
        let named = std::fs::metadata(path).ok();
        if held.nlink() == 0
            || named.is_none_or(|named| (named.dev(), named.ino()) != (held.dev(), held.ino()))
        {
            return Err(SessionError::Locked {
                path: path.to_path_buf(),
            });
        }
        Ok((file, (lock, legacy)))
    }
}

/// Opens the existing session file at `path` under its one writer's
/// lock, for `use_`. The file is opened for writing even when `writable`
/// is false, since on Windows the write access is what claims it.
#[cfg(any(unix, windows))]
fn open_locked(path: &Path, writable: bool, use_: Use) -> Result<(File, Claim), SessionError> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(writable || cfg!(windows));
    claim(&mut opts, path, use_)
}

/// Claims the sidecar used by older builds for sessions outside project directories.
#[cfg(any(unix, windows))]
fn claim_legacy(path: &Path) -> Result<Option<LegacyClaim>, SessionError> {
    if crate::layout::is_project_dir(&crate::layout::project_of(path)) {
        return Ok(None);
    }
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(".lock");
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(sidecar).map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.try_lock().map_err(|err| match err {
        std::fs::TryLockError::WouldBlock => SessionError::Locked {
            path: path.to_path_buf(),
        },
        std::fs::TryLockError::Error(source) => SessionError::Io {
            path: path.to_path_buf(),
            source,
        },
    })?;
    Ok(Some(LegacyClaim(file)))
}

/// Explicit unlocking prevents a concurrently forked child from prolonging a claim.
#[cfg(any(unix, windows))]
#[derive(Debug)]
struct LegacyClaim(File);

#[cfg(any(unix, windows))]
impl Drop for LegacyClaim {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// What a writer holds while it writes: the `flock` on Unix; on Windows
/// the share mode of the file handle itself.
#[cfg(unix)]
type Claim = (nix::fcntl::Flock<File>, Option<LegacyClaim>);
#[cfg(windows)]
type Claim = Option<LegacyClaim>;

#[cfg(not(any(unix, windows)))]
fn claim(_opts: &mut OpenOptions, path: &Path, _use: Use) -> Result<(File, ()), SessionError> {
    Err(SessionError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "session locking is unsupported",
        ),
    })
}
#[cfg(not(any(unix, windows)))]
fn open_locked(path: &Path, _writable: bool, use_: Use) -> Result<(File, ()), SessionError> {
    claim(&mut OpenOptions::new(), path, use_)
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

/// Move a session file to `dst` while holding its writer's lock, so no
/// writer appends to it mid-move or opens it at `dst` before it lands. A
/// session
/// another process holds fails with [`SessionError::Locked`], and an
/// existing distinct `dst` is never replaced. If an interrupted hard-link
/// move left both names for one file, the source name is removed under the
/// same locks. Filesystems without a safe rename or hard links fail closed.
/// Empty directories made for `dst` are removed when the move fails.
pub fn move_locked(path: &Path, dst: &Path) -> Result<(), SessionError> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| SessionError::Io { path, source }
    };
    // The writer's lock is on the file, so it moves with it: a writer at
    // either path is refused until the move is done.
    let (_file, _lock) = open_locked(path, false, Use::Remove)?;
    if path == dst {
        return Ok(());
    }
    if same_file(path, dst) {
        #[cfg(any(unix, windows))]
        let _destination_lock = claim_legacy(dst)?;
        return std::fs::remove_file(path).map_err(io(path));
    }
    // The directories this move makes, deepest first, to remove again if
    // it fails.
    let made: Vec<&Path> = dst
        .ancestors()
        .skip(1)
        .take_while(|dir| !dir.as_os_str().is_empty() && !dir.exists())
        .collect();
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir).map_err(io(dir))?;
    }
    #[cfg(any(unix, windows))]
    let _destination_lock = claim_legacy(dst)?;
    if let Err(source) = place(path, dst) {
        for dir in made {
            let _ = std::fs::remove_dir(dir);
        }
        return Err(io(path)(source));
    }
    Ok(())
}

/// Whether both paths name the same file, including an interrupted hard-link move.
pub(crate) fn same_file(path: &Path, dst: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        && std::fs::symlink_metadata(dst).is_ok_and(|metadata| metadata.is_file())
        && same_file::is_same_file(path, dst).unwrap_or(false)
}

/// Moves without replacing a destination. A hard-link fallback may leave
/// both names after interruption; the next locked move recovers that state.
fn place(path: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use nix::fcntl::{AT_FDCWD, RenameFlags, renameat2};
        match renameat2(AT_FDCWD, path, AT_FDCWD, dst, RenameFlags::RENAME_NOREPLACE) {
            Ok(()) => return Ok(()),
            Err(
                nix::errno::Errno::ENOSYS
                | nix::errno::Errno::EINVAL
                | nix::errno::Errno::EOPNOTSUPP,
            ) => {}
            Err(err) => return Err(std::io::Error::from_raw_os_error(err as i32)),
        }
    }
    #[cfg(any(unix, windows))]
    {
        std::fs::hard_link(path, dst)?;
        std::fs::remove_file(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, dst);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe session moves are unsupported",
        ))
    }
}

/// [`delete_locked`], then the session's own directory, which holds its
/// agents (`<id>/agents/`). An agent's file has no such directory; its
/// `agents` directory, and the session directory above it, go when the
/// agent was the last one there.
pub fn delete_with_agents(path: &Path) -> Result<(), SessionError> {
    if path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == crate::layout::AGENTS_DIR)
    {
        delete_locked(path)?;
        if let Some(agents) = path.parent()
            && std::fs::remove_dir(agents).is_ok()
            && let Some(home) = agents.parent()
        {
            let _ = std::fs::remove_dir(home);
        }
        return Ok(());
    }
    let home = match (path.parent(), path.file_stem()) {
        (Some(dir), Some(stem)) => dir.join(stem),
        _ => return delete_locked(path),
    };
    // Every agent is claimed before anything goes, so an agent another
    // process writes refuses the whole delete instead of leaving it half
    // done.
    let agents_dir = home.join(crate::layout::AGENTS_DIR);
    let agent_files: Vec<PathBuf> = std::fs::read_dir(&agents_dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|file| file.extension().is_some_and(|ext| ext == "jsonl"))
                .collect()
        })
        .unwrap_or_default();
    let claims = agent_files
        .iter()
        .map(|file| open_locked(file, false, Use::Remove))
        .collect::<Result<Vec<_>, _>>()?;
    delete_locked(path)?;
    for file in &agent_files {
        let _ = std::fs::remove_file(file);
    }
    drop(claims);
    if std::fs::remove_dir(&agents_dir).is_ok() {
        let _ = std::fs::remove_dir(&home);
    }
    Ok(())
}

/// Delete a session while holding its writer's lock and its legacy
/// sidecar lock, if applicable. Lock sidecars remain in place.
///
/// The file is opened but not repaired. A session another writer holds is
/// not deleted.
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
    let (_file, _lock) = open_locked(path, false, Use::Remove)?;
    std::fs::remove_file(path).map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Whether a session appears to be locked by another writer.
///
/// This is only a display hint. Errors return false, and callers must use
/// [`delete_locked`] rather than relying on this probe for synchronization.
#[must_use]
pub fn is_locked(path: &Path) -> bool {
    #[cfg(any(unix, windows))]
    {
        matches!(
            open_locked(path, false, Use::Write),
            Err(SessionError::Locked { .. })
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        false
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
    fn interrupted_move_recovers_the_same_inode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        drop(SessionWriter::create(&path, fresh_header()).unwrap());
        let dst = dir.path().join("project-0123456789/s.jsonl");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::hard_link(&path, &dst).unwrap();
        let before = std::fs::read(&dst).unwrap();
        move_locked(&path, &dst).unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), before);
    }

    #[test]
    fn legacy_process_lock_helper() {
        let Some(path) = std::env::var_os("KAGE_TEST_LEGACY_LOCK") else {
            return;
        };
        let path = PathBuf::from(path);
        let probe = std::env::var_os("KAGE_TEST_LEGACY_PROBE").is_some();
        let _transcript = (!probe).then(|| OpenOptions::new().append(true).open(&path).unwrap());
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(sidecar)
            .unwrap();
        if probe {
            assert!(matches!(
                lock.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            return;
        }
        lock.try_lock().unwrap();
        std::fs::write(path.with_extension("ready"), b"").unwrap();
        let mut release = String::new();
        std::io::stdin().read_line(&mut release).unwrap();
    }

    #[test]
    fn legacy_writer_holds_its_sidecar_until_drop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let header = Header {
            version: 3,
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
        let writer = SessionWriter::open(&path).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "writer::tests::legacy_process_lock_helper"])
            .env("KAGE_TEST_LEGACY_LOCK", &path)
            .env("KAGE_TEST_LEGACY_PROBE", "1")
            .status()
            .unwrap();
        assert!(status.success());
        drop(writer);
        let sidecar = OpenOptions::new()
            .write(true)
            .open(dir.path().join("s.jsonl.lock"))
            .unwrap();
        sidecar.try_lock().unwrap();
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn interrupted_move_waits_for_a_writer_at_the_destination() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("s.jsonl");
        drop(SessionWriter::create(&source, fresh_header()).unwrap());
        let dst = dir.path().join("project-0123456789/s.jsonl");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::hard_link(&source, &dst).unwrap();
        let writer = SessionWriter::open(&dst).unwrap();
        assert!(matches!(
            move_locked(&source, &dst),
            Err(SessionError::Locked { .. })
        ));
        assert!(source.exists() && dst.exists());
        drop(writer);
        move_locked(&source, &dst).unwrap();
        assert!(!source.exists());
        assert!(dst.exists());
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_destination_is_not_interrupted_move_recovery() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("s.jsonl");
        drop(SessionWriter::create(&source, fresh_header()).unwrap());
        let dst = dir.path().join("alias.jsonl");
        std::os::unix::fs::symlink(&source, &dst).unwrap();
        assert!(
            matches!(move_locked(&source, &dst), Err(SessionError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert!(source.exists() && dst.exists());
    }

    #[test]
    fn legacy_process_lock_blocks_writes_moves_and_deletes() {
        use std::process::{Command, Stdio};
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let header = Header {
            version: 3,
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
        let before = std::fs::read(&path).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "writer::tests::legacy_process_lock_helper"])
            .env("KAGE_TEST_LEGACY_LOCK", &path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let ready = path.with_extension("ready");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let observed = ready.exists();
        let write = SessionWriter::open(&path);
        let replay = SessionWriter::open_native_replay(&path);
        let moved = move_locked(&path, &dir.path().join("project-0123456789/s.jsonl"));
        let deleted = delete_locked(&path);
        writeln!(child.stdin.take().unwrap()).unwrap();
        assert!(child.wait().unwrap().success());
        assert!(observed, "legacy process acquired its sidecar");
        assert!(matches!(write, Err(SessionError::Locked { .. })));
        assert!(matches!(replay, Err(SessionError::Locked { .. })));
        assert!(matches!(moved, Err(SessionError::Locked { .. })));
        assert!(matches!(deleted, Err(SessionError::Locked { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), before);
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
    fn aliases_cannot_bypass_a_live_writer() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = SessionWriter::create(&path, fresh_header()).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            dir.path().join("session.jsonl.lock").exists(),
            "legacy paths also hold the stable-compatible sidecar"
        );
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
    fn deleting_and_moving_preserve_legacy_lock_identity() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        drop(SessionWriter::create(&path, fresh_header()).unwrap());
        let sidecar = dir.path().join("s.jsonl.lock");
        std::fs::write(&sidecar, b"").unwrap();
        let moved = dir.path().join("moved/s.jsonl");
        move_locked(&path, &moved).unwrap();
        assert!(sidecar.exists(), "a move preserves the sidecar identity");
        let moved_sidecar = dir.path().join("moved/s.jsonl.lock");
        std::fs::write(&moved_sidecar, b"").unwrap();
        delete_locked(&moved).unwrap();
        assert!(
            moved_sidecar.exists(),
            "a delete preserves the sidecar identity"
        );
    }

    #[test]
    fn a_move_refused_by_a_live_writer_leaves_no_directory() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let _writer = SessionWriter::create(&path, fresh_header()).unwrap();
        let dst = dir.path().join("project/01ROOT/agents/s.jsonl");
        assert!(matches!(
            move_locked(&path, &dst),
            Err(SessionError::Locked { .. })
        ));
        assert!(
            !dir.path().join("project").exists(),
            "every level it would have made is gone"
        );
        assert!(path.exists());
    }

    #[test]
    fn a_move_never_replaces_a_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let taken = dir.path().join("t.jsonl");
        std::fs::write(&path, b"mine").unwrap();
        std::fs::write(&taken, b"theirs").unwrap();
        assert!(place(&path, &taken).is_err());
        assert_eq!(std::fs::read(&taken).unwrap(), b"theirs");
        assert_eq!(std::fs::read(&path).unwrap(), b"mine");
    }

    #[test]
    fn a_delete_waits_for_every_agent_to_be_free() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("root.jsonl");
        drop(SessionWriter::create(&path, fresh_header()).unwrap());
        let agent = dir.path().join("root/agents/kid.jsonl");
        let held = SessionWriter::create(&agent, fresh_header()).unwrap();
        assert!(matches!(
            delete_with_agents(&path),
            Err(SessionError::Locked { .. })
        ));
        assert!(path.exists() && agent.exists(), "nothing went");
        drop(held);
        delete_with_agents(&path).unwrap();
        assert!(!path.exists());
        assert!(dir.path().join("root/agents/kid.jsonl.lock").exists());
    }

    /// An older build deletes or renames by path, with no lock of ours:
    /// a live writer must refuse that. Run on a Windows CI leg.
    #[cfg(windows)]
    #[test]
    fn a_live_session_cannot_be_deleted_or_renamed_under_its_writer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let writer = SessionWriter::create(&path, fresh_header()).unwrap();
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&path, dir.path().join("t.jsonl")).is_err());
        assert!(std::fs::read(&path).is_ok(), "readers still open it");
        drop(writer);
        std::fs::rename(&path, dir.path().join("t.jsonl")).unwrap();
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
    /// Run on a Windows CI leg.
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

        // A foreign writer holding the file, exactly as another kage
        // process would: open with the writer's share mode.
        let holder = claim(OpenOptions::new().read(true).write(true), &path, Use::Write).unwrap();
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

    /// The lock must outlive the torn-tail repair, which truncates
    /// through the writer's own handle. Run on a Windows CI leg.
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
