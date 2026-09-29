//! The workbench: what the run is doing right now.
//!
//! Usage, config options, the plan, the subagent tree and the
//! permission asks of the active session, read straight from store
//! state. Layout and polish belong to later surface epics; the
//! sections are placeholders with real data.

use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Div, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px, relative,
};

use crate::store::Store;

/// The right panel.
pub struct WorkbenchView {
    store: Entity<Store>,
}

impl WorkbenchView {
    /// A workbench following `store`.
    #[must_use]
    pub fn new(store: Entity<Store>) -> Self {
        Self { store }
    }

    fn heading(&self, text: &str, cx: &Context<Self>) -> Div {
        div()
            .pt_3()
            .pb_1()
            .text_size(px(11.))
            .text_color(cx.theme().colors.muted_foreground)
            .child(SharedString::from(text.to_owned()))
    }

    fn line(&self, text: impl Into<String>, cx: &Context<Self>) -> Div {
        div()
            .text_size(px(12.))
            .text_color(cx.theme().colors.foreground)
            .child(SharedString::from(text.into()))
    }

    fn usage_section(&self, cx: &Context<Self>) -> Div {
        let theme = cx.theme().colors;
        let Some(session) = self.store.read(cx).active_session() else {
            return v_flex().child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child("no session"),
            );
        };
        let usage = &session.usage;
        let percent = (usage.fill() * 100.0).round() as u64;
        v_flex()
            .gap_1()
            .child(self.line(
                format!("{} of {} tokens, {}%", usage.used, usage.size, percent),
                cx,
            ))
            .child(
                h_flex()
                    .w_full()
                    .h(px(4.))
                    .rounded(px(2.))
                    .bg(theme.muted)
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .bg(theme.primary)
                            .w(relative(percent.min(100) as f32 / 100.0)),
                    ),
            )
            .when_some(usage.cost.as_ref(), |this, cost| {
                this.child(self.line(
                    format!("cost so far {} {:.4}", cost.currency, cost.amount),
                    cx,
                ))
            })
    }

    fn config_section(&self, cx: &Context<Self>) -> Div {
        let theme = cx.theme().colors;
        let Some(session) = self.store.read(cx).active_session() else {
            return div();
        };
        let mut rows = v_flex().gap_0();
        for option in &session.config_options {
            rows = rows.child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(option.name.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.foreground)
                            .child(SharedString::from(option.current_value.clone())),
                    ),
            );
        }
        rows
    }

    fn plan_section(&self, cx: &Context<Self>) -> Div {
        let theme = cx.theme().colors;
        let Some(session) = self.store.read(cx).active_session() else {
            return div();
        };
        let Some(entries) = session.plan.as_ref() else {
            return div();
        };
        let mut rows = v_flex().gap_0();
        for entry in entries {
            let status = entry
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let mark = match status {
                "completed" => "x",
                "in_progress" => "~",
                _ => " ",
            };
            let text = entry
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            rows = rows.child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(12.)).text_color(theme.primary).child(mark))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.foreground)
                            .child(SharedString::from(text.to_owned())),
                    ),
            );
        }
        rows
    }

    fn agents_section(&self, cx: &Context<Self>) -> Div {
        let theme = cx.theme().colors;
        let Some(session) = self.store.read(cx).active_session() else {
            return div();
        };
        let mut rows = v_flex().gap_0();
        for agent in session.agents.values() {
            let name = agent.name.clone().unwrap_or_else(|| "agent".to_owned());
            let state = match agent.state {
                Some(kage_client::wire::SubagentState::Completed) => "completed",
                Some(kage_client::wire::SubagentState::Failed) => "failed",
                Some(_) => "ended",
                None => "running",
            };
            rows = rows.child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.foreground)
                            .child(SharedString::from(name)),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(state),
                    ),
            );
        }
        rows
    }

    fn permissions_section(&self, cx: &Context<Self>) -> Div {
        let theme = cx.theme().colors;
        let Some(session) = self.store.read(cx).active_session() else {
            return div();
        };
        if session.permissions.is_empty() {
            return div()
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .child("none waiting");
        }
        let mut rows = v_flex().gap_1();
        for ask in &session.permissions {
            let title = ask.tool_call.title.clone();
            rows = rows.child(
                h_flex()
                    .gap_2()
                    .child(div().text_size(px(12.)).text_color(theme.warning).child(
                        SharedString::from(title.unwrap_or_else(|| "a tool call".to_owned())),
                    ))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child("asks your verdict"),
                    ),
            );
        }
        rows
    }
}

impl Render for WorkbenchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        v_flex()
            .id("workbench")
            .size_full()
            .overflow_y_scroll()
            .px_3()
            .pb_3()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.heading("context usage", cx))
            .child(self.usage_section(cx))
            .child(self.heading("config", cx))
            .child(self.config_section(cx))
            .child(self.heading("plan", cx))
            .child(self.plan_section(cx))
            .child(self.heading("agents", cx))
            .child(self.agents_section(cx))
            .child(self.heading("permissions", cx))
            .child(self.permissions_section(cx))
    }
}
