//! On-disk credential store for provider API keys.
//!
//! [`AuthStore`] persists per-provider tokens at
//! `$XDG_DATA_HOME/kage/auth.json` so users don't need to re-export
//! env vars every shell. The file is created with mode `0600` on Unix
//! so other users cannot read it.
//!
//! Environment variables still take precedence over the store: the
//! provider registry checks `ANTHROPIC_API_KEY` etc. first, then falls
//! back to whatever the store has, then registers the provider only if
//! one of the two yielded a key.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

/// On-disk shape of the auth file. Bumped only when the schema changes
/// in a non-additive way; readers must tolerate older schemas.
const FORMAT_VERSION: u32 = 1;

/// Persistent credential store, keyed by provider id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStore {
    /// Schema version so future bumps can migrate.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Map of provider id (e.g. `anthropic`, `zai`) to API key.
    #[serde(default)]
    pub providers: BTreeMap<String, String>,
}

fn default_version() -> u32 {
    FORMAT_VERSION
}

impl AuthStore {
    /// Construct an empty store.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            version: FORMAT_VERSION,
            providers: BTreeMap::new(),
        }
    }

    /// Path on disk: `$XDG_DATA_HOME/kage/auth.json`. Returns an error
    /// if the home directory cannot be resolved.
    pub fn default_path() -> Result<PathBuf, String> {
        Ok(crate::data_root()?.join("auth.json"))
    }

    /// Load the store from `path`. Returns an empty store when the file
    /// does not exist; surfaces every other I/O or decode error.
    pub fn load_from(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|e| format!("auth: parse {}: {e}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(err) => Err(format!("auth: read {}: {err}", path.display())),
        }
    }

    /// Convenience wrapper around [`Self::load_from`] using
    /// [`Self::default_path`].
    pub fn load() -> Result<Self, String> {
        Self::load_from(&Self::default_path()?)
    }

    /// Persist the store to `path` with mode `0600` on Unix so other
    /// users on the host cannot read it.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("auth: mkdir {}: {err}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(|e| format!("auth: encode: {e}"))?;
        write_private(path, raw.as_bytes())
            .map_err(|err| format!("auth: write {}: {err}", path.display()))
    }

    /// Convenience wrapper around [`Self::save_to`] using
    /// [`Self::default_path`].
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::default_path()?)
    }

    /// Look up a provider's saved API key.
    #[must_use]
    pub fn get(&self, provider: &str) -> Option<&str> {
        self.providers.get(provider).map(String::as_str)
    }

    /// Insert or replace `provider`'s key. Returns the previous value.
    pub fn set(&mut self, provider: &str, key: impl Into<String>) -> Option<String> {
        self.providers.insert(provider.to_owned(), key.into())
    }

    /// Remove a provider's saved key. Returns true when something was
    /// actually removed.
    pub fn remove(&mut self, provider: &str) -> bool {
        self.providers.remove(provider).is_some()
    }
}

/// Provider ids the auth subcommand can target. `zai` and `zai-coding`
/// are billed separately so each takes its own key.
pub const KNOWN_PROVIDERS: &[&str] = &["anthropic", "openai", "gemini", "zai", "zai-coding"];

/// Env-var name that, when set, supersedes any saved key for `provider`.
#[must_use]
pub fn env_var_for(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        "zai" => "ZAI_API_KEY",
        "zai-coding" => "ZAI_CODING_API_KEY",
        _ => "",
    }
}

/// `kage auth login [provider]`: read an API key from a hidden prompt
/// and persist it to the auth store.
pub fn run_login(provider: Option<&str>) -> ExitCode {
    let provider = match select_provider(provider) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(2);
        }
    };
    let mut store = match AuthStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let key = match read_secret(&format!("{provider} API key: ")) {
        Ok(k) if k.is_empty() => {
            eprintln!("kage: empty key, nothing saved");
            return ExitCode::from(2);
        }
        Ok(k) => k,
        Err(e) => {
            eprintln!("kage: prompt failed: {e}");
            return ExitCode::from(1);
        }
    };
    store.set(&provider, key);
    if let Err(e) = store.save() {
        eprintln!("kage: {e}");
        return ExitCode::from(1);
    }
    eprintln!("kage: saved {provider} credentials");
    ExitCode::SUCCESS
}

/// `kage auth logout <provider>`: remove the saved key for `provider`.
pub fn run_logout(provider: &str) -> ExitCode {
    let mut store = match AuthStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    if !store.remove(provider) {
        eprintln!("kage: no saved key for {provider}");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = store.save() {
        eprintln!("kage: {e}");
        return ExitCode::from(1);
    }
    eprintln!("kage: removed {provider} credentials");
    ExitCode::SUCCESS
}

/// `kage auth list`: show one row per known provider, indicating
/// whether a key is available from the env, the auth store, both, or
/// neither.
pub fn run_list() -> ExitCode {
    let store = match AuthStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{:<14}  {:<8}  source", "PROVIDER", "STATUS");
    for provider in KNOWN_PROVIDERS {
        let env = env_var_for(provider);
        let from_env = !env.is_empty() && std::env::var(env).is_ok_and(|v| !v.is_empty());
        let from_store = store.get(provider).is_some();
        let status = if from_env || from_store { "ready" } else { "-" };
        let source = match (from_env, from_store) {
            (true, true) => format!("{env} + auth.json"),
            (true, false) => env.to_owned(),
            (false, true) => "auth.json".to_owned(),
            (false, false) => "(unset)".to_owned(),
        };
        let _ = writeln!(stdout, "{provider:<14}  {status:<8}  {source}");
    }
    ExitCode::SUCCESS
}

fn select_provider(arg: Option<&str>) -> Result<String, String> {
    if let Some(p) = arg {
        if KNOWN_PROVIDERS.contains(&p) {
            return Ok(p.to_owned());
        }
        return Err(format!(
            "unknown provider '{p}', expected one of: {}",
            KNOWN_PROVIDERS.join(", ")
        ));
    }
    let store = AuthStore::load().unwrap_or_else(|_| AuthStore::empty());
    let items: Vec<kage_tui::PickItem> = KNOWN_PROVIDERS
        .iter()
        .map(|p| {
            let badge = if store.get(p).is_some() { '*' } else { '-' };
            let label = match kage_provider::catalog::provider(p) {
                Some(info) => format!("{} ({}, {} models)", info.name, p, info.models.len()),
                // `zai-coding` and any future kage-only ids without a
                // models.dev entry just render their id.
                None => (*p).to_owned(),
            };
            kage_tui::PickItem::simple(*p).with_label(label).with_badge(badge)
        })
        .collect();
    match kage_tui::pick("Select a provider", &items) {
        Ok(Some(p)) => Ok(p),
        Ok(None) => Err("cancelled".to_owned()),
        Err(e) => Err(format!("picker failed: {e}")),
    }
}

fn read_secret(prompt: &str) -> Result<String, String> {
    rpassword::prompt_password(prompt).map_err(|e| e.to_string())
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn round_trip_through_json() {
        let mut store = AuthStore::empty();
        store.set("anthropic", "sk-ant-xxx");
        store.set("zai", "zai-key");
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        store.save_to(&path).unwrap();
        let read_back = AuthStore::load_from(&path).unwrap();
        assert_eq!(read_back, store);
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let store = AuthStore::load_from(&path).unwrap();
        assert!(store.providers.is_empty());
    }

    #[test]
    fn remove_returns_true_only_when_present() {
        let mut store = AuthStore::empty();
        store.set("zai", "k");
        assert!(store.remove("zai"));
        assert!(!store.remove("zai"));
    }

    #[test]
    fn malformed_file_is_a_decode_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(&path, b"{not json").unwrap();
        let err = AuthStore::load_from(&path).unwrap_err();
        assert!(err.contains("parse"));
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let mut store = AuthStore::empty();
        store.set("anthropic", "x");
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        store.save_to(&path).unwrap();
        let perms = fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }
}
