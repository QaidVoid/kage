//! Check a finished turn against the session's goal.
//!
//! After a completed turn of a session with a goal, the worker asks
//! the active model whether the latest exchange met the goal (a
//! single tiny streaming call, same shape the title generator uses).
//! Any failure - provider error, cancellation, an unreadable reply -
//! counts as not met, so a flaky check never lies about success.

use std::fmt::Write as _;
use std::sync::Arc;

use kage_core::{CancelFlag, Content, Message, Role};
use kage_provider::{Provider, ProviderEvent, StreamRequest};

/// System instruction for the goal check. Deliberately strict so the
/// reply is one word.
const GOAL_SYSTEM: &str = "You judge whether a conversation turn met a stated goal. \
 Reply with ONLY one word: YES if the goal is fully met, NO otherwise.";

/// Says whether the turn that ended with user text `user_text` and
/// assistant text `assistant_text` met `goal`. Anything short of a
/// clear yes is no.
#[must_use]
pub(crate) fn met(
    provider: &dyn Provider,
    model: &str,
    goal: &str,
    user_text: &str,
    assistant_text: &str,
    cancel: &CancelFlag,
) -> bool {
    model_verdict(provider, model, goal, user_text, assistant_text, cancel)
        .map(|t| t.trim().to_ascii_lowercase())
        .is_some_and(|t| t.starts_with("yes"))
}

fn model_verdict(
    provider: &dyn Provider,
    model: &str,
    goal: &str,
    user_text: &str,
    assistant_text: &str,
    cancel: &CancelFlag,
) -> Option<String> {
    let mut prompt = format!("Goal:\n{}\n\n", clip(goal, 800));
    let _ = write!(prompt, "User asked:\n{}\n\n", clip(user_text, 800));
    if !assistant_text.trim().is_empty() {
        let _ = write!(
            prompt,
            "Assistant replied:\n{}\n\n",
            clip(assistant_text, 800)
        );
    }
    prompt.push_str("Was the goal met?");
    let mut req = StreamRequest::new(
        model,
        vec![Arc::new(Message::new(
            Role::User,
            vec![Content::Text { text: prompt }],
            None,
        ))],
    );
    req.system = Some(GOAL_SYSTEM.to_owned());
    let stream = provider.stream(req, cancel).ok()?;
    let mut text = String::new();
    for event in stream {
        if cancel.is_cancelled() {
            return None;
        }
        match event.ok()? {
            ProviderEvent::TextDelta { delta } => text.push_str(&delta),
            ProviderEvent::MessageEnd { .. } => return Some(text),
            _ => {}
        }
    }
    // Stream ended without MessageEnd: use whatever text arrived.
    Some(text)
}

/// Truncate `s` to at most `max` chars on a char boundary so the
/// check prompt cannot blow up on a huge goal or exchange.
fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kage_provider::{ProviderError, testing::MockProvider};
    fn verdict_script(text: &str) -> Vec<Result<ProviderEvent, ProviderError>> {
        vec![
            Ok(ProviderEvent::MessageStart),
            Ok(ProviderEvent::TextDelta {
                delta: text.to_owned(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: kage_core::StopReason::EndTurn,
                usage: kage_core::TokenUsage::default(),
            }),
        ]
    }

    #[test]
    fn a_yes_verdict_meets_the_goal() {
        let mock = MockProvider::replaying(verdict_script("YES"));
        assert!(met(
            &mock,
            "mock:m",
            "tests pass",
            "make tests pass",
            "fixed the bug, all green",
            &CancelFlag::new()
        ));
    }

    #[test]
    fn anything_short_of_yes_does_not() {
        for reply in ["no", "Not yet.", "", "maybe"] {
            let mock = MockProvider::replaying(verdict_script(reply));
            assert!(
                !met(
                    &mock,
                    "mock:m",
                    "tests pass",
                    "go",
                    "half done",
                    &CancelFlag::new()
                ),
                "{reply}"
            );
        }
    }

    #[test]
    fn a_failed_call_is_not_met() {
        let mock = MockProvider::replaying(vec![Err(ProviderError::Auth("boom".into()))]);
        assert!(!met(
            &mock,
            "mock:m",
            "tests pass",
            "go",
            "done",
            &CancelFlag::new()
        ));
    }

    #[test]
    fn the_prompt_carries_goal_and_exchange() {
        let mock = MockProvider::replaying(verdict_script("YES"));
        let _ = met(
            &mock,
            "mock:m",
            "ship it",
            "do the thing",
            "done",
            &CancelFlag::new(),
        );
        let req = mock.last_request().expect("one call");
        let kage_core::Content::Text { text } = &req.messages[0].content[0] else {
            panic!("text message");
        };
        assert!(text.contains("Goal:\nship it"));
        assert!(text.contains("User asked:\ndo the thing"));
        assert!(text.contains("Assistant replied:\ndone"));
    }
}
