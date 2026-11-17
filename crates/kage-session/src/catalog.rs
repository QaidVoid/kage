//! The session catalog: the summaries [`crate::list`] returns, kept in an
//! `SQLite` database beside the sessions ([`CATALOG_FILE`]) and keyed by each
//! file's length and mtime.
//!
//! Everything in it is derived from the session files. A corrupt or
//! other-version catalog is rebuilt from them, and one that cannot be
//! opened at all only costs a rescan. Several kage processes share it:
//! WAL mode lets them read while one writes, and a listing writes only the
//! rows of the files that changed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::list::SessionSummary;

/// The catalog database in a sessions directory. The leading dot and the
/// extension keep it out of every scan for session files.
pub const CATALOG_FILE: &str = ".catalog.db";

/// The schema this build writes. A catalog of another is dropped and
/// rebuilt. Bump it when the table changes or when [`SessionSummary`]
/// changes in a way old rows should not be read as.
const SCHEMA_VERSION: i32 = 1;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS session (
        path TEXT PRIMARY KEY,
        id TEXT NOT NULL,
        size INTEGER NOT NULL,
        mtime INTEGER,
        cwd TEXT NOT NULL,
        created_at TEXT NOT NULL,
        agent TEXT,
        summary TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS session_id ON session (id);
";

/// One file's summary with the length and mtime it was taken at.
pub(crate) type Row = (u64, Option<SystemTime>, SessionSummary);

/// An open catalog.
pub(crate) struct Catalog {
    conn: Connection,
}

impl Catalog {
    /// Opens the catalog of `dir`, creating it when missing and rebuilding
    /// one that is corrupt or of another schema. `None` when it cannot be
    /// opened, such as a busy lock, a full disk or a read-only directory:
    /// only corruption deletes a catalog, since another process may be
    /// using it.
    pub(crate) fn open(dir: &Path) -> Option<Self> {
        match Self::open_at(&dir.join(CATALOG_FILE)) {
            Ok(catalog) => Some(catalog),
            Err(err) if is_corrupt(&err) => Self::rebuild(dir),
            Err(_) => None,
        }
    }

    /// Replaces the catalog of `dir`, which a connection found corrupt.
    /// The caller closes that connection first, since closing it later
    /// would delete the new catalog's `-wal` and `-shm` by name. Another
    /// process may have rebuilt the catalog already, so a fresh connection
    /// that checks clean keeps it; only one that still finds corruption
    /// deletes and recreates it.
    pub(crate) fn rebuild(dir: &Path) -> Option<Self> {
        let path = dir.join(CATALOG_FILE);
        match Self::open_at(&path).and_then(|catalog| catalog.check().map(|()| catalog)) {
            Ok(catalog) => return Some(catalog),
            Err(err) if !is_corrupt(&err) => return None,
            Err(_) => {}
        }
        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.clone().into_os_string();
            name.push(suffix);
            let _ = std::fs::remove_file(name);
        }
        Self::open_at(&path).ok()
    }

    /// Opens the catalog of `dir` only if one exists, never creating it.
    pub(crate) fn open_existing(dir: &Path) -> Option<Self> {
        let path = dir.join(CATALOG_FILE);
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;
        conn.busy_timeout(Duration::from_secs(5)).ok()?;
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .ok()?;
        (version == SCHEMA_VERSION).then_some(Self { conn })
    }

    fn open_at(path: &Path) -> rusqlite::Result<Self> {
        if !path.exists() {
            create_private(path);
        }
        let mut conn = Connection::open(path)?;
        restrict(path);
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let version = |conn: &Connection| -> rusqlite::Result<i32> {
            conn.pragma_query_value(None, "user_version", |row| row.get(0))
        };
        if version(&conn)? != SCHEMA_VERSION {
            // One process at a time sets the schema up, and checks again
            // under the lock, so two first listings do not drop each
            // other's table.
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if version(&tx)? != SCHEMA_VERSION {
                tx.execute_batch("DROP TABLE IF EXISTS session;")?;
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    /// Fails with a corruption error unless an integrity check of the
    /// database comes back clean.
    fn check(&self) -> rusqlite::Result<()> {
        let verdict: String = self
            .conn
            .pragma_query_value(None, "quick_check", |row| row.get(0))?;
        if verdict == "ok" {
            return Ok(());
        }
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(verdict),
        ))
    }

    /// Every row, by file path. A row whose summary no longer decodes is
    /// left out, so its file is summarized again.
    pub(crate) fn rows(&self) -> rusqlite::Result<HashMap<PathBuf, Row>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, size, mtime, summary FROM session")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (path, size, mtime, summary) = row?;
            let Ok(summary) = serde_json::from_str::<SessionSummary>(&summary) else {
                continue;
            };
            let size = u64::try_from(size).unwrap_or_default();
            out.insert(PathBuf::from(path), (size, mtime.map(from_nanos), summary));
        }
        Ok(out)
    }

    /// Stores the rows of `changed` and drops those of `removed`, in one
    /// transaction. A path that is not UTF-8 is not stored, so its file is
    /// summarized again by the next process.
    pub(crate) fn write<'a>(
        &mut self,
        changed: impl IntoIterator<Item = (&'a Path, &'a Row)>,
        removed: impl IntoIterator<Item = &'a Path>,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut put = tx.prepare_cached(
                "INSERT OR REPLACE INTO session
                     (path, id, size, mtime, cwd, created_at, agent, summary)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (path, (size, mtime, summary)) in changed {
                let (Some(path), Some(cwd)) = (path.to_str(), summary.cwd.to_str()) else {
                    continue;
                };
                let Ok(json) = serde_json::to_string(summary) else {
                    continue;
                };
                put.execute(params![
                    path,
                    summary.id.to_string(),
                    i64::try_from(*size).unwrap_or(i64::MAX),
                    mtime.and_then(to_nanos),
                    cwd,
                    summary.created_at.to_rfc3339(),
                    summary.agent,
                    json,
                ])?;
            }
            let mut drop = tx.prepare_cached("DELETE FROM session WHERE path = ?1")?;
            for path in removed {
                if let Some(path) = path.to_str() {
                    drop.execute([path])?;
                }
            }
        }
        tx.commit()
    }

    /// Gives the rows of the files `moved` (from, to) their new paths, so
    /// the moved files are not summarized again. Size and mtime stay, as
    /// a rename keeps both.
    pub(crate) fn relocate(&mut self, moved: &[(PathBuf, PathBuf)]) -> rusqlite::Result<()> {
        // Taking the write lock first lets the busy timeout wait out
        // another writer, which upgrading a read could not.
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut get = tx.prepare_cached("SELECT summary FROM session WHERE path = ?1")?;
            let mut put = tx.prepare_cached(
                "UPDATE OR REPLACE session SET path = ?2, summary = ?3 WHERE path = ?1",
            )?;
            for (from, to) in moved {
                let (Some(from), Some(to_text)) = (from.to_str(), to.to_str()) else {
                    continue;
                };
                let Some(json) = get
                    .query_row([from], |row| row.get::<_, String>(0))
                    .optional()?
                else {
                    continue;
                };
                let Ok(mut summary) = serde_json::from_str::<SessionSummary>(&json) else {
                    continue;
                };
                summary.path.clone_from(to);
                let Ok(json) = serde_json::to_string(&summary) else {
                    continue;
                };
                put.execute(params![from, to_text, json])?;
            }
        }
        tx.commit()
    }

    /// Whether `path` records an agent, from its row, when the row was
    /// taken at `size` and `mtime`. `None` when the row is missing or
    /// stale.
    pub(crate) fn is_agent(
        &self,
        path: &Path,
        size: u64,
        mtime: Option<SystemTime>,
    ) -> Option<bool> {
        let (stored_size, stored_mtime, agent) = self
            .conn
            .query_row(
                "SELECT size, mtime, agent FROM session WHERE path = ?1",
                [path.to_str()?],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .ok()??;
        let fresh = u64::try_from(stored_size).ok() == Some(size)
            && stored_mtime == mtime.and_then(to_nanos);
        fresh.then_some(agent.is_some())
    }
}

/// Whether `err` says the catalog file is damaged or not a database, the
/// only errors that justify deleting it.
pub(crate) fn is_corrupt(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}

fn to_nanos(time: SystemTime) -> Option<i64> {
    let since = time.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(since.as_nanos()).ok()
}

fn from_nanos(nanos: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(u64::try_from(nanos).unwrap_or_default())
}

/// Creates the empty catalog file readable by the owner only, before
/// `SQLite` opens it: the catalog quotes prompts. `SQLite` gives the `-wal`
/// and `-shm` files it creates later the same permissions.
fn create_private(path: &Path) {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let _ = options.open(path);
}

/// Narrows the catalog and its `-wal` and `-shm` to their owner again, in
/// case `SQLite` created them itself after another process removed the
/// catalog.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.to_path_buf().into_os_string();
            name.push(suffix);
            let _ = std::fs::set_permissions(name, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}
