//! Static description of a [`Provider`](crate::Provider) instance.

use kage_core::{Modalities, ModelCost, Reasoning, ReasoningField};

/// Description used by the registry, `kage doctor`, and the model picker UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderMetadata {
    /// Stable id used as the prefix in `provider/model` strings.
    pub id: String,
    /// Human-friendly display name.
    pub display_name: String,
    /// Whether the provider supports prompt caching.
    pub supports_caching: bool,
    /// Whether the provider exposes a thinking-tokens budget.
    pub supports_thinking: bool,
    /// Whether the provider supports tool/function calling.
    pub supports_tool_use: bool,
}

/// One model entry a [`Provider`](crate::Provider) advertises for the UI
/// picker. Plugin providers that have no catalog entry declare their
/// model list this way; built-in providers can leave the
/// [`Provider::models`](crate::Provider::models) override empty and rely
/// on [`crate::catalog`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderModel {
    /// Provider-scoped model id (the portion after `provider:`).
    pub id: String,
    /// Human-friendly display name shown in the picker.
    pub name: String,
    /// Total context window in tokens, when known. Surfaces to the
    /// modeline so the percent-of-context indicator works for
    /// plugin-registered models without catalog entries.
    pub context: Option<u64>,
    /// Largest prompt in tokens, when known. The total context window
    /// remains in [`Self::context`].
    pub input_limit: Option<u64>,
    /// Maximum output tokens per turn, when known. Forwarded on every
    /// stream request so the provider does not silently truncate with
    /// its conservative default.
    pub max_output: Option<u32>,
    /// Thinking settings the model accepts. [`Reasoning::Unknown`]
    /// when not declared.
    pub reasoning: Reasoning,
    /// Modalities the model accepts. Empty when not declared.
    pub input: Modalities,
    /// Modalities the model produces. Empty when not declared, which
    /// is read as text only.
    pub produces: Modalities,
    /// Field an OpenAI-compatible model reads its reasoning back from
    /// during a tool loop, when it takes reasoning back.
    pub interleaved: Option<ReasoningField>,
    /// ISO-8601 date string associated with the model, when known.
    pub release_date: Option<String>,
    /// Per-million-token prices in USD, when declared. A provider that
    /// declares models is priced from them alone, see
    /// [`crate::model_cost`].
    pub cost: Option<ModelCost>,
}

impl ProviderModel {
    /// Tokens a prompt may fill: the input limit when reported, else
    /// the total context window.
    #[must_use]
    pub fn prompt_window(&self) -> Option<u64> {
        self.input_limit.or(self.context)
    }
}

impl From<&crate::catalog::ModelInfo> for ProviderModel {
    fn from(model: &crate::catalog::ModelInfo) -> Self {
        Self {
            id: model.id.to_owned(),
            name: model.name.to_owned(),
            context: model.context,
            input_limit: model.input_limit,
            max_output: model.output.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
            reasoning: model.reasoning,
            input: model.input,
            produces: model.produces,
            interleaved: model.interleaved,
            release_date: model.release_date.map(str::to_owned),
            cost: model.cost,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ModelInfo;
    use kage_core::Modality;

    fn catalog_model() -> ModelInfo {
        ModelInfo {
            id: "example-model",
            name: "Example Model",
            context: Some(200_000),
            input_limit: Some(180_000),
            output: Some(20_000),
            reasoning: Reasoning::Budget {
                min: 1024,
                max: Some(8192),
                toggle: true,
            },
            input: Modalities::of(&[Modality::Text, Modality::Image]),
            produces: Modalities::of(&[Modality::Text]),
            interleaved: Some(ReasoningField::ReasoningContent),
            release_date: Some("2026-11-01"),
            cost: Some(ModelCost {
                input: 1.0,
                output: 5.0,
                cache_read: Some(0.1),
                cache_write: Some(1.25),
            }),
        }
    }

    #[test]
    fn catalog_conversion_preserves_metadata_and_distinct_limits() {
        let source = catalog_model();
        let model = ProviderModel::from(&source);

        assert_eq!(model.id, source.id);
        assert_eq!(model.name, source.name);
        assert_eq!(model.context, Some(200_000));
        assert_eq!(model.input_limit, Some(180_000));
        assert_eq!(model.prompt_window(), Some(180_000));
        assert_eq!(model.max_output, Some(20_000));
        assert_eq!(model.reasoning, source.reasoning);
        assert_eq!(model.input, source.input);
        assert_eq!(model.produces, source.produces);
        assert_eq!(model.interleaved, source.interleaved);
        assert_eq!(model.release_date.as_deref(), source.release_date);
        assert_eq!(model.cost, source.cost);
    }

    #[test]
    fn catalog_conversion_saturates_output_limit() {
        let mut source = catalog_model();
        for (output, expected) in [
            (None, None),
            (Some(u64::from(u32::MAX)), Some(u32::MAX)),
            (Some(u64::from(u32::MAX) + 1), Some(u32::MAX)),
            (Some(u64::MAX), Some(u32::MAX)),
        ] {
            source.output = output;
            assert_eq!(ProviderModel::from(&source).max_output, expected);
        }
    }

    #[test]
    fn catalog_conversion_preserves_missing_metadata() {
        let source = ModelInfo {
            context: None,
            input_limit: None,
            output: None,
            release_date: None,
            cost: None,
            ..catalog_model()
        };
        let model = ProviderModel::from(&source);

        assert_eq!(model.context, None);
        assert_eq!(model.input_limit, None);
        assert_eq!(model.max_output, None);
        assert_eq!(model.release_date, None);
        assert_eq!(model.cost, None);
        assert_eq!(model.prompt_window(), None);
    }

    #[test]
    fn prompt_window_prefers_input_limit_and_falls_back_to_context() {
        for (context, input_limit, expected) in [
            (Some(200_000), Some(180_000), Some(180_000)),
            (Some(200_000), None, Some(200_000)),
            (None, Some(180_000), Some(180_000)),
            (None, None, None),
        ] {
            let model = ProviderModel {
                context,
                input_limit,
                ..ProviderModel::default()
            };
            assert_eq!(model.prompt_window(), expected);
        }
    }
}
