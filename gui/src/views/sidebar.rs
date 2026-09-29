//! The sidebar: connection, agent identity, and the sessions to
//! follow.

use gpui_kit::AnyElement;
use gpui_kit::component::button::Button;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Div, Entity, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};

use crate::store::Store;
use crate::transport::State;

/// A short id for a list row: the tail is what eyes tell apart.
fn short(id: &str) -> &str {
    let take = 8;
    if id.len() <= take {
        id
    } else {
        &id[id.len() - take..]
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

    fn connect_color(&self, cx: &Context<Self>) -> Hsla {
        let theme = cx.theme().colors;
        match self.store.read(cx).connect() {
            State::Connected => theme.success,
            State::Connecting | State::Reconnecting { .. } => theme.warning,
            State::Refused(_) => theme.danger,
            State::Closed => theme.muted_foreground,
        }
    }

    fn session_row(
        &self,
        id: &str,
        title: Option<&str>,
        active: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().colors;
        let store = self.store.clone();
        let id_owned = id.to_owned();
        let label = title.unwrap_or("untitled session").to_owned();
        div()
            .id(SharedString::from(format!("session-{id}")))
            .w_full()
            .px_3()
            .py_1()
            .flex()
            .items_center()
            .justify_between()
            .rounded(px(4.))
            .map(|row| {
                if active {
                    row.bg(theme.list_active)
                } else {
                    row
                }
            })
            .hover(|row| row.bg(theme.list_hover))
            .on_click(move |_, _, cx| {
                store.update(cx, |store, cx| {
                    store.set_active(id_owned.clone());
                    cx.notify();
                });
            })
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(if active {
                        theme.foreground
                    } else {
                        theme.sidebar_foreground
                    })
                    .child(SharedString::from(label)),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(short(id).to_owned())),
            )
            .into_any_element()
    }

    fn section_label(&self, text: &str, cx: &Context<Self>) -> Div {
        div()
            .px_3()
            .pt_3()
            .pb_1()
            .text_size(px(11.))
            .text_color(cx.theme().colors.muted_foreground)
            .child(SharedString::from(text.to_owned()))
    }
}

impl Render for SidebarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let theme = cx.theme().colors;
        let state = store.state();
        let connect_label = SharedString::from(store.connect().label());
        let connect_color = self.connect_color(cx);

        let agent_line = match state.agent.as_ref() {
            Some(agent) => SharedString::from(format!(
                "{} {}",
                agent.name,
                agent.version.as_deref().unwrap_or("?")
            )),
            None => SharedString::from("no agent yet"),
        };

        let mut rows: Vec<AnyElement> = Vec::new();
        for (id, session) in &state.sessions {
            let active = store.active_id() == Some(id.as_str());
            rows.push(self.session_row(id, session.title.as_deref(), active, cx));
        }
        for info in &state.directory {
            if state.sessions.contains_key(&info.session_id) {
                continue;
            }
            rows.push(self.session_row(&info.session_id, info.title.as_deref(), false, cx));
        }

        let new_session = self.store.clone();
        let section = if state.sessions.is_empty() {
            "sessions"
        } else {
            "open sessions"
        };
        let recorded = state.directory.len();
        v_flex()
            .id("sidebar")
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .overflow_y_scroll()
            .child(
                h_flex()
                    .px_3()
                    .pt_3()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child("kage"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child(agent_line),
                    ),
            )
            .child(
                h_flex()
                    .px_3()
                    .pt_1()
                    .gap_2()
                    .items_center()
                    .child(div().size(px(8.)).rounded_full().bg(connect_color))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child(connect_label),
                    ),
            )
            .child(
                div().px_3().pt_2().child(
                    Button::new("new-session")
                        .label("New session (ctrl-n)")
                        .on_click(move |_, _, cx| {
                            new_session.update(cx, |store, cx| {
                                store.new_session();
                                cx.notify();
                            });
                        }),
                ),
            )
            .child(self.section_label(section, cx))
            .children(rows)
            .child(div().flex_1())
            .child(
                div()
                    .px_3()
                    .pb_3()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(format!("{recorded} recorded"))),
            )
    }
}
