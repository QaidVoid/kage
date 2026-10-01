//! Where a running `kage serve` tells local clients it can be reached.
//!
//! Every serve binds `serve-<pid>.sock` in the runtime directory
//! ([`crate::paths::runtime_dir`], mode 0700) and records it in
//! `serve-<pid>.json` as `{pid, socket, started}`. Both files go away
//! when serve shuts down. A serve that died without cleaning up leaves
//! a socket that refuses connections, and the first client to find it
//! removes its files.

use std::fs;
use std::io::{self, ErrorKind, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One registered serve.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Entry {
    /// The serve's process id.
    pub(crate) pid: u32,
    /// The unix socket it accepts attaches on.
    pub(crate) socket: PathBuf,
    /// When it started.
    pub(crate) started: DateTime<Utc>,
}

impl Entry {
    fn record(dir: &Path, pid: u32) -> PathBuf {
        dir.join(format!("serve-{pid}.json"))
    }

    /// Connects to the serve. A socket that is gone or refuses means the
    /// serve is gone, so its files are removed.
    fn connect(&self, dir: &Path) -> Option<UnixStream> {
        match UnixStream::connect(&self.socket) {
            Ok(stream) => Some(stream),
            Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {
                let _ = fs::remove_file(&self.socket);
                let _ = fs::remove_file(Self::record(dir, self.pid));
                None
            }
            Err(_) => None,
        }
    }
}

/// This process's registration. Dropping it removes the socket and the
/// record.
pub(crate) struct Registration {
    socket: PathBuf,
    record: PathBuf,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.record);
    }
}

/// Binds this process's socket in `dir`, owner-only, and records it.
pub(crate) fn register(dir: &Path) -> io::Result<(UnixListener, Registration)> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let pid = std::process::id();
    let socket = dir.join(format!("serve-{pid}.sock"));
    let record = Entry::record(dir, pid);
    // Left by an earlier process that had this pid; no live serve owns it.
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    let registration = Registration {
        socket: socket.clone(),
        record: record.clone(),
    };
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let entry = Entry {
        pid,
        socket,
        started: Utc::now(),
    };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&record)?;
    file.write_all(&serde_json::to_vec(&entry).map_err(io::Error::other)?)?;
    Ok((listener, registration))
}

/// The serves recorded in `dir`, newest first. Records that do not
/// parse are skipped.
fn entries(dir: &Path) -> Vec<Entry> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read
        .filter_map(Result::ok)
        .map(|item| item.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("serve-"))
        })
        .filter_map(|path| serde_json::from_slice(&fs::read(path).ok()?).ok())
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.started));
    entries
}

/// Connections to every serve recorded in `dir`, newest first, opened
/// one at a time as the caller asks. Stale records are removed on the
/// way.
pub(crate) fn connect(dir: &Path) -> impl Iterator<Item = (Entry, UnixStream)> {
    let dir = dir.to_path_buf();
    entries(&dir).into_iter().filter_map(move |entry| {
        let stream = entry.connect(&dir)?;
        Some((entry, stream))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registration_is_listed_and_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        let (_listener, registration) = register(&run).unwrap();
        let mode = fs::metadata(&run).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let found: Vec<_> = connect(&run).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.pid, std::process::id());
        let mode = fs::metadata(&found[0].0.socket)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "the socket must be owner-only");
        drop(found);
        drop(registration);
        assert_eq!(fs::read_dir(&run).unwrap().count(), 0);
    }

    #[test]
    fn stale_records_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("serve-1.sock");
        drop(UnixListener::bind(&socket).unwrap());
        let gone = Entry {
            pid: 1,
            socket,
            started: Utc::now(),
        };
        let missing = Entry {
            pid: 2,
            socket: dir.path().join("serve-2.sock"),
            started: Utc::now(),
        };
        for entry in [&gone, &missing] {
            let record = Entry::record(dir.path(), entry.pid);
            fs::write(record, serde_json::to_vec(entry).unwrap()).unwrap();
        }
        assert_eq!(entries(dir.path()).len(), 2);
        assert_eq!(connect(dir.path()).count(), 0);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
