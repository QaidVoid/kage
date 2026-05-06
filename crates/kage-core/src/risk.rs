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

impl Risk {
    /// Whether the action can change observable state outside of the agent.
    ///
    /// Read is non-destructive. Everything else is.
    #[must_use]
    pub fn is_destructive(self) -> bool {
        !matches!(self, Self::Read)
    }
}

/// Classify a built-in tool by name.
///
/// Returns [`Risk::Exec`] for unknown names. Plugin- and MCP-provided tools
/// declare their own risk through the [`Tool`](https://docs.rs) trait
/// implementation rather than going through this function.
#[must_use]
pub fn classify(tool_name: &str) -> Risk {
    match tool_name {
        "read" | "ls" | "find" | "grep" => Risk::Read,
        "write" | "edit" => Risk::Write,
        "web_fetch" | "web_search" => Risk::Network,
        _ => Risk::Exec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_read_tools() {
        for t in ["read", "ls", "find", "grep"] {
            assert_eq!(classify(t), Risk::Read, "tool {t} should be Read");
        }
    }

    #[test]
    fn classifies_write_tools() {
        for t in ["write", "edit"] {
            assert_eq!(classify(t), Risk::Write, "tool {t} should be Write");
        }
    }

    #[test]
    fn classifies_exec_tools() {
        assert_eq!(classify("bash"), Risk::Exec);
    }

    #[test]
    fn classifies_network_tools() {
        for t in ["web_fetch", "web_search"] {
            assert_eq!(classify(t), Risk::Network, "tool {t} should be Network");
        }
    }

    #[test]
    fn unknown_tool_defaults_to_exec() {
        assert_eq!(classify("never_heard_of_it"), Risk::Exec);
    }

    #[test]
    fn read_is_not_destructive() {
        assert!(!Risk::Read.is_destructive());
    }

    #[test]
    fn write_exec_network_are_destructive() {
        assert!(Risk::Write.is_destructive());
        assert!(Risk::Exec.is_destructive());
        assert!(Risk::Network.is_destructive());
    }

    #[test]
    fn risk_serializes_as_snake_case() {
        assert_eq!(serde_json::to_string(&Risk::Read).unwrap(), "\"read\"");
        assert_eq!(
            serde_json::to_string(&Risk::Network).unwrap(),
            "\"network\""
        );
    }
}
