//! Streaming request shape passed to [`Provider::stream`](crate::Provider::stream).

use kage_core::{Message, ToolSpec};
use serde::{Deserialize, Serialize};

/// Optional thinking-budget configuration.
///
/// Carries an explicit per-turn budget in thinking tokens. Providers
/// that take a numeric budget (Anthropic, Gemini) forward it directly;
/// providers that take an effort string (`OpenAI` Chat Completions and
/// Responses) ignore it and read [`StreamRequest::level`] instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    /// Soft budget on thinking tokens emitted by the model.
    pub budget_tokens: u32,
}

/// Unified thinking effort the host requests for one turn.
///
/// Mirrors Pi's `ThinkingLevel`: a six-step ladder the user cycles
/// through with `Shift+Tab` and the providers translate to whatever
/// shape they accept. Anthropic and Gemini convert to a thinking
/// token budget (using the per-model table on
/// [`crate::ModelInfo::thinking_levels`] when present, falling back
/// to provider defaults). `OpenAI`'s Chat Completions and Responses
/// APIs translate to the `reasoning_effort` enum string.
///
/// The `Off` variant means thinking is disabled outright; providers
/// must omit any thinking-related field from the request body.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// Thinking disabled. Providers omit any thinking-related field.
    #[default]
    Off,
    /// Smallest budget the model accepts (`OpenAI` `reasoning_effort=minimal`).
    Minimal,
    /// Light reasoning. `OpenAI` maps to `reasoning_effort=low`.
    Low,
    /// Default reasoning level. `OpenAI` maps to `reasoning_effort=medium`.
    Medium,
    /// Heavier reasoning. `OpenAI` maps to `reasoning_effort=high`.
    High,
    /// Maximum supported reasoning. `OpenAI` keeps `high`; providers
    /// with budget-based thinking allocate their largest tier.
    #[serde(rename = "xhigh")]
    XHigh,
}

impl ThinkingLevel {
    /// Cycle to the next level in `Off -> Minimal -> Low -> Medium ->
    /// High -> XHigh -> Off` order. Used by the TUI's `Shift+Tab`
    /// rotation.
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Minimal,
            Self::Minimal => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::XHigh,
            Self::XHigh => Self::Off,
        }
    }

    /// Short, lowercase label for the modeline pill. The `Off`
    /// variant returns `"off"`; callers that want to suppress the
    /// pill entirely should check [`Self::is_off`] first.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    /// Wire-stable kebab-case identifier used in plugin event payloads
    /// and session-entry records. Mirrors the [`serde`] rename so
    /// `ThinkingLevel::High.as_str() == "high"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.label()
    }

    /// Parse a wire identifier produced by [`Self::as_str`]. Returns
    /// `None` for unrecognized values; callers that want a strict
    /// fallback should use `unwrap_or(Self::Off)`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            _ => None,
        }
    }

    /// `true` when thinking is disabled. Providers should use this to
    /// decide whether to omit the thinking-related body field.
    #[must_use]
    pub fn is_off(self) -> bool {
        matches!(self, Self::Off)
    }

    /// Map the level to `OpenAI`'s `reasoning_effort` enum string.
    /// Returns `None` for [`Self::Off`] (caller should omit the field).
    /// `XHigh` collapses to `"high"` because `OpenAI` tops out at `high`.
    #[must_use]
    pub fn openai_reasoning_effort(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High | Self::XHigh => Some("high"),
        }
    }

    /// Default budget in thinking tokens for budget-based providers
    /// (Anthropic, Gemini) when a model has no entry in
    /// [`crate::ModelInfo::thinking_levels`]. Returns `None` for
    /// [`Self::Off`].
    #[must_use]
    pub fn default_budget_tokens(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Minimal => Some(1_024),
            Self::Low => Some(4_096),
            Self::Medium => Some(8_192),
            Self::High => Some(16_384),
            Self::XHigh => Some(32_768),
        }
    }
}

/// A single streaming request to a provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamRequest {
    /// Model id, the part after `provider:` in the registry resolver.
    pub model: String,
    /// Conversation history, ending with the most recent user turn.
    pub messages: Vec<Message>,
    /// System prompt, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Tools available to the model this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    /// Maximum number of output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Optional thinking budget. Providers prefer [`Self::level`]
    /// when both are set; this raw budget exists for callers that
    /// need to bypass the catalog mapping (Lua hooks, tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// Unified thinking effort for this turn. Providers translate it
    /// to their native shape (Anthropic budget tokens, `OpenAI`
    /// `reasoning_effort`, Gemini `thinkingConfig.thinkingBudget`).
    /// When set, takes precedence over [`Self::thinking`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<ThinkingLevel>,
}

impl StreamRequest {
    /// Construct a minimal request with just a model id and message history.
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            system: None,
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            thinking: None,
            level: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kage_core::Role;

    #[test]
    fn new_omits_optional_fields() {
        let req = StreamRequest::new("anthropic:claude-sonnet-4-6", vec![]);
        assert_eq!(req.model, "anthropic:claude-sonnet-4-6");
        assert!(req.messages.is_empty());
        assert!(req.system.is_none());
        assert!(req.tools.is_empty());
        assert!(req.max_output_tokens.is_none());
        assert!(req.temperature.is_none());
        assert!(req.thinking.is_none());
        assert!(req.level.is_none());
    }

    #[test]
    fn empty_optional_fields_omitted_from_json() {
        let req = StreamRequest::new("m", vec![]);
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("system").is_none());
        assert!(json.get("tools").is_none());
        assert!(json.get("max_output_tokens").is_none());
        assert!(json.get("temperature").is_none());
        assert!(json.get("thinking").is_none());
        assert!(json.get("level").is_none());
    }

    #[test]
    fn populated_request_roundtrips() {
        let mut req = StreamRequest::new(
            "openai:gpt-4o",
            vec![Message::new(Role::User, vec![], None)],
        );
        req.system = Some("you are helpful".into());
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            schema: serde_json::json!({"type":"object"}),
        }];
        req.max_output_tokens = Some(2_000);
        req.temperature = Some(0.7);
        req.thinking = Some(ThinkingConfig {
            budget_tokens: 5_000,
        });
        req.level = Some(ThinkingLevel::Medium);

        let s = serde_json::to_string(&req).unwrap();
        let back: StreamRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn level_cycle_walks_full_loop() {
        let mut lv = ThinkingLevel::Off;
        let mut seen = vec![lv];
        for _ in 0..6 {
            lv = lv.cycle();
            seen.push(lv);
        }
        assert_eq!(
            seen,
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::XHigh,
                ThinkingLevel::Off,
            ]
        );
    }

    #[test]
    fn level_serializes_as_snake_case() {
        let v = serde_json::to_value(ThinkingLevel::XHigh).unwrap();
        assert_eq!(v, serde_json::json!("xhigh"));
        let back: ThinkingLevel = serde_json::from_value(v).unwrap();
        assert_eq!(back, ThinkingLevel::XHigh);
    }

    #[test]
    fn level_parse_roundtrips_each_variant() {
        for lv in [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
        ] {
            assert_eq!(ThinkingLevel::parse(lv.as_str()), Some(lv));
        }
        assert_eq!(ThinkingLevel::parse("nope"), None);
    }

    #[test]
    fn openai_reasoning_effort_caps_at_high() {
        assert_eq!(ThinkingLevel::Off.openai_reasoning_effort(), None);
        assert_eq!(
            ThinkingLevel::Minimal.openai_reasoning_effort(),
            Some("minimal")
        );
        assert_eq!(ThinkingLevel::High.openai_reasoning_effort(), Some("high"));
        assert_eq!(ThinkingLevel::XHigh.openai_reasoning_effort(), Some("high"));
    }

    #[test]
    fn default_budget_tokens_increases_with_level() {
        assert_eq!(ThinkingLevel::Off.default_budget_tokens(), None);
        assert!(
            ThinkingLevel::Minimal.default_budget_tokens()
                < ThinkingLevel::Low.default_budget_tokens()
        );
        assert!(
            ThinkingLevel::Low.default_budget_tokens()
                < ThinkingLevel::Medium.default_budget_tokens()
        );
        assert!(
            ThinkingLevel::Medium.default_budget_tokens()
                < ThinkingLevel::High.default_budget_tokens()
        );
        assert!(
            ThinkingLevel::High.default_budget_tokens()
                < ThinkingLevel::XHigh.default_budget_tokens()
        );
    }
}
