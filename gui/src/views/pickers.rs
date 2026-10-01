//! The composer's model and permission pickers, as the design's
//! popovers draw them. The composer hosts each above its pill.
//!
//! The model picker searches every model the engine can run, starred
//! ones first and the rest under their provider, each with its context
//! window, prices and a star, over a thinking level row that dims the
//! levels the chosen model does not take. The permission picker lists
//! the modes with what each does to every kind of tool call. Both keep
//! the keys on their search field: the arrows move the highlight,
//! Enter picks and Esc closes.

use gpui_kit::ScrollHandle;
use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::input::{Escape as InputEscape, Input, InputEvent, InputState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px, relative,
};
use kage_client::wire::{ModelEntry, SessionConfigSelectOption};

use crate::store::{Store, StoreHandle as _};
use crate::theme::{FONT_MONO, FS_2XS, FS_SM, FS_XS, Palette, R_MD};
use crate::views::deferred::Deferred;
use crate::views::kit::{BtnTone, btn_sm};

gpui_kit::actions!(kage_desktop, [PickerUp, PickerDown, PickerRun, PickerClose]);

/// The plan mode's id in the `mode` option, which its own chip toggles.
const PLAN_MODE: &str = "plan";

/// What a picker asks of the composer that hosts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerEvent {
    /// The pick is made or abandoned: close the picker.
    Close,
}

/// One model row: the facts the row shows.
#[derive(Debug, Clone)]
struct ModelRow {
    id: String,
    name: String,
    provider: String,
    model: String,
    context: Option<u64>,
    cost: Option<(f64, f64)>,
    images: bool,
}

impl ModelRow {
    fn of(provider: &str, entry: &ModelEntry) -> Self {
        let model = entry
            .id
            .split_once('/')
            .map_or(entry.id.as_str(), |(_, model)| model)
            .to_owned();
        Self {
            id: entry.id.clone(),
            name: entry.name.clone(),
            provider: provider.to_owned(),
            model,
            context: entry.context,
            cost: entry.input_cost.zip(entry.output_cost),
            images: entry.images,
        }
    }

    fn matches(&self, needle: &str, provider_name: &str) -> bool {
        needle.is_empty()
            || format!("{} {} {provider_name}", self.id, self.name)
                .to_lowercase()
                .contains(needle)
    }
}

/// A labeled run of rows.
struct Section {
    label: String,
    rows: Vec<ModelRow>,
}

/// The model and thinking picker.
pub struct ModelPicker {
    store: Entity<Store>,
    query: Entity<InputState>,
    /// Clears the query once the field has been laid out; see
    /// [`crate::views::deferred`].
    query_mirror: Deferred,
    /// The highlighted row, by its place in the flattened sections.
    highlight: usize,
    scroll: ScrollHandle,
    /// Whether the current model still waits to be highlighted and
    /// shown, until the catalog it lives in has arrived.
    reveal: bool,
    /// Frames left to keep the highlighted row scrolled into view. A
    /// scroll lands on the children the last frame laid out, so rows
    /// that just changed need one more frame.
    scroll_frames: u8,
}

impl EventEmitter<PickerEvent> for ModelPicker {}

impl Focusable for ModelPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }
}

impl ModelPicker {
    /// A picker over `store`.
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search models..."));
        cx.subscribe_in(&query, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::Change = event {
                this.highlight = 0;
                cx.notify();
            }
        })
        .detach();
        Self {
            store,
            query,
            query_mirror: Deferred::new(),
            highlight: 0,
            scroll: ScrollHandle::new(),
            reveal: false,
            scroll_frames: 0,
        }
    }

    fn close(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PickerEvent::Close);
    }

    /// Readies the picker as it opens: an empty query, a fresh
    /// catalog asked for, and the current model highlighted and shown
    /// once the catalog is in.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query.clone();
        self.query_mirror.set(String::new(), |text| {
            query.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        self.store.act(cx, Store::ask_models);
        self.reveal = true;
        cx.notify();
    }

    /// Highlights the current model and scrolls to it.
    fn reveal_current(&mut self, cx: &App) {
        let current = self.current(cx);
        self.highlight = self
            .flat(cx)
            .iter()
            .position(|row| Some(&row.id) == current.as_ref())
            .unwrap_or(0);
        self.scroll_frames = 2;
    }

    /// Scrolls the list so the highlighted row shows. The list's
    /// children are the section labels and the rows, in order.
    fn scroll_to_highlight(&self, cx: &App) {
        let mut child = 0;
        let mut row = 0;
        for section in self.sections(cx) {
            child += 1;
            if self.highlight < row + section.rows.len() {
                self.scroll.scroll_to_item(child + self.highlight - row);
                return;
            }
            row += section.rows.len();
            child += section.rows.len();
        }
    }

    fn current(&self, cx: &App) -> Option<String> {
        self.store
            .read(cx)
            .composer_option("model")
            .map(|option| option.current_value)
    }

    /// The rows for the query: the starred models while it is empty,
    /// then each provider's matching models. Before the catalog
    /// arrives, the model option's own values stand in.
    fn sections(&self, cx: &App) -> Vec<Section> {
        let store = self.store.read(cx);
        let needle = self.query.read(cx).value().trim().to_lowercase();
        let mut groups: Vec<(String, Vec<ModelRow>)> = match store.models() {
            Some(providers) => providers
                .iter()
                .map(|provider| {
                    let rows = provider
                        .models
                        .iter()
                        .map(|entry| ModelRow::of(&provider.id, entry))
                        .collect();
                    (provider.name.clone(), rows)
                })
                .collect(),
            None => fallback_groups(store.composer_option("model").map(|o| o.options)),
        };
        let mut sections = Vec::new();
        if needle.is_empty() {
            let starred = &store.prefs().starred_models;
            let rows: Vec<ModelRow> = groups
                .iter()
                .flat_map(|(_, rows)| rows)
                .filter(|row| starred.contains(&row.id))
                .cloned()
                .collect();
            if !rows.is_empty() {
                sections.push(Section {
                    label: "Starred".to_owned(),
                    rows,
                });
            }
        }
        for (name, rows) in &mut groups {
            rows.retain(|row| row.matches(&needle, name));
            if !rows.is_empty() {
                sections.push(Section {
                    label: name.clone(),
                    rows: std::mem::take(rows),
                });
            }
        }
        sections
    }

    fn flat(&self, cx: &App) -> Vec<ModelRow> {
        self.sections(cx)
            .into_iter()
            .flat_map(|section| section.rows)
            .collect()
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let total = self.flat(cx).len();
        if total == 0 {
            return;
        }
        self.highlight = self.highlight.saturating_add_signed(delta).min(total - 1);
        self.scroll_frames = 1;
        cx.notify();
    }

    fn choose(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let id = id.to_owned();
        self.store.act(cx, |store| {
            store.set_option("model", &id);
        });
        self.close(window, cx);
    }

    fn model_row(&self, row: &ModelRow, index: usize, cx: &Context<Self>) -> AnyElement {
        let pal = Palette::active(cx);
        let store = self.store.read(cx);
        let current = self.current(cx).as_deref() == Some(row.id.as_str());
        let starred = store.prefs().starred_models.contains(&row.id);
        let highlighted = index == self.highlight;
        let selected = pal.selected;
        let pick = row.id.clone();
        let star = row.id.clone();
        let store_handle = self.store.clone();
        let mut detail = row.model.clone();
        if row.images {
            detail.push_str(" \u{b7} reads images");
        }
        h_flex()
            .id(SharedString::from(format!("model-row-{}", row.id)))
            .w_full()
            .px(px(9.))
            .py(px(7.))
            .gap(px(10.))
            .items_center()
            .rounded(px(R_MD))
            .cursor_pointer()
            .when(highlighted, |el| el.bg(selected))
            .hover(move |el| el.bg(selected))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.choose(&pick, window, cx);
            }))
            .child(provider_icon(&row.provider, pal))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .text_size(px(FS_SM))
                            .text_color(pal.ink)
                            .child(SharedString::from(row.name.clone()))
                            .when(current, |el| {
                                el.child(div().text_color(pal.accent).child("\u{2713}"))
                            }),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(FS_2XS))
                            .text_color(pal.faint)
                            .child(SharedString::from(detail)),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(6.))
                    .items_center()
                    .children(row.context.map(|context| chip(&window_size(context), pal)))
                    .children(row.cost.map(|(input, output)| {
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(FS_2XS))
                            .text_color(pal.faint)
                            .child(SharedString::from(format!("${input}/{output}")))
                    }))
                    .child(
                        div()
                            .id(SharedString::from(format!("model-star-{}", row.id)))
                            .p(px(2.))
                            .rounded(px(4.))
                            .text_color(if starred { pal.warn } else { pal.ghost })
                            .hover(move |el| el.bg(pal.fill_hover))
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                store_handle.act(cx, |store| store.toggle_star(&star));
                            })
                            .child(Icon::new(IconName::Star).with_size(px(12.))),
                    ),
            )
            .into_any_element()
    }

    /// The thinking level row: one segment per level the thinking
    /// option offers, the ones the current model does not take dimmed.
    fn thinking(&self, cx: &Context<Self>) -> Option<Div> {
        let pal = Palette::active(cx);
        let store = self.store.read(cx);
        let option = store.composer_option("thinking")?;
        let current = self.current(cx);
        let takes: Option<Vec<String>> = current.as_ref().and_then(|id| {
            store
                .models()?
                .iter()
                .flat_map(|provider| &provider.models)
                .find(|entry| &entry.id == id)
                .map(|entry| entry.thinking.clone())
        });
        let mut segments = h_flex()
            .w_full()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(R_MD))
            .bg(pal.fill);
        for value in &option.options {
            let on = value.value == option.current_value;
            let enabled = value.value == "default"
                || takes.as_ref().is_none_or(|levels| {
                    levels.contains(&value.value) || (levels.is_empty() && value.value == "off")
                });
            let store = self.store.clone();
            let level = value.value.clone();
            segments = segments.child(
                div()
                    .id(SharedString::from(format!("thinking-{}", value.value)))
                    .flex_1()
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .text_size(px(FS_XS))
                    .text_color(pal.muted)
                    .when(on, |el| el.bg(pal.raised).text_color(pal.ink_strong))
                    .when(!enabled, |el| el.opacity(0.35))
                    .when(enabled && !on, |el| {
                        el.cursor_pointer()
                            .hover(move |el| el.text_color(pal.ink))
                            .on_click(move |_, _, cx| {
                                store.act(cx, |store| {
                                    store.set_option("thinking", &level);
                                });
                            })
                    })
                    .child(SharedString::from(level_label(value))),
            );
        }
        Some(
            v_flex()
                .gap(px(8.))
                .child(
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .text_size(px(FS_XS))
                        .text_color(pal.muted)
                        .child(Icon::new(IconName::Lightbulb).with_size(px(12.)))
                        .child("Thinking level"),
                )
                .child(segments),
        )
    }
}

impl Render for ModelPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.query.clone();
        self.query_mirror.flush(|text| {
            query.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        if self.reveal && self.store.read(cx).models().is_some() {
            self.reveal = false;
            self.reveal_current(cx);
        }
        if self.scroll_frames > 0 {
            self.scroll_to_highlight(cx);
            self.scroll_frames -= 1;
            if self.scroll_frames > 0 {
                cx.notify();
            }
        }
        let pal = Palette::active(cx);
        let laid_out = self.query_mirror.laid_out().flag();
        let sections = self.sections(cx);
        let mut list = v_flex()
            .id("model-list")
            .track_scroll(&self.scroll)
            .flex_1()
            .min_h_0()
            .max_h(px(360.))
            .overflow_y_scroll()
            .p(px(5.));
        let mut index = 0;
        for section in &sections {
            list = list.child(pop_label(&section.label, pal));
            for row in &section.rows {
                list = list.child(self.model_row(row, index, cx));
                index += 1;
            }
        }
        if index == 0 {
            list = list.child(
                div()
                    .p(px(14.))
                    .text_size(px(FS_SM))
                    .text_color(pal.faint)
                    .child(if self.store.read(cx).models().is_none() {
                        "Loading models..."
                    } else {
                        "No models match"
                    }),
            );
        }
        let mut foot = v_flex()
            .p(px(12.))
            .gap(px(8.))
            .border_t_1()
            .border_color(pal.subtle);
        if let Some(thinking) = self.thinking(cx) {
            foot = foot.child(thinking);
        }
        foot = foot
            .child(div().text_size(px(FS_2XS)).text_color(pal.faint).child(
                "Switching the model or thinking level mid-session invalidates the \
                         prompt cache.",
            ))
            .child(
                h_flex().child(
                    btn_sm("model-providers", BtnTone::Plain, pal)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close(window, cx);
                            window.dispatch_action(Box::new(crate::app::OpenProviders), cx);
                        }))
                        .child(Icon::new(IconName::Settings).with_size(px(12.)))
                        .child("Manage providers"),
                ),
            );
        v_flex()
            .key_context("Picker")
            .w(px(380.))
            .on_action(cx.listener(|this, _: &PickerUp, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &PickerDown, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &PickerRun, window, cx| {
                if let Some(row) = this.flat(cx).get(this.highlight) {
                    let id = row.id.clone();
                    this.choose(&id, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PickerClose, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| this.close(window, cx)))
            .child(search_row(&self.query, laid_out, pal))
            .child(list)
            .child(foot)
    }
}

/// The model option's values grouped by provider, for before the
/// catalog arrives.
fn fallback_groups(
    options: Option<Vec<SessionConfigSelectOption>>,
) -> Vec<(String, Vec<ModelRow>)> {
    let mut groups: Vec<(String, Vec<ModelRow>)> = Vec::new();
    for value in options.unwrap_or_default() {
        let provider = value
            .value
            .find(['/', ':'])
            .map_or(value.value.as_str(), |at| &value.value[..at])
            .to_owned();
        let entry = ModelEntry {
            id: value.value.clone(),
            name: value.name.clone(),
            context: None,
            input_cost: None,
            output_cost: None,
            thinking: Vec::new(),
            images: false,
            released: None,
        };
        let label = value
            .description
            .clone()
            .unwrap_or_else(|| provider.clone());
        let row = ModelRow::of(&provider, &entry);
        match groups.iter_mut().find(|(name, _)| *name == label) {
            Some((_, rows)) => rows.push(row),
            None => groups.push((label, vec![row])),
        }
    }
    groups
}

/// A level's segment label: `Auto` for the engine's default, `Max`
/// for its top effort, else the option's name capitalized.
fn level_label(value: &SessionConfigSelectOption) -> String {
    match value.value.as_str() {
        "default" => "Auto".to_owned(),
        "xhigh" => "Max".to_owned(),
        _ => {
            let mut chars = value.name.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    }
}

/// A context window as the picker's chip shows it: `200k`, `1.0M`.
fn window_size(tokens: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// The two-letter provider tile.
fn provider_icon(provider: &str, pal: &Palette) -> Div {
    let letters: String = provider
        .chars()
        .filter(char::is_ascii_alphabetic)
        .take(2)
        .collect::<String>()
        .to_uppercase();
    div()
        .size(px(22.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(pal.fill)
        .font_family(FONT_MONO)
        .font_weight(FontWeight::BOLD)
        .text_size(px(10.))
        .text_color(pal.muted)
        .child(SharedString::from(letters))
}

/// The small mono chip the rows carry.
fn chip(text: &str, pal: &Palette) -> Div {
    div()
        .h(px(18.))
        .px(px(6.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .bg(pal.fill)
        .font_family(FONT_MONO)
        .text_size(px(10.5))
        .text_color(pal.muted)
        .child(SharedString::from(text.to_owned()))
}

/// A section label: small, faint, upper case.
fn pop_label(text: &str, pal: &Palette) -> Div {
    div()
        .px(px(9.))
        .pt(px(6.))
        .pb(px(3.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(FS_2XS))
        .text_color(pal.faint)
        .child(SharedString::from(text.to_uppercase()))
}

/// The search row over a picker's list, with an optional key hint.
fn search_row(
    query: &Entity<InputState>,
    laid_out: std::rc::Rc<std::cell::Cell<bool>>,
    pal: &Palette,
) -> Div {
    h_flex()
        .items_center()
        .gap(px(8.))
        .px(px(12.))
        .py(px(10.))
        .border_b_1()
        .border_color(pal.subtle)
        .text_color(pal.faint)
        .on_prepaint(move |_, _, _| laid_out.set(true))
        .child(Icon::new(IconName::Search).with_size(px(14.)))
        .child(
            Input::new(query)
                .flex_1()
                .appearance(false)
                .bordered(false)
                .text_color(pal.ink),
        )
}

/// What one permission mode does to one kind of tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Runs,
    Rules,
    Asks,
    Refused,
}

impl Verdict {
    fn word(self) -> &'static str {
        match self {
            Self::Runs => "runs",
            Self::Rules => "your rules",
            Self::Asks => "asks first",
            Self::Refused => "refused",
        }
    }

    fn tone(self, pal: &Palette) -> Hsla {
        match self {
            Self::Runs => pal.ok,
            Self::Rules | Self::Asks => pal.warn,
            Self::Refused => pal.danger,
        }
    }
}

/// The kinds of tool call the policy grid names, in its order.
const KINDS: [&str; 6] = [
    "Read and search",
    "Edits and writes",
    "Shell commands",
    "Web fetch",
    "Agents and swarms",
    "MCP tools",
];

/// What `mode` does to each of [`KINDS`], as the engine's permission
/// gate decides with no config rules: built-ins run, MCP tools ask,
/// and a mode overrides both. `None` for a mode the engine did not
/// define.
fn policy(mode: &str) -> Option<[Verdict; 6]> {
    use Verdict::{Asks, Refused, Rules, Runs};
    Some(match mode {
        "default" => [Runs, Runs, Rules, Runs, Runs, Asks],
        "ask" => [Asks; 6],
        "allow" => [Runs; 6],
        "deny" => [Runs, Refused, Refused, Runs, Refused, Refused],
        _ => return None,
    })
}

/// The permission mode picker.
pub struct ModePicker {
    store: Entity<Store>,
    query: Entity<InputState>,
    query_mirror: Deferred,
    highlight: usize,
}

impl EventEmitter<PickerEvent> for ModePicker {}

impl Focusable for ModePicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }
}

impl ModePicker {
    /// A picker over `store`.
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Permission mode"));
        cx.subscribe_in(&query, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::Change = event {
                this.highlight = 0;
                cx.notify();
            }
        })
        .detach();
        Self {
            store,
            query,
            query_mirror: Deferred::new(),
            highlight: 0,
        }
    }

    fn close(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PickerEvent::Close);
    }

    /// Readies the picker as it opens, with the active mode
    /// highlighted.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query.clone();
        self.query_mirror.set(String::new(), |text| {
            query.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        let active = self.active(cx);
        self.highlight = self
            .modes(cx)
            .iter()
            .position(|mode| Some(&mode.value) == active.as_ref())
            .unwrap_or(0);
        cx.notify();
    }

    fn active(&self, cx: &App) -> Option<String> {
        let store = self.store.read(cx);
        store.permission_mode().or_else(|| {
            store
                .composer_option("mode")
                .map(|option| option.current_value)
        })
    }

    /// The modes the engine offers that match the query, plan mode
    /// aside.
    fn modes(&self, cx: &App) -> Vec<SessionConfigSelectOption> {
        let needle = self.query.read(cx).value().trim().to_lowercase();
        self.store
            .read(cx)
            .composer_option("mode")
            .map(|option| option.options)
            .unwrap_or_default()
            .into_iter()
            .filter(|mode| mode.value != PLAN_MODE)
            .filter(|mode| {
                needle.is_empty()
                    || format!(
                        "{} {}",
                        mode.name,
                        mode.description.as_deref().unwrap_or_default()
                    )
                    .to_lowercase()
                    .contains(&needle)
            })
            .collect()
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let total = self.modes(cx).len();
        if total == 0 {
            return;
        }
        self.highlight = self.highlight.saturating_add_signed(delta).min(total - 1);
        cx.notify();
    }

    fn choose(&mut self, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        let value = value.to_owned();
        self.store.act(cx, |store| {
            store.set_permission(&value);
        });
        self.close(window, cx);
    }

    fn mode_row(
        &self,
        mode: &SessionConfigSelectOption,
        index: usize,
        active: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let pal = Palette::active(cx);
        let tone = mode_tone(&mode.value, pal);
        let selected = pal.selected;
        let value = mode.value.clone();
        let hover = cx.entity();
        h_flex()
            .id(SharedString::from(format!("mode-row-{}", mode.value)))
            .w_full()
            .px(px(9.))
            .py(px(7.))
            .gap(px(10.))
            .items_center()
            .rounded(px(R_MD))
            .cursor_pointer()
            .when(index == self.highlight, |el| el.bg(selected))
            .on_hover(move |hovered, _, cx| {
                if *hovered {
                    hover.update(cx, |this, cx| {
                        this.highlight = index;
                        cx.notify();
                    });
                }
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.choose(&value, window, cx);
            }))
            .child(
                div()
                    .size(px(26.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.))
                    .bg(pal.fill)
                    .text_color(tone.unwrap_or(pal.muted))
                    .child(Icon::new(mode_icon(&mode.value)).with_size(px(14.))),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(FS_SM))
                            .text_color(tone.unwrap_or(pal.ink))
                            .child(SharedString::from(mode.name.clone())),
                    )
                    .children(mode.description.clone().map(|description| {
                        div()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(pal.faint)
                            .child(SharedString::from(description))
                    })),
            )
            .children(policy(&mode.value).map(|verdicts| {
                h_flex()
                    .flex_none()
                    .gap(px(3.))
                    .children(verdicts.map(|verdict| {
                        div()
                            .size(px(7.))
                            .rounded(px(2.))
                            .bg(verdict.tone(pal))
                            .opacity(if verdict == Verdict::Runs { 0.8 } else { 1. })
                    }))
            }))
            .when(active, |el| {
                el.child(
                    Icon::new(IconName::Check)
                        .with_size(px(14.))
                        .text_color(pal.accent),
                )
            })
            .into_any_element()
    }
}

impl Render for ModePicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.query.clone();
        self.query_mirror.flush(|text| {
            query.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        let pal = Palette::active(cx);
        let laid_out = self.query_mirror.laid_out().flag();
        let modes = self.modes(cx);
        let active = self.active(cx);
        let mut list = v_flex().p(px(5.));
        for (index, mode) in modes.iter().enumerate() {
            let on = active.as_deref() == Some(mode.value.as_str());
            list = list.child(self.mode_row(mode, index, on, cx));
        }
        let highlighted = modes
            .get(self.highlight)
            .and_then(|mode| policy(&mode.value));
        let foot = highlighted.map(|verdicts| {
            let mut grid = h_flex().flex_wrap().gap_y(px(8.));
            for (kind, verdict) in KINDS.iter().zip(verdicts) {
                grid = grid.child(
                    v_flex()
                        .w(relative(1. / 3.))
                        .min_w_0()
                        .pr(px(12.))
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(pal.faint)
                                .child(SharedString::from(kind.to_uppercase())),
                        )
                        .child(
                            div()
                                .font_family(FONT_MONO)
                                .text_size(px(FS_XS))
                                .text_color(verdict.tone(pal))
                                .child(verdict.word()),
                        ),
                );
            }
            v_flex()
                .p(px(12.))
                .gap(px(8.))
                .border_t_1()
                .border_color(pal.subtle)
                .bg(pal.fill)
                .child(grid)
                .child(div().text_size(px(11.5)).text_color(pal.muted).child(
                    "Rules in config.toml still apply on top, and a configured \
                             deny refuses in every mode.",
                ))
        });
        v_flex()
            .key_context("Picker")
            .w(px(440.))
            .on_action(cx.listener(|this, _: &PickerUp, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &PickerDown, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &PickerRun, window, cx| {
                if let Some(mode) = this.modes(cx).get(this.highlight) {
                    let value = mode.value.clone();
                    this.choose(&value, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PickerClose, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| this.close(window, cx)))
            .child(
                search_row(&self.query, laid_out, pal).child(
                    div()
                        .h(px(18.))
                        .px(px(5.))
                        .flex()
                        .flex_none()
                        .items_center()
                        .rounded(px(5.))
                        .border_1()
                        .border_color(pal.line)
                        .font_family(FONT_MONO)
                        .text_size(px(10.5))
                        .text_color(pal.faint)
                        .child("Shift Tab"),
                ),
            )
            .child(list)
            .children(foot)
    }
}

/// The icon one permission mode shows.
pub(crate) fn mode_icon(mode: &str) -> IconName {
    if mode.contains("allow") {
        IconName::ShieldAlert
    } else if mode.contains("deny") || mode.contains("read") {
        IconName::Eye
    } else if mode.contains("ask") {
        IconName::Hand
    } else if mode.contains("plan") {
        IconName::PenLine
    } else {
        IconName::ShieldQuestionMark
    }
}

/// The tone one permission mode paints with: warn for the rules,
/// danger for allow, the accent for deny, plain for ask.
pub(crate) fn mode_tone(mode: &str, pal: &Palette) -> Option<Hsla> {
    if mode.contains("allow") {
        Some(pal.danger)
    } else if mode.contains("deny") || mode.contains("read") {
        Some(pal.accent)
    } else if mode.contains("ask") || mode.contains("plan") {
        None
    } else {
        Some(pal.warn)
    }
}

#[cfg(test)]
mod tests {
    use super::{level_label, policy, window_size};
    use kage_client::wire::SessionConfigSelectOption;

    #[test]
    fn context_windows_read_like_the_design() {
        assert_eq!(window_size(200_000), "200k");
        assert_eq!(window_size(1_000_000), "1.0M");
        assert_eq!(window_size(512), "512");
    }

    #[test]
    fn levels_read_as_the_segment_labels() {
        let level = |value: &str, name: &str| SessionConfigSelectOption {
            value: value.into(),
            name: name.into(),
            description: None,
        };
        assert_eq!(level_label(&level("default", "auto")), "Auto");
        assert_eq!(level_label(&level("xhigh", "xhigh")), "Max");
        assert_eq!(level_label(&level("low", "low")), "Low");
    }

    #[test]
    fn every_engine_mode_has_a_policy() {
        for mode in ["default", "ask", "allow", "deny"] {
            assert!(policy(mode).is_some(), "{mode}");
        }
        assert!(policy("plan").is_none());
    }
}
