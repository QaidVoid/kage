//! History compaction.
//!
//! When the running token budget approaches the model's context window the
//! loop summarizes the oldest turns into one synthetic user message wrapped
//! in `<summary>...</summary>` framing and drops the originals. The framing
//! plus user role keeps the post-compaction history valid for providers like
//! ZAI/GLM and Anthropic that require strict role ordering.
//!
//! The summarization is itself a model call: the prior turns are serialized
//! into a plain-text transcript and sent as one user-role prompt (so the
//! request never violates ordering rules), with a brief instruction asking
//! for a concise narrative summary. The returned text becomes the body of
//! the synthetic user message.

use std::fmt::Write as _;

use kage_core::{CancelFlag, Content, LoopError, LoopEvent, Message, MessageId, Role};
use kage_provider::{Provider, ProviderEvent, StreamRequest};

use crate::run::emit_one;
use crate::{AgentContext, Hooks, LoopConfig, TokenBudget};

/// Number of recent turns kept verbatim. Older turns are summarized.
const KEEP_RECENT: usize = 4;

/// Framing wrapper for the synthetic summary message that replaces the
/// drained history. Mirrors pi-mono's `COMPACTION_SUMMARY_PREFIX/SUFFIX`
/// so providers see a clearly labelled context block rather than a
/// rogue assistant turn. Exposed so the resume path can detect the
/// same framing in replayed history and route it back through the
/// compaction widget instead of rendering it as a plain assistant
/// block.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// Closing framing for the synthetic compaction summary message. See
/// [`COMPACTION_SUMMARY_PREFIX`].
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Inspect the agent context and, if usage is past the threshold, summarize
/// the oldest turns and replace them with one synthetic user message that
/// frames the summary so downstream providers accept the conversation.
///
/// Returns whether compaction ran.
pub(crate) fn maybe_compact<F: FnMut(LoopEvent)>(
    cx: &mut AgentContext,
    config: LoopConfig,
    provider: &dyn Provider,
    cancel: &CancelFlag,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<bool, LoopError> {
    if !should_compact(cx, config) {
        return Ok(false);
    }
    run_compaction(cx, provider, cancel, hooks, emit)
}

/// Force a compaction pass right now, ignoring the token-budget
/// threshold. Used by the `:compact` and `/compact` commands so the
/// user can shrink history on demand. Returns `false` when there is
/// not enough history to compact (history at or below `KEEP_RECENT`).
pub fn force_compact<F: FnMut(LoopEvent)>(
    cx: &mut AgentContext,
    provider: &dyn Provider,
    cancel: &CancelFlag,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<bool, LoopError> {
    run_compaction(cx, provider, cancel, hooks, emit)
}

fn run_compaction<F: FnMut(LoopEvent)>(
    cx: &mut AgentContext,
    provider: &dyn Provider,
    cancel: &CancelFlag,
    hooks: &mut dyn Hooks,
    emit: &mut F,
) -> Result<bool, LoopError> {
    if cx.history.len() <= KEEP_RECENT {
        return Ok(false);
    }

    let split = cx.history.len() - KEEP_RECENT;
    let to_summarize: Vec<Message> = cx.history.drain(..split).collect();
    let summary_text = summarize(provider, &cx.model, &to_summarize, cancel)?;
    let summary_body =
        format!("{COMPACTION_SUMMARY_PREFIX}{summary_text}{COMPACTION_SUMMARY_SUFFIX}");

    let summary_msg = Message {
        role: Role::User,
        content: vec![Content::Text {
            text: summary_body.clone(),
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
            summary: summary_body,
        },
    );
    Ok(true)
}

fn should_compact(cx: &AgentContext, config: LoopConfig) -> bool {
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
    // Compare the *most recent* turn's full prompt size to the
    // window. Summing `used_input` would triple-count history
    // because each turn's `usage.input` already contains the entire
    // prior conversation.
    cx.budget.current_context >= threshold
}

/// Instruction appended to the serialized conversation when asking the
/// model to summarize. Kept brief and prescriptive: providers like ZAI
/// reject ambiguous formatting and we want plain text output.
const SUMMARIZE_INSTRUCTION: &str = "Summarize the conversation above into a concise narrative. Capture decisions, outstanding \
     questions, file paths, and any tool results that future turns will need. Plain text, no \
     headings, no markdown, no bullet lists. Preserve any concrete identifiers verbatim \
     (commit hashes, file paths, error codes).";

/// Serialize the to-be-summarized history into a plain-text transcript.
///
/// Sending the original messages verbatim risks two consecutive same-role
/// turns (e.g. ending on a User turn and then appending the User-role
/// summarize instruction), which providers like ZAI/GLM reject with
/// `"messages parameter is illegal"`. Folding everything into a single
/// User message sidesteps the ordering rules entirely.
fn serialize_conversation(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::ToolResult => "tool_result",
            Role::System => "system",
        };
        out.push_str("=== ");
        out.push_str(role);
        out.push_str(" ===\n");
        for block in &msg.content {
            match block {
                Content::Text { text } | Content::Thinking { text } => {
                    out.push_str(text);
                    out.push('\n');
                }
                Content::ToolCall { name, input, .. } => {
                    let _ = writeln!(out, "[tool_call {name}] {input}");
                }
                Content::ToolResultBlock {
                    output, is_error, ..
                } => {
                    let tag = if *is_error {
                        "tool_error"
                    } else {
                        "tool_result"
                    };
                    let _ = writeln!(out, "[{tag}] {output}");
                }
                Content::Image { mime, .. } => {
                    let _ = writeln!(out, "[image {mime}]");
                }
                Content::Custom { kind, data } => {
                    let _ = writeln!(out, "[custom {kind}] {data}");
                }
            }
        }
        out.push('\n');
    }
    out
}

fn summarize(
    provider: &dyn Provider,
    model: &str,
    messages: &[Message],
    cancel: &CancelFlag,
) -> Result<String, LoopError> {
    let transcript = serialize_conversation(messages);
    let prompt = format!("<conversation>\n{transcript}</conversation>\n\n{SUMMARIZE_INSTRUCTION}");
    let payload = vec![Message::new(
        Role::User,
        vec![Content::Text { text: prompt }],
        None,
    )];
    let mut req = StreamRequest::new(model, payload);
    req.system = Some(SUMMARIZE_INSTRUCTION.to_owned());
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
            // The compaction check now compares the *most recent*
            // turn's context fill to the threshold; mirror
            // `used_input` here so existing tests (which were
            // written before the split) keep their original intent.
            current_context: used_input,
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

        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
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

        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
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
        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |ev| {
            events.push(ev);
        })
        .unwrap();
        assert!(ran);
        assert_eq!(cx.history.len(), 1 + KEEP_RECENT);
        assert_eq!(
            cx.history[0].role,
            Role::User,
            "synthetic summary must be User role so providers like ZAI/Anthropic accept the post-compaction history"
        );
        match &cx.history[0].content[0] {
            Content::Text { text } => {
                assert!(text.contains("you discussed X"));
                assert!(text.contains("<summary>"));
                assert!(text.contains("compacted"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
        assert!(events.iter().any(
            |e| matches!(e, LoopEvent::Compaction { kept, summarized, .. }
                if *kept == KEEP_RECENT && *summarized == 10 - KEEP_RECENT)
        ));
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

        let res = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {});
        assert!(matches!(res, Err(LoopError::Provider { .. })));
    }

    #[test]
    fn serialize_conversation_keeps_role_markers_and_text() {
        let msgs = vec![user_msg("hello"), assistant_msg("hi back")];
        let out = serialize_conversation(&msgs);
        assert!(out.contains("=== user ==="));
        assert!(out.contains("=== assistant ==="));
        assert!(out.contains("hello"));
        assert!(out.contains("hi back"));
    }
}
