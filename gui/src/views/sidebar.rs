//! The sidebar as the kage web client design draws it: the brand row,
//! the New session and Search controls, the session list with status
//! badges and relative times, and the connection footer.

use gpui_kit::AnyElement;
use gpui_kit::assets::IconName;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Div, Entity, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div,
    linear_color_stop, linear_gradient, px, relative,
};

use crate::app::{OpenPalette, OpenSettings, ToggleSidebar};
use crate::clock::unix_seconds;
use crate::store::{Store, StoreHandle as _};
use crate::theme::{
    FONT_DISPLAY, FONT_MONO, FS_2XS, FS_BASE, FS_SM, FS_XS, PANEL_HEAD_H, R_MD, WEIGHT_BOLD,
    WEIGHT_REGULAR, WEIGHT_SEMIBOLD,
};
use crate::transport::State;

/// The days from 1970-01-01 to a civil date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
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
fn epoch_seconds(iso: &str) -> Option<i64> {
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
fn ago_label(seconds: i64) -> SharedString {
    let s = seconds.max(0);
    if s < 45 {
        "now".into()
    } else if s < 3_600 {
        format!("{}m", (s + 30) / 60).into()
    } else if s < 86_400 {
        format!("{}h", (s + 1_800) / 3_600).into()
    } else {
        format!("{}d", (s + 43_200) / 86_400).into()
    }
}

/// The relative label for an ISO 8601 timestamp against `now`.
fn ago(iso: Option<&str>, now: i64) -> Option<SharedString> {
    let updated = epoch_seconds(iso?)?;
    Some(ago_label(now - updated))
}

/// What a session row leads or badges with.
#[derive(Clone, Copy, PartialEq, Debug)]
enum RowState {
    /// A permission ask waits; the row badges Approve.
    Approve,
    /// A plan waits for review; the row badges Review.
    Review,
    /// A turn is in flight; the row leads with the orbiting eclipse.
    Running,
    /// The last turn stopped in a refusal; the row badges Failed.
    Failed,
    /// Quiet: the row shows its relative time, or the swarm glyph in
    /// swarm mode.
    Idle,
}

/// One session row before it lands in the list: the facts the rows
/// and the project grouping both read.
struct RowItem {
    id: String,
    title: Option<String>,
    updated_at: Option<String>,
    state: RowState,
    active: bool,
    project: SharedString,
    /// Whether the session works in swarm mode.
    swarm: bool,
    /// Whether it moved since the user last looked.
    unread: bool,
    /// The session it was forked from.
    parent: Option<String>,
    /// Whether the user pinned it to the top.
    pinned: bool,
}

/// Where a row sits in the fork forest.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Place {
    /// The row's index in the list the forest was built from.
    ix: usize,
    /// How many forks deep it sits.
    depth: usize,
    /// Whether it is its parent's last fork.
    last: bool,
    /// Per depth, whether a guide rail runs past the row there because
    /// a later sibling of an ancestor follows.
    rails: Vec<bool>,
    /// How many forks it has.
    kids: usize,
}

/// `parents` as a forest: each row is followed by its forks, depth
/// first. A row whose parent is not in the list is a root.
fn forest(parents: &[Option<&str>], ids: &[&str]) -> Vec<Place> {
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
    let position = |id: &str| ids.iter().position(|known| *known == id);
    let mut kids_of = vec![Vec::new(); ids.len()];
    let mut roots = Vec::new();
    for (ix, parent) in parents.iter().enumerate() {
        match parent.and_then(position).filter(|&at| at != ix) {
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
fn placed(rows: &[&RowItem]) -> Vec<Place> {
    let parents: Vec<Option<&str>> = rows.iter().map(|row| row.parent.as_deref()).collect();
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    forest(&parents, &ids)
}

fn row_state(session: &kage_client::Session) -> RowState {
    if crate::store::plan_review(session).is_some() {
        RowState::Review
    } else if !session.permissions.is_empty() {
        RowState::Approve
    } else if session.running || session.in_turn {
        RowState::Running
    } else if session.last_stop == Some(kage_client::wire::StopReason::Refusal) {
        RowState::Failed
    } else {
        RowState::Idle
    }
}

/// Whether the session works in swarm mode.
fn swarm_on(session: &kage_client::Session) -> bool {
    session
        .config_options
        .iter()
        .any(|option| option.id == "swarm" && option.current_value == "on")
}

/// The eclipse brand mark: a gradient disc with a backdrop-colored
/// disc across its upper right, clipped to the mark's own circle.
fn eclipse_mark(size: f32, backdrop: Hsla, p: &crate::theme::Palette) -> Div {
    div()
        .relative()
        .flex_none()
        .size(px(size))
        .rounded_full()
        .overflow_hidden()
        .border_1()
        .border_color(p.line_strong)
        .bg(linear_gradient(
            135.,
            linear_color_stop(p.orb_1, 0.),
            linear_color_stop(p.orb_2, 1.),
        ))
        .child(
            div()
                .absolute()
                .top(px(size * 0.5 / 24.))
                .left(px(size * 8. / 24.))
                .size(px(size * 17. / 24.))
                .rounded_full()
                .bg(backdrop),
        )
}

/// The bordered keyboard hint chip of the design.
fn kbd_chip(label: &'static str, p: &crate::theme::Palette) -> Div {
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
        .child(label)
}

/// The small rounded status pill of the design's session badges.
fn pill(label: &'static str, fg: Hsla, bg: Hsla) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(18.))
        .px(px(6.))
        .rounded_full()
        .text_size(px(FS_2XS))
        .font_weight(WEIGHT_SEMIBOLD)
        .text_color(fg)
        .bg(bg)
        .child(label)
}

/// The connection lamp: an 8px dot, with the soft halo when linked.
fn conn_dot(color: Hsla, halo: Option<Hsla>) -> Div {
    let dot = div().size(px(8.)).rounded_full().bg(color);
    match halo {
        Some(ring) => div()
            .size(px(14.))
            .rounded_full()
            .bg(ring)
            .flex()
            .items_center()
            .justify_center()
            .child(dot),
        None => div().size(px(14.)).flex().items_center().child(dot),
    }
}

/// The left panel.
pub struct SidebarView {
    store: Entity<Store>,
    /// The projects whose groups are collapsed, keyed by name.
    collapsed: std::collections::BTreeSet<String>,
}

impl SidebarView {
    /// A sidebar following `store`.
    #[must_use]
    pub fn new(store: Entity<Store>) -> Self {
        Self {
            store,
            collapsed: std::collections::BTreeSet::new(),
        }
    }

    /// One session row: title, then the attention badge, the working
    /// eclipse or the swarm glyph, else the relative time. An unread row
    /// reads bold. Grouped rows carry the design's 32px left inset so
    /// they clear the project row's icon.
    fn session_row(
        &self,
        item: &RowItem,
        place: &Place,
        grouped: bool,
        now: i64,
        p: &crate::theme::Palette,
    ) -> AnyElement {
        let base = if grouped { 32. } else { 8. };
        let guide_color = if item.active {
            p.accent_bd
        } else {
            p.line_strong
        };
        let mut guides: Vec<Div> = Vec::new();
        for depth in 1..=place.depth {
            let left = px(base - 14. + (depth - 1) as f32 * 16.);
            let column = div().absolute().top_0().bottom_0().left(left).w(px(12.));
            if depth < place.depth {
                if place.rails.get(depth).copied().unwrap_or(false) {
                    guides.push(column.border_l_1().border_color(p.line_strong));
                }
                continue;
            }
            guides.push(
                column
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top(px(-2.))
                            .h(relative(0.5))
                            .w(px(9.))
                            .border_l_1()
                            .border_b_1()
                            .border_color(guide_color)
                            .rounded_bl(px(7.)),
                    )
                    .when(!place.last, |column| {
                        column.child(
                            div()
                                .absolute()
                                .left_0()
                                .top(relative(0.5))
                                .bottom_0()
                                .border_l_1()
                                .border_color(p.line_strong),
                        )
                    }),
            );
        }
        let forks = (place.kids > 0).then(|| {
            h_flex()
                .flex_none()
                .items_center()
                .gap(px(2.))
                .font_family(FONT_MONO)
                .text_size(px(10.5))
                .text_color(p.faint)
                .child(Icon::new(IconName::GitBranch).with_size(px(11.)))
                .child(SharedString::from(place.kids.to_string()))
        });
        let store = self.store.clone();
        let id_owned = item.id.clone();
        let active = item.active;
        let fg = if active { p.ink_strong } else { p.muted };
        let unread = item.unread && !active;
        let trailing = match item.state {
            RowState::Approve => Some(pill("Approve", p.ok, p.ok_soft).into_any_element()),
            RowState::Review => Some(pill("Review", p.ok, p.ok_soft).into_any_element()),
            RowState::Failed => Some(pill("Failed", p.danger, p.danger_soft).into_any_element()),
            RowState::Running => Some(
                crate::views::eclipse::eclipse(14., Some(crate::clock::epoch()), p)
                    .into_any_element(),
            ),
            RowState::Idle if item.swarm => Some(
                Icon::new(IconName::Waypoints)
                    .with_size(px(12.))
                    .text_color(p.done)
                    .into_any_element(),
            ),
            RowState::Idle => ago(item.updated_at.as_deref(), now).map(|when| {
                div()
                    .flex_none()
                    .text_size(px(FS_XS))
                    .text_color(p.faint)
                    .child(when)
                    .into_any_element()
            }),
        };
        div()
            .id(SharedString::from(format!("session-{}", item.id)))
            .w_full()
            .min_h(px(32.))
            .relative()
            .px(px(8.))
            .pl(px(base + place.depth as f32 * 16.))
            .py(px(5.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(px(R_MD))
            .text_size(px(FS_SM))
            .text_color(fg)
            .when(active, |row| row.bg(p.selected))
            .hover(move |row| {
                if active {
                    row.bg(p.selected_hover)
                } else {
                    row.bg(p.hover).text_color(p.ink)
                }
            })
            .on_click(move |_, _, cx| {
                store.update(cx, |store, cx| {
                    store.set_active(id_owned.clone());
                    cx.notify();
                });
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(unread, |title| {
                        title.text_color(p.ink_strong).font_weight(WEIGHT_SEMIBOLD)
                    })
                    .child(SharedString::from(
                        item.title
                            .clone()
                            .unwrap_or_else(|| "untitled session".to_owned()),
                    )),
            )
            .children(guides)
            .children(forks)
            .children(trailing)
            .when(item.pinned, |row| {
                row.child(
                    Icon::new(IconName::Pin)
                        .with_size(px(11.))
                        .text_color(p.faint),
                )
            })
            .into_any_element()
    }

    /// A full-width nav row with its leading icon and keyboard chip.
    fn nav_button(
        &self,
        id: &'static str,
        icon: IconName,
        label: &'static str,
        keys: &'static str,
        p: &crate::theme::Palette,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .w_full()
            .h(px(34.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .rounded(px(R_MD))
            .text_size(px(FS_BASE))
            .text_color(p.ink)
            .hover(move |row| row.bg(p.hover))
            .child(Icon::new(icon))
            .child(div().flex_1().child(label))
            .child(kbd_chip(keys, p))
    }
}

impl Render for SidebarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::active(cx);
        let store = self.store.read(cx);
        let state = store.state();
        let now = unix_seconds();
        let this = cx.entity();

        // Every row the list carries: live sessions first, then the
        // directory entries no live session holds, each tagged with
        // the project its working directory names.
        let mut items: Vec<RowItem> = Vec::new();
        // A child agent's session is reached through its parent's card
        // and the Agents tab, never listed on its own.
        for (id, session) in state.sessions.iter().filter(|(_, s)| s.parent.is_none()) {
            let active = store.active_id() == Some(id.as_str());
            items.push(RowItem {
                id: id.clone(),
                title: session.title.clone(),
                updated_at: session.updated_at.clone(),
                state: row_state(session),
                active,
                project: crate::app::project_name(session.cwd.as_deref()),
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
                project: crate::app::project_name(Some(&info.cwd)),
                swarm: false,
                unread: false,
                parent: store.fork_parent(&info.session_id).map(str::to_owned),
                pinned: store.prefs().pinned.contains(&info.session_id),
            });
        }
        // Archived sessions leave the list until restored.
        items.retain(|item| !store.prefs().archived.contains(&item.id));
        // Pinned first, then newest first; a live session with no time
        // yet is the newest.
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

        // The list body: groups under project rows, or one flat list.
        let mut list = v_flex()
            .id("side-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(8.))
            .pb(px(12.));
        if store.prefs().group_by_project {
            let mut groups: Vec<(SharedString, Vec<&RowItem>)> = Vec::new();
            for item in &items {
                if let Some(group) = groups.iter_mut().find(|(name, _)| name == &item.project) {
                    group.1.push(item);
                } else {
                    groups.push((item.project.clone(), vec![item]));
                }
            }
            for (project, members) in groups {
                let open = !self.collapsed.contains(project.as_ref());
                let this = this.clone();
                let store = self.store.clone();
                let key = project.to_string();
                list = list.child(
                    h_flex()
                        .id(SharedString::from(format!("proj-{project}")))
                        .w_full()
                        .h(px(32.))
                        .px(px(8.))
                        .mt(px(4.))
                        .items_center()
                        .gap(px(8.))
                        .rounded(px(R_MD))
                        .text_size(px(FS_SM))
                        .text_color(p.muted)
                        .hover(move |row| row.bg(p.hover).text_color(p.ink))
                        .on_click(move |_, _, cx| {
                            this.update(cx, |this, cx| {
                                if this.collapsed.contains(&key) {
                                    this.collapsed.remove(&key);
                                } else {
                                    this.collapsed.insert(key.clone());
                                }
                                cx.notify();
                            });
                        })
                        .child(
                            Icon::new(if open {
                                IconName::FolderOpen
                            } else {
                                IconName::Folder
                            })
                            .with_size(px(14.))
                            .text_color(p.muted),
                        )
                        .child(div().min_w_0().flex_1().truncate().child(project.clone()))
                        .when(!members.is_empty(), |row| {
                            row.child(
                                div()
                                    .flex_none()
                                    .text_size(px(FS_XS))
                                    .text_color(p.faint)
                                    .child(SharedString::from(members.len().to_string())),
                            )
                        })
                        .child({
                            let store = store.clone();
                            div()
                                .id(SharedString::from(format!("proj-add-{project}")))
                                .size(px(22.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(R_MD))
                                .text_color(p.faint)
                                .hover(move |row| row.bg(p.fill_hover).text_color(p.ink))
                                .tooltip(|window, cx| Tooltip::new("New session").build(window, cx))
                                .child(Icon::new(IconName::Plus).with_size(px(13.)))
                                .on_click(move |_, _, cx| {
                                    store.update(cx, |store, cx| {
                                        store.show_welcome();
                                        cx.notify();
                                    });
                                })
                        }),
                );
                if open {
                    if members.is_empty() {
                        list = list.child(
                            div()
                                .pt(px(2.))
                                .pl(px(32.))
                                .pr(px(8.))
                                .pb(px(6.))
                                .text_size(px(FS_SM))
                                .text_color(p.faint)
                                .child("No conversations yet"),
                        );
                    } else {
                        for place in placed(&members) {
                            list = list.child(self.session_row(
                                members[place.ix],
                                &place,
                                true,
                                now,
                                p,
                            ));
                        }
                    }
                }
            }
        } else {
            let rows: Vec<&RowItem> = items.iter().collect();
            for place in placed(&rows) {
                list = list.child(self.session_row(rows[place.ix], &place, false, now, p));
            }
        }

        let version = state.agent.as_ref().and_then(|agent| agent.version.clone());

        let link = store.link();
        let link_name = link.name;
        let status_line = match store.connect() {
            State::Connected => link.detail.clone(),
            State::Connecting => "connecting\u{2026}".to_owned(),
            State::Reconnecting { .. } => "reconnecting\u{2026}".to_owned(),
            State::Refused(_) | State::Closed => "not running".to_owned(),
        };

        let dot = match store.connect() {
            State::Connected => conn_dot(p.ok, Some(p.ok_soft)),
            State::Connecting | State::Reconnecting { .. } => conn_dot(p.warn, None),
            State::Refused(_) => conn_dot(p.danger, None),
            State::Closed => conn_dot(p.faint, None),
        };

        let connect_label = SharedString::from(status_line);
        let new_session = self.store.clone();

        v_flex()
            .id("sidebar")
            .test_support()
            .size_full()
            .overflow_hidden()
            .bg(p.sidebar)
            .text_color(p.ink)
            .child(
                h_flex()
                    .flex_none()
                    .h(px(PANEL_HEAD_H))
                    .items_center()
                    .gap(px(10.))
                    .pl(px(14.))
                    .pr(px(10.))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(9.))
                            .child(eclipse_mark(22., p.sidebar, p))
                            .child(
                                div()
                                    .font_family(FONT_DISPLAY)
                                    .font_weight(WEIGHT_BOLD)
                                    .text_size(px(15.))
                                    .text_color(p.ink_strong)
                                    .child("kage"),
                            )
                            .children(version.map(|version| {
                                div()
                                    .font_family(FONT_MONO)
                                    .font_weight(WEIGHT_REGULAR)
                                    .text_size(px(FS_XS))
                                    .text_color(p.faint)
                                    .child(version)
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("collapse-sidebar")
                            .icon(IconName::PanelLeft)
                            .xsmall()
                            .ghost()
                            .tooltip("Collapse sidebar (Ctrl \\)")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleSidebar), cx);
                            }),
                    ),
            )
            .child(
                v_flex()
                    .flex_none()
                    .px(px(8.))
                    .pt(px(2.))
                    .pb(px(8.))
                    .gap(px(1.))
                    .child(
                        self.nav_button(
                            "nav-new-session",
                            IconName::SquarePen,
                            "New session",
                            "Ctrl N",
                            p,
                        )
                        .on_click(move |_, _, cx| {
                            new_session.update(cx, |store, cx| {
                                store.show_welcome();
                                cx.notify();
                            });
                        }),
                    )
                    .child(
                        self.nav_button("nav-search", IconName::Search, "Search", "Ctrl K", p)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenPalette), cx);
                            }),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(px(4.))
                    .pl(px(16.))
                    .pr(px(8.))
                    .pt(px(10.))
                    .pb(px(4.))
                    .text_size(px(FS_XS))
                    .font_weight(WEIGHT_SEMIBOLD)
                    .text_color(p.faint)
                    .child(div().flex_1().child("SESSIONS"))
                    .child({
                        let store = self.store.clone();
                        let grouped = self.store.read(cx).prefs().group_by_project;
                        div()
                            .id("side-group-toggle")
                            .size(px(20.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(R_MD))
                            .text_color(p.faint)
                            .tooltip(move |window, cx| {
                                Tooltip::new(if grouped {
                                    "Show as one list"
                                } else {
                                    "Group by project"
                                })
                                .build(window, cx)
                            })
                            .child(
                                Icon::new(if grouped {
                                    IconName::List
                                } else {
                                    IconName::Folder
                                })
                                .with_size(px(13.)),
                            )
                            .on_click(move |_, _, cx| {
                                store.act(cx, |store| {
                                    store.update_prefs(|prefs| {
                                        prefs.group_by_project = !prefs.group_by_project;
                                    });
                                });
                            })
                    }),
            )
            .child(list)
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .h(px(52.))
                    .pl(px(14.))
                    .pr(px(10.))
                    .border_t_1()
                    .border_color(p.line)
                    .text_size(px(FS_SM))
                    .child(dot)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_color(p.ink).truncate().child(link_name))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(p.faint)
                                    .truncate()
                                    .child(connect_label),
                            ),
                    )
                    .child(
                        Button::new("open-settings")
                            .icon(IconName::Settings)
                            .xsmall()
                            .ghost()
                            .tooltip("Settings (Ctrl ,)")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenSettings), cx);
                            }),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{RowState, ago, ago_label, days_from_civil, epoch_seconds, forest, row_state};
    use crate::clock::unix_seconds;
    use kage_client::Session;

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
    fn row_states_read_the_session() {
        use kage_client::wire::StopReason;
        let mut session = Session::new("s1");
        assert_eq!(row_state(&session), RowState::Idle);
        session.running = true;
        assert_eq!(row_state(&session), RowState::Running);
        session.running = false;
        session.last_stop = Some(StopReason::Refusal);
        assert_eq!(row_state(&session), RowState::Failed);
        session.last_stop = Some(StopReason::Cancelled);
        assert_eq!(row_state(&session), RowState::Idle);
        session.in_turn = true;
        assert_eq!(row_state(&session), RowState::Running);
    }

    #[test]
    fn the_clock_reads_unix_seconds() {
        assert!(unix_seconds() > 1_700_000_000);
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
}
