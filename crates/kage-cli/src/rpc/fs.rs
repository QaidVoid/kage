//! The confined file operations behind `_kage/fs`.
//!
//! Every path resolves under the session workdir through
//! [`resolve_under`], so traversal, absolute escapes and symlink
//! escapes are refused before anything is touched. Reads are capped
//! and refuse files whose configured `read` deny globs match.

use std::fs;
use std::io::Read as _;
use std::path::Path;

use kage_acp::acp::{FsEntry, FsKind, FsListResult, FsOp, FsReadResult, FsRequest, FsResult};
use kage_core::permissions::{PermissionAction, PermissionsConfig};
use kage_jsonrpc::RpcError;
use kage_tools::resolve_under;

/// Entries one list returns before it reports `truncated`.
const MAX_ENTRIES: usize = 2_000;
/// Deepest level a list descends to, relative to the asked root.
const MAX_DEPTH: usize = 8;
/// Most bytes one read returns.
const READ_CAP: usize = 512 * 1024;

/// Runs `_kage/fs` for one session: `list` or `read`, confined to
/// `workdir`. A path that escapes the workdir and a read whose
/// configured `read` deny glob matches are both refused.
///
/// # Errors
///
/// Returns an [`RpcError`] when the path escapes the workdir, the read
/// is denied, or the file cannot be read.
pub(crate) fn handle(
    workdir: &Path,
    permissions: &PermissionsConfig,
    req: &FsRequest,
) -> Result<FsResult, RpcError> {
    let workdir = workdir
        .canonicalize()
        .map_err(|e| RpcError::new(-32602, format!("workdir {}: {e}", workdir.display())))?;
    let relative = if req.path.is_empty() { "." } else { &req.path };
    let target = resolve_under(&workdir, Path::new(relative))
        .map_err(|e| RpcError::new(-32602, e.to_string()))?;
    match req.op {
        FsOp::List => Ok(FsResult::List(list(&workdir, &target))),
        FsOp::Read => read(permissions, &target).map(FsResult::Read),
    }
}

/// Lists `root` as a subtree of `workdir`, parents directly before
/// their children, sorted by name, until the entry or depth cap cuts
/// it short.
fn list(workdir: &Path, root: &Path) -> FsListResult {
    let mut result = FsListResult::default();
    walk(workdir, root, 0, &mut result);
    result
}

fn walk(workdir: &Path, dir: &Path, depth: usize, result: &mut FsListResult) {
    if result.truncated {
        return;
    }
    let Ok(children) = fs::read_dir(dir) else {
        result.truncated = true;
        return;
    };
    let mut children: Vec<_> = children.filter_map(std::result::Result::ok).collect();
    children.sort_by_key(std::fs::DirEntry::file_name);
    for entry in children {
        if result.entries.len() >= MAX_ENTRIES || depth >= MAX_DEPTH {
            result.truncated = true;
            return;
        }
        let Ok(file_type) = entry.file_type() else {
            result.truncated = true;
            return;
        };
        let kind = if file_type.is_dir() {
            FsKind::Directory
        } else if file_type.is_file() {
            FsKind::File
        } else {
            FsKind::Other
        };
        let size = if kind == FsKind::File {
            entry.metadata().map_or(0, |meta| meta.len())
        } else {
            0
        };
        let entry_path = entry.path();
        let Ok(path) = entry_path.strip_prefix(workdir) else {
            result.truncated = true;
            return;
        };
        result.entries.push(FsEntry {
            path: path.display().to_string(),
            kind,
            size,
        });
        if kind == FsKind::Directory {
            walk(workdir, &entry.path(), depth + 1, result);
        }
    }
}

/// Reads `file` as UTF-8 text capped at [`READ_CAP`] bytes. A file
/// that is not valid UTF-8 comes back as a `binary` marker with no
/// content; a cut inside a trailing multi-byte character is trimmed
/// instead of calling the file binary.
fn read(permissions: &PermissionsConfig, file: &Path) -> Result<FsReadResult, RpcError> {
    let subject = file.display().to_string();
    if permissions.check("read", &subject) == PermissionAction::Deny {
        return Err(RpcError::new(
            -32602,
            format!("denied by permissions: {subject}"),
        ));
    }
    let mut bytes = Vec::new();
    let taken = fs::File::open(file)
        .map_err(|e| RpcError::new(-32602, format!("{}: {e}", file.display())))?
        .take(READ_CAP as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| RpcError::new(-32602, format!("{}: {e}", file.display())))?;
    let mut truncated = false;
    if taken >= READ_CAP {
        truncated = read_len(file)? > taken as u64;
    }
    let (content, binary) = match String::from_utf8(bytes) {
        Ok(text) => (text, false),
        Err(err) => {
            let valid = err.utf8_error().valid_up_to();
            let bytes = err.into_bytes();
            if truncated && bytes.len() - valid <= 3 {
                (String::from_utf8_lossy(&bytes[..valid]).into_owned(), false)
            } else {
                (String::new(), true)
            }
        }
    };
    Ok(FsReadResult {
        content,
        truncated,
        binary,
    })
}

/// The full size of `file` in bytes.
fn read_len(file: &Path) -> Result<u64, RpcError> {
    fs::metadata(file)
        .map(|meta| meta.len())
        .map_err(|e| RpcError::new(-32602, format!("{}: {e}", file.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kage_acp::acp::FsOp;
    use kage_core::permissions::ToolPermissionRules;

    fn request(op: FsOp, path: &str) -> FsRequest {
        FsRequest {
            session_id: "s1".into(),
            op,
            path: path.into(),
        }
    }

    fn deny_read(paths: &[&str]) -> PermissionsConfig {
        let mut permissions = PermissionsConfig::default();
        permissions.tools.insert(
            "read".into(),
            ToolPermissionRules {
                default: PermissionAction::Allow,
                allow: Vec::new(),
                deny: paths.iter().map(|p| (*p).to_owned()).collect(),
            },
        );
        permissions
    }

    #[test]
    fn list_walks_the_tree_parents_before_children() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        fs::write(dir.path().join("src/b.rs"), "b").unwrap();
        fs::write(dir.path().join("src/deep/c.txt"), "c").unwrap();
        fs::write(dir.path().join("a.md"), "a").unwrap();

        let out = handle(
            dir.path(),
            &PermissionsConfig::default(),
            &request(FsOp::List, ""),
        )
        .unwrap();
        let FsResult::List(list) = out else {
            panic!("expected a list result");
        };
        assert!(!list.truncated);
        let paths: Vec<&str> = list.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["a.md", "src", "src/b.rs", "src/deep", "src/deep/c.txt"]
        );
        let src = &list.entries[1];
        assert_eq!(src.kind, FsKind::Directory);
        assert_eq!(src.size, 0);
        assert_eq!(list.entries[2].size, 1);
    }

    #[test]
    fn list_stops_at_the_entry_cap_and_reports_truncated() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_ENTRIES + 10) {
            fs::write(dir.path().join(format!("f{i:05}")), "x").unwrap();
        }
        let out = handle(
            dir.path(),
            &PermissionsConfig::default(),
            &request(FsOp::List, ""),
        )
        .unwrap();
        let FsResult::List(list) = out else {
            panic!("expected a list result");
        };
        assert_eq!(list.entries.len(), MAX_ENTRIES);
        assert!(list.truncated);
    }

    #[test]
    fn list_stops_at_the_depth_cap_instead_of_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for i in 0..40 {
            deep = deep.join(format!("d{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        let out = handle(
            dir.path(),
            &PermissionsConfig::default(),
            &request(FsOp::List, ""),
        )
        .unwrap();
        let FsResult::List(list) = out else {
            panic!("expected a list result");
        };
        assert!(list.truncated);
        assert_eq!(list.entries.len(), MAX_DEPTH);
    }

    #[test]
    fn read_returns_text_and_the_truncation_flag() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.txt");
        fs::write(&small, "hello").unwrap();
        let big = dir.path().join("big.txt");
        fs::write(&big, "x".repeat(READ_CAP + 10)).unwrap();

        let out = handle(
            dir.path(),
            &PermissionsConfig::default(),
            &request(FsOp::Read, "small.txt"),
        )
        .unwrap();
        let FsResult::Read(file) = out else {
            panic!("expected a read result");
        };
        assert_eq!(file.content, "hello");
        assert!(!file.truncated && !file.binary);

        let out = handle(
            dir.path(),
            &PermissionsConfig::default(),
            &request(FsOp::Read, "big.txt"),
        )
        .unwrap();
        let FsResult::Read(file) = out else {
            panic!("expected a read result");
        };
        assert_eq!(file.content.len(), READ_CAP);
        assert!(file.truncated && !file.binary);
    }

    #[test]
    fn read_reports_binary_files_without_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let out = handle(
            dir.path(),
            &PermissionsConfig::default(),
            &request(FsOp::Read, "blob.bin"),
        )
        .unwrap();
        let FsResult::Read(file) = out else {
            panic!("expected a read result");
        };
        assert!(file.binary);
        assert!(file.content.is_empty());
        assert!(!file.truncated);
    }

    #[test]
    fn read_trims_a_cut_character_instead_of_calling_the_file_binary() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!("{}{}", "x".repeat(READ_CAP - 1), "\u{00e9}");
        fs::write(dir.path().join("utf.txt"), &text).unwrap();
        let out = handle(
            dir.path(),
            &PermissionsConfig::default(),
            &request(FsOp::Read, "utf.txt"),
        )
        .unwrap();
        let FsResult::Read(file) = out else {
            panic!("expected a read result");
        };
        assert!(file.truncated);
        assert!(!file.binary);
        assert_eq!(file.content.len(), READ_CAP - 1);
    }

    #[test]
    fn a_deny_glob_refuses_the_read() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("secrets")).unwrap();
        fs::write(dir.path().join("secrets/key.txt"), "k").unwrap();
        let permissions = deny_read(&["**/secrets/**"]);
        let out = handle(
            dir.path(),
            &permissions,
            &request(FsOp::Read, "secrets/key.txt"),
        );
        let err = out.unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("denied by permissions"));
    }

    #[test]
    fn a_path_outside_the_workdir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("secret.txt");
        fs::write(&file, "s").unwrap();
        let err = handle(
            dir.path(),
            &PermissionsConfig::default(),
            &request(FsOp::Read, &file.display().to_string()),
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("escapes workdir"));
    }

    #[test]
    fn dot_dot_traversal_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = handle(
            dir.path(),
            &PermissionsConfig::default(),
            &request(FsOp::List, "../.."),
        )
        .unwrap_err();
        assert!(err.message.contains("escapes workdir"));
    }

    #[test]
    fn an_empty_path_lists_the_workdir_itself() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("only.txt"), "o").unwrap();
        let out = handle(
            dir.path(),
            &PermissionsConfig::default(),
            &request(FsOp::List, ""),
        )
        .unwrap();
        let FsResult::List(list) = out else {
            panic!("expected a list result");
        };
        assert_eq!(list.entries[0].path, "only.txt");
    }
}
