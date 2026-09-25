//! Tool risk classification used by hosts to gate permission prompts.

use serde::{Deserialize, Serialize};

/// What harm a tool could do if invoked.
///
/// Used by hosts (TUI, CLI, editor adapters) to decide which tool calls
/// require explicit user approval. The loop itself never reads this value;
/// it is purely advisory.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Read-only access to local state. Safe to auto-approve in most flows.
    Read,
    /// Modifies local state (filesystem writes, edits).
    Write,
    /// Spawns processes. Most consequential locally.
    Exec,
    /// Performs network I/O. Can exfiltrate data or fetch malicious payloads.
    Network,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_serializes_as_snake_case() {
        assert_eq!(serde_json::to_string(&Risk::Read).unwrap(), "\"read\"");
        assert_eq!(
            serde_json::to_string(&Risk::Network).unwrap(),
            "\"network\""
        );
    }
}
