//! Static provider/model catalog.
//!
//! The data lives in [`generated`], which is rewritten by
//! `cargo xtask refresh-models`. This module exposes a hand-curated
//! API on top so callers don't depend on the generator's exact shape.
//!
//! Provider impls are still hardcoded in their respective modules
//! (`anthropic`, `openai`, `gemini`, `zai`); the catalog only carries
//! the metadata needed for pickers, default-model selection, and
//! display.

mod generated;

pub use generated::PROVIDERS;

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
    /// Env-var names the upstream catalog associates with this
    /// provider. Informational only; kage uses its own auth store.
    pub env: &'static [&'static str],
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
    /// Maximum output tokens per turn, when reported.
    pub output: Option<u64>,
    /// Whether the model exposes a reasoning / thinking budget.
    pub reasoning: bool,
    /// ISO-8601 date string the catalog associates with this model.
    pub release_date: Option<&'static str>,
    /// Per-million-token pricing in USD, when the catalog reports it.
    pub cost: Option<ModelCost>,
}

/// Per-million-token pricing for one model, in USD.
///
/// Cache-read and cache-write are optional because not every provider
/// distinguishes the two; when absent, callers should treat cached
/// tokens at the input rate.
#[derive(Debug, Clone, Copy)]
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

/// Find a provider by its kage id.
#[must_use]
pub fn provider(id: &str) -> Option<&'static ProviderInfo> {
    PROVIDERS.iter().find(|p| p.id == id)
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
        for p in PROVIDERS {
            assert!(!p.models.is_empty(), "{} has no models", p.id);
        }
    }

    #[test]
    fn preferred_model_returns_some_for_known_provider() {
        assert!(preferred_model("anthropic").is_some());
        assert!(preferred_model("nope").is_none());
    }
}
