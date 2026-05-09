//! OpenAI-compatible third-party providers.
//!
//! Each constructor returns an [`OpenAiProvider`] pre-wired to the
//! upstream's base URL and registered under a kage-friendly id. They
//! all stream tool-call deltas through the same `OpenAI` Chat Completions
//! protocol; only the URL, default model list, and display metadata
//! differ. The [`crate::catalog`] module carries the model lists.
//!
//! When an upstream is *not* OpenAI-compatible (Anthropic Messages API,
//! Gemini's `:streamGenerateContent`, Bedrock Converse, etc.) it gets
//! its own module instead.

use crate::ProviderMetadata;
use crate::openai::OpenAiProvider;

/// Construct a `DeepSeek` (`api.deepseek.com`) provider.
#[must_use]
pub fn deepseek(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        "https://api.deepseek.com/v1",
        ProviderMetadata {
            id: "deepseek".into(),
            display_name: "DeepSeek".into(),
            supports_caching: false,
            supports_thinking: false,
            supports_tool_use: true,
        },
    )
}

/// Construct a `Groq` (`api.groq.com/openai/v1`) provider.
#[must_use]
pub fn groq(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        "https://api.groq.com/openai/v1",
        ProviderMetadata {
            id: "groq".into(),
            display_name: "Groq".into(),
            supports_caching: false,
            supports_thinking: false,
            supports_tool_use: true,
        },
    )
}

/// Construct a `Mistral` (`api.mistral.ai/v1`) provider.
#[must_use]
pub fn mistral(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        "https://api.mistral.ai/v1",
        ProviderMetadata {
            id: "mistral".into(),
            display_name: "Mistral".into(),
            supports_caching: false,
            supports_thinking: false,
            supports_tool_use: true,
        },
    )
}

/// Construct a `Cerebras` (`api.cerebras.ai/v1`) provider.
#[must_use]
pub fn cerebras(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        "https://api.cerebras.ai/v1",
        ProviderMetadata {
            id: "cerebras".into(),
            display_name: "Cerebras".into(),
            supports_caching: false,
            supports_thinking: false,
            supports_tool_use: true,
        },
    )
}

/// Construct an xAI (`api.x.ai/v1`) provider.
#[must_use]
pub fn xai(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        "https://api.x.ai/v1",
        ProviderMetadata {
            id: "xai".into(),
            display_name: "xAI".into(),
            supports_caching: false,
            supports_thinking: false,
            supports_tool_use: true,
        },
    )
}

/// Construct an `OpenRouter` (`openrouter.ai/api/v1`) provider.
///
/// `OpenRouter` aggregates many upstreams behind one OpenAI-compatible
/// endpoint, so model ids carry a vendor prefix like
/// `openai/gpt-4o-mini` or `anthropic/claude-sonnet-4-5`.
#[must_use]
pub fn openrouter(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        "https://openrouter.ai/api/v1",
        ProviderMetadata {
            id: "openrouter".into(),
            display_name: "OpenRouter".into(),
            supports_caching: false,
            supports_thinking: false,
            supports_tool_use: true,
        },
    )
}

/// Construct a `Fireworks AI` (`api.fireworks.ai/inference/v1`) provider.
#[must_use]
pub fn fireworks_ai(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        "https://api.fireworks.ai/inference/v1",
        ProviderMetadata {
            id: "fireworks-ai".into(),
            display_name: "Fireworks AI".into(),
            supports_caching: false,
            supports_thinking: false,
            supports_tool_use: true,
        },
    )
}

/// Construct a `Moonshot` (`api.moonshot.ai/v1`) provider.
#[must_use]
pub fn moonshotai(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        "https://api.moonshot.ai/v1",
        ProviderMetadata {
            id: "moonshotai".into(),
            display_name: "Moonshot".into(),
            supports_caching: false,
            supports_thinking: false,
            supports_tool_use: true,
        },
    )
}

/// Construct a `Kimi for Coding` (`api.kimi.com/coding/v1`) provider.
#[must_use]
pub fn kimi_for_coding(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        "https://api.kimi.com/coding/v1",
        ProviderMetadata {
            id: "kimi-for-coding".into(),
            display_name: "Kimi for Coding".into(),
            supports_caching: false,
            supports_thinking: false,
            supports_tool_use: true,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;

    #[test]
    fn ids_match_catalog() {
        assert_eq!(deepseek("k").metadata().id, "deepseek");
        assert_eq!(groq("k").metadata().id, "groq");
        assert_eq!(mistral("k").metadata().id, "mistral");
        assert_eq!(cerebras("k").metadata().id, "cerebras");
        assert_eq!(xai("k").metadata().id, "xai");
        assert_eq!(openrouter("k").metadata().id, "openrouter");
        assert_eq!(fireworks_ai("k").metadata().id, "fireworks-ai");
        assert_eq!(moonshotai("k").metadata().id, "moonshotai");
        assert_eq!(kimi_for_coding("k").metadata().id, "kimi-for-coding");
    }

    #[test]
    fn all_advertise_tool_use() {
        for p in [
            deepseek("k"),
            groq("k"),
            mistral("k"),
            cerebras("k"),
            xai("k"),
            openrouter("k"),
            fireworks_ai("k"),
            moonshotai("k"),
            kimi_for_coding("k"),
        ] {
            assert!(p.metadata().supports_tool_use);
        }
    }
}
