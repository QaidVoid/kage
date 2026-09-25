//! Provider/model catalog.
//!
//! The bundled snapshot lives in [`generated`], which is rewritten by
//! `cargo xtask refresh-models`. `kage models refresh` writes a newer
//! copy of the models.dev catalog to the model cache; [`use_cache`]
//! lays its model entries over the snapshot. This module exposes a
//! hand-curated API on top so callers don't depend on the generator's
//! exact shape.
//!
//! Provider impls are still hardcoded in their respective modules
//! (`anthropic`, `openai`, `gemini`, `compat`); the catalog only
//! carries the metadata needed for pickers, default-model selection,
//! thinking and display. The cache never changes a provider's id,
//! name or endpoint, only its models.

mod generated;
pub mod source;

use std::path::Path;
use std::sync::OnceLock;

use kage_core::{Inputs, Reasoning};

use source::{SourceModel, SourceProvider};

/// Description of one provider in the catalog.
#[derive(Debug, Clone, Copy)]
pub struct ProviderInfo {
    /// Stable id used as the prefix in `provider:model` strings, and
    /// as the key in the auth credential store.
    pub id: &'static str,
    /// Human-friendly display name.
    pub name: &'static str,
    /// Documented API endpoint, when models.dev publishes one.
    pub api: Option<&'static str>,
    /// Models the provider exposes that support tool calling.
    pub models: &'static [ModelInfo],
}

/// Description of one model in the catalog.
#[derive(Debug, Clone, Copy)]
pub struct ModelInfo {
    /// Provider-scoped model id (e.g. `claude-sonnet-4-6`, `glm-4.6`).
    pub id: &'static str,
    /// Human-friendly display name.
    pub name: &'static str,
    /// Context window in tokens, when the catalog reports one.
    pub context: Option<u64>,
    /// Largest prompt in tokens, when the catalog reports one smaller
    /// than the context window.
    pub input_limit: Option<u64>,
    /// Maximum output tokens per turn, when reported.
    pub output: Option<u64>,
    /// Thinking settings the model accepts.
    pub reasoning: Reasoning,
    /// Inputs the model accepts.
    pub input: Inputs,
    /// ISO-8601 date string the catalog associates with this model.
    pub release_date: Option<&'static str>,
    /// Per-million-token pricing in USD, when the catalog reports it.
    pub cost: Option<ModelCost>,
}

impl ModelInfo {
    /// Tokens a prompt may fill: the input limit when reported, else
    /// the context window.
    #[must_use]
    pub fn prompt_window(&self) -> Option<u64> {
        self.input_limit.or(self.context)
    }
}

/// Per-million-token pricing for one model, in USD.
///
/// Cache-read and cache-write are optional because not every provider
/// distinguishes the two; when absent, callers should treat cached
/// tokens at the input rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelCost {
    /// Dollars per million input (prompt) tokens.
    pub input: f64,
    /// Dollars per million output (completion) tokens.
    pub output: f64,
    /// Dollars per million tokens read from the provider's prompt cache.
    pub cache_read: Option<f64>,
    /// Dollars per million tokens written into the provider's prompt
    /// cache for a future turn to reuse.
    pub cache_write: Option<f64>,
}

static CATALOG: OnceLock<&'static [ProviderInfo]> = OnceLock::new();

/// Every provider in the catalog: the bundled snapshot, with the model
/// cache laid over it when [`use_cache`] loaded one.
#[must_use]
pub fn providers() -> &'static [ProviderInfo] {
    CATALOG.get().copied().unwrap_or(generated::PROVIDERS)
}

/// Lay the model cache at `path` over the bundled snapshot for the rest
/// of the process. A missing or unreadable cache is ignored, as is any
/// entry that does not parse; only the first call has an effect.
///
/// The cache may add models to a bundled provider or update their
/// metadata. It never adds a provider or changes a provider's id,
/// name or endpoint.
pub fn use_cache(path: &Path) {
    let Ok(json) = std::fs::read_to_string(path) else {
        return;
    };
    if let Some(merged) = merge_json(generated::PROVIDERS, &json) {
        let _ = CATALOG.set(merged);
    }
}

/// Download the models.dev catalog from `url`, cut it down to kage's
/// providers and write it to `dest`, replacing it atomically. Returns
/// the number of models written.
///
/// # Errors
///
/// The download fails, the body is not a catalog, or `dest` cannot be
/// written.
pub fn refresh(url: &str, dest: &Path) -> Result<usize, String> {
    use std::io::Read as _;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .into();
    let response = agent
        .get(url)
        .header("user-agent", concat!("kage/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|e| format!("get {url}: {e}"))?;
    let mut body = String::new();
    response
        .into_body()
        .into_reader()
        .take(32 * 1024 * 1024)
        .read_to_string(&mut body)
        .map_err(|e| format!("read {url}: {e}"))?;
    let pruned = source::prune(&body)?;
    let count = source::parse(&pruned)?.iter().map(|p| p.models.len()).sum();
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    kage_core::fsutil::atomic_write(dest, pruned.as_bytes())
        .map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(count)
}

/// `bundled` with the models of the catalog `json` laid over it:
/// matching ids are replaced, new ids added. Providers outside
/// `bundled` are dropped and provider fields always come from
/// `bundled`. `None` when `json` does not parse. The merged catalog is
/// leaked: it is built once and lives for the process.
fn merge_json(bundled: &'static [ProviderInfo], json: &str) -> Option<&'static [ProviderInfo]> {
    let cached = source::parse(json).ok()?;
    let merged: Vec<ProviderInfo> = bundled
        .iter()
        .map(|p| match cached.iter().find(|c| c.id == p.id) {
            Some(c) => ProviderInfo {
                models: merge_models(p.models, c),
                ..*p
            },
            None => *p,
        })
        .collect();
    Some(Box::leak(merged.into_boxed_slice()))
}

fn merge_models(bundled: &[ModelInfo], cached: &SourceProvider) -> &'static [ModelInfo] {
    let mut models = bundled.to_vec();
    for m in &cached.models {
        let info = leak_model(m);
        match models.iter_mut().find(|b| b.id == info.id) {
            Some(slot) => *slot = info,
            None => models.push(info),
        }
    }
    models.sort_by(|a, b| a.id.cmp(b.id));
    Box::leak(models.into_boxed_slice())
}

fn leak_model(m: &SourceModel) -> ModelInfo {
    let leak = |s: &str| -> &'static str { Box::leak(s.to_owned().into_boxed_str()) };
    ModelInfo {
        id: leak(&m.id),
        name: leak(&m.name),
        context: m.context,
        input_limit: m.input_limit,
        output: m.output,
        reasoning: m.reasoning,
        input: m.input,
        release_date: m.release_date.as_deref().map(leak),
        cost: m.cost,
    }
}

/// Find a provider by its kage id.
#[must_use]
pub fn provider(id: &str) -> Option<&'static ProviderInfo> {
    providers().iter().find(|p| p.id == id)
}

/// Find a model under `provider`.
#[must_use]
pub fn model(provider_id: &str, model_id: &str) -> Option<&'static ModelInfo> {
    provider(provider_id)?
        .models
        .iter()
        .find(|m| m.id == model_id)
}

/// Pick a sensible default model for `provider`. Prefers the
/// most-recently-released model in the catalog; falls back to the
/// first listed. Returns `None` for unknown providers.
#[must_use]
pub fn preferred_model(provider_id: &str) -> Option<&'static ModelInfo> {
    let p = provider(provider_id)?;
    let by_release = p
        .models
        .iter()
        .filter(|m| m.release_date.is_some())
        .max_by_key(|m| m.release_date.unwrap_or(""));
    by_release.or_else(|| p.models.first())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_lookup() {
        // Catalog ships with at least the four hand-curated providers.
        assert!(provider("anthropic").is_some());
        assert!(provider("openai").is_some());
        assert!(provider("gemini").is_some());
        assert!(provider("zai").is_some());
        assert!(provider("nope").is_none());
    }

    #[test]
    fn each_provider_has_at_least_one_model() {
        for p in providers() {
            assert!(!p.models.is_empty(), "{} has no models", p.id);
        }
    }

    #[test]
    fn preferred_model_returns_some_for_known_provider() {
        assert!(preferred_model("anthropic").is_some());
        assert!(preferred_model("nope").is_none());
    }

    #[test]
    fn cache_updates_and_adds_models_but_never_provider_fields() {
        let cache = r#"{
            "anthropic": {
                "name": "Evil",
                "api": "https://evil.example/v1",
                "models": {
                    "claude-opus-4-7": {
                        "id": "claude-opus-4-7", "name": "Opus Renamed", "tool_call": true,
                        "limit": {"context": 42, "output": 7}
                    },
                    "claude-new": {"id": "claude-new", "name": "New", "tool_call": true},
                    "bad": {"tool_call": true}
                }
            },
            "not-a-provider": {
                "name": "X", "api": "https://x.example",
                "models": {"m": {"id": "m", "tool_call": true}}
            }
        }"#;
        let merged = merge_json(generated::PROVIDERS, cache).unwrap();
        assert_eq!(merged.len(), generated::PROVIDERS.len());
        let anthropic = merged.iter().find(|p| p.id == "anthropic").unwrap();
        let bundled = provider("anthropic").unwrap();
        assert_eq!(anthropic.name, bundled.name);
        assert_eq!(anthropic.api, bundled.api);
        let opus = anthropic
            .models
            .iter()
            .find(|m| m.id == "claude-opus-4-7")
            .unwrap();
        assert_eq!(opus.name, "Opus Renamed");
        assert_eq!(opus.context, Some(42));
        assert!(anthropic.models.iter().any(|m| m.id == "claude-new"));
        assert_eq!(anthropic.models.len(), bundled.models.len() + 1);
        assert!(merged.iter().all(|p| p.id != "not-a-provider"));
    }

    #[test]
    fn unparseable_cache_falls_back() {
        assert!(merge_json(generated::PROVIDERS, "not json").is_none());
        use_cache(Path::new("/nonexistent/kage/models.json"));
        assert!(provider("anthropic").is_some());
    }
}
