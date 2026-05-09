//! Host hook trait for steering the agent loop.
//!
//! Hooks let a host (CLI, TUI, plugin runtime) observe and influence each
//! turn without coupling the loop to specific UIs. Default implementations
//! make the trait noop-by-default: a host overrides only the methods it cares
//! about.

use kage_core::{LoopEvent, ToolOutput};

/// Host-supplied callbacks fired during a [`run`](crate::run).
///
/// All methods take `&mut self` so hosts can accumulate state (logs,
/// permission decisions, queued steering messages) without interior
/// mutability. Calls are sequential: the loop never invokes two hook
/// methods concurrently on the same hook instance.
pub trait Hooks {
    /// Fired before a tool's `execute` runs.
    ///
    /// Return `None` to let the tool execute normally. Return `Some(output)`
    /// to short-circuit: the loop skips the real tool and treats the
    /// returned [`ToolOutput`] as if the tool produced it. Use this for
    /// permission denials, dry-run modes, or test fixtures.
    fn before_tool_call(&mut self, name: &str, input: &serde_json::Value) -> Option<ToolOutput> {
        let _ = (name, input);
        None
    }

    /// Fired after a tool produces an output (real or short-circuited).
    ///
    /// Return value replaces the output the loop appends to history. Use
    /// this to redact secrets, truncate output, or attach metadata.
    fn after_tool_call(&mut self, name: &str, output: ToolOutput) -> ToolOutput {
        let _ = name;
        output
    }

    /// Fired for every [`LoopEvent`] the loop emits.
    ///
    /// Receives events before they reach the caller's emit callback. Use
    /// this for logging, metrics, or driving UI sinks.
    fn on_event(&mut self, event: &LoopEvent) {
        let _ = event;
    }

    /// Polled before each model turn.
    ///
    /// A `Some(text)` return value is appended to history as a user message
    /// before the next call to the provider. Use this to inject reminders,
    /// enforce policies, or prompt for clarification mid-conversation.
    fn get_steering(&mut self) -> Option<String> {
        None
    }

    /// Polled after the model declares the turn finished.
    ///
    /// A `Some(text)` return value re-enters the inner loop with the text
    /// as a new user message. Returning `None` lets the run end normally.
    fn get_followup(&mut self) -> Option<String> {
        None
    }
}

/// Default no-op [`Hooks`] implementation.
///
/// Use when a caller has no need to observe or influence the loop.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopHooks;

impl Hooks for NoopHooks {}

#[cfg(test)]
mod tests {
    use kage_core::{MessageId, TokenUsage};

    use super::*;

    #[derive(Default)]
    struct Recording {
        events: Vec<String>,
        tool_calls_before: Vec<String>,
        tool_calls_after: Vec<String>,
        steering: Option<String>,
        followup: Option<String>,
    }

    impl Hooks for Recording {
        fn before_tool_call(
            &mut self,
            name: &str,
            _input: &serde_json::Value,
        ) -> Option<ToolOutput> {
            self.tool_calls_before.push(name.to_owned());
            None
        }

        fn after_tool_call(&mut self, name: &str, output: ToolOutput) -> ToolOutput {
            self.tool_calls_after.push(name.to_owned());
            output
        }

        fn on_event(&mut self, event: &LoopEvent) {
            self.events.push(
                serde_json::to_value(event).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }

        fn get_steering(&mut self) -> Option<String> {
            self.steering.take()
        }

        fn get_followup(&mut self) -> Option<String> {
            self.followup.take()
        }
    }

    #[test]
    fn noop_hooks_compile_with_defaults() {
        let mut h = NoopHooks;
        assert!(
            h.before_tool_call("read", &serde_json::Value::Null)
                .is_none()
        );
        let out = ToolOutput {
            is_error: false,
            text: "hi".into(),
            structured: None,
        };
        let back = h.after_tool_call("read", out.clone());
        assert_eq!(back, out);
        h.on_event(&LoopEvent::MessageEnd {
            id: MessageId::new(),
            usage: TokenUsage::default(),
        });
        assert!(h.get_steering().is_none());
        assert!(h.get_followup().is_none());
    }

    #[test]
    fn recording_hook_captures_calls() {
        let mut h = Recording::default();
        h.before_tool_call("bash", &serde_json::json!({}));
        h.after_tool_call(
            "bash",
            ToolOutput {
                is_error: false,
                text: "ok".into(),
                structured: None,
            },
        );
        h.on_event(&LoopEvent::MessageStart {
            id: MessageId::new(),
        });
        assert_eq!(h.tool_calls_before, vec!["bash"]);
        assert_eq!(h.tool_calls_after, vec!["bash"]);
        assert_eq!(h.events, vec!["message_start"]);
    }

    #[test]
    fn steering_and_followup_can_be_queued() {
        let mut h = Recording {
            steering: Some("hold on".into()),
            followup: Some("anything else?".into()),
            ..Recording::default()
        };
        assert_eq!(h.get_steering().as_deref(), Some("hold on"));
        assert!(h.get_steering().is_none());
        assert_eq!(h.get_followup().as_deref(), Some("anything else?"));
    }
}
