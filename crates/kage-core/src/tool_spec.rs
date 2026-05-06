//! Tool specification surfaced to providers and the registry.
//!
//! Lives in `kage-core` so both `kage-provider` (which sends tool defs to
//! the model) and `kage-tools` (which produces them from the registry)
//! can use the same type without a circular dependency.

use serde::{Deserialize, Serialize};

/// One tool surfaced to the model for the current turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name (`snake_case`).
    pub name: String,
    /// Short, model-readable description.
    pub description: String,
    /// JSON Schema describing the tool's input.
    pub schema: serde_json::Value,
}
