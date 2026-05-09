//! Persistent UI state, separate from credentials.
//!
//! [`State`] currently only remembers the last provider-qualified model
//! the user picked, but the file format is versioned so future entries
//! (last session id, layout preferences, etc.) can land without churn.
//! Persisted at `$XDG_STATE_HOME/kage/state.json` (default
//! `~/.local/state/kage/state.json`).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FORMAT_VERSION: u32 = 1;

/// Serialised UI state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Schema version so future bumps can migrate.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Last `provider:model` the user successfully ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
}

fn default_version() -> u32 {
    FORMAT_VERSION
}

impl State {
    /// Construct an empty state.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            version: FORMAT_VERSION,
            last_model: None,
        }
    }

    /// Default on-disk path: `$XDG_STATE_HOME/kage/state.json`.
    pub fn default_path() -> Result<PathBuf, String> {
        Ok(crate::state_root()?.join("state.json"))
    }

    /// Load state from `path`. Returns an empty state when the file
    /// does not exist; surfaces every other I/O or decode error.
    pub fn load_from(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|e| format!("state: parse {}: {e}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(err) => Err(format!("state: read {}: {err}", path.display())),
        }
    }

    /// Convenience wrapper over [`Self::load_from`] using the default path.
    pub fn load() -> Self {
        Self::default_path()
            .and_then(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    /// Persist state to `path`.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("state: mkdir {}: {err}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(|e| format!("state: encode: {e}"))?;
        fs::write(path, raw).map_err(|err| format!("state: write {}: {err}", path.display()))
    }

    /// Convenience wrapper over [`Self::save_to`] using the default path.
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::default_path()?)
    }
}

/// Update the saved last-used model. Returns `Ok(true)` if the file
/// was written, `Ok(false)` if `model` already matched the saved
/// value, or an error string on I/O failure. Callers decide how to
/// surface failures: print mode logs to stderr, the TUI pushes a
/// `kage:error` block (stderr would corrupt the alt screen).
pub fn record_last_model(model: &str) -> Result<bool, String> {
    let mut state = State::load();
    if state.last_model.as_deref() == Some(model) {
        return Ok(false);
    }
    state.last_model = Some(model.to_owned());
    state.save().map(|()| true)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn round_trip_through_json() {
        let mut state = State::empty();
        state.last_model = Some("zai-coding:glm-4.6".into());
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        state.save_to(&path).unwrap();
        let read = State::load_from(&path).unwrap();
        assert_eq!(read, state);
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let state = State::load_from(&path).unwrap();
        assert!(state.last_model.is_none());
    }

    #[test]
    fn skip_serializing_none_keeps_file_clean() {
        let state = State::empty();
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.json");
        state.save_to(&path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("last_model"), "raw was: {raw}");
    }
}
