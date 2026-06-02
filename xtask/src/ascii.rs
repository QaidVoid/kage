//! `check-ascii`: enforce the ASCII-only source rule from CLAUDE.md.
//!
//! Scans the workspace Rust sources for raw non-ASCII bytes. The TUI
//! renders Unicode glyphs through `\u{...}` escapes, which are ASCII in
//! source, so this gate bans only literal multibyte characters and
//! leaves intentional escapes alone. It mirrors the `gen-lua-types
//! --check` drift gate: CI runs it and a violation fails with the
//! offending `path:line`.

use std::path::Path;

/// Scan every `*.rs` under the workspace and fail on raw non-ASCII bytes.
///
/// # Errors
///
/// Returns an error listing each offending `file:line` when any source
/// contains a byte outside the ASCII range, or when a path cannot be read.
pub fn run() -> Result<(), String> {
    let root = crate::workspace_root();
    let mut offenders = Vec::new();
    let mut dirs = vec![root.join("crates"), root.join("xtask").join("src")];
    while let Some(dir) = dirs.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read dir entry: {e}"))?;
            let path = entry.path();
            let ty = entry.file_type().map_err(|e| format!("file type: {e}"))?;
            if ty.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                dirs.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
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
}
