//! Workdir-scoped path resolution.
//!
//! Tools that touch the filesystem must funnel candidate paths through
//! [`resolve_under`] before opening anything. The resolver canonicalizes
//! both `workdir` and the longest existing prefix of the candidate so that
//! symlink and `..` traversal can never escape the agent's working root.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::ToolError;

/// Resolve `candidate` against `workdir`, rejecting any path that escapes.
///
/// Behavior:
/// - Relative `candidate` is joined onto `workdir`.
/// - Absolute `candidate` is taken as-is, then verified to fall under `workdir`.
/// - The longest existing ancestor of the result is canonicalized (resolving
///   symlinks); any unresolved tail (for paths that do not yet exist, e.g.
///   the target of a `write` tool) is appended verbatim.
/// - The final assembled path must `starts_with` the canonical workdir.
///   Anything else returns [`ToolError::Path`].
///
/// # Errors
///
/// - `workdir` does not exist or is not canonicalizable.
/// - `candidate` resolves outside `workdir` (traversal, escape, symlink).
/// - The candidate has no existing ancestor at all (every component up to
///   the root is missing).
pub fn resolve_under(workdir: &Path, candidate: &Path) -> Result<PathBuf, ToolError> {
    let canonical_root = workdir.canonicalize().map_err(|e| ToolError::Path {
        path: workdir.to_owned(),
        reason: format!("canonicalize workdir: {e}"),
    })?;

    let absolute = if candidate.is_absolute() {
        candidate.to_owned()
    } else {
        canonical_root.join(candidate)
    };

    let resolved = canonicalize_with_missing_tail(&absolute)?;

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
    fn relative_path_to_existing_file_resolves() {
        let dir = workdir();
        let file = dir.path().join("hello.txt");
        fs::write(&file, b"x").unwrap();
        let resolved = resolve_under(dir.path(), Path::new("hello.txt")).unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn relative_path_to_new_file_resolves_under_workdir() {
        let dir = workdir();
        let resolved = resolve_under(dir.path(), Path::new("new.txt")).unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap().join("new.txt"));
    }

    #[test]
    fn relative_path_in_existing_subdir_resolves() {
        let dir = workdir();
        let subdir = dir.path().join("sub");
        fs::create_dir(&subdir).unwrap();
        let resolved = resolve_under(dir.path(), Path::new("sub/new.txt")).unwrap();
        assert_eq!(resolved, subdir.canonicalize().unwrap().join("new.txt"));
    }

    #[test]
    fn dot_dot_traversal_is_rejected() {
        let dir = workdir();
        let err = resolve_under(dir.path(), Path::new("../escape")).unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }), "got {err:?}");
    }

    #[test]
    fn deep_dot_dot_traversal_is_rejected() {
        let dir = workdir();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let err = resolve_under(dir.path(), Path::new("sub/../../etc/passwd")).unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }

    #[test]
    fn absolute_path_outside_workdir_is_rejected() {
        let dir = workdir();
        let err = resolve_under(dir.path(), Path::new("/etc/passwd")).unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }

    #[test]
    fn absolute_path_inside_workdir_is_accepted() {
        let dir = workdir();
        let file = dir.path().join("inside.txt");
        fs::write(&file, b"x").unwrap();
        let resolved = resolve_under(dir.path(), &file).unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn symlink_that_escapes_is_rejected() {
        let outer = workdir();
        let inner = workdir();
        // inner/escape -> outer/secret
        let target = outer.path().join("secret");
        fs::write(&target, b"shh").unwrap();
        symlink(&target, inner.path().join("escape")).unwrap();
        let err = resolve_under(inner.path(), Path::new("escape")).unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }

    #[test]
    fn workdir_itself_resolves_cleanly() {
        let dir = workdir();
        let resolved = resolve_under(dir.path(), Path::new(".")).unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn nonexistent_workdir_fails_clearly() {
        let err = resolve_under(
            Path::new("/this/path/does/not/exist/anywhere"),
            Path::new("x"),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }
}
