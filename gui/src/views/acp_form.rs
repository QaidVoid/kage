//! The ACP agent form: adds an external ACP agent to `config.toml`,
//! picked as `acp:<name>` in the model picker, or edits one, and tests
//! its handshake through the engine.

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Div, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};
use kage_client::wire::{AcpProbe, ConfigTestRequest, ConfigTestResult};
use serde_json::{Map, Value, json};

use crate::store::Store;
use crate::theme::{FONT_MONO, FS_XS, Palette, R_FULL};
use crate::views::config_forms::{
    Pairs, Saving, field, form_head, preview_block, text_field, toml_preview,
};
use crate::views::kit::{BtnTone, btn_sm};
use crate::views::mcp_form::FormDone;
use crate::views::settings_config::AcpAgent;

/// Agents the form can start from: a label, the name, the command and
/// its arguments.
const PRESETS: [(&str, &str, &str, &[&str]); 3] = [
    (
        "Claude Code",
        "claude",
        "npx",
        &["-y", "@zed-industries/claude-code-acp"],
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
pub struct AcpForm {
    store: Entity<Store>,
    /// The agent being edited; `None` adds one.
    editing: Option<String>,
    name: Entity<InputState>,
    command: Entity<InputState>,
    args: Entity<TextareaState>,
    env: Pairs,
    preview: bool,
    saving: Saving,
    /// The handshake in flight, and the last answer.
    testing: Option<u64>,
    tested: Option<ConfigTestResult>,
}

impl EventEmitter<FormDone> for AcpForm {}

impl AcpForm {
    /// A form for agent `existing`, or for a new one.
    pub fn new(
        store: Entity<Store>,
        existing: Option<(&str, &AcpAgent)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&store, |this, store, cx| {
            if this.saving.settle(store.read(cx)) {
                cx.emit(FormDone);
            }
            if let Some(id) = this.testing
                && let Some(result) = store.read(cx).test_result(id)
            {
                this.testing = None;
                this.tested = Some(result.clone());
            }
            cx.notify();
        })
        .detach();
        let blank = AcpAgent::default();
        let (name, agent) = existing.unwrap_or(("", &blank));
        Self {
            editing: existing.map(|(name, _)| name.to_owned()),
            name: text_field(window, cx, "agent-name", name),
            command: text_field(window, cx, "command", &agent.command),
            args: args_field(window, cx, &agent.args.join("\n")),
            env: Pairs::new(window, cx, &agent.env),
            preview: false,
            saving: Saving::Idle,
            testing: None,
            tested: None,
            store,
        }
    }

    /// Fills the fields from a preset, made anew: a field the form has
    /// not shown yet cannot lay out text on the web.
    fn apply(
        &mut self,
        name: &str,
        command: &str,
        args: &[&str],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.name = text_field(window, cx, "agent-name", name);
        self.command = text_field(window, cx, "command", command);
        self.args = args_field(window, cx, &args.join("\n"));
        cx.notify();
    }

    fn read(input: &Entity<InputState>, cx: &App) -> String {
        input.read(cx).value().trim().to_owned()
    }

    fn args(&self, cx: &App) -> Vec<String> {
        self.args
            .read(cx)
            .value()
            .lines()
            .map(str::trim)
            .filter(|arg| !arg.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// The agent's name and its `[acp.agents.<name>]` table, or why the
    /// form cannot save.
    fn entry(&self, cx: &App) -> Result<(String, Value), String> {
        let name = self
            .editing
            .clone()
            .unwrap_or_else(|| Self::read(&self.name, cx));
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("Name the agent with letters, digits, dashes or underscores.".to_owned());
        }
        let command = Self::read(&self.command, cx);
        if command.is_empty() {
            return Err("Set the command that starts the agent.".to_owned());
        }
        let mut table = Map::new();
        table.insert("command".into(), json!(command));
        let args = self.args(cx);
        if !args.is_empty() {
            table.insert("args".into(), json!(args));
        }
        let env = self.env.values(cx);
        if !env.is_empty() {
            table.insert("env".into(), json!(env));
        }
        Ok((name, Value::Object(table)))
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        match self.entry(cx) {
            Ok((name, value)) => {
                let id = self.store.update(cx, |store, cx| {
                    cx.notify();
                    store.config_set(&["acp", "agents", &name], Some(value))
                });
                self.saving = Saving::Sent(vec![id]);
            }
            Err(why) => self.saving = Saving::Refused(why),
        }
        cx.notify();
    }

    fn remove(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.editing.clone() else {
            return;
        };
        let id = self.store.update(cx, |store, cx| {
            cx.notify();
            store.config_set(&["acp", "agents", &name], None)
        });
        self.saving = Saving::Sent(vec![id]);
        cx.notify();
    }

    /// Asks the engine to start the agent and run its handshake.
    fn test(&mut self, cx: &mut Context<Self>) {
        let name = self
            .editing
            .clone()
            .unwrap_or_else(|| Self::read(&self.name, cx));
        let request = ConfigTestRequest {
            acp: Some(AcpProbe {
                name,
                command: Self::read(&self.command, cx),
                args: self.args(cx),
                env: self.env.values(cx),
            }),
            ..ConfigTestRequest::default()
        };
        let id = self.store.update(cx, |store, cx| {
            cx.notify();
            store.config_probe(&request)
        });
        self.testing = Some(id);
        self.tested = None;
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(FormDone);
    }
}

/// The arguments field, one argument per line, holding `args`.
fn args_field(window: &mut Window, cx: &mut App, args: &str) -> Entity<TextareaState> {
    let args = args.to_owned();
    cx.new(|cx| {
        TextareaState::new(window, cx)
            .placeholder("one argument per line")
            .auto_grow(2, 8)
            .default_value(args)
    })
}

impl Render for AcpForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = Palette::active(cx);
        let this = cx.entity();
        let title = match &self.editing {
            Some(name) => format!("Edit acp:{name}"),
            None => "Add ACP agent".to_owned(),
        };
        let mut form = v_flex()
            .gap(px(14.))
            .child(form_head(title, &this, Self::cancel, pal))
            .child(div().text_size(px(FS_XS)).text_color(pal.muted).child(
                "An external agent kage drives over ACP, picked as acp:<name> in the model picker.",
            ));
        if self.editing.is_none() {
            let mut chips = h_flex().gap(px(6.)).flex_wrap();
            for (label, name, command, args) in PRESETS {
                let this = this.clone();
                chips = chips.child(
                    div()
                        .id(SharedString::from(format!("acp-preset-{name}")))
                        .px(px(10.))
                        .h(px(26.))
                        .flex()
                        .items_center()
                        .rounded(px(R_FULL))
                        .border_1()
                        .border_color(pal.line)
                        .bg(pal.surface)
                        .text_size(px(FS_XS))
                        .text_color(pal.ink)
                        .cursor_pointer()
                        .child(label)
                        .on_click(move |_, window, cx| {
                            this.update(cx, |form, cx| form.apply(name, command, args, window, cx));
                        }),
                );
            }
            form = form
                .child(field("Start from", false, None, chips, pal))
                .child(field(
                    "Name",
                    true,
                    Some("Picked as acp:<name>."),
                    div()
                        .w(px(260.))
                        .font_family(FONT_MONO)
                        .child(Input::new(&self.name).small()),
                    pal,
                ));
        }
        let entry = self
            .entry(cx)
            .map(|(name, value)| toml_preview(&["acp".into(), "agents".into(), name], &value));
        form.child(field(
            "Command",
            true,
            None,
            div()
                .font_family(FONT_MONO)
                .child(Input::new(&self.command).small()),
            pal,
        ))
        .child(field(
            "Arguments",
            false,
            Some("One per line."),
            div()
                .font_family(FONT_MONO)
                .child(Textarea::new(&self.args)),
            pal,
        ))
        .child(field(
            "Environment",
            false,
            Some("A blank value keeps the saved one."),
            self.env
                .render("acp-env", &this, |form: &mut Self| &mut form.env, pal),
            pal,
        ))
        .child(preview_block(
            self.preview,
            entry,
            &this,
            |form: &mut Self| &mut form.preview,
            pal,
        ))
        .children(self.test_line(pal))
        .children(self.saving.line(pal))
        .child(self.actions(&this, pal))
    }
}

impl AcpForm {
    fn test_line(&self, pal: &'static Palette) -> Option<Div> {
        if self.testing.is_some() {
            return Some(
                div()
                    .text_size(px(FS_XS))
                    .text_color(pal.muted)
                    .child("Starting the agent\u{2026}"),
            );
        }
        let result = self.tested.as_ref()?;
        let (icon, color) = if result.ok {
            (IconName::CircleCheck, pal.ok)
        } else {
            (IconName::CircleX, pal.danger)
        };
        Some(
            h_flex()
                .gap(px(6.))
                .items_start()
                .text_size(px(FS_XS))
                .text_color(color)
                .child(Icon::new(icon).with_size(px(12.)))
                .child(div().font_family(FONT_MONO).child(result.message.clone())),
        )
    }

    fn actions(&self, this: &Entity<Self>, pal: &'static Palette) -> Div {
        let busy = matches!(self.saving, Saving::Sent(_));
        let (test, save, cancel, remove) = (this.clone(), this.clone(), this.clone(), this.clone());
        h_flex()
            .gap(px(8.))
            .pt(px(6.))
            .child(
                btn_sm("acp-test", BtnTone::Plain, pal)
                    .on_click(move |_, _, cx| test.update(cx, |form, cx| form.test(cx)))
                    .child("Test handshake"),
            )
            .when_some(self.editing.as_ref(), |row, _| {
                row.child(
                    btn_sm("acp-remove", BtnTone::Danger, pal)
                        .on_click(move |_, _, cx| remove.update(cx, |form, cx| form.remove(cx)))
                        .child("Remove agent"),
                )
            })
            .child(div().flex_1())
            .child(
                btn_sm("acp-cancel", BtnTone::Plain, pal)
                    .on_click(move |_, _, cx| cancel.update(cx, |form, cx| form.cancel(cx)))
                    .child("Cancel"),
            )
            .child(
                btn_sm("acp-save", BtnTone::Primary, pal)
                    .when(busy, |button| button.opacity(0.6))
                    .on_click(move |_, _, cx| {
                        save.update(cx, |form, cx| {
                            if !matches!(form.saving, Saving::Sent(_)) {
                                form.save(cx);
                            }
                        });
                    })
                    .child("Save"),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, TestAppContext, Window};
    use serde_json::json;

    use super::AcpForm;
    use crate::store::Store;
    use crate::views::settings_config::Snapshot;

    #[gpui_kit::test]
    fn an_agent_entry_holds_its_command_line(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let snapshot = Snapshot::parse(&json!({"acp": {"agents": {"claude": {
            "command": "npx",
            "args": ["-y", "@zed-industries/claude-code-acp"],
            "env": {"ANTHROPIC_API_KEY": "<redacted>"},
        }}}}));
        let store = cx.new(|_| Store::new("/w", false));
        let (form, visual) = cx.add_window_view(|window: &mut Window, cx| {
            AcpForm::new(
                store,
                Some(("claude", snapshot.acp_agent("claude").unwrap())),
                window,
                cx,
            )
        });
        let (name, value) = visual
            .update(|_, cx| form.update(cx, |form, cx| form.entry(cx)))
            .unwrap();
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
}
