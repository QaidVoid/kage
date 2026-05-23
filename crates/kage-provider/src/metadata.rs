//! Static description of a [`Provider`](crate::Provider) instance.

/// Description used by the registry, `kage doctor`, and the model picker UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderMetadata {
    /// Stable id used as the prefix in `provider:model` strings.
    pub id: String,
    /// Human-friendly display name.
    pub display_name: String,
    /// Whether the provider supports prompt caching.
    pub supports_caching: bool,
    /// Whether the provider exposes a thinking-tokens budget.
    pub supports_thinking: bool,
    /// Whether the provider supports tool/function calling.
    pub supports_tool_use: bool,
}

/// One model entry a [`Provider`](crate::Provider) advertises for the UI
/// picker. Plugin providers that have no catalog entry declare their
/// model list this way; built-in providers can leave the
/// [`Provider::models`](crate::Provider::models) override empty and rely
/// on [`crate::catalog`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderModel {
    /// Provider-scoped model id (the portion after `provider:`).
    pub id: String,
    /// Human-friendly display name shown in the picker.
    pub name: String,
    /// Total context window in tokens, when known. Surfaces to the
    /// modeline so the percent-of-context indicator works for
    /// plugin-registered models without catalog entries.
    pub context: Option<u64>,
    /// Maximum output tokens per turn, when known. Forwarded on every
    /// stream request so the provider does not silently truncate with
    /// its conservative default.
    pub max_output: Option<u32>,
}
