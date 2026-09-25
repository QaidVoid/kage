//! The model, thinking and mode config options of a client session.

use kage_acp::acp::{
    SessionConfigCategory, SessionConfigKind, SessionConfigOption, SessionConfigSelectOption,
};
use kage_core::ThinkingLevel;
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{CommandKind, SessionState};
use kage_jsonrpc::RpcError;
use kage_provider::ProviderRegistry;

use crate::engine::{AUTO_THINKING, SessionSpec};

/// What a client session's config options last showed.
pub(super) struct Shown {
    pub(super) settings: Settings,
    /// Set while the engine has yet to apply a change the client made, so
    /// the older states it still reports are not sent back to the client.
    pub(super) catching_up: bool,
}

impl Shown {
    /// Takes in a state the engine reported, and says whether the client
    /// has to hear of it.
    pub(super) fn observe(&mut self, settings: &Settings) -> bool {
        if self.catching_up {
            self.catching_up = *settings != self.settings;
            return false;
        }
        if *settings == self.settings {
            return false;
        }
        self.settings = settings.clone();
        true
    }
}

/// The session settings a client sees and changes as config options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Settings {
    pub(super) model: String,
    /// `None` is the automatic level, offered as `default`.
    pub(super) thinking: Option<ThinkingLevel>,
    /// Levels the model accepts, the other `thinking` choices.
    pub(super) levels: Vec<ThinkingLevel>,
    pub(super) mode: Option<PermissionAction>,
}

impl Settings {
    pub(super) fn of(spec: &SessionSpec, registry: &ProviderRegistry) -> Self {
        let mut state = SessionState {
            model: spec.model.clone(),
            thinking: spec.cx.thinking_level,
            permission_mode: spec.gate.mode(),
            ..SessionState::default()
        };
        crate::engine::fit_to_model(&mut state, registry);
        Self::from(&state)
    }

    /// Sets option `id` to `value` and returns the engine command that
    /// makes the same change. `models` are the models the client may pick.
    pub(super) fn apply(
        &mut self,
        models: &[SessionConfigSelectOption],
        id: &str,
        value: &str,
    ) -> Result<CommandKind, RpcError> {
        let invalid = || RpcError::new(-32602, format!("invalid value {value} for option {id}"));
        match id {
            "model" => {
                if value != self.model && !models.iter().any(|m| m.value == value) {
                    return Err(invalid());
                }
                value.clone_into(&mut self.model);
                Ok(CommandKind::SetModel {
                    model: value.to_owned(),
                })
            }
            "thinking" => {
                let level = if value == AUTO_THINKING {
                    None
                } else {
                    let level = ThinkingLevel::parse(value).ok_or_else(invalid)?;
                    if !self.levels.contains(&level) {
                        return Err(invalid());
                    }
                    Some(level)
                };
                self.thinking = level;
                Ok(CommandKind::SetThinking { level })
            }
            "mode" => {
                let (_, mode, ..) = MODES.iter().find(|m| m.0 == value).ok_or_else(invalid)?;
                self.mode = *mode;
                Ok(CommandKind::SetPermissionMode { mode: *mode })
            }
            _ => Err(RpcError::new(-32602, format!("unknown config option {id}"))),
        }
    }
}

impl From<&SessionState> for Settings {
    fn from(state: &SessionState) -> Self {
        Self {
            model: state.model.clone(),
            thinking: state.thinking,
            levels: state.thinking_levels.clone(),
            mode: state.permission_mode,
        }
    }
}

/// The values of the `mode` option, named as `/permission` names them,
/// with the override each applies, a label and a description.
const MODES: [(&str, Option<PermissionAction>, &str, &str); 4] = [
    (
        "default",
        None,
        "Default",
        "The configured permission rules decide",
    ),
    (
        "ask",
        Some(PermissionAction::Ask),
        "Ask",
        "Ask before every tool call",
    ),
    (
        "allow",
        Some(PermissionAction::Allow),
        "Allow",
        "Run every tool call without asking",
    ),
    (
        "deny",
        Some(PermissionAction::Deny),
        "Deny",
        "Refuse every tool call",
    ),
];

/// The model, thinking and mode options showing `settings`. The model
/// option offers `models`, plus the current model when they lack it.
pub(super) fn config_options(
    models: &[SessionConfigSelectOption],
    settings: &Settings,
) -> Vec<SessionConfigOption> {
    let mut model_choices = models.to_vec();
    if !models.iter().any(|m| m.value == settings.model) {
        model_choices.insert(0, choice(&settings.model, &settings.model, None));
    }
    let auto = choice(
        AUTO_THINKING,
        "Default",
        Some("High, or the nearest level the model accepts"),
    );
    let levels = settings
        .levels
        .iter()
        .map(|level| choice(level.as_str(), level.label(), None));
    let mode = MODES
        .iter()
        .find(|m| m.1 == settings.mode)
        .map_or("default", |m| m.0);
    vec![
        select(
            "model",
            "Model",
            SessionConfigCategory::Model,
            &settings.model,
            model_choices,
        ),
        select(
            "thinking",
            "Thinking",
            SessionConfigCategory::ThoughtLevel,
            settings
                .thinking
                .map_or(AUTO_THINKING, ThinkingLevel::as_str),
            std::iter::once(auto).chain(levels).collect(),
        ),
        select(
            "mode",
            "Mode",
            SessionConfigCategory::Mode,
            mode,
            MODES
                .iter()
                .map(|(value, _, name, description)| choice(value, name, Some(description)))
                .collect(),
        ),
    ]
}

fn select(
    id: &str,
    name: &str,
    category: SessionConfigCategory,
    current: &str,
    options: Vec<SessionConfigSelectOption>,
) -> SessionConfigOption {
    SessionConfigOption {
        id: id.to_owned(),
        name: name.to_owned(),
        description: None,
        category: Some(category),
        kind: SessionConfigKind::Select,
        current_value: current.to_owned(),
        options,
    }
}

pub(super) fn choice(
    value: &str,
    name: &str,
    description: Option<&str>,
) -> SessionConfigSelectOption {
    SessionConfigSelectOption {
        value: value.to_owned(),
        name: name.to_owned(),
        description: description.map(str::to_owned),
    }
}
