//! Filesystem helpers shared by tools, stores, the TUI, and
//! credential files: crash-safe writes, tilde expansion and the
//! `/`-separated form of relative paths.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// `path` with `/` between its components on every platform: the form
/// tool output and `@` mentions use. On Unix a `\` is a valid file name
/// character, so only Windows paths change.
#[must_use]
pub fn slashed(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

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
    let suffix = ulid::Ulid::generate().to_string();
    let name = match target.file_name() {
        Some(n) => format!(".{}.{suffix}.tmp", n.to_string_lossy()),
        None => format!(".kage-{suffix}.tmp"),
    };
    parent.join(name)
}

/// Expand a leading `~` the way a shell does for an unquoted word:
/// `~` is the home directory itself and `~/...` is home joined with
/// the rest. `~user` and any other tilde use are left alone, and so
/// is the candidate when the home directory cannot be determined.
#[must_use]
pub fn expand_tilde(candidate: &Path) -> PathBuf {
    let Ok(rest) = candidate.strip_prefix("~") else {
        return candidate.to_owned();
    };
    let Some(home) = dirs::home_dir() else {
        return candidate.to_owned();
    };
    if rest.as_os_str().is_empty() {
        home
    } else {
        home.join(rest)
    }
}

/// `value` ready to use as a path: trimmed, then one layer of
/// matching surrounding quotes stripped (a shell-quoted word or an
/// Explorer "Copy as path" paste), then trimmed again. Unmatched
/// quotes, interior quotes, and a lone quote character are kept,
/// and a value that cleans to the empty string stays empty so
/// callers keep their empty-input behavior.
#[must_use]
pub fn unquote_and_trim(value: &str) -> &str {
    let trimmed = value.trim();
    let first = trimmed.as_bytes().first().copied();
    let quoted = trimmed.len() >= 2
        && matches!(first, Some(b'"' | b'\''))
        && first == trimmed.as_bytes().last().copied();
    if !quoted {
        return trimmed;
    }
    trimmed[1..trimmed.len() - 1].trim()
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

    #[test]
    fn tilde_expands_to_home_and_tilde_user_stays() {
        let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
            return;
        };
        let home = PathBuf::from(home);
        assert_eq!(expand_tilde(Path::new("~")), home);
        assert_eq!(expand_tilde(Path::new("~/a/b")), home.join("a/b"));
        assert_eq!(expand_tilde(Path::new("~foo")), PathBuf::from("~foo"));
        assert_eq!(expand_tilde(Path::new("/tmp/x")), PathBuf::from("/tmp/x"));
    }

    #[test]
    fn unquote_and_trim_cleans_plain_quoted_and_spaced_values() {
        assert_eq!(unquote_and_trim("/opt/kage"), "/opt/kage");
        assert_eq!(unquote_and_trim("  /opt/my dir  "), "/opt/my dir");
        assert_eq!(unquote_and_trim("\"/opt/my dir\""), "/opt/my dir");
        assert_eq!(unquote_and_trim("'/opt/my dir'"), "/opt/my dir");
        assert_eq!(unquote_and_trim(" \"a b\" "), "a b");
        assert_eq!(unquote_and_trim("'a b'"), "a b");
    }

    #[test]
    fn unquote_and_trim_keeps_unmatched_mismatched_and_interior_quotes() {
        assert_eq!(unquote_and_trim("\"a b"), "\"a b");
        assert_eq!(unquote_and_trim("\"mismatched'"), "\"mismatched'");
        assert_eq!(unquote_and_trim("'a b\""), "'a b\"");
        assert_eq!(unquote_and_trim("\"quoted 'inside'\""), "quoted 'inside'");
    }

    #[test]
    fn unquote_and_trim_handles_unicode_and_empty_inputs() {
        assert_eq!(unquote_and_trim("  \"café.png\"  "), "café.png");
        assert_eq!(unquote_and_trim(""), "");
        assert_eq!(unquote_and_trim("   "), "");
        assert_eq!(unquote_and_trim("\"\""), "");
        assert_eq!(unquote_and_trim("''"), "");
        assert_eq!(unquote_and_trim("'"), "'");
        assert_eq!(unquote_and_trim("\""), "\"");
    }

    #[test]
    fn unquote_and_trim_feeds_tilde_expansion() {
        let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
            return;
        };
        let home = PathBuf::from(home);
        assert_eq!(unquote_and_trim(" ~/x "), "~/x");
        assert_eq!(
            expand_tilde(Path::new(unquote_and_trim(" \"~/x\" "))),
            home.join("x")
        );
        assert_eq!(expand_tilde(Path::new(unquote_and_trim("~"))), home);
        assert_eq!(
            expand_tilde(Path::new(unquote_and_trim("~user/x"))),
            PathBuf::from("~user/x")
        );
    }
}
