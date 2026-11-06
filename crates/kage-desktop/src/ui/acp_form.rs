//! The ACP agent form: adds an external ACP agent to `config.toml`,
//! picked as `acp:<name>` in the model picker, or edits one, and tests
//! its handshake through the engine.

use kage_client::wire::{AcpProbe, ConfigTestRequest, ConfigTestResult};
use serde_json::{Map, Value, json};

use crate::store::Store;
use crate::theme::{FAMILY_MONO_REGULAR, FS_XS, Palette, family};
use crate::ui::forms::{self, Pairs, Saving, Tone};
use crate::ui::settings_config::AcpAgent;

/// Agents the form can start from: a label, the name, the command and
/// its arguments.
const PRESETS: [(&str, &str, &str, &[&str]); 3] = [
    (
        "Claude Code",
        "claude",
        "npx",
        &["-y", "@agentclientprotocol/claude-agent-acp"],
    ),
    (
        "Codex",
        "codex",
        "npx",
        &["-y", "@zed-industries/codex-acp"],
    ),
    ("Gemini CLI", "gemini", "gemini", &["--experimental-acp"]),
];

/// The ACP agent form.
#[derive(Debug, Default)]
pub struct AcpForm {
    /// The agent being edited; `None` adds one.
    editing: Option<String>,
    /// The agent's name.
    name: String,
    /// The executable that starts the agent.
    command: String,
    /// The agent's arguments, one per line.
    args: String,
    /// The agent's extra environment.
    env: Pairs,
    /// Whether the TOML preview shows.
    preview: bool,
    /// Where the save stands.
    saving: Saving,
    /// The handshake in flight, and the last answer.
    testing: Option<u64>,
    /// The answer of the last handshake.
    tested: Option<ConfigTestResult>,
    /// What the controls asked for this frame.
    asked: AcpAsked,
}

/// What the ACP form's controls asked for this frame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct AcpAsked {
    /// Close without saving.
    close: bool,
    /// Save what the fields hold.
    save: bool,
    /// Drop the agent's entry.
    remove: bool,
    /// Run the handshake.
    test: bool,
}

impl AcpForm {
    /// A form for agent `existing`, or for a new one.
    #[must_use]
    pub fn new(existing: Option<(&str, &AcpAgent)>) -> Self {
        let blank = AcpAgent::default();
        let (name, agent) = existing.unwrap_or(("", &blank));
        Self {
            editing: existing.map(|(name, _)| name.to_owned()),
            name: name.to_owned(),
            command: agent.command.clone(),
            args: agent.args.join("\n"),
            env: Pairs::new(&agent.env),
            preview: false,
            saving: Saving::Idle,
            testing: None,
            tested: None,
            asked: AcpAsked::default(),
        }
    }

    /// The agent being edited, or `None` when the form adds one.
    #[must_use]
    pub fn editing(&self) -> Option<&str> {
        self.editing.as_deref()
    }

    /// Follows the store: `true` once the save landed, and takes the
    /// answer of the handshake in flight.
    pub fn follow(&mut self, store: &Store) -> bool {
        let mut done = false;
        if self.saving.settle(store) {
            done = true;
        }
        if let Some(id) = self.testing
            && let Some(result) = store.test_result(id).cloned()
        {
            self.testing = None;
            self.tested = Some(result);
        }
        done
    }

    /// The agent's arguments, one per line.
    fn args(&self) -> Vec<String> {
        self.args
            .lines()
            .map(str::trim)
            .filter(|arg| !arg.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// The agent's name and its `[acp.agents.<name>]` table, or why the
    /// form cannot save.
    pub fn entry(&self) -> Result<(String, Value), String> {
        let name = self
            .editing
            .clone()
            .unwrap_or_else(|| self.name.trim().to_owned());
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("Name the agent with letters, digits, dashes or underscores.".to_owned());
        }
        let command = self.command.trim().to_owned();
        if command.is_empty() {
            return Err("Set the command that starts the agent.".to_owned());
        }
        let mut table = Map::new();
        table.insert("command".into(), json!(command));
        let args = self.args();
        if !args.is_empty() {
            table.insert("args".into(), json!(args));
        }
        let env = self.env.values();
        if !env.is_empty() {
            table.insert("env".into(), json!(env));
        }
        Ok((name, Value::Object(table)))
    }

    /// Queues the write the form describes.
    pub fn save(&mut self, store: &mut Store) -> Result<u64, String> {
        let (name, value) = self.entry()?;
        let id = store.config_set(&["acp", "agents", &name], Some(value));
        self.saving = Saving::Sent(vec![id]);
        Ok(id)
    }

    /// Drops the agent's entry.
    pub fn remove(&mut self, store: &mut Store) -> Option<u64> {
        let name = self.editing.clone()?;
        let id = store.config_set(&["acp", "agents", &name], None);
        self.saving = Saving::Sent(vec![id]);
        Some(id)
    }

    /// Asks the engine to start the agent and run its handshake.
    pub fn test(&mut self, store: &mut Store) -> u64 {
        let name = self
            .editing
            .clone()
            .unwrap_or_else(|| self.name.trim().to_owned());
        let request = ConfigTestRequest {
            acp: Some(AcpProbe {
                name,
                command: self.command.trim().to_owned(),
                args: self.args(),
                env: self.env.values(),
            }),
            ..ConfigTestRequest::default()
        };
        let id = store.config_probe(&request);
        self.testing = Some(id);
        self.tested = None;
        id
    }

    /// The page's title.
    #[must_use]
    pub fn title(&self) -> String {
        match &self.editing {
            Some(name) => format!("Edit acp:{name}"),
            None => "Add ACP agent".to_owned(),
        }
    }

    /// Draws the form. Returns whether the form asked to close.
    pub fn show(&mut self, ui: &mut egui::Ui, store: &mut Store) -> bool {
        let palette = Palette::active_of(ui.ctx());
        self.asked = AcpAsked::default();
        let mut edit = AcpEdit::default();
        ui.vertical(|ui| {
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.set_max_width(ui.available_width() - 36.0);
                ui.vertical(|ui| {
                    let (back, _) = forms::form_head(ui, &self.title(), palette);
                    if back.clicked() {
                        self.asked.close = true;
                    }
                    ui.label(
                        egui::RichText::new(
                            "Configure an external ACP agent to use as acp:<name>.",
                        )
                        .size(FS_XS)
                        .color(palette.muted),
                    );
                    if self.editing.is_none() {
                        self.presets(ui, palette, &mut edit);
                        forms::field(ui, "Name", true, Some("Picked as acp:<name>."), |ui| {
                            let mut name = self.name.clone();
                            let response =
                                forms::text_field(ui, &mut name, "agent-name", true, palette);
                            if response.changed() {
                                edit.name = name;
                                edit.name_set = true;
                            }
                        });
                    }
                    forms::field(ui, "Command", true, None, |ui| {
                        let mut command = self.command.clone();
                        let response =
                            forms::text_field(ui, &mut command, "command", true, palette);
                        if response.changed() {
                            edit.command = command;
                            edit.command_set = true;
                        }
                    });
                    forms::field(ui, "Arguments", false, Some("One per line."), |ui| {
                        let mut args = self.args.clone();
                        let response =
                            forms::lines_field(ui, &mut args, "one argument per line", palette);
                        if response.changed() {
                            edit.args = args;
                            edit.args_set = true;
                        }
                    });
                    let mut env = std::mem::take(&mut self.env);
                    forms::field(
                        ui,
                        "Environment",
                        false,
                        Some("A blank value keeps the saved one."),
                        |ui| {
                            forms::pairs(ui, "acp-env", &mut env, palette);
                        },
                    );
                    self.env = env;
                    let preview = self.entry().ok().and_then(|(name, value)| {
                        forms::toml_preview(&["acp".to_owned(), "agents".to_owned(), name], &value)
                    });
                    let toggle = forms::preview_block(ui, self.preview, preview, palette);
                    if toggle.clicked() {
                        edit.preview = Some(!self.preview);
                    }
                    self.test_line(ui, palette);
                    if let Some((line, color)) = self.saving.line(palette) {
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new(line).size(FS_XS).color(color));
                    }
                    self.actions(ui, palette);
                });
            });
        });
        edit.apply(self);
        self.carry(store)
    }

    /// The preset chips a new agent starts from.
    fn presets(&self, ui: &mut egui::Ui, palette: &Palette, edit: &mut AcpEdit) {
        forms::field(ui, "Start from", false, None, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.spacing_mut().item_spacing.y = 6.0;
                for (label, name, command, args) in PRESETS {
                    let response = forms::button(ui, label, Tone::Plain, palette);
                    if response.clicked() {
                        edit.preset = Some((name, command, args));
                    }
                }
            });
        });
    }

    /// The line under the fields: the handshake's answer, or that one
    /// runs.
    fn test_line(&self, ui: &mut egui::Ui, palette: &Palette) {
        if self.testing.is_some() {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Starting the agent\u{2026}")
                    .size(FS_XS)
                    .color(palette.muted),
            );
            return;
        }
        let Some(result) = self.tested.as_ref() else {
            return;
        };
        let (icon, color) = if result.ok {
            (crate::icons::Icon::CircleCheck, palette.ok)
        } else {
            (crate::icons::Icon::CircleX, palette.danger)
        };
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            crate::icons::icon(ui, icon, 12.0, color);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(result.message.clone())
                        .size(FS_XS)
                        .family(family(FAMILY_MONO_REGULAR))
                        .color(color),
                )
                .selectable(false),
            );
        });
    }

    /// The action row.
    fn actions(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let test = forms::button(ui, "Test handshake", Tone::Plain, palette);
            if test.clicked() {
                self.asked.test = true;
            }
            if self.editing.is_some() {
                let remove = forms::button(ui, "Remove agent", Tone::Danger, palette);
                if remove.clicked() {
                    self.asked.remove = true;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let save = forms::button(ui, "Save", Tone::Primary, palette);
                if save.clicked() && !self.saving.busy() {
                    self.asked.save = true;
                }
                let cancel = forms::button(ui, "Cancel", Tone::Plain, palette);
                if cancel.clicked() {
                    self.asked.close = true;
                }
            });
        });
    }

    /// Carries out what the controls asked for.
    fn carry(&mut self, store: &mut Store) -> bool {
        let asked = self.asked;
        self.asked = AcpAsked::default();
        if asked.test {
            self.test(store);
        }
        if asked.remove {
            self.remove(store);
        }
        if asked.save
            && let Err(why) = self.save(store)
        {
            self.saving = Saving::Refused(why);
        }
        asked.close
    }
}

/// What the ACP form's fields asked to change this frame.
#[derive(Debug, Default)]
struct AcpEdit {
    /// The typed agent name.
    name: String,
    /// Whether the name field changed.
    name_set: bool,
    /// The typed command.
    command: String,
    /// Whether the command field changed.
    command_set: bool,
    /// The typed arguments.
    args: String,
    /// Whether the arguments field changed.
    args_set: bool,
    /// The chosen preview state.
    preview: Option<bool>,
    /// The preset to fill the fields from.
    preset: Option<(&'static str, &'static str, &'static [&'static str])>,
}

impl AcpEdit {
    /// Applies the edits to `form`.
    fn apply(self, form: &mut AcpForm) {
        if self.name_set {
            form.name = self.name;
        }
        if self.command_set {
            form.command = self.command;
        }
        if self.args_set {
            form.args = self.args;
        }
        if let Some(preview) = self.preview {
            form.preview = preview;
        }
        if let Some((name, command, args)) = self.preset {
            form.name = name.to_owned();
            form.command = command.to_owned();
            form.args = args.join("\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{AcpEdit, AcpForm, PRESETS};
    use crate::ui::forms::toml_preview;
    use crate::ui::settings_config::Snapshot;

    #[test]
    fn the_claude_preset_fills_the_current_command_line() {
        let (_, name, command, args) = PRESETS
            .iter()
            .find(|(label, _, _, _)| *label == "Claude Code")
            .copied()
            .expect("Claude Code preset");
        let mut form = AcpForm::new(None);
        AcpEdit {
            preset: Some((name, command, args)),
            ..Default::default()
        }
        .apply(&mut form);

        let (name, value) = form.entry().unwrap();
        assert_eq!(name, "claude");
        assert_eq!(
            value,
            json!({
                "command": "npx",
                "args": ["-y", "@agentclientprotocol/claude-agent-acp"],
            })
        );
    }

    #[test]
    fn an_agent_entry_holds_its_command_line() {
        let snapshot = Snapshot::parse(&json!({"acp": {"agents": {"claude": {
            "command": "npx",
            "args": ["-y", "@zed-industries/claude-code-acp"],
            "env": {"ANTHROPIC_API_KEY": "<redacted>"},
        }}}}));
        let form = AcpForm::new(Some(("claude", snapshot.acp_agent("claude").unwrap())));
        let (name, value) = form.entry().unwrap();
        assert_eq!(name, "claude");
        assert_eq!(
            value,
            json!({
                "command": "npx",
                "args": ["-y", "@zed-industries/claude-code-acp"],
                "env": {"ANTHROPIC_API_KEY": "<redacted>"},
            })
        );
    }

    #[test]
    fn a_new_agent_needs_a_name_and_a_command() {
        let form = AcpForm::new(None);
        let err = form.entry().unwrap_err();
        assert!(err.contains("Name the agent"), "{err}");
        let mut form = AcpForm::new(None);
        form.name = "codex".to_owned();
        assert_eq!(
            form.entry().unwrap_err(),
            "Set the command that starts the agent."
        );
    }

    #[test]
    fn the_preview_names_the_entry_it_would_write() {
        let preview = toml_preview(
            &["acp".to_owned(), "agents".to_owned(), "codex".to_owned()],
            &json!({"command": "npx", "args": ["-y", "codex-acp"]}),
        )
        .expect("a preview");
        assert!(preview.contains("[acp.agents.codex]"), "{preview}");
        assert!(preview.contains("command = \"npx\""), "{preview}");
        assert!(preview.contains("[[acp.agents.codex.args]]"), "{preview}");
    }

    #[test]
    fn the_edited_title_names_the_agent() {
        let snapshot = Snapshot::parse(&json!({"acp": {"agents": {"claude": {
            "command": "npx",
        }}}}));
        let form = AcpForm::new(Some(("claude", snapshot.acp_agent("claude").unwrap())));
        assert_eq!(form.title(), "Edit acp:claude");
    }
}
