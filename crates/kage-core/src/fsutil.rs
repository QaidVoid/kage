//! Crash-safe file writes shared by tools, stores, and credential files.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Atomically replace the contents of `target` with `content`.
///
/// Writes to a sibling temp file in the same directory, syncs it, then
/// renames it onto the target. The target's parent must exist. Partial
/// writes never become visible: either the rename succeeds and the file
/// is fully updated, or the temp file is removed and the original is
/// untouched.
///
/// When `target` already exists, its permissions are copied onto the temp
/// file before the rename, so replacing an executable or private file does
/// not silently reset its mode to the umask default.
///
/// # Errors
///
/// - The target has no parent directory.
/// - I/O failure opening, writing, syncing, or renaming the temp file.
/// - An existing target's permissions could not be preserved.
pub fn atomic_write(target: &Path, content: &[u8]) -> io::Result<()> {
    write_via_temp(target, content, false)
}

/// Like [`atomic_write`], for files holding secrets.
///
/// On Unix the temp file is created with mode `0600` before any byte is
/// written, so the secret is never readable by other users, and the
/// target ends up `0600` whatever its previous mode was.
///
/// # Errors
///
/// Same as [`atomic_write`].
pub fn atomic_write_private(target: &Path, content: &[u8]) -> io::Result<()> {
    write_via_temp(target, content, true)
}

fn write_via_temp(target: &Path, content: &[u8], private: bool) -> io::Result<()> {
    if target.parent().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", target.display()),
        ));
    }

    let temp = temp_sibling(target);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = fill_and_replace(&mut file, &temp, target, content, private);
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn fill_and_replace(
    file: &mut fs::File,
    temp: &Path,
    target: &Path,
    content: &[u8],
    private: bool,
) -> io::Result<()> {
    file.write_all(content)?;
    file.sync_all()?;
    if !private && let Ok(meta) = fs::metadata(target) {
        fs::set_permissions(temp, meta.permissions())?;
    }
    fs::rename(temp, target)?;
    #[cfg(unix)]
    sync_parent_entry(target);
    Ok(())
}

/// Best-effort durability for a file's directory entry: `fsync`s the
/// parent directory so a just-completed create or rename survives a
/// power cut. The file's own bytes are already synced by the caller;
/// without this the rename itself can still vanish. Unix only (there
/// is no std directory fsync on Windows) and errors are swallowed:
/// the file is on disk either way, and a failed dir-sync must not
/// report the whole write as failed.
#[cfg(unix)]
pub fn sync_parent_entry(target: &Path) {
    let Some(parent) = target.parent() else {
        return;
    };
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
}

fn temp_sibling(target: &Path) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let suffix = ulid::Ulid::new().to_string();
    let name = match target.file_name() {
        Some(n) => format!(".{}.{suffix}.tmp", n.to_string_lossy()),
        None => format!(".kage-{suffix}.tmp"),
    };
    parent.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn writes_and_replaces_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        atomic_write(&p, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello");
        atomic_write(&p, b"replaced").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "replaced");
    }

    #[cfg(unix)]
    #[test]
    fn parent_entry_sync_tolerates_missing_and_real_dirs() {
        // A real directory: must not error or panic.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        atomic_write(&p, b"x").unwrap();
        sync_parent_entry(&p);
        // A target whose parent is gone: swallow, don't panic.
        sync_parent_entry(&dir.path().join("gone").join("b.txt"));
    }

    #[test]
    fn no_temp_files_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        atomic_write(&dir.path().join("x.txt"), b"x").unwrap();
        atomic_write_private(&dir.path().join("y.txt"), b"y").unwrap();
        assert_eq!(entries(dir.path()), ["x.txt", "y.txt"]);
    }

    #[test]
    fn failed_rename_removes_the_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("occupied");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"old").unwrap();

        assert!(atomic_write_private(&target, b"new").is_err());
        assert_eq!(entries(dir.path()), ["occupied"]);
        assert_eq!(fs::read(target.join("keep")).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn replacing_preserves_existing_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("script.sh");
        fs::write(&p, b"old").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        atomic_write(&p, b"new").unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "mode reset by atomic_write");
    }

    #[cfg(unix)]
    #[test]
    fn private_write_is_0600_for_new_and_widened_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("fresh.json");
        atomic_write_private(&fresh, b"{}").unwrap();
        let mode = fs::metadata(&fresh).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let widened = dir.path().join("widened.json");
        fs::write(&widened, b"{}").unwrap();
        fs::set_permissions(&widened, fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write_private(&widened, b"{}").unwrap();
        let mode = fs::metadata(&widened).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn failed_write_leaves_the_old_file_intact() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        let target = locked.join("auth.json");
        atomic_write_private(&target, b"old").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();

        let probe = locked.join("probe");
        if fs::write(&probe, b"").is_ok() {
            // Privileged users ignore directory modes, so there is no failure to observe.
            let _ = fs::remove_file(&probe);
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let result = atomic_write_private(&target, b"new");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(entries(&locked), ["auth.json"]);
    }
}
