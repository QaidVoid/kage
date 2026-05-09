//! History compaction.
//!
//! When the running token budget approaches the model's context window the
//! loop summarizes the oldest turns into one synthetic assistant message and
//! drops the originals. This keeps long-running sessions from running off
//! the end of the context window.
//!
//! The summarization is itself a model call: the loop sends the doomed
//! turns to the same provider with a small "summarize this conversation"
//! system prompt and a stripped-down request (no tools, no thinking). The
//! returned text becomes the synthetic message.

use kage_core::{CancelFlag, Content, LoopError, LoopEvent, Message, MessageId, Role};
use kage_provider::{Provider, ProviderEvent, StreamRequest};

use crate::run::emit_one;
use crate::{AgentContext, Hooks, LoopConfig, TokenBudget};

/// Number of recent turns kept verbatim. Older turns are summarized.
const KEEP_RECENT: usize = 4;

/// Inspect the agent context and, if usage is past the threshold, summarize
/// the oldest turns and replace them with one synthetic assistant message.
///
/// Returns whether compaction ran.
pub(crate) fn maybe_compact<F: FnMut(LoopEvent)>(
    cx: &mut AgentContext,
    config: &LoopConfig,
    provider: &dyn Provider,
    cancel: &CancelFlag,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<bool, LoopError> {
    if !should_compact(cx, config) {
        return Ok(false);
    }
    if cx.history.len() <= KEEP_RECENT {
        return Ok(false);
    }

    let split = cx.history.len() - KEEP_RECENT;
    let to_summarize: Vec<Message> = cx.history.drain(..split).collect();
    let summary_text = summarize(provider, &cx.model, &to_summarize, cancel)?;

    let summary_msg = Message {
        role: Role::Assistant,
        content: vec![Content::Text {
            text: format!("[summary of {split} earlier turns]\n{summary_text}"),
        }],
        id: MessageId::new(),
        parent: None,
        ts: chrono::Utc::now(),
    };
    cx.history.insert(0, summary_msg);
    cx.budget = TokenBudget::default();

    emit_one(
        hooks,
        emit,
        LoopEvent::Compaction {
            kept: KEEP_RECENT,
            summarized: split,
        },
    );
    Ok(true)
}

fn should_compact(cx: &AgentContext, config: &LoopConfig) -> bool {
    if config.compaction_threshold <= 0.0 || cx.context_window == 0 {
        return false;
    }
    // Apply threshold via permille arithmetic to keep the math in integers.
    let frac = config.compaction_threshold.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let permille = (frac * 1000.0).round() as u64;
    let threshold = cx
        .context_window
        .saturating_mul(permille)
        .saturating_div(1000);
    cx.budget.used_input >= threshold
}

fn summarize(
    provider: &dyn Provider,
    model: &str,
    messages: &[Message],
    cancel: &CancelFlag,
) -> Result<String, LoopError> {
    let mut req = StreamRequest::new(model, messages.to_vec());
    req.system = Some(
        "Summarize the conversation above into a concise narrative. Capture decisions, \
         outstanding questions, and any tool results that future turns will need. Plain \
         text, no headings, no markdown."
            .into(),
    );
    let stream = provider
        .stream(req, cancel)
        .map_err(|e| LoopError::Provider {
            message: e.to_string(),
        })?;

    let mut text = String::new();
    for event in stream {
        if cancel.is_cancelled() {
            return Err(LoopError::Cancelled);
        }
        let event = event.map_err(|e| LoopError::Provider {
            message: e.to_string(),
        })?;
        match event {
            ProviderEvent::TextDelta { delta } => text.push_str(&delta),
            ProviderEvent::MessageEnd { .. } => return Ok(text),
            _ => {}
        }
    }
    Err(LoopError::Provider {
        message: "summary stream ended without MessageEnd".into(),
    })
}

#[cfg(test)]
mod tests {
    use kage_core::{Content, Role, TokenUsage};
    use kage_provider::{ProviderEvent, StopReason, testing::MockProvider};

    use super::*;
    use crate::NoopHooks;

    fn user_msg(text: &str) -> Message {
        Message::new(
            Role::User,
            vec![Content::Text {
                text: text.to_owned(),
            }],
            None,
        )
    }

    fn assistant_msg(text: &str) -> Message {
        Message::new(
            Role::Assistant,
            vec![Content::Text {
                text: text.to_owned(),
            }],
            None,
        )
    }

    fn loaded_context(used_input: u64, history_len: usize) -> AgentContext {
        let mut cx = AgentContext::new("mock:m", "");
        cx.budget = TokenBudget {
            used_input,
            ..Default::default()
        };
        for i in 0..history_len {
            cx.history.push(if i % 2 == 0 {
                user_msg(&format!("turn {i}"))
            } else {
                assistant_msg(&format!("reply {i}"))
            });
        }
        cx
    }

    #[test]
    fn skipped_when_under_threshold() {
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig::default();
        let mut cx = loaded_context(1_000, 20);
        cx.context_window = 200_000;

        let ran =
            maybe_compact(&mut cx, &cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(!ran);
        assert_eq!(cx.history.len(), 20);
    }

    #[test]
    fn skipped_when_history_too_short() {
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig::default();
        let mut cx = loaded_context(u64::MAX / 2, 3);
        cx.context_window = 200_000;

        let ran =
            maybe_compact(&mut cx, &cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(!ran);
        assert_eq!(cx.history.len(), 3);
    }

    #[test]
    fn compacts_and_emits_event_when_over_threshold() {
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "you discussed X".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let mut events = Vec::new();
        let ran = maybe_compact(&mut cx, &cfg, &provider, &cancel, &mut hooks, &mut |ev| {
            events.push(ev);
        })
        .unwrap();
        assert!(ran);
        assert_eq!(cx.history.len(), 1 + KEEP_RECENT);
        match &cx.history[0].content[0] {
            Content::Text { text } => {
                assert!(text.contains("you discussed X"));
                assert!(text.contains("summary"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LoopEvent::Compaction { kept, summarized }
                if *kept == KEEP_RECENT && *summarized == 10 - KEEP_RECENT))
        );
        assert_eq!(cx.budget, TokenBudget::default());
    }

    #[test]
    fn summary_provider_failure_propagates() {
        let provider = MockProvider::replaying(vec![Err(kage_provider::ProviderError::Auth(
            "no key".into(),
        ))]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = loaded_context(150_000, 10);
        cx.context_window = 200_000;

        let res = maybe_compact(&mut cx, &cfg, &provider, &cancel, &mut hooks, &mut |_| {});
        assert!(matches!(res, Err(LoopError::Provider { .. })));
    }
}
