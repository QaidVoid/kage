//! Atomic filesystem writes used by `write` and `edit` tools.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::ToolError;

/// Atomically replace the contents of `target` with `content`.
///
/// Writes to a sibling temp file in the same directory, then renames it onto
/// the target. The target's parent must exist. Partial writes never become
/// visible: either the rename succeeds and the file is fully updated, or the
/// temp file is removed and the original is untouched.
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
pub fn atomic_write(target: &Path, content: &[u8]) -> Result<(), ToolError> {
    target.parent().ok_or_else(|| ToolError::Path {
        path: target.to_owned(),
        reason: "no parent directory".into(),
    })?;

    let temp = temp_sibling(target);
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(content)?;
        file.sync_all()?;
    }
    // Fail rather than rename a widened mode over a private or executable
    // file: the original stays untouched on the error path.
    if let Ok(meta) = fs::metadata(target) {
        fs::set_permissions(&temp, meta.permissions()).map_err(|e| {
            let _ = fs::remove_file(&temp);
            ToolError::Io(e)
        })?;
    }
    if let Err(e) = fs::rename(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(ToolError::Io(e));
    }
    Ok(())
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

    #[test]
    fn writes_and_replaces_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        atomic_write(&p, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello");
        atomic_write(&p, b"replaced").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "replaced");
    }

    #[test]
    fn no_temp_files_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        atomic_write(&dir.path().join("x.txt"), b"x").unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1);
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
}
