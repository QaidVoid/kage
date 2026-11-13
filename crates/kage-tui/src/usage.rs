//! Live session-usage snapshot rendered in the chrome.
//!
//! [`SessionUsage`] is the small bundle of numbers the App updates from
//! the engine's state and usage events. The footer reads the model,
//! token totals and context-window fill from it, and the input rule
//! and the working row read the working flag.

use std::sync::{Arc, Mutex};

use kage_core::{Modalities, ThinkingLevel};

/// Snapshot of one session's running token totals plus the active
/// model and its context window, kept current from the engine's
/// session events.
///
/// Two scalars track tokens: [`Self::current_context`] is the most
/// recent turn's full prompt size (`input + output + cache_*`) and
/// drives the context-window fill percentage; the cumulative
/// `*_tokens` fields are session-wide sums for cost / audit
/// display. They differ because each turn's prompt carries the whole
/// prior history, so summing prompt sizes across turns would count it
/// again on every turn. `input_tokens` holds only the uncached part
/// of each prompt.
#[derive(Clone, Debug, Default, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent session flags mirrored from the engine"
)]
pub struct SessionUsage {
    /// Provider-qualified model id (`anthropic:claude-sonnet-4-6`).
    pub model: String,
    /// Cumulative uncached input tokens across every turn.
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
    /// Whether [`Self::current_context`] is a compaction's estimate
    /// waiting for the next turn's real usage.
    pub context_estimated: bool,
    /// Effective context window for `model`, in tokens. `0` when
    /// unknown (renderer hides the percentage in that case).
    pub context_window: u64,
    /// `true` while a run is in flight (provider streaming, tool
    /// dispatch, or compaction). The modeline reads this to paint a
    /// spinner.
    pub working: bool,
    /// Cumulative dollar cost across every turn this session. `0.0`
    /// when the active model has no known price.
    pub total_cost: f64,
    /// Thinking level the next run sends. `None` (and `Some(Off)`)
    /// leave the footer pill suppressed; the renderer draws
    /// `thinking <level>` for any other variant.
    pub thinking_level: Option<ThinkingLevel>,
    /// Whether [`Self::thinking_level`] is the automatic level rather
    /// than one the user chose.
    pub thinking_auto: bool,
    /// Levels the model accepts, lowest first. Empty when the model has
    /// no thinking setting.
    pub thinking_levels: Vec<ThinkingLevel>,
    /// Modalities the model accepts. Empty when unknown.
    pub input: Modalities,
    /// Session permission mode override. `None` hides the pill (the
    /// configured `[permissions]` rules decide); the renderer draws
    /// `perm:ask` / `perm:deny` for the matching override.
    pub permission_mode: Option<kage_core::permissions::PermissionAction>,
    /// Whether the session's swarm mode is on.
    pub swarm: bool,
    /// Whether the session's plan mode is on.
    pub plan: bool,
    /// Background shell commands still running.
    pub shells: u32,
    /// The modes the session's agent offers, when it offers any. Empty
    /// when the session has no mode switch.
    pub modes: Vec<kage_core::protocol::ModeChoice>,
    /// The id of the mode the session is in, as the agent names it.
    pub mode_value: Option<String>,
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

/// Compact token count in Kimi's style: `999`, `188k`, `650.2M`.
/// Millions keep one decimal trimmed of a trailing `.0`; thousands
/// round to whole `k`. Integer math throughout: token counts never
/// touch `f64`.
#[must_use]
pub fn compact_tokens(n: u64) -> String {
    const MILLION: u64 = 1_000_000;
    const HUNDRED_THOUSAND: u64 = 100_000;
    const THOUSAND: u64 = 1_000;
    if n >= MILLION {
        let tenths = half_up(n, HUNDRED_THOUSAND);
        let (whole, frac) = (tenths / 10, tenths % 10);
        if frac == 0 {
            format!("{whole}M")
        } else {
            format!("{whole}.{frac}M")
        }
    } else if n >= THOUSAND {
        format!("{}k", half_up(n, THOUSAND))
    } else {
        format!("{n}")
    }
}

/// `n` rounded to a multiple of `unit`, half up, in units, without the
/// overflow `n + unit / 2` would risk near `u64::MAX`.
fn half_up(n: u64, unit: u64) -> u64 {
    n / unit + u64::from(n % unit * 2 >= unit)
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

    /// The largest counts must format without an overflow in the
    /// rounding, and the rounding itself must not change.
    #[test]
    fn compact_tokens_rounds_half_up_without_overflowing() {
        assert_eq!(compact_tokens(u64::MAX), "18446744073709.6M");
        assert_eq!(compact_tokens(1_999_999), "2M");
        assert_eq!(compact_tokens(1_949_999), "1.9M");
        assert_eq!(compact_tokens(1_500_000), "1.5M");
        assert_eq!(compact_tokens(999_999), "1000k");
        assert_eq!(compact_tokens(999_499), "999k");
        assert_eq!(compact_tokens(999), "999");
    }
}
