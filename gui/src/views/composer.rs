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
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AnyElement, App, AppContext as _, Context, Div, Entity, Focusable, FontWeight, Hsla,
    InteractiveElement, Interactivity, IntoElement, ParentElement, Render, SharedString, Stateful,
    StatefulInteractiveElement, StyleRefinement, Styled, Window, div, px, relative,
};
use kage_client::Session;
use kage_client::wire::{FsKind, FsListResult, SessionConfigKind, SessionConfigOption};
use serde_json::Value;

use crate::store::{Store, StoreHandle as _};
use crate::theme::{
    FONT_MONO, FS_2XS, FS_BASE, FS_SM, FS_XS, LINE_HEIGHT, Palette, R_COMPOSER, R_FULL, R_LG, R_MD,
    SP_2, WEIGHT_SEMIBOLD,
};
use crate::views::deferred::{Deferred, LaidOut};
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

/// The small rounded badge some rows carry.
fn badge(label: &str, pal: &Palette) -> Div {
    div()
        .h(px(18.))
        .px(px(7.))
        .flex()
        .flex_none()
        .items_center()
        .rounded(px(R_FULL))
        .border_1()
        .border_color(pal.line)
        .bg(pal.fill)
        .font_weight(FontWeight::MEDIUM)
        .text_size(px(10.5))
        .text_color(pal.muted)
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

/// The icon the permission mode pill shows for one mode id, closest
/// to the design's per-mode glyphs.
fn mode_icon(mode: &str) -> IconName {
    if mode.contains("allow") {
        IconName::ShieldAlert
    } else if mode.contains("deny") || mode.contains("read") {
        IconName::Eye
    } else if mode.contains("ask") || mode.contains("rules") {
        IconName::ShieldQuestionMark
    } else if mode.contains("plan") {
        IconName::PenLine
    } else {
        IconName::Hand
    }
}

/// The text tone one mode id paints with: warn for ask and rules,
/// danger for allow, the accent for read-only, plain otherwise.
fn mode_tone(mode: &str, pal: &Palette) -> Option<Hsla> {
    if mode.contains("allow") {
        Some(pal.danger)
    } else if mode.contains("deny") || mode.contains("read") {
        Some(pal.accent)
    } else if mode.contains("ask") || mode.contains("rules") {
        Some(pal.warn)
    } else {
        None
    }
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

/// The slash menu rows the session's available commands deliver for
/// `query`, in the order the agent listed them.
#[must_use]
pub(crate) fn slash_items(commands: &[Value], query: &str) -> Vec<SlashItem> {
    commands
        .iter()
        .filter_map(|command| {
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
        })
        .collect()
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

/// The value Shift+Tab cycles to: the entry after the active one in
/// the mode option's advertised list, wrapping around. No option or an
/// empty list cycles nowhere.
#[must_use]
pub(crate) fn next_mode_value(session: &Session) -> Option<String> {
    let values = &select_option(session, "mode")?.options;
    if values.is_empty() {
        return None;
    }
    let current = active_mode(session).unwrap_or_default();
    let index = values
        .iter()
        .position(|value| value.value == current)
        .map_or(0, |index| (index + 1) % values.len());
    Some(values[index].value.clone())
}

/// The composer view.
pub struct ComposerView {
    store: Entity<Store>,
    input: Entity<TextareaState>,
    goal_input: Entity<InputState>,
    /// The session the textarea currently mirrors.
    loaded: Option<String>,
    /// When the first Esc of an interrupt gesture landed, while the
    /// window is open.
    esc_armed: Option<Instant>,
    /// Whether the plus popover shows.
    plus_open: bool,
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
    /// The goal mirror, held for the same reason and the same moment.
    goal_mirror: Deferred,
}

impl ComposerView {
    /// A composer following `store`, focused and bound to its keys.
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Ask kage anything, @ to mention, / for commands")
                .auto_grow(1, 13)
                .submit_on_enter(true)
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        let goal_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Set the goal option; empty clears it")
        });
        // A store change may land before the composer has ever been laid
        // out, so the mirror of the active session's draft is held until
        // then rather than written; see [`crate::views::deferred`].
        cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { secondary, shift } => {
                    if !*shift {
                        this.submit(*secondary, window, cx);
                    }
                }
                InputEvent::Change => this.on_input_change(cx),
                InputEvent::Blur => {
                    if this.esc_armed.take().is_some() {
                        cx.notify();
                    }
                }
                InputEvent::Focus => {}
            },
        )
        .detach();
        cx.subscribe_in(
            &goal_input,
            window,
            |this, goal, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = goal.read(cx).value().trim().to_owned();
                    this.set_goal(&text, cx);
                    goal.update(cx, |state, cx| state.set_value("", window, cx));
                }
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
        Self {
            store,
            input,
            goal_input,
            loaded: None,
            esc_armed: None,
            plus_open: false,
            fs_asked: false,
            placeholder: String::new(),
            draft_mirror: Deferred::new(),
            goal_mirror: Deferred::new(),
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
        let goal = active
            .as_deref()
            .and_then(|id| self.store.read(cx).state().session(id))
            .and_then(|session| {
                session
                    .config_options
                    .iter()
                    .find(|option| option.id == "goal")
                    .map(|option| option.current_value.clone())
            })
            .unwrap_or_default();
        self.loaded = active;
        self.esc_armed = None;
        self.fs_asked = false;
        self.plus_open = false;
        let input = self.input.clone();
        self.draft_mirror.set(draft, |draft| {
            input.update(cx, |state, cx| state.set_value(draft, window, cx));
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        let goal_input = self.goal_input.clone();
        self.goal_mirror.set(goal, |goal| {
            goal_input.update(cx, |state, cx| state.set_value(goal, window, cx));
        });
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
        let session = self.store.read(cx).active_session();
        let wanted = if self.running(cx) {
            "Queue a follow-up, or Ctrl+Enter to steer the running turn"
        } else if session
            .and_then(|session| select_option(session, "swarm"))
            .is_some_and(|option| option.current_value == "on")
        {
            "Describe work to split across parallel agents..."
        } else if session
            .and_then(active_mode)
            .is_some_and(|mode| mode == "plan")
        {
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
    /// wire allows it. A steer with no run in flight sends plainly. Accepted text leaves the textarea and the draft.
    pub fn submit(&mut self, steer: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_value(cx);
        let text = text.trim();
        if text.is_empty() {
            return;
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
        let loaded = self.loaded.clone();
        self.store
            .update(cx, |store, _| store.set_draft(loaded.as_deref(), ""));
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.esc_armed = None;
        self.sync_placeholder(window, cx);
        cx.notify();
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

    /// Steps the permission mode to the next advertised value.
    fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            let next = store.active_session().and_then(next_mode_value);
            if let Some(value) = next {
                store.set_option("mode", &value);
            }
            cx.notify();
        });
    }

    /// Sets the goal text option. An empty value clears it.
    fn set_goal(&mut self, text: &str, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.set_option("goal", text);
            cx.notify();
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
                let session = self.store.read(cx).active_session()?;
                let items = slash_items(&session.commands, &query);
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
                        row = row.child(badge(tag, pal));
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
                            .child(badge(if item.directory { "dir" } else { "file" }, pal)),
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
        let cost = self
            .store
            .read(cx)
            .active_session()
            .and_then(|session| session.usage.cost.as_ref())
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
    /// `goal_laid_out` releases the goal mirror: that input mounts only
    /// while this popover is open, so its element's prepaint is the first
    /// moment a goal write can land.
    fn plus_button(
        &self,
        cx: &Context<Self>,
        pal: &'static Palette,
        goal_laid_out: Rc<Cell<bool>>,
    ) -> AnyElement {
        let this = cx.entity();
        let store = self.store.clone();
        let goal_input = self.goal_input.clone();
        let release = cx.entity().downgrade();
        let session = self.store.read(cx).active_session();
        let plan_on = session
            .and_then(active_mode)
            .is_some_and(|mode| mode == "plan");
        let swarm_option = session.and_then(|session| select_option(session, "swarm"));
        let swarm_on = swarm_option.is_some_and(|option| option.current_value == "on");
        let swarm_known = swarm_option.is_some();

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
                    .child(Icon::new(IconName::Plus)),
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
                let goal_laid_out = goal_laid_out.clone();
                let release = release.clone();
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
                        add_row("plus-goal", IconName::Target, pal).child(plus_row_body(
                            "Goal",
                            "Set a goal to keep pursuing",
                            None,
                            pal,
                        )),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .px(px(7.))
                            .pb(px(6.))
                            .on_prepaint(move |_, _, cx| {
                                goal_laid_out.set(true);
                                let _ = release.update(cx, |_, cx| cx.notify());
                            })
                            .child(Input::new(&goal_input).flex_1())
                            .child(Button::new("goal-set").label("Set").xsmall().on_click({
                                let this = this.clone();
                                let goal_input = goal_input.clone();
                                move |_, window, cx| {
                                    let text = goal_input.read(cx).value().trim().to_owned();
                                    this.update(cx, |this, cx| {
                                        this.set_goal(&text, cx);
                                    });
                                    goal_input
                                        .update(cx, |state, cx| state.set_value("", window, cx));
                                }
                            })),
                    )
                    .child(
                        add_row("plus-plan", IconName::PenLine, pal)
                            .on_click({
                                let this = this.clone();
                                let store = store.clone();
                                let target = if plan_on { "default" } else { "plan" };
                                move |_, _, cx| {
                                    close(&this, cx);
                                    store.update(cx, |store, cx| {
                                        store.set_option("mode", target);
                                        cx.notify();
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
                                let target = if swarm_on { "off" } else { "on" };
                                move |_, _, cx| {
                                    close(&this, cx);
                                    store.update(cx, |store, cx| {
                                        store.set_option("swarm", target);
                                        cx.notify();
                                    });
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
        let session = self.store.read(cx).active_session();
        let Some(option) = session.and_then(|session| select_option(session, "mode")) else {
            return toolbar_pill("composer-mode", false, pal, cx)
                .opacity(0.45)
                .cursor_default()
                .tooltip(|window, cx| {
                    Tooltip::new("the agent sent no mode config option").build(window, cx)
                })
                .child("Mode")
                .into_any_element();
        };
        let option = option.clone();
        let active = session.and_then(active_mode).unwrap_or_else(|| {
            option
                .options
                .first()
                .map(|value| value.value.clone())
                .unwrap_or_default()
        });
        let label = option
            .options
            .iter()
            .find(|value| value.value == active)
            .map(|value| value.name.clone())
            .unwrap_or_else(|| active.clone());
        let tone = mode_tone(&active, pal);
        let store = self.store.clone();
        let mut pill = toolbar_pill("composer-mode", false, pal, cx)
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
        pill.dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
            let mut built = menu.label("Permission mode");
            for value in &option.options {
                let store = store.clone();
                let value = value.clone();
                built = built.item(
                    PopupMenuItem::new(value.name.clone())
                        .checked(value.value == active)
                        .on_click(move |_, _, cx| {
                            store.update(cx, |store, cx| {
                                store.set_option("mode", &value.value);
                                cx.notify();
                            });
                        }),
                );
            }
            built
        })
        .into_any_element()
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
                        store.update(cx, |store, cx| {
                            store.set_option(option, off);
                            cx.notify();
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
        let session = self.store.read(cx).active_session();
        let model = session.and_then(|session| select_option(session, "model"));
        let thinking = session.and_then(|session| select_option(session, "thinking"));
        let model = model.cloned();
        let thinking = thinking.cloned();
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
                    .current_value
                    .rsplit('/')
                    .next()
                    .unwrap_or(&option.current_value)
                    .to_owned()
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
        let mut pill = toolbar_pill("composer-model", false, pal, cx)
            .tooltip(|window, cx| Tooltip::new("Model and thinking").build(window, cx))
            .child(SharedString::from(model_label));
        if let Some(level) = think_label {
            pill = pill
                .child(
                    div()
                        .text_color(pal.faint)
                        .child(SharedString::from(format!("· {level}"))),
                )
                .text_color(pal.ink);
        }
        pill = pill.child(
            Icon::new(IconName::ChevronDown)
                .with_size(px(12.))
                .opacity(0.7),
        );
        let store = self.store.clone();
        pill.dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
            let mut built = menu;
            if let Some(model) = &model {
                built = built.label("Model");
                for value in &model.options {
                    let store = store.clone();
                    let value = value.clone();
                    built = built.item(
                        PopupMenuItem::new(value.name.clone())
                            .checked(value.value == model.current_value)
                            .on_click(move |_, _, cx| {
                                store.update(cx, |store, cx| {
                                    store.set_option("model", &value.value);
                                    cx.notify();
                                });
                            }),
                    );
                }
            }
            if let Some(thinking) = &thinking {
                built = built.label("Thinking");
                for value in &thinking.options {
                    let store = store.clone();
                    let value = value.clone();
                    built = built.item(
                        PopupMenuItem::new(value.name.clone())
                            .checked(value.value == thinking.current_value)
                            .on_click(move |_, _, cx| {
                                store.update(cx, |store, cx| {
                                    store.set_option("thinking", &value.value);
                                    cx.notify();
                                });
                            }),
                    );
                }
            }
            built
        })
        .into_any_element()
    }

    /// The context ring with its percent, shown only while a session
    /// exists, as the web client's `ring-wrap` draws it. At 80 percent
    /// the ring turns warn and the tooltip says what the wire cannot
    /// do about it: no compaction control exists, so none is offered.
    fn fuel(&self, cx: &Context<Self>, pal: &'static Palette) -> Option<Stateful<Div>> {
        let session = self.store.read(cx).active_session()?;
        let fill = session.usage.fill();
        let (used, size) = (session.usage.used, session.usage.size);
        let percent = (fill * 100.0).round() as i64;
        let hot = fill >= 0.8;
        let ring = if hot { pal.warn } else { pal.accent };
        let mut tooltip = if size > 0 {
            format!("Context: {used} of {size} tokens ({percent}%)")
        } else {
            "Context: the agent sent no usage yet".to_owned()
        };
        if hot {
            tooltip.push_str("; no compaction control exists on the wire");
        }
        let (faint, ink, hover) = (pal.faint, pal.ink, pal.hover);
        let tip: SharedString = tooltip.into();
        Some(
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
                .child(
                    ProgressCircle::new("ring")
                        .value(fill as f32 * 100.0)
                        .color(ring)
                        .with_size(px(22.)),
                )
                .child(SharedString::from(format!("{percent}%"))),
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
    fn toolbar(
        &self,
        cx: &Context<Self>,
        pal: &'static Palette,
        goal_laid_out: Rc<Cell<bool>>,
    ) -> Div {
        let session = self.store.read(cx).active_session();
        let plan_on = session
            .and_then(active_mode)
            .is_some_and(|mode| mode == "plan");
        let swarm_on = session
            .and_then(|session| select_option(session, "swarm"))
            .is_some_and(|option| option.current_value == "on");
        h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .gap(px(SP_2))
            .pt(px(6.))
            .px(px(8.))
            .pb(px(8.))
            .child(self.plus_button(cx, pal, goal_laid_out))
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
        let goal_input = self.goal_input.clone();
        self.goal_mirror.flush(|goal| {
            goal_input.update(cx, |state, cx| state.set_value(goal, window, cx));
        });
        let draft_laid_out = self.draft_mirror.laid_out().flag();
        // The goal input mounts inside the plus popover, so the popover's
        // element is what releases the goal mirror.
        let goal_laid_out = self.goal_mirror.laid_out().flag();
        let release = cx.entity().downgrade();
        let theme = cx.theme().colors;
        let pal = Palette::active(cx);
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let session = self.store.read(cx).active_session();
        let plan_on = session
            .and_then(active_mode)
            .is_some_and(|mode| mode == "plan");
        let swarm_on = session
            .and_then(|session| select_option(session, "swarm"))
            .is_some_and(|option| option.current_value == "on");
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
            .on_action(cx.listener(|this, _: &Escape, _, cx| this.on_escape(cx)))
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
                    .child(self.toolbar(cx, pal, goal_laid_out)),
            )
            .child(self.hint_line(pal, cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, Entity, TestAppContext};

    use super::{
        ComposerView, ESC_WINDOW, EscStep, Suggest, active_mode, esc_step, mention_items,
        next_mode_value, select_option, slash_items, suggest_for,
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
                cx.new(|cx| ComposerView::new(store.clone(), window, cx))
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
                cx.new(|cx| ComposerView::new(store.clone(), window, cx))
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

    use std::time::{Duration, Instant};

    use kage_client::Session;
    use kage_client::wire::{
        FsEntry, FsKind, FsListResult, SessionConfigKind, SessionConfigOption,
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
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "fs:list");
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
            next_mode_value(&session).as_deref(),
            Some("ask"),
            "the cycle follows the option's order"
        );
        let session = mode_session("plan", None, &values);
        assert_eq!(next_mode_value(&session).as_deref(), Some("default"));
        let session = mode_session("mystery", None, &values);
        assert_eq!(
            next_mode_value(&session).as_deref(),
            Some("default"),
            "an unknown current starts the cycle"
        );
        let session = mode_session("ask", Some("deny"), &values);
        assert_eq!(
            next_mode_value(&session).as_deref(),
            Some("plan"),
            "current_mode_update marks the active mode, not the stale option value"
        );
        let bare = Session::new("s1");
        assert_eq!(next_mode_value(&bare), None, "no option, no cycle");
        let empty = mode_session("default", None, &[]);
        assert_eq!(next_mode_value(&empty), None, "no values, no cycle");
        assert_eq!(
            active_mode(&session),
            Some("deny".to_owned()),
            "the mark wins over the option value"
        );
        assert!(select_option(&session, "mode").is_some());
        assert!(select_option(&session, "goal").is_none());
    }
}
