//! The MCP server form: adds a server to `config.toml` or edits one,
//! through `_kage/config/set`.

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};
use kage_client::wire::{ConfigTestRequest, McpProbe, ProbeTool};
use serde_json::{Map, Value, json};

use crate::store::Store;
use crate::theme::{FONT_MONO, FS_SM, FS_XS, Palette, R_FULL};
use crate::views::config_forms::{
    Pairs, Saving, field, form_head, preview_block, segments, text_field, toml_preview,
};
use crate::views::kit::{BtnTone, btn_sm, switch};
use crate::views::settings_config::McpServer;

/// The form closed: saved, removed or cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormDone;

/// Servers the form can start from: a name, the command line or URL.
const PRESETS: [(&str, Preset); 5] = [
    (
        "Filesystem",
        Preset::Stdio(
            "filesystem",
            "npx",
            &["-y", "@modelcontextprotocol/server-filesystem", "."],
        ),
    ),
    (
        "GitHub",
        Preset::Http("github", "https://api.githubcopilot.com/mcp/"),
    ),
    (
        "Context7",
        Preset::Http("context7", "https://mcp.context7.com/mcp"),
    ),
    (
        "Playwright",
        Preset::Stdio("playwright", "npx", &["@playwright/mcp@latest"]),
    ),
    (
        "SQLite",
        Preset::Stdio(
            "sqlite",
            "uvx",
            &["mcp-server-sqlite", "--db-path", "./db.sqlite"],
        ),
    ),
];

/// What a preset fills in.
#[derive(Debug, Clone, Copy)]
enum Preset {
    Stdio(&'static str, &'static str, &'static [&'static str]),
    Http(&'static str, &'static str),
}

/// The MCP server form.
pub struct McpForm {
    store: Entity<Store>,
    /// The server being edited; `None` adds one.
    editing: Option<String>,
    name: Entity<InputState>,
    http: bool,
    command: Entity<InputState>,
    args: Entity<TextareaState>,
    env: Pairs,
    url: Entity<InputState>,
    headers: Pairs,
    oauth: bool,
    client_id: Entity<InputState>,
    scope: Entity<InputState>,
    disabled: bool,
    /// The tools the model does not see, by the server's names.
    disabled_tools: Vec<String>,
    /// The tools the server listed, once listed.
    tools: Option<Vec<ProbeTool>>,
    /// The listing in flight.
    listing: Option<u64>,
    /// Why the last listing failed.
    list_error: Option<String>,
    /// Whether the config.toml preview shows.
    preview: bool,
    saving: Saving,
}

impl EventEmitter<FormDone> for McpForm {}

impl McpForm {
    /// A form for `server`, saved as `name`, or for a new server.
    pub fn new(
        store: Entity<Store>,
        existing: Option<(&str, &McpServer)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&store, |this, store, cx| {
            if this.saving.settle(store.read(cx)) {
                cx.emit(FormDone);
            }
            if let Some(id) = this.listing
                && let Some(result) = store.read(cx).test_result(id)
            {
                this.listing = None;
                if result.ok {
                    this.tools = Some(result.tools.clone());
                    this.list_error = None;
                } else {
                    this.list_error = Some(result.message.clone());
                }
            }
            cx.notify();
        })
        .detach();
        let blank = McpServer::default();
        let (name, server) = existing.unwrap_or(("", &blank));
        let oauth = server.oauth.as_ref();
        let text = |key: &str| {
            oauth
                .and_then(|oauth| oauth[key].as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let args = server.args.join("\n");
        Self {
            editing: existing.map(|(name, _)| name.to_owned()),
            name: text_field(window, cx, "server-name", name),
            http: server.url.is_some(),
            command: text_field(
                window,
                cx,
                "command",
                server.command.as_deref().unwrap_or_default(),
            ),
            args: args_field(window, cx, &args),
            env: Pairs::new(window, cx, &server.env),
            url: text_field(
                window,
                cx,
                "https://example.com/mcp",
                server.url.as_deref().unwrap_or_default(),
            ),
            headers: Pairs::new(window, cx, &server.headers),
            oauth: oauth.is_some(),
            client_id: text_field(window, cx, "pre-registered client id", &text("client_id")),
            scope: text_field(window, cx, "scope the server advertises", &text("scope")),
            disabled: server.disabled,
            disabled_tools: server.disabled_tools.clone(),
            tools: None,
            listing: None,
            list_error: None,
            preview: false,
            saving: Saving::Idle,
            store,
        }
    }

    /// Fills the fields from `preset`. The fields are made anew rather
    /// than set, because one the form has not shown yet cannot lay out
    /// text on the web.
    fn apply(&mut self, preset: Preset, window: &mut Window, cx: &mut Context<Self>) {
        match preset {
            Preset::Stdio(name, command, args) => {
                self.http = false;
                self.name = text_field(window, cx, "server-name", name);
                self.command = text_field(window, cx, "command", command);
                self.args = args_field(window, cx, &args.join("\n"));
            }
            Preset::Http(name, url) => {
                self.http = true;
                self.name = text_field(window, cx, "server-name", name);
                self.url = text_field(window, cx, "https://example.com/mcp", url);
            }
        }
        cx.notify();
    }

    /// The server's name and its `[mcp.servers.<name>]` table, or why
    /// the form cannot save.
    fn entry(&self, cx: &Context<Self>) -> Result<(String, Value), String> {
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
        let name = self.editing.clone().unwrap_or_else(|| read(&self.name));
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("Name the server with letters, digits, dashes or underscores.".to_owned());
        }
        let mut table = Map::new();
        if self.http {
            let url = read(&self.url);
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err("Set an http(s) URL.".to_owned());
            }
            table.insert("url".into(), json!(url));
            let headers = self.headers.values(cx);
            if !headers.is_empty() {
                table.insert("headers".into(), json!(headers));
            }
            if self.oauth {
                let mut oauth = Map::new();
                for (key, input) in [("client_id", &self.client_id), ("scope", &self.scope)] {
                    let value = read(input);
                    if !value.is_empty() {
                        oauth.insert(key.into(), json!(value));
                    }
                }
                table.insert("oauth".into(), Value::Object(oauth));
            }
        } else {
            let command = read(&self.command);
            if command.is_empty() {
                return Err("Set the command that starts the server.".to_owned());
            }
            table.insert("command".into(), json!(command));
            let args: Vec<String> = self
                .args
                .read(cx)
                .value()
                .lines()
                .map(str::trim)
                .filter(|arg| !arg.is_empty())
                .map(str::to_owned)
                .collect();
            if !args.is_empty() {
                table.insert("args".into(), json!(args));
            }
            let env = self.env.values(cx);
            if !env.is_empty() {
                table.insert("env".into(), json!(env));
            }
        }
        if self.disabled {
            table.insert("disabled".into(), json!(true));
        }
        if !self.disabled_tools.is_empty() {
            table.insert("disabled_tools".into(), json!(self.disabled_tools));
        }
        Ok((name, Value::Object(table)))
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        match self.entry(cx) {
            Ok((name, value)) => {
                let id = self.store.update(cx, |store, cx| {
                    cx.notify();
                    store.config_set(&["mcp", "servers", &name], Some(value))
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
            store.config_set(&["mcp", "servers", &name], None)
        });
        self.saving = Saving::Sent(vec![id]);
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(FormDone);
    }

    /// Asks the engine to connect to the server as the form describes
    /// it and list its tools.
    fn list_tools(&mut self, cx: &mut Context<Self>) {
        match self.entry(cx) {
            Ok((name, server)) => {
                let request = ConfigTestRequest {
                    mcp: Some(McpProbe { name, server }),
                    ..ConfigTestRequest::default()
                };
                let id = self.store.update(cx, |store, cx| {
                    cx.notify();
                    store.config_probe(&request)
                });
                self.listing = Some(id);
                self.list_error = None;
            }
            Err(why) => self.list_error = Some(why),
        }
        cx.notify();
    }

    /// Shows tool `name` to the model, or hides it.
    fn flip_tool(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(at) = self.disabled_tools.iter().position(|held| held == name) {
            self.disabled_tools.remove(at);
        } else {
            self.disabled_tools.push(name.to_owned());
            self.disabled_tools.sort();
        }
        cx.notify();
    }

    /// The tools section: the button that lists them, then a chip per
    /// tool, filled while the model sees it.
    fn tools_section(&self, this: &Entity<Self>, pal: &'static Palette) -> gpui_kit::Div {
        let list = this.clone();
        let mut head = h_flex().gap(px(8.)).items_center().child(
            btn_sm("mcp-list-tools", BtnTone::Plain, pal)
                .when(self.listing.is_some(), |button| button.opacity(0.6))
                .on_click(move |_, _, cx| list.update(cx, |form, cx| form.list_tools(cx)))
                .child(if self.listing.is_some() {
                    "Connecting\u{2026}"
                } else {
                    "List tools"
                }),
        );
        let names: Vec<String> = match &self.tools {
            Some(tools) => tools.iter().map(|tool| tool.name.clone()).collect(),
            None => self.disabled_tools.clone(),
        };
        if let Some(tools) = &self.tools {
            let shown = tools
                .iter()
                .filter(|tool| !self.disabled_tools.contains(&tool.name))
                .count();
            let (all, none) = (this.clone(), this.clone());
            let every: Vec<String> = names.clone();
            head = head
                .child(
                    div()
                        .text_size(px(FS_XS))
                        .text_color(pal.muted)
                        .child(format!("{shown} of {} enabled", tools.len())),
                )
                .child(
                    btn_sm("mcp-tools-all", BtnTone::Plain, pal)
                        .on_click(move |_, _, cx| {
                            all.update(cx, |form, cx| {
                                form.disabled_tools.clear();
                                cx.notify();
                            });
                        })
                        .child("Enable all"),
                )
                .child(
                    btn_sm("mcp-tools-none", BtnTone::Plain, pal)
                        .on_click(move |_, _, cx| {
                            let every = every.clone();
                            none.update(cx, |form, cx| {
                                form.disabled_tools = every;
                                cx.notify();
                            });
                        })
                        .child("Disable all"),
                );
        }
        let mut grid = h_flex().gap(px(6.)).flex_wrap();
        for name in names {
            let on = !self.disabled_tools.contains(&name);
            let flip = this.clone();
            let tool = name.clone();
            grid = grid.child(
                h_flex()
                    .id(SharedString::from(format!("mcp-tool-{name}")))
                    .gap(px(5.))
                    .px(px(8.))
                    .h(px(24.))
                    .items_center()
                    .rounded(px(R_FULL))
                    .border_1()
                    .border_color(if on { pal.accent } else { pal.line })
                    .bg(if on { pal.accent_soft } else { pal.surface })
                    .cursor_pointer()
                    .font_family(FONT_MONO)
                    .text_size(px(FS_XS))
                    .text_color(if on { pal.ink_strong } else { pal.muted })
                    .when(on, |chip| {
                        chip.child(Icon::new(IconName::Check).with_size(px(12.)))
                    })
                    .child(name)
                    .on_click(move |_, _, cx| {
                        flip.update(cx, |form, cx| form.flip_tool(&tool, cx))
                    }),
            );
        }
        let hint = match (&self.tools, &self.list_error) {
            (_, Some(why)) => Some((why.clone(), pal.danger)),
            (None, None) if self.disabled_tools.is_empty() => Some((
                "List the server's tools to choose which the model sees.".to_owned(),
                pal.faint,
            )),
            (None, None) => Some((
                "Hidden from the model. List the tools to change them.".to_owned(),
                pal.faint,
            )),
            (Some(_), None) => Some((
                "Per-tool approval rules live on the Permissions page.".to_owned(),
                pal.faint,
            )),
        };
        field(
            "Tools",
            false,
            None,
            v_flex().gap(px(8.)).child(head).child(grid).children(
                hint.map(|(text, color)| div().text_size(px(FS_XS)).text_color(color).child(text)),
            ),
            pal,
        )
    }
}

/// The arguments field, one argument per line, holding `args`.
fn args_field(window: &mut Window, cx: &mut Context<McpForm>, args: &str) -> Entity<TextareaState> {
    let args = args.to_owned();
    cx.new(|cx| {
        TextareaState::new(window, cx)
            .placeholder("one argument per line")
            .auto_grow(2, 8)
            .default_value(args)
    })
}

impl Render for McpForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = Palette::active(cx);
        let this = cx.entity();
        let title = match &self.editing {
            Some(name) => format!("Edit {name}"),
            None => "Add MCP server".to_owned(),
        };
        let mut form = v_flex()
            .gap(px(14.))
            .child(form_head(title, &this, Self::cancel, pal));
        if self.editing.is_none() {
            let mut chips = h_flex().gap(px(6.)).flex_wrap();
            for (label, preset) in PRESETS {
                let this = this.clone();
                chips = chips.child(
                    div()
                        .id(SharedString::from(format!("mcp-preset-{label}")))
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
                            this.update(cx, |form, cx| form.apply(preset, window, cx));
                        }),
                );
            }
            form = form
                .child(field("Start from", false, None, chips, pal))
                .child(field(
                    "Name",
                    true,
                    Some("Tools show as name__tool"),
                    div()
                        .w(px(260.))
                        .font_family(FONT_MONO)
                        .child(Input::new(&self.name).small()),
                    pal,
                ));
        }
        let transport = {
            let this = this.clone();
            segments(
                "mcp-transport",
                &[("stdio", "stdio"), ("http", "Streamable HTTP")],
                if self.http { "http" } else { "stdio" },
                move |value, _, cx| {
                    this.update(cx, |form, cx| {
                        form.http = value == "http";
                        cx.notify();
                    });
                },
                pal,
            )
        };
        form = form.child(field(
            "Transport",
            false,
            None,
            h_flex().child(transport),
            pal,
        ));
        form = if self.http {
            self.http_fields(form, &this, pal)
        } else {
            form.child(field(
                "Command",
                true,
                Some("Runs in the session's directory."),
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
                    .render("mcp-env", &this, |form: &mut Self| &mut form.env, pal),
                pal,
            ))
        };
        let toggle = this.clone();
        form = form.child(
            h_flex()
                .gap(px(10.))
                .items_center()
                .child(
                    switch("mcp-disabled", self.disabled, pal).on_click(move |_, _, cx| {
                        toggle.update(cx, |form, cx| {
                            form.disabled = !form.disabled;
                            cx.notify();
                        });
                    }),
                )
                .child(
                    div()
                        .text_size(px(FS_SM))
                        .text_color(pal.ink)
                        .child("Disabled: configured but not started"),
                ),
        );
        let entry = self
            .entry(cx)
            .map(|(name, value)| toml_preview(&["mcp".into(), "servers".into(), name], &value));
        form.child(self.tools_section(&this, pal))
            .child(preview_block(
                self.preview,
                entry,
                &this,
                |form: &mut Self| &mut form.preview,
                pal,
            ))
            .children(self.saving.line(pal))
            .child(self.actions(&this, pal))
    }
}

impl McpForm {
    fn http_fields(
        &self,
        form: gpui_kit::Div,
        this: &Entity<Self>,
        pal: &'static Palette,
    ) -> gpui_kit::Div {
        let toggle = this.clone();
        form.child(field(
            "URL",
            true,
            None,
            div()
                .font_family(FONT_MONO)
                .child(Input::new(&self.url).small()),
            pal,
        ))
        .child(field(
            "Headers",
            false,
            Some(
                "Sent on every request, such as Authorization. A blank value keeps the saved one.",
            ),
            self.headers.render(
                "mcp-headers",
                this,
                |form: &mut Self| &mut form.headers,
                pal,
            ),
            pal,
        ))
        .child(
            h_flex()
                .gap(px(10.))
                .items_center()
                .child(
                    switch("mcp-oauth", self.oauth, pal).on_click(move |_, _, cx| {
                        toggle.update(cx, |form, cx| {
                            form.oauth = !form.oauth;
                            cx.notify();
                        });
                    }),
                )
                .child(
                    div()
                        .text_size(px(FS_SM))
                        .text_color(pal.ink)
                        .child("Log in with OAuth"),
                ),
        )
        .when(self.oauth, |form| {
            form.child(
                h_flex()
                    .gap(px(10.))
                    .child(div().flex_1().child(field(
                        "Client id",
                        false,
                        Some("Leave blank to register kage with the server."),
                        Input::new(&self.client_id).small(),
                        pal,
                    )))
                    .child(div().flex_1().child(field(
                        "Scope",
                        false,
                        None,
                        Input::new(&self.scope).small(),
                        pal,
                    ))),
            )
        })
    }

    fn actions(&self, this: &Entity<Self>, pal: &'static Palette) -> gpui_kit::Div {
        let busy = matches!(self.saving, Saving::Sent(_));
        let (save, cancel, remove) = (this.clone(), this.clone(), this.clone());
        h_flex()
            .gap(px(8.))
            .pt(px(6.))
            .when_some(self.editing.as_ref(), |row, _| {
                row.child(
                    btn_sm("mcp-remove", BtnTone::Danger, pal)
                        .on_click(move |_, _, cx| remove.update(cx, |form, cx| form.remove(cx)))
                        .child("Remove server"),
                )
            })
            .child(div().flex_1())
            .child(
                btn_sm("mcp-cancel", BtnTone::Plain, pal)
                    .on_click(move |_, _, cx| cancel.update(cx, |form, cx| form.cancel(cx)))
                    .child("Cancel"),
            )
            .child(
                btn_sm("mcp-save", BtnTone::Primary, pal)
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

    use super::McpForm;
    use crate::store::Store;
    use crate::views::settings_config::Snapshot;

    #[gpui_kit::test]
    fn an_edited_server_keeps_its_secret_values(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let snapshot = Snapshot::parse(&json!({"mcp": {"servers": {"fs": {
            "command": "npx",
            "args": ["-y", "server-fs", "."],
            "env": {"TOKEN": "<redacted>", "MODE": "ro"},
        }}}}));
        let store = cx.new(|_| Store::new("/w", false));
        let (form, visual) = cx.add_window_view(|window: &mut Window, cx| {
            McpForm::new(
                store,
                Some(("fs", snapshot.mcp_server("fs").unwrap())),
                window,
                cx,
            )
        });
        let entry = visual.update(|_, cx| form.update(cx, |form, cx| form.entry(cx)));
        let (name, value) = entry.unwrap();
        assert_eq!(name, "fs");
        assert_eq!(
            value,
            json!({
                "command": "npx",
                "args": ["-y", "server-fs", "."],
                "env": {"TOKEN": "<redacted>", "MODE": "ro"},
            })
        );
    }

    #[gpui_kit::test]
    fn a_new_server_needs_a_name_and_a_url(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let store = cx.new(|_| Store::new("/w", false));
        let (form, visual) =
            cx.add_window_view(|window: &mut Window, cx| McpForm::new(store, None, window, cx));
        let entry = visual.update(|_, cx| form.update(cx, |form, cx| form.entry(cx)));
        assert!(entry.unwrap_err().contains("Name the server"));
        visual.update(|_, cx| {
            form.update(cx, |form, cx| {
                form.http = true;
                form.editing = Some("hub".into());
                assert_eq!(form.entry(cx).unwrap_err(), "Set an http(s) URL.");
            });
        });
    }
}
