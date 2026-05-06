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
