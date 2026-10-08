//! Detect tool-call doom loops and steer the model out of them.
//!
//! A doom loop is the model repeating the same failing tool call: same name,
//! same input, error result, several times in a row. The loop can't tell
//! the model "this isn't working" without a nudge, so when the same call
//! fails three times in a row this module synthesizes a steering message
//! that the loop injects into history before the next turn. Streaks are
//! tracked per (name, input-hash) key, so failures alternating across two
//! tools still build a streak on each.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

/// Streak threshold before we synthesize a steering message.
const STREAK_LIMIT: u32 = 3;

/// Maximum distinct failing keys tracked. A model that fails with a fresh
/// input every time would otherwise grow the map for the whole run; past
/// the cap the oldest key is evicted.
const KEY_CAP: usize = 32;

/// Per-key failure streaks. The queue holds the keys in insertion order
/// so eviction can drop the oldest when [`KEY_CAP`] is reached.
#[derive(Debug, Default)]
pub(crate) struct DoomTracker {
    streaks: HashMap<(String, u64), u32>,
    order: VecDeque<(String, u64)>,
}

impl DoomTracker {
    /// Record one tool-call outcome. Returns a steering message to inject as
    /// a user turn if the streak hit [`STREAK_LIMIT`].
    pub(crate) fn observe(
        &mut self,
        name: &str,
        input: &serde_json::Value,
        is_error: bool,
    ) -> Option<String> {
        let hash = hash_value(input);

        if !is_error {
            // A success clears every streak: "in a row" means failures
            // with nothing successful in between.
            self.streaks.clear();
            self.order.clear();
            return None;
        }

        let key = (name.to_owned(), hash);
        if !self.streaks.contains_key(&key) {
            if self.streaks.len() >= KEY_CAP
                && let Some(oldest) = self.order.pop_front()
            {
                self.streaks.remove(&oldest);
            }
            self.order.push_back(key.clone());
        }
        let streak = self.streaks.entry(key.clone()).or_insert(0);
        *streak = streak.saturating_add(1);
        let streak = *streak;

        if streak >= STREAK_LIMIT {
            let msg = format!(
                "You have called the '{name}' tool with the same input {streak} times in a \
                 row and each call has returned an error. Stop, take stock, and try a \
                 different approach."
            );
            self.streaks.remove(&key);
            self.order.retain(|tracked| *tracked != key);
            Some(msg)
        } else {
            None
        }
    }
}

/// Stable, order-insensitive hash of a JSON value.
///
/// `serde_json::Value` does not implement `Hash`, so we hash its serialized
/// form. JSON object key order is not guaranteed by `Value::to_string`, so
/// callers should be aware that two semantically equal objects with reordered
/// keys may hash differently. For doom-loop detection that is fine: the model
/// emits its own JSON and tends to repeat the same key order.
fn hash_value(v: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    v.to_string().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_failures_dont_trigger() {
        let mut t = DoomTracker::default();
        assert!(
            t.observe("read", &serde_json::json!({"path":"a"}), true)
                .is_none()
        );
        assert!(
            t.observe("read", &serde_json::json!({"path":"b"}), true)
                .is_none()
        );
        assert!(
            t.observe("read", &serde_json::json!({"path":"c"}), true)
                .is_none()
        );
    }

    #[test]
    fn same_failure_three_times_triggers() {
        let mut t = DoomTracker::default();
        let input = serde_json::json!({"path": "missing.txt"});
        assert!(t.observe("read", &input, true).is_none());
        assert!(t.observe("read", &input, true).is_none());
        let msg = t.observe("read", &input, true).unwrap();
        assert!(msg.contains("'read'"));
        assert!(msg.contains("3 times"));
    }

    #[test]
    fn streak_resets_after_steering_emitted() {
        let mut t = DoomTracker::default();
        let input = serde_json::json!({"x": 1});
        let _ = t.observe("foo", &input, true);
        let _ = t.observe("foo", &input, true);
        let _ = t.observe("foo", &input, true);
        // Same call again should not re-trigger immediately.
        assert!(t.observe("foo", &input, true).is_none());
    }

    #[test]
    fn success_resets_streak() {
        let mut t = DoomTracker::default();
        let input = serde_json::json!({"x": 1});
        let _ = t.observe("foo", &input, true);
        let _ = t.observe("foo", &input, true);
        // Success in between resets.
        assert!(t.observe("foo", &input, false).is_none());
        assert!(t.observe("foo", &input, true).is_none());
    }

    #[test]
    fn different_input_resets_streak() {
        let mut t = DoomTracker::default();
        let _ = t.observe("foo", &serde_json::json!({"x": 1}), true);
        let _ = t.observe("foo", &serde_json::json!({"x": 1}), true);
        // Different input -> streak resets to 1.
        assert!(
            t.observe("foo", &serde_json::json!({"x": 2}), true)
                .is_none()
        );
        assert!(
            t.observe("foo", &serde_json::json!({"x": 2}), true)
                .is_none()
        );
        let msg = t.observe("foo", &serde_json::json!({"x": 2}), true);
        assert!(msg.is_some());
    }

    /// Failures alternating across two tools must still build a streak per
    /// tool: the third failure of the first tool triggers.
    #[test]
    fn alternating_failures_across_two_tools_trigger_per_key() {
        let mut t = DoomTracker::default();
        let a = serde_json::json!({"tool": "a"});
        let b = serde_json::json!({"tool": "b"});
        assert!(t.observe("read", &a, true).is_none());
        assert!(t.observe("write", &b, true).is_none());
        assert!(t.observe("read", &a, true).is_none());
        assert!(t.observe("write", &b, true).is_none());
        let msg = t.observe("read", &a, true).expect("read's third failure");
        assert!(msg.contains("'read'"));
        assert!(msg.contains("3 times"));
        // The other key keeps its own streak: one more failure triggers it.
        let msg = t.observe("write", &b, true).expect("write's third failure");
        assert!(msg.contains("'write'"));
    }

    /// Distinct failing keys never grow the map past the eviction cap.
    #[test]
    fn distinct_failing_keys_cannot_grow_the_map_past_the_cap() {
        let mut t = DoomTracker::default();
        for i in 0..40 {
            let input = serde_json::json!({"i": i});
            assert!(t.observe("read", &input, true).is_none());
            assert!(
                t.streaks.len() <= 32,
                "map grew to {} keys",
                t.streaks.len()
            );
        }
        assert_eq!(t.streaks.len(), 32);
        assert_eq!(t.order.len(), 32);
    }

    /// A success clears every key, keeping the "in a row" semantics across
    /// the per-key map.
    #[test]
    fn success_clears_every_tracked_key() {
        let mut t = DoomTracker::default();
        let _ = t.observe("read", &serde_json::json!({"x": 1}), true);
        let _ = t.observe("write", &serde_json::json!({"y": 2}), true);
        let _ = t.observe("read", &serde_json::json!({"x": 1}), true);
        assert!(
            t.observe("read", &serde_json::json!({"x": 1}), false)
                .is_none()
        );
        assert!(t.streaks.is_empty());
        assert!(t.order.is_empty());
        // Fresh streaks after the clear.
        assert!(
            t.observe("read", &serde_json::json!({"x": 1}), true)
                .is_none()
        );
    }
}
