//! The composer: the input, its key table, the toolbar and the menus.
//!
//! One view over the store, rendered at the bottom of a chat and fit
//! to host in the welcome pane later, so a draft never resets. The
//! key table: Enter sends and queues while a run is in flight,
//! Ctrl+Enter steers only when the agent advertised steering,
//! Shift+Enter is a newline, Esc Esc interrupts inside a double-press
//! window, and Shift+Tab cycles the permission modes the config
//! options list. The toolbar carries the plus popover, the mode,
//! model and thinking pickers, the context ring with the fuel gauge,
//! and the send and stop pair.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use web_time::Instant;

use gpui_kit::assets::IconName;
use gpui_kit::base::Selectable;
use gpui_kit::component::input::{
    Escape, InlineToken, InputContent, InputEvent, InputToken, Textarea, TextareaState,
};
use gpui_kit::component::menu::DropdownMenu;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AnyElement, AnyView, App, AppContext as _, Bounds, Context, Div, Entity, Focusable,
    FontWeight, Hsla, InteractiveElement, Interactivity, IntoElement, ParentElement, Pixels,
    Render, SharedString, Stateful, StatefulInteractiveElement, StyleRefinement, Styled, Window,
    anchored, deferred, div, point, px, relative,
};
use kage_client::Session;
use kage_client::wire::{
    FsKind, FsListResult, NoticeTone, SessionConfigKind, SessionConfigOption,
    SessionConfigSelectOption,
};
use serde_json::Value;

use crate::store::{Store, StoreHandle as _};
use crate::theme::{
    FONT_MONO, FS_2XS, FS_BASE, FS_SM, FS_XS, LINE_HEIGHT, Palette, R_COMPOSER, R_FULL, R_LG, R_MD,
    SP_2, WEIGHT_SEMIBOLD,
};
use crate::views::agents::tokens;
use crate::views::deferred::{Deferred, LaidOut};
use crate::views::dialog::{DialogKind, DialogView};
use crate::views::kit::badge;
use crate::views::pickers::{ModePicker, ModelPicker, PickerEvent, mode_icon, mode_tone};
use gpui_kit::base::ElementExt as _;
use gpui_kit::base::TestSupportExt as _;

gpui_kit::actions!(kage_desktop, [CycleMode]);

/// How long the first Esc of an interrupt gesture waits for the
/// second one.
pub(crate) const ESC_WINDOW: Duration = Duration::from_millis(1500);

/// A toolbar control that hosts a popover or a dropdown menu while
/// styled as a plain pill. The popover machinery asks its trigger for
/// the selectable contract; the pill keeps its open-state styling on
/// the view's own flag instead.
struct Pill {
    element: Stateful<Div>,
}

impl Pill {
    /// Wraps an id-carrying stateful div as a popover trigger.
    fn new(id: &'static str) -> Self {
        Self {
            element: div().id(id).flex(),
        }
    }
}

impl Selectable for Pill {
    fn selected(self, _: bool) -> Self {
        self
    }

    fn is_selected(&self) -> bool {
        false
    }
}

impl Styled for Pill {
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl InteractiveElement for Pill {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.element.interactivity()
    }
}

impl StatefulInteractiveElement for Pill {}

impl ParentElement for Pill {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.element.extend(elements);
    }
}

impl IntoElement for Pill {
    type Element = Stateful<Div>;

    fn into_element(self) -> Self::Element {
        self.element
    }
}

impl DropdownMenu for Pill {}

/// A trigger, its open flag, and the pill shape both share: 30px
/// tall, fully round, muted text that fills and inks on hover or
/// while the attached surface is open.
fn toolbar_pill(id: &'static str, open: bool, pal: &'static Palette, cx: &App) -> Pill {
    let theme = cx.theme().colors;
    let (hover, ink, muted) = (pal.hover, theme.foreground, theme.muted_foreground);
    Pill::new(id)
        .h(px(30.))
        .px(px(9.))
        .gap(px(6.))
        .flex_none()
        .items_center()
        .rounded(px(R_FULL))
        .text_size(px(FS_SM))
        .text_color(muted)
        .hover(move |style| style.bg(hover).text_color(ink))
        .when(open, |pill| pill.bg(hover).text_color(ink))
}

/// A pill's bounds, as it last prepainted.
type PillBounds = Rc<Cell<Option<Bounds<Pixels>>>>;

/// The two pickers the toolbar opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Picker {
    Mode,
    Model,
}

/// The surface a picker opens on, above the pill at `at` and aligned
/// to its `corner`. A press anywhere but the surface or its own pill
/// closes it; the pill's own press toggles it instead.
fn picker_overlay(
    composer: Entity<ComposerView>,
    picker: AnyView,
    at: Bounds<Pixels>,
    corner: Anchor,
    pal: &Palette,
) -> AnyElement {
    let x = match corner {
        Anchor::BottomRight => at.right(),
        _ => at.left(),
    };
    let mut surface = div()
        .occlude()
        .bg(pal.bg)
        .border_1()
        .border_color(pal.line)
        .rounded(px(R_LG))
        .overflow_hidden()
        .on_mouse_down_out(move |event, window, cx| {
            if at.contains(&event.position) {
                return;
            }
            composer.update(cx, |this, cx| this.close_pickers(window, cx));
        })
        .child(picker);
    surface.style().box_shadow = Some(pal.shadow_menu.clone());
    deferred(
        anchored()
            .anchor(corner)
            .position(point(x, at.top() - px(6.)))
            .snap_to_window_with_margin(px(8.))
            .child(surface),
    )
    .with_priority(1)
    .into_any_element()
}

/// The context fill the engine compacts at on its own.
const COMPACT_AT: f64 = 0.8;

/// What the context gauge shows, read when its trigger renders.
#[derive(Debug, Clone)]
struct Gauge {
    used: u64,
    size: u64,
    cost: Option<String>,
    model: Option<String>,
    turns: usize,
    running: bool,
}

impl Gauge {
    /// The gauge: twenty cells filled to the context in use with the
    /// compaction mark at the sixteenth, the totals, and Compact now.
    fn render(&self, store: &Entity<Store>, pal: &'static Palette) -> Div {
        let fill = if self.size > 0 {
            self.used as f64 / self.size as f64
        } else {
            0.0
        };
        let percent = (fill * 100.0).round() as i64;
        let hot = fill >= COMPACT_AT;
        let cells = h_flex().gap(px(3.)).children((0..20).map(|cell| {
            let mid = (cell as f64 + 0.5) / 20.0;
            let lit = mid <= fill;
            div()
                .flex_1()
                .h(px(14.))
                .rounded(px(3.))
                .bg(match (lit, hot) {
                    (false, _) => pal.fill_hover,
                    (true, true) => pal.warn,
                    (true, false) => pal.accent,
                })
                .when(cell == 16, |cell| {
                    cell.border_l_2().border_color(pal.ink_strong)
                })
        }));
        let stat = |label: &'static str, value: String| {
            v_flex()
                .flex_1()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(FS_2XS))
                        .text_color(pal.faint)
                        .child(label),
                )
                .child(div().text_size(px(FS_SM)).text_color(pal.ink).child(value))
        };
        let store = store.clone();
        let running = self.running;
        v_flex()
            .w(px(400.))
            .px(px(14.))
            .py(px(12.))
            .gap(px(10.))
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(pal.ink_strong)
                            .child("Context"),
                    )
                    .child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(FS_XS))
                            .text_color(pal.muted)
                            .child(format!("{} / {}", tokens(self.used), tokens(self.size))),
                    )
                    .child(
                        div()
                            .px(px(7.))
                            .rounded(px(R_FULL))
                            .text_size(px(FS_2XS))
                            .bg(if hot { pal.warn_soft } else { pal.fill })
                            .text_color(if hot { pal.warn } else { pal.muted })
                            .child(format!("{percent}%")),
                    ),
            )
            .child(cells)
            .child(
                h_flex()
                    .justify_between()
                    .text_size(px(FS_2XS))
                    .text_color(pal.faint)
                    .child("0")
                    .child("compacts at 80%")
                    .child(tokens(self.size)),
            )
            .child(
                v_flex()
                    .gap(px(10.))
                    .pt(px(10.))
                    .border_t_1()
                    .border_color(pal.subtle)
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(stat("Spent", self.cost.clone().unwrap_or_else(|| "unknown".into())))
                            .child(stat("Model", self.model.clone().unwrap_or_else(|| "unknown".into())))
                            .child(stat("Turns", self.turns.to_string())),
                    )
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(FS_2XS))
                                    .text_color(pal.faint)
                                    .child("The engine compacts on its own at 80%, summarizing older turns."),
                            )
                            .child(
                                crate::views::kit::btn_sm("gauge-compact", crate::views::kit::BtnTone::Plain, pal)
                                    .when(running, |btn| btn.opacity(0.45))
                                    .when(!running, |btn| {
                                        btn.on_click(move |_, _, cx| {
                                            store.act(cx, Store::compact);
                                        })
                                    })
                                    .child(Icon::new(IconName::Layers).with_size(px(12.)))
                                    .child("Compact now"),
                            ),
                    ),
            )
    }
}

/// The one-word labels inside the hint line.
fn hint_word(text: &'static str) -> Div {
    div().child(text)
}

/// A key cap as the hint line draws it.
fn kbd(label: &str, pal: &Palette) -> Div {
    div()
        .mx(px(2.))
        .h(px(16.))
        .px(px(5.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .border_1()
        .border_color(pal.line)
        .font_family(FONT_MONO)
        .text_size(px(10.))
        .text_color(pal.faint)
        .child(SharedString::from(label.to_owned()))
}

/// The uppercase section label between popover row groups.
fn pop_label(text: &'static str, pal: &Palette) -> Div {
    div()
        .px(px(9.))
        .pt(px(6.))
        .pb(px(3.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(FS_2XS))
        .text_color(pal.faint)
        .child(text)
}

/// The on/off chip the mode rows carry.
fn onoff(on: bool, pal: &Palette) -> Div {
    let (bg, fg) = if on {
        (pal.accent_soft, pal.accent)
    } else {
        (pal.fill, pal.faint)
    };
    div()
        .h(px(18.))
        .px(px(8.))
        .flex()
        .flex_none()
        .items_center()
        .rounded(px(R_FULL))
        .bg(bg)
        .font_family(FONT_MONO)
        .text_size(px(FS_2XS))
        .text_color(fg)
        .child(if on { "on" } else { "off" })
}

/// A key cap for the right edge of a plus-menu row.
fn add_kbd(label: &'static str, pal: &Palette) -> Div {
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
        .child(label)
}

/// The label and description pair a plus-menu row carries, with an
/// optional chip at the far end.
fn plus_row_body(
    label: &'static str,
    description: &'static str,
    tail: Option<Div>,
    pal: &Palette,
) -> Div {
    h_flex()
        .min_w_0()
        .flex_1()
        .gap(px(10.))
        .child(
            v_flex()
                .min_w_0()
                .flex_1()
                .child(div().text_size(px(FS_SM)).text_color(pal.ink).child(label))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(pal.faint)
                        .truncate()
                        .child(description),
                ),
        )
        .children(tail)
}

/// The popover surface the suggestion menus render on.
fn suggest_surface(id: &'static str, pal: &Palette) -> Stateful<Div> {
    v_flex()
        .id(id)
        .w(px(460.))
        .max_h(px(300.))
        .overflow_y_scroll()
        .p(px(5.))
        .mb(px(6.))
        .bg(pal.menu)
        .border_1()
        .border_color(pal.line)
        .rounded(px(R_LG))
        .shadow(pal.shadow_menu.clone())
}

/// One suggestion row: an icon, the picked text in the mono family,
/// and the row's badge.
fn suggest_row(id: SharedString, icon: IconName, pal: &Palette) -> Stateful<Div> {
    let selected = pal.selected;
    h_flex()
        .id(id)
        .w_full()
        .px(px(9.))
        .py(px(7.))
        .gap(px(10.))
        .items_center()
        .rounded(px(R_MD))
        .hover(move |style| style.bg(selected))
        .child(Icon::new(icon).text_color(pal.muted).with_size(px(16.)))
}

/// One plus-menu row: a filled icon chip in front of the row body.
fn add_row(id: &'static str, icon: IconName, pal: &Palette) -> Stateful<Div> {
    let selected = pal.selected;
    h_flex()
        .id(id)
        .w_full()
        .px(px(7.))
        .py(px(6.))
        .gap(px(10.))
        .items_center()
        .rounded(px(R_MD))
        .hover(move |style| style.bg(selected))
        .child(
            div()
                .size(px(26.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(7.))
                .bg(pal.fill)
                .text_color(pal.muted)
                .child(Icon::new(icon).with_size(px(14.))),
        )
}

/// What one Esc press means, given the interrupt window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscStep {
    /// Open the window and show the hint.
    Arm,
    /// The second press inside the window: interrupt the run.
    Cancel,
    /// Nothing to interrupt or the window closed: drop the hint.
    Clear,
}

/// Classifies one Esc press. Idle presses only ever clear the hint.
#[must_use]
pub(crate) fn esc_step(armed: Option<Instant>, now: Instant, running: bool) -> EscStep {
    if !running {
        return EscStep::Clear;
    }
    match armed {
        Some(at) if now.duration_since(at) < ESC_WINDOW => EscStep::Cancel,
        _ => EscStep::Arm,
    }
}

/// Where the typed text points a suggestion: a slash command or an
/// at-mention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Suggest {
    /// The text before the caret is a bare `/token`.
    Slash {
        /// The text after the slash.
        query: String,
    },
    /// The text before the caret ends in `@token`.
    Mention {
        /// The text after the at.
        query: String,
        /// Byte offset of the at in the text.
        at: usize,
    },
}

/// The suggestion the value and caret point at, if any.
#[must_use]
pub(crate) fn suggest_for(value: &str, cursor: usize) -> Option<Suggest> {
    let before = value.get(..cursor).unwrap_or(value);
    if before.starts_with('/') && !before.contains(char::is_whitespace) {
        return Some(Suggest::Slash {
            query: before[1..].to_owned(),
        });
    }
    let at = before.rfind('@')?;
    if at > 0 && !before[..at].ends_with(char::is_whitespace) {
        return None;
    }
    let token = &before[at + 1..];
    if token.contains(char::is_whitespace) {
        return None;
    }
    Some(Suggest::Mention {
        query: token.to_owned(),
        at,
    })
}

/// One slash menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlashItem {
    /// The command name, without the leading slash.
    pub name: String,
    /// What the command does, as the agent described it.
    pub description: String,
    /// The input shape the command wants.
    pub hint: Option<String>,
    /// The badge: `agent` for the agent's own commands, none for the
    /// server commands whose name carries the `server:` prefix.
    pub badge: Option<&'static str>,
}

/// The commands the client runs itself, ahead of the agent's: name,
/// argument hint and what it does.
const BUILTINS: [(&str, &str, &str); 4] = [
    (
        "swarm",
        "on | off | <task>",
        "Toggle swarm mode, or run one task as a swarm",
    ),
    ("plan", "on | off", "Toggle plan mode"),
    (
        "goal",
        "<text> | clear",
        "Set a goal the agent keeps pursuing",
    ),
    ("new", "", "Start a new session"),
];

/// A built-in command as typed: its name and the rest of the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Builtin<'a> {
    pub name: &'a str,
    pub args: &'a str,
}

/// What became of a built-in command the composer recognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinRun {
    /// It ran.
    Ran,
    /// It needs a session and none is active; the text stays.
    NeedsSession,
    /// The composer does not run it; the agent gets the text.
    NotMine,
}

/// The built-in command `text` runs, if it names one.
pub(crate) fn builtin(text: &str) -> Option<Builtin<'_>> {
    let rest = text.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    BUILTINS
        .iter()
        .any(|(known, ..)| *known == name)
        .then(|| Builtin {
            name,
            args: args.trim(),
        })
}

/// The length of the command that opens `text`, slash included, when
/// it is one the composer offers and a space already follows it.
#[must_use]
pub(crate) fn command_len(text: &str, commands: &[Value]) -> Option<usize> {
    let rest = text.strip_prefix('/')?;
    let (name, _) = rest.split_once(char::is_whitespace)?;
    let known = BUILTINS.iter().any(|(builtin, ..)| *builtin == name)
        || commands
            .iter()
            .any(|command| command.get("name").and_then(Value::as_str) == Some(name));
    (!name.is_empty() && known).then_some(1 + name.len())
}

/// The slash menu rows the session's available commands deliver for
/// `query`, in the order the agent listed them.
#[must_use]
pub(crate) fn slash_items(commands: &[Value], query: &str) -> Vec<SlashItem> {
    let builtins = BUILTINS
        .iter()
        .filter(|(name, ..)| name.starts_with(query))
        .map(|(name, hint, description)| SlashItem {
            name: (*name).to_owned(),
            description: (*description).to_owned(),
            hint: (!hint.is_empty()).then(|| (*hint).to_owned()),
            badge: None,
        });
    let agent = commands.iter().filter_map(|command| {
        let name = command.get("name")?.as_str()?;
        if !name.starts_with(query) {
            return None;
        }
        Some(SlashItem {
            name: name.to_owned(),
            description: command
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            hint: command
                .get("input")
                .and_then(|input| input.get("hint"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            badge: if name.contains(':') {
                None
            } else {
                Some("agent")
            },
        })
    });
    builtins.chain(agent).collect()
}

/// One mention menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MentionItem {
    /// The path relative to the session workdir.
    pub path: String,
    /// Whether the entry is a directory.
    pub directory: bool,
}

/// The mention rows a `_kage/fs` listing delivers for `query`, case
/// insensitively. No listing yields no rows: the menu says so itself.
#[must_use]
pub(crate) fn mention_items(listing: Option<&FsListResult>, query: &str) -> Vec<MentionItem> {
    let query = query.to_lowercase();
    listing
        .into_iter()
        .flat_map(|listing| listing.entries.iter())
        .filter(|entry| entry.path.to_lowercase().contains(&query))
        .map(|entry| MentionItem {
            path: entry.path.clone(),
            directory: entry.kind == FsKind::Directory,
        })
        .collect()
}

/// The select config option `id` of a session, when the agent
/// advertised it as a select.
#[must_use]
pub(crate) fn select_option<'a>(session: &'a Session, id: &str) -> Option<&'a SessionConfigOption> {
    session
        .config_options
        .iter()
        .find(|option| option.id == id && option.kind == SessionConfigKind::Select)
}

/// The mode id that is active now: what `current_mode_update` last
/// marked, else what the mode option reports.
#[must_use]
pub(crate) fn active_mode(session: &Session) -> Option<String> {
    session
        .mode
        .clone()
        .or_else(|| select_option(session, "mode").map(|option| option.current_value.clone()))
}

/// The value of the mode option that is plan mode rather than a
/// permission mode.
pub(crate) const PLAN_MODE: &str = "plan";

/// The permission modes the mode option advertises, in its order: every
/// value but plan mode, which the toolbar shows as its own chip.
pub(crate) fn permission_values(session: &Session) -> Vec<&SessionConfigSelectOption> {
    select_option(session, "mode")
        .map(|option| {
            option
                .options
                .iter()
                .filter(|value| value.value != PLAN_MODE)
                .collect()
        })
        .unwrap_or_default()
}

/// The permission mode Shift+Tab cycles to from `current`: the next
/// advertised one, wrapping around. No option or an empty list cycles
/// nowhere.
#[must_use]
pub(crate) fn next_mode_value(session: &Session, current: &str) -> Option<String> {
    let values = permission_values(session);
    if values.is_empty() {
        return None;
    }
    let index = values
        .iter()
        .position(|value| value.value == current)
        .map_or(0, |index| (index + 1) % values.len());
    Some(values[index].value.clone())
}

/// The goal text option of `session`, when one is set.
fn session_goal(session: Option<&Session>) -> Option<String> {
    session?
        .config_options
        .iter()
        .find(|option| option.id == "goal")
        .map(|option| option.current_value.clone())
        .filter(|goal| !goal.is_empty())
}

/// The composer view.
pub struct ComposerView {
    store: Entity<Store>,
    input: Entity<TextareaState>,
    /// The dialog layer the goal and swarm entries open.
    dialog: Entity<DialogView>,
    /// The session the textarea currently mirrors.
    loaded: Option<String>,
    /// When the first Esc of an interrupt gesture landed, while the
    /// window is open.
    esc_armed: Option<Instant>,
    /// Whether the plus popover shows.
    plus_open: bool,
    /// Whether the permission mode picker shows.
    mode_open: bool,
    /// Whether the model picker shows.
    model_open: bool,
    mode_picker: Entity<ModePicker>,
    model_picker: Entity<ModelPicker>,
    /// Where the mode and model pills sit, recorded as they prepaint,
    /// so their pickers open over them.
    mode_bounds: PillBounds,
    model_bounds: PillBounds,
    /// Whether a listing for the loaded session was already asked, so
    /// typing an at-mention does not ask twice.
    fs_asked: bool,
    /// The placeholder the textarea currently carries, so the
    /// per-state phrasing only re-sets on a real change.
    placeholder: String,
    /// The draft mirror, held until the textarea can take a write. The
    /// input engines copy the window's family once, at construction, and
    /// only the element corrects it, during its own first prepaint, so a
    /// write before that asks the text system for a family the web cannot
    /// resolve and takes the frame down. See [`crate::views::deferred`].
    draft_mirror: Deferred,
}

impl ComposerView {
    /// A composer following `store`, focused and bound to its keys.
    pub fn new(
        store: Entity<Store>,
        dialog: Entity<DialogView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Ask kage anything, @ to mention, / for commands")
                .auto_grow(1, 13)
                .submit_on_enter(true)
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        // A store change may land before the composer has ever been laid
        // out, so the mirror of the active session's draft is held until
        // then rather than written; see [`crate::views::deferred`].
        cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                // With Enter sends off, a plain Enter adds the newline the
                // textarea left out and Ctrl+Enter sends.
                InputEvent::PressEnter { secondary, shift } => {
                    if this.store.read(cx).prefs().enter_sends {
                        if !*shift {
                            this.submit(*secondary, window, cx);
                        }
                    } else if *secondary {
                        this.submit(false, window, cx);
                    } else if !*shift {
                        this.input
                            .update(cx, |state, cx| state.insert("\n", window, cx));
                    }
                }
                InputEvent::Change => {
                    this.on_input_change(cx);
                    this.mark_command(window, cx);
                }
                InputEvent::Blur => {
                    if this.esc_armed.take().is_some() {
                        cx.notify();
                    }
                }
                InputEvent::Focus => {}
            },
        )
        .detach();
        // The mirror is recomputed on every store change and written by
        // the next render once the textarea can take it, so a draft
        // change never waits on further traffic.
        cx.observe_in(&store, window, |this, _, window, cx| {
            this.follow_active(window, cx);
            this.take_back_prompt(window, cx);
            this.sync_placeholder(window, cx);
        })
        .detach();
        let mode_picker = cx.new(|cx| ModePicker::new(store.clone(), window, cx));
        let model_picker = cx.new(|cx| ModelPicker::new(store.clone(), window, cx));
        cx.subscribe_in(
            &mode_picker,
            window,
            |this, _, _: &PickerEvent, window, cx| this.close_pickers(window, cx),
        )
        .detach();
        cx.subscribe_in(
            &model_picker,
            window,
            |this, _, _: &PickerEvent, window, cx| this.close_pickers(window, cx),
        )
        .detach();
        Self {
            store,
            input,
            dialog,
            loaded: None,
            esc_armed: None,
            plus_open: false,
            mode_open: false,
            model_open: false,
            mode_picker,
            model_picker,
            mode_bounds: PillBounds::default(),
            model_bounds: PillBounds::default(),
            fs_asked: false,
            placeholder: String::new(),
            draft_mirror: Deferred::new(),
        }
    }

    /// The multi-line input state, shared with the transcript's edit
    /// action.
    #[must_use]
    pub fn input(&self) -> &Entity<TextareaState> {
        &self.input
    }

    /// The signal for when this view's textarea element has prepainted.
    ///
    /// Another view that writes that same textarea takes a copy of it,
    /// because only this view renders the element.
    #[must_use]
    pub fn input_laid_out(&self) -> LaidOut {
        self.draft_mirror.laid_out().clone()
    }

    /// Whether a run is in flight on the session the composer follows.
    fn running(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .active_session()
            .is_some_and(|session| session.running)
    }

    fn input_value(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    fn input_cursor(&self, cx: &App) -> usize {
        self.input.read(cx).cursor()
    }

    /// Follows the store's active session: banks the text the textarea
    /// holds into the old session's draft, then loads the new
    /// session's draft into the textarea.
    ///
    /// The write is held until the textarea has been laid out once; see
    /// [`crate::views::deferred`] for why it cannot happen earlier.
    fn follow_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.store.read(cx).active_id().map(str::to_owned);
        if active == self.loaded {
            return;
        }
        let previous = self.loaded.take();
        let held = self.input_value(cx);
        self.store
            .update(cx, |store, _| store.set_draft(previous.as_deref(), &held));
        let draft = active
            .as_deref()
            .and_then(|id| self.store.read(cx).draft(id))
            .unwrap_or_default()
            .to_owned();
        self.loaded = active;
        self.esc_armed = None;
        self.fs_asked = false;
        self.plus_open = false;
        let input = self.input.clone();
        self.draft_mirror.set(draft, |draft| {
            input.update(cx, |state, cx| state.set_value(draft, window, cx));
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Puts a welcome prompt whose session failed to open back in an
    /// empty textarea, so the text is not lost with the session.
    fn take_back_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self
            .store
            .update(cx, |store, _| store.take_returned_prompt())
        else {
            return;
        };
        if !self.input_value(cx).trim().is_empty() {
            return;
        }
        let input = self.input.clone();
        self.draft_mirror.set(text, |text| {
            input.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        cx.notify();
    }

    /// Keeps the textarea's placeholder on the per-state phrasing:
    /// queueing while a run is in flight, then swarm, then plan, then
    /// an open ask, then the default.
    fn sync_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let session = store.active_session();
        let wanted = if self.running(cx) {
            "Queue a follow-up, or Ctrl+Enter to steer the running turn"
        } else if store.swarm_on() {
            "Describe work to split across parallel agents..."
        } else if store.plan_on() {
            "Describe what to plan..."
        } else if session.is_some_and(|session| !session.permissions.is_empty()) {
            "Answer the request above first, or type to queue"
        } else {
            "Ask kage anything, @ to mention, / for commands"
        };
        if self.placeholder == wanted {
            return;
        }
        self.placeholder = wanted.to_owned();
        self.input
            .update(cx, |state, cx| state.set_placeholder(wanted, window, cx));
    }

    /// Banks the typed text into the session's draft, clears the
    /// interrupt hint, and asks for a listing the first time an
    /// at-mention needs one.
    fn on_input_change(&mut self, cx: &mut Context<Self>) {
        let value = self.input_value(cx);
        let loaded = self.loaded.clone();
        self.store
            .update(cx, |store, _| store.set_draft(loaded.as_deref(), &value));
        self.esc_armed = None;
        let cursor = self.input_cursor(cx);
        if matches!(suggest_for(&value, cursor), Some(Suggest::Mention { .. })) {
            let asked = self.fs_asked;
            let has_listing = loaded
                .as_deref()
                .and_then(|id| self.store.read(cx).fs_listing(id))
                .is_some();
            if !asked && !has_listing {
                self.fs_asked = true;
                self.store.act(cx, |store| {
                    store.fs_list("");
                });
            }
        }
        cx.notify();
    }

    /// Sends the typed text: on the welcome pane it opens the session
    /// the text rides on; plain when idle, or queued while a run is
    /// in flight, or steered into the run when `steer` says so and the
    /// wire allows it. A steer with no run in flight sends plainly. A
    /// built-in that needs a session keeps its text in the composer.
    /// Accepted text leaves the textarea and the draft.
    pub fn submit(&mut self, steer: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_value(cx);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Some(command) = builtin(text) {
            match self.run_builtin(&command, window, cx) {
                BuiltinRun::Ran => {
                    self.clear_input(window, cx);
                    return;
                }
                BuiltinRun::NeedsSession => {
                    self.store.act(cx, |store| {
                        store.note(
                            NoticeTone::Info,
                            format!("/{} needs a session; open one first", command.name),
                        );
                    });
                    return;
                }
                BuiltinRun::NotMine => {}
            }
        }
        let accepted = if self.loaded.is_none() && !self.store.read(cx).pending_prompt() {
            self.store.act(cx, |store| store.open_with_prompt(text));
            true
        } else if steer && self.running(cx) {
            self.store.act(cx, |store| store.steer(text).is_ok())
        } else {
            self.store.act(cx, |store| store.submit(text).is_some())
        };
        if !accepted {
            return;
        }
        self.clear_input(window, cx);
    }

    /// Empties the textarea and the session draft after a send.
    fn clear_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let loaded = self.loaded.clone();
        self.store
            .update(cx, |store, _| store.set_draft(loaded.as_deref(), ""));
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.esc_armed = None;
        self.sync_placeholder(window, cx);
        cx.notify();
    }

    /// Runs a built-in command. One that needs a session reports
    /// [`BuiltinRun::NeedsSession`], and its text stays in the
    /// composer.
    fn run_builtin(
        &mut self,
        command: &Builtin<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BuiltinRun {
        let has_session = self.store.read(cx).active_session().is_some();
        let dialog = self.dialog.clone();
        match (command.name, command.args) {
            ("new", _) => {
                self.store.act(cx, Store::show_welcome);
                BuiltinRun::Ran
            }
            (_, _) if !has_session => BuiltinRun::NeedsSession,
            ("plan", "on") => {
                self.store.act(cx, Store::enter_plan);
                BuiltinRun::Ran
            }
            ("plan", "off") => {
                self.store.act(cx, Store::exit_plan);
                BuiltinRun::Ran
            }
            ("plan", _) => {
                self.store.act(cx, |store| {
                    if store.plan_on() {
                        store.exit_plan()
                    } else {
                        store.enter_plan()
                    }
                });
                BuiltinRun::Ran
            }
            ("goal", "") => {
                dialog.update(cx, |dialog, cx| dialog.open(DialogKind::Goal, window, cx));
                BuiltinRun::Ran
            }
            ("goal", "clear") => {
                self.store.act(cx, |store| store.set_option("goal", ""));
                BuiltinRun::Ran
            }
            ("goal", text) => {
                self.store.act(cx, |store| store.set_option("goal", text));
                BuiltinRun::Ran
            }
            ("swarm", "off") => {
                self.store.act(cx, |store| store.set_option("swarm", "off"));
                BuiltinRun::Ran
            }
            ("swarm", "on" | "") => {
                dialog.update(cx, |dialog, cx| {
                    dialog.open(DialogKind::ConfirmSwarm, window, cx)
                });
                BuiltinRun::Ran
            }
            ("swarm", task) => {
                let task = task.to_owned();
                let sent = self.store.act(cx, |store| {
                    store.set_option("swarm", "on") && store.submit(&task).is_some()
                });
                if sent {
                    BuiltinRun::Ran
                } else {
                    BuiltinRun::NotMine
                }
            }
            _ => BuiltinRun::NotMine,
        }
    }

    /// One Esc press, per the interrupt window.
    fn on_escape(&mut self, cx: &mut Context<Self>) {
        let running = self.running(cx);
        let now = Instant::now();
        match esc_step(self.esc_armed, now, running) {
            EscStep::Arm => {
                self.esc_armed = Some(now);
                let armed = now;
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(ESC_WINDOW).await;
                    let _ = this.update(cx, |this, cx| {
                        if this.esc_armed == Some(armed) {
                            this.esc_armed = None;
                            cx.notify();
                        }
                    });
                })
                .detach();
                cx.notify();
            }
            EscStep::Cancel => {
                self.esc_armed = None;
                self.store.update(cx, |store, cx| {
                    store.cancel();
                    cx.notify();
                });
                cx.notify();
            }
            EscStep::Clear => {
                if self.esc_armed.take().is_some() {
                    cx.notify();
                }
            }
        }
    }

    /// Steps the permission mode to the next advertised value. Plan
    /// mode stays on; it has its own chip.
    fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        self.store.act(cx, |store| {
            let current = store.permission_mode().unwrap_or_default();
            let next = store
                .active_session()
                .and_then(|session| next_mode_value(session, &current));
            if let Some(value) = next {
                store.set_permission(&value);
            }
        });
    }

    /// Turns the command that opens the text into a token, so it reads
    /// apart from the prompt. The text stays the same.
    fn mark_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let content = self.input.read(cx).content();
        if content.tokens().iter().any(|span| span.range().start == 0) {
            return;
        }
        let text = content.text().clone();
        let Some(len) = command_len(&text, self.store.read(cx).composer_commands()) else {
            return;
        };
        let token = InlineToken::new("command", &text[..len]);
        let Ok(marked) = InputContent::new(text.clone()).with_token(0..len, token) else {
            return;
        };
        let caret = self.input_cursor(cx);
        self.input.update(cx, |state, cx| {
            state.set_value(marked, window, cx);
            state.set_selected_range(caret..caret, cx);
        });
    }

    /// Replaces the slash token before the caret with the picked
    /// command, ready for its arguments.
    fn pick_slash(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.input_value(cx);
        let cursor = self.input_cursor(cx);
        let after = value.get(cursor..).unwrap_or("").to_owned();
        let text = format!("/{name} {after}");
        let caret = 1 + name.len() + 1;
        self.input.update(cx, |state, cx| {
            state.set_value(text.as_str(), window, cx);
            state.set_selected_range(caret..caret, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// Replaces the at-mention before the caret with the picked path.
    fn pick_mention(&mut self, at: usize, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.input_value(cx);
        let cursor = self.input_cursor(cx);
        let before = value.get(..cursor).unwrap_or(&value).to_owned();
        let after = value.get(cursor..).unwrap_or("").to_owned();
        let head = before.get(..at).unwrap_or(&before).to_owned();
        let text = format!("{head}@{path} {after}");
        let caret = head.len() + 1 + path.len() + 1;
        self.input.update(cx, |state, cx| {
            state.set_value(text.as_str(), window, cx);
            state.set_selected_range(caret..caret, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// Inserts an at-mention seed at the caret, as the plus menu's
    /// mention entry does.
    fn seed_mention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_mention(None, window, cx);
    }

    /// Inserts an at-mention at the caret: a bare `@` when `path` is
    /// `None`, else `@path` with a trailing space, as the workbench's
    /// files pane does.
    pub fn insert_mention(
        &mut self,
        path: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = self.input_value(cx);
        let cursor = self.input_cursor(cx);
        let before = value.get(..cursor).unwrap_or(&value).to_owned();
        let after = value.get(cursor..).unwrap_or("").to_owned();
        let spaced = before.is_empty()
            || before.ends_with(char::is_whitespace)
            || after.starts_with(char::is_whitespace);
        let space = if spaced { "" } else { " " };
        let text = match path {
            Some(path) => format!("{before}{space}@{path} {after}"),
            None => format!("{before}{space}@{after}"),
        };
        let caret = match path {
            Some(path) => before.len() + space.len() + 1 + path.len() + 1,
            None => before.len() + space.len() + 1,
        };
        self.input.update(cx, |state, cx| {
            state.set_value(text.as_str(), window, cx);
            state.set_selected_range(caret..caret, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// Fills the input with the slash seed, as the plus menu's
    /// commands entry does.
    fn seed_slash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| {
            state.set_value("/", window, cx);
            state.set_selected_range(1..1, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// The slash or mention rows the current text points at, on a
    /// popover surface above the composer.
    fn suggestion_panel(&self, cx: &Context<Self>, pal: &'static Palette) -> Option<AnyElement> {
        let theme = cx.theme().colors;
        let value = self.input_value(cx);
        let cursor = self.input_cursor(cx);
        let this = cx.entity();
        match suggest_for(&value, cursor)? {
            Suggest::Slash { query } => {
                let items = slash_items(self.store.read(cx).composer_commands(), &query);
                if items.is_empty() {
                    return None;
                }
                let mut panel = suggest_surface("slash-menu", pal);
                for item in items {
                    let this = this.clone();
                    let name = item.name.clone();
                    let mut row = suggest_row(
                        SharedString::from(format!("slash-{}", item.name)),
                        if item.badge.is_none() {
                            IconName::Server
                        } else {
                            IconName::Slash
                        },
                        pal,
                    )
                    .on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| this.pick_slash(&name, window, cx));
                    })
                    .child(
                        v_flex()
                            .min_w_0()
                            .flex_1()
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .font_family(FONT_MONO)
                                            .text_size(px(12.5))
                                            .text_color(theme.foreground)
                                            .child(SharedString::from(format!("/{}", item.name))),
                                    )
                                    .children(item.hint.clone().map(|hint| {
                                        div()
                                            .font_family(FONT_MONO)
                                            .text_size(px(11.))
                                            .text_color(pal.faint)
                                            .child(SharedString::from(hint))
                                    })),
                            )
                            .children((!item.description.is_empty()).then(|| {
                                div()
                                    .text_size(px(FS_XS))
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(SharedString::from(item.description.clone()))
                            })),
                    );
                    if let Some(tag) = item.badge {
                        row = row.child(badge(tag, pal).h(px(18.)).flex().items_center());
                    }
                    panel = panel.child(row);
                }
                Some(panel.into_any_element())
            }
            Suggest::Mention { query, at } => {
                let loaded = self.loaded.clone();
                let listing = loaded
                    .as_deref()
                    .and_then(|id| self.store.read(cx).fs_listing(id));
                let items = mention_items(listing, &query);
                let mut panel = suggest_surface("mention-menu", pal);
                if listing.is_none() {
                    panel = panel.child(
                        div()
                            .px(px(9.))
                            .py(px(7.))
                            .text_size(px(FS_XS))
                            .text_color(theme.muted_foreground)
                            .child("no file listing yet; the session workdir was asked"),
                    );
                } else if items.is_empty() {
                    return None;
                } else {
                    for item in items {
                        let this = this.clone();
                        let path = item.path.clone();
                        panel = panel.child(
                            suggest_row(
                                SharedString::from(format!("mention-{}", item.path)),
                                IconName::File,
                                pal,
                            )
                            .on_click(move |_, window, cx| {
                                this.update(cx, |this, cx| {
                                    this.pick_mention(at, &path, window, cx);
                                });
                            })
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .font_family(FONT_MONO)
                                    .text_size(px(12.5))
                                    .text_color(theme.foreground)
                                    .truncate()
                                    .child(SharedString::from(item.path.clone())),
                            )
                            .child(
                                badge(if item.directory { "dir" } else { "file" }, pal)
                                    .h(px(18.))
                                    .flex()
                                    .items_center(),
                            ),
                        );
                    }
                }
                Some(panel.into_any_element())
            }
        }
    }

    /// The interrupt hint, while the window is open.
    fn esc_hint(&self, cx: &Context<Self>) -> Option<Div> {
        let armed = self.esc_armed?;
        if armed.elapsed() >= ESC_WINDOW {
            return None;
        }
        let theme = cx.theme().colors;
        Some(
            div()
                .text_size(px(11.))
                .text_color(theme.warning)
                .child("Press Esc again to interrupt"),
        )
    }

    /// The line under the input: the key help as key caps, or the
    /// interrupt hint, with the session cost at the far end.
    fn hint_line(&self, pal: &'static Palette, cx: &Context<Self>) -> Div {
        let running = self.running(cx);
        let steer = self.store.read(cx).state().steer_available();
        let left = if let Some(hint) = self.esc_hint(cx) {
            hint.into_any_element()
        } else if !self.store.read(cx).prefs().enter_sends {
            h_flex()
                .flex_wrap()
                .items_center()
                .child(kbd("Ctrl Enter", pal))
                .child(hint_word(if running { "queue" } else { "send" }))
                .child(kbd("Enter", pal))
                .child(hint_word("newline"))
                .into_any_element()
        } else if running {
            let mut row = h_flex()
                .flex_wrap()
                .items_center()
                .child(kbd("Enter", pal))
                .child(hint_word("queue"))
                .when(steer, |row| {
                    row.child(kbd("Ctrl Enter", pal)).child(hint_word("steer"))
                })
                .child(kbd("Esc Esc", pal))
                .child(hint_word("interrupt"));
            if !steer {
                row = row.child(div().ml(px(6.)).child("steering not advertised"));
            }
            row.into_any_element()
        } else {
            h_flex()
                .flex_wrap()
                .items_center()
                .child(kbd("Enter", pal))
                .child(hint_word("send"))
                .child(kbd("Shift Enter", pal))
                .child(hint_word("newline"))
                .into_any_element()
        };
        let store = self.store.read(cx);
        let cost = store
            .active_id()
            .and_then(|id| store.tree_cost(id))
            .map(|cost| format!("{} {:.4}", cost.currency, cost.amount))
            .unwrap_or_default();
        h_flex()
            .w_full()
            .justify_between()
            .items_start()
            .px(px(6.))
            .pt(px(6.))
            .min_h(px(22.))
            .text_size(px(FS_2XS))
            .text_color(pal.faint)
            .child(left)
            .child(div().child(SharedString::from(cost)))
    }

    /// The plus popover: attach, mention, commands, goal, plan and
    /// swarm, as sections of rows over the shared config options.
    ///
    /// Goal opens its dialog, and turning swarm mode on asks first.
    fn plus_button(&self, cx: &Context<Self>, pal: &'static Palette) -> AnyElement {
        let this = cx.entity();
        let store = self.store.clone();
        let dialog = self.dialog.clone();
        let goal = session_goal(self.store.read(cx).active_session());
        let plan_on = self.store.read(cx).plan_on();
        let swarm_on = self.store.read(cx).swarm_on();
        let swarm_known = self.store.read(cx).composer_option("swarm").is_some();

        let close = |this: &Entity<Self>, cx: &mut App| {
            this.update(cx, |this, cx| {
                this.plus_open = false;
                cx.notify();
            });
        };

        let track_open = this.clone();
        Popover::new("composer-plus")
            .trigger(
                toolbar_pill("composer-add", self.plus_open, pal, cx)
                    .w(px(30.))
                    .px(px(0.))
                    .justify_center()
                    .tooltip(|window, cx| Tooltip::new("Add").build(window, cx))
                    .child(Icon::new(IconName::Plus).with_size(px(16.))),
            )
            .anchor(Anchor::BottomLeft)
            .open(self.plus_open)
            .rounded(px(R_LG))
            .on_open_change(move |open, _, cx| {
                let open = *open;
                track_open.update(cx, |this, cx| {
                    this.plus_open = open;
                    cx.notify();
                });
            })
            .content(move |_, _, _| {
                let menu = v_flex()
                    .w(px(440.))
                    .child(pop_label("Attach", pal))
                    .child(
                        add_row("plus-files", IconName::Paperclip, pal)
                            .opacity(0.45)
                            .tooltip(|window, cx| {
                                Tooltip::new(
                                    "the prompt surface carries text only for now; \
                                     attachments are a later story",
                                )
                                .build(window, cx)
                            })
                            .child(plus_row_body(
                                "Files",
                                "Upload files or images",
                                Some(add_kbd("drop", pal)),
                                pal,
                            )),
                    )
                    .child(
                        add_row("plus-mention", IconName::AtSign, pal)
                            .on_click({
                                let this = this.clone();
                                move |_, window, cx| {
                                    close(&this, cx);
                                    this.update(cx, |this, cx| {
                                        this.seed_mention(window, cx);
                                    });
                                }
                            })
                            .child(plus_row_body(
                                "Mention",
                                "Project files from the session workdir",
                                Some(add_kbd("@", pal)),
                                pal,
                            )),
                    )
                    .child(
                        add_row("plus-commands", IconName::Slash, pal)
                            .on_click({
                                let this = this.clone();
                                move |_, window, cx| {
                                    close(&this, cx);
                                    this.update(cx, |this, cx| this.seed_slash(window, cx));
                                }
                            })
                            .child(plus_row_body(
                                "Commands",
                                "The commands the agent sent",
                                Some(add_kbd("/", pal)),
                                pal,
                            )),
                    )
                    .child(pop_label("Modes", pal))
                    .child(
                        add_row("plus-goal", IconName::Target, pal)
                            .on_click({
                                let this = this.clone();
                                let dialog = dialog.clone();
                                move |_, window, cx| {
                                    close(&this, cx);
                                    dialog.update(cx, |dialog, cx| {
                                        dialog.open(DialogKind::Goal, window, cx);
                                    });
                                }
                            })
                            .child(plus_row_body(
                                "Goal",
                                "Set a goal to keep pursuing",
                                Some(add_kbd(if goal.is_some() { "set" } else { "none" }, pal)),
                                pal,
                            )),
                    )
                    .child(
                        add_row("plus-plan", IconName::PenLine, pal)
                            .on_click({
                                let this = this.clone();
                                let store = store.clone();
                                move |_, _, cx| {
                                    close(&this, cx);
                                    store.act(cx, |store| {
                                        if plan_on {
                                            store.exit_plan()
                                        } else {
                                            store.enter_plan()
                                        }
                                    });
                                }
                            })
                            .child(plus_row_body(
                                "Plan",
                                if plan_on {
                                    "Turn plan mode off"
                                } else {
                                    "Turn plan mode on"
                                },
                                Some(onoff(plan_on, pal)),
                                pal,
                            )),
                    );
                if swarm_known {
                    menu.child(
                        add_row("plus-swarm", IconName::Waypoints, pal)
                            .on_click({
                                let this = this.clone();
                                let store = store.clone();
                                let dialog = dialog.clone();
                                move |_, window, cx| {
                                    close(&this, cx);
                                    if swarm_on {
                                        store.act(cx, |store| store.set_option("swarm", "off"));
                                    } else {
                                        dialog.update(cx, |dialog, cx| {
                                            dialog.open(DialogKind::ConfirmSwarm, window, cx);
                                        });
                                    }
                                }
                            })
                            .child(plus_row_body(
                                "Swarm",
                                if swarm_on {
                                    "Turn swarm mode off"
                                } else {
                                    "Turn swarm mode on"
                                },
                                Some(onoff(swarm_on, pal)),
                                pal,
                            )),
                    )
                } else {
                    menu.child(
                        add_row("plus-swarm", IconName::Waypoints, pal)
                            .opacity(0.45)
                            .tooltip(|window, cx| {
                                Tooltip::new("the agent sent no swarm config option")
                                    .build(window, cx)
                            })
                            .child(plus_row_body(
                                "Swarm",
                                "The agent sent no swarm option",
                                None,
                                pal,
                            )),
                    )
                }
            })
            .into_any_element()
    }

    /// The permission mode pill, listing exactly the advertised
    /// values with the active one marked.
    fn mode_button(&self, cx: &Context<Self>, pal: &'static Palette) -> AnyElement {
        let Some(option) = self.store.read(cx).composer_option("mode") else {
            return toolbar_pill("composer-mode", false, pal, cx)
                .opacity(0.45)
                .cursor_default()
                .tooltip(|window, cx| {
                    Tooltip::new("the agent sent no mode config option").build(window, cx)
                })
                .child("Mode")
                .into_any_element();
        };
        let values: Vec<SessionConfigSelectOption> = option
            .options
            .iter()
            .filter(|value| value.value != PLAN_MODE)
            .cloned()
            .collect();
        let active = self
            .store
            .read(cx)
            .permission_mode()
            .unwrap_or_else(|| option.current_value.clone());
        let label = values
            .iter()
            .find(|value| value.value == active)
            .map(|value| value.name.clone())
            .unwrap_or_else(|| active.clone());
        let tone = mode_tone(&active, pal);
        let mut pill = toolbar_pill("composer-mode", self.mode_open, pal, cx)
            .tooltip(|window, cx| {
                Tooltip::new("Permission mode (Shift+Tab cycles)").build(window, cx)
            })
            .child(Icon::new(mode_icon(&active)).with_size(px(14.)))
            .child(SharedString::from(label))
            .child(
                Icon::new(IconName::ChevronDown)
                    .with_size(px(12.))
                    .opacity(0.7),
            );
        if let Some(tone) = tone {
            pill = pill.text_color(tone);
        }
        let bounds = self.mode_bounds.clone();
        let pill = pill
            .on_prepaint(move |at, _, _| bounds.set(Some(at)))
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_picker(Picker::Mode, window, cx);
            }));
        let overlay = self.mode_bounds.get().filter(|_| self.mode_open).map(|at| {
            picker_overlay(
                cx.entity(),
                self.mode_picker.clone().into(),
                at,
                Anchor::BottomLeft,
                pal,
            )
        });
        div().child(pill).children(overlay).into_any_element()
    }

    /// One active-mode chip: the colored pill with its exit button.
    #[allow(clippy::too_many_arguments)]
    fn mode_chip(
        &self,
        id: &'static str,
        icon: IconName,
        label: &'static str,
        option: &'static str,
        off: &'static str,
        fg: Hsla,
        bg: Hsla,
        pal: &'static Palette,
    ) -> Stateful<Div> {
        let store = self.store.clone();
        let fill_hover = pal.fill_hover;
        h_flex()
            .id(id)
            .h(px(26.))
            .pl(px(9.))
            .pr(px(4.))
            .gap(px(5.))
            .flex_none()
            .items_center()
            .rounded(px(R_FULL))
            .bg(bg)
            .font_weight(WEIGHT_SEMIBOLD)
            .text_size(px(FS_XS))
            .text_color(fg)
            .child(Icon::new(icon).with_size(px(12.)))
            .child(label)
            .child(
                div()
                    .id(SharedString::from(format!("{id}-exit")))
                    .size(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(R_FULL))
                    .opacity(0.7)
                    .hover(move |style| style.bg(fill_hover).opacity(1.))
                    .on_click(move |_, _, cx| {
                        store.act(cx, |store| {
                            if option == "mode" {
                                store.exit_plan()
                            } else {
                                store.set_option(option, off)
                            }
                        });
                    })
                    .child(Icon::new(IconName::X).with_size(px(12.))),
            )
    }

    /// The model and thinking pill: one control, as the web client's
    /// model button draws it, naming the model's short label and the
    /// thinking level it runs at. The menu lists the model options and
    /// the thinking options the agent advertised; with neither the pill
    /// dims and names what is missing.
    fn model_button(&self, cx: &Context<Self>, pal: &'static Palette) -> AnyElement {
        let model = self.store.read(cx).composer_option("model");
        let thinking = self.store.read(cx).composer_option("thinking");
        if model.is_none() && thinking.is_none() {
            return toolbar_pill("composer-model", false, pal, cx)
                .opacity(0.45)
                .cursor_default()
                .tooltip(|window, cx| {
                    Tooltip::new("the agent sent no model or thinking config option")
                        .build(window, cx)
                })
                .child("Model")
                .into_any_element();
        }
        let model_label = model
            .as_ref()
            .map(|option| {
                option
                    .options
                    .iter()
                    .find(|value| value.value == option.current_value)
                    .map(|value| value.name.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| {
                        option
                            .current_value
                            .rsplit('/')
                            .next()
                            .unwrap_or(&option.current_value)
                            .to_owned()
                    })
            })
            .unwrap_or_else(|| "default".to_owned());
        // The level label trails a middle dot and drops when thinking
        // is off, as the web client's label does.
        let think_label = thinking
            .as_ref()
            .filter(|option| option.current_value != "off")
            .map(|option| {
                option
                    .options
                    .iter()
                    .find(|value| value.value == option.current_value)
                    .map(|value| value.name.clone())
                    .unwrap_or_else(|| option.current_value.clone())
            });
        let mut pill = toolbar_pill("composer-model", self.model_open, pal, cx)
            .tooltip(|window, cx| Tooltip::new("Model and thinking").build(window, cx))
            .child(SharedString::from(model_label));
        if let Some(level) = think_label {
            pill = pill
                .child(
                    div()
                        .text_color(pal.faint)
                        .child(SharedString::from(format!("\u{b7} {level}"))),
                )
                .text_color(pal.ink);
        }
        pill = pill.child(
            Icon::new(IconName::ChevronDown)
                .with_size(px(12.))
                .opacity(0.7),
        );
        let bounds = self.model_bounds.clone();
        let pill = pill
            .on_prepaint(move |at, _, _| bounds.set(Some(at)))
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_picker(Picker::Model, window, cx);
            }));
        let overlay = self
            .model_bounds
            .get()
            .filter(|_| self.model_open)
            .map(|at| {
                picker_overlay(
                    cx.entity(),
                    self.model_picker.clone().into(),
                    at,
                    Anchor::BottomRight,
                    pal,
                )
            });
        div().child(pill).children(overlay).into_any_element()
    }

    /// Opens the `which` picker over its pill, closing the other, or
    /// closes it when it shows.
    fn toggle_picker(&mut self, which: Picker, window: &mut Window, cx: &mut Context<Self>) {
        let opening = match which {
            Picker::Mode => !self.mode_open,
            Picker::Model => !self.model_open,
        };
        self.close_pickers(window, cx);
        if !opening {
            return;
        }
        let handle = match which {
            Picker::Mode => {
                self.mode_open = true;
                self.mode_picker
                    .update(cx, |picker, cx| picker.open(window, cx));
                self.mode_picker.read(cx).focus_handle(cx)
            }
            Picker::Model => {
                self.model_open = true;
                self.model_picker
                    .update(cx, |picker, cx| picker.open(window, cx));
                self.model_picker.read(cx).focus_handle(cx)
            }
        };
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Closes both pickers and gives the composer the keys back.
    fn close_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.mode_open && !self.model_open {
            return;
        }
        self.mode_open = false;
        self.model_open = false;
        self.input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// The context ring with its percent, shown only while a session
    /// exists, as the web client's `ring-wrap` draws it. The ring turns
    /// warn at 80 percent, where the engine compacts on its own. With
    /// the Lab context gauge on, a click opens the gauge.
    fn fuel(&self, cx: &Context<Self>, pal: &'static Palette) -> Option<AnyElement> {
        let session = self.store.read(cx).active_session()?;
        let fill = session.usage.fill();
        let (used, size) = (session.usage.used, session.usage.size);
        let percent = (fill * 100.0).round() as i64;
        let ring = if fill >= COMPACT_AT {
            pal.warn
        } else {
            pal.accent
        };
        let label = SharedString::from(format!("{percent}%"));
        let circle = ProgressCircle::new("ring")
            .value(fill as f32 * 100.0)
            .color(ring)
            .with_size(px(22.));
        if !self.store.read(cx).prefs().fuel {
            let tip: SharedString = if size > 0 {
                format!(
                    "Context: {} of {} tokens ({percent}%)",
                    tokens(used),
                    tokens(size)
                )
                .into()
            } else {
                "Context: the agent sent no usage yet".into()
            };
            let (faint, ink, hover) = (pal.faint, pal.ink, pal.hover);
            return Some(
                h_flex()
                    .id("composer-fuel")
                    .h(px(30.))
                    .px(px(6.))
                    .gap(px(6.))
                    .flex_none()
                    .items_center()
                    .rounded(px(R_FULL))
                    .font_family(FONT_MONO)
                    .text_size(px(FS_2XS))
                    .text_color(faint)
                    .hover(move |style| style.bg(hover).text_color(ink))
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .child(circle)
                    .child(label)
                    .into_any_element(),
            );
        }
        let gauge = Gauge {
            used,
            size,
            cost: self
                .store
                .read(cx)
                .active_id()
                .and_then(|id| self.store.read(cx).tree_cost(id))
                .map(|cost| format!("{} {:.2}", cost.currency, cost.amount)),
            model: select_option(session, "model").and_then(|option| {
                option
                    .options
                    .iter()
                    .find(|choice| choice.value == option.current_value)
                    .map(|choice| choice.name.clone())
            }),
            turns: session
                .items
                .iter()
                .filter(|item| matches!(item, kage_client::TranscriptItem::User { .. }))
                .count(),
            running: self.running(cx),
        };
        let store = self.store.clone();
        Some(
            Popover::new("composer-gauge")
                .trigger(
                    toolbar_pill("composer-fuel", false, pal, cx)
                        .px(px(6.))
                        .font_family(FONT_MONO)
                        .text_size(px(FS_2XS))
                        .tooltip(|window, cx| Tooltip::new("Context").build(window, cx))
                        .child(circle)
                        .child(label),
                )
                .anchor(Anchor::BottomRight)
                .rounded(px(R_LG))
                .content(move |_, _, _| gauge.render(&store, pal))
                .into_any_element(),
        )
    }

    /// Compact, offered in the toolbar once the context passes the
    /// 80 percent the engine compacts at, while no turn runs.
    fn compact_button(&self, cx: &Context<Self>, pal: &'static Palette) -> Option<Pill> {
        let session = self.store.read(cx).active_session()?;
        if session.usage.fill() < COMPACT_AT || self.running(cx) {
            return None;
        }
        let store = self.store.clone();
        Some(
            toolbar_pill("composer-compact", false, pal, cx)
                .text_color(pal.warn)
                .tooltip(|window, cx| {
                    Tooltip::new("Summarize older turns to free context").build(window, cx)
                })
                .on_click(move |_, _, cx| {
                    store.act(cx, Store::compact);
                })
                .child(Icon::new(IconName::Layers).with_size(px(14.)))
                .child("Compact"),
        )
    }

    /// The send and stop pair: a 32px circle that interrupts while a
    /// run is in flight and the input is empty, and sends otherwise,
    /// graying out with no shadow when there is nothing to send. The
    /// click queues while a run is in flight, as Enter does.
    fn send_button(&self, cx: &Context<Self>, pal: &'static Palette) -> AnyElement {
        let theme = cx.theme().colors;
        let running = self.running(cx);
        let has_text = !self.input_value(cx).trim().is_empty();
        if running && !has_text {
            let store = self.store.clone();
            let (hover_bg, hover_fg) = (pal.danger_soft, theme.danger);
            div()
                .id("composer-stop")
                .size(px(32.))
                .ml(px(4.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(R_FULL))
                .bg(pal.fill_hover)
                .text_color(theme.secondary_foreground)
                .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
                .tooltip(|window, cx| Tooltip::new("Interrupt (Esc Esc)").build(window, cx))
                .child(Icon::new(IconName::Square).with_size(px(14.)))
                .on_click(move |_, _, cx| {
                    store.update(cx, |store, cx| {
                        store.cancel();
                        cx.notify();
                    });
                })
                .into_any_element()
        } else {
            let (bg, fg) = if has_text {
                (pal.send_bg, pal.send_icon)
            } else {
                (pal.send_bg_off, pal.send_icon_off)
            };
            let hover_bg = pal.send_bg_hover;
            let mut send = div()
                .id("composer-send")
                .size(px(32.))
                .ml(px(4.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(R_FULL))
                .bg(bg)
                .text_color(fg)
                .tooltip(move |window, cx| {
                    Tooltip::new(if running {
                        "Queue (Enter) or steer (Ctrl+Enter)"
                    } else {
                        "Send (Enter)"
                    })
                    .build(window, cx)
                })
                .child(Icon::new(IconName::ArrowUp).with_size(px(14.)));
            if has_text {
                send = send
                    .hover(move |style| style.bg(hover_bg))
                    .shadow(pal.shadow_send.clone())
                    .on_click({
                        let this = cx.entity();
                        move |_, window, cx| {
                            this.update(cx, |this, cx| this.submit(false, window, cx));
                        }
                    });
            } else {
                send = send.cursor_default();
            }
            send.into_any_element()
        }
    }

    /// The toolbar row under the input.
    fn toolbar(&self, cx: &Context<Self>, pal: &'static Palette) -> Div {
        let plan_on = self.store.read(cx).plan_on();
        let swarm_on = self.store.read(cx).swarm_on();
        h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .gap(px(SP_2))
            .pt(px(6.))
            .px(px(8.))
            .pb(px(8.))
            .child(self.plus_button(cx, pal))
            .child(self.mode_button(cx, pal))
            .when(plan_on, |row| {
                row.child(self.mode_chip(
                    "composer-plan-chip",
                    IconName::PenLine,
                    "Plan",
                    "mode",
                    "default",
                    pal.accent,
                    pal.accent_soft,
                    pal,
                ))
            })
            .when(swarm_on, |row| {
                row.child(self.mode_chip(
                    "composer-swarm-chip",
                    IconName::Waypoints,
                    "Swarm",
                    "swarm",
                    "off",
                    pal.done,
                    pal.done_soft,
                    pal,
                ))
            })
            .child(div().flex_1().min_w(px(4.)))
            .children(self.compact_button(cx, pal))
            .children(self.fuel(cx, pal))
            .child(self.model_button(cx, pal))
            .child(self.send_button(cx, pal))
    }
}

impl Render for ComposerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A draft the store asked for before the textarea could take it
        // lands here, on the first render after the textarea's element
        // prepainted, so the text does not wait on further traffic.
        let input = self.input.clone();
        self.draft_mirror.flush(|draft| {
            input.update(cx, |state, cx| state.set_value(draft, window, cx));
        });
        let draft_laid_out = self.draft_mirror.laid_out().flag();
        let release = cx.entity().downgrade();
        let theme = cx.theme().colors;
        let pal = Palette::active(cx);
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let plan_on = self.store.read(cx).plan_on();
        let swarm_on = self.store.read(cx).swarm_on();
        let border = if plan_on {
            pal.accent_bd
        } else if swarm_on {
            pal.done_bd
        } else if focused {
            pal.composer_focus_line
        } else {
            pal.composer_line
        };
        let suggestion = self.suggestion_panel(cx, pal);
        v_flex()
            .id("composer")
            .test_support()
            .w_full()
            .on_action(cx.listener(|this, _: &CycleMode, _, cx| this.cycle_mode(cx)))
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                // In vim mode Esc leaves the composer for normal mode,
                // where a second Esc interrupts a running turn.
                if this.store.read(cx).prefs().vim && this.esc_armed.is_none() {
                    window.dispatch_action(Box::new(crate::views::vim::VimLeaveInsert), cx);
                    return;
                }
                this.on_escape(cx);
            }))
            .children(suggestion)
            .child(
                v_flex()
                    .w_full()
                    .overflow_hidden()
                    .bg(pal.composer_bg)
                    .border_1()
                    .border_color(border)
                    .rounded(px(R_COMPOSER))
                    .shadow(pal.shadow_input.clone())
                    .child(
                        div()
                            .on_prepaint({
                                let flag = draft_laid_out.clone();
                                move |_, _, cx| {
                                    flag.set(true);
                                    let _ = release.update(cx, |_, cx| cx.notify());
                                }
                            })
                            .child(
                                Textarea::new(&self.input)
                                    .token(move |token, _, _| {
                                        let chip = InputToken::new(token);
                                        if token.is_selected() {
                                            chip
                                        } else {
                                            chip.text_color(pal.accent)
                                                .bg(pal.accent_soft)
                                                .border_color(pal.accent_bd)
                                        }
                                    })
                                    .appearance(false)
                                    .pt(px(14.))
                                    .px(px(16.))
                                    .pb(px(4.))
                                    .min_h(px(60.))
                                    .text_size(px(FS_BASE))
                                    .text_color(theme.secondary_foreground)
                                    .line_height(relative(LINE_HEIGHT)),
                            ),
                    )
                    .child(self.toolbar(cx, pal)),
            )
            .child(self.hint_line(pal, cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, Entity, TestAppContext};

    use super::{
        ComposerView, ESC_WINDOW, EscStep, Suggest, active_mode, command_len, esc_step,
        mention_items, next_mode_value, select_option, slash_items, suggest_for,
    };
    use crate::store::Store;
    use crate::transport::State;
    use kage_client::Frame;

    /// A store with a session, so the composer has a draft to follow.
    fn store_with_session(cx: &mut TestAppContext) -> Entity<Store> {
        let store = cx.new(|_| Store::new("/tmp", false));
        store.update(cx, |store, cx| {
            store.set_connect(State::Connected);
            store.handshake(false);
            store.new_session();
            cx.notify();
        });
        store
    }

    /// A store change carrying a draft reaches the composer, and the
    /// composer hands it to the textarea.
    ///
    /// The hold in [`crate::views::deferred`] is what makes that safe on
    /// the web, where the family the engine captured at construction has
    /// no installed fallback and resolving it takes the frame down. This
    /// harness always draws a frame while opening a window, so the write
    /// lands immediately here and only the landing half is observable;
    /// [`crate::views::deferred`] covers the hold itself, and the browser
    /// run in `SPIKE.md` covers the pair.
    #[gpui_kit::test]
    fn a_store_draft_reaches_the_textarea(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let store = store_with_session(cx);
        let composer = cx.update(|app| {
            gpui_kit::open_window(Default::default(), app, |window, cx| {
                cx.new(|cx| {
                    ComposerView::new(
                        store.clone(),
                        cx.new(|cx| {
                            crate::views::dialog::DialogView::new(store.clone(), window, cx)
                        }),
                        window,
                        cx,
                    )
                })
            })
            .expect("the window opens")
            .1
        });

        // A second session, opened the way the engine opens one, carrying
        // a draft: the change that has to reach the textarea.
        cx.update(|app| {
            store.update(app, |store, cx| {
                store.new_session();
                let request = store
                    .take_outgoing()
                    .into_iter()
                    .rev()
                    .find_map(|frame| match frame {
                        Frame::Request { id, method, .. } if method == "session/new" => Some(id),
                        _ => None,
                    })
                    .expect("session/new is among the frames the store made");
                store.absorb(Frame::Success {
                    id: request,
                    result: serde_json::json!({ "sessionId": "session-2" }),
                });
                store.set_draft(Some("session-2"), "carried across");
                cx.notify();
            });
        });

        let mirrored = cx.update(|app| composer.read(app).input().read(app).value().to_owned());
        assert_eq!(
            mirrored, "carried across",
            "the active session's draft is mirrored"
        );
        let followed = cx.update(|app| composer.read(app).loaded.clone());
        assert_eq!(
            followed.as_deref(),
            Some("session-2"),
            "the composer follows the session the store activated"
        );
    }

    /// Ctrl+Enter steers a running turn; with nothing running there is
    /// no turn to steer, so the text goes out as a plain prompt.
    #[gpui_kit::test]
    fn a_steer_with_nothing_running_sends_the_prompt(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let store = store_with_session(cx);
        let (handle, composer) = cx.update(|app| {
            gpui_kit::open_window(Default::default(), app, |window, cx| {
                cx.new(|cx| {
                    ComposerView::new(
                        store.clone(),
                        cx.new(|cx| {
                            crate::views::dialog::DialogView::new(store.clone(), window, cx)
                        }),
                        window,
                        cx,
                    )
                })
            })
            .expect("the window opens")
        });
        cx.update(|app| {
            store.update(app, |store, cx| {
                let request = store
                    .take_outgoing()
                    .into_iter()
                    .find_map(|frame| match frame {
                        Frame::Request { id, method, .. } if method == "session/new" => Some(id),
                        _ => None,
                    })
                    .expect("session/new went out");
                store.absorb(Frame::Success {
                    id: request,
                    result: serde_json::json!({ "sessionId": "s1" }),
                });
                cx.notify();
            });
        });
        let _ = handle.update(cx, |_, window, app| {
            composer.update(app, |composer, cx| {
                composer
                    .input()
                    .update(cx, |state, cx| state.set_value("go", window, cx));
                composer.submit(true, window, cx);
            });
        });
        let sent = cx.update(|app| {
            store.update(app, |store, _| {
                store.take_outgoing().into_iter().any(
                    |frame| matches!(frame, Frame::Request { method, .. } if method == "session/prompt"),
                )
            })
        });
        assert!(sent, "the prompt went out");
    }

    /// A built-in that needs a session keeps its text and sends
    /// nothing; `/new` still runs on the welcome pane.
    #[gpui_kit::test]
    fn a_builtin_that_needs_a_session_keeps_its_text(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let store = cx.new(|_| Store::new("/tmp", false));
        let (handle, composer) = cx.update(|app| {
            gpui_kit::open_window(Default::default(), app, |window, cx| {
                cx.new(|cx| {
                    ComposerView::new(
                        store.clone(),
                        cx.new(|cx| {
                            crate::views::dialog::DialogView::new(store.clone(), window, cx)
                        }),
                        window,
                        cx,
                    )
                })
            })
            .expect("the window opens")
        });
        let type_and_submit = |text: &'static str, cx: &mut TestAppContext| {
            let _ = handle.update(cx, |_, window, app| {
                composer.update(app, |composer, cx| {
                    composer
                        .input()
                        .update(cx, |state, cx| state.set_value(text, window, cx));
                    composer.submit(false, window, cx);
                });
            });
        };

        type_and_submit("/swarm on", cx);
        let value = cx.update(|app| composer.read(app).input().read(app).value().to_owned());
        assert_eq!(
            value, "/swarm on",
            "a builtin that needs a session keeps its text"
        );
        let sent = cx.update(|app| store.update(app, |store, _| store.take_outgoing()));
        assert!(
            sent.is_empty(),
            "a builtin that needs a session sends nothing"
        );
        let notes = cx.update(|app| store.update(app, |store, _| store.take_notes()));
        assert!(
            matches!(notes.as_slice(), [note] if note.tone == NoticeTone::Info),
            "the composer explains why nothing was sent"
        );

        type_and_submit("/new", cx);
        let value = cx.update(|app| composer.read(app).input().read(app).value().to_owned());
        assert_eq!(
            value, "",
            "/new runs without a session and clears the input"
        );
        let welcome = cx.update(|app| store.read(app).active_id().is_none());
        assert!(welcome, "/new lands on the welcome pane");
        let sent = cx.update(|app| store.update(app, |store, _| store.take_outgoing()));
        assert!(sent.is_empty(), "/new sends no frame on an empty store");
    }

    use std::time::{Duration, Instant};

    use kage_client::Session;
    use kage_client::wire::{
        FsEntry, FsKind, FsListResult, NoticeTone, SessionConfigKind, SessionConfigOption,
        SessionConfigSelectOption,
    };

    fn mode_session(current: &str, marked: Option<&str>, values: &[&str]) -> Session {
        let mut session = Session::new("s1");
        session.config_options = vec![SessionConfigOption {
            id: "mode".into(),
            name: "Mode".into(),
            description: None,
            category: None,
            kind: SessionConfigKind::Select,
            current_value: current.into(),
            options: values
                .iter()
                .map(|value| SessionConfigSelectOption {
                    value: (*value).into(),
                    name: (*value).into(),
                    description: None,
                })
                .collect(),
        }];
        session.mode = marked.map(str::to_owned);
        session
    }

    #[test]
    fn esc_steps_through_the_interrupt_window() {
        let start = Instant::now();
        assert_eq!(esc_step(None, start, false), EscStep::Clear, "idle clears");
        assert_eq!(esc_step(Some(start), start, false), EscStep::Clear);
        assert_eq!(esc_step(None, start, true), EscStep::Arm, "running arms");
        assert_eq!(
            esc_step(Some(start), start + Duration::from_millis(100), true),
            EscStep::Cancel,
            "the second press inside the window cancels"
        );
        assert_eq!(
            esc_step(
                Some(start),
                start + ESC_WINDOW + Duration::from_millis(1),
                true
            ),
            EscStep::Arm,
            "an expired window arms again"
        );
    }

    #[test]
    fn suggestions_follow_the_caret() {
        assert_eq!(
            suggest_for("/fix", 4),
            Some(Suggest::Slash {
                query: "fix".into()
            })
        );
        assert_eq!(suggest_for("/fix done", 8), None, "a space ends the slash");
        assert_eq!(
            suggest_for("look @src/m", 11),
            Some(Suggest::Mention {
                query: "src/m".into(),
                at: 5
            })
        );
        assert_eq!(suggest_for("plain text", 10), None);
        assert_eq!(suggest_for("a@b@c", 5), None, "an at must start the token");
    }

    #[test]
    fn built_in_commands_parse_their_name_and_arguments() {
        let parsed = super::builtin("/swarm review each crate").unwrap();
        assert_eq!((parsed.name, parsed.args), ("swarm", "review each crate"));
        let parsed = super::builtin("  /plan on ").unwrap();
        assert_eq!((parsed.name, parsed.args), ("plan", "on"));
        assert_eq!(super::builtin("/new").unwrap().args, "");
        assert!(
            super::builtin("/review").is_none(),
            "agent commands go to the agent"
        );
        assert!(super::builtin("plan on").is_none());
    }

    #[test]
    fn a_command_is_marked_once_a_space_follows_it() {
        let commands = vec![serde_json::json!({"name": "review"})];
        assert_eq!(command_len("/plan on", &[]), Some(5));
        assert_eq!(command_len("/review ", &commands), Some(7));
        assert_eq!(command_len("/plan", &[]), None, "still being typed");
        assert_eq!(command_len("/usr/bin is big", &[]), None, "not a command");
        assert_eq!(command_len(" /plan on", &[]), None);
    }

    #[test]
    fn slash_items_split_agent_and_server_commands() {
        let commands = vec![
            serde_json::json!({
                "name": "fs:list",
                "description": "list a directory",
                "input": {"hint": "<path>"},
            }),
            serde_json::json!({
                "name": "review",
                "description": "review the diff",
            }),
        ];
        let items = slash_items(&commands, "");
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(
            names,
            ["swarm", "plan", "goal", "new", "fs:list", "review"],
            "the client's own commands come first"
        );
        let items = &items[4..];
        assert_eq!(items[0].badge, None, "a colon names a server command");
        assert_eq!(items[0].hint.as_deref(), Some("<path>"));
        assert_eq!(items[1].name, "review");
        assert_eq!(items[1].badge, Some("agent"), "the rest are agent commands");
        assert_eq!(slash_items(&commands, "re").len(), 1, "the query filters");
        assert!(slash_items(&commands, "zz").is_empty());
    }

    #[test]
    fn mention_items_come_from_the_listing_only() {
        let listing = FsListResult {
            entries: vec![
                FsEntry {
                    path: "src".into(),
                    kind: FsKind::Directory,
                    size: 0,
                },
                FsEntry {
                    path: "src/main.rs".into(),
                    kind: FsKind::File,
                    size: 12,
                },
            ],
            truncated: false,
        };
        assert!(mention_items(None, "").is_empty(), "no listing, no rows");
        let items = mention_items(Some(&listing), "main");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "src/main.rs");
        assert!(!items[0].directory);
        let all = mention_items(Some(&listing), "");
        assert_eq!(all.len(), 2);
        assert!(all[0].directory);
    }

    #[test]
    fn mode_cycles_exactly_the_advertised_values() {
        let values = ["default", "ask", "allow", "deny", "plan"];
        let session = mode_session("default", None, &values);
        assert_eq!(
            next_mode_value(&session, "default").as_deref(),
            Some("ask"),
            "the cycle follows the option's order"
        );
        assert_eq!(
            next_mode_value(&session, "deny").as_deref(),
            Some("default"),
            "plan mode is not a permission mode"
        );
        assert_eq!(
            next_mode_value(&session, "mystery").as_deref(),
            Some("default"),
            "an unknown current starts the cycle"
        );
        let session = mode_session("ask", Some("deny"), &values);
        let bare = Session::new("s1");
        assert_eq!(
            next_mode_value(&bare, "default"),
            None,
            "no option, no cycle"
        );
        let empty = mode_session("default", None, &[]);
        assert_eq!(
            next_mode_value(&empty, "default"),
            None,
            "no values, no cycle"
        );
        assert_eq!(
            active_mode(&session),
            Some("deny".to_owned()),
            "the mark wins over the option value"
        );
        assert!(select_option(&session, "mode").is_some());
        assert!(select_option(&session, "goal").is_none());
    }
}
