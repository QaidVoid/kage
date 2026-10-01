//! Provider-qualified model ids: `provider/model`, such as
//! `anthropic/claude-sonnet-4-6`.
//!
//! The older `provider/model` form still reads, so configs and session
//! files written before the slash keep working; ids are written back
//! with a slash.

/// Split a provider-qualified model id into its provider and model. The
/// provider ends at the first `/` or `:`: provider ids hold neither,
/// while model names may hold both, as in `ollama/llama3:8b` or
/// `openrouter/anthropic/claude-sonnet-4`. `None` when either part is
/// empty.
#[must_use]
pub fn split_model(id: &str) -> Option<(&str, &str)> {
    let at = id.find(['/', ':'])?;
    let (provider, model) = (&id[..at], &id[at + 1..]);
    (!provider.is_empty() && !model.is_empty()).then_some((provider, model))
}

/// The id of `model` under `provider`: `provider/model`.
#[must_use]
pub fn qualify_model(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// `id` in the `provider/model` form, an older `provider/model` id
/// rewritten. An id that names no provider comes back as it was.
#[must_use]
pub fn canonical_model(id: &str) -> String {
    split_model(id).map_or_else(|| id.to_owned(), |(p, m)| qualify_model(p, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_forms_split_at_the_provider() {
        assert_eq!(
            split_model("anthropic/claude-sonnet-4-6"),
            Some(("anthropic", "claude-sonnet-4-6"))
        );
        assert_eq!(
            split_model("anthropic:claude-sonnet-4-6"),
            Some(("anthropic", "claude-sonnet-4-6"))
        );
        assert_eq!(
            split_model("ollama/llama3:8b"),
            Some(("ollama", "llama3:8b"))
        );
        assert_eq!(
            split_model("ollama:llama3:8b"),
            Some(("ollama", "llama3:8b"))
        );
        assert_eq!(
            split_model("openrouter/anthropic/claude"),
            Some(("openrouter", "anthropic/claude"))
        );
        assert_eq!(
            split_model("openrouter:anthropic/claude"),
            Some(("openrouter", "anthropic/claude"))
        );
        assert_eq!(split_model("bare"), None);
        assert_eq!(split_model("/model"), None);
        assert_eq!(split_model("provider/"), None);
    }

    #[test]
    fn the_old_form_is_rewritten_with_a_slash() {
        assert_eq!(canonical_model("openai:gpt-4o"), "openai/gpt-4o");
        assert_eq!(canonical_model("ollama:llama3:8b"), "ollama/llama3:8b");
        assert_eq!(canonical_model("openai/gpt-4o"), "openai/gpt-4o");
        assert_eq!(canonical_model("inherit"), "inherit");
    }
}
