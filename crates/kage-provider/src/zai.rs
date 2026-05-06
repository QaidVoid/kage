//! ZAI (`Zhipu` GLM) providers.
//!
//! ZAI exposes an OpenAI-compatible Chat Completions API. We surface two
//! base URLs so the `provider:model` resolver can route either the standard
//! plan or the coding plan to ZAI without further config.
//!
//! Configure either id in `~/.kage/config.toml`:
//!
//! ```toml
//! [provider]
//! default_model = "zai:glm-4.6"          # standard plan
//! # or
//! default_model = "zai-coding:glm-4.6"   # coding plan
//! ```

use crate::ProviderMetadata;
use crate::openai::OpenAiProvider;

const STANDARD_BASE_URL: &str = "https://api.z.ai/api/paas/v4";
const CODING_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

/// Construct a ZAI standard-plan provider.
///
/// Registers under the `zai` id; resolve as `zai:<model>`.
#[must_use]
pub fn provider(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        STANDARD_BASE_URL,
        ProviderMetadata {
            id: "zai".into(),
            display_name: "ZAI".into(),
            supports_caching: false,
            supports_thinking: false,
            supports_tool_use: true,
        },
    )
}

/// Construct a ZAI coding-plan provider.
///
/// Registers under the `zai-coding` id; resolve as `zai-coding:<model>`.
#[must_use]
pub fn coding_plan(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        CODING_BASE_URL,
        ProviderMetadata {
            id: "zai-coding".into(),
            display_name: "ZAI Coding Plan".into(),
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
    fn standard_provider_id_is_zai() {
        let p = provider("test-key");
        assert_eq!(p.metadata().id, "zai");
        assert_eq!(p.metadata().display_name, "ZAI");
    }

    #[test]
    fn coding_plan_provider_id_is_zai_coding() {
        let p = coding_plan("test-key");
        assert_eq!(p.metadata().id, "zai-coding");
        assert_eq!(p.metadata().display_name, "ZAI Coding Plan");
    }

    #[test]
    fn both_plans_support_tools() {
        assert!(provider("k").metadata().supports_tool_use);
        assert!(coding_plan("k").metadata().supports_tool_use);
    }
}
