//! Conversation state carried through one [`run`](crate::run) invocation.

use std::path::PathBuf;

use kage_core::{Message, TokenUsage};
use serde::{Deserialize, Serialize};

/// Running token totals consumed and produced over the life of an agent run.
///
/// Updated after every assistant turn from the provider-reported [`TokenUsage`].
/// The cumulative `used_*` fields are session-wide sums for cost and audit
/// purposes; [`Self::current_context`] is the most recent turn's
/// `input + output + cache_read + cache_write` and is what the compaction
/// threshold and the modeline percentage compare against the model's
/// context window. The two are different because providers report each
/// turn's `usage.input` as the *full prompt size* for that request - which
/// already includes the entire prior conversation - so summing across
/// turns triple-counts history. The `OpenCode` project takes the same
/// per-turn snapshot approach in `session/overflow.ts`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenBudget {
    /// Cumulative input tokens charged across all turns.
    pub used_input: u64,
    /// Cumulative output tokens emitted across all turns.
    pub used_output: u64,
    /// Cumulative cache-read tokens (counts against input but is cheaper).
    pub used_cache_read: u64,
    /// Cumulative cache-write tokens.
    pub used_cache_write: u64,
    /// Approximate active-context fill from the most recent turn:
    /// `input + output + cache_read + cache_write` of that single
    /// turn. Compaction and the modeline percentage compare this to
    /// the model's context window.
    pub current_context: u64,
}

impl TokenBudget {
    /// Sum input and output usage. Cache reads/writes are not double-counted.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.used_input.saturating_add(self.used_output)
    }

    /// Fold one turn's [`TokenUsage`] into the running totals and
    /// snapshot the per-turn context fill into [`Self::current_context`].
    pub fn add(&mut self, usage: TokenUsage) {
        self.used_input = self.used_input.saturating_add(usage.input);
        self.used_output = self.used_output.saturating_add(usage.output);
        self.used_cache_read = self.used_cache_read.saturating_add(usage.cache_read);
        self.used_cache_write = self.used_cache_write.saturating_add(usage.cache_write);
        self.current_context = usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write);
    }
}

/// Mutable state threaded through one agent run.
///
/// The loop appends to `history` after every turn, updates `budget` from the
/// provider's reported usage, and reads `model` + `system_prompt` to build
/// each provider request. Hosts may inspect or mutate the context between
/// calls to [`run`](crate::run); during a run the loop owns it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentContext {
    /// Ordered conversation history. Last entry is typically the most recent
    /// user turn before [`run`](crate::run) is invoked.
    pub history: Vec<Message>,
    /// Provider-qualified model id, for example `anthropic:claude-sonnet-4-6`.
    pub model: String,
    /// System prompt prepended to every model call.
    pub system_prompt: String,
    /// Working directory all filesystem-touching tools must scope under.
    pub workdir: PathBuf,
    /// Effective context window for the active model, in tokens. Used
    /// together with [`crate::LoopConfig::compaction_threshold`] to decide
    /// when to summarize older turns. Default is 200,000.
    pub context_window: u64,
    /// Running token totals.
    pub budget: TokenBudget,
}

impl AgentContext {
    /// Construct a fresh context with empty history, zero budget, and the
    /// process current working directory as `workdir`.
    #[must_use]
    pub fn new(model: impl Into<String>, system_prompt: impl Into<String>) -> Self {
        Self {
            history: Vec::new(),
            model: model.into(),
            system_prompt: system_prompt.into(),
            workdir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            context_window: 200_000,
            budget: TokenBudget::default(),
        }
    }

    /// Override the working directory.
    #[must_use]
    pub fn with_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = workdir.into();
        self
    }

    /// Override the model's context window in tokens.
    #[must_use]
    pub fn with_context_window(mut self, window: u64) -> Self {
        self.context_window = window;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_add_accumulates() {
        let mut b = TokenBudget::default();
        b.add(TokenUsage {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5,
        });
        b.add(TokenUsage {
            input: 200,
            output: 75,
            cache_read: 20,
            cache_write: 0,
        });
        assert_eq!(b.used_input, 300);
        assert_eq!(b.used_output, 125);
        assert_eq!(b.used_cache_read, 30);
        assert_eq!(b.used_cache_write, 5);
        assert_eq!(b.total(), 425);
    }

    #[test]
    fn budget_add_saturates_on_overflow() {
        let mut b = TokenBudget {
            used_input: u64::MAX - 5,
            ..Default::default()
        };
        b.add(TokenUsage {
            input: 100,
            output: 0,
            cache_read: 0,
            cache_write: 0,
        });
        assert_eq!(b.used_input, u64::MAX);
    }

    #[test]
    fn agent_context_new_starts_empty() {
        let cx = AgentContext::new("anthropic:claude-sonnet-4-6", "you are helpful");
        assert_eq!(cx.model, "anthropic:claude-sonnet-4-6");
        assert_eq!(cx.system_prompt, "you are helpful");
        assert!(cx.history.is_empty());
        assert_eq!(cx.budget, TokenBudget::default());
    }
}
