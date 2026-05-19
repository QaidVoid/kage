//! Knobs controlling one agent-loop run.

use serde::{Deserialize, Serialize};

/// How the loop drains queued steering and follow-up messages from a hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SteeringMode {
    /// Drain one message per turn. The hook is polled once and the returned
    /// `Some(text)` is pushed as a user message; the next message stays
    /// queued for the next turn. This is the historical kage behavior.
    #[default]
    OneAtATime,
    /// Drain every queued message at once. The hook is polled in a loop
    /// until it returns `None`; all collected messages are concatenated
    /// with blank-line separators into a single user message. Use this
    /// when latency matters more than per-turn pacing.
    All,
}

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
    /// How the loop drains queued steering messages each turn.
    #[serde(default)]
    pub steering_mode: SteeringMode,
    /// How the loop drains queued follow-up messages each turn.
    #[serde(default)]
    pub followup_mode: SteeringMode,
    /// How many times to re-issue a turn whose provider request failed
    /// transiently (a stalled or dropped stream, a 5xx, rate limiting)
    /// before giving up. `0` disables auto-retry (the pre-recovery
    /// behavior: one attempt, then surface the error). The
    /// conversation is preserved across a retry; only the failed
    /// partial turn is discarded so the re-request is clean.
    #[serde(default = "default_max_provider_retries")]
    pub max_provider_retries: u32,
}

/// Serde default for [`LoopConfig::max_provider_retries`]: an old
/// config file without the key still gets auto-retry.
fn default_max_provider_retries() -> u32 {
    4
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            parallel_tools: false,
            compaction_threshold: 0.8,
            steering_mode: SteeringMode::OneAtATime,
            followup_mode: SteeringMode::OneAtATime,
            max_provider_retries: default_max_provider_retries(),
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
            steering_mode: SteeringMode::All,
            followup_mode: SteeringMode::OneAtATime,
            max_provider_retries: 2,
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: LoopConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
        let legacy: LoopConfig =
            serde_json::from_str(r#"{"parallel_tools":false,"compaction_threshold":0.8}"#).unwrap();
        assert_eq!(legacy.max_provider_retries, 4);
    }

    #[test]
    fn steering_mode_default_is_one_at_a_time() {
        let cfg = LoopConfig::default();
        assert_eq!(cfg.steering_mode, SteeringMode::OneAtATime);
        assert_eq!(cfg.followup_mode, SteeringMode::OneAtATime);
    }
}
