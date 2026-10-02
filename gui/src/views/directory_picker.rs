//! The provider directory picker: reads models.dev, or another
//! directory in its `api.json` shape, through the engine, lets the user
//! pick a provider and its models, and hands the provider form an entry
//! holding them, prices and limits included.

use std::collections::BTreeSet;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Div, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};
use kage_client::wire::{DirectoryModel, DirectoryProvider};
use serde_json::{Map, Value, json};

use crate::store::Store;
use crate::theme::{FONT_MONO, FS_SM, FS_XS, Palette, R_FULL, R_MD};
use crate::views::config_forms::{field, form_head, text_field};
use crate::views::kit::{BtnTone, btn_sm};
use crate::views::mcp_form::FormDone;

/// The picker chose a provider: its id and the entry to start from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    /// The provider id.
    pub id: String,
    /// The `[providers.custom.<id>]` table with the chosen models.
    pub entry: Value,
}

/// The directory picker.
pub struct DirectoryPicker {
    store: Entity<Store>,
    /// Whether the directory is one the user names, not models.dev.
    named: bool,
    url: Entity<InputState>,
    key: Entity<InputState>,
    search: Entity<InputState>,
    /// The read in flight.
    reading: Option<u64>,
    providers: Vec<DirectoryProvider>,
    error: Option<String>,
    /// The provider being picked from, by index.
    chosen: Option<usize>,
    /// The models picked, by id.
    picked: BTreeSet<String>,
}

impl EventEmitter<FormDone> for DirectoryPicker {}
impl EventEmitter<Picked> for DirectoryPicker {}

impl DirectoryPicker {
    /// A picker for models.dev, read at once, or for a directory the
    /// user names when `named`.
    pub fn new(
        store: Entity<Store>,
        named: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&store, |this, store, cx| {
            if let Some(id) = this.reading
                && let Some(read) = store.read(cx).directory(id)
            {
                this.reading = None;
                match read {
                    Ok(providers) => {
                        this.providers.clone_from(providers);
                        this.error = None;
                    }
                    Err(why) => this.error = Some(why.clone()),
                }
            }
            cx.notify();
        })
        .detach();
        let search = text_field(window, cx, "Search providers and models", "");
        cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        })
        .detach();
        let mut picker = Self {
            url: text_field(window, cx, "https://example.com/api.json", ""),
            key: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("optional")
                    .masked(true)
            }),
            search,
            reading: None,
            providers: Vec::new(),
            error: None,
            chosen: None,
            picked: BTreeSet::new(),
            named,
            store,
        };
        if !named {
            picker.load(cx);
        }
        picker
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let url = self.url.read(cx).value().trim().to_owned();
        let key = self.key.read(cx).value().trim().to_owned();
        if self.named && url.is_empty() {
            self.error = Some("Name the directory's URL.".to_owned());
            cx.notify();
            return;
        }
        let named = self.named;
        let id = self.store.update(cx, |store, cx| {
            cx.notify();
            store.ask_directory(
                named.then_some(url.as_str()),
                (!key.is_empty()).then_some(key.as_str()),
            )
        });
        self.reading = Some(id);
        self.error = None;
        self.chosen = None;
        cx.notify();
    }

    fn query(&self, cx: &App) -> String {
        self.search.read(cx).value().trim().to_lowercase()
    }

    fn choose(&mut self, ix: Option<usize>, cx: &mut Context<Self>) {
        self.chosen = ix;
        self.picked = ix
            .and_then(|ix| self.providers.get(ix))
            .map(|provider| provider.models.iter().map(|m| m.id.clone()).collect())
            .unwrap_or_default();
        cx.notify();
    }

    fn import(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = self.chosen.and_then(|ix| self.providers.get(ix)) else {
            return;
        };
        cx.emit(picked(provider, &self.picked));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(FormDone);
    }
}

/// The entry provider `provider` makes with the models in `picked`.
fn picked(provider: &DirectoryProvider, picked: &BTreeSet<String>) -> Picked {
    let id: String = provider
        .id
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let models: Vec<Value> = provider
        .models
        .iter()
        .filter(|model| picked.contains(&model.id))
        .map(model_entry)
        .collect();
    let mut entry = Map::new();
    entry.insert("display_name".into(), json!(provider.name));
    entry.insert(
        "kind".into(),
        json!(provider.kind.as_deref().unwrap_or("openai")),
    );
    if let Some(api) = &provider.api {
        entry.insert("base_url".into(), json!(api));
    }
    if let Some(env) = provider.env.first() {
        entry.insert("api_key_env".into(), json!(env));
    }
    entry.insert("models".into(), Value::Array(models));
    Picked {
        id,
        entry: Value::Object(entry),
    }
}

/// One `[[providers.custom.<id>.models]]` entry for `model`.
fn model_entry(model: &DirectoryModel) -> Value {
    let mut entry = Map::new();
    entry.insert("id".into(), json!(model.id));
    entry.insert("name".into(), json!(model.name));
    if let Some(context) = model.context {
        entry.insert("context".into(), json!(context));
    }
    if let Some(output) = model.output {
        entry.insert("max_output".into(), json!(output));
    }
    if model.reasoning {
        entry.insert("reasoning".into(), json!(true));
    }
    if !model.input.is_empty() {
        entry.insert("input".into(), json!(model.input));
    }
    if let Some(cost) = &model.cost {
        let mut prices = Map::new();
        prices.insert("input".into(), json!(cost.input));
        prices.insert("output".into(), json!(cost.output));
        if let Some(read) = cost.cache_read {
            prices.insert("cache_read".into(), json!(read));
        }
        if let Some(write) = cost.cache_write {
            prices.insert("cache_write".into(), json!(write));
        }
        entry.insert("cost".into(), Value::Object(prices));
    }
    Value::Object(entry)
}

impl Render for DirectoryPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = Palette::active(cx);
        let this = cx.entity();
        let title = if self.named {
            "Import from api.json"
        } else {
            "Import from models.dev"
        };
        let mut page = v_flex()
            .gap(px(14.))
            .child(form_head(title, &this, Self::cancel, pal));
        if self.named {
            let load = this.clone();
            page = page.child(
                h_flex()
                    .gap(px(10.))
                    .items_end()
                    .child(
                        div().flex_1().child(field(
                            "Registry URL",
                            true,
                            None,
                            div()
                                .font_family(FONT_MONO)
                                .child(Input::new(&self.url).small()),
                            pal,
                        )),
                    )
                    .child(div().w(px(200.)).child(field(
                        "API key",
                        false,
                        None,
                        Input::new(&self.key).small(),
                        pal,
                    )))
                    .child(
                        btn_sm("directory-load", BtnTone::Primary, pal)
                            .on_click(move |_, _, cx| load.update(cx, |picker, cx| picker.load(cx)))
                            .child("Load"),
                    ),
            );
        }
        if self.reading.is_some() {
            page = page.child(note("Reading the directory\u{2026}", pal.muted));
        }
        if let Some(why) = &self.error {
            page = page.child(note(why, pal.danger));
        }
        if self.providers.is_empty() {
            return page;
        }
        page = page.child(Input::new(&self.search).small());
        match self.chosen {
            Some(ix) => page.child(self.models(ix, &this, cx, pal)),
            None => page.child(self.provider_list(&this, cx, pal)),
        }
    }
}

impl DirectoryPicker {
    fn provider_list(&self, this: &Entity<Self>, cx: &App, pal: &'static Palette) -> Div {
        let query = self.query(cx);
        let mut list = v_flex().gap(px(4.));
        let hover = pal.fill_hover;
        for (ix, provider) in self.providers.iter().enumerate() {
            let matches = query.is_empty()
                || provider.name.to_lowercase().contains(&query)
                || provider.id.contains(&query)
                || provider
                    .models
                    .iter()
                    .any(|m| m.id.to_lowercase().contains(&query));
            if !matches {
                continue;
            }
            let choose = this.clone();
            let detail = format!(
                "{} \u{b7} {} models{}",
                provider.api.as_deref().unwrap_or("no endpoint listed"),
                provider.models.len(),
                if provider.kind.is_none() {
                    " \u{b7} protocol unknown, assumed OpenAI"
                } else {
                    ""
                },
            );
            list = list.child(
                v_flex()
                    .id(SharedString::from(format!("directory-{}", provider.id)))
                    .px(px(12.))
                    .py(px(8.))
                    .gap(px(2.))
                    .rounded(px(R_MD))
                    .cursor_pointer()
                    .hover(move |row| row.bg(hover))
                    .child(
                        div()
                            .text_size(px(FS_SM))
                            .text_color(pal.ink)
                            .child(provider.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(FS_XS))
                            .text_color(pal.muted)
                            .child(detail),
                    )
                    .on_click(move |_, _, cx| {
                        choose.update(cx, |picker, cx| picker.choose(Some(ix), cx));
                    }),
            );
        }
        list
    }

    fn models(&self, ix: usize, this: &Entity<Self>, cx: &App, pal: &'static Palette) -> Div {
        let Some(provider) = self.providers.get(ix) else {
            return div();
        };
        let query = self.query(cx);
        let (back, all, none, import) = (this.clone(), this.clone(), this.clone(), this.clone());
        let every: BTreeSet<String> = provider.models.iter().map(|m| m.id.clone()).collect();
        let mut grid = h_flex().gap(px(6.)).flex_wrap();
        for model in &provider.models {
            if !query.is_empty()
                && !model.id.to_lowercase().contains(&query)
                && !model.name.to_lowercase().contains(&query)
            {
                continue;
            }
            let on = self.picked.contains(&model.id);
            let flip = this.clone();
            let id = model.id.clone();
            let context = model
                .context
                .map(|context| format!(" \u{b7} {}k", context / 1000))
                .unwrap_or_default();
            grid = grid.child(
                div()
                    .id(SharedString::from(format!("directory-model-{}", model.id)))
                    .px(px(8.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .rounded(px(R_FULL))
                    .border_1()
                    .border_color(if on { pal.accent } else { pal.line })
                    .bg(if on { pal.accent_soft } else { pal.surface })
                    .text_size(px(FS_XS))
                    .text_color(if on { pal.ink_strong } else { pal.muted })
                    .cursor_pointer()
                    .child(format!("{}{context}", model.name))
                    .on_click(move |_, _, cx| {
                        flip.update(cx, |picker, cx| {
                            if !picker.picked.remove(&id) {
                                picker.picked.insert(id.clone());
                            }
                            cx.notify();
                        });
                    }),
            );
        }
        let count = self.picked.len();
        v_flex()
            .gap(px(10.))
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        btn_sm("directory-back", BtnTone::Plain, pal)
                            .on_click(move |_, _, cx| {
                                back.update(cx, |picker, cx| picker.choose(None, cx))
                            })
                            .child("All providers"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(FS_SM))
                            .text_color(pal.ink_strong)
                            .child(provider.name.clone()),
                    )
                    .child(
                        btn_sm("directory-all", BtnTone::Plain, pal)
                            .on_click(move |_, _, cx| {
                                let every = every.clone();
                                all.update(cx, |picker, cx| {
                                    picker.picked = every;
                                    cx.notify();
                                });
                            })
                            .child("Select all"),
                    )
                    .child(
                        btn_sm("directory-none", BtnTone::Plain, pal)
                            .on_click(move |_, _, cx| {
                                none.update(cx, |picker, cx| {
                                    picker.picked.clear();
                                    cx.notify();
                                });
                            })
                            .child("Select none"),
                    ),
            )
            .child(grid)
            .child(
                h_flex().child(div().flex_1()).child(
                    btn_sm("directory-import", BtnTone::Primary, pal)
                        .when(count == 0, |button| button.opacity(0.5))
                        .on_click(move |_, _, cx| {
                            import.update(cx, |picker, cx| {
                                if !picker.picked.is_empty() {
                                    picker.import(cx);
                                }
                            });
                        })
                        .child(if count == 1 {
                            "Import 1 model".to_owned()
                        } else {
                            format!("Import {count} models")
                        }),
                ),
            )
    }
}

fn note(text: &str, color: gpui_kit::Hsla) -> Div {
    div()
        .text_size(px(FS_XS))
        .text_color(color)
        .child(text.to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use kage_client::wire::{DirectoryCost, DirectoryModel, DirectoryProvider};

    use super::picked;

    #[test]
    fn an_import_keeps_the_picked_models_with_their_prices() {
        let provider = DirectoryProvider {
            id: "Moon.AI".into(),
            name: "Moon AI".into(),
            api: Some("https://moon/v1".into()),
            env: vec!["MOON_KEY".into()],
            kind: None,
            models: vec![
                DirectoryModel {
                    id: "m1".into(),
                    name: "One".into(),
                    context: Some(128_000),
                    output: Some(8_000),
                    reasoning: true,
                    input: vec!["text".into()],
                    cost: Some(DirectoryCost {
                        input: 1.0,
                        output: 2.0,
                        cache_read: Some(0.1),
                        cache_write: None,
                    }),
                },
                DirectoryModel {
                    id: "m2".into(),
                    name: "Two".into(),
                    ..DirectoryModel::default()
                },
            ],
        };
        let chosen: BTreeSet<String> = ["m1".to_owned()].into();
        let picked = picked(&provider, &chosen);
        assert_eq!(picked.id, "moon-ai");
        let entry = &picked.entry;
        assert_eq!(entry["kind"], "openai");
        assert_eq!(entry["api_key_env"], "MOON_KEY");
        assert_eq!(entry["models"].as_array().unwrap().len(), 1);
        let model = &entry["models"][0];
        assert_eq!(model["max_output"], 8_000);
        assert_eq!(model["reasoning"], true);
        assert_eq!(model["cost"]["cache_read"], 0.1);
        assert!(model["cost"].get("cache_write").is_none());
    }
}
