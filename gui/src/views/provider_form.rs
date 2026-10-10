//! The model provider form: adds a provider or edits one, through
//! `_kage/config/set`, saves a typed key through `_kage/auth/set`, and
//! tests the connection and fetches the model list through
//! `_kage/config/test`, so the engine does the network work.
//!
//! A provider kage registers itself is saved as a `[providers.<id>]`
//! override of its endpoint, key variable and headers. One the config
//! defines is saved whole under `[providers.custom.<id>]`, starting from
//! the entry the snapshot holds, so fields the form does not show, such
//! as a model's prices, stay as written.

use std::collections::BTreeMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Div, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use kage_client::wire::{ConfigTestResult, ProbeModel, ProviderProbe};
use serde_json::{Map, Value, json};

use crate::store::{Store, StoreHandle as _};
use crate::theme::{FONT_MONO, FS_XS, Palette, R_FULL};
use crate::views::config_forms::{
    Pairs, Saving, field, form_head, icon_button, preview_block, segments, text_field, toml_preview,
};
use crate::views::kit::{BtnTone, btn_sm};
use crate::views::mcp_form::FormDone;
use crate::views::settings_config::Snapshot;

/// The protocols a custom provider may speak.
const PROTOCOLS: [(&str, &str); 3] = [
    ("openai", "OpenAI"),
    ("anthropic", "Anthropic"),
    ("gemini", "Gemini"),
];

/// What the form edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A provider kage registers itself, by id.
    Registered(String),
    /// A provider the config defines, by id, or a new one.
    Custom(Option<String>),
    /// A new custom provider for a local server that needs no key.
    Local,
    /// A new custom provider imported from a directory: its id and the
    /// entry to start from.
    Import {
        /// The id the directory gives it.
        id: String,
        /// The `[providers.custom.<id>]` table, models and prices
        /// included.
        entry: Value,
    },
}

/// Where the key comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyFrom {
    Env,
    Paste,
    None,
}

/// One row of the models table.
struct ModelRow {
    id: Entity<InputState>,
    context: Entity<InputState>,
    max_output: Entity<InputState>,
    name: Entity<InputState>,
}

/// The model provider form.
pub struct ProviderForm {
    store: Entity<Store>,
    target: Target,
    /// The entry as the snapshot holds it, the base a save starts from.
    base: Map<String, Value>,
    id: Entity<InputState>,
    display: Entity<InputState>,
    protocol: &'static str,
    key_from: KeyFrom,
    /// The variable kage reads by default, shown as the field's hint.
    default_env: String,
    /// Where the key is now, as the snapshot says.
    key_now: String,
    key_env: Entity<InputState>,
    key: Entity<InputState>,
    base_url: Entity<InputState>,
    headers: Pairs,
    models: Vec<ModelRow>,
    saving: Saving,
    /// Whether the config.toml preview shows.
    preview: bool,
    /// The test or fetch in flight, and whether it fills the models.
    probing: Option<(u64, bool)>,
    tested: Option<ConfigTestResult>,
    /// Fetched models waiting for the next render to become rows.
    fetched: Vec<Value>,
}

impl EventEmitter<FormDone> for ProviderForm {}

impl ProviderForm {
    /// A form for `target`, read from the raw snapshot `config`.
    pub fn new(
        store: Entity<Store>,
        target: Target,
        config: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&store, |this, store, cx| this.follow(&store, cx))
            .detach();
        let snapshot = Snapshot::parse(config);
        let (id, base) = match &target {
            Target::Registered(id) => (id.clone(), config["providers"][id].clone()),
            Target::Custom(Some(id)) => (id.clone(), config["providers"]["custom"][id].clone()),
            Target::Custom(None) => (String::new(), Value::Null),
            Target::Local => (
                "local".to_owned(),
                json!({ "base_url": "http://localhost:11434/v1", "api_key_env": "" }),
            ),
            Target::Import { id, entry } => (id.clone(), entry.clone()),
        };
        let base = base.as_object().cloned().unwrap_or_default();
        let text = |key: &str| {
            base.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let state = snapshot.key_state(&id).cloned().unwrap_or_default();
        let default_env = if state.env.is_empty() || base.contains_key("api_key_env") {
            format!("{}_API_KEY", id.to_uppercase().replace('-', "_"))
        } else {
            state.env.clone()
        };
        let key_from = match base.get("api_key_env").and_then(Value::as_str) {
            // The config names a key variable or turns the key off;
            // honor that over anything else.
            Some("") => KeyFrom::None,
            Some(_) => KeyFrom::Env,
            // With nothing configured, start on the paste field so a
            // typed key is what the probe uses and Save keeps in the
            // auth store, rather than pointing at an env var.
            _ if state.source == "auth" => KeyFrom::Paste,
            _ if state.source == "env" => KeyFrom::Env,
            _ => KeyFrom::Paste,
        };
        let headers: BTreeMap<String, String> = base
            .get("headers")
            .and_then(|headers| serde_json::from_value(headers.clone()).ok())
            .unwrap_or_default();
        let models = base
            .get("models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .map(|model| model_row(window, cx, model))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            id: text_field(window, cx, "my-provider", &id),
            display: text_field(
                window,
                cx,
                "shown in the model picker",
                &text("display_name"),
            ),
            protocol: match text("kind").as_str() {
                "anthropic" => "anthropic",
                "gemini" => "gemini",
                _ => "openai",
            },
            key_from,
            key_env: text_field(window, cx, "", &text("api_key_env")),
            key: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(if state.source == "auth" {
                        "saved; type to replace"
                    } else {
                        "sk-..."
                    })
                    .masked(true)
            }),
            default_env,
            key_now: state.source,
            base_url: text_field(
                window,
                cx,
                if matches!(target, Target::Registered(_)) {
                    "the provider's own endpoint"
                } else {
                    "https://api.example.com/v1"
                },
                &text("base_url"),
            ),
            headers: Pairs::new(window, cx, &headers),
            models,
            saving: Saving::Idle,
            preview: false,
            probing: None,
            tested: None,
            fetched: Vec::new(),
            store,
            target,
            base,
        }
    }

    /// Whether the form edits a provider kage registers itself.
    fn registered(&self) -> bool {
        matches!(self.target, Target::Registered(_))
    }

    /// The provider's id: the edited one, or the one typed.
    fn provider_id(&self, cx: &App) -> String {
        match &self.target {
            Target::Registered(id) | Target::Custom(Some(id)) => id.clone(),
            Target::Custom(None) | Target::Local | Target::Import { .. } => {
                self.id.read(cx).value().trim().to_owned()
            }
        }
    }

    fn read(input: &Entity<InputState>, cx: &App) -> String {
        input.read(cx).value().trim().to_owned()
    }

    /// Follows the store: closes once the save landed, and takes the
    /// answer of the test in flight.
    fn follow(&mut self, store: &Entity<Store>, cx: &mut Context<Self>) {
        if self.saving.settle(store.read(cx)) {
            cx.emit(FormDone);
        }
        if let Some((id, fill)) = self.probing
            && let Some(result) = store.read(cx).test_result(id).cloned()
        {
            self.probing = None;
            if fill && result.ok {
                self.fill_models(&result.models, cx);
            }
            self.tested = Some(result);
        }
        cx.notify();
    }

    /// The probe the form describes, with a typed key for this request
    /// only.
    fn probe(&self, cx: &App) -> ProviderProbe {
        let url = Self::read(&self.base_url, cx);
        ProviderProbe {
            id: self.provider_id(cx),
            kind: (!self.registered()).then(|| self.protocol.to_owned()),
            base_url: (!url.is_empty()).then_some(url),
            api_key_env: match self.key_from {
                KeyFrom::None => Some(String::new()),
                KeyFrom::Env => Some(Self::read(&self.key_env, cx)).filter(|env| !env.is_empty()),
                KeyFrom::Paste => None,
            },
            api_key: (self.key_from == KeyFrom::Paste)
                .then(|| Self::read(&self.key, cx))
                .filter(|key| !key.is_empty()),
            headers: self.headers.values(cx),
        }
    }

    /// Asks the engine to list the provider's models; `fill` adds the
    /// ones the table lacks.
    fn test(&mut self, fill: bool, cx: &mut Context<Self>) {
        let probe = self.probe(cx);
        let id = self.store.act(cx, |store| store.config_test(probe));
        self.probing = Some((id, fill));
        self.tested = None;
        cx.notify();
    }

    /// Queues the fetched models the table does not hold yet; the next
    /// render makes them rows.
    fn fill_models(&mut self, fetched: &[ProbeModel], cx: &App) {
        let held: Vec<String> = self
            .models
            .iter()
            .map(|row| Self::read(&row.id, cx))
            .collect();
        self.fetched = fetched
            .iter()
            .filter(|model| !held.contains(&model.id))
            .map(|model| {
                json!({
                    "id": model.id,
                    "name": model.name.clone().unwrap_or_default(),
                    "context": model.context,
                    "max_output": model.max_output,
                })
            })
            .collect();
    }

    /// The config entry's path and value, or why the form cannot save.
    fn entry(&self, cx: &App) -> Result<(Vec<String>, Option<Value>), String> {
        let id = self.provider_id(cx);
        let url = Self::read(&self.base_url, cx);
        if !(url.is_empty() || url.starts_with("http://") || url.starts_with("https://")) {
            return Err("The base URL must start with http:// or https://.".to_owned());
        }
        let mut entry = self.base.clone();
        let mut put = |key: &str, value: Option<Value>| match value {
            Some(value) => {
                entry.insert(key.to_owned(), value);
            }
            None => {
                entry.remove(key);
            }
        };
        put("base_url", (!url.is_empty()).then(|| json!(url)));
        let env = Self::read(&self.key_env, cx);
        put(
            "api_key_env",
            match self.key_from {
                KeyFrom::None => Some(json!("")),
                KeyFrom::Env if !env.is_empty() => Some(json!(env)),
                KeyFrom::Env | KeyFrom::Paste => None,
            },
        );
        let headers = self.headers.values(cx);
        put("headers", (!headers.is_empty()).then(|| json!(headers)));
        if self.registered() {
            let path = vec!["providers".to_owned(), id];
            return Ok((path, (!entry.is_empty()).then_some(Value::Object(entry))));
        }
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err("Name the provider with lowercase letters, digits or dashes.".to_owned());
        }
        if url.is_empty() {
            return Err("Set the base URL.".to_owned());
        }
        let display = Self::read(&self.display, cx);
        put(
            "display_name",
            (!display.is_empty()).then(|| json!(display)),
        );
        put("kind", Some(json!(self.protocol)));
        let models = self.model_entries(cx)?;
        if models.is_empty() {
            return Err("Add at least one model, or fetch them from /models.".to_owned());
        }
        put("models", Some(Value::Array(models)));
        // The snapshot names every field, defaults too; a default the
        // file did not need stays out of it.
        for (key, default) in [("tool_use", true), ("thinking", false), ("caching", false)] {
            if entry.get(key) == Some(&json!(default)) {
                entry.remove(key);
            }
        }
        let path = vec!["providers".to_owned(), "custom".to_owned(), id];
        Ok((path, Some(Value::Object(entry))))
    }

    /// The models table as `[[providers.custom.<id>.models]]` entries,
    /// each over the saved entry of the same id so its other fields
    /// stay.
    fn model_entries(&self, cx: &App) -> Result<Vec<Value>, String> {
        let saved: Vec<&Map<String, Value>> = self
            .base
            .get("models")
            .and_then(Value::as_array)
            .map(|models| models.iter().filter_map(Value::as_object).collect())
            .unwrap_or_default();
        let mut out = Vec::new();
        for row in &self.models {
            let id = Self::read(&row.id, cx);
            if id.is_empty() {
                continue;
            }
            let mut model = saved
                .iter()
                .find(|model| model.get("id").and_then(Value::as_str) == Some(id.as_str()))
                .map(|model| (*model).clone())
                .unwrap_or_default();
            let name = Self::read(&row.name, cx);
            model.insert("id".into(), json!(id));
            model.insert(
                "name".into(),
                json!(if name.is_empty() { id.clone() } else { name }),
            );
            for (key, input) in [("context", &row.context), ("max_output", &row.max_output)] {
                let text = Self::read(input, cx).replace(['_', ','], "");
                if text.is_empty() {
                    model.remove(key);
                } else {
                    let number: u64 = text
                        .parse()
                        .map_err(|_| format!("{id}: {key} must be a whole number"))?;
                    model.insert(key.into(), json!(number));
                }
            }
            out.push(Value::Object(model));
        }
        Ok(out)
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let (path, value) = match self.entry(cx) {
            Ok(entry) => entry,
            Err(why) => {
                self.saving = Saving::Refused(why);
                cx.notify();
                return;
            }
        };
        let key = (self.key_from == KeyFrom::Paste)
            .then(|| Self::read(&self.key, cx))
            .filter(|key| !key.is_empty());
        let provider = self.provider_id(cx);
        let id = self.store.act(cx, |store| {
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            let id = store.config_set(&path, value);
            if let Some(key) = key {
                store.save_key(&provider, Some(key));
            }
            id
        });
        self.saving = Saving::Sent(vec![id]);
        cx.notify();
    }

    /// Drops the override of a registered provider, or the whole entry
    /// of a custom one.
    fn remove(&mut self, cx: &mut Context<Self>) {
        let id = self.provider_id(cx);
        let path: Vec<String> = if self.registered() {
            vec!["providers".into(), id]
        } else {
            vec!["providers".into(), "custom".into(), id]
        };
        let request = self.store.act(cx, |store| {
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            store.config_set(&path, None)
        });
        self.saving = Saving::Sent(vec![request]);
        cx.notify();
    }

    /// Removes the key saved for the provider from the credential
    /// store.
    fn forget_key(&mut self, cx: &mut Context<Self>) {
        let provider = self.provider_id(cx);
        self.store.act(cx, |store| store.save_key(&provider, None));
        "missing".clone_into(&mut self.key_now);
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(FormDone);
    }
}

/// One models table row holding `model`'s fields.
fn model_row(window: &mut Window, cx: &mut App, model: &Value) -> ModelRow {
    let text = |key: &str| match &model[key] {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    };
    ModelRow {
        id: text_field(window, cx, "model-id", &text("id")),
        context: text_field(window, cx, "tokens", &text("context")),
        max_output: text_field(window, cx, "optional", &text("max_output")),
        name: text_field(window, cx, "optional", &text("name")),
    }
}

impl Render for ProviderForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for model in std::mem::take(&mut self.fetched) {
            let row = model_row(window, cx, &model);
            self.models.push(row);
        }
        let pal = Palette::active(cx);
        let this = cx.entity();
        let title = match &self.target {
            Target::Registered(id) if self.base.is_empty() && self.key_now == "missing" => {
                format!("Set up {id}")
            }
            Target::Registered(id) | Target::Custom(Some(id)) => format!("Edit {id}"),
            Target::Custom(None) | Target::Local | Target::Import { .. } => {
                "Add provider".to_owned()
            }
        };
        let mut form = v_flex()
            .gap(px(14.))
            .child(form_head(title, &this, Self::cancel, pal));
        if self.registered() {
            form = form.child(
                div()
                    .text_size(px(FS_XS))
                    .text_color(pal.muted)
                    .child("A provider kage registers itself. Edits are saved as an override in config.toml; Reset drops the override."),
            );
        } else {
            form = self.identity(form, &this, pal);
        }
        form = self.key_fields(form, &this, pal);
        form = form
            .child(field(
                "Base URL",
                !self.registered(),
                self.registered()
                    .then_some("Leave blank for the provider's own endpoint."),
                div()
                    .font_family(FONT_MONO)
                    .child(Input::new(&self.base_url).small()),
                pal,
            ))
            .child(field(
                "Headers",
                false,
                Some("Sent on every request. A blank value keeps the saved one."),
                self.headers.render(
                    "provider-headers",
                    &this,
                    |form: &mut Self| &mut form.headers,
                    pal,
                ),
                pal,
            ));
        if !self.registered() {
            form = form.child(self.models_table(&this, pal));
        }
        let entry = self.entry(cx).map(|(path, value)| match value {
            Some(value) => toml_preview(&path, &value),
            None => "# Saving removes this override.".to_owned(),
        });
        form.child(preview_block(
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

impl ProviderForm {
    fn identity(&self, form: Div, this: &Entity<Self>, pal: &'static Palette) -> Div {
        let pick = this.clone();
        let new = matches!(
            self.target,
            Target::Custom(None) | Target::Local | Target::Import { .. }
        );
        form.when(new, |form| {
            form.child(field(
                "Name",
                true,
                Some("The id in config.toml and in model names (id/model)."),
                div()
                    .w(px(260.))
                    .font_family(FONT_MONO)
                    .child(Input::new(&self.id).small()),
                pal,
            ))
        })
        .child(field(
            "Display name",
            false,
            None,
            div().w(px(260.)).child(Input::new(&self.display).small()),
            pal,
        ))
        .child(field(
            "API protocol",
            true,
            None,
            h_flex().child(segments(
                "provider-protocol",
                &PROTOCOLS,
                self.protocol,
                move |value, _, cx| {
                    pick.update(cx, |form, cx| {
                        form.protocol = value;
                        cx.notify();
                    });
                },
                pal,
            )),
            pal,
        ))
    }

    fn key_fields(&self, form: Div, this: &Entity<Self>, pal: &'static Palette) -> Div {
        let pick = this.clone();
        let mut choices = vec![("env", "Environment variable"), ("paste", "Paste a key")];
        if !self.registered() {
            choices.push(("none", "None"));
        }
        let current = match self.key_from {
            KeyFrom::Env => "env",
            KeyFrom::Paste => "paste",
            KeyFrom::None => "none",
        };
        let source = segments(
            "provider-key",
            &choices,
            current,
            move |value, _, cx| {
                pick.update(cx, |form, cx| {
                    form.key_from = match value {
                        "paste" => KeyFrom::Paste,
                        "none" => KeyFrom::None,
                        _ => KeyFrom::Env,
                    };
                    cx.notify();
                });
            },
            pal,
        );
        let form = form.child(field(
            "Key source",
            false,
            None,
            h_flex().child(source),
            pal,
        ));
        match self.key_from {
            KeyFrom::Env => {
                let found = match self.key_now.as_str() {
                    "env" => "set in the engine's environment",
                    "auth" => "a saved key is used while the variable is unset",
                    _ => "not set in the engine's environment",
                };
                form.child(field(
                    "API key variable",
                    false,
                    None,
                    v_flex()
                        .gap(px(4.))
                        .child(
                            div()
                                .w(px(260.))
                                .font_family(FONT_MONO)
                                .child(Input::new(&self.key_env).small()),
                        )
                        .child(
                            div()
                                .text_size(px(FS_XS))
                                .text_color(pal.faint)
                                .child(format!("Blank reads {}; {found}.", self.default_env)),
                        ),
                    pal,
                ))
            }
            KeyFrom::Paste => {
                let forget = this.clone();
                let saved = self.key_now == "auth";
                form.child(field(
                    "API key",
                    false,
                    Some("Saved to the engine's auth.json with 0600 permissions, never to config.toml."),
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .child(div().w(px(360.)).child(Input::new(&self.key).small()))
                        .when(saved, |row| {
                            row.child(
                                btn_sm("provider-forget-key", BtnTone::Plain, pal)
                                    .on_click(move |_, _, cx| {
                                        forget.update(cx, |form, cx| form.forget_key(cx));
                                    })
                                    .child("Forget saved key"),
                            )
                        }),
                    pal,
                ))
            }
            KeyFrom::None => form.child(
                div()
                    .text_size(px(FS_XS))
                    .text_color(pal.muted)
                    .child("No key: for local servers that do not check auth."),
            ),
        }
    }

    fn models_table(&self, this: &Entity<Self>, pal: &'static Palette) -> Div {
        let head = h_flex()
            .gap(px(6.))
            .text_size(px(FS_XS))
            .text_color(pal.faint)
            .child(div().flex_1().child("Model id"))
            .child(div().w(px(100.)).child("Context"))
            .child(div().w(px(100.)).child("Max output"))
            .child(div().flex_1().child("Display name"))
            .child(div().w(px(26.)));
        let mut table = v_flex().gap(px(6.)).child(head);
        for (ix, row) in self.models.iter().enumerate() {
            let owner = this.clone();
            table = table.child(
                h_flex()
                    .gap(px(6.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .font_family(FONT_MONO)
                            .child(Input::new(&row.id).small()),
                    )
                    .child(div().w(px(100.)).child(Input::new(&row.context).small()))
                    .child(div().w(px(100.)).child(Input::new(&row.max_output).small()))
                    .child(div().flex_1().child(Input::new(&row.name).small()))
                    .child(
                        icon_button(format!("model-rm-{ix}"), IconName::Trash, pal).on_click(
                            move |_, _, cx| {
                                owner.update(cx, |form, cx| {
                                    if ix < form.models.len() {
                                        form.models.remove(ix);
                                    }
                                    cx.notify();
                                });
                            },
                        ),
                    ),
            );
        }
        let (add, fetch) = (this.clone(), this.clone());
        let fetching = matches!(self.probing, Some((_, true)));
        table = table.child(
            h_flex()
                .gap(px(8.))
                .child(
                    div()
                        .id("model-add")
                        .px(px(8.))
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .rounded(px(R_FULL))
                        .border_1()
                        .border_dashed()
                        .border_color(pal.line)
                        .text_size(px(FS_XS))
                        .text_color(pal.muted)
                        .cursor_pointer()
                        .child(Icon::new(IconName::Plus).with_size(px(12.)))
                        .child("Add model")
                        .on_click(move |_, window, cx| {
                            add.update(cx, |form, cx| {
                                form.models.push(model_row(window, cx, &Value::Null));
                                cx.notify();
                            });
                        }),
                )
                .child(
                    btn_sm("model-fetch", BtnTone::Plain, pal)
                        .when(fetching, |button| button.opacity(0.6))
                        .on_click(move |_, _, cx| fetch.update(cx, |form, cx| form.test(true, cx)))
                        .child(Icon::new(IconName::Download).with_size(px(12.)))
                        .child(if fetching {
                            "Fetching\u{2026}"
                        } else {
                            "Fetch from /models"
                        }),
                ),
        );
        field(
            "Models",
            true,
            Some("Context windows come from the endpoint or the model catalog when known."),
            table,
            pal,
        )
    }

    fn test_line(&self, pal: &'static Palette) -> Option<Div> {
        if matches!(self.probing, Some((_, false))) {
            return Some(
                div()
                    .text_size(px(FS_XS))
                    .text_color(pal.muted)
                    .child("Testing\u{2026}"),
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
        let saved = match &self.target {
            Target::Registered(_) => !self.base.is_empty(),
            Target::Custom(id) => id.is_some(),
            Target::Local | Target::Import { .. } => false,
        };
        h_flex()
            .gap(px(8.))
            .pt(px(6.))
            .child(
                btn_sm("provider-test", BtnTone::Plain, pal)
                    .on_click(move |_, _, cx| test.update(cx, |form, cx| form.test(false, cx)))
                    .child("Test connection"),
            )
            .when(saved, |row| {
                row.child(
                    btn_sm("provider-remove", BtnTone::Danger, pal)
                        .on_click(move |_, _, cx| remove.update(cx, |form, cx| form.remove(cx)))
                        .child(if self.registered() {
                            "Reset"
                        } else {
                            "Delete provider"
                        }),
                )
            })
            .child(div().flex_1())
            .child(
                btn_sm("provider-cancel", BtnTone::Plain, pal)
                    .on_click(move |_, _, cx| cancel.update(cx, |form, cx| form.cancel(cx)))
                    .child("Cancel"),
            )
            .child(
                btn_sm("provider-save", BtnTone::Primary, pal)
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

    use super::{ProviderForm, Target};
    use crate::store::Store;

    fn snapshot() -> serde_json::Value {
        json!({
            "providers": {
                "custom": {"lab": {
                    "kind": "anthropic",
                    "base_url": "http://lab:8080",
                    "api_key_env": "",
                    "tool_use": false,
                    "headers": {"X-Team": "<redacted>"},
                    "models": [{"id": "m1", "name": "One", "context": 1000,
                                "cost": {"input": 1.0, "output": 2.0}}],
                }},
                "deepseek": {"base_url": "https://proxy/v1"},
            },
            "providerKeys": {
                "lab": {"env": "", "source": "unneeded"},
                "deepseek": {"env": "DEEPSEEK_API_KEY", "source": "missing"},
            },
        })
    }

    #[gpui_kit::test]
    fn an_edit_keeps_what_the_form_does_not_show(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let store = cx.new(|_| Store::new("/w", false));
        let config = snapshot();
        let (form, visual) = cx.add_window_view(|window: &mut Window, cx| {
            ProviderForm::new(
                store,
                Target::Custom(Some("lab".into())),
                &config,
                window,
                cx,
            )
        });
        let (path, value) = visual
            .update(|_, cx| form.update(cx, |form, cx| form.entry(cx)))
            .unwrap();
        assert_eq!(path, ["providers", "custom", "lab"]);
        let value = value.unwrap();
        assert_eq!(value["kind"], "anthropic");
        assert_eq!(value["api_key_env"], "");
        assert_eq!(value["tool_use"], false);
        assert_eq!(value["headers"]["X-Team"], "<redacted>");
        assert_eq!(value["models"][0]["cost"]["output"], 2.0);
        assert_eq!(value["models"][0]["context"], 1000);
    }

    #[gpui_kit::test]
    fn an_override_left_empty_is_dropped(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let store = cx.new(|_| Store::new("/w", false));
        let mut config = snapshot();
        config["providers"]["deepseek"] = json!({});
        let (form, visual) = cx.add_window_view(|window: &mut Window, cx| {
            ProviderForm::new(
                store,
                Target::Registered("deepseek".into()),
                &config,
                window,
                cx,
            )
        });
        let (path, value) = visual
            .update(|_, cx| form.update(cx, |form, cx| form.entry(cx)))
            .unwrap();
        assert_eq!(path, ["providers", "deepseek"]);
        assert_eq!(value, None);
    }

    #[gpui_kit::test]
    fn a_new_custom_provider_needs_a_name_url_and_model(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let store = cx.new(|_| Store::new("/w", false));
        let config = snapshot();
        let (form, visual) = cx.add_window_view(|window: &mut Window, cx| {
            ProviderForm::new(store, Target::Local, &config, window, cx)
        });
        let err = visual
            .update(|_, cx| form.update(cx, |form, cx| form.entry(cx)))
            .unwrap_err();
        assert!(err.contains("at least one model"), "{err}");
    }
}
