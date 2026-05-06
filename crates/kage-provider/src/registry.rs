//! Lookup of [`Provider`] implementations by id.
//!
//! Models are addressed as `provider:model` strings (for example
//! `anthropic:claude-sonnet-4-6`). The registry holds one `Provider`
//! per id; the model portion is forwarded to that provider as the
//! [`StreamRequest::model`] field.

use std::collections::HashMap;
use std::sync::Arc;

use crate::{Provider, ProviderError};

/// Registry of provider implementations indexed by their stable id.
///
/// Cloning is cheap: the inner map uses `Arc<dyn Provider>` so all clones
/// share the same instances.
#[derive(Clone, Debug, Default)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
}

/// A provider plus the model id it should be invoked with.
#[derive(Debug)]
pub struct ResolvedProvider<'a> {
    /// Borrowed provider.
    pub provider: &'a Arc<dyn Provider>,
    /// Model id (the portion after `provider:`).
    pub model: String,
}

impl ProviderRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider under its [`metadata().id`](crate::ProviderMetadata::id).
    ///
    /// Returns `self` to allow chaining.
    #[must_use]
    pub fn with(mut self, provider: Arc<dyn Provider>) -> Self {
        self.register(provider);
        self
    }

    /// Register a provider under its [`metadata().id`](crate::ProviderMetadata::id).
    ///
    /// Replaces any prior registration with the same id.
    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        let id = provider.metadata().id.clone();
        self.providers.insert(id, provider);
    }

    /// Look up a registered provider by id (no `provider:model` parsing).
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.get(id)
    }

    /// Resolve a `provider:model` string into a provider plus the model id.
    ///
    /// Returns [`ProviderError::UnknownModel`] when the input has no
    /// `:` separator or when no provider is registered for the prefix.
    pub fn resolve(&self, model_id: &str) -> Result<ResolvedProvider<'_>, ProviderError> {
        let (prefix, model) = model_id
            .split_once(':')
            .ok_or_else(|| ProviderError::UnknownModel(model_id.to_owned()))?;
        let provider = self
            .providers
            .get(prefix)
            .ok_or_else(|| ProviderError::UnknownModel(model_id.to_owned()))?;
        Ok(ResolvedProvider {
            provider,
            model: model.to_owned(),
        })
    }

    /// Iterate registered provider ids.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventStream, ProviderEvent, ProviderMetadata, StopReason, StreamRequest};
    use kage_core::{CancelFlag, TokenUsage};

    #[derive(Debug)]
    struct StaticProvider {
        meta: ProviderMetadata,
    }

    impl Provider for StaticProvider {
        fn metadata(&self) -> &ProviderMetadata {
            &self.meta
        }

        fn stream(
            &self,
            _req: StreamRequest,
            _cancel: &CancelFlag,
        ) -> Result<EventStream, ProviderError> {
            let events: Vec<Result<ProviderEvent, ProviderError>> =
                vec![Ok(ProviderEvent::MessageEnd {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })];
            Ok(Box::new(events.into_iter()))
        }
    }

    fn provider(id: &str) -> Arc<dyn Provider> {
        Arc::new(StaticProvider {
            meta: ProviderMetadata {
                id: id.to_owned(),
                display_name: id.to_owned(),
                supports_caching: false,
                supports_thinking: false,
                supports_tool_use: false,
            },
        })
    }

    #[test]
    fn empty_registry_resolves_to_unknown_model() {
        let r = ProviderRegistry::new();
        let err = r.resolve("anthropic:claude-sonnet-4-6").unwrap_err();
        assert!(matches!(err, ProviderError::UnknownModel(_)));
    }

    #[test]
    fn registered_provider_resolves() {
        let r = ProviderRegistry::new().with(provider("anthropic"));
        let resolved = r.resolve("anthropic:claude-sonnet-4-6").unwrap();
        assert_eq!(resolved.model, "claude-sonnet-4-6");
        assert_eq!(resolved.provider.metadata().id, "anthropic");
    }

    #[test]
    fn missing_separator_is_unknown_model() {
        let r = ProviderRegistry::new().with(provider("anthropic"));
        let err = r.resolve("claude-sonnet-4-6").unwrap_err();
        assert!(matches!(err, ProviderError::UnknownModel(_)));
    }

    #[test]
    fn register_replaces_prior_entry() {
        let mut r = ProviderRegistry::new();
        r.register(provider("anthropic"));
        r.register(provider("anthropic"));
        assert_eq!(r.ids().count(), 1);
    }

    #[test]
    fn ids_lists_registered_providers() {
        let r = ProviderRegistry::new()
            .with(provider("anthropic"))
            .with(provider("openai"));
        let mut ids: Vec<&str> = r.ids().collect();
        ids.sort_unstable();
        assert_eq!(ids, ["anthropic", "openai"]);
    }
}
