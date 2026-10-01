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
    /// Whether the session plans before it changes anything, which
    /// rides above the permission mode.
    pub(super) plan: bool,
    /// Whether the session delegates repeated work through `swarm`.
    pub(super) swarm: bool,
    /// The goal the session works toward, if set.
    pub(super) goal: Option<String>,
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

    /// The settings a new session on `model` opens with.
    pub(super) fn fresh(model: &str, registry: &ProviderRegistry) -> Self {
        let mut state = SessionState {
            model: model.to_owned(),
            ..SessionState::default()
        };
        crate::engine::fit_to_model(&mut state, registry);
        Self::from(&state)
    }

    /// Sets option `id` to `value` and returns the engine commands that
    /// make the same change, in order. `models` are the models the
    /// client may pick. Leaving plan mode for a regular mode needs two
    /// commands, so a change can be more than one.
    pub(super) fn apply(
        &mut self,
        models: &[SessionConfigSelectOption],
        id: &str,
        value: &str,
    ) -> Result<Vec<CommandKind>, RpcError> {
        let invalid = || RpcError::new(-32602, format!("invalid value {value} for option {id}"));
        match id {
            "model" => {
                if value != self.model && !models.iter().any(|m| m.value == value) {
                    return Err(invalid());
                }
                value.clone_into(&mut self.model);
                Ok(vec![CommandKind::SetModel {
                    model: value.to_owned(),
                }])
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
                Ok(vec![CommandKind::SetThinking { level }])
            }
            "mode" => {
                let mut commands = Vec::new();
                if value == PLAN_MODE {
                    self.plan = true;
                    commands.push(CommandKind::PlanMode { on: true });
                    return Ok(commands);
                }
                let (_, mode, ..) = MODES.iter().find(|m| m.0 == value).ok_or_else(invalid)?;
                if self.plan {
                    self.plan = false;
                    commands.push(CommandKind::PlanMode { on: false });
                }
                self.mode = *mode;
                commands.push(CommandKind::SetPermissionMode { mode: *mode });
                Ok(commands)
            }
            "swarm" => {
                let on = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err(invalid()),
                };
                self.swarm = on;
                Ok(vec![CommandKind::SwarmMode { on }])
            }
            "goal" => {
                let goal = if value.is_empty() {
                    None
                } else {
                    Some(value.to_owned())
                };
                self.goal.clone_from(&goal);
                Ok(vec![CommandKind::SetGoal { goal }])
            }
            _ => Err(RpcError::new(-32602, format!("unknown config option {id}"))),
        }
    }

    /// The mode id a `current_mode_update` names: `plan` while the
    /// session plans, else the permission mode's id.
    pub(super) fn mode_id(&self) -> &'static str {
        if self.plan {
            PLAN_MODE
        } else {
            MODES
                .iter()
                .find(|m| m.1 == self.mode)
                .map_or("default", |m| m.0)
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
            plan: state.plan,
            swarm: state.swarm,
            goal: state.goal.clone(),
        }
    }
}

/// The values of the `mode` option, named as `/permission` names them,
/// with the override each applies, a label and a description.
const MODES: [(&str, Option<PermissionAction>, &str, &str); 4] = [
    (
        "default",
        None,
        "Ask When Needed",
        "Built-in tools run unless a config rule says otherwise; MCP tools ask",
    ),
    (
        "ask",
        Some(PermissionAction::Ask),
        "Always Ask",
        "Every tool call needs your approval first, reads included",
    ),
    (
        "allow",
        Some(PermissionAction::Allow),
        "Never Ask",
        "Everything runs without asking; only configured denies refuse",
    ),
    (
        "deny",
        Some(PermissionAction::Deny),
        "Read Only",
        "Reads, searches and fetches run; edits, commands and agents are refused",
    ),
];

/// The id of plan mode in the `mode` option and on
/// `current_mode_update`.
const PLAN_MODE: &str = "plan";

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
        "auto",
        Some("High, or the nearest level the model accepts"),
    );
    let levels = settings
        .levels
        .iter()
        .map(|level| choice(level.as_str(), level.label(), None));
    let mode_values = MODES
        .iter()
        .map(|(value, _, name, description)| choice(value, name, Some(description)))
        .chain(std::iter::once(choice(
            PLAN_MODE,
            "Plan",
            Some("Read-only until the agent presents a plan"),
        )));
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
            settings.mode_id(),
            mode_values.collect(),
        ),
        select(
            "swarm",
            "Swarm",
            SessionConfigCategory::Mode,
            if settings.swarm { "on" } else { "off" },
            vec![
                choice("off", "Off", Some("Do the work in this session")),
                choice("on", "On", Some("Delegate repeated work to swarm batches")),
            ],
        ),
        text(
            "goal",
            "Goal",
            Some("What done looks like; the session keeps working until a turn meets it"),
            settings.goal.as_deref().unwrap_or_default(),
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

fn text(id: &str, name: &str, description: Option<&str>, current: &str) -> SessionConfigOption {
    SessionConfigOption {
        id: id.to_owned(),
        name: name.to_owned(),
        description: description.map(str::to_owned),
        category: None,
        kind: SessionConfigKind::Text,
        current_value: current.to_owned(),
        options: Vec::new(),
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
