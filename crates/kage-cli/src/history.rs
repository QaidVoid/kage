//! On-disk prompt history for the TUI.
//!
//! Each entry is JSON-encoded and stored on its own line in
//! `$XDG_STATE_HOME/kage/history.txt` so multi-line prompts round-trip
//! safely. The TUI seeds [`kage_tui::InputState`] with the loaded
//! entries on startup and appends each submission as it happens; load
//! and append are independent so an interrupted process never corrupts
//! prior history.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use kage_tui::HISTORY_MAX;

/// Default path: `$XDG_STATE_HOME/kage/history.txt`.
pub fn default_path() -> Result<PathBuf, String> {
    Ok(crate::state_root()?.join("history.txt"))
}

/// Read all entries from `path` in chronological order. Missing file
/// returns an empty vector. Malformed lines are skipped silently so a
/// partial write never blocks startup; valid entries before and after
/// the bad line are still loaded.
pub fn load_from(path: &Path) -> Result<Vec<String>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("history: read {}: {err}", path.display())),
    };
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<String>(line) {
            out.push(entry);
        }
    }
    if out.len() > HISTORY_MAX {
        let drop = out.len() - HISTORY_MAX;
        out.drain(..drop);
    }
    Ok(out)
}

/// Convenience wrapper using [`default_path`].
pub fn load() -> Vec<String> {
    default_path()
        .and_then(|p| load_from(&p))
        .unwrap_or_default()
}

/// Append `entry` to the history file, creating it if needed.
pub fn append_to(path: &Path, entry: &str) -> Result<(), String> {
    if entry.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("history: mkdir {}: {err}", parent.display()))?;
    }
    let line = serde_json::to_string(entry).map_err(|e| format!("history: encode: {e}"))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| format!("history: open {}: {err}", path.display()))?;
    writeln!(file, "{line}").map_err(|err| format!("history: write {}: {err}", path.display()))
}

/// Convenience wrapper using [`default_path`].
pub fn append(entry: &str) -> Result<(), String> {
    append_to(&default_path()?, entry)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn round_trip_through_jsonl_preserves_newlines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.txt");
        append_to(&path, "first").unwrap();
        append_to(&path, "second\nwith\nnewlines").unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded, vec!["first", "second\nwith\nnewlines"]);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempdir().unwrap();
        let loaded = load_from(&dir.path().join("nope.txt")).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.txt");
        fs::write(&path, "\"good\"\nnot-json\n\"also good\"\n").unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded, vec!["good", "also good"]);
    }

    #[test]
    fn load_truncates_to_history_max() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.txt");
        for i in 0..(HISTORY_MAX + 10) {
            append_to(&path, &format!("entry{i}")).unwrap();
        }
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.len(), HISTORY_MAX);
        assert_eq!(loaded.first().unwrap(), "entry10");
    }

    #[test]
    fn empty_entries_are_not_appended() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.txt");
        append_to(&path, "").unwrap();
        assert!(!path.exists());
    }
}
