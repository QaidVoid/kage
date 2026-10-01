//! The shell's modal dialogs: setting a goal and confirming swarm mode.
//!
//! One view the shell mounts over everything; it draws nothing while no
//! dialog is open. A dialog closes on Esc, on a click on the scrim, and
//! after its action.

use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::store::{Store, StoreHandle as _};
use crate::theme::{FS_SM, Palette, R_MD};
use crate::views::deferred::Deferred;
use crate::views::kit::{self, BtnTone};

gpui_kit::actions!(kage_desktop, [DialogClose]);

/// The dialogs the shell can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    /// Set or clear the session goal.
    Goal,
    /// Confirm turning swarm mode on.
    ConfirmSwarm,
}

/// The dialog layer over the shell.
pub struct DialogView {
    store: Entity<Store>,
    open: Option<DialogKind>,
    focus: FocusHandle,
    goal: Entity<InputState>,
    /// The goal text, held until the dialog's field has been laid out;
    /// see [`crate::views::deferred`].
    goal_mirror: Deferred,
}

impl Focusable for DialogView {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DialogView {
    /// A dialog layer over `store`, closed.
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let goal = cx.new(|cx| {
            InputState::new(window, cx).placeholder("e.g. all retry tests pass three runs in a row")
        });
        cx.subscribe_in(&goal, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.save_goal(cx);
            }
        })
        .detach();
        Self {
            store,
            open: None,
            focus: cx.focus_handle(),
            goal,
            goal_mirror: Deferred::new(),
        }
    }

    /// Whether a dialog shows.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Shows `kind`, taking the focus.
    pub fn open(&mut self, kind: DialogKind, window: &mut Window, cx: &mut Context<Self>) {
        self.open = Some(kind);
        match kind {
            DialogKind::Goal => {
                let current = self.store.read(cx).active_session().and_then(|session| {
                    session
                        .config_options
                        .iter()
                        .find(|option| option.id == "goal")
                        .map(|option| option.current_value.clone())
                });
                let goal = self.goal.clone();
                self.goal_mirror.set(current.unwrap_or_default(), |text| {
                    goal.update(cx, |state, cx| state.set_value(text, window, cx));
                });
                goal.update(cx, |state, cx| state.focus(window, cx));
            }
            DialogKind::ConfirmSwarm => window.focus(&self.focus, cx),
        }
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.open = None;
        cx.notify();
    }

    fn save_goal(&mut self, cx: &mut Context<Self>) {
        let text = self.goal.read(cx).value().trim().to_owned();
        self.store.act(cx, |store| store.set_option("goal", &text));
        self.close(cx);
    }

    fn enable_swarm(&mut self, permission: Option<String>, cx: &mut Context<Self>) {
        self.store.act(cx, |store| {
            if let Some(permission) = &permission {
                store.set_permission(permission);
            }
            store.set_option("swarm", "on")
        });
        self.close(cx);
    }

    /// The advertised name of permission mode `value`, or the value.
    fn mode_name(&self, value: &str, cx: &Context<Self>) -> String {
        self.store
            .read(cx)
            .active_session()
            .and_then(|session| {
                session
                    .config_options
                    .iter()
                    .find(|option| option.id == "mode")?
                    .options
                    .iter()
                    .find(|choice| choice.value == value)
                    .map(|choice| choice.name.clone())
            })
            .unwrap_or_else(|| value.to_owned())
    }

    fn goal_body(
        &self,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> (IconName, &'static str, AnyElement, AnyElement) {
        let view = cx.entity();
        let laid_out = self.goal_mirror.laid_out().flag();
        let release = cx.entity().downgrade();
        let body = v_flex()
            .gap(px(10.))
            .child(div().text_size(px(FS_SM)).text_color(pal.muted).child(
                "The agent keeps working until the goal is met, checking it after each turn.",
            ))
            .child(
                div()
                    .on_prepaint(move |_, _, cx| {
                        laid_out.set(true);
                        let _ = release.update(cx, |_, cx| cx.notify());
                    })
                    .child(Input::new(&self.goal)),
            )
            .into_any_element();
        let cancel = view.clone();
        let foot = h_flex()
            .gap(px(8.))
            .child(
                kit::btn_sm("goal-cancel", BtnTone::Plain, pal)
                    .on_click(move |_, _, cx| cancel.update(cx, |this, cx| this.close(cx)))
                    .child("Cancel"),
            )
            .child(
                kit::btn_sm("goal-save", BtnTone::Primary, pal)
                    .on_click(move |_, _, cx| view.update(cx, |this, cx| this.save_goal(cx)))
                    .child("Set goal"),
            )
            .into_any_element();
        (IconName::Target, "Set a goal", body, foot)
    }

    fn swarm_body(
        &self,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> (IconName, &'static str, AnyElement, AnyElement) {
        let view = cx.entity();
        let asks_always = self.store.read(cx).permission_mode().as_deref() == Some("ask");
        let default_name = self.mode_name("default", cx);
        let mut body = v_flex().gap(px(10.)).child(
            div()
                .text_size(px(FS_SM))
                .text_color(pal.ink)
                .child("The agent will split work into many parallel sub-agents, one per item, and merge their results."),
        );
        if asks_always {
            body = body.child(
                div()
                    .text_size(px(13.))
                    .text_color(pal.muted)
                    .child(SharedString::from(format!(
                        "You are in {}. Every worker edit would stop for approval, so swarms work best with {default_name}.",
                        self.mode_name("ask", cx)
                    ))),
            );
        }
        let cancel = view.clone();
        let mut foot = h_flex().gap(px(8.)).child(
            kit::btn_sm("swarm-cancel", BtnTone::Plain, pal)
                .on_click(move |_, _, cx| cancel.update(cx, |this, cx| this.close(cx)))
                .child("Cancel"),
        );
        if asks_always {
            let relax = view.clone();
            foot = foot.child(
                kit::btn_sm("swarm-relax", BtnTone::Plain, pal)
                    .on_click(move |_, _, cx| {
                        relax.update(cx, |this, cx| {
                            this.enable_swarm(Some("default".to_owned()), cx)
                        });
                    })
                    .child(SharedString::from(format!("Enable with {default_name}"))),
            );
        }
        foot = foot.child(
            kit::btn_sm("swarm-enable", BtnTone::Primary, pal)
                .on_click(move |_, _, cx| view.update(cx, |this, cx| this.enable_swarm(None, cx)))
                .child("Enable swarm"),
        );
        (
            IconName::Waypoints,
            "Enable swarm mode?",
            body.into_any_element(),
            foot.into_any_element(),
        )
    }
}

impl Render for DialogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let goal = self.goal.clone();
        self.goal_mirror.flush(|text| {
            goal.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        let Some(kind) = self.open else {
            return div().into_any_element();
        };
        let pal = Palette::active(cx);
        let (icon, title, body, foot) = match kind {
            DialogKind::Goal => self.goal_body(pal, cx),
            DialogKind::ConfirmSwarm => self.swarm_body(pal, cx),
        };
        let (icon_fg, icon_bg) = match kind {
            DialogKind::Goal => (pal.accent, pal.accent_soft),
            DialogKind::ConfirmSwarm => (pal.done, pal.done_soft),
        };
        let scrim_close = cx.entity();
        let mut card = v_flex()
            .id("dialog")
            .track_focus(&self.focus)
            .key_context("Dialog")
            .on_action(cx.listener(|this, _: &DialogClose, _, cx| this.close(cx)))
            .w(px(if kind == DialogKind::Goal { 480. } else { 460. }))
            .bg(pal.bg)
            .border_1()
            .border_color(pal.line)
            .rounded(px(16.))
            .overflow_hidden()
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .child(
                h_flex()
                    .gap(px(10.))
                    .items_center()
                    .px(px(18.))
                    .pt(px(16.))
                    .pb(px(8.))
                    .child(
                        div()
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(R_MD))
                            .bg(icon_bg)
                            .text_color(icon_fg)
                            .child(Icon::new(icon).with_size(px(14.))),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(16.))
                            .text_color(pal.ink_strong)
                            .child(title),
                    ),
            )
            .child(div().px(px(18.)).py(px(8.)).child(body))
            .child(
                h_flex()
                    .justify_end()
                    .px(px(18.))
                    .pt(px(12.))
                    .pb(px(16.))
                    .child(foot),
            );
        card.style().box_shadow = Some(pal.shadow_2.clone());
        div()
            .id("dialog-scrim")
            .absolute()
            .inset_0()
            .bg(Hsla {
                h: 0.,
                s: 0.,
                l: 0.,
                a: 0.45,
            })
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(gpui_kit::MouseButton::Left, move |_, _, cx| {
                scrim_close.update(cx, |this, cx| this.close(cx));
            })
            .child(card)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, TestAppContext, Window};
    use kage_client::Frame;

    use super::{DialogKind, DialogView};
    use crate::store::{Command, Store};
    use crate::transport::State;

    /// A store with session `s1` in Ask mode and swarm mode off.
    fn asking_store() -> Store {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        for command in store.take_commands() {
            if let Command::Handshake { replay_sessions } = command {
                store.handshake(replay_sessions);
            }
        }
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: 1,
            result: serde_json::json!({"protocolVersion": 1, "agentCapabilities": {}}),
        });
        let _ = store.take_outgoing();
        store.new_session();
        let opened = store
            .take_outgoing()
            .into_iter()
            .find_map(|frame| match frame {
                Frame::Request { id, method, .. } if method == "session/new" => Some(id),
                _ => None,
            })
            .expect("the session opens");
        store.absorb(Frame::Success {
            id: opened,
            result: serde_json::json!({
                "sessionId": "s1",
                "configOptions": [
                    {"id": "mode", "name": "Mode", "type": "select", "currentValue": "ask",
                     "options": [{"value": "default", "name": "Default"}, {"value": "ask", "name": "Ask"}]},
                    {"id": "swarm", "name": "Swarm", "type": "select", "currentValue": "off",
                     "options": [{"value": "off", "name": "Off"}, {"value": "on", "name": "On"}]},
                ],
            }),
        });
        let _ = store.take_outgoing();
        store
    }

    #[gpui_kit::test]
    fn enabling_swarm_from_ask_relaxes_the_mode_first(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let store = cx.new(|_| asking_store());
        let (dialog, visual) = cx
            .add_window_view(|window: &mut Window, cx| DialogView::new(store.clone(), window, cx));
        visual.update(|window, cx| {
            dialog.update(cx, |dialog, cx| {
                dialog.open(DialogKind::ConfirmSwarm, window, cx);
                assert!(dialog.is_open());
                dialog.enable_swarm(Some("default".to_owned()), cx);
                assert!(!dialog.is_open(), "the dialog closes after its action");
            });
        });
        let sent: Vec<(String, String)> = visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store
                    .take_outgoing()
                    .into_iter()
                    .filter_map(|frame| match frame {
                        Frame::Request { params, .. } => Some((
                            params["configId"].as_str()?.to_owned(),
                            params["value"].as_str()?.to_owned(),
                        )),
                        _ => None,
                    })
                    .collect()
            })
        });
        assert_eq!(
            sent,
            [
                ("mode".to_owned(), "default".to_owned()),
                ("swarm".to_owned(), "on".to_owned())
            ]
        );
    }
}
