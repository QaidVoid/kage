//! Third-party catalog providers.
//!
//! Every entry here is described once in the [`COMPAT_PROVIDERS`] table
//! and built on demand with [`CompatProvider::build`]; [`crate::catalog`]
//! carries the matching model lists. An entry speaks one of two wire
//! formats, [`CompatKind`], and builds on the matching hand-written
//! [`OpenAiProvider`] or [`AnthropicProvider`]: those impls own the
//! protocol, the table only carries id, display name, base URL and
//! format. [`OpenAiProvider`] detects the Z.AI and Zhipu endpoints and
//! shapes their requests the way those upstreams expect.
//!
//! When an upstream matches neither format (Gemini's
//! `:streamGenerateContent`, Bedrock Converse, ...) it gets its own
//! module instead.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{Provider, ProviderMetadata, anthropic::AnthropicProvider, openai::OpenAiProvider};

/// Wire format a compat entry speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompatKind {
    /// `OpenAI` Chat Completions: `{base_url}/chat/completions`, the
    /// credential as `Authorization: Bearer`.
    #[default]
    OpenAi,
    /// Anthropic Messages: `{base_url}/v1/messages`, the credential as
    /// `x-api-key`.
    Anthropic,
}

/// One third-party provider kage ships: its kage id, display name,
/// upstream base URL, and wire format.
#[derive(Clone, Copy, Debug)]
pub struct CompatProvider {
    /// kage id (the `provider` in `provider/model`).
    pub id: &'static str,
    /// Human-readable name for the model picker.
    pub display_name: &'static str,
    /// Upstream base URL.
    pub base_url: &'static str,
    /// Wire format; picks the `Provider` impl [`CompatProvider::build`]
    /// returns and how the credential is sent.
    pub kind: CompatKind,
}

impl CompatProvider {
    /// Build the live provider for this entry with `api_key`, sending
    /// `extra_headers` on every request after the protocol's own.
    #[must_use]
    pub fn build(
        &self,
        api_key: impl Into<String>,
        extra_headers: BTreeMap<String, String>,
    ) -> Arc<dyn Provider> {
        self.build_with_base_url(api_key, self.base_url, extra_headers)
    }

    /// Build the live provider for this entry with `api_key`, pointed
    /// at `base_url` instead of the catalog default. Used by the
    /// `[providers.<id>]` `base_url` override.
    #[must_use]
    pub fn build_with_base_url(
        &self,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        extra_headers: BTreeMap<String, String>,
    ) -> Arc<dyn Provider> {
        let metadata = ProviderMetadata {
            id: self.id.into(),
            display_name: self.display_name.into(),
            supports_caching: self.kind == CompatKind::Anthropic,
            supports_thinking: self.kind == CompatKind::Anthropic,
            supports_tool_use: true,
        };
        match self.kind {
            CompatKind::OpenAi => Arc::new(
                OpenAiProvider::compatible(api_key, base_url, metadata)
                    .with_extra_headers(extra_headers),
            ),
            CompatKind::Anthropic => Arc::new(
                AnthropicProvider::with_base_url(api_key, base_url)
                    .with_metadata(metadata)
                    .with_extra_headers(extra_headers),
            ),
        }
    }
}

/// Every third-party provider kage ships, in registration order.
/// The host iterates this to register the ones the user has a key for.
pub const COMPAT_PROVIDERS: &[CompatProvider] = &[
    CompatProvider {
        id: "zai",
        display_name: "Z.AI",
        base_url: "https://api.z.ai/api/paas/v4",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "zai-coding-plan",
        display_name: "Z.AI Coding Plan",
        base_url: "https://api.z.ai/api/coding/paas/v4",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "zhipuai-coding-plan",
        display_name: "Zhipu AI Coding Plan",
        base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "deepseek",
        display_name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "groq",
        display_name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "mistral",
        display_name: "Mistral",
        base_url: "https://api.mistral.ai/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "cerebras",
        display_name: "Cerebras",
        base_url: "https://api.cerebras.ai/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "xai",
        display_name: "xAI",
        base_url: "https://api.x.ai/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "openrouter",
        display_name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "fireworks-ai",
        display_name: "Fireworks AI",
        base_url: "https://api.fireworks.ai/inference/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "moonshotai",
        display_name: "Moonshot",
        base_url: "https://api.moonshot.ai/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "kimi-for-coding",
        display_name: "Kimi for Coding",
        base_url: "https://api.kimi.com/coding/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "xiaomi",
        display_name: "Xiaomi",
        base_url: "https://api.xiaomimimo.com/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "xiaomi-token-plan-ams",
        display_name: "Xiaomi Token Plan (Europe)",
        base_url: "https://token-plan-ams.xiaomimimo.com/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "xiaomi-token-plan-cn",
        display_name: "Xiaomi Token Plan (China)",
        base_url: "https://token-plan-cn.xiaomimimo.com/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "xiaomi-token-plan-sgp",
        display_name: "Xiaomi Token Plan (Singapore)",
        base_url: "https://token-plan-sgp.xiaomimimo.com/v1",
        kind: CompatKind::OpenAi,
    },
    // Command Code fronts many models behind OpenAI- and
    // Anthropic-shaped endpoints. Claude models answer on `/messages`
    // only, so they get their own entry against the Anthropic base;
    // both read the same `CMD_API_KEY` credential.
    CompatProvider {
        id: "commandcode",
        display_name: "Command Code",
        base_url: "https://api.commandcode.ai/provider/v1",
        kind: CompatKind::OpenAi,
    },
    CompatProvider {
        id: "commandcode-claude",
        display_name: "Command Code (Claude)",
        base_url: "https://api.commandcode.ai/provider",
        kind: CompatKind::Anthropic,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_preserves_id_and_advertises_tool_use() {
        for entry in COMPAT_PROVIDERS {
            let provider = entry.build("k", BTreeMap::new());
            assert_eq!(provider.metadata().id, entry.id);
            assert!(provider.metadata().supports_tool_use);
        }
    }

    /// The Anthropic-format entries advertise what the Messages
    /// endpoint supports, the OpenAI-format entries what compat
    /// entries always have.
    #[test]
    fn metadata_flags_follow_the_kind() {
        for entry in COMPAT_PROVIDERS {
            let provider = entry.build("k", BTreeMap::new());
            let caching = entry.kind == CompatKind::Anthropic;
            assert_eq!(
                provider.metadata().supports_caching,
                caching,
                "{}",
                entry.id
            );
            assert_eq!(
                provider.metadata().supports_thinking,
                caching,
                "{}",
                entry.id
            );
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in COMPAT_PROVIDERS {
            assert!(seen.insert(entry.id), "duplicate compat id {}", entry.id);
        }
    }

    /// Every compat provider needs a models.dev catalog key, or the
    /// picker lists no models and costs are missing for it.
    #[test]
    fn every_compat_id_has_a_catalog_entry() {
        for entry in COMPAT_PROVIDERS {
            assert!(
                crate::catalog::source::SUPPORTED_PROVIDERS
                    .iter()
                    .any(|map| map.kage_id == entry.id),
                "{} is missing from SUPPORTED_PROVIDERS",
                entry.id
            );
        }
    }
}
