//! The workbench: the right panel as the kage web client design draws
//! it. A head of tabs over the active session's file changes, its file
//! listing, the announced agents, the shell calls and the fetched
//! pages, read straight from the session transcript.

use crate::icons::PaintSnapped as _;
use crate::ui::forms::ShowCrisp as _;
use std::collections::HashSet;

use kage_client::wire::{ToolCallContent, ToolCallStatus, ToolKind};
use kage_client::{ToolCallItem, TranscriptItem};

use crate::store::Store;
use crate::theme::{
    FAMILY_BODY_SEMIBOLD, FAMILY_MONO_REGULAR, FS_2XS, FS_SM, FS_XS, Palette, R_MD, family,
};
use crate::ui::agents;
use crate::ui::transcript::model::{DiffLine, diff_lines};
use crate::ui::transcript::tool_rows::RowAction;
use crate::ui::transcript::view::TranscriptView;

/// What a file change row reports, collected from one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEntry {
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
pub fn change_path(call: &ToolCallItem) -> String {
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
pub fn change_entries(items: &[TranscriptItem]) -> Vec<ChangeEntry> {
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
        let (add, del) = diff_lines(call)
            .map(|lines| {
                lines.iter().fold((0, 0), |(add, del), line| match line {
                    DiffLine::Add(_) => (add + 1, del),
                    DiffLine::Del(_) => (add, del + 1),
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
#[must_use]
pub fn session_changes(store: &Store, session: &kage_client::Session) -> Vec<ChangeEntry> {
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
#[must_use]
pub fn strip_ansi(text: &str) -> String {
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
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// How many output lines an open run shows, from the end.
const TERM_TAIL: usize = 40;

/// One shell call's facts, collected from the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermEntry {
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
pub fn term_entries(items: &[TranscriptItem]) -> Vec<TermEntry> {
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
                    .and_then(serde_json::Value::as_i64)
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
pub struct FetchEntry {
    /// The URL the call fetched, as its title carries it.
    pub url: String,
    /// The reader text the call streamed, if any.
    pub text: String,
}

/// How many remote fetches the transcript holds, without building
/// their reader text.
#[must_use]
pub fn fetch_count(items: &[TranscriptItem]) -> usize {
    items
        .iter()
        .filter(
            |item| matches!(item, TranscriptItem::ToolCall(call) if call.kind == ToolKind::Fetch),
        )
        .count()
}

/// The remote fetches the transcript holds, in transcript order.
#[must_use]
pub fn fetch_entries(items: &[TranscriptItem]) -> Vec<FetchEntry> {
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

/// The panes the workbench tabs switch between.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
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
    pub const ALL: [Tab; 5] = [
        Tab::Changes,
        Tab::Files,
        Tab::Agents,
        Tab::Terminal,
        Tab::Browser,
    ];

    /// The label the tab carries.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Tab::Changes => "Changes",
            Tab::Files => "Files",
            Tab::Agents => "Agents",
            Tab::Terminal => "Terminal",
            Tab::Browser => "Browser",
        }
    }

    /// The icon the tab leads with.
    #[must_use]
    pub fn icon(self) -> crate::icons::Icon {
        match self {
            Tab::Changes => crate::icons::Icon::FileDiff,
            Tab::Files => crate::icons::Icon::Folder,
            Tab::Agents => crate::icons::Icon::Users,
            Tab::Terminal => crate::icons::Icon::Terminal,
            Tab::Browser => crate::icons::Icon::Globe,
        }
    }
}

/// The filter the agents pane narrows its rows with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Filter {
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
    pub const ALL: [Filter; 4] = [
        Filter::All,
        Filter::Running,
        Filter::Done,
        Filter::Background,
    ];

    /// The label the chip carries.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Running => "Running",
            Filter::Done => "Done",
            Filter::Background => "Background",
        }
    }

    /// Whether one agent survives the filter.
    #[must_use]
    pub fn keeps(self, agent: &kage_client::Subagent) -> bool {
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

/// A byte count as the design writes it: `812 B`, `4.2 KB`, `1.2 MB`.
#[must_use]
pub fn bytes(n: usize) -> String {
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

/// The uppercase section label between panes.
fn section_head(ui: &mut egui::Ui, text: &str, palette: &Palette) {
    ui.add_space(12.0);
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .size(FS_XS)
            .family(family(FAMILY_BODY_SEMIBOLD))
            .color(palette.faint),
    );
    ui.add_space(4.0);
}

/// The empty-state block of the design: a faint icon over one line.
fn empty_block(ui: &mut egui::Ui, icon: crate::icons::Icon, text: &str, palette: &Palette) {
    ui.add_space(48.0);
    ui.vertical_centered(|ui| {
        crate::icons::icon(ui, icon, 28.0, palette.faint);
        ui.add_space(10.0);
        // The text wraps on its 21px lines inside the reference's 24px
        // side padding, so it never holds the panel wider than itself.
        let mut job = egui::text::LayoutJob::single_section(
            text.to_owned(),
            egui::TextFormat {
                font_id: egui::FontId::new(
                    FS_SM,
                    crate::theme::family(crate::theme::FAMILY_BODY_REGULAR),
                ),
                color: palette.faint,
                line_height: Some(crate::theme::line_h(FS_SM)),
                ..Default::default()
            },
        );
        job.wrap.max_width = (ui.available_width() - 48.0).max(0.0);
        let galley = ui.painter().layout_job(job);
        let (rect, response) = ui.allocate_exact_size(galley.size().ceil(), egui::Sense::hover());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
        ui.painter().galley(rect.min, galley, palette.faint);
    });
}

/// The workbench's state: the active tab, its filters and the panes'
/// selections.
#[derive(Debug, Default)]
pub struct Workbench {
    /// The subagent whose detail the workbench shows, when one is open.
    agent: Option<String>,
    /// The subagent's transcript, pinned to `agent`.
    agent_view: Option<TranscriptView>,
    /// Whether the subagent's detail tab shows, rather than the tab
    /// the head left behind it.
    agent_on: bool,
    /// The pane the tabs show.
    tab: Tab,
    /// The file the files pane previews.
    file: Option<String>,
    /// The filter the agents pane narrows with.
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

impl Workbench {
    /// A workbench on the changes pane.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The pane the tabs show.
    #[must_use]
    pub const fn tab(&self) -> Tab {
        self.tab
    }

    /// Switches to `tab`.
    pub fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.agent_on = false;
    }

    /// The filter the agents pane narrows with.
    #[must_use]
    pub const fn filter(&self) -> Filter {
        self.filter
    }

    /// Sets the agents pane's filter.
    pub fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
    }

    /// The file the files pane previews.
    #[must_use]
    pub fn file(&self) -> Option<&str> {
        self.file.as_deref()
    }

    /// Previews `file` in the files pane.
    pub fn set_file(&mut self, file: Option<String>) {
        self.file = file;
    }

    /// Whether the workdir listing was asked for `session`.
    #[must_use]
    pub fn listing_asked(&self, session: &str) -> bool {
        self.loaded.as_deref() == Some(session) && self.files_asked
    }

    /// Records that the workdir listing was asked for `session`.
    pub fn mark_listing_asked(&mut self, session: &str) {
        self.files_asked = true;
        self.loaded = Some(session.to_owned());
    }

    /// Whether the shell run `id` shows its output, against the
    /// default: the latest run and running ones start open.
    #[must_use]
    pub fn run_open(&self, id: &str, running: bool, latest: bool) -> bool {
        let open = running || latest;
        if self.term_flipped.contains(id) {
            !open
        } else {
            open
        }
    }

    /// Flips the shell run `id`'s open state against its default.
    pub fn flip_run(&mut self, id: &str) {
        if !self.term_flipped.insert(id.to_owned()) {
            self.term_flipped.remove(id);
        }
    }

    /// The change entry the pane shows a diff for.
    #[must_use]
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Selects the change entry `id`, or clears it when `None`.
    pub fn select(&mut self, id: Option<String>) {
        self.selected = id;
    }

    /// The subagent whose detail is open.
    #[must_use]
    pub fn agent(&self) -> Option<&str> {
        self.agent.as_deref()
    }

    /// Opens subagent `id`'s detail on the agents tab.
    pub fn open_agent(&mut self, id: String) {
        self.tab = Tab::Agents;
        self.agent_view = Some(TranscriptView::pinned(id.clone()));
        self.agent = Some(id);
        self.agent_on = true;
    }

    /// Returns from a subagent's detail to the agents list.
    pub fn close_agent(&mut self) {
        self.agent = None;
        self.agent_view = None;
        self.agent_on = false;
    }

    /// Whether the subagent's detail tab is the one that shows.
    #[must_use]
    pub fn agent_on(&self) -> bool {
        self.agent.is_some() && self.agent_on
    }
}

/// What the workbench asks the shell to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkbenchEvent {
    /// Close the workbench.
    Close,
    /// The user picked a file in the files pane; the shell mentions it
    /// in the composer.
    Mention(String),
    /// Open the child session `id` in the detail tab.
    OpenAgent(String),
    /// Stop the child session `id`.
    StopAgent(String),
    /// Ask the engine for the workdir listing.
    AskFiles,
    /// Read the file at `path` for the files pane's preview.
    ReadFile(String),
    /// An action a row of the subagent's transcript raised.
    Row(RowAction),
}

/// Draws the workbench and reports what its panes asked for.
pub fn show(
    ui: &mut egui::Ui,
    workbench: &mut Workbench,
    store: &mut Store,
) -> Vec<WorkbenchEvent> {
    let palette = Palette::active_of(ui.ctx());
    let mut events = Vec::new();
    ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
    head(ui, workbench, store, palette, &mut events);
    if store.active_session().is_none() {
        empty_block(
            ui,
            crate::icons::Icon::PanelRight,
            "Start a session to see its changes, agents and output here.",
            palette,
        );
        return events;
    }
    if workbench.agent_on() {
        agent_detail(ui, workbench, store, palette, &mut events);
        return events;
    }
    // One scroll area per tab, so a long tab's position never lands
    // past the end of a shorter one.
    egui::ScrollArea::vertical()
        .id_salt(("workbench-body", workbench.tab().label()))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            match workbench.tab() {
                Tab::Changes => changes_pane(ui, workbench, store, palette, &mut events),
                Tab::Files => files_pane(ui, workbench, store, palette, &mut events),
                Tab::Agents => {
                    let store = &*store;
                    if let Some(session) = store.active_session() {
                        agents_pane(ui, workbench, store, palette, &session.agents, &mut events);
                    }
                }
                Tab::Terminal => terminal_pane(ui, workbench, store, palette),
                Tab::Browser => {
                    if let Some(session) = store.active_session() {
                        browser_pane(ui, palette, &session.items);
                    }
                }
            }
            ui.add_space(16.0);
        });
    events
}

/// The head: one button per tab, the active one labeled, with the
/// counts the session gives them; the open subagent's tab; and the
/// close button at the far end.
fn head(
    ui: &mut egui::Ui,
    workbench: &mut Workbench,
    store: &Store,
    palette: &Palette,
    events: &mut Vec<WorkbenchEvent>,
) {
    let (bar, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), crate::theme::PANEL_HEAD_H),
        egui::Sense::hover(),
    );
    ui.painter().hline(
        bar.x_range(),
        bar.bottom() - 0.5,
        egui::Stroke::new(1.0, palette.line),
    );
    let session = store.active_session();
    let count = |tab: Tab| -> Option<String> {
        let session = session?;
        match tab {
            Tab::Changes => {
                let entries = session_changes(store, session);
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
                            || agent.state == Some(kage_client::wire::SubagentState::Running)
                    })
                    .count();
                if live > 0 {
                    Some(format!("{live} live"))
                } else {
                    (total > 0).then(|| total.to_string())
                }
            }
            Tab::Terminal => {
                let runs = session_run_count(store, session);
                (runs > 0).then(|| runs.to_string())
            }
            Tab::Browser => {
                let pages = fetch_count(&session.items);
                (pages > 0).then(|| pages.to_string())
            }
        }
    };
    // The reference's extra small ghost button: 20px, a 12px glyph,
    // centered above the bottom border and 4px in from the end.
    let close = egui::Rect::from_min_size(
        egui::pos2(
            bar.right() - 4.0 - 20.0,
            crate::theme::snap(bar.top() + (bar.height() - 1.0 - 20.0) / 2.0),
        ),
        egui::vec2(20.0, 20.0),
    );
    let mut x = bar.left() + 8.0;
    // The middle of what sits above the bottom border.
    let mid = bar.top() + (bar.height() - 1.0) / 2.0;
    let agent_on = workbench.agent_on();
    let agent_name = workbench.agent().map(|id| {
        session
            .and_then(|session| session.agents.get(id))
            .and_then(|agent| agent.name.clone())
            .unwrap_or_else(|| "agent".to_owned())
    });
    // A panel dragged too narrow sheds the active label, then the
    // counts, and the tabs never run under the close button.
    let counts = Tab::ALL.map(count);
    let room = egui::Rect::from_min_max(bar.min, egui::pos2(close.left() - 4.0, bar.max.y));
    let fits = |labels: bool, counted: bool| {
        let tabs: f32 = Tab::ALL
            .iter()
            .zip(&counts)
            .map(|(tab, count)| {
                let on = labels && workbench.tab() == *tab && !agent_on;
                let count = count.as_deref().filter(|_| counted);
                tab_width(ui, on.then_some(tab.label()), count, false) + 2.0
            })
            .sum();
        let agent = agent_name.as_deref().map_or(0.0, |name| {
            tab_width(ui, Some(name).filter(|_| labels), None, true)
        });
        x + tabs + agent <= room.right()
    };
    let labeled = fits(true, true);
    let counted = labeled || fits(false, true);
    let mut tabs = ui.new_child(egui::UiBuilder::new().max_rect(room));
    tabs.set_clip_rect(room.intersect(tabs.clip_rect()));
    for (tab, count) in Tab::ALL.into_iter().zip(&counts) {
        let on = workbench.tab() == tab && !agent_on;
        let label = (on && labeled).then_some(tab.label());
        let (rect, response) = tab_button(
            &mut tabs,
            egui::pos2(x, mid),
            tab.icon(),
            label,
            count.as_deref().filter(|_| counted),
            on,
            tab.label(),
            palette,
        );
        x = rect.right() + 2.0;
        if response.clicked() {
            workbench.set_tab(tab);
        }
    }
    if let Some(name) = agent_name {
        let (rect, response) = tab_button(
            &mut tabs,
            egui::pos2(x, mid),
            crate::icons::Icon::Bot,
            labeled.then_some(name.as_str()),
            None,
            agent_on,
            &name,
            palette,
        );
        let close = egui::Rect::from_min_size(
            egui::pos2(rect.right() - 8.0 - 15.0, mid - 7.5),
            egui::vec2(15.0, 15.0),
        );
        let close_response = tabs
            .interact(
                close,
                tabs.id().with("wb-agent-close"),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if close_response.hovered() {
            tabs.painter()
                .rect_filled(close, egui::CornerRadius::same(8), palette.fill_hover);
        }
        crate::icons::themed_image(crate::icons::Icon::X, 11.0, palette.muted).paint_snapped(
            &tabs,
            egui::Rect::from_center_size(close.center(), egui::vec2(11.0, 11.0)),
        );
        if close_response.clicked() {
            workbench.close_agent();
        } else if response.clicked() {
            workbench.agent_on = true;
        }
    }
    // A child of the bar's own room, so drawing the button moves no
    // cursor under the head.
    let mut close_ui = ui.new_child(egui::UiBuilder::new().max_rect(close));
    let close_response = crate::ui::forms::icon_btn(
        &mut close_ui,
        "Close workbench",
        crate::icons::Icon::X,
        20.0,
        12.0,
        R_MD,
        crate::ui::forms::IconTones {
            fill: egui::Color32::TRANSPARENT,
            hover_fill: palette.hover,
            ink: palette.ink,
            hover_ink: palette.ink,
        },
    )
    .on_hover_text("Close workbench (Ctrl B)");
    if close_response.clicked() {
        events.push(WorkbenchEvent::Close);
    }
}

/// The width of a head tab carrying `label` and `count`, with the
/// agent's close mark when `closer`.
fn tab_width(ui: &egui::Ui, label: Option<&str>, count: Option<&str>, closer: bool) -> f32 {
    let measure = |text: &str, font: egui::FontId| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), font, egui::Color32::PLACEHOLDER)
            .size()
            .x
    };
    tab_extent(
        label.map(|label| {
            measure(
                label,
                egui::FontId::new(FS_SM, family(crate::theme::FAMILY_BODY_REGULAR)),
            )
        }),
        count.map(|count| {
            measure(
                count,
                egui::FontId::new(FS_2XS, family(FAMILY_MONO_REGULAR)),
            )
        }),
        closer,
    )
}

/// A head tab's width from its label's and count's text widths.
fn tab_extent(label: Option<f32>, count: Option<f32>, closer: bool) -> f32 {
    10.0 + 14.0
        + 10.0
        + label.map_or(0.0, |w| 6.0 + w)
        + count.map_or(0.0, |w| 6.0 + w)
        + if closer { 6.0 + 15.0 } else { 0.0 }
}

/// One head tab: its icon, its label when given, and its count, 30px
/// tall from `at` (left edge, vertical center), filled when on. A tab
/// with room for the agent's close mark carries it at its end.
#[allow(clippy::too_many_arguments)]
fn tab_button(
    ui: &mut egui::Ui,
    at: egui::Pos2,
    icon: crate::icons::Icon,
    label: Option<&str>,
    count: Option<&str>,
    on: bool,
    name: &str,
    palette: &Palette,
) -> (egui::Rect, egui::Response) {
    let ink = if on {
        palette.ink_strong
    } else {
        palette.muted
    };
    let label = label.map(|label| {
        ui.painter().layout_no_wrap(
            label.to_owned(),
            egui::FontId::new(FS_SM, family(crate::theme::FAMILY_BODY_REGULAR)),
            ink,
        )
    });
    let count = count.map(|count| {
        ui.painter().layout_no_wrap(
            count.to_owned(),
            egui::FontId::new(FS_2XS, family(FAMILY_MONO_REGULAR)),
            palette.faint,
        )
    });
    let width = tab_extent(
        label.as_ref().map(|label| label.size().x),
        count.as_ref().map(|count| count.size().x),
        icon == crate::icons::Icon::Bot,
    );
    let rect = egui::Rect::from_min_size(
        egui::pos2(at.x, crate::theme::snap(at.y - 15.0)),
        egui::vec2(width, 30.0),
    );
    // The contents center on the tab as it landed on the pixel grid.
    let mid = rect.center().y;
    let response = ui
        .interact(rect, ui.id().with(("wb-tab", name)), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(name);
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, name));
    let fill = match (on, response.hovered()) {
        (true, true) => Some(palette.selected_hover),
        (true, false) => Some(palette.selected),
        (false, true) => Some(palette.hover),
        (false, false) => None,
    };
    if let Some(fill) = fill {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(R_MD as u8), fill);
    }
    let ink = if !on && response.hovered() {
        palette.ink
    } else {
        ink
    };
    let mut x = rect.left() + 10.0;
    crate::icons::themed_image(icon, 14.0, ink).paint_snapped(
        ui,
        egui::Rect::from_min_size(egui::pos2(x, mid - 7.0), egui::vec2(14.0, 14.0)),
    );
    x += 14.0;
    if let Some(label) = label {
        x += 6.0;
        let w = label.size().x;
        ui.painter().galley(
            egui::pos2(x, crate::theme::line_y(mid, 0.0, &label)),
            label,
            ink,
        );
        x += w;
    }
    if let Some(count) = count {
        x += 6.0;
        ui.painter().galley(
            egui::pos2(x, crate::theme::line_y(mid, 0.0, &count)),
            count,
            palette.faint,
        );
    }
    (rect, response)
}

/// The changes pane: the count and line totals, one row per changed
/// file with its author when a subagent made it, then the selected
/// file's diff under its path and Open file.
fn changes_pane(
    ui: &mut egui::Ui,
    workbench: &mut Workbench,
    store: &mut Store,
    palette: &Palette,
    events: &mut Vec<WorkbenchEvent>,
) {
    let Some(session) = store.active_session() else {
        return;
    };
    let entries = session_changes(store, session);
    if entries.is_empty() {
        empty_block(
            ui,
            crate::icons::Icon::FileDiff,
            "No file changes in this session yet.",
            palette,
        );
        return;
    }
    let add: usize = entries.iter().map(|entry| entry.add).sum();
    let del: usize = entries.iter().map(|entry| entry.del).sum();
    let count = entries.len();
    let (head, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 12.0 + 20.0 + 4.0),
        egui::Sense::hover(),
    );
    let mid = head.top() + 12.0 + 10.0;
    let title = format!("{count} FILE{} CHANGED", if count == 1 { "" } else { "S" });
    ui.interact(head, ui.id().with("wb-changes-head"), egui::Sense::hover())
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &title));
    ui.painter().text(
        egui::pos2(head.left() + 14.0, mid),
        egui::Align2::LEFT_CENTER,
        title.clone(),
        egui::FontId::new(FS_XS, family(FAMILY_BODY_SEMIBOLD)),
        palette.faint,
    );
    let mut right = head.right() - 14.0;
    if del > 0 {
        right = diff_chip(ui, right, mid, &format!("-{del}"), false, palette) - 6.0;
    }
    diff_chip(ui, right, mid, &format!("+{add}"), true, palette);
    let selected = workbench
        .selected()
        .map(str::to_owned)
        .filter(|id| entries.iter().any(|entry| &entry.call_id == id))
        .or_else(|| entries.first().map(|entry| entry.call_id.clone()));
    for entry in &entries {
        let on = selected.as_deref() == Some(entry.call_id.as_str());
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, &entry.path)
        });
        let fill = match (on, response.hovered()) {
            (true, true) => Some(palette.selected_hover),
            (true, false) => Some(palette.selected),
            (false, true) => Some(palette.hover),
            (false, false) => None,
        };
        if let Some(fill) = fill {
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::ZERO, fill);
        }
        let mid = rect.center().y;
        let icon = if entry.created {
            crate::icons::Icon::FilePlus
        } else {
            crate::icons::Icon::File
        };
        crate::icons::themed_image(icon, 14.0, palette.faint).paint_snapped(
            ui,
            egui::Rect::from_min_size(
                egui::pos2(rect.left() + 14.0, mid - 7.0),
                egui::vec2(14.0, 14.0),
            ),
        );
        let mut right = rect.right() - 14.0;
        if entry.del > 0 {
            right = diff_chip(ui, right, mid, &format!("-{}", entry.del), false, palette) - 8.0;
        }
        right = diff_chip(ui, right, mid, &format!("+{}", entry.add), true, palette) - 8.0;
        if let Some(by) = &entry.by {
            right = by_badge(ui, right, mid, by, palette) - 8.0;
        }
        let x = rect.left() + 14.0 + 14.0 + 8.0;
        let path = crate::ui::transcript::head::one_line(
            ui,
            entry.path.clone(),
            egui::TextFormat::simple(
                egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)),
                palette.ink,
            ),
            right - x,
        );
        ui.painter().galley(
            egui::pos2(x, crate::theme::line_y(mid, 0.0, &path)),
            path,
            palette.ink,
        );
        if response.clicked() {
            workbench.select(Some(entry.call_id.clone()));
        }
    }
    let Some(call) = selected
        .as_deref()
        .and_then(|id| find_call(store, session, id))
    else {
        return;
    };
    let path = change_path(call);
    ui.add_space(8.0);
    let (bar, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), egui::Sense::hover());
    for y in [bar.top() + 0.5, bar.bottom() - 0.5] {
        ui.painter()
            .hline(bar.x_range(), y, egui::Stroke::new(1.0, palette.subtle));
    }
    let mid = bar.center().y;
    crate::icons::themed_image(crate::icons::Icon::FileDiff, 14.0, palette.faint).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(bar.left() + 14.0, mid - 7.0),
            egui::vec2(14.0, 14.0),
        ),
    );
    let open = egui::Rect::from_min_max(
        egui::pos2(bar.right() - 14.0 - 90.0, bar.top()),
        egui::pos2(bar.right() - 14.0, bar.bottom()),
    );
    let opened = ui
        .scope_builder(
            egui::UiBuilder::new()
                .max_rect(open)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| ghost_button(ui, "Open file", palette),
        )
        .inner;
    let x = bar.left() + 14.0 + 14.0 + 8.0;
    let label = crate::ui::transcript::head::one_line(
        ui,
        path.clone(),
        egui::TextFormat::simple(
            egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)),
            palette.ink,
        ),
        opened.rect.left() - 8.0 - x,
    );
    ui.painter().galley(
        egui::pos2(x, crate::theme::line_y(mid, 0.0, &label)),
        label,
        palette.ink,
    );
    if opened.clicked() {
        workbench.set_tab(Tab::Files);
        workbench.set_file(Some(path.clone()));
        events.push(WorkbenchEvent::ReadFile(path));
    }
    if let Some(lines) = diff_lines(call) {
        crate::ui::transcript::tool_rows::render_diff(ui, &lines, palette);
    }
}

/// A diff count chip with its right edge at `right`, centered on
/// `mid`: green for added lines, red for deleted. Gives back its left
/// edge.
fn diff_chip(ui: &egui::Ui, right: f32, mid: f32, text: &str, add: bool, palette: &Palette) -> f32 {
    let (fg, bg) = if add {
        (palette.diff_add, palette.diff_add_bg)
    } else {
        (palette.diff_del, palette.diff_del_bg)
    };
    let width = ui
        .painter()
        .layout_no_wrap(
            text.to_owned(),
            egui::FontId::new(FS_2XS, family(FAMILY_MONO_REGULAR)),
            fg,
        )
        .size()
        .x
        + 14.0;
    crate::ui::transcript::head::paint_chip(ui, egui::pos2(right - width, mid), text, fg, bg).left()
}

/// The badge naming the subagent that made a change, right edge at
/// `right`. Gives back its left edge.
fn by_badge(ui: &egui::Ui, right: f32, mid: f32, by: &str, palette: &Palette) -> f32 {
    let galley = ui.painter().layout_no_wrap(
        by.to_owned(),
        egui::FontId::new(10.5, family(crate::theme::FAMILY_BODY_REGULAR)),
        palette.muted,
    );
    let rect = egui::Rect::from_min_size(
        egui::pos2(right - galley.size().x - 16.0, mid - 9.0),
        egui::vec2(galley.size().x + 16.0, 18.0),
    );
    let radius = egui::CornerRadius::same(9);
    ui.painter().rect_filled(rect, radius, palette.fill);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, palette.line),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, palette.muted);
    rect.left()
}

/// The design's quiet text button: no frame, the hover fill.
fn ghost_button(ui: &mut egui::Ui, label: &str, palette: &Palette) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::new(FS_XS, family(crate::theme::FAMILY_BODY_MEDIUM)),
        palette.ink,
    );
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(galley.size().x + 16.0, 22.0),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(R_MD as u8), palette.hover);
    }
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, palette.ink);
    response
}

/// The files pane: the workdir's listing, with a preview of the
/// picked file.
fn files_pane(
    ui: &mut egui::Ui,
    workbench: &mut Workbench,
    store: &mut Store,
    palette: &Palette,
    events: &mut Vec<WorkbenchEvent>,
) {
    let Some(id) = store.active_id().map(str::to_owned) else {
        empty_block(
            ui,
            crate::icons::Icon::Folder,
            "Start a session to see its changes, agents and output here.",
            palette,
        );
        return;
    };
    if !workbench.listing_asked(&id) {
        workbench.mark_listing_asked(&id);
        events.push(WorkbenchEvent::AskFiles);
    }
    let Some(listing) = store.fs_listing(&id) else {
        empty_block(
            ui,
            crate::icons::Icon::Folder,
            "Listing the session workdir\u{2026}",
            palette,
        );
        return;
    };
    let entries: Vec<_> = listing.entries.clone();
    let truncated = listing.truncated;
    let project = crate::ui::sidebar::project_name(
        store
            .active_session()
            .and_then(|session| session.cwd.as_deref()),
    );
    section_head(ui, &project, palette);
    for entry in &entries {
        let is_dir = entry.kind == kage_client::wire::FsKind::Directory;
        let depth = entry.path.matches('/').count();
        let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
        let on = workbench.file() == Some(entry.path.as_str());
        let unlisted = is_dir
            && truncated
            && !entries
                .iter()
                .any(|other| other.path.starts_with(&format!("{}/", entry.path)));
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::click());
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, name)
        });
        let fill = match (on, response.hovered()) {
            (true, true) => Some(palette.selected_hover),
            (true, false) => Some(palette.selected),
            (false, true) => Some(palette.hover),
            (false, false) => None,
        };
        if let Some(fill) = fill {
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::ZERO, fill);
        }
        let mid = rect.center().y;
        let x = rect.left() + 14.0 + depth as f32 * 14.0;
        let icon = if is_dir {
            crate::icons::Icon::Folder
        } else {
            crate::icons::Icon::File
        };
        crate::icons::themed_image(icon, 12.0, palette.faint).paint_snapped(
            ui,
            egui::Rect::from_min_size(egui::pos2(x, mid - 6.0), egui::vec2(12.0, 12.0)),
        );
        let ink = if is_dir { palette.muted } else { palette.ink };
        let label = crate::ui::transcript::head::one_line(
            ui,
            name.to_owned(),
            egui::TextFormat::simple(
                egui::FontId::new(FS_SM, family(crate::theme::FAMILY_BODY_REGULAR)),
                ink,
            ),
            rect.right() - 14.0 - (x + 19.0) - if unlisted { 14.0 } else { 0.0 },
        );
        let label_right = x + 19.0 + label.size().x;
        ui.painter().galley(
            egui::pos2(x + 19.0, crate::theme::line_y(mid, 0.0, &label)),
            label,
            ink,
        );
        if unlisted {
            ui.painter().text(
                egui::pos2(label_right + 7.0, mid),
                egui::Align2::LEFT_CENTER,
                "\u{2026}",
                egui::FontId::new(FS_2XS, family(crate::theme::FAMILY_BODY_REGULAR)),
                palette.faint,
            );
        }
        if response.clicked() {
            if is_dir {
                if unlisted {
                    store.fs_list(&entry.path);
                }
            } else {
                workbench.set_file(Some(entry.path.clone()));
                events.push(WorkbenchEvent::ReadFile(entry.path.clone()));
            }
        }
    }
    file_preview(ui, store, &id, workbench, palette, events);
}

/// The preview of the selected file: its path with a mention button,
/// then the numbered lines, or a note when the file is binary or was
/// cut short.
fn file_preview(
    ui: &mut egui::Ui,
    store: &Store,
    session: &str,
    workbench: &Workbench,
    palette: &Palette,
    events: &mut Vec<WorkbenchEvent>,
) {
    let Some(selected) = workbench.file() else {
        return;
    };
    let Some((path, read)) = store.fs_preview(session) else {
        return;
    };
    if path != selected {
        return;
    }
    ui.add_space(10.0);
    let (bar, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), egui::Sense::hover());
    for y in [bar.top() + 0.5, bar.bottom() - 0.5] {
        ui.painter()
            .hline(bar.x_range(), y, egui::Stroke::new(1.0, palette.subtle));
    }
    let mid = bar.center().y;
    crate::icons::themed_image(crate::icons::Icon::File, 14.0, palette.faint).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(bar.left() + 14.0, mid - 7.0),
            egui::vec2(14.0, 14.0),
        ),
    );
    let mention = egui::Rect::from_min_size(
        egui::pos2(bar.right() - 14.0 - 24.0, mid - 12.0),
        egui::vec2(24.0, 24.0),
    );
    let mentioned = ui
        .scope_builder(egui::UiBuilder::new().max_rect(mention), |ui| {
            crate::ui::forms::icon_btn(
                ui,
                "Mention in composer",
                crate::icons::Icon::AtSign,
                24.0,
                13.0,
                R_MD,
                crate::ui::forms::IconTones {
                    fill: egui::Color32::TRANSPARENT,
                    hover_fill: palette.hover,
                    ink: palette.muted,
                    hover_ink: palette.ink,
                },
            )
            .on_hover_text("Mention in composer")
        })
        .inner;
    if mentioned.clicked() {
        events.push(WorkbenchEvent::Mention(path.to_owned()));
    }
    ui.interact(bar, ui.id().with("wb-preview-head"), egui::Sense::hover())
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, path));
    let x = bar.left() + 14.0 + 14.0 + 8.0;
    let label = crate::ui::transcript::head::one_line(
        ui,
        path.to_owned(),
        egui::TextFormat::simple(
            egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)),
            palette.ink,
        ),
        mention.left() - 8.0 - x,
    );
    ui.painter().galley(
        egui::pos2(x, crate::theme::line_y(mid, 0.0, &label)),
        label,
        palette.ink,
    );
    if read.binary {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.label(
                egui::RichText::new("Binary file; not shown.")
                    .size(FS_XS)
                    .color(palette.faint),
            );
        });
        return;
    }
    egui::Frame::NONE
        .fill(palette.deep)
        .inner_margin(egui::Margin::symmetric(0, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let font = egui::FontId::new(12.0, family(FAMILY_MONO_REGULAR));
            for (n, line) in read.content.lines().enumerate() {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 19.0),
                    egui::Sense::hover(),
                );
                let mid = rect.center().y;
                ui.painter().text(
                    egui::pos2(rect.left() + 10.0 + 28.0, mid),
                    egui::Align2::RIGHT_CENTER,
                    (n + 1).to_string(),
                    font.clone(),
                    palette.ghost,
                );
                let text = crate::ui::transcript::head::one_line(
                    ui,
                    line.to_owned(),
                    egui::TextFormat::simple(font.clone(), palette.ink),
                    rect.right() - 10.0 - (rect.left() + 10.0 + 28.0 + 12.0),
                );
                ui.painter().galley(
                    egui::pos2(
                        rect.left() + 10.0 + 28.0 + 12.0,
                        crate::theme::line_y(mid, 0.0, &text),
                    ),
                    text,
                    palette.ink,
                );
            }
        });
    if read.truncated {
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.label(
                egui::RichText::new("Truncated: the file is longer than the preview.")
                    .size(FS_XS)
                    .color(palette.faint),
            );
        });
    }
}

/// The subagent detail: back, the name, the kind, the state with Stop
/// while it runs, the task and the measured facts, then its transcript.
fn agent_detail(
    ui: &mut egui::Ui,
    workbench: &mut Workbench,
    store: &Store,
    palette: &Palette,
    events: &mut Vec<WorkbenchEvent>,
) {
    let Some(id) = workbench.agent().map(str::to_owned) else {
        return;
    };
    let found = store
        .active_session()
        .and_then(|parent| parent.agents.get(&id).map(|agent| (parent, agent)));
    let Some((parent, agent)) = found else {
        empty_block(ui, crate::icons::Icon::Bot, "Agent not found.", palette);
        return;
    };
    let facts = agents::agent_facts(store, parent, &id, agent);
    let kind = match &facts.item {
        Some((_, index)) => format!("swarm #{:02}", index + 1),
        None => facts.name.clone(),
    };
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_space(8.0);
        if crate::ui::forms::icon_button(ui, "agent-back", crate::icons::Icon::ChevronLeft, palette)
            .on_hover_text("Back to agents")
            .clicked()
        {
            workbench.close_agent();
        }
        ui.label(
            egui::RichText::new(&facts.name)
                .size(FS_SM)
                .family(family(FAMILY_BODY_SEMIBOLD))
                .color(palette.ink_strong),
        );
        ui.label(egui::RichText::new(kind).size(11.0).color(palette.muted));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            if facts.stoppable
                && ui
                    .add(egui::Button::new(
                        egui::RichText::new("Stop")
                            .size(FS_XS)
                            .color(palette.danger),
                    ))
                    .clicked()
            {
                events.push(WorkbenchEvent::StopAgent(id.clone()));
            }
            ui.label(
                egui::RichText::new(facts.phase.label())
                    .size(FS_XS)
                    .color(facts.phase.color(palette)),
            );
        });
    });
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(&facts.task)
                .size(FS_SM)
                .color(palette.ink),
        );
    });
    let calls = store.state().session(&id).map_or(0, |child| {
        child
            .items
            .iter()
            .filter(|item| matches!(item, TranscriptItem::ToolCall(_)))
            .count()
    });
    let line = [
        Some(facts.name.clone()),
        facts.model.clone(),
        facts.elapsed.map(crate::clock::span),
        facts.tokens.map(|n| format!("{} tok", agents::tokens(n))),
        facts.cost.map(|cost| format!("USD {cost:.4}")),
        Some(format!("{calls} tool calls")),
        facts.item.as_ref().map(|(item, _)| format!("item: {item}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" \u{b7} ");
    ui.horizontal_wrapped(|ui| {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(line)
                .size(11.0)
                .family(family(FAMILY_MONO_REGULAR))
                .color(palette.faint),
        );
    });
    if let Some(reason) = &facts.reason {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(reason).size(FS_XS).color(palette.warn));
        });
    }
    ui.separator();
    let mut rows = Vec::new();
    if let Some(view) = workbench.agent_view.as_mut() {
        view.show(ui, store, &mut rows);
    }
    events.extend(rows.into_iter().map(WorkbenchEvent::Row));
}

/// The agents pane: the filter chips, the subagents, then each swarm's
/// workers under its description and done count. A row opens the
/// agent's detail tab.
fn agents_pane(
    ui: &mut egui::Ui,
    workbench: &mut Workbench,
    store: &Store,
    palette: &Palette,
    agents: &std::collections::BTreeMap<String, kage_client::Subagent>,
    events: &mut Vec<WorkbenchEvent>,
) {
    let (head, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 12.0 + 20.0 + 4.0),
        egui::Sense::hover(),
    );
    let mid = head.top() + 12.0 + 10.0;
    ui.interact(head, ui.id().with("wb-agents-head"), egui::Sense::hover())
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "AGENTS"));
    ui.painter().text(
        egui::pos2(head.left() + 14.0, mid),
        egui::Align2::LEFT_CENTER,
        "AGENTS",
        egui::FontId::new(FS_XS, family(FAMILY_BODY_SEMIBOLD)),
        palette.faint,
    );
    let mut right = head.right() - 14.0;
    for filter in Filter::ALL.iter().rev() {
        let on = workbench.filter() == *filter;
        let ink = if on {
            palette.ink_strong
        } else {
            palette.faint
        };
        let galley = ui.painter().layout_no_wrap(
            filter.label().to_owned(),
            egui::FontId::new(FS_2XS, family(crate::theme::FAMILY_BODY_REGULAR)),
            ink,
        );
        let chip = egui::Rect::from_min_max(
            egui::pos2(right - galley.size().x - 16.0, mid - 10.0),
            egui::pos2(right, mid + 10.0),
        );
        right = chip.left() - 2.0;
        let response = ui
            .interact(
                chip,
                ui.id().with(("wb-filter", filter.label())),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, filter.label())
        });
        let radius = egui::CornerRadius::same(10);
        if response.hovered() {
            ui.painter().rect_filled(chip, radius, palette.hover);
        } else if on {
            ui.painter().rect_filled(chip, radius, palette.selected);
        }
        ui.painter()
            .galley(chip.center() - galley.size() / 2.0, galley, ink);
        if response.clicked() {
            workbench.set_filter(*filter);
        }
    }
    let Some(parent) = store.active_session() else {
        return;
    };
    if agents.is_empty() {
        empty_block(
            ui,
            crate::icons::Icon::Users,
            "No agents yet. Subagents and swarm workers show up here while they run.",
            palette,
        );
        return;
    }
    let filter = workbench.filter();
    let mut shown_any = false;
    let solo: Vec<_> = agents
        .iter()
        .filter(|(_, agent)| agent.swarm.is_none() && filter.keeps(agent))
        .collect();
    if !solo.is_empty() {
        agent_group(ui, crate::icons::Icon::Bot, "Subagents", None, palette);
        for (id, agent) in solo {
            let facts = agents::agent_facts(store, parent, id, agent);
            agent_row(ui, &facts, palette, events);
        }
        shown_any = true;
    }
    for item in &parent.items {
        let TranscriptItem::ToolCall(call) = item else {
            continue;
        };
        if call.title != "swarm" {
            continue;
        }
        let members = agents::children_of(parent, &call.tool_call_id);
        let ended = members
            .iter()
            .filter(|(_, agent)| Filter::Done.keeps(agent))
            .count();
        let total = members.len();
        let shown: Vec<_> = members
            .into_iter()
            .filter(|(_, agent)| filter.keeps(agent))
            .collect();
        if shown.is_empty() {
            continue;
        }
        let description = call
            .input
            .as_ref()
            .and_then(|input| input.get("description"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Swarm");
        agent_group(
            ui,
            crate::icons::Icon::Waypoints,
            description,
            Some(&format!("{ended}/{total}")),
            palette,
        );
        for (id, agent) in shown {
            let facts = agents::agent_facts(store, parent, id, agent);
            agent_row(ui, &facts, palette, events);
        }
        shown_any = true;
    }
    if !shown_any {
        empty_block(
            ui,
            crate::icons::Icon::Users,
            "No agents match this filter.",
            palette,
        );
    }
}

/// The label over one group of agent rows, with its count.
fn agent_group(
    ui: &mut egui::Ui,
    icon: crate::icons::Icon,
    label: &str,
    count: Option<&str>,
    palette: &Palette,
) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 10.0 + 18.0 + 4.0),
        egui::Sense::hover(),
    );
    let mid = rect.top() + 10.0 + 9.0;
    crate::icons::themed_image(icon, 12.0, palette.faint).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(rect.left() + 14.0, mid - 6.0),
            egui::vec2(12.0, 12.0),
        ),
    );
    let mut right = rect.right() - 14.0;
    if let Some(count) = count {
        let galley = ui.painter().layout_no_wrap(
            count.to_owned(),
            egui::FontId::new(FS_2XS, family(FAMILY_MONO_REGULAR)),
            palette.faint,
        );
        right -= galley.size().x;
        ui.painter().galley(
            egui::pos2(right, crate::theme::line_y(mid, 0.0, &galley)),
            galley,
            palette.faint,
        );
        right -= 6.0;
    }
    let x = rect.left() + 14.0 + 12.0 + 6.0;
    let text = crate::ui::transcript::head::one_line(
        ui,
        label.to_owned(),
        egui::TextFormat::simple(
            egui::FontId::new(FS_2XS, family(crate::theme::FAMILY_BODY_REGULAR)),
            palette.faint,
        ),
        right - x,
    );
    ui.painter().galley(
        egui::pos2(x, crate::theme::line_y(mid, 0.0, &text)),
        text,
        palette.faint,
    );
}

/// One agent row: avatar, the swarm index and name, the latest tool
/// line or the result or the task, the state with elapsed time and
/// tokens, and Stop while it runs. A click opens the agent's detail.
fn agent_row(
    ui: &mut egui::Ui,
    facts: &agents::AgentFacts,
    palette: &Palette,
    events: &mut Vec<WorkbenchEvent>,
) {
    let name_h = crate::theme::line_h(FS_SM);
    let line_h = crate::theme::line_h(FS_XS);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 7.0 + name_h + line_h + 7.0),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &facts.name));
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::ZERO, palette.hover);
    }
    let mid = rect.center().y;
    let tile = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 14.0, mid - 13.0),
        egui::vec2(26.0, 26.0),
    );
    agents::paint_avatar(ui, tile, &facts.name, palette);
    let mut right = rect.right() - 14.0;
    let mut stopped = false;
    if facts.stoppable {
        let stop =
            egui::Rect::from_min_size(egui::pos2(right - 22.0, mid - 11.0), egui::vec2(22.0, 22.0));
        stopped = ui
            .scope_builder(egui::UiBuilder::new().max_rect(stop), |ui| {
                crate::ui::forms::icon_btn(
                    ui,
                    "Stop",
                    crate::icons::Icon::Square,
                    22.0,
                    12.0,
                    R_MD,
                    crate::ui::forms::IconTones {
                        fill: egui::Color32::TRANSPARENT,
                        hover_fill: palette.fill_hover,
                        ink: palette.muted,
                        hover_ink: palette.ink,
                    },
                )
                .on_hover_text("Stop")
                .clicked()
            })
            .inner;
        right = stop.left() - 9.0;
    }
    let state_color = facts.phase.color(palette);
    let state = ui.painter().layout_no_wrap(
        facts.phase.label().to_owned(),
        egui::FontId::new(FS_XS, family(crate::theme::FAMILY_BODY_REGULAR)),
        state_color,
    );
    let meta = ui.painter().layout_no_wrap(
        [
            facts.elapsed.map(crate::clock::span),
            facts.tokens.map(|n| format!("{} tok", agents::tokens(n))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" \u{b7} "),
        egui::FontId::new(11.0, family(FAMILY_MONO_REGULAR)),
        palette.faint,
    );
    let column_w = (state.size().x + 17.0).max(meta.size().x);
    let top = rect.top() + 7.0;
    let state_w = state.size().x;
    ui.painter().galley(
        egui::pos2(right - state_w, top + (name_h - state.size().y) / 2.0),
        state,
        state_color,
    );
    crate::icons::themed_image(facts.phase.icon(), 12.0, state_color).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(right - state_w - 5.0 - 12.0, top + name_h / 2.0 - 6.0),
            egui::vec2(12.0, 12.0),
        ),
    );
    let meta_w = meta.size().x;
    ui.painter().galley(
        egui::pos2(
            right - meta_w,
            top + name_h + (line_h - meta.size().y) / 2.0,
        ),
        meta,
        palette.faint,
    );
    right -= column_w + 9.0;
    let x = tile.right() + 9.0;
    let mut nx = x;
    let body = egui::FontId::new(FS_SM, family(crate::theme::FAMILY_BODY_REGULAR));
    let name = match &facts.item {
        Some((item, index)) => {
            let galley = ui.painter().layout_no_wrap(
                format!("{:02}", index + 1),
                egui::FontId::new(11.0, family(FAMILY_MONO_REGULAR)),
                palette.faint,
            );
            let width = galley.size().x;
            ui.painter().galley(
                egui::pos2(nx, top + (name_h - galley.size().y) / 2.0),
                galley,
                palette.faint,
            );
            nx += width + 6.0;
            item.clone()
        }
        None => facts.name.clone(),
    };
    let badge_w = if facts.background { 6.0 + 22.0 } else { 0.0 };
    let label = crate::ui::transcript::head::one_line(
        ui,
        name,
        egui::TextFormat::simple(body, palette.ink),
        right - nx - badge_w,
    );
    let label_w = label.size().x;
    ui.painter().galley(
        egui::pos2(nx, top + (name_h - label.size().y) / 2.0),
        label,
        palette.ink,
    );
    if facts.background {
        let galley = ui.painter().layout_no_wrap(
            "bg".to_owned(),
            egui::FontId::new(10.0, family(crate::theme::FAMILY_BODY_REGULAR)),
            palette.accent,
        );
        let pill = egui::Rect::from_min_size(
            egui::pos2(nx + label_w + 6.0, top + name_h / 2.0 - 8.0),
            egui::vec2(galley.size().x + 12.0, 16.0),
        );
        ui.painter()
            .rect_filled(pill, egui::CornerRadius::same(8), palette.accent_soft);
        ui.painter()
            .galley(pill.center() - galley.size() / 2.0, galley, palette.accent);
    }
    let line = if facts.phase.live() {
        facts.last.clone().unwrap_or_else(|| facts.task.clone())
    } else {
        facts.result.clone().unwrap_or_else(|| facts.task.clone())
    };
    let line = crate::ui::transcript::head::one_line(
        ui,
        line.replace('`', ""),
        egui::TextFormat::simple(
            egui::FontId::new(FS_XS, family(crate::theme::FAMILY_BODY_REGULAR)),
            palette.faint,
        ),
        right - x,
    );
    ui.painter().galley(
        egui::pos2(x, top + name_h + (line_h - line.size().y) / 2.0),
        line,
        palette.faint,
    );
    if stopped {
        events.push(WorkbenchEvent::StopAgent(facts.id.clone()));
    } else if response.clicked() {
        events.push(WorkbenchEvent::OpenAgent(facts.id.clone()));
    }
}

/// The terminal pane: the shell calls the session and its subagents
/// ran, as cards; the latest and the running ones start open.
fn terminal_pane(ui: &mut egui::Ui, workbench: &mut Workbench, store: &Store, palette: &Palette) {
    let Some(session) = store.active_session() else {
        return;
    };
    let runs = session_runs(store, session);
    if runs.is_empty() {
        empty_block(
            ui,
            crate::icons::Icon::Terminal,
            "Shell commands and their output collect here.",
            palette,
        );
        return;
    }
    let last = runs.len() - 1;
    for (ix, run) in runs.iter().enumerate() {
        let open = workbench.run_open(&run.id, run.running, ix == last);
        if term_run(ui, run, open, palette) {
            workbench.flip_run(&run.id);
        }
    }
}

/// One shell run as a card: its state, the command's first line and
/// who ran it on a head that folds the rest, then the whole command and
/// the tail of its output. Answers whether the head was clicked.
fn term_run(ui: &mut egui::Ui, run: &TermEntry, open: bool, palette: &Palette) -> bool {
    let mono = |size: f32| egui::FontId::new(size, family(FAMILY_MONO_REGULAR));
    let mut clicked = false;
    ui.add_space(8.0);
    egui::Frame::NONE
        .outer_margin(egui::Margin::symmetric(12, 0))
        .fill(palette.deep)
        .stroke(egui::Stroke::new(1.0, palette.line))
        .corner_radius(egui::CornerRadius::same(R_MD as u8))
        .show_crisp(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            let (head, response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), 7.0 + 18.0 + 7.0),
                egui::Sense::click(),
            );
            let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &run.command)
            });
            if response.hovered() {
                let radius = if open {
                    egui::CornerRadius {
                        nw: R_MD as u8,
                        ne: R_MD as u8,
                        sw: 0,
                        se: 0,
                    }
                } else {
                    egui::CornerRadius::same(R_MD as u8)
                };
                ui.painter().rect_filled(head, radius, palette.fill_hover);
            }
            clicked = response.clicked();
            let mid = head.center().y;
            let state = egui::Rect::from_min_size(
                egui::pos2(head.left() + 10.0, mid - 6.0),
                egui::vec2(12.0, 12.0),
            );
            let mark = match run.exit {
                _ if run.running => None,
                Some(0) => Some((crate::icons::Icon::Check, palette.ok)),
                Some(_) => Some((crate::icons::Icon::X, palette.danger)),
                None if run.failed => Some((crate::icons::Icon::X, palette.danger)),
                None => Some((crate::icons::Icon::Minus, palette.faint)),
            };
            match mark {
                Some((icon, color)) => {
                    crate::icons::themed_image(icon, 12.0, color).paint_snapped(ui, state);
                }
                None => crate::ui::transcript::head::paint_spinner(ui, state, palette.accent),
            }
            let mut right = head.right() - 10.0;
            if let Some(code) = run.exit.filter(|code| *code != 0) {
                let text = format!("exit {code}");
                let width = ui
                    .painter()
                    .layout_no_wrap(text.clone(), mono(FS_2XS), palette.danger)
                    .size()
                    .x
                    + 14.0;
                right = crate::ui::transcript::head::paint_chip(
                    ui,
                    egui::pos2(right - width, mid),
                    &text,
                    palette.danger,
                    palette.danger_soft,
                )
                .left()
                    - 8.0;
            }
            if let Some(who) = &run.who {
                right = by_badge(ui, right, mid, who, palette) - 8.0;
            }
            let first = run.command.lines().next().unwrap_or_default().to_owned();
            let more = run.command.lines().count() > 1;
            let x = state.right() + 8.0;
            let line = crate::ui::transcript::head::one_line(
                ui,
                if more && !open {
                    format!("{first} \u{2026}")
                } else {
                    first
                },
                egui::TextFormat::simple(mono(FS_XS), palette.ink),
                right - x,
            );
            ui.painter().galley(
                egui::pos2(x, crate::theme::line_y(mid, 0.0, &line)),
                line,
                palette.ink,
            );
            if !open {
                return;
            }
            let (rule, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rule, egui::CornerRadius::ZERO, palette.line);
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 6.0;
                    let text =
                        |ui: &mut egui::Ui, body: String, size: f32, color: egui::Color32| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(body)
                                        .font(mono(size))
                                        .line_height(Some(18.0))
                                        .color(color),
                                )
                                .wrap(),
                            );
                        };
                    let rest: Vec<&str> = run.command.lines().skip(1).collect();
                    if !rest.is_empty() {
                        text(ui, rest.join("\n"), 12.0, palette.muted);
                    }
                    if run.hidden > 0 {
                        text(
                            ui,
                            format!(
                                "\u{2026} {} earlier line{}",
                                run.hidden,
                                if run.hidden == 1 { "" } else { "s" }
                            ),
                            FS_2XS,
                            palette.faint,
                        );
                    }
                    if !run.output.is_empty() {
                        text(ui, run.output.clone(), 12.0, palette.ink);
                    } else if !run.running {
                        text(ui, "(no output)".to_owned(), 12.0, palette.faint);
                    }
                });
        });
    clicked
}

/// The browser pane: the latest fetched page as reader text, its title
/// from the first heading, its URL, its paragraphs and its size.
/// Nothing on the page ever loads.
fn browser_pane(ui: &mut egui::Ui, palette: &Palette, items: &[TranscriptItem]) {
    let Some(page) = fetch_entries(items).pop() else {
        empty_block(
            ui,
            crate::icons::Icon::Globe,
            "Pages the agent fetches open here as reader text. No scripts, no remote images.",
            palette,
        );
        return;
    };
    let text = strip_ansi(&page.text);
    let title = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("# "))
        .unwrap_or(page.url.as_str())
        .to_owned();
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(18, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            let wrap = |ui: &mut egui::Ui, text: egui::RichText| {
                ui.add(egui::Label::new(text).wrap());
            };
            wrap(
                ui,
                egui::RichText::new(title)
                    .size(18.0)
                    .family(family(FAMILY_BODY_SEMIBOLD))
                    .color(palette.ink_strong),
            );
            wrap(
                ui,
                egui::RichText::new(page.url.clone())
                    .size(11.0)
                    .family(family(FAMILY_MONO_REGULAR))
                    .color(palette.faint),
            );
            for paragraph in text
                .split("\n\n")
                .map(str::trim)
                .filter(|paragraph| !paragraph.is_empty())
            {
                wrap(
                    ui,
                    egui::RichText::new(paragraph)
                        .size(FS_SM)
                        .line_height(Some(21.0))
                        .color(palette.ink),
                );
            }
            let (rule, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rule, egui::CornerRadius::ZERO, palette.subtle);
            ui.label(
                egui::RichText::new(format!(
                    "{} fetched \u{b7} reader text only, remote content never loads",
                    bytes(page.text.len())
                ))
                .size(FS_2XS)
                .color(palette.faint),
            );
        });
}

#[cfg(test)]
mod tests {
    use super::Workbench;

    #[test]
    fn opening_an_agent_shows_its_detail_and_closing_returns() {
        let mut workbench = Workbench::new();
        assert_eq!(workbench.agent(), None);
        workbench.open_agent("child-1".to_owned());
        assert_eq!(workbench.agent(), Some("child-1"));
        workbench.close_agent();
        assert_eq!(workbench.agent(), None);
    }

    use super::{
        ChangeEntry, Filter, Tab, TermEntry, bytes, change_entries, change_path, fetch_count,
        fetch_entries, strip_ansi, term_entries,
    };
    use kage_client::wire::{
        DiffContent, FsEntry, FsKind, ToolCallContent, ToolCallStatus, ToolKind,
    };
    use kage_client::{Frame, ToolCallItem, TranscriptItem};
    use serde_json::json;

    fn call(id: &str, kind: ToolKind, title: &str) -> ToolCallItem {
        ToolCallItem {
            tool_call_id: id.to_owned(),
            name: None,
            locations: None,
            title: title.to_owned(),
            kind,
            status: ToolCallStatus::Completed,
            content: Vec::new(),
            raw_output: None,
            input: None,
            swarm: None,
            took_ms: None,
        }
    }

    fn edit_call(id: &str, path: &str, old: &str, new: &str) -> ToolCallItem {
        ToolCallItem {
            content: vec![ToolCallContent::Diff(DiffContent {
                path: path.to_owned(),
                old_text: (!old.is_empty()).then(|| old.to_owned()),
                new_text: new.to_owned(),
            })],
            input: Some(json!({"path": path})),
            ..call(id, ToolKind::Edit, "edit")
        }
    }

    #[test]
    fn the_tabs_are_the_five_the_head_draws() {
        assert_eq!(Tab::ALL.len(), 5);
        assert_eq!(Tab::ALL[0], Tab::Changes);
        assert_eq!(Tab::ALL[0].label(), "Changes");
        assert_eq!(Tab::ALL[4].label(), "Browser");
        assert_eq!(Tab::default(), Tab::Changes);
    }

    #[test]
    fn the_filters_keep_the_agents_they_name() {
        let agent = |state, background| kage_client::Subagent {
            name: None,
            task: None,
            capabilities: None,
            state,
            swarm: None,
            reason: None,
            tool_call_id: None,
            usage: None,
            model: None,
            background,
        };
        assert!(Filter::All.keeps(&agent(None, false)));
        assert!(Filter::Running.keeps(&agent(None, false)));
        assert!(!Filter::Running.keeps(&agent(
            Some(kage_client::wire::SubagentState::Completed),
            false
        )));
        assert!(Filter::Done.keeps(&agent(
            Some(kage_client::wire::SubagentState::Completed),
            false
        )));
        assert!(!Filter::Done.keeps(&agent(None, false)));
        assert!(Filter::Background.keeps(&agent(None, true)));
        assert!(!Filter::Background.keeps(&agent(None, false)));
    }

    #[test]
    fn byte_counts_read_like_the_design() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(812), "812 B");
        assert_eq!(bytes(4_301), "4.2 KB");
        assert_eq!(bytes(1_258_291), "1.2 MB");
    }

    #[test]
    fn a_change_names_the_diff_path_first_then_the_input_then_the_title() {
        let diff_call = edit_call("c1", "src/main.rs", "old", "new");
        assert_eq!(change_path(&diff_call), "src/main.rs");
        let input_call = ToolCallItem {
            input: Some(json!({"path": "src/lib.rs"})),
            ..call("c2", ToolKind::Edit, "the title")
        };
        assert_eq!(change_path(&input_call), "src/lib.rs");
        let bare = call("c3", ToolKind::Edit, "the title");
        assert_eq!(change_path(&bare), "the title");
    }

    #[test]
    fn the_changes_fold_onto_their_paths_and_count_both_sides() {
        let items = vec![
            TranscriptItem::ToolCall(edit_call("c1", "src/main.rs", "a\nb\nc", "a\nB\nc\nd")),
            TranscriptItem::ToolCall(edit_call("c2", "src/main.rs", "", "x\ny")),
            TranscriptItem::ToolCall(edit_call("c3", "src/lib.rs", "z", "z\nw")),
            // Not a change kind.
            TranscriptItem::ToolCall(call("c4", ToolKind::Read, "read")),
        ];
        let changes = change_entries(&items);
        assert_eq!(changes.len(), 2, "the two main.rs calls fold onto one");
        let main = changes
            .iter()
            .find(|entry| entry.path == "src/main.rs")
            .expect("main.rs");
        assert_eq!(
            main.add, 5,
            "three adds from the first call, two from the second"
        );
        assert_eq!(
            main.del, 2,
            "two dels from the first call, none from the second"
        );
        assert!(!main.created);
        let lib = changes
            .iter()
            .find(|entry| entry.path == "src/lib.rs")
            .expect("lib.rs");
        assert_eq!(lib.add, 1);
        assert_eq!(lib.del, 0);
    }

    #[test]
    fn a_created_file_names_itself_as_created() {
        let items = vec![TranscriptItem::ToolCall(edit_call("c1", "new.rs", "", "x"))];
        let changes = change_entries(&items);
        assert!(changes[0].created, "no old text means the file was made");
    }

    #[test]
    fn ansi_escapes_drop_from_shell_output() {
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}after"), "after");
        assert_eq!(strip_ansi("\u{1b}[2K\u{1b}[1;32mok\u{1b}[m"), "ok");
        assert_eq!(
            strip_ansi("a\u{1b}b"),
            "ab",
            "a lone escape drops with its byte"
        );
    }

    #[test]
    fn the_shell_framing_drops_and_the_exit_code_names_itself() {
        let items = vec![TranscriptItem::ToolCall(ToolCallItem {
            raw_output: Some(json!({"exitCode": 0})),
            input: Some(json!({"command": "ls -la"})),
            ..call("c1", ToolKind::Execute, "shell")
        })];
        let runs = term_entries(&items);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].command, "ls -la");
        assert_eq!(runs[0].exit, Some(0));
        assert!(!runs[0].running);
        assert!(!runs[0].failed);
    }

    #[test]
    fn a_shell_run_names_its_command_from_its_input() {
        let items = vec![TranscriptItem::ToolCall(ToolCallItem {
            input: Some(json!({"command": "  cargo test  "})),
            ..call("c1", ToolKind::Execute, "shell")
        })];
        let runs = term_entries(&items);
        assert_eq!(runs[0].command, "cargo test", "the command is trimmed");
    }

    #[test]
    fn the_shell_tail_keeps_the_last_lines_and_counts_the_hidden() {
        let text: String = (0..50).map(|n| format!("line {n}\n")).collect();
        let (out, hidden) = super::tail(&text);
        assert_eq!(hidden, 10, "fifty lines, forty shown");
        assert!(out.contains("line 49"));
        assert!(!out.contains("line 0\n"));
        let (short, hidden) = super::tail("one\ntwo");
        assert_eq!(hidden, 0);
        assert_eq!(short, "one\ntwo");
    }

    #[test]
    fn the_browser_count_takes_only_fetches() {
        let items = vec![
            TranscriptItem::ToolCall(call("c1", ToolKind::Fetch, "https://a.test")),
            TranscriptItem::ToolCall(call("c2", ToolKind::Execute, "ls")),
            TranscriptItem::ToolCall(call("c3", ToolKind::Fetch, "https://b.test")),
        ];
        assert_eq!(fetch_count(&items), 2);
        assert_eq!(fetch_count(&items), fetch_entries(&items).len());
    }

    #[test]
    fn the_fetches_name_their_final_url() {
        let items = vec![TranscriptItem::ToolCall(ToolCallItem {
            raw_output: Some(json!({"url": "https://example.com/final"})),
            ..call("c1", ToolKind::Fetch, "https://example.com/start")
        })];
        let pages = fetch_entries(&items);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].url, "https://example.com/final");
        let bare = vec![TranscriptItem::ToolCall(call(
            "c2",
            ToolKind::Fetch,
            "https://x.test",
        ))];
        assert_eq!(
            fetch_entries(&bare)[0].url,
            "https://x.test",
            "no redirect keeps the title"
        );
    }

    #[test]
    fn the_workbench_opens_on_changes_and_tracks_its_state() {
        let mut workbench = super::Workbench::new();
        assert_eq!(workbench.tab(), Tab::Changes);
        workbench.set_tab(Tab::Agents);
        assert_eq!(workbench.tab(), Tab::Agents);
        workbench.set_filter(Filter::Running);
        assert_eq!(workbench.filter(), Filter::Running);
        workbench.set_file(Some("src/main.rs".to_owned()));
        assert_eq!(workbench.file(), Some("src/main.rs"));
        workbench.set_file(None);
        assert_eq!(workbench.file(), None);
        workbench.select(Some("c1".to_owned()));
        assert_eq!(workbench.selected(), Some("c1"));
        workbench.select(None);
        assert_eq!(workbench.selected(), None);
    }

    #[test]
    fn the_listing_ask_rides_the_session_it_was_made_for() {
        let mut workbench = super::Workbench::new();
        assert!(!workbench.listing_asked("s1"));
        workbench.mark_listing_asked("s1");
        assert!(workbench.listing_asked("s1"));
        assert!(!workbench.listing_asked("s2"), "a new session asks again");
    }

    #[test]
    fn a_shell_run_opens_on_its_default_and_flips_against_it() {
        let mut workbench = super::Workbench::new();
        // Running and the latest run start open; the rest folded.
        assert!(workbench.run_open("r1", true, false));
        assert!(workbench.run_open("r2", false, true));
        assert!(!workbench.run_open("r3", false, false));
        workbench.flip_run("r3");
        assert!(workbench.run_open("r3", false, false), "the flip opens it");
        workbench.flip_run("r3");
        assert!(
            !workbench.run_open("r3", false, false),
            "the second flip closes it"
        );
    }

    #[test]
    fn a_change_entry_carries_its_author() {
        let entry = ChangeEntry {
            call_id: "c1".to_owned(),
            path: "a.rs".to_owned(),
            add: 1,
            del: 0,
            created: false,
            by: Some("explore".to_owned()),
        };
        assert_eq!(entry.by.as_deref(), Some("explore"));
        let run = TermEntry {
            id: "c1".to_owned(),
            command: "ls".to_owned(),
            output: String::new(),
            hidden: 0,
            exit: None,
            running: false,
            failed: false,
            who: None,
        };
        assert_eq!(run.command, "ls");
        let listing = FsEntry {
            path: "src".to_owned(),
            kind: FsKind::Directory,
            size: 0,
        };
        assert_eq!(listing.kind, FsKind::Directory);
        let _ = Frame::Notification {
            method: "session/update".to_owned(),
            params: json!({"sessionId": "s", "update": {}}),
        };
    }
}
