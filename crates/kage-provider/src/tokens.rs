//! Pre-flight token estimation.
//!
//! For known `OpenAI` models, defers to `tiktoken-rs` for exact counts. For
//! everything else (Anthropic, Gemini, ZAI, plugin-defined providers) falls
//! back to a heuristic of one token per four characters.
//!
//! For exact post-hoc counts use the [`crate::ProviderEvent::MessageEnd::usage`]
//! field, which carries the provider's own accounting.

const HEURISTIC_CHARS_PER_TOKEN: usize = 4;

/// Estimate the token count of `text` for `model`.
///
/// The estimate is used pre-flight (before sending the request) to budget
/// context and decide when to compact. It is intentionally cheap; for
/// post-hoc accounting use the provider-reported usage on
/// [`crate::ProviderEvent::MessageEnd`].
#[must_use]
pub fn estimate_tokens(model: &str, text: &str) -> u64 {
    if let Ok(bpe) = tiktoken_rs::bpe_for_model(model) {
        return u64::try_from(bpe.encode_with_special_tokens(text).len()).unwrap_or(u64::MAX);
    }
    if let Some(stripped) = strip_provider_prefix(model)
        && let Ok(bpe) = tiktoken_rs::bpe_for_model(stripped)
    {
        return u64::try_from(bpe.encode_with_special_tokens(text).len()).unwrap_or(u64::MAX);
    }
    u64::try_from(text.len() / HEURISTIC_CHARS_PER_TOKEN).unwrap_or(u64::MAX)
}

fn strip_provider_prefix(model: &str) -> Option<&str> {
    model.split_once(':').map(|(_, m)| m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_zero_tokens() {
        assert_eq!(estimate_tokens("gpt-4o", ""), 0);
        assert_eq!(estimate_tokens("unknown-model", ""), 0);
    }

    #[test]
    fn known_openai_model_counts_via_tiktoken() {
        let n = estimate_tokens("gpt-4o", "hello world");
        assert!((1..=4).contains(&n), "expected 1-4 tokens, got {n}");
    }

    #[test]
    fn unknown_model_uses_heuristic() {
        let text = "a".repeat(40);
        let n = estimate_tokens("anthropic:claude-sonnet-4-6", &text);
        assert_eq!(n, 10);
    }

    #[test]
    fn provider_prefix_resolves_to_underlying_model() {
        let with_prefix = estimate_tokens("openai:gpt-4o", "hello world");
        let without = estimate_tokens("gpt-4o", "hello world");
        assert_eq!(with_prefix, without);
    }

    #[test]
    fn long_text_does_not_overflow() {
        let text = "x".repeat(1_000_000);
        let n = estimate_tokens("unknown", &text);
        assert_eq!(n, 250_000);
    }
}
