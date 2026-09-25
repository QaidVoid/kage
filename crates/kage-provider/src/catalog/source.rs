//! Parsing of the models.dev catalog (`https://models.dev/api.json`).
//!
//! Shared by `cargo xtask refresh-models`, which renders the bundled
//! snapshot, and by `kage models refresh` and the cache loader, which
//! read the same shape at runtime. Parsing is lenient: a model that
//! does not parse is skipped rather than failing the whole catalog.

use std::collections::BTreeMap;

use kage_core::{Effort, Efforts, Input, Inputs, Reasoning, ReasoningField};
use serde::Deserialize;
use serde_json::Value;

use super::ModelCost;

/// URL of the upstream catalog.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// A models.dev provider id and the kage provider id it feeds.
#[derive(Clone, Copy, Debug)]
pub struct ProviderMap {
    /// Provider key in the models.dev catalog.
    pub api_id: &'static str,
    /// kage provider id (the `provider` in `provider:model`).
    pub kage_id: &'static str,
}

impl ProviderMap {
    const fn same(id: &'static str) -> Self {
        Self {
            api_id: id,
            kage_id: id,
        }
    }
}

/// Provider ids kage carries `Provider` impls for. The catalog is
/// pruned to just these; adding a provider impl means appending its id
/// here and re-running `cargo xtask refresh-models`.
pub const SUPPORTED_PROVIDERS: &[ProviderMap] = &[
    ProviderMap::same("anthropic"),
    ProviderMap::same("openai"),
    // The Responses API hits the same upstream provider; re-emit the
    // OpenAI model list under a second kage id so the model picker
    // offers `openai-responses:` rows alongside `openai:`.
    ProviderMap {
        api_id: "openai",
        kage_id: "openai-responses",
    },
    ProviderMap::same("zai"),
    ProviderMap::same("zai-coding-plan"),
    ProviderMap::same("zhipuai-coding-plan"),
    ProviderMap::same("deepseek"),
    ProviderMap::same("groq"),
    ProviderMap::same("mistral"),
    ProviderMap::same("cerebras"),
    ProviderMap::same("xai"),
    ProviderMap::same("openrouter"),
    ProviderMap::same("fireworks-ai"),
    ProviderMap::same("moonshotai"),
    ProviderMap::same("xiaomi"),
    ProviderMap::same("xiaomi-token-plan-ams"),
    ProviderMap::same("xiaomi-token-plan-cn"),
    ProviderMap::same("xiaomi-token-plan-sgp"),
    // models.dev calls Google's API "google" but kage's Provider impl
    // is registered under "gemini".
    ProviderMap {
        api_id: "google",
        kage_id: "gemini",
    },
];

/// One provider read from the catalog, under its kage id.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceProvider {
    /// kage provider id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Documented API endpoint, when published.
    pub api: Option<String>,
    /// Models that support tool calling, sorted by id.
    pub models: Vec<SourceModel>,
}

/// One model read from the catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceModel {
    /// Provider-scoped model id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Context window in tokens.
    pub context: Option<u64>,
    /// Largest prompt in tokens, when smaller than the context window.
    pub input_limit: Option<u64>,
    /// Maximum output tokens per turn.
    pub output: Option<u64>,
    /// Thinking settings the model accepts.
    pub reasoning: Reasoning,
    /// Inputs the model accepts.
    pub input: Inputs,
    /// Field an OpenAI-compatible model reads its reasoning back from
    /// during a tool loop. `None` for models.dev `interleaved: true`,
    /// which names no field.
    pub interleaved: Option<ReasoningField>,
    /// Release date (`YYYY-MM-DD`).
    pub release_date: Option<String>,
    /// Pricing, when both input and output rates are published.
    pub cost: Option<ModelCost>,
}

#[derive(Deserialize)]
struct ApiProvider {
    #[serde(default)]
    name: String,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ApiModel {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    reasoning_options: Option<Vec<Value>>,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    modalities: Option<ApiModalities>,
    #[serde(default)]
    interleaved: Option<Value>,
    #[serde(default)]
    limit: Option<ApiLimit>,
    #[serde(default)]
    cost: Option<ApiCost>,
}

#[derive(Deserialize)]
struct ApiModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Deserialize)]
struct ApiLimit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    input: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

#[derive(Deserialize)]
struct ApiCost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

/// Parse a models.dev catalog into kage's supported providers, in
/// [`SUPPORTED_PROVIDERS`] order. Providers the catalog lacks are left
/// out, and so are models that fail to parse or lack tool calling.
///
/// # Errors
///
/// `json` is not a JSON object.
pub fn parse(json: &str) -> Result<Vec<SourceProvider>, String> {
    let root: BTreeMap<String, Value> =
        serde_json::from_str(json).map_err(|e| format!("parse catalog: {e}"))?;
    Ok(SUPPORTED_PROVIDERS
        .iter()
        .filter_map(|map| {
            let raw = ApiProvider::deserialize(root.get(map.api_id)?).ok()?;
            Some(provider(&raw, map))
        })
        .collect())
}

/// `json` cut down to the supported providers and their tool-calling
/// models, still in the models.dev shape, for the model cache.
///
/// # Errors
///
/// `json` is not a JSON object, or it holds no supported provider.
pub fn prune(json: &str) -> Result<String, String> {
    let mut root: BTreeMap<String, Value> =
        serde_json::from_str(json).map_err(|e| format!("parse catalog: {e}"))?;
    let mut kept = serde_json::Map::new();
    for map in SUPPORTED_PROVIDERS {
        let Some(mut provider) = root.remove(map.api_id) else {
            continue;
        };
        if let Some(models) = provider.get_mut("models").and_then(Value::as_object_mut) {
            models.retain(|_, m| m.get("tool_call").and_then(Value::as_bool) == Some(true));
        }
        kept.insert(map.api_id.to_owned(), provider);
    }
    if kept.is_empty() {
        return Err("catalog has none of kage's providers".to_owned());
    }
    serde_json::to_string(&kept).map_err(|e| e.to_string())
}

fn provider(raw: &ApiProvider, map: &ProviderMap) -> SourceProvider {
    let mut models: Vec<SourceModel> = raw
        .models
        .values()
        .filter_map(|v| ApiModel::deserialize(v).ok())
        .filter(|m| m.tool_call)
        .map(|m| model(&m, map.api_id))
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    SourceProvider {
        id: map.kage_id.to_owned(),
        name: raw.name.clone(),
        api: raw.api.clone(),
        models,
    }
}

fn model(m: &ApiModel, api_id: &str) -> SourceModel {
    let limit = m.limit.as_ref();
    let cost = m.cost.as_ref().and_then(|c| {
        Some(ModelCost {
            input: c.input?,
            output: c.output?,
            cache_read: c.cache_read,
            cache_write: c.cache_write,
        })
    });
    let input = m
        .modalities
        .iter()
        .flat_map(|md| md.input.iter())
        .filter_map(|s| Input::parse(s))
        .collect();
    SourceModel {
        id: m.id.clone(),
        name: if m.name.is_empty() {
            m.id.clone()
        } else {
            m.name.clone()
        },
        context: limit.and_then(|l| l.context),
        input_limit: limit.and_then(|l| l.input),
        output: limit.and_then(|l| l.output),
        reasoning: reasoning(
            m.reasoning,
            m.reasoning_options.as_deref().unwrap_or_default(),
            api_id,
        ),
        input,
        interleaved: m
            .interleaved
            .as_ref()
            .and_then(|v| v.get("field")?.as_str())
            .and_then(ReasoningField::parse),
        release_date: m.release_date.clone(),
        cost,
    }
}

/// The thinking settings for a model. A budget wins over efforts, and
/// efforts over a bare toggle, because a budget works on every model
/// that lists one.
fn reasoning(thinks: bool, options: &[Value], api_id: &str) -> Reasoning {
    if !thinks {
        return Reasoning::None;
    }
    let mut toggle = false;
    let mut efforts = Efforts::default();
    let mut budget = None;
    for option in options {
        match option.get("type").and_then(Value::as_str) {
            Some("toggle") => toggle = true,
            Some("effort") => {
                efforts = option
                    .get("values")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|v| Effort::parse(v.as_str()?))
                    .collect();
            }
            Some("budget_tokens") => {
                let bound = |key: &str| {
                    option
                        .get(key)
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                };
                budget = Some((bound("min").unwrap_or(0), bound("max")));
            }
            _ => {}
        }
    }
    if let Some((min, max)) = budget {
        // Anthropic runs a budget model without thinking when the
        // request leaves `thinking` out, so off is always available.
        let toggle = toggle || api_id == "anthropic";
        return Reasoning::Budget { min, max, toggle };
    }
    if !efforts.is_empty() {
        return Reasoning::Effort { efforts, toggle };
    }
    if toggle {
        Reasoning::Toggle
    } else {
        Reasoning::Fixed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "anthropic": {
            "name": "Anthropic",
            "models": {
                "claude-x": {
                    "id": "claude-x", "name": "Claude X", "tool_call": true,
                    "reasoning": true,
                    "reasoning_options": [
                        {"type": "effort", "values": ["low", "medium", "high", "xhigh", "max"]}
                    ],
                    "modalities": {"input": ["text", "image", "pdf"], "output": ["text"]},
                    "limit": {"context": 1000000, "output": 128000},
                    "cost": {"input": 5, "output": 25}
                },
                "claude-old": {
                    "id": "claude-old", "tool_call": true, "reasoning": true,
                    "interleaved": true,
                    "reasoning_options": [{"type": "budget_tokens", "min": 1024}]
                },
                "no-tools": {"id": "no-tools", "tool_call": false},
                "broken": {"name": "missing id", "tool_call": true}
            }
        },
        "zai": {
            "name": "Z.AI", "api": "https://api.z.ai/api/paas/v4",
            "models": {
                "glm": {
                    "id": "glm", "name": "GLM", "tool_call": true, "reasoning": true,
                    "reasoning_options": [{"type": "toggle"}],
                    "modalities": {"input": ["text", "hologram"]},
                    "interleaved": {"field": "reasoning_content"},
                    "limit": {"context": 400000, "input": 272000, "output": 32000}
                }
            }
        },
        "unrelated": {"name": "Other", "models": {}}
    }"#;

    #[test]
    fn parse_maps_reasoning_options_and_modalities() {
        let providers = parse(SAMPLE).unwrap();
        let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["anthropic", "zai"]);

        let anthropic = &providers[0];
        let models: Vec<&str> = anthropic.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(models, ["claude-old", "claude-x"]);
        let x = &anthropic.models[1];
        assert_eq!(
            x.reasoning,
            Reasoning::Effort {
                efforts: Efforts::of(&[
                    Effort::Low,
                    Effort::Medium,
                    Effort::High,
                    Effort::XHigh,
                    Effort::Max
                ]),
                toggle: false,
            }
        );
        assert_eq!(x.input.label(), "text image pdf");
        assert_eq!(x.cost.map(|c| c.output), Some(25.0));
        assert_eq!(
            anthropic.models[0].reasoning,
            Reasoning::Budget {
                min: 1024,
                max: None,
                toggle: true,
            }
        );
        assert_eq!(anthropic.models[0].name, "claude-old");

        let glm = &providers[1].models[0];
        assert_eq!(glm.reasoning, Reasoning::Toggle);
        assert_eq!(glm.input, Inputs::of(&[Input::Text]));
        assert_eq!(glm.input_limit, Some(272_000));
        assert_eq!(glm.interleaved, Some(ReasoningField::ReasoningContent));
        assert_eq!(anthropic.models[0].interleaved, None);
    }

    #[test]
    fn reasoning_without_options_is_fixed_and_no_reasoning_is_none() {
        assert_eq!(reasoning(true, &[], "xai"), Reasoning::Fixed);
        assert_eq!(reasoning(false, &[], "xai"), Reasoning::None);
    }

    #[test]
    fn prune_keeps_supported_tool_models_only() {
        let pruned = prune(SAMPLE).unwrap();
        let value: Value = serde_json::from_str(&pruned).unwrap();
        assert!(value.get("unrelated").is_none());
        assert!(value["anthropic"]["models"].get("no-tools").is_none());
        assert_eq!(parse(&pruned).unwrap(), parse(SAMPLE).unwrap());
        assert!(prune(r#"{"unrelated": {}}"#).is_err());
        assert!(prune("not json").is_err());
    }
}
