//! On-disk credential store for provider API keys and OAuth tokens.
//!
//! [`AuthStore`] persists per-provider credentials at
//! `$XDG_DATA_HOME/kage/auth.json` so users don't need to re-export
//! env vars every shell. The file is created with mode `0600` on Unix
//! so other users cannot read it.
//!
//! Each provider's entry is a [`Credential`] which is either an API
//! key or an OAuth record. The deserializer accepts the legacy
//! bare-string form so v1 files keep loading; the writer always emits
//! the tagged form.
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

use chrono::{DateTime, Duration, Utc};
use serde::de::{Error as DeError, Unexpected};
use serde::{Deserialize, Deserializer, Serialize};

/// On-disk shape of the auth file. Bumped to `2` when [`Credential`]
/// became a tagged enum; the loader still accepts v1 strings so older
/// files keep working without an explicit migration step.
const FORMAT_VERSION: u32 = 2;

/// One stored credential. Either a long-lived API key or an OAuth
/// access/refresh token pair the host periodically renews.
///
/// The deserializer accepts three shapes:
/// * a bare JSON string (the v1 form) - parsed as [`Credential::ApiKey`];
/// * an object `{ "type": "api_key", "key": "..." }`;
/// * an object `{ "type": "oauth", "access_token": "...", ... }`.
///
/// The serializer always emits the tagged-object form so files
/// rewritten by the current binary advance to the v2 layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    /// Long-lived API key the provider exchanges for one request at a time.
    ApiKey {
        /// Raw key as supplied by the provider's dashboard.
        key: String,
    },
    /// OAuth access token (with optional refresh) the host renews.
    Oauth(OAuthCredential),
}

impl Credential {
    /// Construct an [`Credential::ApiKey`] from a key string.
    #[must_use]
    pub fn api_key(key: impl Into<String>) -> Self {
        Self::ApiKey { key: key.into() }
    }

    /// `true` when this credential is an OAuth record (regardless of
    /// expiry). Useful for `auth list` to label rows.
    #[must_use]
    pub fn is_oauth(&self) -> bool {
        matches!(self, Self::Oauth(_))
    }

    /// Return the API key string, or the OAuth access token, without
    /// any expiry check. Callers that need refresh-on-expiry semantics
    /// should go through [`AuthStore::access_token`] instead.
    #[must_use]
    pub fn raw_token(&self) -> &str {
        match self {
            Self::ApiKey { key } => key.as_str(),
            Self::Oauth(o) => o.access_token.as_str(),
        }
    }
}

/// OAuth token bundle persisted alongside the access token. Mirrors
/// the canonical RFC 6749 fields the providers we support all return.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthCredential {
    /// Bearer token presented in the `Authorization` header.
    pub access_token: String,
    /// Refresh token used to mint a new access token. `None` for
    /// providers that only issue short-lived tokens with no refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// UTC instant the access token stops being valid. `None` when
    /// the provider did not return an expiry; the host treats absent
    /// expiry as "never refresh proactively".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Space-delimited scope string the provider granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl OAuthCredential {
    /// `true` when the access token is either expired or due to expire
    /// within `slack`. Used by the refresh path to renew a few minutes
    /// before the deadline so the next request doesn't race the clock.
    #[must_use]
    pub fn expires_within(&self, slack: Duration, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|t| now + slack >= t)
    }
}

/// Custom [`Deserialize`] tolerant of the legacy bare-string form.
impl<'de> Deserialize<'de> for Credential {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(s) = value.as_str() {
            return Ok(Self::ApiKey { key: s.to_owned() });
        }
        let map = value.as_object().ok_or_else(|| {
            D::Error::invalid_type(
                Unexpected::Other(&value.to_string()),
                &"a credential string or object",
            )
        })?;
        let kind = map
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| D::Error::missing_field("type"))?;
        match kind {
            "api_key" => {
                let key = map
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| D::Error::missing_field("key"))?;
                Ok(Self::ApiKey {
                    key: key.to_owned(),
                })
            }
            "oauth" => {
                let oauth: OAuthCredential = serde_json::from_value(value)
                    .map_err(|e| D::Error::custom(format!("oauth: {e}")))?;
                Ok(Self::Oauth(oauth))
            }
            other => Err(D::Error::unknown_variant(other, &["api_key", "oauth"])),
        }
    }
}

/// Persistent credential store, keyed by provider id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStore {
    /// Schema version so future bumps can migrate.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Map of provider id (e.g. `anthropic`, `zai`) to credential.
    #[serde(default)]
    pub providers: BTreeMap<String, Credential>,
}

fn default_version() -> u32 {
    FORMAT_VERSION
}

impl AuthStore {
    /// Construct an empty store at the current format version.
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
    /// users on the host cannot read it. Always rewrites at the
    /// current [`FORMAT_VERSION`] so loading and resaving an older
    /// file performs the v1 -> v2 migration in place.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("auth: mkdir {}: {err}", parent.display()))?;
        }
        let mut bumped = self.clone();
        bumped.version = FORMAT_VERSION;
        let raw =
            serde_json::to_string_pretty(&bumped).map_err(|e| format!("auth: encode: {e}"))?;
        write_private(path, raw.as_bytes())
            .map_err(|err| format!("auth: write {}: {err}", path.display()))
    }

    /// Convenience wrapper around [`Self::save_to`] using
    /// [`Self::default_path`].
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::default_path()?)
    }

    /// Look up a provider's full [`Credential`].
    #[must_use]
    pub fn credential(&self, provider: &str) -> Option<&Credential> {
        self.providers.get(provider)
    }

    /// Look up a single bearer token for `provider`, regardless of
    /// credential kind. For API keys returns the raw key; for OAuth
    /// returns the access token *as currently stored*. The returned
    /// token may be expired - the refresh path lives in [`crate::oauth`].
    #[must_use]
    pub fn access_token(&self, provider: &str) -> Option<&str> {
        self.providers.get(provider).map(Credential::raw_token)
    }

    /// Insert or replace `provider`'s credential. Returns the previous
    /// value, if any.
    pub fn set(&mut self, provider: &str, credential: Credential) -> Option<Credential> {
        self.providers.insert(provider.to_owned(), credential)
    }

    /// Insert or replace `provider`'s credential as an API key.
    /// Convenience wrapper around [`Self::set`].
    pub fn set_api_key(&mut self, provider: &str, key: impl Into<String>) -> Option<Credential> {
        self.set(provider, Credential::api_key(key))
    }

    /// Insert or replace `provider`'s credential as an OAuth record.
    /// Convenience wrapper around [`Self::set`].
    pub fn set_oauth(&mut self, provider: &str, creds: OAuthCredential) -> Option<Credential> {
        self.set(provider, Credential::Oauth(creds))
    }

    /// Remove a provider's saved credential. Returns true when something
    /// was actually removed.
    pub fn remove(&mut self, provider: &str) -> bool {
        self.providers.remove(provider).is_some()
    }
}

/// Provider ids the auth subcommand can target. The list mirrors the
/// catalog (and our hardcoded `Provider` impls). `zai` and
/// `zai-coding-plan` are billed separately so each takes its own key.
pub const KNOWN_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "gemini",
    "zai",
    "zai-coding-plan",
    "deepseek",
    "groq",
    "mistral",
    "cerebras",
    "xai",
    "openrouter",
    "fireworks-ai",
    "moonshotai",
    "kimi-for-coding",
];

/// Env-var name that, when set, supersedes any saved key for `provider`.
#[must_use]
pub fn env_var_for(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        "zai" => "ZAI_API_KEY",
        "zai-coding-plan" => "ZAI_CODING_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "groq" => "GROQ_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "cerebras" => "CEREBRAS_API_KEY",
        "xai" => "XAI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "fireworks-ai" => "FIREWORKS_API_KEY",
        "moonshotai" => "MOONSHOT_API_KEY",
        "kimi-for-coding" => "KIMI_API_KEY",
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
    store.set_api_key(&provider, key);
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
/// whether a credential is available from the env, the auth store
/// (and what kind), both, or neither.
pub fn run_list() -> ExitCode {
    let store = match AuthStore::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{:<18}  {:<8}  source", "PROVIDER", "STATUS");
    let mut providers: Vec<&str> = KNOWN_PROVIDERS.to_vec();
    providers.sort_unstable();
    for provider in providers {
        let env = env_var_for(provider);
        let from_env = !env.is_empty() && std::env::var(env).is_ok_and(|v| !v.is_empty());
        let stored = store.credential(provider);
        let status = if from_env || stored.is_some() {
            "ready"
        } else {
            "-"
        };
        let stored_label = stored.map(|c| {
            if c.is_oauth() {
                "auth.json (oauth)"
            } else {
                "auth.json"
            }
        });
        let source = match (from_env, stored_label) {
            (true, Some(label)) => format!("{env} + {label}"),
            (true, None) => env.to_owned(),
            (false, Some(label)) => label.to_owned(),
            (false, None) => "(unset)".to_owned(),
        };
        let _ = writeln!(stdout, "{provider:<18}  {status:<8}  {source}");
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
            let badge = if store.credential(p).is_some() {
                '*'
            } else {
                '-'
            };
            let label = match kage_provider::catalog::provider(p) {
                Some(info) => format!("{} ({}, {} models)", info.name, p, info.models.len()),
                // `zai-coding` and any future kage-only ids without a
                // models.dev entry just render their id.
                None => (*p).to_owned(),
            };
            kage_tui::PickItem::simple(*p)
                .with_label(label)
                .with_badge(badge)
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

    use chrono::TimeZone;
    use tempfile::tempdir;

    use super::*;

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    #[test]
    fn round_trip_through_json() {
        let mut store = AuthStore::empty();
        store.set_api_key("anthropic", "sk-ant-xxx");
        store.set_api_key("zai", "zai-key");
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
        store.set_api_key("zai", "k");
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
        store.set_api_key("anthropic", "x");
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        store.save_to(&path).unwrap();
        let perms = fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }

    #[test]
    fn legacy_v1_string_entries_load_as_api_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(
            &path,
            r#"{"version":1,"providers":{"anthropic":"sk-ant-old","zai":"zai-old"}}"#,
        )
        .unwrap();
        let store = AuthStore::load_from(&path).unwrap();
        assert_eq!(store.access_token("anthropic"), Some("sk-ant-old"));
        assert_eq!(store.access_token("zai"), Some("zai-old"));
        assert!(matches!(
            store.credential("anthropic"),
            Some(Credential::ApiKey { .. })
        ));
    }

    #[test]
    fn save_after_loading_v1_rewrites_as_v2_tagged_form() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(
            &path,
            r#"{"version":1,"providers":{"anthropic":"sk-ant-old"}}"#,
        )
        .unwrap();
        let store = AuthStore::load_from(&path).unwrap();
        store.save_to(&path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"version\": 2"));
        assert!(raw.contains("\"type\": \"api_key\""));
        assert!(raw.contains("\"key\": \"sk-ant-old\""));
    }

    #[test]
    fn oauth_credential_round_trips() {
        let mut store = AuthStore::empty();
        let expires_at = fixed_now() + Duration::seconds(3600);
        let oauth = OAuthCredential {
            access_token: "access-1".into(),
            refresh_token: Some("refresh-1".into()),
            expires_at: Some(expires_at),
            scope: Some("scope-x".into()),
        };
        store.set_oauth("anthropic", oauth.clone());
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        store.save_to(&path).unwrap();
        let read_back = AuthStore::load_from(&path).unwrap();
        assert_eq!(read_back, store);
        assert!(read_back.credential("anthropic").unwrap().is_oauth());
        assert_eq!(read_back.access_token("anthropic"), Some("access-1"));
        let Credential::Oauth(o) = read_back.credential("anthropic").unwrap() else {
            panic!("expected oauth");
        };
        assert_eq!(o.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(o.scope.as_deref(), Some("scope-x"));
        assert_eq!(o.expires_at, Some(expires_at));
    }

    #[test]
    fn expires_within_returns_true_inside_slack_and_false_outside() {
        let now = fixed_now();
        let oauth = OAuthCredential {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: Some(now + Duration::seconds(120)),
            scope: None,
        };
        assert!(!oauth.expires_within(Duration::seconds(60), now));
        assert!(oauth.expires_within(Duration::seconds(120), now));
        assert!(oauth.expires_within(Duration::seconds(180), now));
    }

    #[test]
    fn expires_within_returns_false_when_no_expiry_recorded() {
        let oauth = OAuthCredential {
            access_token: "a".into(),
            ..OAuthCredential::default()
        };
        assert!(!oauth.expires_within(Duration::days(365), fixed_now()));
    }

    #[test]
    fn deserialize_rejects_unknown_credential_type() {
        let err = serde_json::from_str::<Credential>(r#"{"type":"weird"}"#).unwrap_err();
        assert!(err.to_string().contains("weird"));
    }

    #[test]
    fn deserialize_rejects_object_without_type() {
        let err = serde_json::from_str::<Credential>(r#"{"key":"x"}"#).unwrap_err();
        assert!(err.to_string().contains("type"));
    }
}
