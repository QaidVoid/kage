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
//! default_model = "zai-coding-plan:glm-4.6"   # coding plan
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
/// Registers under the `zai-coding-plan` id; resolve as `zai-coding-plan:<model>`.
#[must_use]
pub fn coding_plan(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::compatible(
        api_key,
        CODING_BASE_URL,
        ProviderMetadata {
            id: "zai-coding-plan".into(),
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
    use crate::{Provider, ProviderEvent, StreamRequest};
    use kage_core::{CancelFlag, Content, Message, Role};

    #[test]
    fn standard_provider_id_is_zai() {
        let p = provider("test-key");
        assert_eq!(p.metadata().id, "zai");
        assert_eq!(p.metadata().display_name, "ZAI");
    }

    #[test]
    fn coding_plan_provider_id_is_zai_coding() {
        let p = coding_plan("test-key");
        assert_eq!(p.metadata().id, "zai-coding-plan");
        assert_eq!(p.metadata().display_name, "ZAI Coding Plan");
    }

    #[test]
    fn both_plans_support_tools() {
        assert!(provider("k").metadata().supports_tool_use);
        assert!(coding_plan("k").metadata().supports_tool_use);
    }

    fn live_smoke(provider_under_test: &crate::openai::OpenAiProvider, model: &str) {
        let req = StreamRequest::new(
            model,
            vec![Message::new(
                Role::User,
                vec![Content::Text {
                    text: "Reply with exactly the word: pong".into(),
                }],
                None,
            )],
        );
        let stream = provider_under_test
            .stream(req, &CancelFlag::new())
            .expect("stream opens");
        let events: Vec<_> = stream.collect();
        let saw_start = events
            .iter()
            .any(|e| matches!(e, Ok(ProviderEvent::MessageStart)));
        let saw_text = events
            .iter()
            .any(|e| matches!(e, Ok(ProviderEvent::TextDelta { .. })));
        let saw_end = events
            .iter()
            .any(|e| matches!(e, Ok(ProviderEvent::MessageEnd { .. })));
        assert!(saw_start, "expected MessageStart, got {events:#?}");
        assert!(saw_text, "expected at least one TextDelta, got {events:#?}");
        assert!(saw_end, "expected MessageEnd, got {events:#?}");
    }

    /// Live smoke test against ZAI standard plan.
    ///
    /// ```sh
    /// ZAI_API_KEY=... ZAI_MODEL=glm-4.5-air \
    ///   nix develop --command cargo test -p kage-provider -- --ignored zai_live_standard
    /// ```
    #[test]
    #[ignore = "requires ZAI_API_KEY"]
    fn zai_live_standard() {
        let key = std::env::var("ZAI_API_KEY").expect("set ZAI_API_KEY");
        let model = std::env::var("ZAI_MODEL").unwrap_or_else(|_| "glm-4.5-air".into());
        live_smoke(&provider(key), &model);
    }

    /// Live smoke test against ZAI coding plan.
    ///
    /// ```sh
    /// ZAI_CODING_API_KEY=... ZAI_MODEL=glm-4.6 \
    ///   nix develop --command cargo test -p kage-provider -- --ignored zai_live_coding
    /// ```
    #[test]
    #[ignore = "requires ZAI_CODING_API_KEY (or falls back to ZAI_API_KEY)"]
    fn zai_live_coding() {
        let key = std::env::var("ZAI_CODING_API_KEY")
            .or_else(|_| std::env::var("ZAI_API_KEY"))
            .expect("set ZAI_CODING_API_KEY or ZAI_API_KEY");
        let model = std::env::var("ZAI_MODEL").unwrap_or_else(|_| "glm-4.6".into());
        live_smoke(&coding_plan(key), &model);
    }
}
