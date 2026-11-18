//! Path resolution for tools and the plugin filesystem helpers.
//!
//! Three entry points, sharing canonicalization machinery:
//!
//! - [`resolve`] normalizes a candidate path against `workdir` and
//!   canonicalizes it (resolving symlinks; preserving any non-existent
//!   tail). `.` and `..` are resolved lexically before any filesystem
//!   interaction, and a traversal that leaves the workdir is returned
//!   as-is. A leading `~` expands to the home directory.
//! - [`resolve_reachable`] is what the built-in file tools use: it
//!   wraps [`resolve`] and refuses a relative path that leaves the
//!   workdir, so reaching outside takes an absolute or `~` path.
//! - [`resolve_under`] wraps [`resolve`] with a `starts_with(workdir)`
//!   check, returning [`ToolError::Path`] on escape. Used by the
//!   plugin filesystem helpers (`kage.fs.read` / `kage.fs.write`)
//!   because Lua plugins are third-party code running inside a
//!   sandbox, and by agents working in their own worktree.
//!
//! [`is_sensitive`] names the files holding secrets (`.env`, SSH
//! private keys, cloud credentials) that the file tools refuse.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::ToolError;

/// Resolve `candidate` against `workdir` without an escape check.
///
/// Behavior:
/// - A leading `~` expands to the home directory before the
///   relative/absolute decision: `~` is home itself and `~/...` is
///   home joined with the rest, like a shell's unquoted word. On Unix
///   `~user` takes that user's home (see
///   [`kage_core::fsutil::expand_tilde`]). Environment variables are
///   left untouched; the `bash` tool's shell handles those.
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
///   as-is. See [`resolve_reachable`] and [`resolve_under`] for the
///   checked variants.
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

    let candidate = kage_core::fsutil::expand_tilde(candidate);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        canonical_root.join(candidate)
    };

    canonicalize_with_missing_tail(&normalize(&absolute))
}

/// Resolve `candidate` against `workdir` like [`resolve`], refusing a
/// relative path that leaves the workdir. An absolute or `~` path may
/// point anywhere: naming it is the deliberate step a stray `../`
/// is not.
///
/// Whether a relative path leaves is decided from its own components,
/// not from where symlinks lead, so `shared/notes.md` is reachable
/// even when `shared` links outside the workdir.
///
/// # Errors
///
/// - Any error from [`resolve`].
/// - A relative candidate whose `..` components climb out of the
///   workdir.
pub fn resolve_reachable(workdir: &Path, candidate: &Path) -> Result<PathBuf, ToolError> {
    let resolved = resolve(workdir, candidate)?;
    let expanded = kage_core::fsutil::expand_tilde(candidate);
    if expanded.is_absolute() {
        return Ok(resolved);
    }
    let canonical_root = workdir.canonicalize().map_err(|e| ToolError::Path {
        path: workdir.to_owned(),
        reason: format!("canonicalize workdir: {e}"),
    })?;
    if !normalize(&canonical_root.join(&expanded)).starts_with(&canonical_root) {
        return Err(ToolError::Path {
            path: candidate.to_owned(),
            reason: format!(
                "leaves the working directory {}; use an absolute path to reach outside it",
                canonical_root.display()
            ),
        });
    }
    Ok(resolved)
}

/// Basenames that hold secrets whatever directory they are in.
const SENSITIVE_NAMES: &[&str] = &[".env", "id_rsa", "id_ed25519", "id_ecdsa", "credentials"];

/// Templates of `.env` files, which hold no secrets by convention.
const ENV_TEMPLATES: &[&str] = &[".env.example", ".env.sample", ".env.template"];

/// Suffixes of copies of a private key that still hold it.
const KEY_COPY_SUFFIXES: &[&str] = &[
    ".bak",
    ".backup",
    ".copy",
    ".disabled",
    ".key",
    ".old",
    ".orig",
    ".pem",
    ".save",
    ".tmp",
];

/// Whether `path` names a file that holds secrets: `.env` and its
/// variants other than templates, an SSH private key or a copy of one,
/// or a `credentials` file such as `~/.aws/credentials`. Public keys
/// (`id_ed25519.pub`) are not secrets. Compared case-insensitively.
#[must_use]
pub fn is_sensitive(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy().to_lowercase();
    if ENV_TEMPLATES.contains(&name.as_str())
        || path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pub"))
    {
        return false;
    }
    if SENSITIVE_NAMES.contains(&name.as_str()) || name.starts_with(".env.") {
        return true;
    }
    ["id_rsa", "id_ed25519", "id_ecdsa", "credentials"]
        .iter()
        .filter_map(|prefix| name.strip_prefix(prefix))
        .any(|rest| {
            rest.starts_with(['-', '_'])
                || (rest.starts_with('.') && KEY_COPY_SUFFIXES.contains(&rest))
        })
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
/// Used by the plugin filesystem helpers and by agents in a worktree;
/// the other tools call [`resolve_reachable`].
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

    /// The root `/` means from `dir`: on Windows, the root of its drive.
    fn root_of(dir: &Path) -> PathBuf {
        dir.ancestors().last().unwrap().canonicalize().unwrap()
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
        assert_eq!(resolved, root_of(dir.path()));
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
    fn resolve_expands_leading_tilde_to_home() {
        let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
            return;
        };
        let dir = workdir();
        let resolved = resolve(dir.path(), Path::new("~/new.txt")).unwrap();
        assert_eq!(
            resolved,
            Path::new(&home).canonicalize().unwrap().join("new.txt")
        );
    }

    #[test]
    fn resolve_expands_bare_tilde_to_home() {
        let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
            return;
        };
        let dir = workdir();
        let resolved = resolve(dir.path(), Path::new("~")).unwrap();
        assert_eq!(resolved, Path::new(&home).canonicalize().unwrap());
    }

    #[test]
    fn resolve_leaves_tilde_names_of_no_user_alone() {
        let dir = workdir();
        let resolved = resolve(dir.path(), Path::new("~kage-no-such-user")).unwrap();
        assert_eq!(
            resolved,
            dir.path()
                .canonicalize()
                .unwrap()
                .join("~kage-no-such-user")
        );
    }

    #[test]
    fn resolve_under_expanded_tilde_outside_workdir_is_rejected() {
        let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
            return;
        };
        if Path::new(&home).starts_with(std::env::temp_dir()) {
            return;
        }
        let dir = workdir();
        let err = resolve_under(dir.path(), Path::new("~/x")).unwrap_err();
        assert!(
            matches!(err, ToolError::Path { ref reason, .. } if reason.contains("escapes workdir"))
        );
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
        assert_eq!(resolved, root_of(dir.path()));
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

    #[test]
    fn resolve_reachable_takes_absolute_and_tilde_paths_but_not_a_relative_escape() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let err = resolve_reachable(&work, Path::new("../x")).unwrap_err();
        assert!(err.to_string().contains("use an absolute path"), "{err}");
        let outside = root.path().join("x");
        assert_eq!(
            resolve_reachable(&work, &outside).unwrap(),
            root.path().canonicalize().unwrap().join("x")
        );
        assert!(resolve_reachable(&work, Path::new("~")).is_ok());
        assert_eq!(
            resolve_reachable(&work, Path::new("sub/../y")).unwrap(),
            work.canonicalize().unwrap().join("y")
        );
    }

    #[test]
    fn is_sensitive_names_secrets_but_not_templates_or_public_keys() {
        for secret in [
            ".env",
            ".env.local",
            "/home/u/.ssh/id_ed25519",
            "id_rsa.bak",
            "id_rsa-old",
            "/home/u/.aws/credentials",
            "credentials_prod",
            ".ENV",
        ] {
            assert!(is_sensitive(Path::new(secret)), "{secret}");
        }
        for plain in [
            ".env.example",
            ".envrc",
            "id_ed25519.pub",
            "credentials.rs",
            "id_rsa.txt",
            "src/main.rs",
        ] {
            assert!(!is_sensitive(Path::new(plain)), "{plain}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_reachable_follows_a_link_inside_the_workdir_that_leads_out() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let shared = root.path().join("shared");
        std::fs::create_dir(&work).unwrap();
        std::fs::create_dir(&shared).unwrap();
        std::os::unix::fs::symlink(&shared, work.join("shared")).unwrap();
        assert_eq!(
            resolve_reachable(&work, Path::new("shared/notes.md")).unwrap(),
            shared.canonicalize().unwrap().join("notes.md")
        );
        assert!(resolve_reachable(&work, Path::new("shared/../../x")).is_err());
    }
}
