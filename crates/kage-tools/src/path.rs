//! Path resolution for tools and the plugin filesystem helpers.
//!
//! Two entry points, sharing canonicalization machinery:
//!
//! - [`resolve`] normalizes a candidate path against `workdir` and
//!   canonicalizes it (resolving symlinks; preserving any non-existent
//!   tail). No escape check: an absolute path or `..` traversal that
//!   leaves the workdir is returned as-is. Built-in tools call this -
//!   the user already chose the workdir, and `bash` can reach anywhere
//!   on the filesystem anyway, so a tool-side sandbox is friction
//!   without security.
//! - [`resolve_under`] wraps [`resolve`] with a `starts_with(workdir)`
//!   check, returning [`ToolError::Path`] on escape. Used by the
//!   plugin filesystem helpers (`kage.fs.read` / `kage.fs.write`)
//!   because Lua plugins are third-party code running inside a
//!   sandbox; their fs reach should not exceed the workdir.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::ToolError;

/// Resolve `candidate` against `workdir` without an escape check.
///
/// Behavior:
/// - Relative `candidate` is joined onto `workdir`.
/// - Absolute `candidate` is taken as-is.
/// - The longest existing ancestor of the result is canonicalized
///   (resolving symlinks); any unresolved tail (for paths that do not
///   yet exist, e.g. the target of a `write` tool) is appended verbatim.
///
/// # Errors
///
/// - `workdir` does not exist or is not canonicalizable.
/// - The candidate has no existing ancestor at all (every component up
///   to the root is missing).
pub fn resolve(workdir: &Path, candidate: &Path) -> Result<PathBuf, ToolError> {
    let canonical_root = workdir.canonicalize().map_err(|e| ToolError::Path {
        path: workdir.to_owned(),
        reason: format!("canonicalize workdir: {e}"),
    })?;

    let absolute = if candidate.is_absolute() {
        candidate.to_owned()
    } else {
        canonical_root.join(candidate)
    };

    canonicalize_with_missing_tail(&absolute)
}

/// Resolve `candidate` against `workdir` and refuse anything that
/// escapes (via `..`, an absolute path outside, or a symlink that
/// points outside).
///
/// Used by the plugin filesystem helpers; tools call [`resolve`]
/// instead.
///
/// # Errors
///
/// - Any error from [`resolve`].
/// - The resolved path falls outside the canonical workdir.
pub fn resolve_under(workdir: &Path, candidate: &Path) -> Result<PathBuf, ToolError> {
    let canonical_root = workdir.canonicalize().map_err(|e| ToolError::Path {
        path: workdir.to_owned(),
        reason: format!("canonicalize workdir: {e}"),
    })?;
    let resolved = resolve(workdir, candidate)?;
    if !resolved.starts_with(&canonical_root) {
        return Err(ToolError::Path {
            path: candidate.to_owned(),
            reason: format!("escapes workdir {}", canonical_root.display()),
        });
    }
    Ok(resolved)
}

/// Walk back through `path`'s ancestors until one exists, canonicalize it,
/// then re-attach the unresolved tail.
fn canonicalize_with_missing_tail(path: &Path) -> Result<PathBuf, ToolError> {
    let mut tail: Vec<OsString> = Vec::new();
    let mut current = path.to_owned();

    loop {
        if current.exists() {
            let canonical = current.canonicalize().map_err(|e| ToolError::Path {
                path: path.to_owned(),
                reason: format!("canonicalize: {e}"),
            })?;
            let mut result = canonical;
            for component in tail.iter().rev() {
                result.push(component);
            }
            return Ok(result);
        }
        let Some(name) = current.file_name() else {
            return Err(ToolError::Path {
                path: path.to_owned(),
                reason: "no existing ancestor".into(),
            });
        };
        tail.push(name.to_owned());
        if !current.pop() {
            return Err(ToolError::Path {
                path: path.to_owned(),
                reason: "no existing ancestor".into(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    fn workdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn resolve_relative_existing_file() {
        let dir = workdir();
        let file = dir.path().join("hello.txt");
        fs::write(&file, b"x").unwrap();
        let resolved = resolve(dir.path(), Path::new("hello.txt")).unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn resolve_relative_new_file_under_workdir() {
        let dir = workdir();
        let resolved = resolve(dir.path(), Path::new("new.txt")).unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap().join("new.txt"));
    }

    #[test]
    fn resolve_relative_in_existing_subdir() {
        let dir = workdir();
        let subdir = dir.path().join("sub");
        fs::create_dir(&subdir).unwrap();
        let resolved = resolve(dir.path(), Path::new("sub/new.txt")).unwrap();
        assert_eq!(resolved, subdir.canonicalize().unwrap().join("new.txt"));
    }

    #[test]
    fn resolve_accepts_dot_dot_escape() {
        let dir = workdir();
        let outside = dir.path().parent().unwrap();
        let resolved = resolve(dir.path(), Path::new("../")).unwrap();
        assert_eq!(resolved, outside.canonicalize().unwrap());
    }

    #[test]
    fn resolve_accepts_absolute_outside_workdir() {
        let dir = workdir();
        let resolved = resolve(dir.path(), Path::new("/")).unwrap();
        assert_eq!(resolved, Path::new("/").canonicalize().unwrap());
    }

    #[test]
    fn resolve_workdir_itself() {
        let dir = workdir();
        let resolved = resolve(dir.path(), Path::new(".")).unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn resolve_nonexistent_workdir_fails() {
        let err = resolve(
            Path::new("/this/path/does/not/exist/anywhere"),
            Path::new("x"),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }

    #[test]
    fn resolve_under_dot_dot_traversal_is_rejected() {
        let dir = workdir();
        let err = resolve_under(dir.path(), Path::new("../escape")).unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }), "got {err:?}");
    }

    #[test]
    fn resolve_under_deep_dot_dot_traversal_is_rejected() {
        let dir = workdir();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let err = resolve_under(dir.path(), Path::new("sub/../../etc/passwd")).unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }

    #[test]
    fn resolve_under_absolute_outside_workdir_is_rejected() {
        let dir = workdir();
        let err = resolve_under(dir.path(), Path::new("/etc/passwd")).unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }

    #[test]
    fn resolve_under_absolute_inside_workdir_is_accepted() {
        let dir = workdir();
        let file = dir.path().join("inside.txt");
        fs::write(&file, b"x").unwrap();
        let resolved = resolve_under(dir.path(), &file).unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn resolve_under_symlink_that_escapes_is_rejected() {
        let outer = workdir();
        let inner = workdir();
        let target = outer.path().join("secret");
        fs::write(&target, b"shh").unwrap();
        symlink(&target, inner.path().join("escape")).unwrap();
        let err = resolve_under(inner.path(), Path::new("escape")).unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }
}
