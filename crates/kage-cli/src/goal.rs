//! Check a finished turn against the session's goal.
//!
//! After a completed turn of a session with a goal, the worker asks
//! the active model whether the work met the goal (a single tiny
//! streaming call, the same shape the title generator uses). The judge
//! reads the work since the user's last prompt: the replies, the tool
//! calls and their results, newest first within a budget, so a goal
//! such as "the tests pass" is judged on what the tools showed rather
//! than on the closing words alone.
//!
//! A check that fails or answers neither yes nor no gives no verdict,
//! so a broken check neither claims success nor keeps the session
//! working on its own.

use std::fmt::Write as _;
use std::sync::Arc;

use kage_core::{CancelFlag, Content, Message, Role};
use kage_provider::{Provider, ProviderEvent, StreamRequest};

/// System instruction for the goal check.
const GOAL_SYSTEM: &str = "You judge whether an assistant's work met a stated goal. \
 Judge the recorded work: tool calls, their results and what the assistant did, not \
 its claims of success. Reply YES only if that work shows the goal is fully met. \
 Otherwise reply NO, then one short line saying what is still missing. Reply with \
 nothing else.";

/// The most characters of work the judge reads.
const WORK_BUDGET: usize = 12_000;

/// Prefix of the user message setting a goal delivers to the
/// session, and the anchor the judge reads the work from: everything
/// since the goal was set, not just since the last prompt.
pub(crate) const GOAL_INTRO_PREFIX: &str = "[goal] Work toward this goal: ";

/// What the judge said about a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The goal is met.
    Met,
    /// The goal is not met; what is still missing, when the judge said.
    NotMet(String),
    /// No verdict: why the check failed or what it answered instead.
    Unknown(String),
}

/// Judges whether the work in `history` met `goal`.
#[must_use]
pub(crate) fn judge(
    provider: &dyn Provider,
    model: &str,
    goal: &str,
    history: &[Arc<Message>],
    cancel: &CancelFlag,
) -> Verdict {
    let mut prompt = format!("Goal:\n{}\n\nWork so far:\n", clip(goal, 800));
    prompt.push_str(&work(history));
    prompt.push_str("\n\nWas the goal met?");
    let mut req = StreamRequest::new(
        model,
        vec![Arc::new(Message::new(
            Role::User,
            vec![Content::Text { text: prompt }],
            None,
        ))],
    );
    req.system = Some(GOAL_SYSTEM.to_owned());
    match answer(provider, req, cancel) {
        Ok(text) => verdict(&text),
        Err(why) => Verdict::Unknown(why),
    }
}

fn answer(
    provider: &dyn Provider,
    req: StreamRequest,
    cancel: &CancelFlag,
) -> Result<String, String> {
    let stream = provider.stream(req, cancel).map_err(|e| e.to_string())?;
    let mut text = String::new();
    for event in stream {
        if cancel.is_cancelled() {
            return Err("the check was cancelled".to_owned());
        }
        match event.map_err(|e| e.to_string())? {
            ProviderEvent::TextDelta { delta } => text.push_str(&delta),
            ProviderEvent::MessageEnd { .. } => break,
            _ => {}
        }
    }
    Ok(text)
}

/// The verdict a judge's reply reads as: its first word, past any
/// markup, decides; what follows a no says what is missing.
fn verdict(reply: &str) -> Verdict {
    let start = reply
        .find(|c: char| c.is_alphabetic())
        .unwrap_or(reply.len());
    let rest = &reply[start..];
    let end = rest
        .find(|c: char| !c.is_alphabetic())
        .unwrap_or(rest.len());
    let missing = || {
        rest[end..]
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .lines()
            .next()
            .map(|line| clip(line.trim(), 200))
            .unwrap_or_default()
    };
    match rest[..end].to_ascii_lowercase().as_str() {
        "yes" => Verdict::Met,
        "no" => Verdict::NotMet(missing()),
        _ if reply.trim().is_empty() => Verdict::Unknown("the check gave no answer".to_owned()),
        _ => Verdict::Unknown(format!("the check answered \"{}\"", clip(reply.trim(), 60))),
    }
}

/// The work since the goal was set when an intro message says where
/// that was, else since the user's last own prompt, a goal nudge
/// aside, as lines the judge reads, newest kept when it runs over the
/// budget.
fn work(history: &[Arc<Message>]) -> String {
    let user_text = |m: &Arc<Message>, prefix: &str, keep: bool| {
        m.role == Role::User
            && m.content.iter().any(|block| match block {
                Content::Text { text } => text.starts_with(prefix) == keep,
                _ => false,
            })
    };
    let prompt = |m: &Arc<Message>| user_text(m, "[goal]", false);
    let intro = |m: &Arc<Message>| user_text(m, GOAL_INTRO_PREFIX, true);
    let start = history
        .iter()
        .rposition(|m| prompt(m) || intro(m))
        .unwrap_or(0);
    let mut out = String::new();
    for message in &history[start..] {
        for block in &message.content {
            match (message.role, block) {
                (Role::User, Content::Text { text }) => {
                    let _ = writeln!(out, "User: {}", clip(text, 800));
                }
                (Role::Assistant, Content::Text { text }) if !text.trim().is_empty() => {
                    let _ = writeln!(out, "Assistant: {}", clip(text, 1500));
                }
                (_, Content::ToolCall { name, input, .. }) => {
                    let _ = writeln!(out, "Called {name} {}", clip(&input.to_string(), 200));
                }
                (
                    _,
                    Content::ToolResultBlock {
                        output, is_error, ..
                    },
                ) => {
                    let label = if *is_error { "Failed" } else { "Result" };
                    let _ = writeln!(out, "{label}: {}", clip(output, 400));
                }
                _ => {}
            }
        }
    }
    let total = out.chars().count();
    if total <= WORK_BUDGET {
        return out;
    }
    let kept: String = out.chars().skip(total - WORK_BUDGET).collect();
    format!("[earlier work cut]\n{kept}")
}

/// Truncate `s` to at most `max` chars on a char boundary so the
/// check prompt cannot blow up on a huge goal or exchange.
fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kage_core::ToolCallId;
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

    fn text(role: Role, text: &str) -> Arc<Message> {
        Arc::new(Message::new(
            role,
            vec![Content::Text { text: text.into() }],
            None,
        ))
    }

    #[test]
    fn a_reply_reads_as_a_verdict_past_its_markup() {
        assert_eq!(verdict("YES"), Verdict::Met);
        assert_eq!(verdict("**Yes.**"), Verdict::Met);
        assert_eq!(
            verdict("NO - the tests still fail.\nmore"),
            Verdict::NotMet("the tests still fail.".into())
        );
        assert_eq!(verdict("No"), Verdict::NotMet(String::new()));
        assert!(matches!(verdict("Maybe"), Verdict::Unknown(_)));
        assert!(matches!(verdict("  "), Verdict::Unknown(_)));
    }

    #[test]
    fn a_failed_check_gives_no_verdict() {
        let mock = MockProvider::replaying(vec![Err(ProviderError::Auth("boom".into()))]);
        let verdict = judge(&mock, "mock:m", "ship", &[], &CancelFlag::new());
        assert!(matches!(verdict, Verdict::Unknown(_)), "{verdict:?}");
    }

    #[test]
    fn the_judge_reads_the_work_since_the_last_prompt() {
        let mock = MockProvider::replaying(verdict_script("YES"));
        let call = Arc::new(Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId::new("c1"),
                name: "shell".into(),
                input: serde_json::json!({"command": "cargo test"}),
            }],
            None,
        ));
        let result = Arc::new(Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("c1"),
                output: "test result: ok. 12 passed".into(),
                is_error: false,
            }],
            None,
        ));
        let history = vec![
            text(Role::User, "an older prompt"),
            text(Role::User, "make the tests pass"),
            call,
            result,
            text(Role::User, "[goal] The goal is not met yet: tests pass."),
            text(Role::Assistant, "All green now."),
        ];
        assert_eq!(
            judge(&mock, "mock:m", "tests pass", &history, &CancelFlag::new()),
            Verdict::Met
        );
        let req = mock.last_request().expect("one call");
        let Content::Text { text } = &req.messages[0].content[0] else {
            panic!("text message");
        };
        assert!(text.contains("Goal:\ntests pass"), "{text}");
        assert!(!text.contains("an older prompt"), "{text}");
        assert!(text.contains("User: make the tests pass"), "{text}");
        assert!(
            text.contains("Called shell {\"command\":\"cargo test\"}"),
            "{text}"
        );
        assert!(
            text.contains("Result: test result: ok. 12 passed"),
            "{text}"
        );
        assert!(text.contains("Assistant: All green now."), "{text}");
    }

    #[test]
    fn the_judge_reads_the_work_since_the_goal_was_set() {
        let mock = MockProvider::replaying(verdict_script("YES"));
        let intro = text(
            Role::User,
            "[goal] Work toward this goal: ship it. Keep working until it is met, then stop.",
        );
        let history = vec![
            text(Role::User, "an older prompt"),
            intro,
            text(Role::Assistant, "part one is done"),
            text(Role::User, "[goal] The goal is not met yet: ship it."),
            text(Role::Assistant, "part two is done"),
        ];
        assert_eq!(
            judge(&mock, "mock:m", "ship it", &history, &CancelFlag::new()),
            Verdict::Met
        );
        let req = mock.last_request().expect("one call");
        let Content::Text { text } = &req.messages[0].content[0] else {
            panic!("text message");
        };
        assert!(text.contains("Assistant: part one is done"), "{text}");
        assert!(text.contains("Assistant: part two is done"), "{text}");
        assert!(!text.contains("an older prompt"), "{text}");
    }

    #[test]
    fn long_work_keeps_its_newest_part() {
        let long = "x".repeat(1500);
        let mut history = vec![text(Role::User, "go")];
        for _ in 0..9 {
            history.push(text(Role::Assistant, &long));
        }
        history.push(text(Role::Assistant, "the end"));
        let work = work(&history);
        assert!(work.starts_with("[earlier work cut]"), "{}", &work[..40]);
        assert!(work.contains("Assistant: the end"));
    }
}
