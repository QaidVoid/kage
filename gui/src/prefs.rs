//! The client's own preferences: the theme choice, the toggles the
//! settings dialog writes, and the marks the user puts on sessions.
//!
//! None of this is engine state, so none of it rides the wire. It is
//! stored as one JSON document: `desktop.json` in the kage config
//! directory natively, and one `localStorage` entry in the browser. A
//! document that is missing or does not parse reads as the defaults,
//! and a field it lacks takes its default, so older documents keep
//! loading.

use std::collections::BTreeSet;

use kage_client::wire::SessionConfigOption;
use serde::{Deserialize, Serialize};

use crate::theme::ThemeChoice;

/// Everything the client remembers about itself across launches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Prefs {
    /// The theme: one of the kage themes, or the platform's choice.
    pub theme: ThemeChoice,
    /// Whether Enter sends. Off, Enter adds a newline and Ctrl+Enter
    /// sends.
    pub enter_sends: bool,
    /// Whether the sidebar groups sessions under their project.
    pub group_by_project: bool,
    /// Whether turning swarm mode on asks first.
    pub confirm_swarm: bool,
    /// Lab: swarm cards draw one star per worker.
    pub constellation: bool,
    /// Lab: the context ring opens the gauge with a Compact action.
    pub fuel: bool,
    /// Lab: the turn timeline rail beside the transcript.
    pub rail: bool,
    /// Vim mode: normal-mode motions over the transcript, a `:` line
    /// and a modeline. Off by default.
    pub vim: bool,
    /// Sessions pinned to the top of the sidebar.
    pub pinned: BTreeSet<String>,
    /// Sessions left out of the sidebar until restored.
    pub archived: BTreeSet<String>,
    /// The model, thinking and mode options the last session showed,
    /// which the welcome pane offers before a session exists.
    pub template: Vec<SessionConfigOption>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            enter_sends: true,
            group_by_project: true,
            confirm_swarm: true,
            constellation: true,
            fuel: true,
            rail: true,
            vim: false,
            pinned: BTreeSet::new(),
            archived: BTreeSet::new(),
            template: Vec::new(),
        }
    }
}

impl Prefs {
    /// Reads a stored document; anything unreadable is the defaults.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }

    /// The document to store.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("prefs serialize")
    }
}

/// The stored preferences, or the defaults when none are stored.
#[must_use]
pub fn load() -> Prefs {
    storage::read().map_or_else(Prefs::default, |text| Prefs::parse(&text))
}

/// Stores `prefs`. A failed write is reported on stderr and otherwise
/// ignored: the running app keeps its state either way.
pub fn save(prefs: &Prefs) {
    if let Err(error) = storage::write(&prefs.to_json()) {
        eprintln!("kage-desktop: saving preferences failed: {error}");
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod storage {
    use std::path::PathBuf;

    /// `desktop.json` in the kage config directory: `$XDG_CONFIG_HOME/kage`,
    /// else `~/.config/kage`, else `%APPDATA%\kage`.
    fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))?;
        Some(base.join("kage").join("desktop.json"))
    }

    pub(super) fn read() -> Option<String> {
        std::fs::read_to_string(path()?).ok()
    }

    pub(super) fn write(text: &str) -> Result<(), String> {
        let path = path().ok_or("no config directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(target_arch = "wasm32")]
mod storage {
    /// The `localStorage` key the document lives under.
    const KEY: &str = "kage.desktop";

    fn local() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }

    pub(super) fn read() -> Option<String> {
        local()?.get_item(KEY).ok().flatten()
    }

    pub(super) fn write(text: &str) -> Result<(), String> {
        local()
            .ok_or("no local storage")?
            .set_item(KEY, text)
            .map_err(|_| "local storage refused the write".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::Prefs;
    use crate::theme::ThemeChoice;

    #[test]
    fn a_document_round_trips_and_missing_fields_take_defaults() {
        let mut prefs = Prefs {
            theme: ThemeChoice::Dawn,
            enter_sends: false,
            ..Prefs::default()
        };
        prefs.pinned.insert("s1".into());
        assert_eq!(Prefs::parse(&prefs.to_json()), prefs);

        let old = Prefs::parse(r#"{"theme": "shadow"}"#);
        assert_eq!(old.theme, ThemeChoice::Shadow);
        assert!(
            old.enter_sends && old.constellation,
            "absent fields default"
        );
        assert_eq!(Prefs::parse("not json"), Prefs::default());
    }
}
