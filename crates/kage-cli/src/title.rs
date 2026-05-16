//! Generate a short session title from the first exchange.
//!
//! After a brand-new session's first assistant response, the worker
//! asks the active model for a terse title (a single tiny streaming
//! call, same shape the compaction summarizer uses) and persists it
//! as a [`kage_session::SessionEntry::Title`]. Any failure - provider
//! error, cancellation, empty/garbage reply - falls back to a free
//! heuristic derived from the first user message, so a session always
//! gets a usable title and the picker never has to show a raw prompt.

use kage_core::{CancelFlag, Content, Message, Role};
use kage_provider::{Provider, ProviderEvent, StreamRequest};

/// Upper bound on the stored title length (characters). Keeps the
/// picker column tight regardless of what the model returns.
const MAX_TITLE_CHARS: usize = 60;

/// System instruction for the title call. Deliberately strict so the
/// reply needs little cleanup.
const TITLE_SYSTEM: &str = "You write terse conversation titles. Reply with ONLY the title: \
 at most 6 words, no quotes, no trailing punctuation, no preamble.";

/// Produce a title for the session whose first exchange is
/// `user_text` -> `assistant_text`. Tries the model; on any failure
/// returns the heuristic. Never returns an empty string.
#[must_use]
pub(crate) fn generate(
    provider: &dyn Provider,
    model: &str,
    user_text: &str,
    assistant_text: &str,
    cancel: &CancelFlag,
) -> String {
    model_title(provider, model, user_text, assistant_text, cancel)
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
    cancel: &CancelFlag,
) -> Option<String> {
    let prompt = format!(
        "User asked:\n{}\n\nAssistant replied:\n{}\n\nTitle:",
        clip(user_text, 800),
        clip(assistant_text, 800),
    );
    let mut req = StreamRequest::new(
        model,
        vec![Message::new(
            Role::User,
            vec![Content::Text { text: prompt }],
            None,
        )],
    );
    req.system = Some(TITLE_SYSTEM.to_owned());
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
            &cancel,
        );
        assert_eq!(title, "Add a retry to the HTTP client");
    }
}
