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

use kage_client::wire::{ConfigTestResult, ProbeModel, ProviderProbe};
use serde_json::{Map, Value, json};

use crate::store::Store;
use crate::theme::{FAMILY_MONO_REGULAR, FS_XS, Palette, family};
use crate::ui::forms::{self, Pairs, Saving, Tone};
use crate::ui::settings_config::Snapshot;

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
    /// A new custom provider a directory import filled in, by id and
    /// entry.
    Picked(String, serde_json::Value),
}

/// Where the key comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyFrom {
    /// The environment variable the config names.
    #[default]
    Env,
    /// A key typed into the form, saved through the engine's auth.
    Paste,
    /// No key at all, for local servers.
    None,
}

/// One row of the models table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ModelRow {
    /// The model's id.
    id: String,
    /// The context window in tokens.
    context: String,
    /// The most output tokens per turn.
    max_output: String,
    /// The name the picker shows.
    name: String,
}

/// What the form's controls asked for this frame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Asked {
    /// Test the connection.
    test: bool,
    /// Fetch the models from /models.
    fetch: bool,
    /// Forget the saved key.
    forget: bool,
    /// Drop the provider's entry.
    remove: bool,
    /// Save what the fields hold.
    save: bool,
    /// Close without saving.
    close: bool,
}

/// The model provider form.
#[derive(Debug, Default)]
pub struct ProviderForm {
    /// What the form edits.
    target: Option<Target>,
    /// The entry as the snapshot holds it, the base a save starts from.
    base: Map<String, Value>,
    /// The provider's id.
    id: String,
    /// The name the picker shows.
    display: String,
    /// The wire protocol a custom provider speaks.
    protocol: String,
    /// Where the key comes from.
    key_from: KeyFrom,
    /// The variable kage reads by default, shown as the field's hint.
    default_env: String,
    /// Where the key is now, as the snapshot says.
    key_now: String,
    /// The variable the config names.
    key_env: String,
    /// A key typed into the form, used for this request only.
    key: String,
    /// The endpoint.
    base_url: String,
    /// The headers the provider sends.
    headers: Pairs,
    /// The models the provider serves.
    models: Vec<ModelRow>,
    /// The `models_url` the saved entry lists its models from, which
    /// kage keeps in its cache: the table then only overrides it.
    models_url: Option<String>,
    /// Where the save stands.
    saving: Saving,
    /// The test or fetch in flight, and whether it fills the models.
    probing: Option<(u64, bool)>,
    /// The answer of the last test.
    tested: Option<ConfigTestResult>,
    /// Fetched models waiting for the next render to become rows.
    fetched: Vec<Value>,
    /// What the controls asked for this frame.
    asked: Asked,
}

impl ProviderForm {
    /// A form for `target`, read from the raw snapshot `config`.
    #[must_use]
    pub fn new(target: Target, config: &Value) -> Self {
        let snapshot = Snapshot::parse(config);
        let (id, base) = match &target {
            Target::Registered(id) => (id.clone(), config["providers"][id].clone()),
            Target::Custom(Some(id)) => (id.clone(), config["providers"]["custom"][id].clone()),
            Target::Custom(None) => (String::new(), Value::Null),
            Target::Local => (
                "local".to_owned(),
                json!({ "base_url": "http://localhost:11434/v1", "api_key_env": "" }),
            ),
            Target::Picked(id, entry) => (id.clone(), entry.clone()),
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
            Some("") => KeyFrom::None,
            _ if state.source == "auth" => KeyFrom::Paste,
            _ => KeyFrom::Env,
        };
        let headers: BTreeMap<String, String> = base
            .get("headers")
            .and_then(|headers| serde_json::from_value(headers.clone()).ok())
            .unwrap_or_default();
        let models = base
            .get("models")
            .and_then(Value::as_array)
            .map(|models| models.iter().map(model_row).collect())
            .unwrap_or_default();
        Self {
            id,
            display: text("display_name"),
            protocol: match text("kind").as_str() {
                "anthropic" => "anthropic",
                "gemini" | "google" => "gemini",
                _ => "openai",
            }
            .to_owned(),
            key_from,
            default_env,
            key_now: state.source,
            key_env: text("api_key_env"),
            key: String::new(),
            base_url: text("base_url"),
            headers: Pairs::new(&headers),
            models,
            models_url: Some(text("models_url")).filter(|url| !url.is_empty()),
            saving: Saving::Idle,
            probing: None,
            tested: None,
            fetched: Vec::new(),
            asked: Asked::default(),
            target: Some(target),
            base,
        }
    }

    /// Whether the form edits a provider kage registers itself.
    #[must_use]
    pub fn registered(&self) -> bool {
        matches!(self.target, Some(Target::Registered(_)))
    }

    /// What the form edits.
    #[must_use]
    pub fn target(&self) -> Option<&Target> {
        self.target.as_ref()
    }

    /// The provider's id: the edited one, or the one typed.
    fn provider_id(&self) -> String {
        match &self.target {
            Some(Target::Registered(id) | Target::Custom(Some(id)) | Target::Picked(id, _)) => {
                id.clone()
            }
            Some(Target::Custom(None) | Target::Local) | None => self.id.trim().to_owned(),
        }
    }

    /// Follows the store: `true` once the save landed, and takes the
    /// answer of the test in flight.
    pub fn follow(&mut self, store: &Store) -> bool {
        let mut done = false;
        if self.saving.settle(store) {
            done = true;
        }
        if let Some((id, fill)) = self.probing
            && let Some(result) = store.test_result(id).cloned()
        {
            self.probing = None;
            if fill && result.ok {
                self.fill_models(&result.models);
            }
            self.tested = Some(result);
        }
        done
    }

    /// The probe the form describes, with a typed key for this request
    /// only.
    fn probe(&self) -> ProviderProbe {
        let url = self.base_url.trim().to_owned();
        ProviderProbe {
            id: self.provider_id(),
            kind: (!self.registered()).then(|| self.protocol.clone()),
            base_url: (!url.is_empty()).then_some(url),
            models_url: None,
            api_key_env: match self.key_from {
                KeyFrom::None => Some(String::new()),
                KeyFrom::Env => Some(self.key_env.trim().to_owned()).filter(|env| !env.is_empty()),
                KeyFrom::Paste => None,
            },
            api_key: (self.key_from == KeyFrom::Paste)
                .then(|| self.key.trim().to_owned())
                .filter(|key| !key.is_empty()),
            headers: self.headers.values(),
        }
    }

    /// Asks the engine to list the provider's models; `fill` adds the
    /// ones the table lacks. Returns the request id.
    pub fn test(&mut self, fill: bool, store: &mut Store) -> u64 {
        let probe = self.probe();
        let id = store.config_test(probe);
        self.probing = Some((id, fill));
        self.tested = None;
        id
    }

    /// Queues the fetched models the table does not hold yet; the next
    /// render makes them rows.
    fn fill_models(&mut self, fetched: &[ProbeModel]) {
        let held: Vec<String> = self.models.iter().map(|row| row.id.clone()).collect();
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
    pub fn entry(&self) -> Result<(Vec<String>, Option<Value>), String> {
        let id = self.provider_id();
        let url = self.base_url.trim().to_owned();
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
        let env = self.key_env.trim().to_owned();
        put(
            "api_key_env",
            match self.key_from {
                KeyFrom::None => Some(json!("")),
                KeyFrom::Env if !env.is_empty() => Some(json!(env)),
                KeyFrom::Env | KeyFrom::Paste => None,
            },
        );
        let headers = self.headers.values();
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
        let display = self.display.trim().to_owned();
        put(
            "display_name",
            (!display.is_empty()).then(|| json!(display)),
        );
        put("kind", Some(json!(self.protocol.clone())));
        let models = self.model_entries()?;
        if models.is_empty() && self.models_url.is_none() {
            return Err("Add at least one model, or fetch them from /models.".to_owned());
        }
        put(
            "models",
            (!models.is_empty()).then_some(Value::Array(models)),
        );
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
    fn model_entries(&self) -> Result<Vec<Value>, String> {
        let saved: Vec<&Map<String, Value>> = self
            .base
            .get("models")
            .and_then(Value::as_array)
            .map(|models| models.iter().filter_map(Value::as_object).collect())
            .unwrap_or_default();
        let mut out = Vec::new();
        for row in &self.models {
            let id = row.id.trim().to_owned();
            if id.is_empty() {
                continue;
            }
            let mut model = saved
                .iter()
                .find(|model| model.get("id").and_then(Value::as_str) == Some(id.as_str()))
                .map(|model| (*model).clone())
                .unwrap_or_default();
            let name = row.name.trim().to_owned();
            model.insert("id".into(), json!(id));
            if !name.is_empty() {
                model.insert("name".into(), json!(name));
            } else if self.models_url.is_some() {
                // An override without a name keeps the listed one.
                model.remove("name");
            } else {
                model.insert("name".into(), json!(id));
            }
            for (key, text) in [("context", &row.context), ("max_output", &row.max_output)] {
                let text = text.replace(['_', ','], "");
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

    /// Queues the writes the form describes, returning the request id
    /// the store answers under.
    pub fn save(&mut self, store: &mut Store) -> Result<u64, String> {
        let (path, value) = self.entry()?;
        let key = (self.key_from == KeyFrom::Paste)
            .then(|| self.key.trim().to_owned())
            .filter(|key| !key.is_empty());
        let provider = self.provider_id();
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        let id = store.config_set(&path, value);
        if let Some(key) = key {
            store.save_key(&provider, Some(key));
        }
        self.saving = Saving::Sent(vec![id]);
        Ok(id)
    }

    /// Drops the override of a registered provider, or the whole entry
    /// of a custom one.
    pub fn remove(&mut self, store: &mut Store) -> u64 {
        let id = self.provider_id();
        let path: Vec<String> = if self.registered() {
            vec!["providers".into(), id]
        } else {
            vec!["providers".into(), "custom".into(), id]
        };
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        let request = store.config_set(&path, None);
        self.saving = Saving::Sent(vec![request]);
        request
    }

    /// Removes the key saved for the provider from the credential
    /// store.
    pub fn forget_key(&mut self, store: &mut Store) {
        let provider = self.provider_id();
        store.save_key(&provider, None);
        self.key_now = "missing".to_owned();
    }

    /// The page's title.
    #[must_use]
    pub fn title(&self) -> String {
        match &self.target {
            Some(Target::Registered(id)) if self.base.is_empty() && self.key_now == "missing" => {
                format!("Set up {id}")
            }
            Some(Target::Registered(id) | Target::Custom(Some(id))) => format!("Edit {id}"),
            Some(Target::Picked(id, _)) => format!("Import {id}"),
            Some(Target::Custom(None) | Target::Local) | None => "Add provider".to_owned(),
        }
    }

    /// Whether a save already exists to drop.
    #[must_use]
    pub fn saved(&self) -> bool {
        match &self.target {
            Some(Target::Registered(_)) => !self.base.is_empty(),
            Some(Target::Custom(id)) => id.is_some(),
            Some(Target::Picked(..)) => false,
            Some(Target::Local) | None => false,
        }
    }

    /// Draws the form. Returns whether the form asked to close.
    pub fn show(&mut self, ui: &mut egui::Ui, store: &mut Store) -> bool {
        for model in std::mem::take(&mut self.fetched) {
            self.models.push(model_row(&model));
        }
        let palette = Palette::active_of(ui.ctx());
        self.asked = Asked::default();
        let mut edit = Edit::default();
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
                    if self.registered() {
                        ui.label(
                            egui::RichText::new(
                                "A provider kage registers itself. Edits are saved as an override in config.toml; Reset drops the override.",
                            )
                            .size(FS_XS)
                            .color(palette.muted),
                        );
                    } else {
                        self.identity(ui, palette, &mut edit);
                    }
                    self.key_fields(ui, palette, &mut edit);
                    forms::field(
                        ui,
                        "Base URL",
                        !self.registered(),
                        self.registered()
                            .then_some("Leave blank for the provider's own endpoint."),
                        |ui| {
                            let mut url = self.base_url.clone();
                            let response = forms::text_field(ui, &mut url, "", true, palette);
                            if response.changed() {
                                edit.base_url = url;
                                edit.base_url_set = true;
                            }
                        },
                    );
                    forms::field(
                        ui,
                        "Headers",
                        false,
                        Some("Sent on every request. A blank value keeps the saved one."),
                        |ui| {
                            forms::pairs(ui, "provider-headers", &mut self.headers, palette);
                        },
                    );
                    if !self.registered() {
                        self.models_table(ui, palette, &mut edit);
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

    /// The identity fields a new custom provider carries.
    fn identity(&mut self, ui: &mut egui::Ui, palette: &Palette, edit: &mut Edit) {
        let new = matches!(
            self.target,
            Some(Target::Custom(None) | Target::Local) | None
        );
        if new {
            forms::field(
                ui,
                "Name",
                true,
                Some("The id in config.toml and in model names (id/model)."),
                |ui| {
                    let mut id = self.id.clone();
                    let response = forms::text_field(ui, &mut id, "my-provider", true, palette);
                    if response.changed() {
                        edit.id = id;
                        edit.id_set = true;
                    }
                },
            );
        }
        forms::field(ui, "Display name", false, None, |ui| {
            let mut display = self.display.clone();
            let response = forms::text_field(
                ui,
                &mut display,
                "shown in the model picker",
                false,
                palette,
            );
            if response.changed() {
                edit.display = display;
                edit.display_set = true;
            }
        });
        let mut picked = None;
        forms::field(ui, "API protocol", true, None, |ui| {
            picked = forms::segments(ui, "provider-protocol", &PROTOCOLS, &self.protocol, palette);
        });
        if let Some(value) = picked {
            edit.protocol = Some(value.to_owned());
        }
    }

    /// The key source fields.
    fn key_fields(&mut self, ui: &mut egui::Ui, palette: &Palette, edit: &mut Edit) {
        const CHOICES: &[(&str, &str)] = &[
            ("env", "Environment variable"),
            ("paste", "Paste a key"),
            ("none", "None"),
        ];
        let choices = if self.registered() {
            &CHOICES[..2]
        } else {
            CHOICES
        };
        let current = match self.key_from {
            KeyFrom::Env => "env",
            KeyFrom::Paste => "paste",
            KeyFrom::None => "none",
        };
        let mut picked = None;
        forms::field(ui, "Key source", false, None, |ui| {
            picked = forms::segments(ui, "provider-key", choices, current, palette);
        });
        edit.key_from = picked.map(|value| match value {
            "paste" => KeyFrom::Paste,
            "none" => KeyFrom::None,
            _ => KeyFrom::Env,
        });
        match self.key_from {
            KeyFrom::Env => {
                let found = match self.key_now.as_str() {
                    "env" => "set in the engine's environment",
                    "auth" => "a saved key is used while the variable is unset",
                    _ => "not set in the engine's environment",
                };
                forms::field(ui, "API key variable", false, None, |ui| {
                    let mut env = self.key_env.clone();
                    let response = forms::text_field(ui, &mut env, "", true, palette);
                    if response.changed() {
                        edit.key_env = env;
                        edit.key_env_set = true;
                    }
                    ui.label(
                        egui::RichText::new(format!("Blank reads {}; {found}.", self.default_env))
                            .size(FS_XS)
                            .color(palette.faint),
                    );
                });
            }
            KeyFrom::Paste => {
                let saved = self.key_now == "auth";
                forms::field(
                    ui,
                    "API key",
                    false,
                    Some(
                        "Saved to the engine's auth.json with 0600 permissions, never to config.toml.",
                    ),
                    |ui| {
                        // The forget button takes the right edge first
                        // (right-to-left draws its first widget
                        // rightmost), so the key field fills what is
                        // left instead of pushing the button outside
                        // the column.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if saved {
                                let forget =
                                    forms::button(ui, "Forget saved key", Tone::Plain, palette);
                                if forget.clicked() {
                                    edit.forget = true;
                                }
                            }
                            let mut key = self.key.clone();
                            let hint = if saved {
                                "saved; type to replace"
                            } else {
                                "sk-..."
                            };
                            let response = forms::secret_field(ui, &mut key, hint, palette);
                            if response.changed() {
                                edit.key = key;
                                edit.key_set = true;
                            }
                        });
                    },
                );
            }
            KeyFrom::None => {
                ui.label(
                    egui::RichText::new("No key: for local servers that do not check auth.")
                        .size(FS_XS)
                        .color(palette.muted),
                );
            }
        }
    }

    /// The models table.
    fn models_table(&mut self, ui: &mut egui::Ui, palette: &Palette, edit: &mut Edit) {
        let listed = self.models_url.as_ref().map(|url| {
            format!(
                "Listed at {url} and kept current on their own. A row here overrides the fields it sets."
            )
        });
        forms::field(
            ui,
            "Models",
            listed.is_none(),
            Some(listed.as_deref().unwrap_or(
                "Context windows come from the endpoint or the model catalog when known.",
            )),
            |ui| {
                let mut drop: Option<usize> = None;
                for (ix, row) in self.models.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        // The id cell fills what the fixed cells and the
                        // remove button leave, so the row never grows
                        // past its column.
                        let id_w = (ui.available_width()
                            - 96.0
                            - 96.0
                            - 160.0
                            - forms::ROW_REMOVE_W
                            - 4.0 * 6.0)
                            .max(120.0);
                        let mut cells = [
                            row.id.clone(),
                            row.context.clone(),
                            row.max_output.clone(),
                            row.name.clone(),
                        ];
                        let id_response =
                            forms::text_field_w(ui, &mut cells[0], "model-id", true, id_w, palette);
                        let context_response =
                            forms::text_field_w(ui, &mut cells[1], "tokens", false, 96.0, palette);
                        let output_response = forms::text_field_w(
                            ui,
                            &mut cells[2],
                            "optional",
                            false,
                            96.0,
                            palette,
                        );
                        let name_response = forms::text_field_w(
                            ui,
                            &mut cells[3],
                            "optional",
                            false,
                            160.0,
                            palette,
                        );
                        if id_response.changed() {
                            edit.models.push((ix, 0, cells[0].clone()));
                        }
                        if context_response.changed() {
                            edit.models.push((ix, 1, cells[1].clone()));
                        }
                        if output_response.changed() {
                            edit.models.push((ix, 2, cells[2].clone()));
                        }
                        if name_response.changed() {
                            edit.models.push((ix, 3, cells[3].clone()));
                        }
                        let remove = forms::icon_button(
                            ui,
                            &format!("model-rm-{ix}"),
                            crate::icons::Icon::Trash,
                            palette,
                        );
                        if remove.clicked() {
                            drop = Some(ix);
                        }
                    });
                }
                if let Some(ix) = drop {
                    edit.drop_model = Some(ix);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let add = forms::add_button(ui, "Add model", crate::icons::Icon::Plus, palette);
                    if add.clicked() {
                        edit.add_model = true;
                    }
                    if self.models_url.is_some() {
                        return;
                    }
                    let fetching = matches!(self.probing, Some((_, true)));
                    let label = if fetching {
                        "Fetching\u{2026}"
                    } else {
                        "Fetch from /models"
                    };
                    let fetch = forms::button(ui, label, Tone::Plain, palette);
                    if fetch.clicked() {
                        edit.fetch = true;
                    }
                });
            },
        );
    }

    /// The line under the fields: the test's answer, or that one runs.
    fn test_line(&self, ui: &mut egui::Ui, palette: &Palette) {
        if matches!(self.probing, Some((_, false))) {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Testing\u{2026}")
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
            let test = forms::button(ui, "Test connection", Tone::Plain, palette);
            if test.clicked() {
                self.asked.test = true;
            }
            if self.saved() {
                let remove = forms::button(
                    ui,
                    if self.registered() {
                        "Reset"
                    } else {
                        "Delete provider"
                    },
                    Tone::Danger,
                    palette,
                );
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
        self.asked = Asked::default();
        if asked.forget {
            self.forget_key(store);
        }
        if asked.fetch {
            self.test(true, store);
        }
        if asked.test {
            self.test(false, store);
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

/// What the fields asked to change this frame, applied once the draw
/// is over so no closure borrows the form while a widget holds it.
#[derive(Debug, Default)]
struct Edit {
    /// The typed provider id.
    id: String,
    /// Whether the id field changed.
    id_set: bool,
    /// The typed display name.
    display: String,
    /// Whether the display field changed.
    display_set: bool,
    /// The chosen protocol.
    protocol: Option<String>,
    /// The chosen key source.
    key_from: Option<KeyFrom>,
    /// The typed key variable.
    key_env: String,
    /// Whether the key variable field changed.
    key_env_set: bool,
    /// The typed key.
    key: String,
    /// Whether the key field changed.
    key_set: bool,
    /// The typed base URL.
    base_url: String,
    /// Whether the base URL field changed.
    base_url_set: bool,
    /// Whether to forget the saved key.
    forget: bool,
    /// A models table cell that changed: row, column, text.
    models: Vec<(usize, usize, String)>,
    /// The models table row to drop.
    drop_model: Option<usize>,
    /// Whether to add a blank models row.
    add_model: bool,
    /// Whether to fetch the models.
    fetch: bool,
}

impl Edit {
    /// Applies the edits to `form`.
    fn apply(self, form: &mut ProviderForm) {
        if self.id_set {
            form.id = self.id;
        }
        if self.display_set {
            form.display = self.display;
        }
        if let Some(protocol) = self.protocol {
            form.protocol = protocol;
        }
        if let Some(key_from) = self.key_from {
            form.key_from = key_from;
        }
        if self.key_env_set {
            form.key_env = self.key_env;
        }
        if self.key_set {
            form.key = self.key;
        }
        if self.base_url_set {
            form.base_url = self.base_url;
        }
        for (row, column, text) in self.models {
            if let Some(row) = form.models.get_mut(row) {
                match column {
                    0 => row.id = text,
                    1 => row.context = text,
                    2 => row.max_output = text,
                    _ => row.name = text,
                }
            }
        }
        if let Some(ix) = self.drop_model
            && ix < form.models.len()
        {
            form.models.remove(ix);
        }
        if self.add_model {
            form.models.push(ModelRow::default());
        }
        if self.forget {
            form.asked.forget = true;
        }
        if self.fetch {
            form.asked.fetch = true;
        }
    }
}

/// One models table row holding `model`'s fields.
fn model_row(model: &Value) -> ModelRow {
    let text = |key: &str| match &model[key] {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    };
    ModelRow {
        id: text("id"),
        context: text("context"),
        max_output: text("max_output"),
        name: text("name"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{KeyFrom, ProviderForm, Target};

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

    #[test]
    fn an_edit_keeps_what_the_form_does_not_show() {
        let form = ProviderForm::new(Target::Custom(Some("lab".into())), &snapshot());
        let (path, value) = form.entry().unwrap();
        assert_eq!(path, ["providers", "custom", "lab"]);
        let value = value.unwrap();
        assert_eq!(value["kind"], "anthropic");
        assert_eq!(value["api_key_env"], "");
        assert_eq!(value["tool_use"], false);
        assert_eq!(value["headers"]["X-Team"], "<redacted>");
        assert_eq!(value["models"][0]["cost"]["output"], 2.0);
        assert_eq!(value["models"][0]["context"], 1000);
    }

    #[test]
    fn an_imported_google_kind_saves_the_gemini_protocol() {
        let form = ProviderForm::new(
            Target::Picked(
                "goo".into(),
                json!({
                    "kind": "google",
                    "base_url": "https://generativelanguage.googleapis.com",
                    "models": [{"id": "m1", "name": "One"}],
                }),
            ),
            &snapshot(),
        );
        let (path, value) = form.entry().unwrap();
        assert_eq!(path, ["providers", "custom", "goo"]);
        assert_eq!(value.unwrap()["kind"], "gemini");
    }

    #[test]
    fn an_override_left_empty_is_dropped() {
        let mut config = snapshot();
        config["providers"]["deepseek"] = json!({});
        let form = ProviderForm::new(Target::Registered("deepseek".into()), &config);
        let (path, value) = form.entry().unwrap();
        assert_eq!(path, ["providers", "deepseek"]);
        assert_eq!(value, None);
    }

    #[test]
    fn a_new_custom_provider_needs_a_name_url_and_model() {
        let form = ProviderForm::new(Target::Local, &snapshot());
        let err = form.entry().unwrap_err();
        assert!(err.contains("at least one model"), "{err}");
    }

    #[test]
    fn a_provider_listing_its_models_at_a_url_saves_without_rows() {
        let mut config = snapshot();
        let lab = &mut config["providers"]["custom"]["lab"];
        lab["models_url"] = json!("http://lab:8080/models.json");
        lab.as_object_mut().unwrap().remove("models");
        let form = ProviderForm::new(Target::Custom(Some("lab".into())), &config);
        let (_, value) = form.entry().unwrap();
        let value = value.unwrap();
        assert_eq!(value["models_url"], "http://lab:8080/models.json");
        assert!(value.get("models").is_none(), "the list stays in the cache");

        config["providers"]["custom"]["lab"]["models"] = json!([{"id": "m1", "context": 2000}]);
        let form = ProviderForm::new(Target::Custom(Some("lab".into())), &config);
        let (_, value) = form.entry().unwrap();
        let row = &value.unwrap()["models"][0];
        assert_eq!(row["context"], 2000);
        assert!(row.get("name").is_none(), "the listed name stays");
    }

    #[test]
    fn a_base_url_must_be_http() {
        let mut form = ProviderForm::new(Target::Local, &snapshot());
        form.base_url = "ftp://example.com".to_owned();
        let err = form.entry().unwrap_err();
        assert_eq!(err, "The base URL must start with http:// or https://.");
    }

    #[test]
    fn a_custom_provider_needs_a_name_of_lowercase_letters() {
        let mut form = ProviderForm::new(Target::Local, &snapshot());
        form.id = "My_Provider".to_owned();
        form.base_url = "https://api.example.com/v1".to_owned();
        let err = form.entry().unwrap_err();
        assert_eq!(
            err,
            "Name the provider with lowercase letters, digits or dashes."
        );
    }

    #[test]
    fn a_registered_provider_keeps_its_own_name() {
        let form = ProviderForm::new(Target::Registered("deepseek".into()), &snapshot());
        assert!(form.registered());
        assert_eq!(form.title(), "Edit deepseek");
        assert!(form.saved(), "the snapshot holds an entry for it");
    }

    #[test]
    fn the_local_target_reads_no_key() {
        let form = ProviderForm::new(Target::Local, &snapshot());
        assert_eq!(form.key_from, KeyFrom::None);
        assert_eq!(form.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn a_typed_key_replaces_the_variable_in_the_probe() {
        let mut form = ProviderForm::new(Target::Local, &snapshot());
        form.key_from = KeyFrom::Paste;
        form.key = "sk-test".to_owned();
        let probe = form.probe();
        assert_eq!(probe.api_key.as_deref(), Some("sk-test"));
        assert_eq!(probe.api_key_env, None);
    }
}
