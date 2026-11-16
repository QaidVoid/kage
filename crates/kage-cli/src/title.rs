//! Generate a short session title from the first exchange.
//!
//! After a brand-new session's first assistant response, the worker
//! asks the active model for a terse title (a single tiny streaming
//! call, same shape the compaction summarizer uses) and persists it
//! as a [`kage_session::SessionEntry::Title`]. Any failure - provider
//! error, cancellation, empty/garbage reply - falls back to a free
//! heuristic derived from the first user message, so a session always
//! gets a usable title and the picker never has to show a raw prompt.

use std::fmt::Write as _;
use std::sync::Arc;

use kage_core::environment::Environment;
use kage_core::{CancelFlag, Content, Message, Role};
use kage_provider::{Provider, ProviderEvent, StreamRequest};

/// Upper bound on the stored title length (characters). Keeps the
/// picker column tight regardless of what the model returns.
const MAX_TITLE_CHARS: usize = 60;

/// System instruction for the title call. Deliberately strict so the
/// reply needs little cleanup.
const TITLE_SYSTEM: &str = "You write terse conversation titles. Reply with ONLY the title: \
 at most 6 words, no quotes, no trailing punctuation, no preamble.";

/// The user message a title is drawn from: the first prompt the user
/// typed. What the engine adds as a user message says nothing about
/// the session: a note such as the plan mode instructions a `/plan on`
/// before the first prompt leaves, a `!` command's output, or a
/// compaction summary. A history with no typed prompt, such as one
/// opened with a goal, falls back to its first user message.
pub(crate) fn source_text(history: &[Arc<Message>]) -> String {
    let mut users = history.iter().filter(|m| m.role == Role::User);
    let typed = |m: &&Arc<Message>| {
        let text = crate::cli_loop_run::first_user_text(m);
        kage_tui::events::typed_by_the_user(m)
            && kage_core::message::ShellRun::parse(&text).is_none()
            && !text.starts_with(kage_core::message::COMPACTION_SUMMARY_PREFIX)
    };
    users
        .clone()
        .find(typed)
        .or_else(|| users.next())
        .map(|m| crate::cli_loop_run::first_user_text(m))
        .unwrap_or_default()
}

/// Produce a title for the session whose first exchange is
/// `user_text` -> `assistant_text`. A blank `assistant_text`, as when the
/// first reply only calls tools, is left out of the request. Tries the
/// model; on any failure returns the heuristic. Never returns an empty
/// string. The request carries `workdir` and `env` like a normal turn,
/// since the ACP client launches the model subprocess with them.
#[must_use]
pub(crate) fn generate(
    provider: &dyn Provider,
    model: &str,
    user_text: &str,
    assistant_text: &str,
    workdir: &std::path::Path,
    env: &Environment,
    cancel: &CancelFlag,
) -> String {
    model_title(
        provider,
        model,
        user_text,
        assistant_text,
        workdir,
        env,
        cancel,
    )
    .map(|t| sanitize(&t))
    .filter(|t| !t.is_empty())
    .unwrap_or_else(|| heuristic(user_text))
}

/// Heuristic fallback: the first non-empty line of the user's first
/// message, trimmed to [`MAX_TITLE_CHARS`]. Always non-empty (uses a
/// constant when the prompt is blank).
fn heuristic(user_text: &str) -> String {
    let line = user_text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("untitled session");
    let cleaned = sanitize(line);
    if cleaned.is_empty() {
        "untitled session".to_owned()
    } else {
        cleaned
    }
}

/// Collapse whitespace, drop wrapping quotes, strip trailing
/// punctuation, and clamp to [`MAX_TITLE_CHARS`] on a char boundary.
fn sanitize(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim_end_matches(['.', '!', '?', ':', ';', ','])
        .trim();
    trimmed.chars().take(MAX_TITLE_CHARS).collect()
}

fn model_title(
    provider: &dyn Provider,
    model: &str,
    user_text: &str,
    assistant_text: &str,
    workdir: &std::path::Path,
    env: &Environment,
    cancel: &CancelFlag,
) -> Option<String> {
    let mut prompt = format!("User asked:\n{}\n\n", clip(user_text, 800));
    if !assistant_text.trim().is_empty() {
        let _ = write!(
            prompt,
            "Assistant replied:\n{}\n\n",
            clip(assistant_text, 800)
        );
    }
    prompt.push_str("Title:");
    let mut req = StreamRequest::new(
        model,
        vec![Arc::new(Message::new(
            Role::User,
            vec![Content::Text { text: prompt }],
            None,
        ))],
    );
    req.system = Some(TITLE_SYSTEM.to_owned());
    req.workdir = Some(workdir.to_path_buf());
    req.env = env.clone();
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
/// title prompt cannot blow up on a huge first message.
fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Arc<Message> {
        Arc::new(Message::new(
            Role::User,
            vec![Content::Text {
                text: text.to_owned(),
            }],
            None,
        ))
    }

    #[test]
    fn a_title_skips_the_notes_the_engine_adds() {
        let history = [
            user("[plan mode on] Plan before changing anything."),
            user("plan the fix"),
        ];
        assert_eq!(source_text(&history), "plan the fix");
        let shell = kage_core::message::ShellRun {
            command: "git status".to_owned(),
            exit_code: Some(0),
            output: "clean".to_owned(),
        }
        .to_text();
        let summary = format!(
            "{}earlier work{}",
            kage_core::message::COMPACTION_SUMMARY_PREFIX,
            kage_core::message::COMPACTION_SUMMARY_SUFFIX
        );
        assert_eq!(
            source_text(&[user(&summary), user(&shell), user("fix the build")]),
            "fix the build",
            "a summary and a command's output are not the prompt"
        );
        assert_eq!(
            source_text(&[user("[goal] Work toward this goal: ship it.")]),
            "[goal] Work toward this goal: ship it.",
            "with nothing typed, the first user message still names it"
        );
    }

    #[test]
    fn sanitize_strips_quotes_punctuation_and_collapses_ws() {
        assert_eq!(sanitize("  \"Hello   World.\"  "), "Hello World");
        assert_eq!(sanitize("`refactor: auth`"), "refactor: auth");
        assert_eq!(sanitize("trailing!!!"), "trailing");
    }

    #[test]
    fn sanitize_clamps_length_on_char_boundary() {
        let long = "x".repeat(200);
        assert_eq!(sanitize(&long).chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn heuristic_uses_first_nonempty_line() {
        assert_eq!(
            heuristic("\n\n  fix the parser bug  \nmore detail"),
            "fix the parser bug"
        );
    }

    #[test]
    fn heuristic_never_empty() {
        assert_eq!(heuristic("   \n  "), "untitled session");
        assert_eq!(heuristic(""), "untitled session");
    }

    #[test]
    fn generate_falls_back_to_heuristic_on_provider_error() {
        use kage_provider::ProviderError;
        use kage_provider::testing::MockProvider;
        let provider = MockProvider::replaying(vec![Err(ProviderError::Auth("nope".into()))]);
        let cancel = CancelFlag::new();
        let title = generate(
            &provider,
            "mock:model",
            "Add a retry to the HTTP client",
            "Sure, here is the patch...",
            std::path::Path::new("."),
            &Environment::inherit(),
            &cancel,
        );
        assert_eq!(title, "Add a retry to the HTTP client");
    }

    #[test]
    fn blank_reply_is_left_out_of_the_request() {
        use kage_core::{StopReason, TokenUsage};
        use kage_provider::testing::MockProvider;
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "Fix the parser".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let title = generate(
            &provider,
            "m",
            "fix it",
            " \n",
            std::path::Path::new("."),
            &Environment::inherit(),
            &CancelFlag::new(),
        );
        assert_eq!(title, "Fix the parser");
        let requests = provider.requests();
        let Content::Text { text } = &requests[0].messages[0].content[0] else {
            panic!("text prompt expected");
        };
        assert_eq!(text, "User asked:\nfix it\n\nTitle:");
    }

    /// The title request reaches the ACP client with the session's
    /// workdir and environment, which its subprocess launches with.
    #[test]
    fn the_title_request_carries_the_session_context() {
        use kage_core::environment::Environment;
        use kage_core::{StopReason, TokenUsage};
        use kage_provider::testing::MockProvider;
        let provider = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "Fix the parser".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        ]);
        let title = generate(
            &provider,
            "m",
            "fix it",
            "done",
            std::path::Path::new("/work/project"),
            &Environment::captured(vec![("CLIENT".into(), "editor".into())]),
            &CancelFlag::new(),
        );
        assert_eq!(title, "Fix the parser");
        let req = provider.last_request().unwrap();
        assert_eq!(
            req.workdir.as_deref(),
            Some(std::path::Path::new("/work/project"))
        );
        assert_eq!(
            req.env.var("CLIENT").as_deref(),
            Some(std::ffi::OsStr::new("editor"))
        );
    }
}
