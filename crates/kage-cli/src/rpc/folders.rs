//! `_kage/folders`: the folders inside one folder on the machine kage
//! runs on, so a client can browse to the folder a session opens in.
//! Only folder names are listed, never file contents.

use std::path::{Path, PathBuf};

use kage_acp::acp::{FoldersRequest, FoldersResult};
use kage_jsonrpc::RpcError;

/// Most folders one answer lists before it reports `truncated`.
const MAX_FOLDERS: usize = 2_000;

/// Lists the folders inside the folder `req` names: the home folder
/// when it names none, with `~` standing for home. Hidden folders are
/// left out, and the names sort case-insensitively.
///
/// # Errors
///
/// Returns an [`RpcError`] when the folder does not exist or cannot be
/// read.
pub(crate) fn folders(req: &FoldersRequest) -> Result<FoldersResult, RpcError> {
    let home = dirs::home_dir();
    let asked = req.path.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let path = match (asked, &home) {
        (None, Some(home)) => home.clone(),
        (None, None) => PathBuf::from("/"),
        (Some(path), Some(home)) if path == "~" || path.starts_with("~/") => {
            home.join(path.trim_start_matches('~').trim_start_matches('/'))
        }
        (Some(path), _) => PathBuf::from(path),
    };
    let path = path
        .canonicalize()
        .map_err(|e| RpcError::new(-32602, format!("{}: {e}", path.display())))?;
    let entries = std::fs::read_dir(&path)
        .map_err(|e| RpcError::new(-32602, format!("{}: {e}", path.display())))?;
    let mut folders: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    folders.sort_by_key(|name| name.to_lowercase());
    let truncated = folders.len() > MAX_FOLDERS;
    folders.truncate(MAX_FOLDERS);
    Ok(FoldersResult {
        parent: path.parent().map(display),
        home: home.as_deref().map(display),
        path: display(&path),
        folders,
        truncated,
    })
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_visible_folders_sorted_with_the_parent() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["beta", "Alpha", ".hidden"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        std::fs::write(dir.path().join("file.txt"), "").unwrap();
        let listed = folders(&FoldersRequest {
            path: Some(dir.path().display().to_string()),
        })
        .unwrap();
        assert_eq!(listed.folders, ["Alpha", "beta"]);
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(listed.path, root.display().to_string());
        assert_eq!(
            listed.parent,
            root.parent().map(|p| p.display().to_string())
        );
        assert!(!listed.truncated);
    }

    #[test]
    fn a_missing_folder_is_an_error() {
        let err = folders(&FoldersRequest {
            path: Some("/no/such/kage/folder".into()),
        })
        .unwrap_err();
        assert!(
            err.message.contains("/no/such/kage/folder"),
            "{}",
            err.message
        );
    }
}
