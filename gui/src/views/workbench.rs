//! The workbench: the right panel as the kage web client design
//! draws it. A head of tabs over the active session's file changes,
//! its file listing, the announced agents, the shell calls and the
//! fetched pages, read straight from the session transcript.

use gpui_kit::AnyElement;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Div, Entity, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use kage_client::wire::{ToolCallContent, ToolKind};
use kage_client::{ToolCallItem, TranscriptItem};

use crate::app::ToggleWorkbench;
use crate::store::{Store, StoreHandle as _};
use crate::theme::{FONT_MONO, FS_2XS, FS_SM, FS_XS, PANEL_HEAD_H, R_FULL, R_MD, WEIGHT_SEMIBOLD};

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
        });
    }
    out
}

/// One shell call's facts, collected from the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TermEntry {
    /// The command the call ran.
    pub command: String,
    /// The output tail the call streamed, if any.
    pub output: String,
    /// The exit code, when the output carried one.
    pub exit: Option<i64>,
    /// Whether the call failed.
    pub failed: bool,
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
        let exit = call.raw_output.as_ref().and_then(|output| {
            output
                .get("exitCode")
                .or_else(|| output.get("exit_code"))
                .and_then(|code| code.as_i64())
        });
        out.push(TermEntry {
            command,
            output: call.text(),
            exit,
            failed: call.status == kage_client::wire::ToolCallStatus::Failed,
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
        out.push(FetchEntry {
            url: change_path(call),
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
}

impl Filter {
    /// The filters in chip order.
    fn all() -> [Filter; 3] {
        [Filter::All, Filter::Running, Filter::Done]
    }

    /// The label the chip carries.
    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Running => "Running",
            Filter::Done => "Done",
        }
    }

    /// Whether one agent survives the filter.
    fn keeps(self, agent: &kage_client::Subagent) -> bool {
        match self {
            Filter::All => true,
            Filter::Running => {
                agent.state.is_none()
                    || agent.state == Some(kage_client::wire::SubagentState::Running)
            }
            Filter::Done => matches!(
                agent.state,
                Some(
                    kage_client::wire::SubagentState::Completed
                        | kage_client::wire::SubagentState::Failed
                        | kage_client::wire::SubagentState::Cancelled
                )
            ),
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

/// The `+N` and `-N` chip a change row carries.
fn diff_chip(label: String, color: gpui_kit::Hsla) -> Div {
    div()
        .flex_none()
        .text_size(px(FS_2XS))
        .font_family(FONT_MONO)
        .text_color(color)
        .child(SharedString::from(label))
}

/// The right panel.
pub struct WorkbenchView {
    store: Entity<Store>,
    tab: Tab,
    filter: Filter,
    /// The change entry the pane shows a diff for, by call id.
    selected: Option<String>,
    /// Whether the workdir listing was asked for this session, so
    /// opening the files pane does not ask twice.
    files_asked: bool,
    /// The session the files ask belongs to.
    loaded: Option<String>,
}

impl EventEmitter<WorkbenchEvent> for WorkbenchView {}

impl WorkbenchView {
    /// A workbench following `store`.
    #[must_use]
    pub fn new(store: Entity<Store>) -> Self {
        Self {
            store,
            tab: Tab::default(),
            filter: Filter::default(),
            selected: None,
            files_asked: false,
            loaded: None,
        }
    }

    /// Shows the change tool call `call_id` made in the changes pane.
    pub fn open_change(&mut self, call_id: String, cx: &mut Context<Self>) {
        self.tab = Tab::Changes;
        self.selected = Some(call_id);
        cx.notify();
    }

    /// Shows the files pane.
    pub fn open_files(&mut self, cx: &mut Context<Self>) {
        self.tab = Tab::Files;
        cx.notify();
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

    /// The changes pane: one row per changed file, then the selected
    /// file's diff, as the web client's changes pane draws them.
    fn changes_section(&self, p: &crate::theme::Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(session) = self.store.read(cx).active_session() else {
            return Vec::new();
        };
        let entries = change_entries(&session.items);
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
        let summary = h_flex()
            .items_center()
            .justify_between()
            .px(px(14.))
            .py(px(4.))
            .child(
                div()
                    .text_size(px(FS_XS))
                    .text_color(p.muted)
                    .child(SharedString::from(format!(
                        "{} files changed",
                        entries.len()
                    ))),
            )
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(diff_chip(format!("+{add}"), p.diff_add))
                    .child(diff_chip(format!("-{del}"), p.diff_del)),
            )
            .into_any_element();
        let selected = self
            .selected
            .clone()
            .or_else(|| entries.first().map(|entry| entry.call_id.clone()));
        let mut calls = std::collections::BTreeMap::new();
        for item in &session.items {
            if let TranscriptItem::ToolCall(call) = item {
                calls.insert(call.tool_call_id.clone(), call);
            }
        }
        let mut out = vec![summary];
        for entry in &entries {
            let on = selected.as_deref() == Some(entry.call_id.as_str());
            out.push(
                h_flex()
                    .id(SharedString::from(format!("wb-change-{}", entry.call_id)))
                    .w_full()
                    .px(px(14.))
                    .py(px(6.))
                    .gap(px(8.))
                    .items_center()
                    .text_size(px(FS_SM))
                    .when(on, |row| row.bg(p.selected))
                    .hover(move |row| row.bg(if on { p.selected_hover } else { p.hover }))
                    .on_click({
                        let this = cx.entity();
                        let call_id = entry.call_id.clone();
                        move |_, _, cx| {
                            this.update(cx, |this, cx| {
                                this.selected = Some(call_id.clone());
                                cx.notify();
                            });
                        }
                    })
                    .child(
                        Icon::new(IconName::File)
                            .with_size(px(13.))
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
                    )
                    .child(diff_chip(format!("+{}", entry.add), p.diff_add))
                    .when(entry.del > 0, |row| {
                        row.child(diff_chip(format!("-{}", entry.del), p.diff_del))
                    })
                    .into_any_element(),
            );
        }
        let diff = selected
            .as_deref()
            .and_then(|id| calls.get(id))
            .and_then(|call| crate::views::transcript::diff_lines(call));
        if let Some(lines) = diff {
            out.push(
                div()
                    .px(px(14.))
                    .pt(px(8.))
                    .child(crate::views::transcript::render_diff(&lines, cx))
                    .into_any_element(),
            );
        }
        out
    }

    /// The files pane: the session workdir listing, one row per entry;
    /// a file row mentions the file in the composer.
    fn files_section(
        &mut self,
        p: &crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let active = self.store.read(cx).active_id().map(str::to_owned);
        if active != self.loaded {
            self.loaded = active.clone();
            self.files_asked = false;
        }
        let listing = active
            .as_deref()
            .and_then(|id| self.store.read(cx).fs_listing(id));
        if listing.is_none() {
            if self.files_asked {
                return vec![
                    empty_block(IconName::Folder, "No file listing yet.", p).into_any_element(),
                ];
            }
            self.files_asked = true;
            self.store.act(cx, |store| {
                store.fs_list("");
            });
            return vec![
                empty_block(
                    IconName::Folder,
                    "Asking the session for its workdir listing...",
                    p,
                )
                .into_any_element(),
            ];
        }
        let listing = listing.expect("the listing was Some above");
        let mut out = vec![section_head("WORKDIR", p).into_any_element()];
        let this = cx.entity();
        for entry in &listing.entries {
            let is_dir = entry.kind == kage_client::wire::FsKind::Directory;
            let path = entry.path.clone();
            let row = h_flex()
                .id(SharedString::from(format!("wb-file-{}", entry.path)))
                .w_full()
                .px(px(14.))
                .py(px(5.))
                .gap(px(8.))
                .items_center()
                .text_size(px(FS_SM))
                .hover(move |row| row.bg(p.hover))
                .child(
                    Icon::new(if is_dir {
                        IconName::Folder
                    } else {
                        IconName::File
                    })
                    .with_size(px(13.))
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
                        .child(path.clone()),
                );
            out.push(
                if is_dir {
                    row.cursor_default()
                } else {
                    let this = this.clone();
                    row.on_click(move |_, _, cx| {
                        this.update(cx, |_, cx| {
                            cx.emit(WorkbenchEvent::Mention(path.clone()));
                        });
                    })
                }
                .into_any_element(),
            );
        }
        out
    }

    /// The agents pane: the announced subagents, one row each, under
    /// the design's filter chips.
    fn agents_section(&self, p: &crate::theme::Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(session) = self.store.read(cx).active_session() else {
            return Vec::new();
        };
        let this = cx.entity();
        let seg = h_flex()
            .gap(px(2.))
            .children(Filter::all().map(|filter| {
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
            }))
            .into_any_element();
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
        let agents: Vec<&kage_client::Subagent> = session
            .agents
            .values()
            .filter(|agent| self.filter.keeps(agent))
            .collect();
        if agents.is_empty() {
            out.push(
                empty_block(IconName::Users, "No agents match this filter.", p).into_any_element(),
            );
            return out;
        }
        for agent in agents {
            let name = agent.name.clone().unwrap_or_else(|| "agent".to_owned());
            let live = agent.state.is_none()
                || agent.state == Some(kage_client::wire::SubagentState::Running);
            let (state, color) = match agent.state {
                None | Some(kage_client::wire::SubagentState::Running) => ("running", p.accent),
                Some(kage_client::wire::SubagentState::Completed) => ("completed", p.ok),
                Some(kage_client::wire::SubagentState::Failed) => ("failed", p.danger),
                Some(kage_client::wire::SubagentState::Cancelled) => ("cancelled", p.faint),
                Some(_) => ("paused", p.warn),
            };
            let initial: SharedString = name
                .chars()
                .next()
                .map(String::from)
                .unwrap_or_else(|| "?".to_owned())
                .into();
            out.push(
                h_flex()
                    .id(SharedString::from(format!("wb-agent-{name}")))
                    .w_full()
                    .px(px(14.))
                    .py(px(7.))
                    .gap(px(9.))
                    .items_center()
                    .text_size(px(FS_SM))
                    .hover(move |row| row.bg(p.hover))
                    .child(
                        div()
                            .size(px(22.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(R_FULL))
                            .bg(p.fill)
                            .text_size(px(FS_2XS))
                            .font_weight(WEIGHT_SEMIBOLD)
                            .text_color(p.muted)
                            .child(initial),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .flex_1()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(p.ink)
                                    .child(name.clone()),
                            )
                            .children(agent.task.clone().map(|task| {
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(FS_XS))
                                    .text_color(p.faint)
                                    .child(task)
                            })),
                    )
                    .child(if live {
                        div()
                            .flex_none()
                            .child(
                                Spinner::new()
                                    .icon(IconName::LoaderCircle)
                                    .color(p.accent)
                                    .with_size(px(13.)),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .flex_none()
                            .text_size(px(FS_XS))
                            .text_color(color)
                            .child(state)
                            .into_any_element()
                    })
                    .into_any_element(),
            );
        }
        out
    }

    /// The terminal pane: the shell calls with their commands and
    /// output tails, oldest first.
    fn terminal_section(&self, p: &crate::theme::Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(session) = self.store.read(cx).active_session() else {
            return Vec::new();
        };
        let runs = term_entries(&session.items);
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
        let mut out = Vec::new();
        for run in runs {
            let (chip, chip_color) = match run.exit {
                Some(0) => ("exit 0".to_owned(), p.ok),
                Some(code) => (format!("exit {code}"), p.danger),
                None if run.failed => ("failed".to_owned(), p.danger),
                None => (String::new(), p.faint),
            };
            let head = h_flex()
                .items_center()
                .gap(px(8.))
                .px(px(14.))
                .py(px(5.))
                .child(
                    div()
                        .flex_none()
                        .font_family(FONT_MONO)
                        .text_size(px(FS_SM))
                        .text_color(p.muted)
                        .child("$"),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .font_family(FONT_MONO)
                        .text_size(px(FS_XS))
                        .text_color(p.ink)
                        .child(run.command.clone()),
                );
            let head = if chip.is_empty() {
                head.child(
                    Spinner::new()
                        .icon(IconName::LoaderCircle)
                        .color(p.accent)
                        .with_size(px(12.)),
                )
            } else {
                head.child(
                    div()
                        .flex_none()
                        .text_size(px(FS_2XS))
                        .font_family(FONT_MONO)
                        .text_color(chip_color)
                        .child(chip),
                )
            };
            out.push(head.into_any_element());
            if !run.output.is_empty() {
                let tail: String = {
                    let lines: Vec<&str> = run.output.lines().collect();
                    let start = lines.len().saturating_sub(8);
                    lines[start..].join("\n")
                };
                out.push(
                    div()
                        .mx(px(14.))
                        .mb(px(6.))
                        .px(px(8.))
                        .py(px(6.))
                        .rounded(px(R_MD))
                        .bg(p.raised)
                        .max_h(px(160.))
                        .overflow_hidden()
                        .font_family(FONT_MONO)
                        .text_size(px(FS_2XS))
                        .line_height(px(16.))
                        .text_color(p.muted)
                        .child(tail)
                        .into_any_element(),
                );
            }
        }
        out
    }

    /// The browser pane: the fetched pages as reader text.
    fn browser_section(&self, p: &crate::theme::Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(session) = self.store.read(cx).active_session() else {
            return Vec::new();
        };
        let pages = fetch_entries(&session.items);
        if pages.is_empty() {
            return vec![empty_block(
                IconName::Globe,
                "Pages the agent fetches open here as reader text. No scripts, no remote images.",
                p,
            )
            .into_any_element()];
        }
        let mut out = Vec::new();
        for page in pages {
            out.push(
                v_flex()
                    .px(px(14.))
                    .py(px(8.))
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(FS_SM))
                            .font_weight(WEIGHT_SEMIBOLD)
                            .text_color(p.ink_strong)
                            .child(page.url.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(FS_XS))
                            .text_color(p.muted)
                            .max_h(px(220.))
                            .overflow_hidden()
                            .child(page.text.clone()),
                    )
                    .into_any_element(),
            );
        }
        out
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
                        let entries = change_entries(&session.items);
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
                        let runs = term_entries(&session.items);
                        (!runs.is_empty()).then(|| runs.len().to_string())
                    }
                    Tab::Browser => {
                        let pages = fetch_entries(&session.items);
                        (!pages.is_empty()).then(|| pages.len().to_string())
                    }
                });
            head = head.child(self.tab_button(tab, self.tab == tab, count, p).on_click(
                move |_, _, cx| {
                    this.update(cx, |this, cx| {
                        this.tab = tab;
                        cx.notify();
                    });
                },
            ));
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

        let body = if has_session {
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
        }
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
        assert!(runs[1].exit.is_none(), "a running call has no exit yet");
    }

    #[test]
    fn fetch_entries_keep_the_url_and_reader_text() {
        let mut fetch = call("t1", ToolKind::Fetch);
        fetch.title = "web_fetch example.com".to_owned();
        fetch.content = vec![ToolCallContent::Content(MessageChunk {
            content: kage_client::wire::ContentBlock::Text(kage_client::wire::TextContent {
                text: "reader text".into(),
            }),
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
