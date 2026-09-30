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

use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AnyElement, App, AppContext as _, Context, Div, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use kage_client::Session;
use kage_client::wire::{FsKind, FsListResult, SessionConfigKind, SessionConfigOption};
use serde_json::Value;

use crate::store::Store;

gpui_kit::actions!(kage_desktop, [CycleMode]);

/// How long the first Esc of an interrupt gesture waits for the
/// second one.
pub(crate) const ESC_WINDOW: Duration = Duration::from_millis(1500);

/// The cells of the context fuel gauge.
pub(crate) const FUEL_CELLS: usize = 20;

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

/// How many fuel gauge cells a context fill lights, out of
/// [`FUEL_CELLS`].
#[must_use]
pub(crate) fn fuel_cells(fill: f64) -> usize {
    ((fill * FUEL_CELLS as f64) + 0.5)
        .floor()
        .clamp(0., FUEL_CELLS as f64) as usize
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
}

impl ComposerView {
    /// A composer following `store`, focused and bound to its keys.
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Message the agent; Enter sends, Shift+Enter breaks the line")
                .auto_grow(1, 8)
                .submit_on_enter(true)
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        let goal_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Set the goal option; empty clears it")
        });
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
        cx.observe_in(&store, window, |this, _, window, cx| {
            this.follow_active(window, cx);
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
        }
    }

    /// The multi-line input state, shared with the transcript's edit
    /// action.
    #[must_use]
    pub fn input(&self) -> &Entity<TextareaState> {
        &self.input
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
        self.input.update(cx, |state, cx| {
            state.set_value(draft.as_str(), window, cx);
            state.focus(window, cx);
        });
        self.goal_input.update(cx, |state, cx| {
            state.set_value(goal.as_str(), window, cx);
        });
        self.loaded = active;
        self.esc_armed = None;
        self.fs_asked = false;
        self.plus_open = false;
        cx.notify();
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
                self.store.update(cx, |store, _| {
                    store.fs_list("");
                });
            }
        }
        cx.notify();
    }

    /// Sends the typed text: plain when idle, queued while a run is in
    /// flight, or steered when `steer` says so and the wire allows it.
    /// Accepted text leaves the textarea and the draft.
    pub fn submit(&mut self, steer: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_value(cx);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let accepted = if steer {
            self.store.update(cx, |store, _| store.steer(text).is_ok())
        } else {
            self.store
                .update(cx, |store, _| store.submit(text).is_some())
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
        let value = self.input_value(cx);
        let cursor = self.input_cursor(cx);
        let before = value.get(..cursor).unwrap_or(&value).to_owned();
        let after = value.get(cursor..).unwrap_or("").to_owned();
        let spaced = before.is_empty()
            || before.ends_with(char::is_whitespace)
            || after.starts_with(char::is_whitespace);
        let space = if spaced { "" } else { " " };
        let text = format!("{before}{space}@{after}");
        let caret = before.len() + space.len() + 1;
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

    /// The slash or mention rows the current text points at, above the
    /// input.
    fn suggestion_panel(&self, cx: &Context<Self>) -> Option<AnyElement> {
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
                let mut panel = v_flex()
                    .id("slash-menu")
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(6.))
                    .py_1();
                for item in items {
                    let this = this.clone();
                    let name = item.name.clone();
                    let mut row = h_flex()
                        .id(SharedString::from(format!("slash-{}", item.name)))
                        .w_full()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .hover(|row| row.bg(theme.list_hover))
                        .on_click(move |_, window, cx| {
                            this.update(cx, |this, cx| this.pick_slash(&name, window, cx));
                        })
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(SharedString::from(format!("/{}", item.name))),
                        );
                    if let Some(hint) = item.hint {
                        row = row.child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(SharedString::from(hint)),
                        );
                    }
                    if let Some(badge) = item.badge {
                        row = row.child(
                            div()
                                .text_size(px(10.))
                                .px_1()
                                .rounded_xs()
                                .bg(theme.secondary)
                                .text_color(theme.secondary_foreground)
                                .child(badge),
                        );
                    }
                    if !item.description.is_empty() {
                        row = row.child(
                            div()
                                .flex_1()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(SharedString::from(item.description)),
                        );
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
                let mut panel = v_flex()
                    .id("mention-menu")
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(6.))
                    .py_1();
                if listing.is_none() {
                    panel = panel.child(
                        div()
                            .px_3()
                            .py_1()
                            .text_size(px(11.))
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
                            h_flex()
                                .id(SharedString::from(format!("mention-{}", item.path)))
                                .w_full()
                                .px_3()
                                .py_1()
                                .gap_2()
                                .items_center()
                                .hover(|row| row.bg(theme.list_hover))
                                .on_click(move |_, window, cx| {
                                    this.update(cx, |this, cx| {
                                        this.pick_mention(at, &path, window, cx);
                                    });
                                })
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .child(SharedString::from(item.path.clone())),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(theme.muted_foreground)
                                        .child(if item.directory { "dir" } else { "file" }),
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

    /// The line under the input: the key help, or the interrupt hint.
    fn hint_line(&self, cx: &Context<Self>) -> Div {
        let theme = cx.theme().colors;
        let running = self.running(cx);
        let steer = self.store.read(cx).state().steer_available();
        let help = if running {
            if steer {
                "Enter queue \u{b7} Ctrl+Enter steer \u{b7} Esc Esc interrupt"
            } else {
                "Enter queue \u{b7} Esc Esc interrupt \u{b7} steering not advertised"
            }
        } else {
            "Enter send \u{b7} Shift+Enter newline \u{b7} Shift+Tab mode"
        };
        let left = match self.esc_hint(cx) {
            Some(hint) => hint.into_any_element(),
            None => div()
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child(help)
                .into_any_element(),
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
            .items_center()
            .child(left)
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(cost)),
            )
    }

    /// The plus popover: attach, mention, commands, goal, plan and
    /// swarm.
    fn plus_button(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let this = cx.entity();
        let store = self.store.clone();
        let goal_input = self.goal_input.clone();
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
                Button::new("plus")
                    .icon(IconName::Plus)
                    .xsmall()
                    .ghost()
                    .tooltip("Add"),
            )
            .anchor(Anchor::BottomLeft)
            .open(self.plus_open)
            .on_open_change(move |open, _, cx| {
                let open = *open;
                track_open.update(cx, |this, cx| {
                    this.plus_open = open;
                    cx.notify();
                });
            })
            .content(move |_, _, _| {
                v_flex()
                    .w(px(280.))
                    .gap_1()
                    .child(
                        Button::new("plus-attach")
                            .label("Attach files")
                            .ghost()
                            .disabled(true)
                            .tooltip(
                                "the prompt surface carries text only for now; \
                                 attachments are a later story",
                            ),
                    )
                    .child(
                        h_flex()
                            .id("plus-mention")
                            .w_full()
                            .px_2()
                            .py_1()
                            .rounded(px(4.))
                            .hover(|row| row.bg(theme.list_hover))
                            .on_click({
                                let this = this.clone();
                                move |_, window, cx| {
                                    close(&this, cx);
                                    this.update(cx, |this, cx| {
                                        this.seed_mention(window, cx);
                                    });
                                }
                            })
                            .child(div().text_size(px(13.)).child("Mention a file"))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.muted_foreground)
                                    .child("@ names a path from the session workdir"),
                            ),
                    )
                    .child(
                        h_flex()
                            .id("plus-commands")
                            .w_full()
                            .px_2()
                            .py_1()
                            .rounded(px(4.))
                            .hover(|row| row.bg(theme.list_hover))
                            .on_click({
                                let this = this.clone();
                                move |_, window, cx| {
                                    close(&this, cx);
                                    this.update(cx, |this, cx| this.seed_slash(window, cx));
                                }
                            })
                            .child(div().text_size(px(13.)).child("Commands"))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.muted_foreground)
                                    .child("/ lists the commands the agent sent"),
                            ),
                    )
                    .child(
                        v_flex()
                            .w_full()
                            .gap_1()
                            .px_2()
                            .py_1()
                            .child(div().text_size(px(13.)).child("Goal"))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.muted_foreground)
                                    .child("the goal text option; the session works toward it"),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Input::new(&goal_input).flex_1())
                                    .child(Button::new("goal-set").label("Set").xsmall().on_click(
                                        {
                                            let this = this.clone();
                                            let goal_input = goal_input.clone();
                                            move |_, window, cx| {
                                                let text =
                                                    goal_input.read(cx).value().trim().to_owned();
                                                this.update(cx, |this, cx| {
                                                    this.set_goal(&text, cx);
                                                });
                                                goal_input.update(cx, |state, cx| {
                                                    state.set_value("", window, cx)
                                                });
                                            }
                                        },
                                    )),
                            ),
                    )
                    .child(
                        h_flex()
                            .id("plus-plan")
                            .w_full()
                            .px_2()
                            .py_1()
                            .rounded(px(4.))
                            .hover(|row| row.bg(theme.list_hover))
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
                            .child(div().text_size(px(13.)).child("Plan mode"))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(if plan_on {
                                        theme.success
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(if plan_on { "on" } else { "off" }),
                            ),
                    )
                    .child(if swarm_known {
                        h_flex()
                            .id("plus-swarm")
                            .w_full()
                            .px_2()
                            .py_1()
                            .rounded(px(4.))
                            .hover(|row| row.bg(theme.list_hover))
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
                            .child(div().text_size(px(13.)).child("Swarm"))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(if swarm_on {
                                        theme.success
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(if swarm_on { "on" } else { "off" }),
                            )
                            .into_any_element()
                    } else {
                        Button::new("plus-swarm-unknown")
                            .label("Swarm")
                            .ghost()
                            .disabled(true)
                            .tooltip("the agent sent no swarm config option")
                            .into_any_element()
                    })
            })
            .into_any_element()
    }

    /// The permission mode picker, listing exactly the advertised
    /// values with the active one marked.
    fn mode_button(&self, cx: &Context<Self>) -> AnyElement {
        let session = self.store.read(cx).active_session();
        let Some(option) = session.and_then(|session| select_option(session, "mode")) else {
            return Button::new("composer-mode")
                .label("Mode")
                .xsmall()
                .ghost()
                .disabled(true)
                .tooltip("the agent sent no mode config option")
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
        let store = self.store.clone();
        Button::new("composer-mode")
            .label(SharedString::from(label))
            .xsmall()
            .ghost()
            .dropdown_caret(true)
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
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

    /// A picker button over one select config option.
    fn picker_button(
        &self,
        id: &'static str,
        element_id: &'static str,
        missing_tooltip: &'static str,
        title: &'static str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let session = self.store.read(cx).active_session();
        let Some(option) = session.and_then(|session| select_option(session, id)) else {
            return Button::new(element_id)
                .label(title)
                .xsmall()
                .ghost()
                .disabled(true)
                .tooltip(missing_tooltip)
                .into_any_element();
        };
        let option = option.clone();
        let label = option
            .current_value
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        let store = self.store.clone();
        Button::new(element_id)
            .label(SharedString::from(label))
            .xsmall()
            .ghost()
            .dropdown_caret(true)
            .tooltip(SharedString::from(format!(
                "{title}: {}",
                option.current_value
            )))
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                let mut built = menu.label(title);
                for value in &option.options {
                    let store = store.clone();
                    let value = value.clone();
                    built = built.item(
                        PopupMenuItem::new(value.name.clone())
                            .checked(value.value == option.current_value)
                            .on_click(move |_, _, cx| {
                                store.update(cx, |store, cx| {
                                    store.set_option(id, &value.value);
                                    cx.notify();
                                });
                            }),
                    );
                }
                built
            })
            .into_any_element()
    }

    /// The context ring, the percent and the fuel gauge cells. At 80
    /// percent the tooltip says what the wire cannot do about it: no
    /// compaction control exists, so none is offered.
    fn fuel(&self, cx: &Context<Self>) -> Div {
        let theme = cx.theme().colors;
        let (fill, used, size) = self
            .store
            .read(cx)
            .active_session()
            .map(|session| (session.usage.fill(), session.usage.used, session.usage.size))
            .unwrap_or((0.0, 0, 0));
        let percent = (fill * 100.0).round() as i64;
        let filled = fuel_cells(fill);
        let hot = fill >= 0.8;
        let mut tooltip = if size > 0 {
            format!("Context: {used} of {size} tokens ({percent}%)")
        } else {
            "Context: the agent sent no usage yet".to_owned()
        };
        if hot {
            tooltip.push_str("; no compaction control exists on the wire");
        }
        let cells = h_flex().gap(px(1.)).children((0..FUEL_CELLS).map(|cell| {
            div()
                .w(px(3.))
                .h(px(10.))
                .rounded_xs()
                .when(cell < filled, |cell| {
                    cell.bg(if hot { theme.warning } else { theme.primary })
                })
                .when(cell >= filled, |cell| cell.bg(theme.border))
        }));
        h_flex().gap_2().items_center().child(cells).child(
            Button::new("fuel")
                .icon(
                    ProgressCircle::new("ring")
                        .value(fill as f32 * 100.0)
                        .with_size(px(14.))
                        .when(hot, |ring| ring.color(theme.warning)),
                )
                .label(SharedString::from(format!("{percent}%")))
                .xsmall()
                .ghost()
                .tooltip(tooltip),
        )
    }

    /// The send and stop pair: stop while a run is in flight, send
    /// otherwise.
    fn send_button(&self, cx: &Context<Self>) -> AnyElement {
        let this = cx.entity();
        let running = self.running(cx);
        if running {
            let store = self.store.clone();
            Button::new("composer-stop")
                .icon(IconName::Square)
                .xsmall()
                .tooltip("Interrupt (Esc Esc)")
                .on_click(move |_, _, cx| {
                    store.update(cx, |store, cx| {
                        store.cancel();
                        cx.notify();
                    });
                })
                .into_any_element()
        } else {
            let empty = self.input_value(cx).trim().is_empty();
            Button::new("composer-send")
                .icon(IconName::ArrowUp)
                .xsmall()
                .disabled(empty)
                .tooltip("Send (Enter); queues while a turn runs")
                .on_click(move |_, window, cx| {
                    this.update(cx, |this, cx| this.submit(false, window, cx));
                })
                .into_any_element()
        }
    }

    /// The steer button, shown while a run is in flight: disabled with
    /// the missing capability named when the agent did not advertise
    /// steering.
    fn steer_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let advertised = self.store.read(cx).state().steer_available();
        Button::new("composer-steer")
            .label("Steer")
            .xsmall()
            .ghost()
            .disabled(!advertised)
            .tooltip(if advertised {
                "Steer the running turn (Ctrl+Enter)"
            } else {
                "the agent did not advertise the steer capability"
            })
            .on_click(move |_, window, cx| {
                this.update(cx, |this, cx| this.submit(true, window, cx));
            })
    }

    /// The toolbar row under the input.
    fn toolbar(&self, cx: &Context<Self>) -> Div {
        let running = self.running(cx);
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(self.plus_button(cx))
            .child(self.mode_button(cx))
            .child(div().flex_1())
            .child(self.fuel(cx))
            .child(self.picker_button(
                "model",
                "composer-model",
                "the agent sent no model config option",
                "Model",
                cx,
            ))
            .child(self.picker_button(
                "thinking",
                "composer-thinking",
                "the agent sent no thinking config option",
                "Thinking",
                cx,
            ))
            .when(running, |row| row.child(self.steer_button(cx)))
            .child(self.send_button(cx))
    }
}

impl Render for ComposerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let suggestion = self.suggestion_panel(cx);
        v_flex()
            .id("composer")
            .w_full()
            .border_t_1()
            .border_color(theme.border)
            .p_3()
            .gap_2()
            .on_action(cx.listener(|this, _: &CycleMode, _, cx| this.cycle_mode(cx)))
            .on_action(cx.listener(|this, _: &Escape, _, cx| this.on_escape(cx)))
            .children(suggestion)
            .child(Textarea::new(&self.input))
            .child(self.hint_line(cx))
            .child(self.toolbar(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ESC_WINDOW, EscStep, FUEL_CELLS, Suggest, active_mode, esc_step, fuel_cells, mention_items,
        next_mode_value, select_option, slash_items, suggest_for,
    };
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
    fn fuel_cells_cover_twenty_cells() {
        assert_eq!(fuel_cells(0.0), 0);
        assert_eq!(fuel_cells(0.5), FUEL_CELLS / 2);
        assert_eq!(fuel_cells(1.0), FUEL_CELLS);
        assert_eq!(fuel_cells(0.8), 16);
        assert_eq!(fuel_cells(2.0), FUEL_CELLS, "overfull clamps");
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
