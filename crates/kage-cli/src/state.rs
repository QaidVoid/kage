//! Persistent UI state, separate from credentials.
//!
//! [`State`] remembers the last provider-qualified model the user
//! picked, the last binary version seen, and the last authentication
//! failure per provider. The file format is versioned so future entries
//! can land without churn.
//! Persisted at `$XDG_STATE_HOME/kage/state.json` (default
//! `~/.local/state/kage/state.json`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use kage_core::LoopError;
use kage_core::protocol::RunOutcome;
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
    /// Last `kage` binary version (`CARGO_PKG_VERSION`) that opened
    /// the TUI. Compared on startup to detect upgrades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_version: Option<String>,
    /// Detail of the last authentication failure per provider id.
    /// Cleared by a completed run on that provider or a successful
    /// `:login <provider>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub auth_failures: BTreeMap<String, String>,
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
            last_seen_version: None,
            auth_failures: BTreeMap::new(),
        }
    }

    /// Apply a finished run on the provider-qualified `model` to
    /// [`Self::auth_failures`]: an authentication failure is recorded
    /// against its provider and a completed run clears it. Returns
    /// whether anything changed.
    pub fn note_run(&mut self, model: &str, outcome: &RunOutcome) -> bool {
        let provider = model.split_once(':').map_or(model, |(p, _)| p);
        match outcome {
            RunOutcome::Completed => self.auth_failures.remove(provider).is_some(),
            RunOutcome::Failed {
                error: LoopError::Auth { message },
            } if !provider.is_empty() => {
                let prev = self
                    .auth_failures
                    .insert(provider.to_owned(), message.clone());
                prev.as_ref() != Some(message)
            }
            _ => false,
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

    /// Convenience wrapper over [`Self::load_from`] using the default
    /// path. A corrupt or unreadable state file is reported on stderr
    /// before falling back to empty state, so the user knows their
    /// saved preferences were lost rather than silently wiped. A
    /// missing file is not an error ([`Self::load_from`] yields empty).
    pub fn load() -> Self {
        match Self::default_path().and_then(|p| Self::load_from(&p)) {
            Ok(state) => state,
            Err(err) => {
                eprintln!("kage: {err}; starting from empty state");
                Self::default()
            }
        }
    }

    /// Persist state to `path`.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("state: mkdir {}: {err}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(|e| format!("state: encode: {e}"))?;
        kage_core::fsutil::atomic_write(path, raw.as_bytes())
            .map_err(|err| format!("state: write {}: {err}", path.display()))
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

/// Record the running binary version into [`State::last_seen_version`]
/// and report whether it changed since the last run.
///
/// Returns `Ok(Some(previous))` when the saved version differed from
/// `current` (the caller can surface a "kage updated from X to Y"
/// notice), `Ok(None)` when this is the first run or the version
/// matches. The state file is rewritten on a real change; first-run
/// also writes so subsequent launches do not all look like upgrades.
pub fn record_version_seen(current: &str) -> Result<Option<String>, String> {
    let mut state = State::load();
    let prev = state.last_seen_version.clone();
    if prev.as_deref() == Some(current) {
        return Ok(None);
    }
    state.last_seen_version = Some(current.to_owned());
    state.save()?;
    Ok(prev)
}

/// Apply a finished run to the saved auth failures (see
/// [`State::note_run`]), writing the file only when it changed.
pub fn record_run_outcome(model: &str, outcome: &RunOutcome) -> Result<(), String> {
    let mut state = State::load();
    if state.note_run(model, outcome) {
        state.save()?;
    }
    Ok(())
}

/// Forget the saved auth failure for `provider`, writing the file only
/// when one was recorded.
pub fn clear_auth_failure(provider: &str) -> Result<(), String> {
    let mut state = State::load();
    if state.auth_failures.remove(provider).is_some() {
        state.save()?;
    }
    Ok(())
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
        assert!(!raw.contains("last_seen_version"), "raw was: {raw}");
        assert!(!raw.contains("auth_failures"), "raw was: {raw}");
    }

    #[test]
    fn last_seen_version_round_trips() {
        let mut state = State::empty();
        state.last_seen_version = Some("0.2.0".into());
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        state.save_to(&path).unwrap();
        let read = State::load_from(&path).unwrap();
        assert_eq!(read.last_seen_version.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn state_files_missing_last_seen_version_load_as_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, r#"{"version":1,"last_model":"x:y"}"#).unwrap();
        let state = State::load_from(&path).unwrap();
        assert!(state.last_seen_version.is_none());
        assert_eq!(state.last_model.as_deref(), Some("x:y"));
    }

    fn auth_failed(message: &str) -> RunOutcome {
        RunOutcome::Failed {
            error: LoopError::Auth {
                message: message.into(),
            },
        }
    }

    #[test]
    fn auth_failure_is_recorded_per_provider_and_persists() {
        let mut state = State::empty();
        assert!(state.note_run("zai-coding-plan:glm-4.6", &auth_failed("status 401")));
        assert!(!state.note_run("zai-coding-plan:glm-4.6", &auth_failed("status 401")));
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        state.save_to(&path).unwrap();
        let read = State::load_from(&path).unwrap();
        assert_eq!(
            read.auth_failures
                .get("zai-coding-plan")
                .map(String::as_str),
            Some("status 401")
        );
    }

    #[test]
    fn completed_run_clears_only_its_provider() {
        let mut state = State::empty();
        state.note_run("zai:glm-4.6", &auth_failed("bad key"));
        state.note_run("openai:gpt-4o", &auth_failed("bad key"));
        assert!(state.note_run("zai:glm-4.6", &RunOutcome::Completed));
        assert!(!state.auth_failures.contains_key("zai"));
        assert!(state.auth_failures.contains_key("openai"));
        assert!(!state.note_run("zai:glm-4.6", &RunOutcome::Completed));
    }

    #[test]
    fn other_outcomes_leave_failures_alone() {
        let mut state = State::empty();
        state.note_run("zai:glm-4.6", &auth_failed("bad key"));
        let provider_error = RunOutcome::Failed {
            error: LoopError::Provider {
                message: "boom".into(),
            },
        };
        assert!(!state.note_run("zai:glm-4.6", &provider_error));
        assert!(!state.note_run("zai:glm-4.6", &RunOutcome::Cancelled));
        assert!(state.auth_failures.contains_key("zai"));
    }
}
