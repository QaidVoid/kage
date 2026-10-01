//! The sidebar as the kage web client design draws it: the brand row,
//! the New session and Search controls, the session list with status
//! badges and relative times, and the connection footer.

use gpui_kit::AnyElement;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, Div, Entity, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div,
    linear_color_stop, linear_gradient, px,
};

use crate::app::{OpenPalette, ToggleSidebar};
use crate::clock::unix_seconds;
use crate::store::Store;
use crate::theme::{
    FONT_DISPLAY, FONT_MONO, FS_2XS, FS_SM, FS_XS, PANEL_HEAD_H, R_MD, WEIGHT_BOLD, WEIGHT_REGULAR,
    WEIGHT_SEMIBOLD,
};
use crate::transport::State;

/// The design palette of the active theme: the shell installs the
/// dark kage palette at startup, and the light dawn palette when it
/// installs the light mode instead.
fn design_palette(cx: &App) -> crate::theme::Palette {
    if cx.theme().mode.is_dark() {
        crate::theme::Palette::shadow()
    } else {
        crate::theme::Palette::dawn()
    }
}

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
    /// A turn is in flight; the row leads with the spinner.
    Running,
    /// Permission asks wait; the row badges for review.
    Review,
    /// The last turn stopped in a refusal; the row badges failed.
    Failed,
    /// The session works in plan mode.
    Plan,
    /// Quiet: the row shows its relative time.
    Idle,
}

fn row_state(session: &kage_client::Session) -> RowState {
    if session.running || session.in_turn {
        RowState::Running
    } else if !session.permissions.is_empty() {
        RowState::Review
    } else if session.last_stop == Some(kage_client::wire::StopReason::Refusal) {
        RowState::Failed
    } else if session.mode.as_deref() == Some("plan") {
        RowState::Plan
    } else {
        RowState::Idle
    }
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
}

impl SidebarView {
    /// A sidebar following `store`.
    #[must_use]
    pub fn new(store: Entity<Store>) -> Self {
        Self { store }
    }

    /// One session row: title, relative time, and the lead or badge
    /// the row state carries.
    #[allow(clippy::too_many_arguments)]
    fn session_row(
        &self,
        id: &str,
        title: Option<&str>,
        updated_at: Option<&str>,
        state: RowState,
        active: bool,
        now: i64,
        p: &crate::theme::Palette,
    ) -> AnyElement {
        let store = self.store.clone();
        let id_owned = id.to_owned();
        let fg = if active { p.ink_strong } else { p.muted };
        div()
            .id(SharedString::from(format!("session-{id}")))
            .w_full()
            .min_h(px(32.))
            .px(px(8.))
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
                    .child(SharedString::from(title.unwrap_or("untitled session"))),
            )
            .children(match state {
                RowState::Running => Some(
                    Spinner::new()
                        .icon(Icon::new(IconName::LoaderCircle))
                        .color(p.accent)
                        .with_size(px(14.))
                        .into_any_element(),
                ),
                RowState::Review => Some(pill("Review", p.ok, p.ok_soft).into_any_element()),
                RowState::Failed => {
                    Some(pill("Failed", p.danger, p.danger_soft).into_any_element())
                }
                RowState::Plan => Some(pill("Plan", p.accent, p.accent_soft).into_any_element()),
                RowState::Idle => ago(updated_at, now).map(|when| {
                    div()
                        .flex_none()
                        .text_size(px(FS_XS))
                        .text_color(p.faint)
                        .child(when)
                        .into_any_element()
                }),
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
            .text_size(px(FS_SM))
            .text_color(p.ink)
            .hover(move |row| row.bg(p.hover))
            .child(Icon::new(icon))
            .child(div().flex_1().child(label))
            .child(kbd_chip(keys, p))
    }
}

impl Render for SidebarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = design_palette(cx);
        let store = self.store.read(cx);
        let state = store.state();
        let now = unix_seconds();

        let mut rows: Vec<AnyElement> = Vec::new();
        for (id, session) in &state.sessions {
            let active = store.active_id() == Some(id.as_str());
            rows.push(self.session_row(
                id,
                session.title.as_deref(),
                session.updated_at.as_deref(),
                row_state(session),
                active,
                now,
                &p,
            ));
        }
        for info in &state.directory {
            if state.sessions.contains_key(&info.session_id) {
                continue;
            }
            rows.push(self.session_row(
                &info.session_id,
                info.title.as_deref(),
                info.updated_at.as_deref(),
                RowState::Idle,
                false,
                now,
                &p,
            ));
        }

        let version = state.agent.as_ref().and_then(|agent| agent.version.clone());

        let status_line = match store.connect() {
            State::Connected => "stdio \u{b7} local",
            State::Connecting => "connecting...",
            State::Reconnecting { .. } => "reconnecting...",
            State::Refused(_) | State::Closed => "not running",
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
                            .child(eclipse_mark(22., p.sidebar, &p))
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
                            &p,
                        )
                        .on_click(move |_, _, cx| {
                            new_session.update(cx, |store, cx| {
                                store.new_session();
                                cx.notify();
                            });
                        }),
                    )
                    .child(
                        self.nav_button("nav-search", IconName::Search, "Search", "Ctrl K", &p)
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
                    .child(div().flex_1().child("SESSIONS")),
            )
            .child(
                v_flex()
                    .id("side-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(8.))
                    .pb(px(12.))
                    .children(rows),
            )
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
                            .child(div().text_color(p.ink).truncate().child("kage rpc"))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(p.faint)
                                    .truncate()
                                    .child(connect_label),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{RowState, ago, ago_label, days_from_civil, epoch_seconds, row_state};
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
        session.mode = Some("plan".to_owned());
        assert_eq!(row_state(&session), RowState::Plan);
        session.mode = None;
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
}
