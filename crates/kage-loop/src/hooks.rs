//! Host hook trait for steering the agent loop.
//!
//! Hooks let a host (CLI, TUI, plugin runtime) observe and influence each
//! turn without coupling the loop to specific UIs. Default implementations
//! make the trait noop-by-default: a host overrides only the methods it cares
//! about.

use kage_core::{LoopEvent, Message, TokenUsage, ToolOutput};
use kage_provider::StreamRequest;

/// Outcome of a pre-action hook that can veto, patch, or pass through.
///
/// Used by hook variants where the host must run code *before* an action and
/// the hook needs to be able to block the action with a user-visible reason
/// or replace the target with a different value. Plain observation-style
/// hooks (e.g. [`Hooks::on_event`]) do not need this; transform-style hooks
/// that mutate in place (e.g. a future `transform_context`) keep their
/// `Result<()>` shape because they cannot meaningfully veto.
///
/// The generic parameter is the target of [`HookResult::Patch`]: a session
/// id for `session_before_switch`, a path for `session_before_fork`, etc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookResult<T> {
    /// The action should proceed unchanged.
    Proceed,
    /// The action is vetoed. The string is a user-facing reason the host can
    /// surface in the UI; pick wording the end user can act on, not stack
    /// traces.
    Cancel {
        /// Human-readable explanation for the veto. Surfaced verbatim by
        /// the host (toast, error block, etc.).
        reason: String,
    },
    /// The action should proceed against the patched target instead of the
    /// original. The host substitutes this value and runs the action.
    Patch(T),
}

impl<T> HookResult<T> {
    /// Map the patched target to a different type while preserving the
    /// `Proceed` and `Cancel` variants.
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> HookResult<U> {
        match self {
            Self::Proceed => HookResult::Proceed,
            Self::Cancel { reason } => HookResult::Cancel { reason },
            Self::Patch(t) => HookResult::Patch(f(t)),
        }
    }

    /// Returns `true` when the host should abandon the action.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancel { .. })
    }

    /// Returns the cancellation reason if the result is `Cancel`.
    #[must_use]
    pub fn cancel_reason(&self) -> Option<&str> {
        match self {
            Self::Cancel { reason } => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// Snapshot of a just-finished turn passed to [`Hooks::should_stop_after_turn`].
///
/// Carries the minimum a predicate needs to decide whether to abandon the
/// run: which turn this was, whether the assistant requested tool calls,
/// and what the provider reported for usage. Hosts that need richer state
/// (full assistant message, current history) should keep that state on
/// the hook implementation itself.
#[derive(Clone, Copy, Debug)]
pub struct TurnSummary {
    /// Zero-based turn index within the current `run`, matching the value
    /// passed to [`Hooks::on_turn_start`] / [`Hooks::on_turn_end`].
    pub index: u32,
    /// Whether the assistant requested at least one tool call. When
    /// `true`, returning `false` from `should_stop_after_turn` will let
    /// the inner loop dispatch those tools and continue.
    pub had_tool_calls: bool,
    /// Provider-reported token usage for this turn.
    pub usage: TokenUsage,
}

/// Mutable summarization plan handed to [`Hooks::prepare_compaction`]
/// just before history compaction calls the summarizer model.
///
/// Rewrite [`Self::prompt`] or [`Self::instruction`] to steer the
/// summary, or set [`Self::summary_override`] to skip the model call
/// entirely and use the given text as the summary body.
#[derive(Clone, Debug)]
pub struct CompactionPrep {
    /// Plain-text transcript of the turns being summarized. Read-only
    /// context; change what the model sees by rewriting `prompt`.
    pub transcript: String,
    /// System instruction for the summarization call. Mutable.
    pub instruction: String,
    /// Full user-role prompt the summarizer receives. Mutable.
    pub prompt: String,
    /// Model id the summarization call uses. Defaults to the live
    /// conversation model; rewrite it to route summarization to a
    /// cheaper or faster model.
    pub model: String,
    /// Number of messages being summarized away.
    pub summarized: usize,
    /// Number of recent messages kept verbatim after compaction.
    pub kept: usize,
    /// When `Some`, the loop skips the summarizer model call and uses
    /// this text as the summary body verbatim.
    pub summary_override: Option<String>,
}

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

    /// Fired immediately before the loop hands a built [`StreamRequest`]
    /// to the provider. Hosts can rewrite the request in place: inject a
    /// system header, strip or rewrite tools, swap the model, etc. The
    /// resulting request is what the provider actually receives.
    ///
    /// Returning an error aborts the turn with
    /// [`kage_core::LoopError::HookFailed`]. The default implementation is
    /// a no-op.
    ///
    /// Runs after [`Self::transform_context`]: that hook reshapes history,
    /// then `build_request` produces a [`StreamRequest`], then this hook
    /// can adjust the request as a whole.
    fn transform_provider_request(&mut self, req: &mut StreamRequest) -> Result<(), String> {
        let _ = req;
        Ok(())
    }

    /// Fired immediately before each provider turn, with the full message
    /// history in scope. Hosts can prune, redact, or rewrite messages in
    /// place; the loop sends the resulting `Vec<Message>` to the provider.
    ///
    /// Returning an error aborts the turn with
    /// [`kage_core::LoopError::HookFailed`]; the loop emits the terminal
    /// error event and returns. The default implementation is a no-op.
    ///
    /// Use cases: strip secrets from history before sending, trim old tool
    /// outputs that have rotted, inject a per-turn system reminder. Avoid
    /// expensive work here: this runs on every turn including compaction
    /// follow-ups.
    fn transform_context(&mut self, messages: &mut Vec<Message>) -> Result<(), String> {
        let _ = messages;
        Ok(())
    }

    /// Fired right before history compaction calls the summarizer
    /// model. Rewrite `prep.prompt` / `prep.instruction` to steer the
    /// summary, or set `prep.summary_override` to replace the summary
    /// outright and skip the model call.
    ///
    /// Returning an error aborts compaction with
    /// [`kage_core::LoopError::HookFailed`]. The default is a no-op.
    fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
        let _ = prep;
        Ok(())
    }

    /// Fired just before each inner-loop iteration begins a provider call.
    ///
    /// `index` is the zero-based turn index within the current `run`: the
    /// first provider call has `index == 0`. Compaction-induced extra calls
    /// do not advance the index; follow-up rounds do. Use this for per-turn
    /// timers, request logging, or plugin notifications.
    fn on_turn_start(&mut self, index: u32) {
        let _ = index;
    }

    /// Fired after the provider stream for the current turn closes.
    ///
    /// `had_tool_calls` is `true` when the model finished a turn that
    /// requested at least one tool call (the inner loop will continue);
    /// `false` when the turn produced text only and the inner loop will
    /// break. Pair with [`Self::on_turn_start`] for tok/s-style metrics.
    fn on_turn_end(&mut self, index: u32, had_tool_calls: bool) {
        let _ = (index, had_tool_calls);
    }

    /// Polled after every turn closes. Returning `true` short-circuits the
    /// run: pending tool calls are abandoned, follow-ups are not dequeued,
    /// and the loop returns `Ok(())` immediately. Use this for plan-mode
    /// plugins that halt after the model emits its plan, before the loop
    /// can execute any tools the plan requested.
    ///
    /// Fires after [`Self::on_turn_end`] and before any tool dispatch or
    /// follow-up handling. The default implementation always returns
    /// `false`, so the loop's existing behavior is unchanged.
    fn should_stop_after_turn(&mut self, summary: &TurnSummary) -> bool {
        let _ = summary;
        false
    }

    /// Consulted when the loop detects a tool-call doom loop: the same
    /// tool called with the same input failing several times in a row.
    /// `suggested` is the steering message the loop would inject as a
    /// user turn to break the loop.
    ///
    /// Return `Some(text)` to inject `text` (the suggestion unchanged or
    /// a rewrite), or `None` to suppress the nudge entirely. The default
    /// returns the suggestion unchanged, preserving the built-in
    /// behavior. Use this to localize the message, raise the bar, or
    /// disable doom steering for an autonomous agent that handles its
    /// own retries.
    fn on_doom_loop(&mut self, name: &str, suggested: String) -> Option<String> {
        let _ = name;
        Some(suggested)
    }

    /// Polled before each model turn.
    ///
    /// A `Some(text)` return value is appended to history as a user message
    /// before the next call to the provider. Use this to inject reminders,
    /// enforce policies, or prompt for clarification mid-conversation.
    fn get_steering(&mut self) -> Option<String> {
        None
    }

    /// Fired right after the loop appends a user message it produced
    /// itself: a drained steering message, a follow-up, or a synthetic
    /// nudge from the loop's stall guard. The first user message of a
    /// run does not flow through here because the host pushes it onto
    /// history before calling [`run`](crate::run).
    ///
    /// Use this to persist the just-added message: the agent loop does
    /// not emit user messages as [`LoopEvent`]s, so session writers and
    /// other recorders that hook [`Self::on_event`] would otherwise
    /// miss it. The default implementation is a no-op.
    fn on_user_message(&mut self, message: &Message) {
        let _ = message;
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
            terminate: false,
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
                terminate: false,
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
    fn hook_result_proceed_is_not_cancelled() {
        let r: HookResult<String> = HookResult::Proceed;
        assert!(!r.is_cancelled());
        assert!(r.cancel_reason().is_none());
    }

    #[test]
    fn hook_result_cancel_carries_reason() {
        let r: HookResult<String> = HookResult::Cancel {
            reason: "no".into(),
        };
        assert!(r.is_cancelled());
        assert_eq!(r.cancel_reason(), Some("no"));
    }

    #[test]
    fn hook_result_patch_preserves_target() {
        let r: HookResult<i32> = HookResult::Patch(42);
        match r {
            HookResult::Patch(v) => assert_eq!(v, 42),
            other => panic!("expected Patch, got {other:?}"),
        }
    }

    #[test]
    fn hook_result_map_preserves_variants() {
        let p: HookResult<i32> = HookResult::Patch(7);
        match p.map(|v| v * 2) {
            HookResult::Patch(v) => assert_eq!(v, 14),
            other => panic!("expected Patch, got {other:?}"),
        }
        let c: HookResult<i32> = HookResult::Cancel { reason: "x".into() };
        match c.map(|v| v + 1) {
            HookResult::Cancel { reason } => assert_eq!(reason, "x"),
            other => panic!("expected Cancel, got {other:?}"),
        }
        let s: HookResult<i32> = HookResult::Proceed;
        assert!(matches!(s.map(|v| v.to_string()), HookResult::Proceed));
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
