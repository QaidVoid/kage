//! Live session-usage snapshot rendered in the bottom modeline.
//!
//! [`SessionUsage`] is the small bundle of numbers the host worker
//! thread updates after every agent turn. The renderer reads it via
//! an `Arc<Mutex<...>>` to paint a one-line strip below the input
//! card with model id, total token usage, and context-window fill.
//!
//! The modeline only appears when a host registers a usage handle on
//! the [`crate::App`]; without one, [`crate::layout::split`] is
//! called with `status_bottom_height = 0` and the row collapses.

use std::sync::{Arc, Mutex};

/// Snapshot of one session's running token totals plus the active
/// model and its context window. The host produces this from
/// [`kage_loop::AgentContext`] after every turn.
///
/// Two scalars track tokens: [`Self::current_context`] is the most
/// recent turn's full prompt size (`input + output + cache_*`) and
/// drives the context-window fill percentage; the cumulative
/// `*_tokens` fields are session-wide sums for cost / audit
/// display. They differ because providers report each turn's
/// `usage.input` as the entire prompt (including all prior history),
/// so summing across turns triple-counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionUsage {
    /// Provider-qualified model id (`anthropic:claude-sonnet-4-6`).
    pub model: String,
    /// Cumulative input tokens charged across every turn.
    pub input_tokens: u64,
    /// Cumulative output tokens emitted across every turn.
    pub output_tokens: u64,
    /// Cumulative cache-read tokens.
    pub cache_read_tokens: u64,
    /// Cumulative cache-write tokens.
    pub cache_write_tokens: u64,
    /// Sum of `input + output + cache_read + cache_write` for the
    /// most recent assistant turn. Compared to [`Self::context_window`]
    /// for the modeline percentage; left at `0` until the first turn
    /// finishes.
    pub current_context: u64,
    /// Effective context window for `model`, in tokens. `0` when
    /// unknown (renderer hides the percentage in that case).
    pub context_window: u64,
    /// `true` while the agent loop is mid-flight on a turn (provider
    /// streaming, tool dispatch, or compaction). The modeline
    /// reads this to paint a spinner; the host worker thread sets
    /// it `true` on `run_with_hooks` entry and `false` on return.
    pub working: bool,
}

impl SessionUsage {
    /// Total tokens consumed (cumulative `input + output`), used as
    /// a session-wide running tally separate from the per-turn
    /// context fill in [`Self::current_context`].
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Shared handle the host wires into [`crate::App`] and updates from
/// its worker thread after every turn.
pub type SharedSessionUsage = Arc<Mutex<SessionUsage>>;

/// Construct an empty handle.
#[must_use]
pub fn shared_session_usage() -> SharedSessionUsage {
    Arc::new(Mutex::new(SessionUsage::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_tokens_is_input_plus_output() {
        let u = SessionUsage {
            input_tokens: 1000,
            output_tokens: 250,
            ..SessionUsage::default()
        };
        assert_eq!(u.total_tokens(), 1250);
    }
}
