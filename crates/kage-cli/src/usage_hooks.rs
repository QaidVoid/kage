//! Hook wrapper that updates the live session-usage snapshot
//! (`SharedSessionUsage`) every time the loop emits a
//! [`LoopEvent::MessageEnd`].
//!
//! Without this wrapper the modeline only refreshes when
//! `run_with_hooks` returns - which is fine for a single-turn
//! response but stale for multi-turn tool flows. With it, the
//! modeline ticks forward as soon as each assistant turn finishes,
//! mid-flow, mirroring the cumulative `cx.budget` the loop maintains.

use kage_core::{LoopEvent, Message, TokenCost, ToolOutput};
use kage_loop::{Hooks, StreamRequest, TurnSummary};
use kage_tui::SharedSessionUsage;

/// Wraps an inner [`Hooks`] implementation and accumulates token
/// usage from every [`LoopEvent::MessageEnd`] into the shared
/// `session_usage` handle. Other hook methods delegate to `inner`
/// unchanged.
pub struct UsageHooks<H> {
    /// Hook this wrapper sits in front of (TUI display, session
    /// recording, plugin dispatch). All non-`MessageEnd` events are
    /// forwarded verbatim.
    inner: H,
    /// Shared snapshot the modeline reads. Populated incrementally
    /// here and re-anchored to `cx.budget` in the worker after the
    /// turn's outer call finishes.
    handle: SharedSessionUsage,
    /// Current `provider:model` id. Captured at hook-construction
    /// time; the worker rebuilds the wrapper on each turn so a model
    /// switch propagates here.
    model: String,
    /// Effective context window for `model`. Same lifecycle.
    context_window: u64,
}

impl<H: Hooks> UsageHooks<H> {
    /// Construct a wrapper for one turn's hook chain. `model` and
    /// `context_window` are stamped onto the snapshot every time a
    /// `MessageEnd` lands so the modeline stays in sync after a
    /// `:model` swap.
    pub fn new(inner: H, handle: SharedSessionUsage, model: String, context_window: u64) -> Self {
        Self {
            inner,
            handle,
            model,
            context_window,
        }
    }

    /// Recover the wrapped inner hooks once the loop returns. Lets
    /// the caller fire post-run lifecycle methods that aren't part
    /// of the [`Hooks`] trait (e.g.
    /// [`crate::plugins::PluginEventHooks::dispatch_agent_end`]).
    pub fn into_inner(self) -> H {
        self.inner
    }
}

impl<H: Hooks> Hooks for UsageHooks<H> {
    fn before_tool_call(&mut self, name: &str, input: &serde_json::Value) -> Option<ToolOutput> {
        self.inner.before_tool_call(name, input)
    }

    fn after_tool_call(&mut self, name: &str, output: ToolOutput) -> ToolOutput {
        self.inner.after_tool_call(name, output)
    }

    fn on_event(&mut self, event: &LoopEvent) {
        if let LoopEvent::MessageEnd { usage, .. } = event
            && let Ok(mut snap) = self.handle.lock()
        {
            snap.model.clone_from(&self.model);
            snap.context_window = self.context_window;
            snap.input_tokens = snap.input_tokens.saturating_add(usage.input);
            snap.output_tokens = snap.output_tokens.saturating_add(usage.output);
            snap.cache_read_tokens = snap.cache_read_tokens.saturating_add(usage.cache_read);
            snap.cache_write_tokens = snap.cache_write_tokens.saturating_add(usage.cache_write);
            // Replace, not add: this is a snapshot of the most recent
            // turn's full prompt size. Summing would triple-count
            // history (see [`SessionUsage::current_context`]).
            snap.current_context = usage
                .input
                .saturating_add(usage.output)
                .saturating_add(usage.cache_read)
                .saturating_add(usage.cache_write);
            if let Some((p_id, m_id)) = self.model.split_once(':')
                && let Some(model) = kage_provider::catalog::model(p_id, m_id)
                && let Some(cost_rate) = model.cost
            {
                let cost = TokenCost::from_usage(
                    usage,
                    cost_rate.input,
                    cost_rate.output,
                    cost_rate.cache_read,
                    cost_rate.cache_write,
                );
                snap.total_cost += cost.total;
            }
        }
        self.inner.on_event(event);
    }

    fn transform_context(&mut self, messages: &mut Vec<Message>) -> Result<(), String> {
        self.inner.transform_context(messages)
    }

    fn transform_provider_request(&mut self, req: &mut StreamRequest) -> Result<(), String> {
        self.inner.transform_provider_request(req)
    }

    fn prepare_compaction(&mut self, prep: &mut kage_loop::CompactionPrep) -> Result<(), String> {
        self.inner.prepare_compaction(prep)
    }

    fn on_turn_start(&mut self, index: u32) {
        self.inner.on_turn_start(index);
    }

    fn on_turn_end(&mut self, index: u32, had_tool_calls: bool) {
        self.inner.on_turn_end(index, had_tool_calls);
    }

    fn should_stop_after_turn(&mut self, summary: &TurnSummary) -> bool {
        self.inner.should_stop_after_turn(summary)
    }

    fn get_steering(&mut self) -> Option<String> {
        self.inner.get_steering()
    }

    fn get_followup(&mut self) -> Option<String> {
        self.inner.get_followup()
    }

    fn on_user_message(&mut self, message: &kage_core::Message) {
        self.inner.on_user_message(message);
    }
}

#[cfg(test)]
mod tests {
    use kage_loop::CompactionPrep;

    use super::*;

    #[derive(Default)]
    struct RecordingInner {
        prepare_calls: u32,
    }

    impl Hooks for RecordingInner {
        fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
            self.prepare_calls += 1;
            prep.summary_override = Some("from inner".to_owned());
            Ok(())
        }
    }

    fn prep() -> CompactionPrep {
        CompactionPrep {
            transcript: String::new(),
            instruction: String::new(),
            prompt: String::new(),
            model: "p:m".to_owned(),
            summarized: 0,
            kept: 0,
            summary_override: None,
        }
    }

    #[test]
    fn prepare_compaction_delegates_to_inner() {
        let mut hooks = UsageHooks::new(
            RecordingInner::default(),
            kage_tui::shared_session_usage(),
            "p:m".to_owned(),
            1000,
        );
        let mut p = prep();
        hooks.prepare_compaction(&mut p).expect("prepare ok");
        assert_eq!(p.summary_override.as_deref(), Some("from inner"));
        assert_eq!(hooks.into_inner().prepare_calls, 1);
    }
}
