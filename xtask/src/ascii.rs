//! `check-ascii`: enforce the ASCII-only source rule.
//!
//! Scans the workspace Rust sources plus the Lua, Rust and markdown
//! files under `plugins/` and `man/` for raw non-ASCII bytes. The TUI
//! renders Unicode glyphs through `\u{...}` escapes, which are ASCII in
//! source, so this gate bans only literal multibyte characters and
//! leaves intentional escapes alone. It mirrors the `gen-lua-types
//! --check` drift gate: CI runs it and a violation fails with the
//! offending `path:line`.

use std::path::{Path, PathBuf};

/// Directories the gate scans, relative to the workspace root.
fn scan_roots() -> Vec<PathBuf> {
    let root = crate::workspace_root();
    vec![
        root.join("crates"),
        root.join("gui").join("src"),
        root.join("xtask").join("src"),
        root.join("plugins"),
        root.join("man"),
    ]
}

/// Directories never descended into: build output and vendored
/// dependencies.
fn skipped(dir: &Path) -> bool {
    dir.file_name()
        .is_some_and(|n| n == "target" || n == "node_modules")
}

/// Scan every `*.rs`, `*.lua` and `*.md` under the default roots and
/// fail on raw non-ASCII bytes.
///
/// # Errors
///
/// Returns an error listing each offending `file:line` when any source
/// contains a byte outside the ASCII range, or when a path cannot be read.
pub fn run() -> Result<(), String> {
    run_roots(&scan_roots())
}

/// Scan every `*.rs`, `*.lua` and `*.md` under `roots` and fail on raw
/// non-ASCII bytes.
///
/// # Errors
///
/// Same as [`run`].
pub fn run_roots(roots: &[PathBuf]) -> Result<(), String> {
    let mut offenders = Vec::new();
    let mut dirs = roots.to_vec();
    while let Some(dir) = dirs.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read dir entry: {e}"))?;
            let path = entry.path();
            let ty = entry.file_type().map_err(|e| format!("file type: {e}"))?;
            if ty.is_dir() {
                if skipped(&path) {
                    continue;
                }
                dirs.push(path);
            } else if matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("rs" | "lua" | "md")
            ) {
                scan_file(&path, &mut offenders)?;
            }
        }
    }
    if offenders.is_empty() {
        return Ok(());
    }
    Err(format!(
        "non-ASCII bytes in source (use \\u{{..}} escapes instead):\n  {}",
        offenders.join("\n  ")
    ))
}

/// Record one `path:line` for the first non-ASCII byte on each line.
fn scan_file(path: &Path, offenders: &mut Vec<String>) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut line = 1usize;
    let mut flagged_line = 0usize;
    for &b in &bytes {
        if b == b'\n' {
            line += 1;
        } else if !b.is_ascii() && flagged_line != line {
            offenders.push(format!("{}:{line}", path.display()));
            flagged_line = line;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_source_is_ascii_only() {
        if let Err(report) = super::run() {
            panic!("{report}");
        }
    }

    #[test]
    fn non_ascii_under_a_root_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("plugins");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("ok.lua"), "local x = 1\n").unwrap();
        std::fs::write(sub.join("bad.lua"), "local s = \"caf\u{e9}\"\n").unwrap();
        let err = super::run_roots(&[dir.path().to_path_buf()]).unwrap_err();
        assert!(err.contains("bad.lua:1"), "{err}");
        assert!(!err.contains("ok.lua"), "{err}");
    }

    #[test]
    fn unscanned_extensions_skipped_dirs_and_ascii_pass() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested").join("node_modules");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "caf\u{e9}\n").unwrap();
        std::fs::write(nested.join("vendored.md"), "caf\u{e9}\n").unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn main() {}\n").unwrap();
        super::run_roots(&[dir.path().to_path_buf()]).expect("only scanned extensions count");
    }
}
