//! Knobs controlling one agent-loop run.

use serde::{Deserialize, Serialize};

/// Loop-wide configuration.
///
/// Defaults are tuned for interactive use: 100 inner-loop iterations is
/// well above any sane tool-using turn, parallel tools are off because they
/// magnify blast radius, and compaction kicks in at 80% of the model's
/// context window.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopConfig {
    /// Hard cap on inner-loop iterations (model turns within one
    /// [`run`](crate::run)). Reaching it terminates the run with an error.
    pub max_iterations: u32,
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
            max_iterations: 100,
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
        assert_eq!(cfg.max_iterations, 100);
        assert!(!cfg.parallel_tools);
        assert!(cfg.compaction_threshold > 0.0 && cfg.compaction_threshold <= 1.0);
    }

    #[test]
    fn config_roundtrips_through_json() {
        let cfg = LoopConfig {
            max_iterations: 50,
            parallel_tools: true,
            compaction_threshold: 0.9,
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: LoopConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }
}
