//! The app chrome: find in chat, the command palette, the toast
//! stack and the welcome pane.
//!
//! Four self-contained pieces the shell mounts: [`FindBar`] over the
//! transcript, [`PaletteView`] as a modal, [`Toasts`] in a corner and
//! [`WelcomeView`] when no session is active. Matching, palette
//! narrowing and the notice scan are pure functions over the client
//! state, so they test without a window. Key scopes: the find bar
//! answers in the `Find` context, the palette in `Palette`, so their
//! keys never reach the composer.

use std::collections::HashMap;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, TextareaState};
use gpui_kit::component::theme::{ActiveTheme as _, ThemeColor};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, TestSupportExt as _, Window, div, px,
};
use kage_client::wire::NoticeTone;
use kage_client::{Change, State, TranscriptItem};
use serde_json::Value;

use crate::store::Store;
use crate::views::transcript::{FindMarks, RowKey, TranscriptView};

gpui_kit::actions!(kage_desktop, [FindNext, FindPrev, FindClose]);
gpui_kit::actions!(
    kage_desktop,
    [PaletteUp, PaletteDown, PaletteRun, PaletteClose]
);

/// How long a toast stays before it expires on its own.
pub const TOAST_TTL: Duration = Duration::from_secs(4);

/// The most toasts the stack holds; the oldest give way.
pub const TOAST_MAX: usize = 5;

/// The row indexes `query` matches, case insensitively. An empty
/// query matches nothing.
#[must_use]
pub fn find_matches(rows: &[(RowKey, String)], query: &str) -> Vec<usize> {
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    rows.iter()
        .enumerate()
        .filter_map(|(ix, (_, text))| text.to_lowercase().contains(&needle).then_some(ix))
        .collect()
}

/// The match index one step from `current`, wrapping at the ends.
/// No matches stays put.
#[must_use]
pub fn step_match(current: usize, total: usize, back: bool) -> usize {
    if total == 0 {
        return 0;
    }
    if back {
        (current + total - 1) % total
    } else {
        (current + 1) % total
    }
}

/// The counter the find bar renders, one based on both ends.
#[must_use]
pub fn counter_text(current: usize, total: usize) -> String {
    if total == 0 {
        "0 / 0".to_owned()
    } else {
        format!("{} / {total}", current + 1)
    }
}

/// What a click on a toast does beyond dismissing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastAction {
    /// Nothing: the click only dismisses.
    None,
    /// Follow the session the toast is about.
    ActivateSession(String),
}

/// One toast before it renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastDraft {
    /// Severity, as the notice tones carry it.
    pub tone: NoticeTone,
    /// The message text.
    pub text: String,
    /// What a click does beyond dismissing.
    pub action: ToastAction,
}

/// The color a notice tone renders in, matching the transcript rows.
#[must_use]
pub fn tone_color(tone: NoticeTone, colors: ThemeColor) -> Hsla {
    match tone {
        NoticeTone::Info => colors.info,
        NoticeTone::Warn => colors.warning,
        NoticeTone::Error => colors.danger,
        NoticeTone::Success => colors.success,
    }
}

/// The toasts a handled frame's changes raise. An ask withdrawn
/// through `$/cancel_request` was answered by another client; its
/// toast follows the session on click.
#[must_use]
pub fn toasts_for_changes(changes: &[Change]) -> Vec<ToastDraft> {
    changes
        .iter()
        .filter_map(|change| match change {
            Change::AnsweredElsewhere { .. } => {
                let action = match change.session_id() {
                    Some(id) => ToastAction::ActivateSession(id.to_owned()),
                    None => ToastAction::None,
                };
                Some(ToastDraft {
                    tone: NoticeTone::Info,
                    text: "an approval was answered in another session".to_owned(),
                    action,
                })
            }
            _ => None,
        })
        .collect()
}

/// Tracks how many notice items each session held at the last scan,
/// so a frame that adds notices raises its toast once. A session's
/// existing notices are history at first sight and stay quiet.
#[derive(Debug, Default)]
pub struct NoticeWatch {
    seen: HashMap<String, usize>,
}

impl NoticeWatch {
    /// The notices `state` holds beyond the last scan, as toasts.
    pub fn scan(&mut self, state: &State) -> Vec<ToastDraft> {
        let mut drafts = Vec::new();
        for (id, session) in &state.sessions {
            let notices: Vec<&TranscriptItem> = session
                .items
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Notice { .. }))
                .collect();
            let entry = self.seen.entry(id.clone()).or_insert(notices.len());
            if notices.len() > *entry {
                for item in notices[(*entry)..].iter() {
                    if let TranscriptItem::Notice { tone, text } = item {
                        drafts.push(ToastDraft {
                            tone: *tone,
                            text: text.clone(),
                            action: ToastAction::None,
                        });
                    }
                }
                *entry = notices.len();
            }
        }
        drafts
    }
}

/// What the find bar tells the shell once it closed itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindEvent {
    /// The user closed the bar; the composer can take the focus back.
    Closed,
}

/// The find bar over the transcript: a query, the match counter and
/// the stepping keys. Enter steps forward, Shift+Enter steps back,
/// Esc closes. A matching row tints as a whole; the markdown text
/// view exposes no range highlight, so characters inside a row stay
/// unmarked.
pub struct FindBar {
    transcript: Entity<TranscriptView>,
    query: Entity<InputState>,
    open: bool,
    /// The keys of the rows the query matched, in transcript order.
    matches: Vec<RowKey>,
    /// Which match the counter points at.
    current: usize,
}

impl EventEmitter<FindEvent> for FindBar {}

impl FindBar {
    /// A closed find bar over `transcript`.
    pub fn new(
        store: Entity<Store>,
        transcript: Entity<TranscriptView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Find in chat"));
        cx.subscribe_in(&query, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::Change = event {
                this.recompute(cx);
            }
        })
        .detach();
        cx.observe_in(&store, window, |this, _, _, cx| this.recompute(cx))
            .detach();
        Self {
            transcript,
            query,
            open: false,
            matches: Vec::new(),
            current: 0,
        }
    }

    /// Whether the bar shows.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The counter's two numbers: which match, and how many.
    #[must_use]
    pub fn counter(&self) -> (usize, usize) {
        (self.current, self.matches.len())
    }

    /// Opens the bar over a fresh query and focuses it.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.current = 0;
        self.matches.clear();
        self.query.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        self.recompute(cx);
    }

    /// Closes the bar and clears the transcript tint.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.matches.clear();
        self.current = 0;
        self.query
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.transcript
            .update(cx, |transcript, cx| transcript.set_find(None, cx));
        cx.notify();
    }

    /// Re-matches the query against the transcript rows. Runs on
    /// every query change and frame the store moves, so the counter
    /// and the tint follow the transcript as it streams.
    fn recompute(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        let query = self.query.read(cx).value().to_string();
        let rows = self.transcript.read(cx).searchable_rows(cx);
        self.matches = find_matches(&rows, &query)
            .into_iter()
            .map(|ix| rows[ix].0)
            .collect();
        if self.current >= self.matches.len() {
            self.current = 0;
        }
        let marks = self.marks();
        self.transcript
            .update(cx, |transcript, cx| transcript.set_find(marks, cx));
        cx.notify();
    }

    /// The marks the transcript tints rows with.
    fn marks(&self) -> Option<FindMarks> {
        if self.matches.is_empty() {
            None
        } else {
            Some(FindMarks {
                keys: self.matches.clone(),
                current: self.current,
            })
        }
    }

    /// Steps to the next or previous match and brings it into view.
    fn step(&mut self, back: bool, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        self.current = step_match(self.current, self.matches.len(), back);
        let key = self.matches[self.current];
        let marks = self.marks();
        self.transcript.update(cx, |transcript, cx| {
            transcript.set_find(marks, cx);
            transcript.scroll_to_row(key, cx);
        });
        cx.notify();
    }
}

impl Render for FindBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = cx.theme().colors;
        let this = cx.entity();
        let total = self.matches.len();
        let step = move |this: &Entity<Self>, back: bool, cx: &mut App| {
            this.update(cx, |this, cx| this.step(back, cx));
        };
        let prev = this.clone();
        let next = this.clone();
        h_flex()
            .id("find-bar")
            .test_support()
            .key_context("Find")
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .bg(theme.secondary)
            .border_b_1()
            .border_color(theme.border)
            .on_action(cx.listener(|this, _: &FindNext, _, cx| this.step(false, cx)))
            .on_action(cx.listener(|this, _: &FindPrev, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &FindClose, window, cx| {
                this.close(window, cx);
                cx.emit(FindEvent::Closed);
            }))
            .child(Icon::new(IconName::Search).text_color(theme.muted_foreground))
            .child(Input::new(&self.query).w(px(320.)))
            .child(
                div()
                    .id("find-counter")
                    .test_support()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .aria_label(SharedString::from(counter_text(self.current, total)))
                    .child(SharedString::from(counter_text(self.current, total))),
            )
            .child(
                Button::new("find-prev")
                    .icon(IconName::ChevronUp)
                    .xsmall()
                    .ghost()
                    .tooltip("Previous match (Shift+Enter)")
                    .on_click(move |_, _, cx| step(&prev, true, cx)),
            )
            .child(
                Button::new("find-next")
                    .icon(IconName::ChevronDown)
                    .xsmall()
                    .ghost()
                    .tooltip("Next match (Enter)")
                    .on_click(move |_, _, cx| step(&next, false, cx)),
            )
            .child(
                Button::new("find-close")
                    .icon(IconName::Close)
                    .xsmall()
                    .ghost()
                    .tooltip("Close (Esc)")
                    .on_click({
                        let this = this.clone();
                        move |_, window, cx| {
                            this.update(cx, |this, cx| {
                                this.close(window, cx);
                                cx.emit(FindEvent::Closed);
                            });
                        }
                    }),
            )
            .into_any_element()
    }
}

/// One command palette row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteEntry {
    /// An agent command; running it fills the composer draft with the
    /// command text, ready for arguments.
    Command {
        /// The command name, without the leading slash.
        name: String,
        /// What the agent described the command as.
        description: String,
    },
    /// A session; running it follows the session.
    Session {
        /// The session id.
        id: String,
        /// The display title, when the agent set one.
        title: Option<String>,
        /// Whether the connection holds the session open.
        open: bool,
    },
}

impl PaletteEntry {
    /// The text the row renders and the query narrows over.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Command { name, .. } => format!("/{name}"),
            Self::Session { title, .. } => title
                .clone()
                .unwrap_or_else(|| "untitled session".to_owned()),
        }
    }

    /// The kind word the row carries.
    #[must_use]
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Command { .. } => "command",
            Self::Session { open: true, .. } => "session",
            Self::Session { open: false, .. } => "recorded",
        }
    }

    /// The detail line a row carries beside its label.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Command { description, .. } => description.clone(),
            Self::Session { id, .. } => id.clone(),
        }
    }
}

/// The palette rows for the agent's commands and the session
/// directory, narrowed by `query` case insensitively. Commands come
/// first, then open sessions, then recorded ones.
#[must_use]
pub fn palette_entries(
    commands: &[Value],
    sessions: &[(String, Option<String>, bool)],
    query: &str,
) -> Vec<PaletteEntry> {
    let needle = query.to_lowercase();
    let mut entries = Vec::new();
    for command in commands {
        let Some(name) = command.get("name").and_then(Value::as_str) else {
            continue;
        };
        let entry = PaletteEntry::Command {
            name: name.to_owned(),
            description: command
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
        if needle.is_empty() || entry.label().to_lowercase().contains(&needle) {
            entries.push(entry);
        }
    }
    for (id, title, open) in sessions {
        let entry = PaletteEntry::Session {
            id: id.clone(),
            title: title.clone(),
            open: *open,
        };
        let hit =
            entry.label().to_lowercase().contains(&needle) || id.to_lowercase().contains(&needle);
        if needle.is_empty() || hit {
            entries.push(entry);
        }
    }
    entries
}

/// The command palette: a modal over the agent's commands and the
/// session directory. The query narrows, the arrows move, Enter runs
/// and Esc closes.
pub struct PaletteView {
    store: Entity<Store>,
    composer: Entity<TextareaState>,
    query: Entity<InputState>,
    open: bool,
    /// The highlighted row index.
    selected: usize,
}

impl PaletteView {
    /// A closed palette over `store`, filling `composer` when a
    /// command runs.
    pub fn new(
        store: Entity<Store>,
        composer: Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query =
            cx.new(|cx| InputState::new(window, cx).placeholder("Run a command or switch session"));
        cx.subscribe_in(&query, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::Change = event {
                this.selected = 0;
                cx.notify();
            }
        })
        .detach();
        Self {
            store,
            composer,
            query,
            open: false,
            selected: 0,
        }
    }

    /// Whether the palette shows.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The composer textarea a running command fills.
    #[must_use]
    pub fn textarea(&self) -> &Entity<TextareaState> {
        &self.composer
    }

    /// The rows for the current state and query.
    #[must_use]
    pub fn entries(&self, cx: &App) -> Vec<PaletteEntry> {
        let store = self.store.read(cx);
        let state = store.state();
        let query = self.query.read(cx).value();
        let no_commands: Vec<Value> = Vec::new();
        let commands = store
            .active_session()
            .map(|session| &session.commands)
            .unwrap_or(&no_commands);
        let mut sessions: Vec<(String, Option<String>, bool)> = state
            .sessions
            .iter()
            .map(|(id, session)| (id.clone(), session.title.clone(), true))
            .collect();
        for info in &state.directory {
            if state.sessions.contains_key(&info.session_id) {
                continue;
            }
            sessions.push((info.session_id.clone(), info.title.clone(), false));
        }
        palette_entries(commands, &sessions, &query)
    }

    /// Opens the palette over a fresh query and focuses it.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.selected = 0;
        self.query.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// Closes the palette.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        cx.notify();
    }

    /// Moves the highlight by `delta`, wrapping at the ends.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let total = self.entries(cx).len();
        if total == 0 {
            return;
        }
        let total = total as isize;
        let next = self.selected as isize + delta;
        self.selected = (((next % total) + total) % total) as usize;
        cx.notify();
    }

    /// Runs the row at `index`: a command fills the composer draft
    /// with its text, a session becomes the followed one. Either way
    /// the palette closes and the composer takes the focus.
    pub fn run_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries(cx).get(index).cloned() else {
            return;
        };
        match entry {
            PaletteEntry::Command { name, .. } => {
                let text = format!("/{name} ");
                let active = self.store.read(cx).active_id().map(str::to_owned);
                self.store
                    .update(cx, |store, _| store.set_draft(active.as_deref(), &text));
                self.composer.update(cx, |state, cx| {
                    state.set_value(text.as_str(), window, cx);
                    state.focus(window, cx);
                });
            }
            PaletteEntry::Session { id, .. } => {
                self.store.update(cx, |store, cx| {
                    store.set_active(id);
                    cx.notify();
                });
                self.composer
                    .update(cx, |state, cx| state.focus(window, cx));
            }
        }
        self.close(cx);
    }
}

impl Render for PaletteView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = cx.theme().colors;
        let entries = self.entries(cx);
        if self.selected >= entries.len() {
            self.selected = 0;
        }
        let selected = self.selected;
        let this = cx.entity();
        let mut list = v_flex()
            .id("palette-list")
            .max_h(px(360.))
            .overflow_y_scroll();
        if entries.is_empty() {
            list = list.child(
                div()
                    .id("palette-empty")
                    .test_support()
                    .px_3()
                    .py_2()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child("nothing matches"),
            );
        }
        for (ix, entry) in entries.iter().enumerate() {
            let this = this.clone();
            let row = h_flex()
                .id(SharedString::from(format!("palette-entry-{ix}")))
                .test_support()
                .w_full()
                .px_2()
                .py_1()
                .gap_2()
                .items_center()
                .rounded(px(4.))
                .map(|row| {
                    if ix == selected {
                        row.bg(theme.list_active)
                    } else {
                        row
                    }
                })
                .hover(|row| row.bg(theme.list_hover))
                .aria_label(SharedString::from(format!(
                    "{} {}",
                    entry.badge(),
                    entry.label()
                )))
                .on_click(move |_, window, cx| {
                    this.update(cx, |this, cx| this.run_index(ix, window, cx));
                })
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme.foreground)
                        .child(SharedString::from(entry.label())),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .px_1()
                        .rounded_xs()
                        .bg(theme.secondary)
                        .text_color(theme.secondary_foreground)
                        .child(entry.badge()),
                )
                .child(
                    div()
                        .flex_1()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(SharedString::from(entry.detail())),
                );
            list = list.child(row);
        }
        div()
            .id("palette-overlay")
            .absolute()
            .inset_0()
            .bg(Hsla {
                h: 0.,
                s: 0.,
                l: 0.,
                a: 0.35,
            })
            .flex()
            .justify_center()
            .items_start()
            .pt(px(80.))
            .child(
                v_flex()
                    .id("palette")
                    .test_support()
                    .key_context("Palette")
                    .w(px(560.))
                    .max_h(px(480.))
                    .p_2()
                    .gap_2()
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(8.))
                    .overflow_hidden()
                    .on_action(cx.listener(|this, _: &PaletteUp, _, cx| {
                        this.move_selection(-1, cx);
                    }))
                    .on_action(cx.listener(|this, _: &PaletteDown, _, cx| {
                        this.move_selection(1, cx);
                    }))
                    .on_action(cx.listener(|this, _: &PaletteRun, window, cx| {
                        let index = this.selected;
                        this.run_index(index, window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &PaletteClose, _, cx| this.close(cx)))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(IconName::Search).text_color(theme.muted_foreground))
                            .child(Input::new(&self.query).flex_1()),
                    )
                    .child(list)
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child("up down move \u{b7} enter runs \u{b7} esc closes"),
                    ),
            )
            .into_any_element()
    }
}

/// One rendered toast.
struct Toast {
    id: usize,
    draft: ToastDraft,
}

/// The toast stack: a corner overlay fed by notice tones, the
/// answered-elsewhere change and local actions. Toasts expire on
/// their own; a click dismisses and runs the toast's action.
pub struct Toasts {
    store: Entity<Store>,
    items: Vec<Toast>,
    next_id: usize,
}

impl Toasts {
    /// An empty stack over `store`, for the click actions that follow
    /// a session.
    pub fn new(store: Entity<Store>) -> Self {
        Self {
            store,
            items: Vec::new(),
            next_id: 0,
        }
    }

    /// Raises `draft` and schedules its expiry. The stack holds at
    /// most [`TOAST_MAX`] toasts; the oldest give way.
    pub fn push(&mut self, draft: ToastDraft, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Toast { id, draft });
        if self.items.len() > TOAST_MAX {
            self.items.remove(0);
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_TTL).await;
            let _ = this.update(cx, |this, cx| this.remove(id, cx));
        })
        .detach();
        cx.notify();
    }

    /// The toasts in raise order, oldest first.
    #[must_use]
    pub fn items(&self) -> Vec<ToastDraft> {
        self.items.iter().map(|toast| toast.draft.clone()).collect()
    }

    fn remove(&mut self, id: usize, cx: &mut Context<Self>) {
        self.items.retain(|toast| toast.id != id);
        cx.notify();
    }

    /// A click: runs the toast's action, then dismisses it.
    fn clicked(&mut self, id: usize, cx: &mut Context<Self>) {
        let action = self
            .items
            .iter()
            .find(|toast| toast.id == id)
            .map(|toast| toast.draft.action.clone());
        if let Some(ToastAction::ActivateSession(session)) = action {
            self.store.update(cx, |store, cx| {
                store.set_active(session);
                cx.notify();
            });
        }
        self.remove(id, cx);
    }
}

impl Render for Toasts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let this = cx.entity();
        let raised: Vec<(usize, ToastDraft)> = self
            .items
            .iter()
            .map(|toast| (toast.id, toast.draft.clone()))
            .collect();
        div()
            .absolute()
            .bottom(px(36.))
            .right(px(16.))
            .flex()
            .flex_col()
            .gap_2()
            .items_end()
            .children(raised.into_iter().map(|(id, draft)| {
                let this = this.clone();
                h_flex()
                    .id(SharedString::from(format!("toast-{id}")))
                    .test_support()
                    .w(px(340.))
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .cursor_pointer()
                    .hover(|toast| toast.bg(theme.list_hover))
                    .aria_label(SharedString::from(draft.text.clone()))
                    .on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| this.clicked(id, cx));
                    })
                    .child(
                        div()
                            .size(px(8.))
                            .rounded_full()
                            .bg(tone_color(draft.tone, theme)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(theme.foreground)
                            .child(SharedString::from(draft.text)),
                    )
            }))
    }
}

/// One suggestion card's effect: the config option it sets once a
/// session offers it, and the draft it fills.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCard {
    option_id: &'static str,
    option_value: String,
    /// The draft still to fill; taken once a session carried it.
    draft: Option<String>,
}

/// The draft the plan card fills.
pub const WELCOME_PLAN_DRAFT: &str = "Plan a refactor of this repository before changing anything";

/// The draft the swarm card fills.
pub const WELCOME_SWARM_DRAFT: &str = "Run an audit of this repository with a swarm";

/// One suggestion card.
struct Suggestion {
    id: &'static str,
    title: &'static str,
    body: &'static str,
    option_id: &'static str,
    option_value: &'static str,
    draft: &'static str,
}

/// The cards that set an option and fill the draft. Every option here
/// is one the agent can advertise; a card sets nothing the session
/// was never offered. The goal card stands alone: its value is
/// typed, not suggested.
const SUGGESTIONS: [Suggestion; 2] = [
    Suggestion {
        id: "welcome-card-plan",
        title: "Plan a refactor",
        body: "sets the mode option to plan when the agent offers it",
        option_id: "mode",
        option_value: "plan",
        draft: WELCOME_PLAN_DRAFT,
    },
    Suggestion {
        id: "welcome-card-swarm",
        title: "Run a swarm audit",
        body: "sets the swarm option on when the agent offers it",
        option_id: "swarm",
        option_value: "on",
        draft: WELCOME_SWARM_DRAFT,
    },
];

/// The welcome pane: shown when no session is active. A wordmark,
/// suggestion cards over the real config options, and the composer
/// the shell keeps below it. A card clicked before any session exists
/// applies to the first session that opens, and only the option the
/// agent actually advertised.
pub struct WelcomeView {
    store: Entity<Store>,
    composer: Entity<TextareaState>,
    goal: Entity<InputState>,
    /// A card clicked while no session could carry it.
    pending: Option<PendingCard>,
}

impl WelcomeView {
    /// A welcome pane over `store`, filling the shared composer draft.
    pub fn new(
        store: Entity<Store>,
        composer: Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let goal =
            cx.new(|cx| InputState::new(window, cx).placeholder("State the goal; Enter sets it"));
        cx.subscribe_in(
            &goal,
            window,
            |this, goal, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = goal.read(cx).value().trim().to_owned();
                    if !text.is_empty() {
                        this.raise(
                            PendingCard {
                                option_id: "goal",
                                option_value: text,
                                draft: None,
                            },
                            window,
                            cx,
                        );
                    }
                    goal.update(cx, |state, cx| state.set_value("", window, cx));
                }
            },
        )
        .detach();
        cx.observe_in(&store, window, |this, _, window, cx| {
            if let Some(card) = this.pending.take() {
                this.pending = this.apply_card(card, window, cx);
            }
        })
        .detach();
        Self {
            store,
            composer,
            goal,
            pending: None,
        }
    }

    /// The composer textarea the cards fill.
    #[must_use]
    pub fn textarea(&self) -> &Entity<TextareaState> {
        &self.composer
    }

    /// Applies what `card` can carry now: the draft once a session
    /// exists, the option once that session offers it. Returns
    /// whatever still waits.
    fn apply_card(
        &mut self,
        mut card: PendingCard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PendingCard> {
        if self.store.read(cx).active_session().is_none() {
            return Some(card);
        }
        if let Some(draft) = card.draft.take() {
            let active = self.store.read(cx).active_id().map(str::to_owned);
            self.store
                .update(cx, |store, _| store.set_draft(active.as_deref(), &draft));
            let fill = draft.clone();
            self.composer.update(cx, |state, cx| {
                state.set_value(fill.as_str(), window, cx);
                state.focus(window, cx);
            });
            cx.notify();
        }
        if !self.offers(&card, cx) {
            return Some(card);
        }
        let option_id = card.option_id;
        let value = card.option_value.clone();
        self.store.update(cx, |store, cx| {
            store.set_option(option_id, &value);
            cx.notify();
        });
        None
    }

    /// Whether the active session advertises the card's option, and a
    /// select option lists the card's value.
    fn offers(&self, card: &PendingCard, cx: &App) -> bool {
        self.store.read(cx).active_session().is_some_and(|session| {
            session.config_options.iter().any(|option| {
                option.id == card.option_id
                    && (option.options.is_empty()
                        || option
                            .options
                            .iter()
                            .any(|value| value.value == card.option_value))
            })
        })
    }

    /// Applies a card now when it can, else holds it for the session
    /// that opens or advertises next.
    fn raise(&mut self, card: PendingCard, window: &mut Window, cx: &mut Context<Self>) {
        self.pending = self.apply_card(card, window, cx);
        cx.notify();
    }

    fn suggestion(&mut self, card: &Suggestion, window: &mut Window, cx: &mut Context<Self>) {
        self.raise(
            PendingCard {
                option_id: card.option_id,
                option_value: card.option_value.to_owned(),
                draft: Some(card.draft.to_owned()),
            },
            window,
            cx,
        );
    }
}

impl Render for WelcomeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let this = cx.entity();
        let mut cards = h_flex().flex_wrap().justify_center().gap_3();
        for suggestion in &SUGGESTIONS {
            let this = this.clone();
            cards = cards.child(
                v_flex()
                    .id(suggestion.id)
                    .test_support()
                    .w(px(256.))
                    .p_3()
                    .gap_1()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .cursor_pointer()
                    .hover(|card| card.border_color(theme.primary))
                    .on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| this.suggestion(suggestion, window, cx));
                    })
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child(suggestion.title),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(suggestion.body),
                    ),
            );
        }
        let goal_input = self.goal.clone();
        v_flex()
            .id("welcome")
            .test_support()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .w(px(560.))
                    .gap_4()
                    .items_center()
                    .child(
                        h_flex()
                            .id("welcome-wordmark")
                            .test_support()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(30.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.primary)
                                    .child("\u{25b8}"),
                            )
                            .child(
                                div()
                                    .text_size(px(30.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.foreground)
                                    .child("kage"),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child("the agent runs here; ctrl-n opens a session"),
                    )
                    .child(cards)
                    .child(
                        v_flex()
                            .id("welcome-card-goal")
                            .test_support()
                            .w(px(256.))
                            .p_3()
                            .gap_1()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.foreground)
                                    .child("Set a goal"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.muted_foreground)
                                    .child(
                                        "the session works toward it until a notice reports it met",
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Input::new(&goal_input).flex_1())
                                    .child(
                                        Button::new("welcome-goal-set")
                                            .label("Set")
                                            .xsmall()
                                            .on_click(move |_, window, cx| {
                                                let text =
                                                    goal_input.read(cx).value().trim().to_owned();
                                                this.update(cx, |this, cx| {
                                                    if !text.is_empty() {
                                                        this.raise(
                                                            PendingCard {
                                                                option_id: "goal",
                                                                option_value: text,
                                                                draft: None,
                                                            },
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                });
                                                goal_input.update(cx, |state, cx| {
                                                    state.set_value("", window, cx)
                                                });
                                            }),
                                    ),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use gpui_kit::component::input::TextareaState;
    use gpui_kit::component::v_flex;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, ElementId, Entity, IntoElement, ParentElement as _, Styled as _,
        TestAppContext, VisualTestContext, Window, div,
    };
    use serde_json::Value;

    use super::{
        FindBar, NoticeWatch, PaletteEntry, PaletteView, ToastAction, ToastDraft, Toasts,
        WelcomeView, counter_text, find_matches, palette_entries, step_match, toasts_for_changes,
    };
    use crate::store::{Command, Store};
    use crate::transport::State;
    use crate::views::transcript::TranscriptView;
    use kage_client::wire::NoticeTone;
    use kage_client::{Change, Frame};

    /// An initialize answer with everything the gate accepts.
    fn init_answer() -> Frame {
        Frame::Success {
            id: 1,
            result: serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "steer": true,
                    "sessionCapabilities": {"close": {}},
                },
                "agentInfo": {"name": "kage", "version": "0.1.0"},
            }),
        }
    }

    /// Carries out `commands` the way the shell does.
    fn run_commands(store: &mut Store) {
        for command in store.take_commands() {
            match command {
                Command::Handshake { replay_sessions } => store.handshake(replay_sessions),
                Command::NewSession => store.new_session(),
                Command::ReplayPrompt => {
                    let _ = store.prompt("fix the null check");
                }
            }
        }
    }

    /// A store with one open, empty session "s1".
    fn booted_store() -> Store {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer());
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: 2,
            result: serde_json::json!({"sessionId": "s1"}),
        });
        let _ = store.take_outgoing();
        store
    }

    /// A session update frame for `s1`.
    fn update(params: Value) -> Frame {
        Frame::Notification {
            method: "session/update".to_owned(),
            params: serde_json::json!({"sessionId": "s1", "update": params}),
        }
    }

    /// A user chunk frame for `s1`.
    fn user_chunk(text: &str) -> Frame {
        update(serde_json::json!({
            "sessionUpdate": "user_message_chunk",
            "content": {"type": "text", "text": text},
        }))
    }

    /// An assistant chunk frame for `s1`.
    fn agent_chunk(text: &str) -> Frame {
        update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text},
        }))
    }

    /// A notice update in `tone`.
    fn notice(tone: &str, text: &str) -> Frame {
        update(serde_json::json!({
            "sessionUpdate": "_kage/notice", "tone": tone, "text": text,
        }))
    }

    /// A config option update advertising the mode, swarm and goal
    /// options.
    fn options_frame() -> Frame {
        update(serde_json::json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [
                {"id": "mode", "name": "Mode", "category": "mode", "type": "select",
                 "currentValue": "default",
                 "options": [
                     {"value": "default", "name": "Default"},
                     {"value": "plan", "name": "Plan"},
                 ]},
                {"id": "swarm", "name": "Swarm", "type": "select", "currentValue": "off",
                 "options": [
                     {"value": "off", "name": "Off"},
                     {"value": "on", "name": "On"},
                 ]},
                {"id": "goal", "name": "Goal", "type": "text", "currentValue": "",
                 "options": []},
            ],
        }))
    }

    /// An available commands update carrying one command.
    fn commands_frame() -> Frame {
        update(serde_json::json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {"name": "review", "description": "review the diff"},
            ],
        }))
    }

    /// The outgoing requests drained, as method and params.
    fn drain_requests(
        store: &Entity<Store>,
        visual: &mut VisualTestContext,
    ) -> Vec<(String, Value)> {
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store
                    .take_outgoing()
                    .into_iter()
                    .filter_map(|frame| match frame {
                        Frame::Request { method, params, .. } => Some((method, params)),
                        _ => None,
                    })
                    .collect()
            })
        })
    }

    /// Binds the chrome keys the launch registers, so a press routes
    /// the way it does in the app.
    fn bind_chrome_keys(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.bind_keys([
                gpui_kit::KeyBinding::new("enter", super::FindNext, Some("Find")),
                gpui_kit::KeyBinding::new("shift-enter", super::FindPrev, Some("Find")),
                gpui_kit::KeyBinding::new("escape", super::FindClose, Some("Find")),
                gpui_kit::KeyBinding::new("up", super::PaletteUp, Some("Palette")),
                gpui_kit::KeyBinding::new("down", super::PaletteDown, Some("Palette")),
                gpui_kit::KeyBinding::new("enter", super::PaletteRun, Some("Palette")),
                gpui_kit::KeyBinding::new("escape", super::PaletteClose, Some("Palette")),
            ]);
        });
    }

    #[test]
    fn find_matches_over_row_text_case_insensitively() {
        let rows = vec![
            (super::RowKey::Item(0), "Read main.rs".to_owned()),
            (super::RowKey::Item(1), "Ran cargo test".to_owned()),
            (super::RowKey::Item(2), "the CARGO build passes".to_owned()),
        ];
        let hits = find_matches(&rows, "cargo");
        assert_eq!(hits, vec![1, 2], "case folding matches both rows");
        assert!(find_matches(&rows, "").is_empty(), "empty query, no hits");
        assert!(find_matches(&rows, "rustc").is_empty());
    }

    #[test]
    fn stepping_wraps_both_ways_and_stays_put_without_matches() {
        assert_eq!(step_match(0, 3, false), 1);
        assert_eq!(step_match(2, 3, false), 0, "forward wraps");
        assert_eq!(step_match(0, 3, true), 2, "back wraps");
        assert_eq!(step_match(1, 0, false), 0);
        assert_eq!(step_match(1, 0, true), 0);
    }

    #[test]
    fn the_counter_is_one_based_and_reports_zero_of_zero() {
        assert_eq!(counter_text(0, 3), "1 / 3");
        assert_eq!(counter_text(2, 3), "3 / 3");
        assert_eq!(counter_text(0, 0), "0 / 0");
    }

    #[test]
    fn palette_entries_order_commands_then_sessions_and_narrow() {
        let commands = vec![serde_json::json!({
            "name": "review", "description": "review the diff",
        })];
        let sessions = vec![
            ("s1".to_owned(), Some("fix the null check".to_owned()), true),
            ("s2".to_owned(), None, true),
            ("rec1".to_owned(), None, false),
        ];
        let all = palette_entries(&commands, &sessions, "");
        assert_eq!(all.len(), 4);
        assert_eq!(
            all[0],
            PaletteEntry::Command {
                name: "review".into(),
                description: "review the diff".into(),
            }
        );
        assert_eq!(all[0].badge(), "command");
        assert_eq!(all[1].badge(), "session");
        assert_eq!(all[3].badge(), "recorded");
        assert_eq!(all[2].label(), "untitled session");

        let narrowed = palette_entries(&commands, &sessions, "rev");
        assert_eq!(narrowed.len(), 1);
        let by_id = palette_entries(&commands, &sessions, "s2");
        assert_eq!(by_id.len(), 1, "sessions narrow over their id too");
        assert_eq!(by_id[0].badge(), "session");
        assert!(palette_entries(&commands, &sessions, "zzz").is_empty());
    }

    #[test]
    fn the_answered_elsewhere_change_raises_a_toast_that_follows_the_session() {
        let changes = vec![Change::AnsweredElsewhere {
            id: "s1".to_owned(),
            request_id: 101,
        }];
        let drafts = toasts_for_changes(&changes);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].tone, NoticeTone::Info);
        assert_eq!(drafts[0].action, ToastAction::ActivateSession("s1".into()));
        assert!(toasts_for_changes(&[]).is_empty());
    }

    #[test]
    fn the_notice_watch_raises_each_new_notice_once() {
        let mut store = booted_store();
        let mut watch = NoticeWatch::default();
        assert!(
            watch.scan(store.state()).is_empty(),
            "the first scan sees history, not live notices"
        );
        store.absorb(notice("warn", "context is filling up"));
        let drafts = watch.scan(store.state());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].tone, NoticeTone::Warn);
        assert_eq!(drafts[0].text, "context is filling up");
        assert!(
            watch.scan(store.state()).is_empty(),
            "the same notice never raises twice"
        );
        store.absorb(notice("error", "the turn failed"));
        store.absorb(notice("success", "goal met: shipped"));
        let drafts = watch.scan(store.state());
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].tone, NoticeTone::Error);
        assert_eq!(drafts[1].tone, NoticeTone::Success);
    }

    /// A test root that mounts the find bar over its transcript.
    struct FindHost {
        find: Entity<FindBar>,
        transcript: Entity<TranscriptView>,
    }

    impl gpui_kit::Render for FindHost {
        fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
            v_flex()
                .size_full()
                .child(self.find.clone())
                .child(self.transcript.clone())
        }
    }

    /// The entities a window builder hands back to the test.
    type Captured = Rc<RefCell<Option<(Entity<FindBar>, Entity<TranscriptView>)>>>;

    /// Opens a window with the find bar over a transcript of `store`.
    fn find_window(
        cx: &mut TestAppContext,
        store: Entity<Store>,
    ) -> (
        Entity<FindBar>,
        Entity<TranscriptView>,
        &mut VisualTestContext,
    ) {
        cx.update(gpui_kit::init);
        let captured: Captured = Rc::default();
        let cap = captured.clone();
        let (_, visual) = cx.add_window_view(move |window: &mut Window, cx| {
            let composer = cx.new(|cx| TextareaState::new(window, cx));
            let transcript = cx.new(|cx| TranscriptView::new(store.clone(), composer, cx));
            let find = cx.new(|cx| FindBar::new(store.clone(), transcript.clone(), window, cx));
            cap.borrow_mut().replace((find.clone(), transcript.clone()));
            FindHost { find, transcript }
        });
        let (find, transcript) = captured.borrow().clone().expect("the host was built");
        (find, transcript, visual)
    }

    #[gpui_kit::test]
    fn find_matches_steps_through_the_counter_and_closes(cx: &mut TestAppContext) {
        bind_chrome_keys(cx);
        let store = cx.new(|_| booted_store());
        visual_feed(
            &store,
            cx,
            vec![
                user_chunk("please look at the cargo build"),
                agent_chunk("the cargo build passes now"),
            ],
        );
        let (find, transcript, visual) = find_window(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));

        visual.update(|window, cx| {
            find.update(cx, |find, cx| find.open(window, cx));
        });
        visual.update(|window, cx| window.input("cargo", cx));
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, _| {
            assert_eq!(window.find("find-counter").label(), Some("1 / 2"));
            assert!(
                window
                    .try_find(ElementId::named_usize("find-hit", 0))
                    .is_some(),
                "the matching rows tint"
            );
            assert!(
                window
                    .try_find(ElementId::named_usize("find-hit", 1))
                    .is_some(),
                "the matching rows tint"
            );
        });

        visual.update(|window, cx| window.press("enter", cx));
        assert_eq!(
            visual.update(|_, cx| find.read(cx).counter()),
            (1, 2),
            "enter steps to the second match"
        );
        visual.update(|window, cx| window.press("shift-enter", cx));
        assert_eq!(
            visual.update(|_, cx| find.read(cx).counter()),
            (0, 2),
            "shift-enter steps back"
        );

        visual.update(|window, cx| window.press("escape", cx));
        assert!(
            visual.update(|_, cx| !find.read(cx).is_open()),
            "esc closes the bar"
        );
        visual.update(|window, cx| window.render_frame(cx));
        assert!(
            visual.update(|window, _| window
                .try_find(ElementId::named_usize("find-hit", 0))
                .is_none()),
            "a closed bar clears the tint"
        );
        drop(transcript);
    }

    /// Feeds `frames` into `store` the way the shell does.
    fn visual_feed(store: &Entity<Store>, cx: &mut TestAppContext, frames: Vec<Frame>) {
        cx.update(|cx| {
            store.update(cx, |store, _| {
                for frame in frames {
                    store.absorb(frame);
                }
            });
        });
    }

    /// A store with two open sessions "s1" (active) and "s2", and one
    /// agent command.
    fn two_session_store() -> Store {
        let mut store = booted_store();
        store.new_session();
        store.absorb(Frame::Success {
            id: 3,
            result: serde_json::json!({"sessionId": "s2"}),
        });
        let _ = store.take_outgoing();
        store.absorb(commands_frame());
        store
    }

    #[gpui_kit::test]
    fn the_palette_narrows_and_runs_commands_and_sessions(cx: &mut TestAppContext) {
        bind_chrome_keys(cx);
        let store = cx.new(|_| two_session_store());
        let (palette, visual) = palette_window(cx, store.clone());
        visual.update(|_, cx| {
            assert_eq!(palette.read(cx).entries(cx).len(), 3);
        });
        visual.update(|window, cx| {
            palette.update(cx, |palette, cx| palette.open(window, cx));
        });
        assert!(visual.update(|_, cx| palette.read(cx).is_open()));

        visual.update(|window, cx| window.input("rev", cx));
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, _| {
            assert!(
                window.try_find("palette-entry-0").is_some(),
                "the command survives the query"
            );
            assert!(
                window.try_find("palette-entry-1").is_none(),
                "the query narrowed the sessions out"
            );
        });
        visual.update(|window, cx| window.press("enter", cx));
        assert!(
            visual.update(|_, cx| !palette.read(cx).is_open()),
            "a run closes the palette"
        );
        visual.update(|_, cx| {
            assert_eq!(
                store.read(cx).draft("s1"),
                Some("/review "),
                "a command fills the draft with its text"
            );
            let textarea = palette.read(cx).textarea();
            assert_eq!(textarea.read(cx).value().trim_end(), "/review");
        });

        visual.update(|window, cx| {
            palette.update(cx, |palette, cx| palette.open(window, cx));
        });
        visual.update(|window, cx| window.input("s2", cx));
        visual.update(|window, cx| window.press("down", cx));
        visual.update(|window, cx| window.press("up", cx));
        visual.update(|window, cx| window.press("enter", cx));
        assert_eq!(
            visual.update(|_, cx| store.read(cx).active_id().map(str::to_owned)),
            Some("s2".to_owned()),
            "enter on the highlighted session follows it"
        );
    }

    /// Opens a window on a palette over `store` with its own composer
    /// textarea.
    fn palette_window(
        cx: &mut TestAppContext,
        store: Entity<Store>,
    ) -> (Entity<PaletteView>, &mut VisualTestContext) {
        let captured: Rc<RefCell<Option<Entity<PaletteView>>>> = Rc::default();
        let cap = captured.clone();
        cx.update(gpui_kit::init);
        let (_, visual) = cx.add_window_view(move |window: &mut Window, cx| {
            let composer = cx.new(|cx| TextareaState::new(window, cx));
            let palette =
                cx.new(|cx| PaletteView::new(store.clone(), composer.clone(), window, cx));
            cap.borrow_mut().replace(palette.clone());
            PaletteHost { palette }
        });
        let palette = captured.borrow().clone().expect("the palette was built");
        (palette, visual)
    }

    /// A test root that mounts a palette.
    struct PaletteHost {
        palette: Entity<PaletteView>,
    }

    impl gpui_kit::Render for PaletteHost {
        fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
            div().size_full().child(self.palette.clone())
        }
    }

    #[gpui_kit::test]
    fn toasts_expire_on_their_own_and_clicks_run_the_action(cx: &mut TestAppContext) {
        let store = cx.new(|_| two_session_store());
        cx.update(gpui_kit::init);
        let (toasts, visual) = cx.add_window_view(|_, _| Toasts::new(store.clone()));
        visual.update(|_, cx| {
            toasts.update(cx, |toasts, cx| {
                toasts.push(
                    ToastDraft {
                        tone: NoticeTone::Warn,
                        text: "context is filling up".to_owned(),
                        action: ToastAction::None,
                    },
                    cx,
                );
                toasts.push(
                    ToastDraft {
                        tone: NoticeTone::Info,
                        text: "an approval was answered in another session".to_owned(),
                        action: ToastAction::ActivateSession("s2".to_owned()),
                    },
                    cx,
                );
            });
        });
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, _| {
            assert_eq!(
                window.find("toast-0").label(),
                Some("context is filling up")
            );
            assert_eq!(
                window.find("toast-1").label(),
                Some("an approval was answered in another session")
            );
        });

        visual.update(|window, cx| window.click("toast-1", cx));
        visual.update(|_, cx| {
            assert_eq!(
                store.read(cx).active_id().map(str::to_owned),
                Some("s2".to_owned()),
                "the click followed the session"
            );
            assert_eq!(toasts.read(cx).items().len(), 1, "the click dismissed");
        });

        visual
            .executor()
            .advance_clock(super::TOAST_TTL + Duration::from_millis(100));
        visual.run_until_parked();
        visual.update(|window, cx| window.render_frame(cx));
        assert!(
            visual.update(|_, cx| toasts.read(cx).items().is_empty()),
            "the last toast expired on its own"
        );
        assert!(
            visual.update(|window, _| window.try_find("toast-0").is_none()),
            "the stack renders nothing after expiry"
        );
    }

    #[gpui_kit::test]
    fn welcome_cards_render_and_a_card_sets_its_option_and_fills_the_draft(
        cx: &mut TestAppContext,
    ) {
        let mut store = booted_store();
        store.absorb(options_frame());
        let _ = store.take_outgoing();
        let store = cx.new(|_| store);
        let (welcome, visual) = welcome_window(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, _| {
            assert!(window.try_find("welcome-wordmark").is_some());
            assert!(window.try_find("welcome-card-plan").is_some());
            assert!(window.try_find("welcome-card-swarm").is_some());
            assert!(window.try_find("welcome-card-goal").is_some());
        });

        visual.update(|window, cx| window.click("welcome-card-plan", cx));
        let frames = drain_requests(&store, visual);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, "session/set_config_option");
        assert_eq!(frames[0].1["configId"], "mode");
        assert_eq!(frames[0].1["value"], "plan");
        visual.update(|_, cx| {
            assert_eq!(
                store.read(cx).draft("s1"),
                Some(super::WELCOME_PLAN_DRAFT),
                "the card filled the session draft"
            );
            assert_eq!(
                welcome.read(cx).textarea().read(cx).value(),
                super::WELCOME_PLAN_DRAFT,
                "the card filled the composer"
            );
        });
    }

    #[gpui_kit::test]
    fn a_card_clicked_before_a_session_applies_to_the_first_one(cx: &mut TestAppContext) {
        let store = cx.new(|_| Store::new("/w", false));
        let (welcome, visual) = welcome_window(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, cx| window.click("welcome-card-swarm", cx));
        assert!(
            drain_requests(&store, visual).is_empty(),
            "no session, no option frame yet"
        );

        visual.update(|_, cx| {
            store.update(cx, |store, cx| {
                store.set_connect(State::Connected);
                for command in store.take_commands() {
                    match command {
                        Command::Handshake { replay_sessions } => store.handshake(replay_sessions),
                        Command::NewSession => store.new_session(),
                        Command::ReplayPrompt => {
                            let _ = store.prompt("fix the null check");
                        }
                    }
                }
                let _ = store.take_outgoing();
                store.absorb(init_answer());
                let _ = store.take_outgoing();
                store.new_session();
                let _ = store.take_outgoing();
                store.absorb(Frame::Success {
                    id: 2,
                    result: serde_json::json!({"sessionId": "s1"}),
                });
                store.absorb(options_frame());
                cx.notify();
            });
        });
        let frames = drain_requests(&store, visual);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1["configId"], "swarm");
        assert_eq!(frames[0].1["value"], "on");
        visual.update(|_, cx| {
            assert_eq!(
                store.read(cx).draft("s1"),
                Some(super::WELCOME_SWARM_DRAFT),
                "the held card filled the first session's draft"
            );
        });
        drop(welcome);
    }

    /// Opens a window on a welcome pane over `store` with its own
    /// composer textarea.
    fn welcome_window(
        cx: &mut TestAppContext,
        store: Entity<Store>,
    ) -> (Entity<WelcomeView>, &mut VisualTestContext) {
        let captured: Rc<RefCell<Option<Entity<WelcomeView>>>> = Rc::default();
        let cap = captured.clone();
        cx.update(gpui_kit::init);
        let (_, visual) = cx.add_window_view(move |window: &mut Window, cx| {
            let composer = cx.new(|cx| TextareaState::new(window, cx));
            let welcome =
                cx.new(|cx| WelcomeView::new(store.clone(), composer.clone(), window, cx));
            cap.borrow_mut().replace(welcome.clone());
            WelcomeHost { welcome }
        });
        let welcome = captured.borrow().clone().expect("the welcome was built");
        (welcome, visual)
    }

    /// A test root that mounts a welcome pane.
    struct WelcomeHost {
        welcome: Entity<WelcomeView>,
    }

    impl gpui_kit::Render for WelcomeHost {
        fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
            v_flex().size_full().child(self.welcome.clone())
        }
    }
}
