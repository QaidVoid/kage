//! The sidebar: the brand row, the New session and Search controls,
//! the session list with status badges and relative times, and the
//! connection footer.
//!
//! The list carries live sessions first, then the recorded directory
//! entries no live session holds, each tagged with the project its
//! working directory names. Rows nest under the session they were
//! forked from, with guide rails past later siblings. A child agent's
//! session is reached through its parent's card and the Agents tab,
//! never listed on its own.

use crate::icons::PaintSnapped as _;
use std::collections::{BTreeSet, HashMap};

use kage_client::Session;
use kage_client::wire::StopReason;

use crate::clock::unix_seconds;
use crate::store::Store;
use crate::theme::{
    FAMILY_BODY_BOLD, FAMILY_BODY_REGULAR, FAMILY_BODY_SEMIBOLD, FAMILY_MONO_REGULAR, FS_2XS,
    FS_BASE, FS_SM, FS_XS, Palette, R_MD, SP_4, family,
};

const FOOTER_H: f32 = 52.0;

/// The brand row: the mark, the wordmark and the version.
const BRAND_H: f32 = 32.0;

/// The days from 1970-01-01 to a civil date.
#[must_use]
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * i64::from(m) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The Unix seconds of an ISO 8601 timestamp, fractions dropped.
/// `Z` and numeric offsets in hours and minutes are honored.
#[must_use]
pub fn epoch_seconds(iso: &str) -> Option<i64> {
    let year: i64 = iso.get(0..4)?.parse().ok()?;
    let month: u32 = iso.get(5..7)?.parse().ok()?;
    let day: u32 = iso.get(8..10)?.parse().ok()?;
    let hour: i64 = iso.get(11..13)?.parse().ok()?;
    let minute: i64 = iso.get(14..16)?.parse().ok()?;
    let second: i64 = iso.get(17..19)?.parse().ok()?;
    let rest = iso.get(19..).unwrap_or("");
    let offset = match rest.chars().next() {
        Some('+') => -offset_seconds(rest.get(1..)?)?,
        Some('-') => offset_seconds(rest.get(1..)?)?,
        _ => 0,
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second + offset)
}

/// The seconds a `hh:mm` UTC offset stands for.
fn offset_seconds(rest: &str) -> Option<i64> {
    let hours: i64 = rest.get(0..2)?.parse().ok()?;
    let minutes: i64 = rest.get(3..5).and_then(|s| s.parse().ok()).unwrap_or(0);
    Some(hours * 3_600 + minutes * 60)
}

/// The relative label the design's session rows carry: now, minutes,
/// hours or days, halves rounded up like the web client's.
#[must_use]
pub fn ago_label(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 45 {
        "now".to_owned()
    } else if s < 3_600 {
        format!("{}m", (s + 30) / 60)
    } else if s < 86_400 {
        format!("{}h", (s + 1_800) / 3_600)
    } else {
        format!("{}d", (s + 43_200) / 86_400)
    }
}

/// The relative label for an ISO 8601 timestamp against `now`.
#[must_use]
pub fn ago(iso: Option<&str>, now: i64) -> Option<String> {
    let updated = epoch_seconds(iso?)?;
    Some(ago_label(now - updated))
}

/// The project a working directory names: its last path segment, or
/// `local` when there is no directory.
#[must_use]
pub fn project_name(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd else {
        return "local".to_owned();
    };
    let trimmed = cwd.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if name.is_empty() {
        "local".to_owned()
    } else {
        name.to_owned()
    }
}

/// What a session row leads or badges with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    /// A permission ask waits; the row badges Approve.
    Approve,
    /// A question waits for an answer; the row badges Answer.
    Answer,
    /// A plan waits for review; the row badges Review.
    Review,
    /// A turn is in flight; the row leads with the working cat.
    Running,
    /// The last turn stopped in a refusal; the row badges Failed.
    Failed,
    /// Quiet: the row shows its relative time, or the swarm glyph in
    /// swarm mode.
    Idle,
}

/// Where session `session` stands, as its sidebar row shows it.
#[must_use]
pub fn row_state(session: &Session) -> RowState {
    if crate::store::plan_review(session).is_some() {
        RowState::Review
    } else if session
        .permissions
        .first()
        .is_some_and(|ask| ask.question.is_some())
    {
        RowState::Answer
    } else if !session.permissions.is_empty() {
        RowState::Approve
    } else if session.running || session.in_turn {
        RowState::Running
    } else if session.last_stop == Some(StopReason::Refusal) {
        RowState::Failed
    } else {
        RowState::Idle
    }
}

/// One session row before it lands in the list: the facts the rows
/// and the project grouping both read.
#[derive(Debug, Clone)]
pub struct RowItem {
    /// The session's id.
    pub id: String,
    /// The row's title, or `None` for an untitled one.
    pub title: Option<String>,
    /// ISO 8601 time of the last activity.
    pub updated_at: Option<String>,
    /// What the row leads or badges with.
    pub state: RowState,
    /// Whether the row is the active session.
    pub active: bool,
    /// The project the row groups under.
    pub project: String,
    /// The directory the session works in.
    pub dir: Option<String>,
    /// Whether the session works in swarm mode.
    pub swarm: bool,
    /// Whether it moved since the user last looked.
    pub unread: bool,
    /// The session it was forked from.
    pub parent: Option<String>,
    /// Whether the user pinned it to the top.
    pub pinned: bool,
}

/// Where a row sits in the fork forest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// The row's index in the list the forest was built from.
    pub ix: usize,
    /// How many forks deep it sits.
    pub depth: usize,
    /// Whether it is its parent's last fork.
    pub last: bool,
    /// Per depth, whether a guide rail runs past the row there because
    /// a later sibling of an ancestor follows.
    pub rails: Vec<bool>,
    /// How many forks it has.
    pub kids: usize,
}

/// `parents` as a forest: each row is followed by its forks, depth
/// first. A row whose parent is not in the list is a root. A row in a
/// cycle emits neither itself nor its cycle, because neither has a
/// root to hang from.
#[must_use]
pub fn forest(parents: &[Option<&str>], ids: &[&str]) -> Vec<Place> {
    fn walk(
        ix: usize,
        depth: usize,
        last: bool,
        rails: Vec<bool>,
        kids_of: &[Vec<usize>],
        out: &mut Vec<Place>,
    ) {
        let kids = &kids_of[ix];
        out.push(Place {
            ix,
            depth,
            last,
            rails: rails.clone(),
            kids: kids.len(),
        });
        for (n, &kid) in kids.iter().enumerate() {
            let mut rails = rails.clone();
            rails.resize(depth + 2, false);
            rails[depth + 1] = n + 1 < kids.len();
            walk(kid, depth + 1, n + 1 == kids.len(), rails, kids_of, out);
        }
    }
    let mut at_of: HashMap<&str, usize> = HashMap::with_capacity(ids.len());
    for (ix, id) in ids.iter().copied().enumerate() {
        at_of.entry(id).or_insert(ix);
    }
    let mut kids_of = vec![Vec::new(); ids.len()];
    let mut roots = Vec::new();
    for (ix, parent) in parents.iter().enumerate() {
        match parent
            .and_then(|id| at_of.get(id).copied())
            .filter(|&at| at != ix)
        {
            Some(at) => kids_of[at].push(ix),
            None => roots.push(ix),
        }
    }
    let mut out = Vec::with_capacity(ids.len());
    for root in roots {
        walk(root, 0, true, Vec::new(), &kids_of, &mut out);
    }
    out
}

/// The fork forest of `rows`.
#[must_use]
pub fn placed(rows: &[&RowItem]) -> Vec<Place> {
    let parents: Vec<Option<&str>> = rows.iter().map(|row| row.parent.as_deref()).collect();
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    forest(&parents, &ids)
}

/// Whether the session works in swarm mode.
#[must_use]
pub fn swarm_on(session: &Session) -> bool {
    session
        .config_options
        .iter()
        .any(|option| option.id == "swarm" && option.current_value == "on")
}

/// The rows the sidebar lists, in the order the design draws them:
/// pinned first, then newest first, with a live session that has no
/// time yet counted as the newest.
#[must_use]
pub fn sidebar_rows(store: &Store) -> Vec<RowItem> {
    let state = store.state();
    let mut items: Vec<RowItem> = Vec::new();
    for (id, session) in state.sessions.iter().filter(|(_, s)| s.parent.is_none()) {
        items.push(RowItem {
            id: id.clone(),
            title: Some(store.display_title(id)),
            updated_at: session.updated_at.clone(),
            state: row_state(session),
            active: store.active_id() == Some(id.as_str()),
            project: project_name(session.cwd.as_deref()),
            dir: session.cwd.clone(),
            swarm: swarm_on(session),
            unread: store.is_unread(id),
            parent: store.fork_parent(id).map(str::to_owned),
            pinned: store.prefs().pinned.contains(id),
        });
    }
    for info in &state.directory {
        if state.sessions.contains_key(&info.session_id) {
            continue;
        }
        items.push(RowItem {
            id: info.session_id.clone(),
            title: info.title.clone(),
            updated_at: info.updated_at.clone(),
            state: RowState::Idle,
            active: store.active_id() == Some(info.session_id.as_str()),
            project: project_name(Some(&info.cwd)),
            dir: Some(info.cwd.clone()),
            swarm: false,
            unread: false,
            parent: store.fork_parent(&info.session_id).map(str::to_owned),
            pinned: store.prefs().pinned.contains(&info.session_id),
        });
    }
    // Archived sessions leave the list until restored.
    items.retain(|item| !store.prefs().archived.contains(&item.id));
    items.sort_by_key(|item| {
        (
            !item.pinned,
            std::cmp::Reverse(
                item.updated_at
                    .as_deref()
                    .and_then(epoch_seconds)
                    .unwrap_or(i64::MAX),
            ),
        )
    });
    items
}

/// The rows `query` keeps: the title and the project both match, case
/// insensitively, and an empty query keeps every row.
#[must_use]
pub fn filter_rows(rows: &[RowItem], query: &str) -> Vec<RowItem> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|row| {
            let title = row.title.as_deref().unwrap_or("untitled session");
            format!("{title} {}", row.project)
                .to_lowercase()
                .contains(&needle)
        })
        .cloned()
        .collect()
}

/// The projects `rows` group under, in first-seen order, each with the
/// rows under it.
#[must_use]
pub fn project_groups(rows: &[RowItem]) -> Vec<(String, Vec<RowItem>)> {
    let mut groups: Vec<(String, Vec<RowItem>)> = Vec::new();
    for row in rows {
        if let Some(group) = groups.iter_mut().find(|(name, _)| *name == row.project) {
            group.1.push(row.clone());
        } else {
            groups.push((row.project.clone(), vec![row.clone()]));
        }
    }
    groups
}

/// The connection footer's two lines: the link's name, and the state
/// the transport stands in.
#[must_use]
pub fn connect_lines(store: &Store) -> (String, String) {
    let link = store.link();
    let status = match store.connect() {
        crate::transport::State::Connected => link.detail.clone(),
        crate::transport::State::Connecting => "connecting\u{2026}".to_owned(),
        crate::transport::State::Reconnecting { .. } => "reconnecting\u{2026}".to_owned(),
        crate::transport::State::Refused(_) | crate::transport::State::Closed => {
            "not running".to_owned()
        }
    };
    (link.name.to_owned(), status)
}

/// The connection lamp's color, and whether it carries a soft halo.
#[must_use]
pub fn connect_dot(palette: &Palette, store: &Store) -> (egui::Color32, bool) {
    match store.connect() {
        crate::transport::State::Connected => (palette.ok, true),
        crate::transport::State::Connecting | crate::transport::State::Reconnecting { .. } => {
            (palette.warn, false)
        }
        crate::transport::State::Refused(_) => (palette.danger, false),
        crate::transport::State::Closed => (palette.faint, false),
    }
}

/// What the sidebar asks the shell to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarEvent {
    /// Open a new session in `dir`, or the shell's own when `None`.
    NewSession(Option<String>),
    /// Make `id` the active session.
    Activate(String),
    /// Open the command palette.
    OpenPalette,
    /// Open the settings page.
    OpenSettings,
    /// Collapse the sidebar.
    Collapse,
    /// Toggle the project grouping.
    ToggleGrouping,
}

/// The sidebar's state: the search query and the collapsed projects.
#[derive(Debug, Default)]
pub struct Sidebar {
    /// The query the list narrows over.
    query: String,
    /// The projects whose groups are collapsed, keyed by name.
    collapsed: BTreeSet<String>,
}

impl Sidebar {
    /// A closed sidebar over an empty query.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The query the list narrows over.
    pub fn query(&mut self) -> &mut String {
        &mut self.query
    }

    /// Whether `project`'s group is collapsed.
    #[must_use]
    pub fn collapsed(&self, project: &str) -> bool {
        self.collapsed.contains(project)
    }

    /// Collapses or opens `project`'s group.
    pub fn toggle_project(&mut self, project: &str) {
        if !self.collapsed.remove(project) {
            self.collapsed.insert(project.to_owned());
        }
    }
}

/// Draws the sidebar and reports what its controls asked for.
pub fn show(ui: &mut egui::Ui, sidebar: &mut Sidebar, store: &Store) -> Vec<SidebarEvent> {
    let rows = sidebar_rows(store);
    let grouped = store.prefs().group_by_project;
    let (name, status) = connect_lines(store);
    let palette = Palette::active_of(ui.ctx());
    let (dot, halo) = connect_dot(palette, store);
    let version = store
        .state()
        .agent
        .as_ref()
        .and_then(|agent| agent.version.clone());
    show_rows(
        ui,
        sidebar,
        &rows,
        grouped,
        (&name, &status, dot, halo),
        version.as_deref(),
    )
}

/// Draws the sidebar over `rows`, which the caller read from the store
/// once. The panel draws the sidebar twice while narrow, so the rows
/// ride in as data rather than being read per draw.
pub fn show_rows(
    ui: &mut egui::Ui,
    sidebar: &mut Sidebar,
    rows: &[RowItem],
    grouped: bool,
    link: (&str, &str, egui::Color32, bool),
    version: Option<&str>,
) -> Vec<SidebarEvent> {
    let palette = Palette::active_of(ui.ctx());
    let mut events = Vec::new();
    let now = unix_seconds();

    let (name, status, dot, halo) = link;
    ui.vertical(|ui| {
        ui.set_width(ui.available_width());
        brand_row(ui, version, &mut events);
        ui.add_space(4.0);
        nav_rows(ui, &mut events);
        ui.add_space(16.0);
        list_header(ui, grouped, &mut events);
        list_body(ui, sidebar, rows, grouped, now, &mut events);
        connect_footer(ui, palette, name, status, dot, halo, &mut events);
    });
    events
}

/// The brand row: the nekomata mark, the wordmark, the engine version
/// and the collapse control.
fn brand_row(ui: &mut egui::Ui, version: Option<&str>, events: &mut Vec<SidebarEvent>) {
    let palette = Palette::active_of(ui.ctx());
    let (rect, _) = ui.allocate_at_least(
        egui::vec2(ui.available_width(), BRAND_H),
        egui::Sense::hover(),
    );
    let mark = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 6.0, rect.center().y - 11.0),
        crate::ui::mark::size(22.0),
    );
    crate::ui::mark::paint(ui, mark);
    let wordmark = line_at(
        ui,
        mark.right() + 9.0,
        rect.center().y,
        "kage",
        egui::FontId::new(15.0, family(FAMILY_BODY_BOLD)),
        palette.ink_strong,
    );
    if let Some(version) = version {
        line_at(
            ui,
            wordmark.right() + 9.0,
            rect.center().y,
            version,
            egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)),
            palette.faint,
        );
    }
    let collapse = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 16.0, rect.center().y),
        egui::vec2(28.0, 28.0),
    );
    let tip = "Collapse sidebar (Ctrl \\)";
    if ghost_icon(ui, collapse, crate::icons::Icon::PanelLeft, tip, palette).clicked() {
        events.push(SidebarEvent::Collapse);
    }
}

/// Paints `text` with its line centered on `mid` the way the reference
/// centers a line, its left edge at `x`. Gives back the line's rect.
fn line_at(
    ui: &egui::Ui,
    x: f32,
    mid: f32,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
) -> egui::Rect {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let y = crate::theme::line_y(mid, 0.0, &galley);
    let rect = egui::Rect::from_min_size(
        egui::pos2(x, y),
        egui::vec2(galley.size().x.ceil(), galley.size().y),
    );
    ui.painter().galley(rect.min, galley, color);
    rect
}

/// The New session and Search nav rows.
fn nav_rows(ui: &mut egui::Ui, events: &mut Vec<SidebarEvent>) {
    let palette = Palette::active_of(ui.ctx());
    ui.spacing_mut().item_spacing.y = 1.0;
    if nav_row(
        ui,
        palette,
        crate::icons::Icon::SquarePen,
        "New session",
        "Ctrl N",
    ) {
        events.push(SidebarEvent::NewSession(None));
    }
    if nav_row(ui, palette, crate::icons::Icon::Search, "Search", "Ctrl K") {
        events.push(SidebarEvent::OpenPalette);
    }
}

/// One full-width nav row: the icon, the label and the shortcut chip.
/// Reports whether it was clicked.
fn nav_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    icon: crate::icons::Icon,
    label: &str,
    keys: &str,
) -> bool {
    let (rect, response) =
        ui.allocate_at_least(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, egui::CornerRadius::same(R_MD as u8), palette.hover);
    }
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 8.0 + 8.0, rect.center().y),
        egui::vec2(16.0, 16.0),
    );
    crate::icons::themed_image(icon, 16.0, palette.ink).paint_snapped(ui, icon_rect);
    line_at(
        ui,
        rect.left() + 8.0 + 16.0 + 10.0,
        rect.center().y,
        label,
        egui::FontId::new(FS_BASE, family(FAMILY_BODY_REGULAR)),
        palette.ink,
    );
    // The reference's key cap, 8px in from the row's end.
    crate::ui::palette::paint_kbd(
        ui,
        egui::pos2(
            rect.right() - 8.0,
            crate::theme::snap(rect.center().y - 9.0),
        ),
        keys,
        palette,
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    response.clicked()
}

/// The list header: the SESSIONS label and the grouping toggle.
fn list_header(ui: &mut egui::Ui, grouped: bool, events: &mut Vec<SidebarEvent>) {
    let palette = Palette::active_of(ui.ctx());
    // One 20px row, the toggle's height, the label's line centered on it.
    let (row, label) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
    label.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "SESSIONS"));
    let galley = ui.painter().layout_no_wrap(
        "SESSIONS".to_owned(),
        egui::FontId::new(FS_XS, family(FAMILY_BODY_SEMIBOLD)),
        palette.faint,
    );
    // The row sits on whole pixels in the reference, whose layout rounds
    // the label's half-pixel offset up.
    let line = crate::theme::line_h(FS_XS);
    let line_top = row.top() + ((row.height() - line) / 2.0).round();
    let y = crate::theme::line_y(line_top, line, &galley);
    ui.painter()
        .galley(egui::pos2(row.left() + 8.0, y), galley, palette.faint);
    let rect = egui::Rect::from_min_size(
        egui::pos2(row.right() - 20.0, row.top()),
        egui::vec2(20.0, 20.0),
    );
    let response = ui
        .interact(
            rect,
            ui.id().with("sessions-grouping"),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(if grouped {
            "Show as one list"
        } else {
            "Group by project"
        });
    let glyph = if grouped {
        crate::icons::Icon::List
    } else {
        crate::icons::Icon::Folder
    };
    crate::icons::themed_image(glyph, 13.0, palette.faint).paint_snapped(
        ui,
        egui::Rect::from_center_size(rect.center(), egui::vec2(13.0, 13.0)),
    );
    if response.clicked() {
        events.push(SidebarEvent::ToggleGrouping);
    }
    ui.add_space(4.0);
}

/// The list body: groups under project rows, or one flat list.
fn list_body(
    ui: &mut egui::Ui,
    sidebar: &mut Sidebar,
    rows: &[RowItem],
    grouped: bool,
    now: i64,
    events: &mut Vec<SidebarEvent>,
) {
    let palette = Palette::active_of(ui.ctx());
    let filtered = filter_rows(rows, &sidebar.query().clone());
    // The footer sits on the window's bottom edge, as the reference
    // pins it; the panel's own rect can run a pixel past the window.
    let bottom = ui.ctx().content_rect().bottom() - SP_4;
    let body_height =
        (bottom - ui.cursor().top() - (FOOTER_H - SP_4) - ui.spacing().item_spacing.y).max(0.0);
    egui::ScrollArea::vertical()
        .max_height(body_height)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            if grouped {
                let groups = project_groups(&filtered);
                let shown: usize = groups
                    .iter()
                    .filter(|(project, _)| !sidebar.collapsed(project))
                    .map(|(_, members)| members.len().max(1))
                    .sum();
                let content = groups.len() as f32 * (PROJECT_ROW_H + 4.0) + shown as f32 * 32.0;
                let head_h = if content <= body_height {
                    PROJECT_ROW_H
                } else {
                    PROJECT_ROW_SHRUNK_H
                };
                for (project, members) in groups {
                    project_row(ui, sidebar, palette, &project, &members, head_h, events);
                    if sidebar.collapsed(&project) {
                        continue;
                    }
                    if members.is_empty() {
                        ui.label(
                            egui::RichText::new("No conversations yet")
                                .size(FS_SM)
                                .color(palette.faint),
                        );
                        continue;
                    }
                    let refs: Vec<&RowItem> = members.iter().collect();
                    for place in placed(&refs) {
                        session_row(ui, palette, &members[place.ix], &place, true, now, events);
                    }
                }
            } else {
                let refs: Vec<&RowItem> = filtered.iter().collect();
                for place in placed(&refs) {
                    session_row(ui, palette, &filtered[place.ix], &place, false, now, events);
                }
            }
        });
}

/// One project group row, with its count and its new-session control.
fn project_row(
    ui: &mut egui::Ui,
    sidebar: &mut Sidebar,
    palette: &Palette,
    project: &str,
    members: &[RowItem],
    height: f32,
    events: &mut Vec<SidebarEvent>,
) {
    let open = !sidebar.collapsed(project);
    let dir = members.iter().find_map(|item| item.dir.clone());
    ui.add_space(4.0);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let plus = egui::Rect::from_min_size(
        egui::pos2(rect.right() - 8.0 - 22.0, rect.center().y - 11.0),
        egui::vec2(22.0, 22.0),
    );
    let plus_response = dir.as_ref().map(|_| {
        ui.interact(
            plus,
            ui.id().with(("project-add", project)),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("New session")
    });
    let plus_hovered = plus_response.as_ref().is_some_and(egui::Response::hovered);
    let hovered = response.hovered() || plus_hovered;
    if hovered {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(R_MD as u8), palette.hover);
    }
    let ink = if hovered { palette.ink } else { palette.muted };
    let mid = rect.center().y;
    let folder = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 8.0, mid - 7.0),
        egui::vec2(14.0, 14.0),
    );
    let glyph = if open {
        crate::icons::Icon::FolderOpen
    } else {
        crate::icons::Icon::Folder
    };
    crate::icons::themed_image(glyph, 14.0, palette.muted).paint_snapped(ui, folder);
    let mut right = if dir.is_some() {
        plus.left() - 8.0
    } else {
        rect.right() - 8.0
    };
    if !members.is_empty() {
        let count = ui.painter().layout_no_wrap(
            members.len().to_string(),
            egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
            palette.faint,
        );
        let left = right - count.size().x.ceil();
        ui.painter().galley(
            egui::pos2(left, crate::theme::line_y(mid, 0.0, &count)),
            count,
            palette.faint,
        );
        right = left - 8.0;
    }
    let name_left = folder.right() + 8.0;
    let name = crate::ui::transcript::head::one_line(
        ui,
        project.to_owned(),
        egui::TextFormat::simple(egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)), ink),
        right - name_left,
    );
    let y = crate::theme::line_y(mid, 0.0, &name);
    ui.painter().galley(egui::pos2(name_left, y), name, ink);
    if let Some(add) = &plus_response {
        if add.hovered() {
            ui.painter().rect_filled(
                plus,
                egui::CornerRadius::same(R_MD as u8),
                palette.fill_hover,
            );
        }
        let tint = if add.hovered() {
            palette.ink
        } else {
            palette.faint
        };
        crate::icons::themed_image(crate::icons::Icon::Plus, 13.0, tint).paint_snapped(
            ui,
            egui::Rect::from_center_size(plus.center(), egui::vec2(13.0, 13.0)),
        );
    }
    match (plus_response, dir) {
        (Some(add), Some(dir)) if add.clicked() => {
            events.push(SidebarEvent::NewSession(Some(dir)));
        }
        _ if response.clicked() => sidebar.toggle_project(project),
        _ => {}
    }
}

/// A project header's height, as the reference declares it.
const PROJECT_ROW_H: f32 = 32.0;
/// A project header's height once the list overflows: the reference's
/// flex list shrinks the header down to its 22px plus button.
const PROJECT_ROW_SHRUNK_H: f32 = 22.0;

/// One session row: title, then the attention badge, the working cat
/// or the swarm glyph, else the relative time. An unread row reads
/// bold.
fn session_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    item: &RowItem,
    place: &Place,
    grouped: bool,
    now: i64,
    events: &mut Vec<SidebarEvent>,
) {
    let base = if grouped { 32.0 } else { 8.0 };
    let indent = base + place.depth as f32 * 16.0;
    let fg = if item.active {
        palette.ink_strong
    } else {
        palette.muted
    };
    let title = item
        .title
        .clone()
        .unwrap_or_else(|| "untitled session".to_owned());
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let fill = match (item.active, response.hovered()) {
        (true, false) => Some(palette.selected),
        (true, true) => Some(palette.selected_hover),
        (false, true) => Some(palette.hover),
        (false, false) => None,
    };
    if let Some(fill) = fill {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(R_MD as u8), fill);
    }
    let fg = if response.hovered() && !item.active {
        palette.ink
    } else {
        fg
    };
    // The fork guides: a rail per depth, with the elbow at the row's
    // own depth.
    for depth in 1..=place.depth {
        let left = rect.left() + base - 14.0 + (depth - 1) as f32 * 16.0;
        if depth < place.depth {
            if place.rails.get(depth).copied().unwrap_or(false) {
                ui.painter().vline(
                    left + 6.0,
                    egui::Rangef::new(rect.top(), rect.bottom()),
                    egui::Stroke::new(1.0, palette.line_strong),
                );
            }
            continue;
        }
        let color = if item.active {
            palette.accent_bd
        } else {
            palette.line_strong
        };
        ui.painter().hline(
            egui::Rangef::new(left, left + 9.0),
            rect.center().y,
            egui::Stroke::new(1.0, color),
        );
        ui.painter().vline(
            left,
            egui::Rangef::new(rect.top(), rect.center().y),
            egui::Stroke::new(1.0, palette.line_strong),
        );
        if !place.last {
            ui.painter().vline(
                left,
                egui::Rangef::new(rect.center().y, rect.bottom()),
                egui::Stroke::new(1.0, palette.line_strong),
            );
        }
    }
    let mid = rect.center().y;
    let mut right = rect.right() - 8.0;
    if item.pinned {
        let pin =
            egui::Rect::from_min_size(egui::pos2(right - 11.0, mid - 5.5), egui::vec2(11.0, 11.0));
        crate::icons::themed_image(crate::icons::Icon::Pin, 11.0, palette.faint)
            .paint_snapped(ui, pin);
        right = pin.left() - 8.0;
    }
    let badge = match item.state {
        RowState::Approve => Some(("Approve", palette.ok, palette.ok_soft)),
        RowState::Answer => Some(("Answer", palette.accent, palette.accent_soft)),
        RowState::Review => Some(("Review", palette.ok, palette.ok_soft)),
        RowState::Failed => Some(("Failed", palette.danger, palette.danger_soft)),
        _ => None,
    };
    if let Some((label, fg, bg)) = badge {
        let galley = ui.painter().layout_no_wrap(
            label.to_owned(),
            egui::FontId::new(FS_2XS, family(FAMILY_BODY_SEMIBOLD)),
            fg,
        );
        let pill = egui::Rect::from_min_size(
            egui::pos2(right - galley.size().x - 12.0, mid - 9.0),
            egui::vec2(galley.size().x + 12.0, 18.0),
        );
        ui.painter()
            .rect_filled(pill, egui::CornerRadius::same(9), bg);
        ui.painter()
            .galley(pill.center() - galley.size() / 2.0, galley, fg);
        right = pill.left() - 8.0;
    } else if item.state == RowState::Running {
        let at =
            egui::Rect::from_min_size(egui::pos2(right - 14.0, mid - 7.0), egui::vec2(14.0, 14.0));
        crate::ui::mark::working(ui, at, crate::clock::epoch());
        right = at.left() - 8.0;
    } else if item.swarm {
        let at =
            egui::Rect::from_min_size(egui::pos2(right - 12.0, mid - 6.0), egui::vec2(12.0, 12.0));
        crate::icons::themed_image(crate::icons::Icon::Waypoints, 12.0, palette.done)
            .paint_snapped(ui, at);
        right = at.left() - 8.0;
    } else if let Some(when) = ago(item.updated_at.as_deref(), now) {
        let galley = ui.painter().layout_no_wrap(
            when,
            egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
            palette.faint,
        );
        let left = right - galley.size().x.ceil();
        ui.painter().galley(
            egui::pos2(left, crate::theme::line_y(mid, 0.0, &galley)),
            galley,
            palette.faint,
        );
        right = left - 8.0;
    }
    if place.kids > 0 {
        let count = ui.painter().layout_no_wrap(
            place.kids.to_string(),
            egui::FontId::new(10.5, family(FAMILY_MONO_REGULAR)),
            palette.faint,
        );
        let left = right - count.size().x.ceil();
        ui.painter().galley(
            egui::pos2(left, crate::theme::line_y(mid, 0.0, &count)),
            count,
            palette.faint,
        );
        let icon = egui::Rect::from_min_size(
            egui::pos2(left - 2.0 - 11.0, mid - 5.5),
            egui::vec2(11.0, 11.0),
        );
        crate::icons::themed_image(crate::icons::Icon::GitBranch, 11.0, palette.faint)
            .paint_snapped(ui, icon);
        right = icon.left() - 8.0;
    }
    let face = if item.unread && !item.active {
        family(FAMILY_BODY_SEMIBOLD)
    } else {
        family(FAMILY_BODY_REGULAR)
    };
    let title_galley = crate::ui::transcript::head::one_line(
        ui,
        title,
        egui::TextFormat::simple(egui::FontId::new(FS_SM, face), fg),
        right - rect.left() - indent,
    );
    let title_y = crate::theme::line_y(mid, 0.0, &title_galley);
    ui.painter()
        .galley(egui::pos2(rect.left() + indent, title_y), title_galley, fg);
    if response.clicked() {
        events.push(SidebarEvent::Activate(item.id.clone()));
    }
}

/// The connection footer: the lamp, the link's two lines and the
/// settings control.
fn connect_footer(
    ui: &mut egui::Ui,
    palette: &Palette,
    name: &str,
    status: &str,
    dot: egui::Color32,
    halo: bool,
    events: &mut Vec<SidebarEvent>,
) {
    // The footer runs to the panel's edges, under its margins, as
    // the reference's full-bleed footer does.
    let (inner, _) = ui.allocate_at_least(
        egui::vec2(ui.available_width(), FOOTER_H - SP_4),
        egui::Sense::hover(),
    );
    let rect = egui::Rect::from_min_max(
        egui::pos2(inner.left() - SP_4, inner.top()),
        egui::pos2(inner.right() + SP_4, inner.top() + FOOTER_H),
    );
    ui.painter().hline(
        rect.x_range(),
        rect.top(),
        egui::Stroke::new(1.0, palette.line),
    );
    let mid = rect.center().y;
    let center = egui::pos2(rect.left() + 21.0, mid);
    if halo {
        ui.painter().circle_filled(center, 7.0, palette.ok_soft);
    }
    ui.painter().circle_filled(center, 4.0, dot);
    // The name over the status, one block centered under the border.
    let (name_h, status_h) = (crate::theme::line_h(FS_SM), crate::theme::line_h(11.0));
    let top = crate::theme::snap(rect.top() + 1.0 + (FOOTER_H - 1.0 - name_h - status_h) / 2.0);
    line_at(
        ui,
        rect.left() + 36.0,
        top + name_h / 2.0,
        name,
        egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
        palette.ink,
    );
    line_at(
        ui,
        rect.left() + 36.0,
        top + name_h + status_h / 2.0,
        status,
        egui::FontId::new(11.0, family(FAMILY_BODY_REGULAR)),
        palette.faint,
    );
    let gear =
        egui::Rect::from_center_size(egui::pos2(rect.right() - 24.0, mid), egui::vec2(28.0, 28.0));
    let tip = "Settings (Ctrl ,)";
    if ghost_icon(ui, gear, crate::icons::Icon::Settings, tip, palette).clicked() {
        events.push(SidebarEvent::OpenSettings);
    }
}

/// The reference's ghost icon button at `rect`: the icon in the ink,
/// with no frame until the hover fill.
fn ghost_icon(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    icon: crate::icons::Icon,
    tooltip: &str,
    palette: &Palette,
) -> egui::Response {
    let response = ui
        .interact(rect, ui.id().with(("ghost", tooltip)), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tooltip));
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(R_MD as u8),
            palette.fill_hover,
        );
    }
    crate::icons::themed_image(icon, 16.0, palette.ink).paint_snapped(
        ui,
        egui::Rect::from_center_size(rect.center(), egui::vec2(16.0, 16.0)),
    );
    response.on_hover_text(tooltip)
}

#[cfg(test)]
mod tests {
    use super::{
        RowState, ago, ago_label, days_from_civil, epoch_seconds, filter_rows, forest, placed,
        project_name, row_state, sidebar_rows, swarm_on,
    };
    use crate::clock::unix_seconds;
    use kage_client::Session;

    fn session(id: &str) -> Session {
        Session {
            id: id.to_owned(),
            opened: true,
            parent: None,
            cwd: None,
            external_agent: None,
            title: None,
            updated_at: None,
            items: Vec::new(),
            usage: Default::default(),
            mode: None,
            config_options: Vec::new(),
            commands: Vec::new(),
            mcp: Default::default(),
            permissions: Vec::new(),
            queue: Vec::new(),
            running: false,
            in_turn: false,
            compacting: false,
            agents: Default::default(),
            plan: None,
            last_stop: None,
            draft: None,
        }
    }

    #[test]
    fn civil_dates_map_to_unix_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 9, 30), 20_726);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn iso_timestamps_parse_with_zones_and_fractions() {
        assert_eq!(epoch_seconds("2026-09-30T12:34:56Z"), Some(1_790_771_696));
        assert_eq!(
            epoch_seconds("2026-09-30T12:34:56+02:00"),
            Some(1_790_771_696 - 7_200)
        );
        assert_eq!(
            epoch_seconds("2026-09-30T12:34:56.250Z"),
            Some(1_790_771_696)
        );
        assert_eq!(epoch_seconds("not a time"), None);
        assert_eq!(epoch_seconds(""), None);
    }

    #[test]
    fn relative_labels_round_half_up_like_the_web_client() {
        assert_eq!(ago_label(0), "now");
        assert_eq!(ago_label(44), "now");
        assert_eq!(ago_label(45), "1m");
        assert_eq!(ago_label(3_599), "60m");
        assert_eq!(ago_label(3_600), "1h");
        assert_eq!(ago_label(86_399), "24h");
        assert_eq!(ago_label(86_400), "1d");
        assert_eq!(ago_label(-5), "now");
    }

    #[test]
    fn ago_reports_the_gap_between_the_timestamp_and_now() {
        let now = epoch_seconds("2026-09-30T12:34:56Z").unwrap();
        assert_eq!(
            ago(Some("2026-09-30T12:34:30Z"), now).as_deref(),
            Some("now")
        );
        assert_eq!(
            ago(Some("2026-09-30T12:04:56Z"), now).as_deref(),
            Some("30m")
        );
        assert_eq!(
            ago(Some("2026-09-28T12:34:56Z"), now).as_deref(),
            Some("2d")
        );
        assert_eq!(ago(None, now), None);
    }

    #[test]
    fn project_names_take_the_last_path_segment() {
        assert_eq!(project_name(Some("/home/u/dev/kage")), "kage");
        assert_eq!(project_name(Some("/home/u/dev/kage/")), "kage");
        assert_eq!(project_name(None), "local");
        assert_eq!(project_name(Some("/")), "local");
    }

    #[test]
    fn row_states_read_the_session() {
        let mut session = session("s1");
        assert_eq!(row_state(&session), RowState::Idle);
        session.running = true;
        assert_eq!(row_state(&session), RowState::Running);
        session.running = false;
        session.last_stop = Some(kage_client::wire::StopReason::Refusal);
        assert_eq!(row_state(&session), RowState::Failed);
        session.last_stop = Some(kage_client::wire::StopReason::Cancelled);
        assert_eq!(row_state(&session), RowState::Idle);
        session.in_turn = true;
        assert_eq!(row_state(&session), RowState::Running);
    }

    #[test]
    fn swarm_mode_reads_the_swarm_option() {
        let mut session = session("s1");
        assert!(!swarm_on(&session));
        session
            .config_options
            .push(kage_client::wire::SessionConfigOption {
                id: "swarm".to_owned(),
                name: "Swarm".to_owned(),
                description: None,
                category: None,
                kind: kage_client::wire::SessionConfigKind::Select,
                current_value: "on".to_owned(),
                options: Vec::new(),
            });
        assert!(swarm_on(&session));
    }

    #[test]
    fn forks_nest_under_their_source_with_rails_past_later_siblings() {
        let ids = ["a", "b", "c", "d", "e"];
        let parents = [None, Some("a"), Some("a"), Some("b"), Some("gone")];
        let placed = forest(&parents, &ids);
        let order: Vec<(&str, usize, bool, usize)> = placed
            .iter()
            .map(|place| (ids[place.ix], place.depth, place.last, place.kids))
            .collect();
        assert_eq!(
            order,
            [
                ("a", 0, true, 2),
                ("b", 1, false, 1),
                ("d", 2, true, 0),
                ("c", 1, true, 0),
                ("e", 0, true, 0),
            ]
        );
        assert_eq!(
            placed[2].rails.get(1),
            Some(&true),
            "b's later sibling keeps a rail running past d"
        );
    }

    #[test]
    fn duplicate_ids_nest_under_the_first_occurrence() {
        let ids = ["a", "b", "a"];
        let parents = [None, Some("a"), None];
        let placed = forest(&parents, &ids);
        let order: Vec<&str> = placed.iter().map(|place| ids[place.ix]).collect();
        assert_eq!(order, ["a", "b", "a"], "every row lands exactly once");
        assert_eq!(
            (placed[1].depth, placed[1].last, placed[1].kids),
            (1, true, 0),
            "the first row carrying the id is the parent"
        );
    }

    #[test]
    fn a_parent_outside_the_list_is_a_root() {
        let ids = ["a", "b"];
        let parents = [Some("ghost"), None];
        let placed = forest(&parents, &ids);
        let order: Vec<(&str, usize)> = placed
            .iter()
            .map(|place| (ids[place.ix], place.depth))
            .collect();
        assert_eq!(order, [("a", 0), ("b", 0)], "both rows show as roots");
    }

    #[test]
    fn a_two_cycle_emits_neither_row() {
        let ids = ["a", "b"];
        let parents = [Some("b"), Some("a")];
        let placed = forest(&parents, &ids);
        assert!(placed.is_empty(), "a cycle legitimately emits neither row");
    }

    #[test]
    fn the_filter_keeps_the_rows_the_query_names() {
        let rows = vec![
            row("s1", Some("fix the flake"), "kage"),
            row("s2", Some("write the docs"), "docs"),
            row("s3", Some("tend the garden"), "docs"),
            row("s4", None, "kage"),
        ];
        assert_eq!(filter_rows(&rows, "").len(), 4, "an empty query keeps all");
        assert_eq!(filter_rows(&rows, "flake").len(), 1);
        assert_eq!(
            filter_rows(&rows, "FLAKE").len(),
            1,
            "the query is case insensitive"
        );
        assert_eq!(
            filter_rows(&rows, "docs").len(),
            2,
            "the project field is searched as well as the title"
        );
        assert_eq!(
            filter_rows(&rows, "garden").len(),
            1,
            "a row's own title matches"
        );
        assert_eq!(
            filter_rows(&rows, "untitled").len(),
            1,
            "an untitled row matches its fallback"
        );
        assert_eq!(filter_rows(&rows, "zzz").len(), 0);
    }

    /// A row with the given title and project.
    fn row(id: &str, title: Option<&str>, project: &str) -> super::RowItem {
        super::RowItem {
            id: id.to_owned(),
            title: title.map(str::to_owned),
            updated_at: None,
            state: RowState::Idle,
            active: false,
            project: project.to_owned(),
            dir: None,
            swarm: false,
            unread: false,
            parent: None,
            pinned: false,
        }
    }

    #[test]
    fn the_sidebar_rows_come_from_the_store() {
        let store = crate::store::Store::new("/w", false);
        let rows = sidebar_rows(&store);
        assert!(rows.is_empty(), "an empty store lists no rows");
        let _ = placed(&rows.iter().collect::<Vec<_>>());
    }

    #[test]
    fn the_clock_reads_unix_seconds() {
        assert!(unix_seconds() > 1_700_000_000);
    }
}
