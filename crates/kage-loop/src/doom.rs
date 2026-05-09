//! Detect tool-call doom loops and steer the model out of them.
//!
//! A doom loop is the model repeating the same failing tool call: same name,
//! same input, error result, several times in a row. The loop can't tell
//! the model "this isn't working" without a nudge, so when the same call
//! fails three times in a row this module synthesizes a steering message
//! that the loop injects into history before the next turn.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Streak threshold before we synthesize a steering message.
const STREAK_LIMIT: u32 = 3;

/// State tracking the most recent (name, input-hash) and how many times it
/// has failed in a row.
#[derive(Debug, Default)]
pub(crate) struct DoomTracker {
    last: Option<(String, u64)>,
    streak: u32,
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
        let same = matches!(
            &self.last,
            Some((prev_name, prev_hash)) if prev_name == name && *prev_hash == hash
        );

        if !is_error {
            self.last = None;
            self.streak = 0;
            return None;
        }

        if same {
            self.streak = self.streak.saturating_add(1);
        } else {
            self.last = Some((name.to_owned(), hash));
            self.streak = 1;
        }

        if self.streak >= STREAK_LIMIT {
            let msg = format!(
                "You have called the '{name}' tool with the same input {streak} times in a \
                 row and each call has returned an error. Stop, take stock, and try a \
                 different approach.",
                streak = self.streak,
            );
            self.last = None;
            self.streak = 0;
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
}
