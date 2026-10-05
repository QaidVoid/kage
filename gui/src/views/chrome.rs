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

use gpui_kit::StyledImage as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::theme::{ActiveTheme as _, ThemeColor};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Div, Entity, EventEmitter, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, Window, div, px, relative,
};
use kage_client::wire::NoticeTone;
use kage_client::{Change, State, TranscriptItem};
use serde_json::Value;

use crate::store::Store;
use crate::theme::{
    FONT_DISPLAY, FONT_MONO, FS_2XS, FS_SM, FS_XS, R_LG, R_MD, R_XL, WEIGHT_EXTRABOLD,
    WEIGHT_SEMIBOLD, WELCOME_PAD_BOTTOM_SHARE, WELCOME_PAD_TOP, WELCOME_PAD_X, WELCOME_W,
};
use crate::views::deferred::{Deferred, LaidOut};
use crate::views::transcript::{FindMarks, RowKey, TranscriptView};
use gpui_kit::base::ElementExt as _;

gpui_kit::actions!(kage_desktop, [FindNext, FindPrev, FindClose]);
gpui_kit::actions!(
    kage_desktop,
    [PaletteUp, PaletteDown, PaletteRun, PaletteClose]
);

/// The bordered keyboard hint chip of the design: 18px tall with
/// 10.5px type, as `.kbd` draws it.
fn kbd_chip(label: &str, p: &crate::theme::Palette) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(18.))
        .px(px(5.))
        .rounded(px(5.))
        .border_1()
        .border_color(p.line)
        .text_size(px(10.5))
        .font_family(FONT_MONO)
        .text_color(p.faint)
        .whitespace_nowrap()
        .child(SharedString::from(label.to_owned()))
}

/// The small rounded meta chip of the design's badges.
fn badge_chip(label: &str, p: &crate::theme::Palette) -> Div {
    crate::views::kit::badge(label.to_owned(), p)
        .mt(px(2.))
        .whitespace_nowrap()
}

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
    /// Bring an archived session back.
    Restore(String),
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
/// toast follows the session on click. A request the agent refused
/// toasts its error.
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
            Change::Failed { error, .. } => Some(ToastDraft {
                tone: NoticeTone::Error,
                text: error.message.clone(),
                action: ToastAction::None,
            }),
            _ => None,
        })
        .collect()
}

/// Tracks how many notice items each session held at the last scan,
/// so a frame that adds notices raises its toast once. A session's
/// existing notices are history at first sight and stay quiet, and so
/// are the ones a load replays before the session opens.
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
            if !session.opened {
                *entry = notices.len();
                continue;
            }
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
    /// The query text, held until the field has been laid out. The bar
    /// mounts only when it opens, so its first write lands on an element
    /// that has never been laid out; see [`crate::views::deferred`].
    query_mirror: Deferred,
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
            query_mirror: Deferred::new(),
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
        let query = self.query.clone();
        self.query_mirror.set(String::new(), |text| {
            query.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        query.update(cx, |state, cx| state.focus(window, cx));
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
        let query = self.query.clone();
        self.query_mirror.set(String::new(), |text| {
            query.update(cx, |state, cx| state.set_value(text, window, cx));
        });
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
    pub fn step(&mut self, back: bool, cx: &mut Context<Self>) {
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The query text held while the field was unlaid lands here, on
        // the first render after the field's element prepainted.
        let state = self.query.clone();
        self.query_mirror.flush(|text| {
            state.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        if !self.open {
            return div().into_any_element();
        }
        let p = crate::theme::Palette::active(cx);
        let this = cx.entity();
        let laid_out = self.query_mirror.laid_out().flag();
        let release = cx.entity().downgrade();
        let total = self.matches.len();
        let step = move |this: &Entity<Self>, back: bool, cx: &mut App| {
            this.update(cx, |this, cx| this.step(back, cx));
        };
        let prev = this.clone();
        let next = this.clone();
        let counter = SharedString::from(counter_text(self.current, total));
        let mut pop = h_flex()
            .id("find-bar")
            .test_support()
            .key_context("Find")
            .w(px(340.))
            .px(px(8.))
            .py(px(6.))
            .gap(px(8.))
            .items_center()
            .rounded(px(R_LG))
            .border_1()
            .border_color(p.line)
            .bg(p.menu)
            .on_prepaint({
                let laid_out = laid_out.clone();
                move |_, _, cx| {
                    laid_out.set(true);
                    let _ = release.update(cx, |_, cx| cx.notify());
                }
            })
            .on_action(cx.listener(|this, _: &FindNext, _, cx| this.step(false, cx)))
            .on_action(cx.listener(|this, _: &FindPrev, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &FindClose, window, cx| {
                this.close(window, cx);
                cx.emit(FindEvent::Closed);
            }))
            .child(
                Icon::new(IconName::Search)
                    .with_size(px(14.))
                    .text_color(p.faint),
            )
            .child(
                crate::views::kit::input(&self.query)
                    .flex_1()
                    .appearance(false)
                    .bordered(false)
                    .text_color(p.ink),
            )
            .child(
                div()
                    .id("find-counter")
                    .test_support()
                    .flex_none()
                    .text_size(px(FS_2XS))
                    .font_family(FONT_MONO)
                    .text_color(p.faint)
                    .aria_label(counter.clone())
                    .child(counter),
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
            );
        pop.style().box_shadow = Some(p.shadow_menu.clone());
        div()
            .w_full()
            .flex()
            .justify_end()
            .px(px(10.))
            .py(px(6.))
            .child(pop)
            .into_any_element()
    }
}

/// An app command the palette offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppAction {
    /// Show the welcome pane for a fresh session.
    NewSession,
    /// Turn swarm mode on or off.
    Swarm,
    /// Turn plan mode on or off.
    Plan,
    /// Set or clear the goal.
    Goal,
    /// Show or hide the workbench.
    Workbench,
    /// Show or hide the sidebar.
    Sidebar,
    /// Show the agents list.
    Agents,
    /// Show the files the session changed.
    Changes,
    /// Open the settings.
    Settings,
    /// Open the theme cards.
    Theme,
}

impl AppAction {
    /// Runs the command through its window action, which the shell
    /// handles.
    pub fn dispatch(self, window: &mut Window, cx: &mut App) {
        use crate::app::{
            NewSession, OpenSettings, ReviewChanges, SetGoal, ShowAgents, TogglePlan,
            ToggleSidebar, ToggleSwarm, ToggleWorkbench,
        };
        let action: Box<dyn gpui_kit::Action> = match self {
            Self::NewSession => Box::new(NewSession),
            Self::Swarm => Box::new(ToggleSwarm),
            Self::Plan => Box::new(TogglePlan),
            Self::Goal => Box::new(SetGoal),
            Self::Workbench => Box::new(ToggleWorkbench),
            Self::Sidebar => Box::new(ToggleSidebar),
            Self::Agents => Box::new(ShowAgents),
            Self::Changes => Box::new(ReviewChanges),
            Self::Settings | Self::Theme => Box::new(OpenSettings),
        };
        window.dispatch_action(action, cx);
    }
}

/// The app commands for a palette over `session`: the session ones
/// only while a session shows, named for the state they flip.
#[must_use]
pub fn app_actions(session: Option<&kage_client::Session>, plan_on: bool) -> Vec<PaletteEntry> {
    let action = |action, label: &str, keys: Option<&'static str>| PaletteEntry::Action {
        action,
        label: label.to_owned(),
        keys,
    };
    let mut out = vec![action(AppAction::NewSession, "New session", Some("Ctrl N"))];
    if let Some(session) = session {
        let swarm = session
            .config_options
            .iter()
            .any(|option| option.id == "swarm" && option.current_value == "on");
        out.push(action(
            AppAction::Swarm,
            if swarm {
                "Turn swarm mode off"
            } else {
                "Turn swarm mode on"
            },
            None,
        ));
        out.push(action(
            AppAction::Plan,
            if plan_on {
                "Turn plan mode off"
            } else {
                "Turn plan mode on"
            },
            None,
        ));
        out.push(action(AppAction::Goal, "Set goal", None));
    }
    out.push(action(
        AppAction::Workbench,
        "Toggle workbench",
        Some("Ctrl B"),
    ));
    out.push(action(
        AppAction::Sidebar,
        "Toggle sidebar",
        Some("Ctrl \\"),
    ));
    if session.is_some() {
        out.push(action(AppAction::Agents, "Show agents", None));
        out.push(action(AppAction::Changes, "Review changes", None));
    }
    out.push(action(AppAction::Settings, "Settings", Some("Ctrl ,")));
    out.push(action(AppAction::Theme, "Change theme", None));
    out
}

/// One command palette row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteEntry {
    /// An app command.
    Action {
        /// What runs.
        action: AppAction,
        /// The row text.
        label: String,
        /// Its shortcut, when it has one.
        keys: Option<&'static str>,
    },
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
            Self::Action { label, .. } => label.clone(),
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
            Self::Action { .. } | Self::Command { .. } => "command",
            Self::Session { open: true, .. } => "session",
            Self::Session { open: false, .. } => "recorded",
        }
    }

    /// The detail line a row carries beside its label.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Action { .. } => String::new(),
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

/// The icon an app command's row leads with.
fn action_icon(action: AppAction) -> IconName {
    match action {
        AppAction::NewSession => IconName::SquarePen,
        AppAction::Swarm => IconName::Waypoints,
        AppAction::Plan => IconName::ListTodo,
        AppAction::Goal => IconName::Target,
        AppAction::Workbench => IconName::PanelRight,
        AppAction::Sidebar => IconName::PanelLeft,
        AppAction::Agents => IconName::Users,
        AppAction::Changes => IconName::FileDiff,
        AppAction::Settings => IconName::Settings,
        AppAction::Theme => IconName::Moon,
    }
}

/// The command palette: a modal over the agent's commands and the
/// session directory. The query narrows, the arrows move, Enter runs
/// and Esc closes.
pub struct PaletteView {
    /// Where the focus goes when this closes.
    focus_return: crate::views::kit::FocusReturn,
    store: Entity<Store>,
    composer: Entity<TextareaState>,
    query: Entity<InputState>,
    /// The query text, held until the field has been laid out. The
    /// palette mounts only when it opens, so its first write lands on an
    /// element that has never been laid out; see
    /// [`crate::views::deferred`].
    query_mirror: Deferred,
    /// The command text handed to the composer, held on the composer's
    /// own layout, because the composer is what renders that element.
    composer_mirror: Deferred,
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
        composer_laid_out: LaidOut,
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
            focus_return: crate::views::kit::FocusReturn::default(),
            store,
            composer,
            query,
            query_mirror: Deferred::new(),
            composer_mirror: Deferred::after(composer_laid_out),
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
            .filter(|(_, session)| session.parent.is_none())
            .map(|(id, _)| (id.clone(), Some(store.display_title(id)), true))
            .collect();
        for info in &state.directory {
            if state.sessions.contains_key(&info.session_id) {
                continue;
            }
            sessions.push((info.session_id.clone(), info.title.clone(), false));
        }
        let needle = query.to_lowercase();
        let mut entries: Vec<PaletteEntry> = app_actions(store.active_session(), store.plan_on())
            .into_iter()
            .filter(|entry| needle.is_empty() || entry.label().to_lowercase().contains(&needle))
            .collect();
        // Recent sessions are a short list; a query widens it a little.
        let cap = if needle.is_empty() { 5 } else { 8 };
        let mut found = palette_entries(commands, &sessions, &query);
        let commands = found
            .iter()
            .take_while(|entry| matches!(entry, PaletteEntry::Command { .. }))
            .count();
        found.truncate(commands + cap);
        entries.extend(found);
        entries
    }

    /// Opens the palette over a fresh query and focuses it.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.selected = 0;
        self.focus_return.remember(window, cx);
        let query = self.query.clone();
        self.query_mirror.set(String::new(), |text| {
            query.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        query.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Closes the palette and hands the focus back.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.focus_return.restore(cx);
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
            PaletteEntry::Action { action, .. } => {
                self.close(cx);
                action.dispatch(window, cx);
                return;
            }
            PaletteEntry::Command { name, .. } => {
                let text = format!("/{name} ");
                let active = self.store.read(cx).active_id().map(str::to_owned);
                self.store
                    .update(cx, |store, _| store.set_draft(active.as_deref(), &text));
                let composer = self.composer.clone();
                self.composer_mirror.set(text, |text| {
                    composer.update(cx, |state, cx| state.set_value(text, window, cx));
                });
                self.composer
                    .update(cx, |state, cx| state.focus(window, cx));
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The query text held while the field was unlaid lands here, on
        // the first render after the field's element prepainted.
        let state = self.query.clone();
        self.query_mirror.flush(|text| {
            state.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        if !self.open {
            // A command run while the palette closes still hands its text
            // to the composer, so the flush runs whether the palette is
            // open or not.
            let composer = self.composer.clone();
            self.composer_mirror.flush(|text| {
                composer.update(cx, |state, cx| state.set_value(text, window, cx));
            });
            return div().into_any_element();
        }
        let p = crate::theme::Palette::active(cx);
        let laid_out = self.query_mirror.laid_out().flag();
        let release = cx.entity().downgrade();
        let entries = self.entries(cx);
        if self.selected >= entries.len() {
            self.selected = 0;
        }
        let selected = self.selected;
        let this = cx.entity();
        let query = self.query.read(cx).value().trim().to_lowercase();
        let top: f32 = (window.viewport_size().height * 0.12).into();

        let mut list = v_flex()
            .id("palette-list")
            .max_h(px(420.))
            .overflow_y_scroll()
            .p(px(6.));
        if entries.is_empty() {
            list = list.child(
                div()
                    .id("palette-empty")
                    .test_support()
                    .px(px(24.))
                    .py(px(24.))
                    .text_size(px(FS_SM))
                    .text_color(p.faint)
                    .child("Nothing matches"),
            );
        }
        let mut last_kind = "";
        for (ix, entry) in entries.iter().enumerate() {
            let this = this.clone();
            let kind = match entry {
                PaletteEntry::Action { .. } | PaletteEntry::Command { .. } => "command",
                PaletteEntry::Session { .. } => "session",
            };
            if kind != last_kind {
                last_kind = kind;
                let label = match kind {
                    "session" if query.is_empty() => "RECENT SESSIONS",
                    "session" => "SESSIONS",
                    _ => "COMMANDS",
                };
                list = list.child(
                    div()
                        .px(px(9.))
                        .pt(px(6.))
                        .pb(px(3.))
                        .text_size(px(FS_2XS))
                        .font_weight(WEIGHT_SEMIBOLD)
                        .text_color(p.faint)
                        .child(label),
                );
            }
            let (icon, mono_label) = match entry {
                PaletteEntry::Action { action, .. } => (action_icon(*action), false),
                PaletteEntry::Command { .. } => (IconName::Command, true),
                PaletteEntry::Session { .. } => (IconName::MessageSquare, false),
            };
            let mut label_row = div()
                .min_w_0()
                .truncate()
                .text_size(px(if mono_label { 12.5 } else { FS_SM }))
                .text_color(p.ink)
                .child(SharedString::from(entry.label()));
            if mono_label {
                label_row = label_row.font_family(FONT_MONO);
            }
            let row = h_flex()
                .id(SharedString::from(format!("palette-entry-{ix}")))
                .test_support()
                .w_full()
                .px(px(9.))
                .py(px(7.))
                .gap(px(10.))
                .items_start()
                .rounded(px(R_MD))
                .map(|row| {
                    if ix == selected {
                        row.bg(p.selected)
                    } else {
                        row
                    }
                })
                .hover(|row| row.bg(p.selected))
                .aria_label(SharedString::from(format!(
                    "{} {}",
                    entry.badge(),
                    entry.label()
                )))
                .on_click(move |_, window, cx| {
                    this.update(cx, |this, cx| this.run_index(ix, window, cx));
                })
                .child(
                    Icon::new(icon)
                        .mt(px(2.))
                        .with_size(px(14.))
                        .text_color(p.faint),
                )
                .child({
                    let detail = entry.detail();
                    v_flex().flex_1().min_w_0().child(label_row).when(
                        !detail.is_empty(),
                        |column| {
                            column.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(FS_XS))
                                    .text_color(p.muted)
                                    .child(SharedString::from(detail)),
                            )
                        },
                    )
                })
                .child(match entry {
                    PaletteEntry::Action { keys, .. } => {
                        keys.map_or_else(div, |keys| kbd_chip(keys, p))
                    }
                    _ => badge_chip(entry.badge(), p),
                });
            list = list.child(row);
        }

        let mut dialog = v_flex()
            .id("palette")
            .test_support()
            .key_context("Palette")
            .w(px(620.))
            .max_h(px(520.))
            .overflow_hidden()
            .bg(p.bg)
            .border_1()
            .border_color(p.line)
            .rounded(px(R_XL))
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
            .on_action(cx.listener(|this, _: &PaletteClose, window, cx| {
                this.close(cx);
                this.composer
                    .update(cx, |state, cx| state.focus(window, cx));
            }))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(14.))
                    .border_b_1()
                    .border_color(p.subtle)
                    .text_color(p.faint)
                    .on_prepaint({
                        let laid_out = laid_out.clone();
                        move |_, _, cx| {
                            laid_out.set(true);
                            let _ = release.update(cx, |_, cx| cx.notify());
                        }
                    })
                    .child(Icon::new(IconName::Search).with_size(px(16.)))
                    .child(
                        crate::views::kit::input(&self.query)
                            .flex_1()
                            .appearance(false)
                            .bordered(false)
                            .text_size(px(15.))
                            .text_color(p.ink_strong),
                    ),
            )
            .child(list)
            .child(
                h_flex()
                    .items_center()
                    .gap(px(14.))
                    .px(px(16.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(p.subtle)
                    .text_size(px(FS_2XS))
                    .text_color(p.faint)
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(4.))
                            .child(kbd_chip("\u{2191}\u{2193}", p))
                            .child("move"),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(4.))
                            .child(kbd_chip("Enter", p))
                            .child("run"),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(4.))
                            .child(kbd_chip("Esc", p))
                            .child("close"),
                    ),
            );
        dialog.style().box_shadow = Some(p.shadow_2.clone());
        let overlay = div()
            .id("palette-overlay")
            .absolute()
            .inset_0()
            .occlude()
            .bg(Hsla {
                h: 0.,
                s: 0.,
                l: 0.,
                a: 0.45,
            })
            .flex()
            .justify_center()
            .items_start()
            .pt(px(top))
            .child(dialog);
        gpui_kit::deferred(overlay)
            .with_priority(2)
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
        match action {
            Some(ToastAction::ActivateSession(session)) => {
                self.store.update(cx, |store, cx| {
                    store.set_active(session);
                    cx.notify();
                });
            }
            Some(ToastAction::Restore(session)) => {
                self.store.update(cx, |store, cx| {
                    store.restore(&session);
                    cx.notify();
                });
            }
            _ => {}
        }
        self.remove(id, cx);
    }
}

impl Render for Toasts {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::active(cx);
        let this = cx.entity();
        let raised: Vec<(usize, ToastDraft)> = self
            .items
            .iter()
            .map(|toast| (toast.id, toast.draft.clone()))
            .collect();
        let mut root = div()
            .absolute()
            .bottom(px(42.))
            .right(px(16.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .items_end();
        for (id, draft) in raised {
            let this = this.clone();
            let (icon, color) = match draft.tone {
                NoticeTone::Success => (IconName::CircleCheck, p.ok),
                NoticeTone::Error => (IconName::CircleX, p.danger),
                NoticeTone::Warn => (IconName::TriangleAlert, p.warn),
                NoticeTone::Info => (IconName::Info, p.accent),
            };
            let mut card = h_flex()
                .id(SharedString::from(format!("toast-{id}")))
                .test_support()
                .min_w(px(260.))
                .max_w(px(380.))
                .px(px(12.))
                .py(px(10.))
                .gap(px(10.))
                .items_start()
                .rounded(px(R_LG))
                .border_1()
                .border_color(p.line)
                .bg(p.raised)
                .cursor_pointer()
                .hover(|toast| toast.border_color(p.line_strong))
                .aria_label(SharedString::from(draft.text.clone()))
                .on_click(move |_, _, cx| {
                    this.update(cx, |this, cx| this.clicked(id, cx));
                })
                .child(
                    Icon::new(icon)
                        .mt(px(2.))
                        .with_size(px(14.))
                        .text_color(color),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(FS_SM))
                        .text_color(p.ink)
                        .child(SharedString::from(draft.text)),
                );
            card.style().box_shadow = Some(p.shadow_2.clone());
            root = root.child(card);
        }
        root
    }
}

/// One suggestion card: what it fills the draft with and the one
/// config option it sets, when the card carries a mode.
struct Suggestion {
    id: &'static str,
    icon: IconName,
    text: &'static str,
    /// The option the card turns on, as the web client's cards do:
    /// swarm for the audit, plan for the planning prompt. `None` for
    /// the plain suggestions, which only fill the draft.
    option: Option<(&'static str, &'static str)>,
    /// The tag pill the card trails, if any: `swarm` or `plan`.
    tag: Option<&'static str>,
}

/// The cards of the welcome pane, the web client's four suggestions.
/// Every option here is one the agent can advertise; a card sets
/// nothing the session was never offered.
const SUGGESTIONS: [Suggestion; 4] = [
    Suggestion {
        id: "welcome-card-fix",
        icon: IconName::Lightbulb,
        text: "Find the failing tests and fix them",
        option: None,
        tag: None,
    },
    Suggestion {
        id: "welcome-card-swarm",
        icon: IconName::Waypoints,
        text: "Review every module for unhandled errors",
        option: Some(("swarm", "on")),
        tag: Some("swarm"),
    },
    Suggestion {
        id: "welcome-card-plan",
        icon: IconName::ListTodo,
        text: "Plan the next change before touching code",
        option: Some(("mode", "plan")),
        tag: Some("plan"),
    },
    Suggestion {
        id: "welcome-card-delegate",
        icon: IconName::Bot,
        text: "Delegate a docs check to subagents",
        option: None,
        tag: None,
    },
];

/// The welcome pane: shown when no session is active. A wordmark, the
/// composer, then suggestion cards over the real config options. The
/// composer is the shell's own entity mounted here rather than in the
/// bottom band, which is where it goes once a session exists, so the
/// draft survives the move. A card fills the composer and holds its
/// option for the session the send opens, which sets it before the
/// prompt runs when the agent advertised it.
pub struct WelcomeView {
    store: Entity<Store>,
    composer: Entity<TextareaState>,
    /// The composer element, mounted here so the wordmark reads into the
    /// input rather than over the cards.
    composer_view: Entity<crate::views::composer::ComposerView>,
    /// The card text to hand the composer, held until the composer has
    /// been laid out. A card raised before the first frame would
    /// otherwise reach an engine still holding its construction font; see
    /// [`crate::views::deferred`].
    fill_mirror: Deferred,
    /// The dialog layer, for picking a folder by path.
    dialog: Entity<crate::views::dialog::DialogView>,
    /// Whether the project picker shows.
    project_open: bool,
    /// Where the project chip sits, recorded as it prepaints.
    chip_bounds: std::rc::Rc<std::cell::Cell<Option<gpui_kit::Bounds<gpui_kit::Pixels>>>>,
}

impl WelcomeView {
    /// A welcome pane over `store`, filling the shared composer draft.
    pub fn new(
        store: Entity<Store>,
        composer: Entity<TextareaState>,
        composer_view: Entity<crate::views::composer::ComposerView>,
        dialog: Entity<crate::views::dialog::DialogView>,
        composer_laid_out: LaidOut,
    ) -> Self {
        Self {
            store,
            composer,
            composer_view,
            fill_mirror: Deferred::after(composer_laid_out),
            dialog,
            project_open: false,
            chip_bounds: std::rc::Rc::default(),
        }
    }

    /// Picks the folder the next session opens in: the platform's
    /// folder dialog natively, a path typed into a dialog in the
    /// browser, which has no picker for the server's files, or when no
    /// platform picker answers.
    fn open_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.project_open = false;
        #[cfg(not(target_arch = "wasm32"))]
        {
            let chosen = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some("Open folder".into()),
            });
            cx.spawn_in(window, async move |this, cx| {
                let picked = match chosen.await {
                    Ok(Ok(Some(paths))) => paths.first().map(|path| path.display().to_string()),
                    // No platform picker answered: say why, and type the
                    // path instead.
                    Ok(Err(err)) => {
                        eprintln!("kage-desktop: the folder dialog failed: {err:#}");
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.dialog.update(cx, |dialog, cx| {
                                dialog.open(
                                    crate::views::dialog::DialogKind::OpenFolder,
                                    window,
                                    cx,
                                );
                            });
                        });
                        None
                    }
                    _ => None,
                };
                if let Some(path) = picked {
                    let _ = this.update(cx, |this, cx| {
                        this.store
                            .update(cx, |store, _| store.set_project(Some(path)));
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        #[cfg(target_arch = "wasm32")]
        self.dialog.update(cx, |dialog, cx| {
            dialog.open(crate::views::dialog::DialogKind::OpenFolder, window, cx);
        });
        cx.notify();
    }

    /// The project picker's rows: every directory a session worked in,
    /// most recent first, then the folder action.
    fn project_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::active(cx);
        let store = self.store.read(cx);
        let current = store.session_dir().map(str::to_owned);
        let this = cx.entity();
        let mut menu = v_flex().w(px(320.)).p(px(5.)).child(
            div()
                .px(px(9.))
                .pt(px(6.))
                .pb(px(3.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(FS_2XS))
                .text_color(p.faint)
                .child("PROJECT"),
        );
        for dir in store.projects().into_iter().take(8) {
            let checked = current.as_deref() == Some(dir.as_str());
            let pick = this.clone();
            let chosen = dir.clone();
            menu = menu.child(
                h_flex()
                    .id(SharedString::from(format!("project-{dir}")))
                    .px(px(9.))
                    .py(px(7.))
                    .gap(px(10.))
                    .items_center()
                    .rounded(px(R_MD))
                    .cursor_pointer()
                    .hover(move |row| row.bg(p.selected))
                    .on_click(move |_, _, cx| {
                        let chosen = chosen.clone();
                        pick.update(cx, |this, cx| {
                            this.project_open = false;
                            this.store
                                .update(cx, |store, _| store.set_project(Some(chosen)));
                            cx.notify();
                        });
                    })
                    .child(
                        Icon::new(IconName::Folder)
                            .with_size(px(14.))
                            .text_color(p.muted),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(FS_SM))
                                    .text_color(p.ink)
                                    .child(crate::app::project_name(Some(&dir))),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(FS_XS))
                                    .text_color(p.faint)
                                    .child(SharedString::from(dir.clone())),
                            ),
                    )
                    .when(checked, |row| {
                        row.child(
                            Icon::new(IconName::Check)
                                .with_size(px(14.))
                                .text_color(p.accent),
                        )
                    }),
            );
        }
        let open = this.clone();
        menu.child(div().h(px(1.)).mx(px(2.)).my(px(4.)).bg(p.subtle))
            .child(
                h_flex()
                    .id("project-open-folder")
                    .px(px(9.))
                    .py(px(7.))
                    .gap(px(10.))
                    .items_center()
                    .rounded(px(R_MD))
                    .cursor_pointer()
                    .hover(move |row| row.bg(p.selected))
                    .on_click(move |_, window, cx| {
                        open.update(cx, |this, cx| this.open_folder(window, cx));
                    })
                    .child(
                        Icon::new(IconName::Plus)
                            .with_size(px(14.))
                            .text_color(p.muted),
                    )
                    .child(
                        div()
                            .text_size(px(FS_SM))
                            .text_color(p.ink)
                            .child("Open folder\u{2026}"),
                    ),
            )
    }

    /// The composer textarea the cards fill.
    #[must_use]
    pub fn textarea(&self) -> &Entity<TextareaState> {
        &self.composer
    }

    /// Fills the composer with the card's text and holds its option for
    /// the session the send opens.
    fn suggestion(&mut self, card: &Suggestion, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((id, value)) = card.option {
            self.store
                .update(cx, |store, _| store.hold_option(id, value));
        }
        let composer = self.composer.clone();
        self.fill_mirror.set(card.text.to_owned(), |text| {
            composer.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        composer.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }
}

impl Render for WelcomeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Card text held while the composer was unlaid lands here, on
        // the first render after the composer's element prepainted. The
        // flag is the composer's own: this view does not lay that element
        // out, so a prepaint here would say nothing about it.
        let composer = self.composer.clone();
        self.fill_mirror.flush(|text| {
            composer.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        let p = crate::theme::Palette::active(cx);
        // Under 860px the web client stacks the cards in one column and
        // pads the pane 14px, as `@media (max-width: 860px)` does.
        let narrow = window.viewport_size().width <= px(860.);
        let this = cx.entity();

        // The suggestion grid: two columns of cards, as the web
        // client's `.suggest` lays them out.
        let mut cards = v_flex()
            .id("welcome-cards")
            .test_support()
            .w_full()
            .grid()
            .grid_cols(if narrow { 1 } else { 2 })
            .gap(px(8.))
            .mt(px(16.));
        for suggestion in &SUGGESTIONS {
            let this = this.clone();
            let (tag_fg, tag_bg) = if suggestion.tag == Some("swarm") {
                (p.done, p.done_soft)
            } else {
                (p.accent, p.accent_soft)
            };
            let mut card = h_flex()
                .id(suggestion.id)
                .test_support()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .py(px(10.))
                .rounded(px(R_LG))
                .border_1()
                .border_color(p.line)
                .text_size(px(FS_SM))
                .text_color(p.muted)
                .cursor_pointer()
                .hover(move |card| {
                    card.bg(p.hover)
                        .border_color(p.line_strong)
                        .text_color(p.ink)
                })
                .on_click(move |_, window, cx| {
                    this.update(cx, |this, cx| this.suggestion(suggestion, window, cx));
                })
                .child(
                    Icon::new(suggestion.icon)
                        .with_size(px(14.))
                        .text_color(p.faint),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .line_height(relative(1.5))
                        .child(suggestion.text),
                );
            if let Some(label) = suggestion.tag {
                card = card.child(
                    div()
                        .flex_none()
                        .px(px(7.))
                        .py(px(1.))
                        .rounded_full()
                        .text_size(px(FS_2XS))
                        .text_color(tag_fg)
                        .bg(tag_bg)
                        .child(label),
                );
            }
            cards = cards.child(card);
        }

        // The project pill above the composer: the folder and the name
        // of the directory the next session opens in.
        let project = crate::app::project_name(self.store.read(cx).session_dir());
        let proj_picker = h_flex()
            .id("welcome-proj")
            .mb(px(8.))
            .ml(px(12.))
            .h(px(28.))
            .px(px(10.))
            .gap(px(7.))
            .items_center()
            .self_start()
            .rounded(px(R_MD))
            .bg(p.surface)
            .text_size(px(FS_SM))
            .text_color(p.ink)
            .cursor_pointer()
            .hover(move |chip| chip.bg(p.hover))
            .on_prepaint({
                let bounds = self.chip_bounds.clone();
                move |at, _, _| bounds.set(Some(at))
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.project_open = !this.project_open;
                cx.notify();
            }))
            .child(
                Icon::new(IconName::Folder)
                    .with_size(px(14.))
                    .text_color(p.muted),
            )
            .child(project)
            .child(
                Icon::new(IconName::ChevronDown)
                    .with_size(px(12.))
                    .text_color(p.faint),
            );
        let project_menu = self
            .chip_bounds
            .get()
            .filter(|_| self.project_open)
            .map(|at| {
                let close = cx.entity();
                let mut surface = div()
                    .occlude()
                    .bg(p.bg)
                    .border_1()
                    .border_color(p.line)
                    .rounded(px(R_LG))
                    .overflow_hidden()
                    .on_mouse_down_out(move |event, _, cx| {
                        if at.contains(&event.position) {
                            return;
                        }
                        close.update(cx, |this, cx| {
                            this.project_open = false;
                            cx.notify();
                        });
                    })
                    .child(self.project_menu(cx));
                surface.style().box_shadow = Some(p.shadow_menu.clone());
                gpui_kit::deferred(
                    gpui_kit::anchored()
                        .anchor(gpui_kit::Anchor::TopLeft)
                        .position(gpui_kit::point(at.left(), at.bottom() + px(6.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(surface),
                )
                .with_priority(1)
            });
        // A flex row, so the chip keeps its own width instead of
        // stretching across the column.
        let proj_picker = h_flex().w_full().child(proj_picker).children(project_menu);

        // The wordmark: the kanji glyph over its hard offset shadow,
        // then the name, as the web client's `.wordmark` draws them.
        // The face carries the design's real orb gradient, pre-rendered
        // per palette, because the toolkit's SVG element paints one flat
        // color only.
        let face = if cx.theme().mode.is_dark() {
            crate::assets::GLYPH_FACE_SHADOW
        } else {
            crate::assets::GLYPH_FACE_DAWN
        };
        let glyph_shadow = gpui_kit::svg()
            .path(crate::assets::GLYPH_PATH)
            .size(px(76.))
            .flex_none()
            .text_color(p.accent_soft);
        let wordmark_shadow = div()
            .absolute()
            .top(px(6.))
            .left(px(6.))
            .child(glyph_shadow);
        let wordmark_face = div()
            .relative()
            .child(gpui_kit::img(face).size(px(76.)).flex_none());

        let hint = h_flex()
            .id("welcome-hint")
            .test_support()
            .flex_none()
            .items_center()
            .mt(px(18.))
            .text_size(px(FS_XS))
            .text_color(p.faint)
            .child(kbd_chip("Ctrl K", p).mr(px(4.)))
            .child("commands and sessions");

        let column = v_flex()
            .w_full()
            .max_w(px(WELCOME_W))
            .flex_none()
            .items_center()
            .mt_auto()
            .mb_auto()
            .child(
                h_flex()
                    .id("welcome-wordmark")
                    .test_support()
                    .items_center()
                    .gap(px(16.))
                    .mb(px(30.))
                    .child(div().relative().child(wordmark_shadow).child(wordmark_face))
                    .child(
                        div()
                            .relative()
                            .child(
                                div()
                                    .absolute()
                                    .top(px(4.2))
                                    .left(px(4.2))
                                    .font_family(FONT_DISPLAY)
                                    .font_weight(WEIGHT_EXTRABOLD)
                                    .text_size(px(60.))
                                    .line_height(relative(1.))
                                    .text_color(p.accent_soft)
                                    .child("kage"),
                            )
                            .child(
                                div()
                                    .relative()
                                    .font_family(FONT_DISPLAY)
                                    .font_weight(WEIGHT_EXTRABOLD)
                                    .text_size(px(60.))
                                    .line_height(relative(1.))
                                    .text_color(p.ink_strong)
                                    .child("kage"),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .child(proj_picker)
                    .child(div().w_full().child(self.composer_view.clone())),
            )
            .child(cards)
            .child(hint);

        // The design centres the stack in the pane less a 40px top pad and
        // a 12vh bottom pad, which lifts the stack above the middle. The
        // bottom pad is a share of the viewport, so it is measured rather
        // than a constant. Auto margins do the centring: they collapse to
        // zero when the stack is taller than the padded pane, which pins
        // the stack to the 40px pad instead of spilling it over the
        // topbar, as the web client's scroll container resolves it.
        let viewport = window.viewport_size().height / px(1.0);
        let glow = if cx.theme().mode.is_dark() {
            crate::assets::WELCOME_GLOW
        } else {
            crate::assets::WELCOME_GLOW_DAWN
        };
        v_flex()
            .id("welcome")
            .test_support()
            .size_full()
            .relative()
            .items_center()
            .pt(px(WELCOME_PAD_TOP))
            .pb(px(viewport * WELCOME_PAD_BOTTOM_SHARE))
            .px(px(if narrow { 14. } else { WELCOME_PAD_X }))
            // The web client's `.welcome` radial glow, behind the stack.
            // Fill stretches the unit-space render over the pane, which
            // reproduces the ellipse at any viewport size.
            .child(
                div().absolute().inset_0().child(
                    gpui_kit::img(glow)
                        .size_full()
                        .object_fit(gpui_kit::ObjectFit::Fill),
                ),
            )
            .child(column)
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
        TestAppContext, VisualTestContext, Window, div, px,
    };
    use serde_json::Value;

    use super::{
        FindBar, LaidOut, NoticeWatch, PaletteEntry, PaletteView, SUGGESTIONS, ToastAction,
        ToastDraft, Toasts, WelcomeView, counter_text, find_matches, palette_entries, step_match,
        toasts_for_changes,
    };
    use crate::store::{Command, Store, StoreHandle as _};
    use crate::theme::{WELCOME_PAD_BOTTOM_SHARE, WELCOME_PAD_TOP, WELCOME_W};
    use crate::transport::State;
    use crate::views::composer::ComposerView;
    use crate::views::transcript::TranscriptView;
    use gpui_kit::base::ElementExt as _;
    use gpui_kit::component::input::Textarea;
    use kage_client::wire::NoticeTone;
    use kage_client::{Change, Frame, RequestId};

    /// An initialize answer with everything the gate accepts.
    fn init_answer() -> Frame {
        Frame::Success {
            id: RequestId::Number(1),
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
        let _ = store.take_outgoing();
        store.new_session();
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: RequestId::Number(3),
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
            request_id: RequestId::Number(101),
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

    #[test]
    fn a_loaded_history_toasts_nothing_and_later_notices_toast() {
        let mut store = booted_store();
        let mut watch = NoticeWatch::default();
        let _ = watch.scan(store.state());
        store.set_active("r1");
        let Some(Frame::Request { id: load, .. }) = store.take_outgoing().pop() else {
            panic!("the recorded session loads");
        };
        let notice_on = |text: &str| Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({
                "sessionId": "r1",
                "update": {"sessionUpdate": "_kage/notice", "tone": "warn", "text": text},
            }),
        };
        store.absorb(notice_on("an old warning"));
        assert!(watch.scan(store.state()).is_empty(), "a replayed notice");
        store.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({}),
        });
        assert!(watch.scan(store.state()).is_empty());
        store.absorb(notice_on("a new warning"));
        let drafts = watch.scan(store.state());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].text, "a new warning");
    }

    #[test]
    fn a_refused_request_toasts_its_error() {
        let changes = vec![Change::Failed {
            request: 7,
            error: kage_client::RpcError {
                code: -32603,
                message: "no provider configured".into(),
                data: None,
            },
        }];
        let drafts = toasts_for_changes(&changes);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].tone, NoticeTone::Error);
        assert_eq!(drafts[0].text, "no provider configured");
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
            let transcript = cx.new(|cx| TranscriptView::new(store.clone(), composer, window, cx));
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
        // The opening query write waits for the bar's element to prepaint
        // and lands on the render after that, so the bar takes a frame
        // before the test types into it. The field itself is focused at
        // once, because focusing is not a text write.
        visual.update(|window, cx| window.render_frame(cx));
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
            id: RequestId::Number(4),
            result: serde_json::json!({"sessionId": "s2"}),
        });
        store.set_active("s1");
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
            let entries = palette.read(cx).entries(cx);
            let actions = entries
                .iter()
                .filter(|entry| matches!(entry, PaletteEntry::Action { .. }))
                .count();
            assert_eq!(
                actions, 10,
                "the app commands lead, the session ones included"
            );
            assert_eq!(
                entries.len() - actions,
                3,
                "then the command and both sessions"
            );
        });
        visual.update(|window, cx| {
            palette.update(cx, |palette, cx| palette.open(window, cx));
        });
        assert!(visual.update(|_, cx| palette.read(cx).is_open()));
        // The opening query write waits for the bar's element to prepaint
        // and lands on the render after that, so the bar takes a frame
        // before the test types into it. The field itself is focused at
        // once, because focusing is not a text write.
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, cx| window.input("/rev", cx));
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
            let composer_laid_out = LaidOut::new();
            let palette = cx.new(|cx| {
                PaletteView::new(
                    store.clone(),
                    composer.clone(),
                    composer_laid_out.clone(),
                    window,
                    cx,
                )
            });
            cap.borrow_mut().replace(palette.clone());
            PaletteHost {
                palette,
                composer,
                composer_laid_out,
            }
        });
        let palette = captured.borrow().clone().expect("the palette was built");
        (palette, visual)
    }

    /// A test root that mounts a palette.
    /// A test root that mounts the palette over the composer it fills.
    ///
    /// The composer is mounted because the palette's fill waits on that
    /// textarea's element laying out, which the shell's arrangement is what
    /// makes possible.
    struct PaletteHost {
        palette: Entity<PaletteView>,
        composer: Entity<TextareaState>,
        composer_laid_out: LaidOut,
    }

    impl gpui_kit::Render for PaletteHost {
        fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(self.palette.clone())
                .child(Textarea::new(&self.composer))
                .on_prepaint({
                    let laid_out = self.composer_laid_out.clone();
                    move |_, _, _| laid_out.mark()
                })
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

    /// A connected store with no session, which is the state the welcome
    /// pane is for.
    fn sessionless_store() -> Store {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer());
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store
    }

    /// The welcome pane mounts the composer inside its own column, so the
    /// wordmark reads into the input rather than over the cards. The
    /// browser only reaches this state when the agent reports no session,
    /// which a live `kage serve` never does, so the layout is checked here.
    #[gpui_kit::test]
    fn the_welcome_pane_carries_the_composer(cx: &mut TestAppContext) {
        let store = cx.new(|_| sessionless_store());
        let (welcome, visual) = welcome_window(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, _| {
            assert!(
                window.try_find("composer").is_some(),
                "the composer is mounted in the welcome column"
            );
        });
        // The composer sits below the wordmark and above the cards, so it
        // reads as part of the welcome stack.
        visual.update(|window, _| {
            let wordmark = window
                .try_find("welcome-wordmark")
                .expect("the wordmark is on screen")
                .bounds();
            let composer = window
                .try_find("composer")
                .expect("the composer is on screen")
                .bounds();
            let cards = window
                .try_find("welcome-cards")
                .expect("the cards are on screen")
                .bounds();
            assert!(
                wordmark.origin.y < composer.origin.y,
                "the wordmark is above the composer"
            );
            assert!(
                composer.origin.y < cards.origin.y,
                "the composer is above the cards, not pinned to the window bottom"
            );
        });
        drop(welcome);
    }

    /// The welcome block measures to the design's column and the cards sit
    /// in the design's two-up grid. The browser only reaches this state
    /// when the agent reports no session, which a live `kage serve` never
    /// does, so the layout is checked here against measured bounds.
    #[gpui_kit::test]
    fn the_welcome_block_measures_to_the_design_column(cx: &mut TestAppContext) {
        let store = cx.new(|_| sessionless_store());
        let (welcome, visual) = welcome_window(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, cx| window.render_frame(cx));

        let mut bounds = |id: &'static str| {
            visual.update(|window, _| {
                window
                    .try_find(id)
                    .unwrap_or_else(|| panic!("{id} is on screen"))
                    .bounds()
            })
        };
        let composer = bounds("composer");
        let cards = bounds("welcome-cards");
        let fix = bounds("welcome-card-fix");
        let swarm = bounds("welcome-card-swarm");
        let plan = bounds("welcome-card-plan");
        let delegate = bounds("welcome-card-delegate");
        let hint = bounds("welcome-hint");

        // The composer and the cards span the design's column, and the
        // hint is centred in it rather than spread across it.
        assert_eq!(
            composer.size.width,
            px(WELCOME_W),
            "the composer is the column wide"
        );
        assert_eq!(
            cards.size.width,
            px(WELCOME_W),
            "the cards are the column wide"
        );
        assert!(
            hint.size.width < cards.size.width,
            "the hint is one centred line, not a row spread across the column"
        );

        // Two columns in the design's order: fix and swarm share the
        // first row, plan and delegate the second.
        assert_eq!(fix.origin.y, swarm.origin.y, "the first cards share a row");
        assert!(
            fix.origin.x < swarm.origin.x,
            "the swarm card is beside fix"
        );
        assert_eq!(
            plan.origin.y, delegate.origin.y,
            "the second cards share a row"
        );
        assert!(plan.origin.x < delegate.origin.x);
        assert!(fix.origin.y < plan.origin.y, "the rows stack in order");
        assert!(
            plan.size.width < cards.size.width,
            "a card is one half of the grid, not the whole row"
        );
        assert!(
            cards.origin.y + cards.size.height <= hint.origin.y,
            "the hint sits below the cards"
        );
        drop(welcome);
    }

    /// The design pads the welcome pane 40px at the top and 12vh at the
    /// bottom, so the stack sits above the middle by exactly the
    /// difference between those two pads. That difference is the whole
    /// invariant and holds whatever height the content turns out to be.
    #[gpui_kit::test]
    fn the_welcome_stack_sits_above_the_pane_middle(cx: &mut TestAppContext) {
        let store = cx.new(|_| sessionless_store());
        let (welcome, visual) = welcome_window(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, _| {
            let wordmark = window
                .try_find("welcome-wordmark")
                .expect("the wordmark is on screen")
                .bounds();
            let hint = window
                .try_find("welcome-hint")
                .expect("the hint is on screen")
                .bounds();
            let pane = window.viewport_size().height / px(1.0);
            let top = wordmark.origin.y / px(1.0);
            let bottom = pane - (hint.origin.y + hint.size.height) / px(1.0);
            let design = pane * WELCOME_PAD_BOTTOM_SHARE - WELCOME_PAD_TOP;
            assert!(
                (bottom - top - design).abs() < 2.0,
                "the bottom gap {bottom} minus the top gap {top} is the design's \
                 {design}, which is what lifts the stack above the middle"
            );
        });
        drop(welcome);
    }

    /// A connected store on the welcome pane, with no session yet.
    fn welcome_store() -> Store {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer());
        let _ = store.take_outgoing();
        store
    }

    #[gpui_kit::test]
    fn welcome_cards_render_and_a_card_fills_the_composer(cx: &mut TestAppContext) {
        let store = cx.new(|_| welcome_store());
        let (welcome, visual) = welcome_window(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, _| {
            assert!(window.try_find("welcome-wordmark").is_some());
            assert!(window.try_find("welcome-card-plan").is_some());
            assert!(window.try_find("welcome-card-swarm").is_some());
            assert!(window.try_find("welcome-card-fix").is_some());
            assert!(window.try_find("welcome-card-delegate").is_some());
        });

        visual.update(|window, cx| window.click("welcome-card-plan", cx));
        visual.update(|window, cx| window.render_frame(cx));
        assert!(
            drain_requests(&store, visual).is_empty(),
            "no session yet, so nothing goes out"
        );
        visual.update(|_, cx| {
            assert_eq!(
                welcome.read(cx).textarea().read(cx).value(),
                SUGGESTIONS[2].text,
                "the card filled the composer"
            );
        });
    }

    #[gpui_kit::test]
    fn a_card_option_is_set_before_the_prompt_it_rides_with(cx: &mut TestAppContext) {
        let store = cx.new(|_| welcome_store());
        let (welcome, visual) = welcome_window(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, cx| window.click("welcome-card-plan", cx));
        visual.update(|_, cx| {
            store.act(cx, |store| store.open_with_prompt(SUGGESTIONS[2].text));
        });
        let opened = visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store
                    .take_outgoing()
                    .into_iter()
                    .find_map(|frame| match frame {
                        Frame::Request { id, method, .. } if method == "session/new" => Some(id),
                        _ => None,
                    })
                    .expect("the send opens a session")
            })
        });
        visual.update(|_, cx| {
            store.act(cx, |store| {
                store.absorb(Frame::Success {
                    id: opened,
                    result: serde_json::json!({
                        "sessionId": "s1",
                        "configOptions": [{
                            "id": "mode", "name": "Mode", "type": "select",
                            "currentValue": "default",
                            "options": [
                                {"value": "default", "name": "Default"},
                                {"value": "plan", "name": "Plan"},
                            ],
                        }],
                    }),
                })
            });
        });
        let frames = drain_requests(&store, visual);
        let methods: Vec<&str> = frames.iter().map(|(method, _)| method.as_str()).collect();
        assert_eq!(methods, ["session/set_config_option", "session/prompt"]);
        assert_eq!(frames[0].1["configId"], "mode");
        assert_eq!(frames[0].1["value"], "plan");
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
            let dialog =
                cx.new(|cx| crate::views::dialog::DialogView::new(store.clone(), window, cx));
            let composer_view =
                cx.new(|cx| ComposerView::new(store.clone(), dialog.clone(), window, cx));
            let composer = composer_view.read(cx).input().clone();
            let composer_laid_out = composer_view.read(cx).input_laid_out();
            let welcome = cx.new(|_| {
                WelcomeView::new(
                    store.clone(),
                    composer.clone(),
                    composer_view.clone(),
                    dialog.clone(),
                    composer_laid_out.clone(),
                )
            });
            cap.borrow_mut().replace(welcome.clone());
            // The composer_view and composer_laid_out handles stay alive
            // through the welcome entity, which holds both.
            drop(composer_view);
            WelcomeHost { welcome }
        });
        let welcome = captured.borrow().clone().expect("the welcome was built");
        (welcome, visual)
    }

    /// A test root that mounts a welcome pane over the composer it fills.
    ///
    /// The composer is mounted because the welcome pane's fill waits on the
    /// composer's element laying out, which is the shell's arrangement and
    /// the only one where the fill can land.
    struct WelcomeHost {
        welcome: Entity<WelcomeView>,
    }

    impl gpui_kit::Render for WelcomeHost {
        fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
            v_flex().size_full().child(self.welcome.clone())
        }
    }
}
