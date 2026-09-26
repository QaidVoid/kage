//! Path resolution for tools and the plugin filesystem helpers.
//!
//! Two entry points, sharing canonicalization machinery:
//!
//! - [`resolve`] normalizes a candidate path against `workdir` and
//!   canonicalizes it (resolving symlinks; preserving any non-existent
//!   tail). `.` and `..` are resolved lexically before any filesystem
//!   interaction, and a traversal that leaves the workdir is returned
//!   as-is. Built-in tools call this - the user already chose the
//!   workdir, and `bash` can reach anywhere on the filesystem anyway,
//!   so a tool-side sandbox is friction without security.
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
/// - `.` and `..` components are resolved lexically (the way the kernel
///   resolves them for directory lookup) before any filesystem
///   interaction, so `a/../b` resolves identically whether or not `a`
///   exists, and `/..` clamps at the root.
/// - The longest existing ancestor of the result is canonicalized
///   (resolving symlinks); any unresolved tail (for paths that do not
///   yet exist, e.g. the target of a `write` tool) is appended verbatim.
/// - No escape check: a traversal that leaves the workdir is returned
///   as-is. Built-in tools call this; see [`resolve_under`] for the
///   confined variant.
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

    canonicalize_with_missing_tail(&normalize(&absolute))
}

/// Resolve `candidate` against `workdir` and refuse anything that
/// escapes the canonical workdir: via `..` (normalized lexically before
/// resolution, so it is caught whether or not the intermediate
/// components exist), an absolute path outside, or a symlink in the
/// existing ancestor chain that resolves outside.
///
/// Components that do not exist yet are only checked lexically; a
/// symlink planted in that unresolved tail is invisible here. Callers
/// that create parent directories must re-verify containment
/// afterwards (see the plugin fs helpers).
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

/// Collapse `.` and `..` components lexically, matching how the kernel
/// resolves them for directory lookup: `..` pops the previous normal
/// component, and traversal past the root clamps at the root (`/..`
/// stays at `/`).
///
/// The input must be absolute (a root to anchor the traversal at, as
/// [`resolve`] guarantees). Symlinks are not consulted; the caller
/// canonicalizes the existing ancestor chain separately.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.file_name().is_some() {
                    out.pop();
                }
            }
            component => out.push(component),
        }
    }
    out
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
    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[test]
    fn resolve_under_dot_dot_over_missing_component_is_rejected_cleanly() {
        let dir = workdir();
        // `a` does not exist; the traversal still resolves outside and
        // must be rejected with the escape error, not a resolution
        // failure.
        match resolve_under(dir.path(), Path::new("a/../../evil")) {
            Err(ToolError::Path { reason, .. }) => {
                assert!(reason.contains("escapes workdir"), "got {reason:?}");
            }
            other => panic!("expected path error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_under_dot_dot_over_missing_component_staying_inside_resolves() {
        let dir = workdir();
        let resolved = resolve_under(dir.path(), Path::new("missing/../ok.txt")).unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap().join("ok.txt"));
    }

    #[test]
    fn resolve_traversal_past_workdir_via_missing_component_is_returned() {
        let dir = workdir();
        let outside = dir.path().parent().unwrap();
        let resolved = resolve(dir.path(), Path::new("a/../../x")).unwrap();
        assert_eq!(resolved, outside.canonicalize().unwrap().join("x"));
    }

    #[test]
    fn resolve_absolute_traversal_clamps_at_root() {
        let dir = workdir();
        let resolved = resolve(dir.path(), Path::new("/../")).unwrap();
        assert_eq!(resolved, Path::new("/").canonicalize().unwrap());
    }

    #[test]
    fn resolve_under_absolute_traversal_is_rejected() {
        let dir = workdir();
        let err = resolve_under(dir.path(), Path::new("/../etc/passwd")).unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }

    #[test]
    fn resolve_normalizes_intermediate_dot_components() {
        let dir = workdir();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let resolved = resolve(dir.path(), Path::new("./sub/../new.txt")).unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap().join("new.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_under_dangling_symlink_tail_is_not_resolved_through() {
        // A dangling symlink inside the workdir cannot be canonicalized,
        // so it stays in the unresolved tail and the path still verifies
        // as inside. Containment for writes through such a tail is
        // enforced by the plugin fs helpers after directory creation.
        let dir = workdir();
        symlink("../escape-target", dir.path().join("dangling")).unwrap();
        let resolved = resolve_under(dir.path(), Path::new("dangling/x")).unwrap();
        assert_eq!(
            resolved,
            dir.path()
                .canonicalize()
                .unwrap()
                .join("dangling")
                .join("x")
        );
        assert!(!dir.path().join("../escape-target").exists());
    }
}
