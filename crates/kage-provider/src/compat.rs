//! OpenAI-compatible third-party providers.
//!
//! Every entry here streams tool-call deltas through the same `OpenAI`
//! Chat Completions protocol; only the id, display name, and upstream
//! base URL differ (caching and thinking are off for all). They are
//! described once in the [`COMPAT_PROVIDERS`] table and built on demand
//! with [`CompatProvider::build`]; [`crate::catalog`] carries the
//! matching model lists.
//!
//! When an upstream is *not* OpenAI-compatible (Anthropic Messages API,
//! Gemini's `:streamGenerateContent`, Bedrock Converse, etc.) it gets
//! its own module instead.

use crate::ProviderMetadata;
use crate::openai::OpenAiProvider;

/// One OpenAI-compatible provider kage ships: its kage id, display name,
/// and upstream base URL.
#[derive(Clone, Copy, Debug)]
pub struct CompatProvider {
    /// kage id (the `provider` in `provider:model`).
    pub id: &'static str,
    /// Human-readable name for the model picker.
    pub display_name: &'static str,
    /// Upstream OpenAI-compatible base URL.
    pub base_url: &'static str,
}

impl CompatProvider {
    /// Build the live [`OpenAiProvider`] for this entry with `api_key`.
    #[must_use]
    pub fn build(&self, api_key: impl Into<String>) -> OpenAiProvider {
        OpenAiProvider::compatible(
            api_key,
            self.base_url,
            ProviderMetadata {
                id: self.id.into(),
                display_name: self.display_name.into(),
                supports_caching: false,
                supports_thinking: false,
                supports_tool_use: true,
            },
        )
    }
}

/// Every OpenAI-compatible provider kage ships, in registration order.
/// The host iterates this to register the ones the user has a key for.
pub const COMPAT_PROVIDERS: &[CompatProvider] = &[
    CompatProvider {
        id: "zai",
        display_name: "Z.AI",
        base_url: "https://api.z.ai/api/paas/v4",
    },
    CompatProvider {
        id: "zai-coding-plan",
        display_name: "Z.AI Coding Plan",
        base_url: "https://api.z.ai/api/coding/paas/v4",
    },
    CompatProvider {
        id: "deepseek",
        display_name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
    },
    CompatProvider {
        id: "groq",
        display_name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
    },
    CompatProvider {
        id: "mistral",
        display_name: "Mistral",
        base_url: "https://api.mistral.ai/v1",
    },
    CompatProvider {
        id: "cerebras",
        display_name: "Cerebras",
        base_url: "https://api.cerebras.ai/v1",
    },
    CompatProvider {
        id: "xai",
        display_name: "xAI",
        base_url: "https://api.x.ai/v1",
    },
    CompatProvider {
        id: "openrouter",
        display_name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
    },
    CompatProvider {
        id: "fireworks-ai",
        display_name: "Fireworks AI",
        base_url: "https://api.fireworks.ai/inference/v1",
    },
    CompatProvider {
        id: "moonshotai",
        display_name: "Moonshot",
        base_url: "https://api.moonshot.ai/v1",
    },
    CompatProvider {
        id: "kimi-for-coding",
        display_name: "Kimi for Coding",
        base_url: "https://api.kimi.com/coding/v1",
    },
    CompatProvider {
        id: "xiaomi",
        display_name: "Xiaomi",
        base_url: "https://api.xiaomimimo.com/v1",
    },
    CompatProvider {
        id: "xiaomi-token-plan-ams",
        display_name: "Xiaomi Token Plan (Europe)",
        base_url: "https://token-plan-ams.xiaomimimo.com/v1",
    },
    CompatProvider {
        id: "xiaomi-token-plan-cn",
        display_name: "Xiaomi Token Plan (China)",
        base_url: "https://token-plan-cn.xiaomimimo.com/v1",
    },
    CompatProvider {
        id: "xiaomi-token-plan-sgp",
        display_name: "Xiaomi Token Plan (Singapore)",
        base_url: "https://token-plan-sgp.xiaomimimo.com/v1",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;

    #[test]
    fn build_preserves_id_and_advertises_tool_use() {
        for entry in COMPAT_PROVIDERS {
            let provider = entry.build("k");
            assert_eq!(provider.metadata().id, entry.id);
            assert!(provider.metadata().supports_tool_use);
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in COMPAT_PROVIDERS {
            assert!(seen.insert(entry.id), "duplicate compat id {}", entry.id);
        }
    }
}
