//! Knobs controlling one agent-loop run.

use serde::{Deserialize, Serialize};

/// Loop-wide configuration.
///
/// Defaults are tuned for interactive use: parallel tools are off because
/// they magnify blast radius, and compaction kicks in at 80% of the
/// model's context window. The agent loop has no iteration cap: a runaway
/// agent is bounded by user cancellation, compaction, and provider quota,
/// not by a magic number here.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopConfig {
    /// When true, tool calls within a single assistant turn run on
    /// independent threads. Order is preserved in the resulting tool-result
    /// messages.
    pub parallel_tools: bool,
    /// Trigger compaction once estimated token usage exceeds this fraction
    /// of the model's context window. Must be in `(0.0, 1.0]`.
    pub compaction_threshold: f32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            parallel_tools: false,
            compaction_threshold: 0.8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_are_sane() {
        let cfg = LoopConfig::default();
        assert!(!cfg.parallel_tools);
        assert!(cfg.compaction_threshold > 0.0 && cfg.compaction_threshold <= 1.0);
    }

    #[test]
    fn config_roundtrips_through_json() {
        let cfg = LoopConfig {
            parallel_tools: true,
            compaction_threshold: 0.9,
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: LoopConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }
}
