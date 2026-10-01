//! The workbench: the right panel as the kage web client design
//! draws it. A 48px head of tabs over the active session's context
//! usage, config options, plan, subagents and permission asks, read
//! straight from store state.

use gpui_kit::AnyElement;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, Div, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, px,
    relative,
};

use crate::app::ToggleWorkbench;
use crate::store::Store;
use crate::theme::{FONT_MONO, FS_2XS, FS_SM, FS_XS, PANEL_HEAD_H, R_FULL, R_MD, WEIGHT_SEMIBOLD};

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

/// The panes the workbench tabs switch between.
#[derive(Clone, Copy, Default, PartialEq)]
enum Tab {
    /// Context fill and cost.
    #[default]
    Usage,
    /// The config options the session advertises.
    Config,
    /// The plan entries and their status.
    Plan,
    /// The announced subagents.
    Agents,
    /// The permission asks waiting for a verdict.
    Permissions,
}

impl Tab {
    /// The tabs in head order.
    fn all() -> [Tab; 5] {
        [
            Tab::Usage,
            Tab::Config,
            Tab::Plan,
            Tab::Agents,
            Tab::Permissions,
        ]
    }

    /// The label the tab carries.
    fn label(self) -> &'static str {
        match self {
            Tab::Usage => "Usage",
            Tab::Config => "Config",
            Tab::Plan => "Plan",
            Tab::Agents => "Agents",
            Tab::Permissions => "Permissions",
        }
    }

    /// The icon the tab leads with.
    fn icon(self) -> IconName {
        match self {
            Tab::Usage => IconName::Gauge,
            Tab::Config => IconName::SlidersHorizontal,
            Tab::Plan => IconName::SquarePen,
            Tab::Agents => IconName::Users,
            Tab::Permissions => IconName::ShieldQuestionMark,
        }
    }
}

/// The uppercase section label of the design's workbench panes.
fn section_head(text: &'static str, p: &crate::theme::Palette) -> Div {
    div()
        .flex()
        .items_center()
        .px(px(14.))
        .pt(px(12.))
        .pb(px(4.))
        .text_size(px(FS_XS))
        .font_weight(WEIGHT_SEMIBOLD)
        .text_color(p.faint)
        .child(text)
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
    tab: Tab,
}

impl WorkbenchView {
    /// A workbench following `store`.
    #[must_use]
    pub fn new(store: Entity<Store>) -> Self {
        Self {
            store,
            tab: Tab::default(),
        }
    }

    /// A tab styled as the design's workbench tabs.
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
            .child(tab.label())
            .children(count.map(|count| tab_count(&count, p)))
    }

    /// The usage pane: fill bar, token counts and the fill chip.
    fn usage_section(&self, cx: &Context<Self>, p: &crate::theme::Palette) -> Vec<AnyElement> {
        let Some(session) = self.store.read(cx).active_session() else {
            return Vec::new();
        };
        let usage = &session.usage;
        let percent = (usage.fill() * 100.0).round() as u64;
        let mut out = vec![
            section_head("CONTEXT", p).into_any_element(),
            h_flex()
                .items_center()
                .justify_between()
                .px(px(14.))
                .py(px(4.))
                .child(
                    div()
                        .text_size(px(FS_XS))
                        .font_family(FONT_MONO)
                        .text_color(p.muted)
                        .child(SharedString::from(format!(
                            "{} / {}",
                            usage.used, usage.size
                        ))),
                )
                .child(
                    div()
                        .flex_none()
                        .h(px(20.))
                        .px(px(7.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .text_size(px(FS_2XS))
                        .font_family(FONT_MONO)
                        .when(percent >= 80, |chip| {
                            chip.text_color(p.warn).bg(p.warn_soft)
                        })
                        .when(percent < 80, |chip| chip.text_color(p.muted).bg(p.fill))
                        .child(SharedString::from(format!("{percent}%"))),
                )
                .into_any_element(),
            div()
                .mx(px(14.))
                .h(px(3.))
                .rounded(px(R_FULL))
                .bg(p.fill)
                .overflow_hidden()
                .child(
                    div()
                        .h_full()
                        .bg(p.accent)
                        .w(relative((percent.min(100) as f32) / 100.0)),
                )
                .into_any_element(),
        ];
        if let Some(cost) = usage.cost.as_ref() {
            out.push(
                div()
                    .px(px(14.))
                    .py(px(4.))
                    .text_size(px(FS_XS))
                    .font_family(FONT_MONO)
                    .text_color(p.faint)
                    .child(SharedString::from(format!(
                        "cost so far {} {:.4}",
                        cost.currency, cost.amount
                    )))
                    .into_any_element(),
            );
        }
        out
    }

    /// The config pane: one row per advertised option.
    fn config_section(&self, cx: &Context<Self>, p: &crate::theme::Palette) -> Vec<AnyElement> {
        let Some(session) = self.store.read(cx).active_session() else {
            return Vec::new();
        };
        let mut out = vec![section_head("CONFIG", p).into_any_element()];
        for option in &session.config_options {
            out.push(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(5.))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(FS_SM))
                            .text_color(p.muted)
                            .child(SharedString::from(option.name.clone())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(FS_SM))
                            .font_family(FONT_MONO)
                            .text_color(p.ink)
                            .child(SharedString::from(option.current_value.clone())),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    /// The plan pane: entries with their status marks.
    fn plan_section(&self, cx: &Context<Self>, p: &crate::theme::Palette) -> Vec<AnyElement> {
        let Some(session) = self.store.read(cx).active_session() else {
            return Vec::new();
        };
        let Some(entries) = session.plan.as_ref() else {
            return vec![section_head("PLAN", p).into_any_element()];
        };
        let mut out = vec![section_head("PLAN", p).into_any_element()];
        for entry in entries {
            let status = entry
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let text = entry
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let (mark, mark_color, text_color) = match status {
                "completed" => ("x", p.ok, p.muted),
                "in_progress" => ("~", p.accent, p.ink_strong),
                _ => ("", p.faint, p.ink),
            };
            let mut text_row = div().min_w_0().flex_1().text_size(px(FS_SM));
            if status == "completed" {
                text_row = text_row.line_through();
            }
            out.push(
                h_flex()
                    .items_start()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(2.))
                    .child(
                        div()
                            .w(px(14.))
                            .flex_none()
                            .text_size(px(FS_SM))
                            .text_color(mark_color)
                            .child(mark),
                    )
                    .child(
                        text_row
                            .text_color(text_color)
                            .child(SharedString::from(text.to_owned())),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    /// The agents pane: announced subagents and their states.
    fn agents_section(&self, cx: &Context<Self>, p: &crate::theme::Palette) -> Vec<AnyElement> {
        let Some(session) = self.store.read(cx).active_session() else {
            return Vec::new();
        };
        if session.agents.is_empty() {
            return vec![
                section_head("AGENTS", p).into_any_element(),
                div()
                    .px(px(14.))
                    .py(px(6.))
                    .text_size(px(FS_SM))
                    .text_color(p.faint)
                    .child(
                        "No agents yet. Subagents and swarm workers show up here while they run.",
                    )
                    .into_any_element(),
            ];
        }
        let mut out = vec![section_head("AGENTS", p).into_any_element()];
        for agent in session.agents.values() {
            let name = agent.name.clone().unwrap_or_else(|| "agent".to_owned());
            let (state, color) = match agent.state {
                None => ("running", p.accent),
                Some(kage_client::wire::SubagentState::Completed) => ("completed", p.ok),
                Some(kage_client::wire::SubagentState::Failed) => ("failed", p.danger),
                Some(_) => ("ended", p.faint),
            };
            out.push(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(8.))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(FS_SM))
                            .text_color(p.ink)
                            .child(SharedString::from(name)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(FS_XS))
                            .text_color(color)
                            .child(state),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    /// The permissions pane: the asks waiting for a verdict.
    fn permissions_section(
        &self,
        cx: &Context<Self>,
        p: &crate::theme::Palette,
    ) -> Vec<AnyElement> {
        let Some(session) = self.store.read(cx).active_session() else {
            return Vec::new();
        };
        if session.permissions.is_empty() {
            return vec![
                section_head("PERMISSIONS", p).into_any_element(),
                div()
                    .px(px(14.))
                    .py(px(6.))
                    .text_size(px(FS_SM))
                    .text_color(p.faint)
                    .child("none waiting")
                    .into_any_element(),
            ];
        }
        let mut out = vec![section_head("PERMISSIONS", p).into_any_element()];
        for ask in &session.permissions {
            out.push(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(5.))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(FS_SM))
                            .text_color(p.warn)
                            .child(SharedString::from(
                                ask.tool_call
                                    .title
                                    .clone()
                                    .unwrap_or_else(|| "a tool call".to_owned()),
                            )),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(FS_XS))
                            .text_color(p.muted)
                            .child("asks your verdict"),
                    )
                    .into_any_element(),
            );
        }
        out
    }
}

impl Render for WorkbenchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = design_palette(cx);
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
                    Tab::Usage | Tab::Config => None,
                    Tab::Plan => session.plan.as_ref().map(|plan| plan.len().to_string()),
                    Tab::Agents => {
                        let total = session.agents.len();
                        let live = session
                            .agents
                            .values()
                            .filter(|agent| agent.state.is_none())
                            .count();
                        if live > 0 {
                            Some(format!("{live} live"))
                        } else if total > 0 {
                            Some(total.to_string())
                        } else {
                            None
                        }
                    }
                    Tab::Permissions => session
                        .permissions
                        .is_empty()
                        .then(|| session.permissions.len().to_string()),
                });
            head = head.child(self.tab_button(tab, self.tab == tab, count, &p).on_click(
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
                .children(match self.tab {
                    Tab::Usage => self.usage_section(cx, &p),
                    Tab::Config => self.config_section(cx, &p),
                    Tab::Plan => self.plan_section(cx, &p),
                    Tab::Agents => self.agents_section(cx, &p),
                    Tab::Permissions => self.permissions_section(cx, &p),
                })
        } else {
            v_flex().id("wb-body").child(
                empty_block(
                    IconName::PanelRight,
                    "Start a session to see its changes, agents and output here.",
                    &p,
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
