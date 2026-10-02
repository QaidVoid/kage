//! The workbench: the right panel as the kage web client design
//! draws it. A head of tabs over the active session's file changes,
//! its file listing, the announced agents, the shell calls and the
//! fetched pages, read straight from the session transcript.

use std::collections::HashSet;

use gpui_kit::AnyElement;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::TextareaState;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Div, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled as _, Window, div, px,
};
use kage_client::wire::{ToolCallContent, ToolCallStatus, ToolKind};
use kage_client::{ToolCallItem, TranscriptItem};

use crate::app::ToggleWorkbench;
use crate::store::{Store, StoreHandle as _};
use crate::theme::{FONT_MONO, FS_2XS, FS_SM, FS_XS, PANEL_HEAD_H, R_FULL, R_MD, WEIGHT_SEMIBOLD};
use crate::views::agents::{agent_facts, avatar, meta_line, state_chip, tokens};
use crate::views::kit::{self, BtnTone};
use crate::views::transcript::TranscriptView;

/// What a file change row reports, collected from one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangeEntry {
    /// The tool call the entry came from, so the pane can select it.
    pub call_id: String,
    /// The file path the change names.
    pub path: String,
    /// Lines only the new text has.
    pub add: usize,
    /// Lines only the old text has.
    pub del: usize,
    /// Whether the first change made the file: its diff named no old
    /// text.
    pub created: bool,
    /// The subagent that made the change, when one did.
    pub by: Option<String>,
}

/// The path one change names: the diff content's path first, then the
/// tool input's `path`, then the call title.
#[must_use]
pub(crate) fn change_path(call: &ToolCallItem) -> String {
    for content in &call.content {
        if let ToolCallContent::Diff(diff) = content {
            return diff.path.clone();
        }
    }
    if let Some(path) = call
        .input
        .as_ref()
        .and_then(|input| input.get("path"))
        .and_then(|path| path.as_str())
    {
        return path.to_owned();
    }
    call.title.clone()
}

/// The changed files the transcript holds: the edit, delete and move
/// calls, in transcript order, folded onto their paths.
#[must_use]
pub(crate) fn change_entries(items: &[TranscriptItem]) -> Vec<ChangeEntry> {
    let mut out: Vec<ChangeEntry> = Vec::new();
    for item in items {
        let TranscriptItem::ToolCall(call) = item else {
            continue;
        };
        if !matches!(
            call.kind,
            ToolKind::Edit | ToolKind::Delete | ToolKind::Move
        ) {
            continue;
        }
        let (add, del) = crate::views::transcript::diff_lines(call)
            .map(|lines| {
                lines.iter().fold((0, 0), |(add, del), line| match line {
                    crate::views::transcript::DiffLine::Add(_) => (add + 1, del),
                    crate::views::transcript::DiffLine::Del(_) => (add, del + 1),
                    _ => (add, del),
                })
            })
            .unwrap_or((0, 0));
        let path = change_path(call);
        if let Some(entry) = out.iter_mut().find(|entry| entry.path == path) {
            entry.add += add;
            entry.del += del;
            continue;
        }
        let created = call.content.iter().any(
            |content| matches!(content, ToolCallContent::Diff(diff) if diff.old_text.is_none()),
        );
        out.push(ChangeEntry {
            call_id: call.tool_call_id.clone(),
            path,
            add,
            del,
            created,
            by: None,
        });
    }
    out
}

/// The files `session` and the subagents under it changed: the
/// session's own edits, then each child's, a child's named by its agent.
/// A path two of them touched keeps its first author and sums the lines.
pub(crate) fn session_changes(store: &Store, session: &kage_client::Session) -> Vec<ChangeEntry> {
    let mut out = change_entries(&session.items);
    for (id, agent) in &session.agents {
        let Some(child) = store.state().session(id) else {
            continue;
        };
        for mut entry in change_entries(&child.items) {
            if let Some(known) = out.iter_mut().find(|known| known.path == entry.path) {
                known.add += entry.add;
                known.del += entry.del;
                continue;
            }
            entry.by = agent.name.clone();
            out.push(entry);
        }
    }
    out
}

/// The tool call `id` of `session` or of a child under it.
fn find_call<'a>(
    store: &'a Store,
    session: &'a kage_client::Session,
    id: &str,
) -> Option<&'a ToolCallItem> {
    let own = std::iter::once(session);
    let children = session
        .agents
        .keys()
        .filter_map(|child| store.state().session(child));
    own.chain(children).find_map(|session| {
        session.items.iter().find_map(|item| match item {
            TranscriptItem::ToolCall(call) if call.tool_call_id == id => Some(call),
            _ => None,
        })
    })
}

/// How many shell calls `session` and the subagents under it ran.
fn session_run_count(store: &Store, session: &kage_client::Session) -> usize {
    let shells = |items: &[TranscriptItem]| {
        items
            .iter()
            .filter(|item| {
                matches!(item, TranscriptItem::ToolCall(call) if call.kind == ToolKind::Execute)
            })
            .count()
    };
    shells(&session.items)
        + session
            .agents
            .keys()
            .filter_map(|id| store.state().session(id))
            .map(|child| shells(&child.items))
            .sum::<usize>()
}

/// The shell calls `session` and the subagents under it ran: the
/// session's own, then each child's, named by its agent.
fn session_runs(store: &Store, session: &kage_client::Session) -> Vec<TermEntry> {
    let mut out = term_entries(&session.items);
    for (id, agent) in &session.agents {
        let Some(child) = store.state().session(id) else {
            continue;
        };
        out.extend(term_entries(&child.items).into_iter().map(|mut run| {
            run.who = agent.name.clone();
            run
        }));
    }
    out
}

/// `text` without its ANSI escape sequences: a run of styled output
/// shows as its plain characters, never as markup.
pub(crate) fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then one final byte in @..~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ST.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                        if c == '\u{1b}' {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// How many output lines an open run shows, from the end.
const TERM_TAIL: usize = 40;

/// One shell call's facts, collected from the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TermEntry {
    /// The tool call id, which keys the run's open state.
    pub id: String,
    /// The command the call ran.
    pub command: String,
    /// The last [`TERM_TAIL`] lines of output, without the shell
    /// tool's `stdout:` and `exit:` framing.
    pub output: String,
    /// How many output lines the tail leaves out.
    pub hidden: usize,
    /// The exit code, from the raw output or the `exit:` line.
    pub exit: Option<i64>,
    /// Whether the call is still running.
    pub running: bool,
    /// Whether the call failed.
    pub failed: bool,
    /// The subagent that ran it, when one did.
    pub who: Option<String>,
}

/// The shell tool's text without its framing: the output with the
/// `stdout:` header and the `(no output)` stand-in dropped, and the
/// exit code its closing `exit: N` line names.
fn shell_text(text: &str) -> (&str, Option<i64>) {
    let (body, exit) = match text.rsplit_once("\nexit: ") {
        Some((body, code)) if !code.contains('\n') => (body, code.trim().parse().ok()),
        _ => (text, None),
    };
    let body = body.strip_prefix("stdout:\n").unwrap_or(body);
    let body = if body == "(no output)" { "" } else { body };
    (body.trim_end(), exit)
}

/// The last [`TERM_TAIL`] lines of `text`, and how many come before.
fn tail(text: &str) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let hidden = lines.len().saturating_sub(TERM_TAIL);
    (lines[hidden..].join("\n"), hidden)
}

/// The shell calls the transcript holds, in transcript order.
#[must_use]
pub(crate) fn term_entries(items: &[TranscriptItem]) -> Vec<TermEntry> {
    let mut out = Vec::new();
    for item in items {
        let TranscriptItem::ToolCall(call) = item else {
            continue;
        };
        if call.kind != ToolKind::Execute {
            continue;
        }
        let command = call
            .input
            .as_ref()
            .and_then(|input| input.get("command"))
            .and_then(|command| command.as_str())
            .unwrap_or(&call.title)
            .to_owned();
        let text = call.text();
        let (body, text_exit) = shell_text(&text);
        let exit = call
            .raw_output
            .as_ref()
            .and_then(|output| {
                output
                    .get("exitCode")
                    .or_else(|| output.get("exit_code"))
                    .and_then(|code| code.as_i64())
            })
            .or(text_exit);
        let (output, hidden) = tail(&strip_ansi(body));
        out.push(TermEntry {
            id: call.tool_call_id.clone(),
            command: command.trim().to_owned(),
            output,
            hidden,
            exit,
            running: matches!(
                call.status,
                ToolCallStatus::Pending | ToolCallStatus::InProgress
            ),
            failed: call.status == ToolCallStatus::Failed,
            who: None,
        });
    }
    out
}

/// One fetched page's facts, collected from the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchEntry {
    /// The URL the call fetched, as its title carries it.
    pub url: String,
    /// The reader text the call streamed, if any.
    pub text: String,
}

/// The remote fetches the transcript holds, in transcript order.
#[must_use]
pub(crate) fn fetch_entries(items: &[TranscriptItem]) -> Vec<FetchEntry> {
    let mut out = Vec::new();
    for item in items {
        let TranscriptItem::ToolCall(call) = item else {
            continue;
        };
        if call.kind != ToolKind::Fetch {
            continue;
        }
        // The final URL after redirects, when the call reported one.
        let url = call
            .raw_output
            .as_ref()
            .and_then(|output| output.get("url"))
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| change_path(call), str::to_owned);
        out.push(FetchEntry {
            url,
            text: call.text(),
        });
    }
    out
}

/// The events the workbench raises to the shell.
pub enum WorkbenchEvent {
    /// The user picked a file in the files pane; the shell mentions it
    /// in the composer.
    Mention(String),
}

/// The panes the workbench tabs switch between.
#[derive(Clone, Copy, Default, PartialEq)]
enum Tab {
    /// The file changes the session made.
    #[default]
    Changes,
    /// The session workdir's listing.
    Files,
    /// The announced subagents.
    Agents,
    /// The shell calls and their output.
    Terminal,
    /// The pages the session fetched.
    Browser,
    /// One subagent and its transcript.
    Agent,
}

impl Tab {
    /// The tabs in head order.
    fn all() -> [Tab; 5] {
        [
            Tab::Changes,
            Tab::Files,
            Tab::Agents,
            Tab::Terminal,
            Tab::Browser,
        ]
    }

    /// The label the tab carries.
    fn label(self) -> &'static str {
        match self {
            Tab::Changes => "Changes",
            Tab::Files => "Files",
            Tab::Agents => "Agents",
            Tab::Terminal => "Terminal",
            Tab::Browser => "Browser",
            // The detail tab names its agent instead.
            Tab::Agent => "",
        }
    }

    /// The icon the tab leads with.
    fn icon(self) -> IconName {
        match self {
            Tab::Changes => IconName::FileDiff,
            Tab::Files => IconName::Folder,
            Tab::Agents => IconName::Users,
            Tab::Terminal => IconName::Terminal,
            Tab::Browser => IconName::Globe,
            Tab::Agent => IconName::Bot,
        }
    }
}

/// The filter the agents pane narrows its rows with.
#[derive(Clone, Copy, Default, PartialEq)]
enum Filter {
    /// Every announced agent.
    #[default]
    All,
    /// Only the agents still working.
    Running,
    /// Only the agents that ended.
    Done,
    /// Only the agents that run in the background.
    Background,
}

impl Filter {
    /// The filters in chip order.
    fn all() -> [Filter; 4] {
        [
            Filter::All,
            Filter::Running,
            Filter::Done,
            Filter::Background,
        ]
    }

    /// The label the chip carries.
    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Running => "Running",
            Filter::Done => "Done",
            Filter::Background => "Background",
        }
    }

    /// Whether one agent survives the filter.
    fn keeps(self, agent: &kage_client::Subagent) -> bool {
        match self {
            Filter::All => true,
            Filter::Running => matches!(
                agent.state,
                None | Some(
                    kage_client::wire::SubagentState::Running
                        | kage_client::wire::SubagentState::Paused
                )
            ),
            Filter::Done => matches!(
                agent.state,
                Some(
                    kage_client::wire::SubagentState::Completed
                        | kage_client::wire::SubagentState::Failed
                        | kage_client::wire::SubagentState::Cancelled
                )
            ),
            Filter::Background => agent.background,
        }
    }
}

/// The uppercase section label between panes.
fn section_head(text: &str, p: &crate::theme::Palette) -> Div {
    div()
        .flex()
        .items_center()
        .px(px(14.))
        .pt(px(12.))
        .pb(px(4.))
        .text_size(px(FS_XS))
        .font_weight(WEIGHT_SEMIBOLD)
        .text_color(p.faint)
        .child(SharedString::from(text.to_owned()))
}

/// A diff count chip on its tint.
fn chip(text: String, fg: gpui_kit::Hsla, bg: gpui_kit::Hsla) -> Div {
    div()
        .flex_none()
        .h(px(20.))
        .px(px(7.))
        .flex()
        .items_center()
        .rounded(px(R_FULL))
        .bg(bg)
        .font_family(FONT_MONO)
        .text_size(px(FS_2XS))
        .text_color(fg)
        .child(SharedString::from(text))
}

/// A byte count as the design writes it: `812 B`, `4.2 KB`, `1.2 MB`.
fn bytes(n: usize) -> String {
    #[allow(clippy::cast_precision_loss)]
    let n = n as f64;
    if n < 1024.0 {
        format!("{n} B")
    } else if n < 1024.0 * 1024.0 {
        format!("{:.1} KB", n / 1024.0)
    } else {
        format!("{:.1} MB", n / (1024.0 * 1024.0))
    }
}

/// The empty-state block of the design: a faint icon over one line.
fn empty_block(icon: IconName, text: &str, p: &crate::theme::Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(10.))
        .px(px(24.))
        .py(px(48.))
        .text_size(px(FS_SM))
        .text_color(p.faint)
        .child(
            Icon::new(icon)
                .with_size(px(28.))
                .opacity(0.6)
                .text_color(p.faint),
        )
        .child(SharedString::from(text.to_owned()))
}

/// The mono count a tab carries beside its label.
fn tab_count(count: &str, p: &crate::theme::Palette) -> Div {
    div()
        .text_size(px(FS_2XS))
        .font_family(FONT_MONO)
        .text_color(p.faint)
        .child(SharedString::from(count.to_owned()))
}

/// The right panel.
pub struct WorkbenchView {
    store: Entity<Store>,
    /// The composer a subagent transcript's actions write into.
    composer: Entity<TextareaState>,
    tab: Tab,
    /// The file the files pane previews.
    file: Option<String>,
    /// The subagent the detail tab shows, with its transcript.
    agent: Option<(String, Entity<TranscriptView>)>,
    filter: Filter,
    /// The change entry the pane shows a diff for, by call id.
    selected: Option<String>,
    /// Whether the workdir listing was asked for this session, so
    /// opening the files pane does not ask twice.
    files_asked: bool,
    /// The session the files ask belongs to.
    loaded: Option<String>,
    /// Shell runs the user opened or closed against their default: the
    /// latest run and running ones start open, the rest folded.
    term_flipped: HashSet<String>,
}

impl EventEmitter<WorkbenchEvent> for WorkbenchView {}

impl WorkbenchView {
    /// A workbench following `store`.
    #[must_use]
    pub fn new(
        store: Entity<Store>,
        composer: Entity<TextareaState>,
        cx: &mut Context<Self>,
    ) -> Self {
        // A request made while drawing waits for the next event to go
        // out, so the listing is asked for here, as the store moves.
        cx.observe(&store, |this, _, cx| this.ask_listing(cx))
            .detach();
        Self {
            store,
            composer,
            tab: Tab::default(),
            agent: None,
            file: None,
            filter: Filter::default(),
            selected: None,
            files_asked: false,
            loaded: None,
            term_flipped: HashSet::new(),
        }
    }

    /// Shows the change tool call `call_id` made in the changes pane.
    pub fn open_change(&mut self, call_id: String, cx: &mut Context<Self>) {
        self.tab = Tab::Changes;
        self.selected = Some(call_id);
        cx.notify();
    }

    /// Asks for the workdir listing while the files pane shows and the
    /// active session has none yet, once per session.
    fn ask_listing(&mut self, cx: &mut Context<Self>) {
        if self.tab != Tab::Files {
            return;
        }
        let active = self.store.read(cx).active_id().map(str::to_owned);
        if active != self.loaded {
            self.loaded = active.clone();
            self.files_asked = false;
        }
        let Some(active) = active else {
            return;
        };
        if self.files_asked || self.store.read(cx).fs_listing(&active).is_some() {
            return;
        }
        self.files_asked = true;
        self.store.act(cx, |store| store.fs_list(""));
    }

    /// Shows `path` in the files pane, its contents read for the
    /// preview.
    pub fn open_file(&mut self, path: &str, cx: &mut Context<Self>) {
        self.tab = Tab::Files;
        self.ask_listing(cx);
        self.file = Some(path.to_owned());
        self.store.act(cx, |store| store.fs_read(path));
        cx.notify();
    }

    /// Shows subagent `id` in the detail tab, its transcript loaded
    /// when this client holds none of it.
    pub fn open_agent(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent.as_ref().map(|(open, _)| open) != Some(&id) {
            let store = self.store.clone();
            let composer = self.composer.clone();
            let child = id.clone();
            let view = cx.new(|cx| TranscriptView::pinned(store, composer, child, window, cx));
            self.agent = Some((id.clone(), view));
        }
        self.store.act(cx, |store| store.load_child(&id));
        self.tab = Tab::Agent;
        cx.notify();
    }

    /// Closes the subagent detail tab.
    fn close_agent(&mut self, cx: &mut Context<Self>) {
        self.agent = None;
        if self.tab == Tab::Agent {
            self.tab = Tab::Agents;
        }
        cx.notify();
    }

    /// The subagent detail: back, the avatar, name, kind and state with
    /// Stop while it runs, the task and the measured facts, then the
    /// child's transcript.
    fn agent_detail(&self, p: &'static crate::theme::Palette, cx: &Context<Self>) -> AnyElement {
        let Some((id, transcript)) = &self.agent else {
            return empty_block(IconName::Bot, "Agent not found.", p).into_any_element();
        };
        let store = self.store.read(cx);
        let Some(parent) = store.active_session() else {
            return empty_block(IconName::Bot, "Agent not found.", p).into_any_element();
        };
        let Some(agent) = parent.agents.get(id) else {
            return empty_block(IconName::Bot, "Agent not found.", p).into_any_element();
        };
        let facts = agent_facts(store, parent, id, agent);
        let this = cx.entity();
        let kind = match &facts.item {
            Some((_, index)) => format!("swarm #{:02}", index + 1),
            None => facts.name.clone(),
        };
        let mut line = h_flex()
            .gap(px(8.))
            .items_center()
            .child(
                Button::new("wb-agent-back")
                    .icon(IconName::ChevronLeft)
                    .xsmall()
                    .ghost()
                    .tooltip("Back to agents")
                    .on_click({
                        let this = this.clone();
                        move |_, _, cx| {
                            this.update(cx, |this, cx| {
                                this.tab = Tab::Agents;
                                cx.notify();
                            });
                        }
                    }),
            )
            .child(avatar(&facts.name, p))
            .child(
                div()
                    .font_weight(WEIGHT_SEMIBOLD)
                    .text_size(px(FS_SM))
                    .text_color(p.ink_strong)
                    .child(SharedString::from(facts.name.clone())),
            )
            .child(
                div()
                    .px(px(7.))
                    .py(px(1.))
                    .rounded(px(R_FULL))
                    .bg(p.fill)
                    .text_size(px(11.))
                    .text_color(p.muted)
                    .child(SharedString::from(kind)),
            )
            .child(div().flex_1())
            .child(state_chip(&facts, p));
        if facts.stoppable {
            let store = self.store.clone();
            let child = id.clone();
            line = line.child(
                kit::btn_sm("wb-agent-stop", BtnTone::Danger, p)
                    .on_click(move |_, _, cx| {
                        store.act(cx, |store| store.cancel_session(&child));
                    })
                    .child(Icon::new(IconName::Square).with_size(px(11.)))
                    .child("Stop"),
            );
        }
        let calls = store.state().session(id).map_or(0, |child| {
            child
                .items
                .iter()
                .filter(|item| matches!(item, TranscriptItem::ToolCall(_)))
                .count()
        });
        let facts_line = [
            Some(facts.name.clone()),
            facts.model.clone(),
            facts.elapsed.map(crate::clock::span),
            facts.tokens.map(|n| format!("{} tok", tokens(n))),
            facts.cost.map(|cost| format!("USD {cost:.4}")),
            Some(format!("{calls} tool calls")),
            facts.item.as_ref().map(|(item, _)| format!("item: {item}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" \u{b7} ");
        let mut head = v_flex()
            .gap(px(6.))
            .px(px(14.))
            .py(px(10.))
            .border_b_1()
            .border_color(p.line)
            .child(line)
            .child(
                div()
                    .text_size(px(FS_SM))
                    .text_color(p.ink)
                    .child(SharedString::from(facts.task.clone())),
            )
            .child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(px(11.))
                    .text_color(p.faint)
                    .child(SharedString::from(facts_line)),
            );
        if let Some(reason) = &facts.reason {
            head = head.child(
                div()
                    .text_size(px(FS_XS))
                    .text_color(p.warn)
                    .child(SharedString::from(reason.clone())),
            );
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .child(head)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(10.))
                    .child(transcript.clone()),
            )
            .into_any_element()
    }

    /// Shows the files the session changed.
    pub fn open_changes(&mut self, cx: &mut Context<Self>) {
        self.tab = Tab::Changes;
        cx.notify();
    }

    /// Shows the agents list.
    pub fn open_agents(&mut self, cx: &mut Context<Self>) {
        self.tab = Tab::Agents;
        cx.notify();
    }

    /// Shows the agents list narrowed to the background agents.
    pub fn open_background_agents(&mut self, cx: &mut Context<Self>) {
        self.filter = Filter::Background;
        self.open_agents(cx);
    }

    /// Shows the fetched pages.
    pub fn open_browser(&mut self, cx: &mut Context<Self>) {
        self.tab = Tab::Browser;
        cx.notify();
    }

    /// A tab styled as the design's workbench tabs: the icon always,
    /// the label and count only while the tab is on.
    fn tab_button(
        &self,
        tab: Tab,
        on: bool,
        count: Option<String>,
        p: &crate::theme::Palette,
    ) -> Stateful<Div> {
        div()
            .id(SharedString::from(format!(
                "wb-tab-{}",
                tab.label().to_lowercase()
            )))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .h(px(30.))
            .px(px(10.))
            .rounded(px(R_MD))
            .text_size(px(FS_SM))
            .text_color(if on { p.ink_strong } else { p.muted })
            .when(on, |button| button.bg(p.selected))
            .hover(move |button| {
                if on {
                    button.bg(p.selected_hover)
                } else {
                    button.bg(p.hover).text_color(p.ink)
                }
            })
            .child(Icon::new(tab.icon()).with_size(px(14.)))
            .when(on, |button| button.child(tab.label()))
            .children(count.map(|count| tab_count(&count, p)))
    }

    /// The changes pane: the count and line totals, one row per changed
    /// file with its author when a subagent made it, then the selected
    /// file's diff under its path and Open file.
    fn changes_section(
        &self,
        p: &'static crate::theme::Palette,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let store = self.store.read(cx);
        let Some(session) = store.active_session() else {
            return Vec::new();
        };
        let entries = session_changes(store, session);
        if entries.is_empty() {
            return vec![
                empty_block(
                    IconName::FileDiff,
                    "No file changes in this session yet.",
                    p,
                )
                .into_any_element(),
            ];
        }
        let add: usize = entries.iter().map(|entry| entry.add).sum();
        let del: usize = entries.iter().map(|entry| entry.del).sum();
        let count = entries.len();
        let mut head = section_head(
            &format!("{count} FILE{} CHANGED", if count == 1 { "" } else { "S" }),
            p,
        )
        .gap(px(6.))
        .child(div().flex_1())
        .child(chip(format!("+{add}"), p.diff_add, p.diff_add_bg));
        if del > 0 {
            head = head.child(chip(format!("-{del}"), p.diff_del, p.diff_del_bg));
        }
        let selected = self
            .selected
            .clone()
            .filter(|id| entries.iter().any(|entry| &entry.call_id == id))
            .or_else(|| entries.first().map(|entry| entry.call_id.clone()));
        let mut out = vec![head.into_any_element()];
        for entry in &entries {
            let on = selected.as_deref() == Some(entry.call_id.as_str());
            let this = cx.entity();
            let call_id = entry.call_id.clone();
            let mut row = h_flex()
                .id(SharedString::from(format!("wb-change-{}", entry.call_id)))
                .w_full()
                .h(px(34.))
                .px(px(14.))
                .gap(px(8.))
                .items_center()
                .text_size(px(FS_SM))
                .cursor_pointer()
                .when(on, |row| row.bg(p.selected))
                .hover(move |row| row.bg(if on { p.selected_hover } else { p.hover }))
                .on_click(move |_, _, cx| {
                    this.update(cx, |this, cx| {
                        this.selected = Some(call_id.clone());
                        cx.notify();
                    });
                })
                .child(
                    Icon::new(if entry.created {
                        IconName::FilePlus
                    } else {
                        IconName::File
                    })
                    .with_size(px(14.))
                    .text_color(p.faint),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .font_family(FONT_MONO)
                        .text_size(px(FS_XS))
                        .text_color(p.ink)
                        .child(entry.path.clone()),
                );
            if let Some(by) = &entry.by {
                row = row.child(
                    div()
                        .px(px(7.))
                        .py(px(1.))
                        .rounded(px(R_FULL))
                        .border_1()
                        .border_color(p.line)
                        .bg(p.fill)
                        .text_size(px(10.5))
                        .text_color(p.muted)
                        .child(SharedString::from(by.clone())),
                );
            }
            row = row.child(chip(format!("+{}", entry.add), p.diff_add, p.diff_add_bg));
            if entry.del > 0 {
                row = row.child(chip(format!("-{}", entry.del), p.diff_del, p.diff_del_bg));
            }
            out.push(row.into_any_element());
        }
        let Some(call) = selected
            .as_deref()
            .and_then(|id| find_call(store, session, id))
        else {
            return out;
        };
        let path = change_path(call);
        let this = cx.entity();
        let open = path.clone();
        out.push(
            h_flex()
                .mt(px(8.))
                .h(px(36.))
                .px(px(14.))
                .gap(px(8.))
                .items_center()
                .border_y_1()
                .border_color(p.subtle)
                .text_size(px(FS_SM))
                .child(
                    Icon::new(IconName::FileDiff)
                        .with_size(px(14.))
                        .text_color(p.faint),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(FONT_MONO)
                        .text_size(px(FS_XS))
                        .text_color(p.ink)
                        .child(SharedString::from(path)),
                )
                .child(
                    Button::new("wb-open-file")
                        .label("Open file")
                        .xsmall()
                        .ghost()
                        .on_click(move |_, _, cx| {
                            let open = open.clone();
                            this.update(cx, |this, cx| this.open_file(&open, cx));
                        }),
                )
                .into_any_element(),
        );
        if let Some(lines) = crate::views::transcript::diff_lines(call) {
            out.push(crate::views::transcript::render_diff(&lines, cx).into_any_element());
        }
        out
    }

    /// The files pane: the workdir tree `_kage/fs` listed, indented by
    /// depth; a directory the listing cut short lists itself on click,
    /// and a file previews under the tree with Mention.
    fn files_section(
        &mut self,
        p: &'static crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(active) = self.store.read(cx).active_id().map(str::to_owned) else {
            return Vec::new();
        };
        if self.store.read(cx).fs_listing(&active).is_none() {
            return vec![
                empty_block(IconName::Folder, "Listing the session workdir\u{2026}", p)
                    .into_any_element(),
            ];
        }
        let store = self.store.read(cx);
        let listing = store
            .fs_listing(&active)
            .expect("the listing was checked above");
        let project =
            crate::app::project_name(store.active_session().and_then(|s| s.cwd.as_deref()));
        let mut out = vec![section_head(&project.to_uppercase(), p).into_any_element()];
        let this = cx.entity();
        let selected = self.file.clone();
        for entry in &listing.entries {
            let is_dir = entry.kind == kage_client::wire::FsKind::Directory;
            let depth = entry.path.matches('/').count();
            let name = entry
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&entry.path)
                .to_owned();
            let path = entry.path.clone();
            let on = selected.as_deref() == Some(entry.path.as_str());
            let unlisted = is_dir
                && listing.truncated
                && !listing
                    .entries
                    .iter()
                    .any(|other| other.path.starts_with(&format!("{}/", entry.path)));
            let this = this.clone();
            out.push(
                h_flex()
                    .id(SharedString::from(format!("wb-file-{}", entry.path)))
                    .w_full()
                    .h(px(26.))
                    .pl(px(14. + depth as f32 * 14.))
                    .pr(px(14.))
                    .gap(px(7.))
                    .items_center()
                    .text_size(px(FS_SM))
                    .text_color(if is_dir { p.muted } else { p.ink })
                    .cursor_pointer()
                    .when(on, |row| row.bg(p.selected))
                    .hover(move |row| row.bg(if on { p.selected_hover } else { p.hover }))
                    .on_click(move |_, _, cx| {
                        let path = path.clone();
                        this.update(cx, |this, cx| {
                            if is_dir {
                                if unlisted {
                                    this.store.act(cx, |store| store.fs_list(&path));
                                }
                            } else {
                                this.open_file(&path, cx);
                            }
                        });
                    })
                    .child(
                        Icon::new(if is_dir {
                            IconName::Folder
                        } else {
                            IconName::File
                        })
                        .with_size(px(12.))
                        .text_color(p.faint),
                    )
                    .child(div().min_w_0().truncate().child(SharedString::from(name)))
                    .when(unlisted, |row| {
                        row.child(
                            div()
                                .text_size(px(FS_2XS))
                                .text_color(p.faint)
                                .child("\u{2026}"),
                        )
                    })
                    .into_any_element(),
            );
        }
        if let Some((path, read)) = store.fs_preview(&active)
            && selected.as_deref() == Some(path)
        {
            let mention = path.to_owned();
            let this = this.clone();
            out.push(
                h_flex()
                    .mt(px(10.))
                    .h(px(36.))
                    .px(px(14.))
                    .gap(px(8.))
                    .items_center()
                    .border_y_1()
                    .border_color(p.subtle)
                    .child(
                        Icon::new(IconName::File)
                            .with_size(px(14.))
                            .text_color(p.faint),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(FONT_MONO)
                            .text_size(px(FS_XS))
                            .text_color(p.ink)
                            .child(SharedString::from(path.to_owned())),
                    )
                    .child(
                        Button::new("wb-file-mention")
                            .icon(IconName::AtSign)
                            .xsmall()
                            .ghost()
                            .tooltip("Mention in composer")
                            .on_click(move |_, _, cx| {
                                let path = mention.clone();
                                this.update(cx, |_, cx| cx.emit(WorkbenchEvent::Mention(path)));
                            }),
                    )
                    .into_any_element(),
            );
            if read.binary {
                out.push(
                    div()
                        .px(px(14.))
                        .py(px(8.))
                        .text_size(px(FS_XS))
                        .text_color(p.faint)
                        .child("Binary file; not shown.")
                        .into_any_element(),
                );
            } else {
                let mut code = v_flex()
                    .py(px(6.))
                    .bg(p.deep)
                    .font_family(FONT_MONO)
                    .text_size(px(12.))
                    .line_height(px(19.));
                for (n, line) in read.content.lines().enumerate() {
                    code = code.child(
                        h_flex()
                            .px(px(10.))
                            .gap(px(12.))
                            .child(
                                div()
                                    .w(px(28.))
                                    .flex_none()
                                    .text_right()
                                    .text_color(p.ghost)
                                    .child((n + 1).to_string()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_color(p.ink)
                                    .child(SharedString::from(line.to_owned())),
                            ),
                    );
                }
                out.push(code.into_any_element());
                if read.truncated {
                    out.push(
                        div()
                            .px(px(14.))
                            .py(px(6.))
                            .text_size(px(FS_XS))
                            .text_color(p.faint)
                            .child("Truncated: the file is longer than the preview.")
                            .into_any_element(),
                    );
                }
            }
        }
        out
    }

    /// The agents pane: the subagents, then each swarm's workers under
    /// its description and done count, as the design's agent rows, under
    /// the filter chips. A row opens the agent's detail tab.
    fn agents_section(
        &self,
        p: &'static crate::theme::Palette,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let store = self.store.read(cx);
        let Some(session) = store.active_session() else {
            return Vec::new();
        };
        let this = cx.entity();
        let seg = h_flex().gap(px(2.)).children(Filter::all().map(|filter| {
            let this = this.clone();
            let on = self.filter == filter;
            div()
                .id(SharedString::from(format!("wb-filter-{}", filter.label())))
                .px(px(8.))
                .h(px(20.))
                .flex()
                .items_center()
                .rounded(px(R_FULL))
                .text_size(px(FS_2XS))
                .text_color(if on { p.ink_strong } else { p.faint })
                .when(on, |chip| chip.bg(p.selected))
                .hover(move |chip| chip.bg(p.hover))
                .on_click(move |_, _, cx| {
                    this.update(cx, |this, cx| {
                        this.filter = filter;
                        cx.notify();
                    });
                })
                .child(filter.label())
        }));
        let mut out = vec![
            h_flex()
                .items_center()
                .justify_between()
                .px(px(14.))
                .pt(px(12.))
                .pb(px(4.))
                .child(
                    div()
                        .text_size(px(FS_XS))
                        .font_weight(WEIGHT_SEMIBOLD)
                        .text_color(p.faint)
                        .child("AGENTS"),
                )
                .child(seg)
                .into_any_element(),
        ];
        if session.agents.is_empty() {
            out.push(
                empty_block(
                    IconName::Users,
                    "No agents yet. Subagents and swarm workers show up here while they run.",
                    p,
                )
                .into_any_element(),
            );
            return out;
        }
        let group = |icon: IconName, label: String, count: Option<String>| {
            h_flex()
                .gap(px(6.))
                .items_center()
                .px(px(14.))
                .pt(px(10.))
                .pb(px(4.))
                .text_size(px(FS_2XS))
                .text_color(p.faint)
                .child(Icon::new(icon).with_size(px(12.)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(label)),
                )
                .children(count.map(|count| div().font_family(FONT_MONO).child(count)))
                .into_any_element()
        };
        let solo: Vec<_> = session
            .agents
            .iter()
            .filter(|(_, agent)| agent.swarm.is_none() && self.filter.keeps(agent))
            .collect();
        if !solo.is_empty() {
            out.push(group(IconName::Bot, "Subagents".to_owned(), None));
            for (id, agent) in solo {
                out.push(self.agent_row(&agent_facts(store, session, id, agent), p, cx));
            }
        }
        for item in &session.items {
            let TranscriptItem::ToolCall(call) = item else {
                continue;
            };
            if call.title != "swarm" {
                continue;
            }
            let members = crate::views::agents::children_of(session, &call.tool_call_id);
            let ended = members
                .iter()
                .filter(|(_, agent)| Filter::Done.keeps(agent))
                .count();
            let total = members.len();
            let shown: Vec<_> = members
                .into_iter()
                .filter(|(_, agent)| self.filter.keeps(agent))
                .collect();
            if shown.is_empty() {
                continue;
            }
            let description = call
                .input
                .as_ref()
                .and_then(|input| input.get("description"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Swarm")
                .to_owned();
            out.push(group(
                IconName::Waypoints,
                description,
                Some(format!("{ended}/{total}")),
            ));
            for (id, agent) in shown {
                out.push(self.agent_row(&agent_facts(store, session, id, agent), p, cx));
            }
        }
        if out.len() == 1 {
            out.push(
                empty_block(IconName::Users, "No agents match this filter.", p).into_any_element(),
            );
        }
        out
    }

    /// One agent row: avatar, the swarm index and name, the latest tool
    /// line or the result or the task, the state with elapsed time and
    /// tokens, and Stop while it runs.
    fn agent_row(
        &self,
        facts: &crate::views::agents::AgentFacts,
        p: &'static crate::theme::Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let this = cx.entity();
        let id = facts.id.clone();
        let line = if facts.phase.live() {
            facts.last.clone().unwrap_or_else(|| facts.task.clone())
        } else {
            facts.result.clone().unwrap_or_else(|| facts.task.clone())
        };
        let mut name = h_flex().gap(px(6.)).items_center().min_w_0();
        if let Some((item, index)) = &facts.item {
            name = name
                .child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(11.))
                        .text_color(p.faint)
                        .child(format!("{:02}", index + 1)),
                )
                .child(div().truncate().child(SharedString::from(item.clone())));
        } else {
            name = name.child(
                div()
                    .truncate()
                    .child(SharedString::from(facts.name.clone())),
            );
        }
        if facts.background {
            name = name.child(
                div()
                    .flex_none()
                    .px(px(6.))
                    .rounded(px(R_FULL))
                    .bg(p.accent_soft)
                    .text_size(px(10.))
                    .text_color(p.accent)
                    .child("bg"),
            );
        }
        let mut row = h_flex()
            .id(SharedString::from(format!("wb-agent-{}", facts.id)))
            .w_full()
            .px(px(14.))
            .py(px(7.))
            .gap(px(9.))
            .items_center()
            .text_size(px(FS_SM))
            .cursor_pointer()
            .hover(move |row| row.bg(p.hover))
            .on_click(move |_, window, cx| {
                let id = id.clone();
                this.update(cx, |this, cx| this.open_agent(id, window, cx));
            })
            .child(avatar(&facts.name, p))
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .child(name.text_color(p.ink))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(FS_XS))
                            .text_color(p.faint)
                            .child(SharedString::from(line.replace('`', ""))),
                    ),
            )
            .child(
                v_flex()
                    .items_end()
                    .gap(px(2.))
                    .flex_none()
                    .child(state_chip(facts, p))
                    .child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(11.))
                            .text_color(p.faint)
                            .child(meta_line(facts)),
                    ),
            );
        if facts.stoppable {
            let store = self.store.clone();
            let child = facts.id.clone();
            row = row.child(
                Button::new(SharedString::from(format!("wb-agent-stop-{}", facts.id)))
                    .icon(IconName::Square)
                    .xsmall()
                    .ghost()
                    .tooltip("Stop")
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        store.act(cx, |store| store.cancel_session(&child));
                    }),
            );
        }
        row.into_any_element()
    }

    /// The terminal pane: every shell call of the session and its
    /// subagents, each as its command with the agent that ran it, the
    /// exit chip or a spinner, and the output as plain text.
    fn terminal_section(
        &self,
        p: &'static crate::theme::Palette,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let store = self.store.read(cx);
        let Some(session) = store.active_session() else {
            return Vec::new();
        };
        let runs = session_runs(store, session);
        if runs.is_empty() {
            return vec![
                empty_block(
                    IconName::Terminal,
                    "Shell commands and their output collect here.",
                    p,
                )
                .into_any_element(),
            ];
        }
        let last = runs.len() - 1;
        let mut out = Vec::new();
        for (ix, run) in runs.into_iter().enumerate() {
            let open = (ix == last || run.running) != self.term_flipped.contains(&run.id);
            out.push(self.term_run(run, open, p, cx).into_any_element());
        }
        out
    }

    /// One shell run as a card: its state, the command's first line and
    /// who ran it on a row that folds the rest, then the whole command
    /// and the tail of its output.
    fn term_run(
        &self,
        run: TermEntry,
        open: bool,
        p: &'static crate::theme::Palette,
        cx: &Context<Self>,
    ) -> gpui_kit::Stateful<Div> {
        let state = match run.exit {
            _ if run.running => Spinner::new()
                .icon(IconName::LoaderCircle)
                .color(p.accent)
                .with_size(px(12.))
                .into_any_element(),
            Some(0) => Icon::new(IconName::Check)
                .with_size(px(12.))
                .text_color(p.ok)
                .into_any_element(),
            Some(_) => Icon::new(IconName::X)
                .with_size(px(12.))
                .text_color(p.danger)
                .into_any_element(),
            None if run.failed => Icon::new(IconName::X)
                .with_size(px(12.))
                .text_color(p.danger)
                .into_any_element(),
            None => Icon::new(IconName::Minus)
                .with_size(px(12.))
                .text_color(p.faint)
                .into_any_element(),
        };
        let first = run.command.lines().next().unwrap_or_default().to_owned();
        let more = run.command.lines().count() > 1;
        let this = cx.entity();
        let id = run.id.clone();
        let mut head = h_flex()
            .id("term-head")
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .py(px(7.))
            .cursor_pointer()
            .hover(move |style| style.bg(p.fill_hover))
            .on_click(move |_, _, cx| {
                this.update(cx, |this, cx| {
                    if !this.term_flipped.remove(&id) {
                        this.term_flipped.insert(id.clone());
                    }
                    cx.notify();
                });
            })
            .child(div().flex_none().child(state))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .font_family(FONT_MONO)
                    .text_size(px(FS_XS))
                    .text_color(p.ink)
                    .child(SharedString::from(if more && !open {
                        format!("{first} \u{2026}")
                    } else {
                        first
                    })),
            );
        if let Some(who) = &run.who {
            head = head.child(
                div()
                    .flex_none()
                    .px(px(7.))
                    .py(px(1.))
                    .rounded(px(R_FULL))
                    .border_1()
                    .border_color(p.line)
                    .bg(p.fill)
                    .text_size(px(10.5))
                    .text_color(p.muted)
                    .child(SharedString::from(who.clone())),
            );
        }
        if let Some(code) = run.exit.filter(|code| *code != 0) {
            head = head.child(chip(format!("exit {code}"), p.danger, p.danger_soft));
        }
        let mut card = v_flex()
            .id(SharedString::from(format!("term-{}", run.id)))
            .mx(px(12.))
            .mt(px(8.))
            .rounded(px(R_MD))
            .border_1()
            .border_color(p.line)
            .bg(p.deep)
            .overflow_hidden()
            .child(head);
        if !open {
            return card;
        }
        let rest: Vec<&str> = run.command.lines().skip(1).collect();
        let mut body = v_flex()
            .border_t_1()
            .border_color(p.line)
            .px(px(10.))
            .py(px(8.))
            .gap(px(6.))
            .font_family(FONT_MONO)
            .text_size(px(12.))
            .line_height(px(18.));
        if !rest.is_empty() {
            body = body.child(
                div()
                    .text_color(p.muted)
                    .child(SharedString::from(rest.join("\n"))),
            );
        }
        if run.hidden > 0 {
            body = body.child(div().text_size(px(FS_2XS)).text_color(p.faint).child(
                SharedString::from(format!(
                    "\u{2026} {} earlier line{}",
                    run.hidden,
                    if run.hidden == 1 { "" } else { "s" }
                )),
            ));
        }
        if !run.output.is_empty() {
            body = body.child(
                div()
                    .text_color(p.ink)
                    .child(SharedString::from(run.output)),
            );
        } else if !run.running {
            body = body.child(div().text_color(p.faint).child("(no output)"));
        }
        card = card.child(body);
        card
    }

    /// The browser pane: the latest fetched page as reader text, its
    /// title from the first heading, its URL, its paragraphs and its
    /// size. Nothing on the page ever loads.
    fn browser_section(
        &self,
        p: &'static crate::theme::Palette,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(session) = self.store.read(cx).active_session() else {
            return Vec::new();
        };
        let Some(page) = fetch_entries(&session.items).pop() else {
            return vec![
                empty_block(
                    IconName::Globe,
                    "Pages the agent fetches open here as reader text. No scripts, no remote images.",
                    p,
                )
                .into_any_element(),
            ];
        };
        let text = strip_ansi(&page.text);
        let title = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("# "))
            .unwrap_or(page.url.as_str())
            .to_owned();
        let mut reader = v_flex()
            .px(px(18.))
            .py(px(14.))
            .gap(px(10.))
            .child(
                div()
                    .text_size(px(18.))
                    .font_weight(WEIGHT_SEMIBOLD)
                    .text_color(p.ink_strong)
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(px(11.))
                    .text_color(p.faint)
                    .child(SharedString::from(page.url.clone())),
            );
        for paragraph in text
            .split("\n\n")
            .map(str::trim)
            .filter(|para| !para.is_empty())
        {
            reader = reader.child(
                div()
                    .text_size(px(FS_SM))
                    .line_height(px(21.))
                    .text_color(p.ink)
                    .child(SharedString::from(paragraph.to_owned())),
            );
        }
        reader = reader.child(
            div()
                .pt(px(8.))
                .border_t_1()
                .border_color(p.subtle)
                .text_size(px(FS_2XS))
                .text_color(p.faint)
                .child(SharedString::from(format!(
                    "{} fetched \u{b7} reader text only, remote content never loads",
                    bytes(page.text.len())
                ))),
        );
        vec![reader.into_any_element()]
    }
}

impl Render for WorkbenchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::active(cx);
        let has_session = self.store.read(cx).active_session().is_some();

        let this = cx.entity();
        let mut head = h_flex()
            .flex_none()
            .h(px(PANEL_HEAD_H))
            .items_center()
            .gap(px(2.))
            .pl(px(8.))
            .pr(px(4.))
            .border_b_1()
            .border_color(p.line);
        for tab in Tab::all() {
            let this = this.clone();
            let count = self
                .store
                .read(cx)
                .active_session()
                .and_then(|session| match tab {
                    Tab::Changes => {
                        let entries = session_changes(self.store.read(cx), session);
                        (!entries.is_empty()).then(|| entries.len().to_string())
                    }
                    Tab::Files => None,
                    Tab::Agents => {
                        let total = session.agents.len();
                        let live = session
                            .agents
                            .values()
                            .filter(|agent| {
                                agent.state.is_none()
                                    || agent.state
                                        == Some(kage_client::wire::SubagentState::Running)
                            })
                            .count();
                        if live > 0 {
                            Some(format!("{live} live"))
                        } else if total > 0 {
                            Some(total.to_string())
                        } else {
                            None
                        }
                    }
                    Tab::Terminal => {
                        let runs = session_run_count(self.store.read(cx), session);
                        (runs > 0).then(|| runs.to_string())
                    }
                    Tab::Browser => {
                        let pages = fetch_entries(&session.items);
                        (!pages.is_empty()).then(|| pages.len().to_string())
                    }
                    Tab::Agent => None,
                });
            head = head.child(self.tab_button(tab, self.tab == tab, count, p).on_click(
                move |_, _, cx| {
                    this.update(cx, |this, cx| {
                        this.tab = tab;
                        this.ask_listing(cx);
                        cx.notify();
                    });
                },
            ));
        }
        if let Some((id, _)) = &self.agent {
            let name = self
                .store
                .read(cx)
                .active_session()
                .and_then(|session| session.agents.get(id))
                .and_then(|agent| agent.name.clone())
                .unwrap_or_else(|| "agent".to_owned());
            let on = self.tab == Tab::Agent;
            let open = this.clone();
            let close = this.clone();
            head = head.child(
                self.tab_button(Tab::Agent, true, None, p)
                    .when(!on, |tab| tab.bg(gpui_kit::transparent_black()))
                    .child(SharedString::from(name))
                    .child(
                        div()
                            .id("wb-agent-close")
                            .rounded(px(R_FULL))
                            .hover(move |style| style.bg(p.fill_hover))
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                close.update(cx, |this, cx| this.close_agent(cx));
                            })
                            .child(Icon::new(IconName::X).with_size(px(11.))),
                    )
                    .on_click(move |_, _, cx| {
                        open.update(cx, |this, cx| {
                            this.tab = Tab::Agent;
                            cx.notify();
                        });
                    }),
            );
        }
        head = head.child(div().flex_1()).child(
            Button::new("close-workbench")
                .icon(IconName::X)
                .xsmall()
                .ghost()
                .tooltip("Close workbench (Ctrl B)")
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(ToggleWorkbench), cx);
                }),
        );

        let body = if has_session && self.tab == Tab::Agent {
            v_flex()
                .id("wb-body")
                .flex_1()
                .min_h_0()
                .child(self.agent_detail(p, cx))
        } else if has_session {
            v_flex()
                .id("wb-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .pb(px(16.))
                .children(match self.tab {
                    Tab::Changes => self.changes_section(p, cx),
                    Tab::Files => self.files_section(p, cx),
                    Tab::Agents => self.agents_section(p, cx),
                    Tab::Terminal => self.terminal_section(p, cx),
                    Tab::Browser => self.browser_section(p, cx),
                    Tab::Agent => Vec::new(),
                })
        } else {
            v_flex().id("wb-body").child(
                empty_block(
                    IconName::PanelRight,
                    "Start a session to see its changes, agents and output here.",
                    p,
                )
                .into_any_element(),
            )
        };

        v_flex()
            .id("workbench")
            .size_full()
            .overflow_hidden()
            .bg(p.sidebar)
            .text_color(p.ink)
            .border_l_1()
            .border_color(p.line)
            .child(head)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::{change_entries, change_path, fetch_entries, term_entries};
    use crate::views::transcript::DiffLine;
    use kage_client::wire::{DiffContent, MessageChunk, ToolCallContent};
    use kage_client::{ToolCallItem, TranscriptItem};
    use serde_json::Value;

    fn call(id: &str, kind: ToolKind) -> ToolCallItem {
        ToolCallItem {
            tool_call_id: id.to_owned(),
            title: "a call".to_owned(),
            kind,
            status: kage_client::wire::ToolCallStatus::Completed,
            input: None,
            swarm: None,
            content: Vec::new(),
            raw_output: None,
            took_ms: None,
        }
    }

    #[test]
    fn ansi_escapes_leave_only_the_text() {
        assert_eq!(
            super::strip_ansi(
                "\u{1b}[1;31merror\u{1b}[0m: \u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}"
            ),
            "error: link"
        );
        assert_eq!(super::strip_ansi("plain"), "plain");
    }

    #[test]
    fn change_entries_collect_edits_and_fold_their_paths() {
        let mut edit = call("t1", ToolKind::Edit);
        edit.content = vec![ToolCallContent::Diff(DiffContent {
            path: "src/lib.rs".into(),
            old_text: Some("a\nb\n".into()),
            new_text: "a\nc\n".into(),
        })];
        let mut again = call("t2", ToolKind::Edit);
        again.content = vec![ToolCallContent::Diff(DiffContent {
            path: "src/lib.rs".into(),
            old_text: None,
            new_text: "d\n".into(),
        })];
        let read = call("t3", ToolKind::Read);
        let items = vec![
            TranscriptItem::ToolCall(edit),
            TranscriptItem::ToolCall(read),
            TranscriptItem::ToolCall(again),
        ];
        let entries = change_entries(&items);
        assert_eq!(entries.len(), 1, "one path, two calls folded");
        assert_eq!(entries[0].path, "src/lib.rs");
        assert_eq!(entries[0].add, 2, "c and d are new");
        assert_eq!(entries[0].del, 1, "b is gone");
    }

    #[test]
    fn change_path_falls_through_diff_input_then_title() {
        let mut diffed = call("t1", ToolKind::Edit);
        diffed.content = vec![ToolCallContent::Diff(DiffContent {
            path: "a.rs".into(),
            old_text: None,
            new_text: String::new(),
        })];
        assert_eq!(change_path(&diffed), "a.rs");
        let mut with_path = call("t2", ToolKind::Edit);
        with_path.input = Some(serde_json::json!({"path": "b.rs"}));
        assert_eq!(change_path(&with_path), "b.rs");
        let bare = call("t3", ToolKind::Delete);
        assert_eq!(change_path(&bare), "a call", "the title is the last resort");
        let _ = Value::Null;
    }

    #[test]
    fn term_entries_carry_commands_and_exit_codes() {
        let mut shell = call("t1", ToolKind::Execute);
        shell.input = Some(serde_json::json!({"command": "cargo test"}));
        shell.raw_output = Some(serde_json::json!({"exitCode": 0}));
        let mut running = call("t2", ToolKind::Execute);
        running.status = kage_client::wire::ToolCallStatus::InProgress;
        running.input = Some(serde_json::json!({"command": "sleep 5"}));
        let items = vec![
            TranscriptItem::ToolCall(shell),
            TranscriptItem::ToolCall(running),
        ];
        let runs = term_entries(&items);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].command, "cargo test");
        assert_eq!(runs[0].exit, Some(0));
        assert!(!runs[0].running);
        assert!(runs[1].exit.is_none(), "a running call has no exit yet");
        assert!(runs[1].running);
    }

    #[test]
    fn a_replayed_shell_call_reads_its_exit_from_the_text() {
        let mut shell = call("t1", ToolKind::Execute);
        shell.status = kage_client::wire::ToolCallStatus::Completed;
        shell.content = vec![ToolCallContent::Content(MessageChunk {
            content: kage_client::wire::ContentBlock::Text(kage_client::wire::TextContent {
                text: "stdout:\nok\n\nexit: 3".into(),
            }),
            meta: None,
        })];
        let runs = term_entries(&[TranscriptItem::ToolCall(shell)]);
        assert_eq!(runs[0].exit, Some(3));
        assert_eq!(runs[0].output, "ok");
        assert!(!runs[0].running, "a finished call never spins");
    }

    #[test]
    fn long_output_keeps_its_tail() {
        let text = (1..=50)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let (kept, hidden) = super::tail(&text);
        assert_eq!(hidden, 10);
        assert!(kept.starts_with("11\n") && kept.ends_with("50"));
    }

    #[test]
    fn fetch_entries_keep_the_url_and_reader_text() {
        let mut fetch = call("t1", ToolKind::Fetch);
        fetch.title = "web_fetch example.com".to_owned();
        fetch.content = vec![ToolCallContent::Content(MessageChunk {
            content: kage_client::wire::ContentBlock::Text(kage_client::wire::TextContent {
                text: "reader text".into(),
            }),
            meta: None,
        })];
        let pages = fetch_entries(&[TranscriptItem::ToolCall(fetch)]);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].url, "web_fetch example.com");
        assert_eq!(pages[0].text, "reader text");
    }

    #[test]
    fn diff_lines_split_adds_and_dels() {
        let mut edit = call("t1", ToolKind::Edit);
        edit.content = vec![ToolCallContent::Diff(DiffContent {
            path: "a.rs".into(),
            old_text: Some("x\n".into()),
            new_text: "y\n".into(),
        })];
        let lines = crate::views::transcript::diff_lines(&edit).expect("a diff");
        assert!(
            lines
                .iter()
                .any(|line| matches!(line, DiffLine::Add(body) if body == "y")),
            "the new line is an add"
        );
        assert!(
            lines
                .iter()
                .any(|line| matches!(line, DiffLine::Del(body) if body == "x")),
            "the old line is a del"
        );
    }

    use kage_client::wire::ToolKind;
}
