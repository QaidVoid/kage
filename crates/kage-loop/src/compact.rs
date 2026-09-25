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

use crate::{AgentContext, CompactionPrep, Hooks, LoopConfig, TokenBudget};

/// Target number of recent turns kept verbatim. Older turns are
/// summarized. The boundary may extend past this to keep an assistant
/// tool-call turn together with its results.
const KEEP_RECENT: usize = 4;

pub use kage_core::message::{COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX};

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
/// not enough history to compact (history at or below `KEEP_RECENT`,
/// or nothing summarizable before the keep window).
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

    // Pick the boundary first. Kept history may not open with a
    // `ToolResult` whose assistant tool-call turn was summarized away:
    // strict providers reject the orphaned result. Walk the boundary
    // backwards until it sits on a non-ToolResult message, pulling the
    // parent assistant turn into the keep window.
    let mut split = cx.history.len() - KEEP_RECENT;
    while split > 0 && cx.history[split].role == Role::ToolResult {
        split -= 1;
    }
    if split == 0 {
        return Ok(false);
    }
    let kept = cx.history.len() - split;

    // Summarize from a borrowed slice and splice only after every
    // fallible step below has succeeded, so a failed summary or hook
    // leaves the conversation intact.
    let transcript = serialize_conversation(&cx.history[..split]);
    let mut prep = CompactionPrep {
        prompt: format!("<conversation>\n{transcript}</conversation>\n\n{SUMMARIZE_INSTRUCTION}"),
        transcript,
        instruction: SUMMARIZE_INSTRUCTION.to_owned(),
        model: cx.model.clone(),
        summarized: split,
        kept,
        summary_override: None,
    };
    hooks
        .prepare_compaction(&mut prep)
        .map_err(|message| LoopError::HookFailed {
            hook: "compact_prepare".to_owned(),
            message,
        })?;
    let summary_text = match prep.summary_override {
        Some(text) => text,
        None => summarize(
            provider,
            &prep.model,
            &prep.prompt,
            &prep.instruction,
            cancel,
        )?,
    };
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
    cx.history.splice(..split, std::iter::once(summary_msg));
    cx.budget = TokenBudget::default();

    emit(LoopEvent::Compaction {
        kept,
        summarized: split,
        summary: summary_body,
    });
    Ok(true)
}

fn should_compact(cx: &AgentContext, config: LoopConfig) -> bool {
    if config.compaction_threshold <= 0.0 || cx.context_window == 0 {
        return false;
    }
    // Apply threshold via permille arithmetic to keep the math in integers.
    let frac = config.compaction_threshold.clamp(0.0, 1.0);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the fraction is clamped to 0..=1 first"
    )]
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
    prompt: &str,
    instruction: &str,
    cancel: &CancelFlag,
) -> Result<String, LoopError> {
    let payload = vec![Message::new(
        Role::User,
        vec![Content::Text {
            text: prompt.to_owned(),
        }],
        None,
    )];
    let mut req = StreamRequest::new(model, payload);
    req.system = Some(instruction.to_owned());
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
    use kage_provider::testing::{MockProvider, user_msg};
    use kage_provider::{ProviderEvent, StopReason};

    use super::*;
    use crate::NoopHooks;

    fn assistant_msg(text: &str) -> Message {
        Message::new(
            Role::Assistant,
            vec![Content::Text {
                text: text.to_owned(),
            }],
            None,
        )
    }

    fn assistant_tool_call(id: &str) -> Message {
        Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: kage_core::ToolCallId::new(id),
                name: "bash".into(),
                input: serde_json::json!({}),
            }],
            None,
        )
    }

    fn tool_result(id: &str) -> Message {
        Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: kage_core::ToolCallId::new(id),
                output: "out".into(),
                is_error: false,
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
    fn boundary_extends_backwards_to_keep_tool_group_intact() {
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "summary".into(),
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
        let mut cx = AgentContext::new("mock:m", "");
        cx.budget = TokenBudget {
            used_input: 150_000,
            current_context: 150_000,
            ..Default::default()
        };
        cx.context_window = 200_000;
        cx.history.push(user_msg("turn 0"));
        cx.history.push(assistant_msg("reply 1"));
        cx.history.push(user_msg("turn 2"));
        cx.history.push(assistant_tool_call("call_1"));
        cx.history.push(tool_result("call_1"));
        cx.history.push(user_msg("turn 5"));
        cx.history.push(assistant_msg("reply 6"));
        cx.history.push(user_msg("turn 7"));

        let mut events = Vec::new();
        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |ev| {
            events.push(ev);
        })
        .unwrap();
        assert!(ran);
        // The raw boundary (len 8 - KEEP_RECENT) lands on the tool
        // result at index 4; it must walk back to the assistant
        // tool-call turn at index 3, keeping the pair together.
        assert_eq!(cx.history.len(), 1 + 5);
        assert!(
            matches!(&cx.history[1].content[0], Content::ToolCall { .. }),
            "kept history must open with the parent tool-call turn, not a result"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::Compaction {
                kept: 5,
                summarized: 3,
                ..
            }
        )));
    }

    #[test]
    fn nothing_summarizable_before_keep_window_compacts_nothing() {
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = NoopHooks;
        let cfg = LoopConfig {
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        };
        let mut cx = AgentContext::new("mock:m", "");
        cx.budget = TokenBudget {
            used_input: 150_000,
            current_context: 150_000,
            ..Default::default()
        };
        cx.context_window = 200_000;
        cx.history.push(assistant_tool_call("call_1"));
        for _ in 0..4 {
            cx.history.push(tool_result("call_1"));
        }

        let ran = maybe_compact(&mut cx, cfg, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(!ran);
        assert_eq!(cx.history.len(), 5, "history must be untouched");
    }

    #[test]
    fn prepare_compaction_hook_can_redirect_the_summarizer_model() {
        struct RedirectModel;
        impl crate::Hooks for RedirectModel {
            fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
                assert_eq!(prep.model, "mock:m");
                prep.model = "mock:cheap".to_owned();
                Ok(())
            }
        }

        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "summary".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let cancel = CancelFlag::new();
        let mut hooks = RedirectModel;
        let mut cx = loaded_context(0, 10);

        let ran = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(ran);
        assert_eq!(
            provider.last_request().expect("a summarize request").model,
            "mock:cheap",
            "summarize must use the model the hook set on prep.model"
        );
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
        assert_eq!(
            cx.history.len(),
            10,
            "a failed summary must leave the history intact"
        );
        assert!(
            matches!(&cx.history[0].content[0], Content::Text { text } if text == "turn 0"),
            "a failed summary must not replace the oldest turn"
        );
    }

    #[test]
    fn prepare_compaction_override_skips_model_call() {
        struct OverrideHook;
        impl Hooks for OverrideHook {
            fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
                assert_eq!(prep.kept, KEEP_RECENT);
                assert_eq!(prep.summarized, 10 - KEEP_RECENT);
                assert!(prep.prompt.contains("<conversation>"));
                prep.summary_override = Some("PLUGIN WROTE THIS".to_owned());
                Ok(())
            }
        }
        // Empty replay: if summarize() were called it would error, so
        // a clean run proves the model call was skipped.
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = OverrideHook;
        let mut cx = loaded_context(1_000, 10);
        cx.context_window = 200_000;

        let ran = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(ran);
        match &cx.history[0].content[0] {
            Content::Text { text } => {
                assert!(text.contains("PLUGIN WROTE THIS"));
                assert!(text.contains("<summary>"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn prepare_compaction_error_aborts_with_hook_failed() {
        struct FailHook;
        impl Hooks for FailHook {
            fn prepare_compaction(&mut self, _prep: &mut CompactionPrep) -> Result<(), String> {
                Err("nope".to_owned())
            }
        }
        let provider = MockProvider::replaying(vec![]);
        let cancel = CancelFlag::new();
        let mut hooks = FailHook;
        let mut cx = loaded_context(1_000, 10);
        cx.context_window = 200_000;

        let res = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {});
        match res {
            Err(LoopError::HookFailed { hook, message }) => {
                assert_eq!(hook, "compact_prepare");
                assert_eq!(message, "nope");
            }
            other => panic!("expected HookFailed, got {other:?}"),
        }
        assert_eq!(
            cx.history.len(),
            10,
            "a failed hook must leave the history intact"
        );
    }

    #[test]
    fn prepare_compaction_prompt_rewrite_reaches_model() {
        struct RewriteHook;
        impl Hooks for RewriteHook {
            fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
                prep.prompt = "REWRITTEN".to_owned();
                prep.instruction = "BE TERSE".to_owned();
                Ok(())
            }
        }
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "summarized".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let cancel = CancelFlag::new();
        let mut hooks = RewriteHook;
        let mut cx = loaded_context(1_000, 10);
        cx.context_window = 200_000;

        let ran = force_compact(&mut cx, &provider, &cancel, &mut hooks, &mut |_| {}).unwrap();
        assert!(ran);
        match &cx.history[0].content[0] {
            Content::Text { text } => assert!(text.contains("summarized")),
            other => panic!("expected Text, got {other:?}"),
        }
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
