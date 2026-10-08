//! `_kage/folders`: the folders inside one folder on the machine kage
//! runs on, so a client can browse to the folder a session opens in.
//! Only folder names are listed, never file contents.

use std::path::{Path, PathBuf};

use kage_acp::acp::{FoldersRequest, FoldersResult};
use kage_jsonrpc::RpcError;

/// Most folders one answer lists before it reports `truncated`.
const MAX_FOLDERS: usize = 2_000;

/// Lists the folders inside the folder `req` names: the home folder
/// when it names none, with `~` standing for home (a `\` separator
/// counts too, so `~\docs` works). Hidden folders are left out, and
/// the names sort case-insensitively. Hidden folders, entries that
/// cannot be read, and names that are not UTF-8 are counted in
/// `skipped`.
///
/// # Errors
///
/// Returns an [`RpcError`] when the folder does not exist or cannot
/// be read, or when the path is drive-relative (`C:temp`), which has
/// no current directory to resolve against.
pub(crate) fn folders(req: &FoldersRequest) -> Result<FoldersResult, RpcError> {
    let home = dirs::home_dir();
    let asked = req.path.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let path = resolve(asked, home.as_deref()).map_err(|message| RpcError::new(-32602, message))?;
    let path = path
        .canonicalize()
        .map_err(|e| RpcError::new(-32602, format!("{}: {e}", path.display())))?;
    let entries = std::fs::read_dir(&path)
        .map_err(|e| RpcError::new(-32602, format!("{}: {e}", path.display())))?;
    let mut skipped = 0;
    let mut folders: Vec<String> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!(
                    "kage: folders: skipped an unreadable entry in {}: {error}",
                    path.display()
                );
                skipped += 1;
                continue;
            }
        };
        if !entry.path().is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(name) => {
                eprintln!(
                    "kage: folders: skipped a non-UTF8 name in {}: {}",
                    path.display(),
                    name.to_string_lossy()
                );
                skipped += 1;
                continue;
            }
        };
        if is_hidden(&name, entry_attributes(&entry)) {
            skipped += 1;
            continue;
        }
        folders.push(name);
    }
    folders.sort_by_key(|name| name.to_lowercase());
    let truncated = folders.len() > MAX_FOLDERS;
    folders.truncate(MAX_FOLDERS);
    Ok(FoldersResult {
        parent: path.parent().map(display),
        home: home.as_deref().map(display),
        path: display(&path),
        folders,
        truncated,
        skipped,
    })
}

/// The folder `requested` names, with the separators a Windows client
/// sends normalized for the tilde match only (never on the expanded
/// result): `~` and `~/...` in either separator flavor join onto
/// `home`, and absolute paths pass through. A drive-relative path
/// (`C:temp`) is refused rather than silently resolved against the
/// process's current directory on that drive. No request means
/// `home`, or the root when the home directory is unknown.
fn resolve(requested: Option<&str>, home: Option<&Path>) -> Result<PathBuf, String> {
    let normalized = requested.map(|path| path.replace('\\', "/"));
    match (normalized.as_deref(), home) {
        (None, Some(home)) => Ok(home.to_path_buf()),
        (None, None) => Ok(PathBuf::from("/")),
        (Some(path), Some(home)) if path == "~" || path.starts_with("~/") => {
            Ok(home.join(path.trim_start_matches('~').trim_start_matches('/')))
        }
        (Some(path), _) if is_drive_relative(path) => Err(format!(
            "`{path}` is drive-relative; pass an absolute path, or one starting with ~"
        )),
        (Some(path), _) => Ok(PathBuf::from(path)),
    }
}

/// Whether `path` is drive-relative: a drive letter and colon not
/// followed by a separator (`C:` alone, or `C:temp`).
fn is_drive_relative(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && !bytes
            .get(2)
            .is_some_and(|byte| matches!(byte, b'/' | b'\\'))
}

/// Whether a folder entry is hidden: a dot name on every platform,
/// plus the Windows `FILE_ATTRIBUTE_HIDDEN` bit when the entry's
/// attributes are known.
fn is_hidden(name: &str, attributes: u32) -> bool {
    name.starts_with('.') || attributes & 0x2 != 0
}

/// The Windows file attributes of a directory entry, `0` elsewhere.
#[cfg(windows)]
fn entry_attributes(entry: &std::fs::DirEntry) -> u32 {
    use std::os::windows::fs::MetadataExt as _;
    entry
        .metadata()
        .map(|meta| meta.file_attributes())
        .unwrap_or(0)
}

/// The Windows file attributes of a directory entry, `0` elsewhere.
#[cfg(not(windows))]
fn entry_attributes(entry: &std::fs::DirEntry) -> u32 {
    let _ = entry;
    0
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

    #[test]
    fn hidden_folders_are_left_out_and_counted_as_skipped() {
        let dir = tempfile::tempdir().unwrap();
        for name in [".cache", "keep"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        let listed = folders(&FoldersRequest {
            path: Some(dir.path().display().to_string()),
        })
        .unwrap();
        assert_eq!(listed.folders, ["keep"]);
        assert_eq!(listed.skipped, 1);
        assert!(!listed.truncated);
    }

    // APFS and NTFS reject non-UTF-8 names, so the fixture only
    // exists on Linux.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_name_is_skipped_and_counted() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("good")).unwrap();
        let raw = OsStr::from_bytes(b"\xffnot-utf8");
        std::fs::create_dir(dir.path().join(raw)).unwrap();
        let listed = folders(&FoldersRequest {
            path: Some(dir.path().display().to_string()),
        })
        .unwrap();
        assert_eq!(listed.folders, ["good"]);
        assert_eq!(listed.skipped, 1);
        assert!(!listed.truncated);
    }

    #[test]
    fn tilde_and_backslash_tilde_requests_join_onto_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            resolve(Some("~"), Some(home)).unwrap(),
            PathBuf::from("/home/u")
        );
        assert_eq!(
            resolve(Some("~/docs"), Some(home)).unwrap(),
            PathBuf::from("/home/u/docs")
        );
        assert_eq!(
            resolve(Some("~\\docs"), Some(home)).unwrap(),
            PathBuf::from("/home/u/docs")
        );
    }

    #[test]
    fn a_drive_relative_path_is_refused() {
        let err = resolve(Some("C:temp"), Some(Path::new("/home/u"))).unwrap_err();
        assert!(err.contains("C:temp"), "{err}");
        assert!(err.contains("drive-relative"), "{err}");
        assert!(resolve(Some("C:"), None).is_err());
    }

    #[test]
    fn absolute_paths_pass_through_resolution() {
        assert_eq!(
            resolve(Some("/srv/data"), None).unwrap(),
            PathBuf::from("/srv/data")
        );
        assert_eq!(
            resolve(Some("C:\\abs"), None).unwrap(),
            PathBuf::from("C:/abs")
        );
        assert_eq!(
            resolve(Some("C:/abs"), None).unwrap(),
            PathBuf::from("C:/abs")
        );
    }

    #[test]
    fn no_request_means_home_then_root() {
        assert_eq!(
            resolve(None, Some(Path::new("/home/u"))).unwrap(),
            PathBuf::from("/home/u")
        );
        assert_eq!(resolve(None, None).unwrap(), PathBuf::from("/"));
    }

    #[test]
    fn is_hidden_covers_dot_names_and_the_windows_bit() {
        assert!(is_hidden(".cache", 0));
        assert!(!is_hidden("build", 0));
        assert!(is_hidden("AppData", 0x2));
        assert!(!is_hidden("AppData", 0x10));
        // The Windows attribute path itself only executes on a
        // Windows CI leg; `entry_attributes` returns 0 on this OS.
    }
}
