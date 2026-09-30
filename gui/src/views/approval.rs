//! The approval card: the open permission asks and their verdicts.
//!
//! One view over the store, for the shell to mount where the dock
//! renders the approval card; it draws nothing while no ask is open.
//! With asks open it renders each exactly as the agent offered it:
//! the shield glyph, or the swarm glyph when a child asked, the tool
//! title, the asking session's byline, the subject and the remaining
//! input detail the ask carried, and the options in offer order
//! numbered from 1. Option ids and labels are the offered ones; the
//! card never synthesizes either.
//!
//! Answering:
//!
//! - A click on an option row answers with that option's offered id.
//! - The number keys 1 to 9 answer the oldest open ask. The key
//!   handling is scoped to the card by focus: the card registers its
//!   own focus handle, takes the window focus when a first ask
//!   arrives, and answers a digit only while that handle, and not a
//!   child such as the feedback field, holds the focus. A keypress
//!   aimed at the composer therefore never answers, because the
//!   composer sits outside the card's focus subtree and the key
//!   never reaches the card's handler.
//! - The feedback field answers with the ask's first reject option
//!   and sends the typed text through the client's feedback channel,
//!   which carries it to the agent under `_meta.kage.planReview`; the
//!   field renders only when the ask offers a reject.
//!
//! When `$/cancel_request` withdraws an ask, the state drops it and
//! the store surfaces the answered-elsewhere change; the card closes
//! because its queue is the state's. The toast naming the ask as
//! answered elsewhere is the toast story's surface and is not built
//! here.
//!
//! Focus ownership in one sentence: the card holds the window focus
//! from the first open ask until it is answered or withdrawn, hands
//! it back when the last ask closes, and takes it no other time; a
//! click into the composer takes it back for typing at once.

use std::collections::HashMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight, InteractiveElement as _,
    IntoElement, KeyDownEvent, Modifiers, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, Window, div, px,
};
use kage_client::wire::PermissionOptionKind;
use kage_client::{PermissionAsk, PermissionDecision};
use serde_json::Value;

use crate::store::Store;

/// The keys of a tool input that name its primary argument, most
/// specific first.
const SUBJECT_KEYS: [&str; 5] = ["command", "path", "pattern", "url", "description"];

/// The text one input value renders as, strings verbatim.
fn value_text(value: &Value) -> String {
    match value.as_str() {
        Some(text) => text.to_owned(),
        None => value.to_string(),
    }
}

/// The subject an ask is about: the input's primary argument, or the
/// whole input as the wire carried it. `None` when the ask carries no
/// input at all.
fn subject_of(ask: &PermissionAsk) -> Option<String> {
    let input = ask.tool_call.raw_input.as_ref()?;
    SUBJECT_KEYS
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| Some(input.to_string()))
}

/// The rest of the input the subject line does not already show, as
/// `key: value` entries. `None` when the subject came from the whole
/// input or nothing remains.
fn reason_of(ask: &PermissionAsk) -> Option<String> {
    let input = ask.tool_call.raw_input.as_ref()?.as_object()?;
    let subject_key = SUBJECT_KEYS
        .iter()
        .find(|key| input.contains_key(**key))
        .copied()?;
    let entries: Vec<String> = input
        .iter()
        .filter(|(key, _)| Some(key.as_str()) != Some(subject_key))
        .map(|(key, value)| format!("{key}: {}", value_text(value)))
        .collect();
    if entries.is_empty() {
        None
    } else {
        Some(entries.join(", "))
    }
}

/// The id of the ask's first reject option: the option the feedback
/// field answers with. `None` when the ask offers no reject, in which
/// case the card offers no feedback field.
fn reject_option_id(ask: &PermissionAsk) -> Option<&str> {
    ask.options
        .iter()
        .find(|option| {
            matches!(
                option.kind,
                PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
            )
        })
        .map(|option| option.option_id.as_str())
}

/// The approval card over the store's open permission asks.
pub struct ApprovalCard {
    store: Entity<Store>,
    /// The card's own focus handle; the digit answers ride it.
    focus: FocusHandle,
    /// The open-ask count at the last sync, so a first ask can take
    /// the window focus and a last ask can hand it back.
    seen: usize,
    /// The feedback field of each ask that offers a reject, keyed by
    /// request id. The typed text survives every redraw with the
    /// entity.
    feedback: HashMap<u64, Entity<InputState>>,
}

impl Focusable for ApprovalCard {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ApprovalCard {
    /// An approval card following `store`. Mount it in the dock row
    /// above the composer with `cx.new(|cx| ApprovalCard::new(store,
    /// window, cx))`; it renders nothing while no ask is open, so the
    /// mount needs no visibility wiring.
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_in(&store, window, |this, _, window, cx| {
            this.sync_open_asks(window, cx);
            cx.notify();
        })
        .detach();
        let mut card = Self {
            store,
            focus: cx.focus_handle(),
            seen: 0,
            feedback: HashMap::new(),
        };
        card.sync_open_asks(window, cx);
        card
    }

    /// Syncs the view with the state's ask queue: takes the window
    /// focus when a first ask arrives, hands it back when the last ask
    /// closes while the card holds it, and keeps one feedback field
    /// per open ask that offers a reject.
    fn sync_open_asks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let asks: Vec<(String, u64, bool)> = self
            .store
            .read(cx)
            .state()
            .open_asks()
            .into_iter()
            .map(|(session_id, ask)| {
                (
                    session_id.to_owned(),
                    ask.request_id,
                    reject_option_id(ask).is_some(),
                )
            })
            .collect();
        let open = asks.len();
        let was = self.seen;
        self.seen = open;
        if open > 0 && was == 0 {
            window.focus(&self.focus, cx);
        } else if open == 0 && was > 0 && self.focus.is_focused(window) {
            window.blur(cx);
        }
        self.feedback
            .retain(|request_id, _| asks.iter().any(|(_, id, _)| id == request_id));
        for (session_id, request_id, rejectable) in asks {
            if !rejectable || self.feedback.contains_key(&request_id) {
                continue;
            }
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder("Reject with feedback..."));
            let store = self.store.clone();
            cx.subscribe_in(
                &input,
                window,
                move |this, input, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        let text = input.read(cx).value().trim().to_owned();
                        if text.is_empty() {
                            return;
                        }
                        let answered = store.update(cx, |store, _| {
                            let reject = store
                                .state()
                                .session(&session_id)
                                .and_then(|session| {
                                    session
                                        .permissions
                                        .iter()
                                        .find(|ask| ask.request_id == request_id)
                                })
                                .and_then(reject_option_id);
                            let Some(reject) = reject else {
                                return false;
                            };
                            store.reply_permission(
                                &session_id,
                                request_id,
                                &PermissionDecision::Feedback {
                                    option_id: reject.to_owned(),
                                    feedback: text.clone(),
                                },
                            )
                        });
                        if answered {
                            input.update(cx, |state, cx| state.set_value("", window, cx));
                            this.feedback.remove(&request_id);
                            cx.notify();
                        }
                    }
                },
            )
            .detach();
            self.feedback.insert(request_id, input);
        }
    }

    /// The oldest open ask, owned, for the digit answers.
    fn first_ask(&self, cx: &gpui_kit::App) -> Option<(String, PermissionAsk)> {
        self.store
            .read(cx)
            .state()
            .open_asks()
            .into_iter()
            .next()
            .map(|(session_id, ask)| (session_id.to_owned(), ask.clone()))
    }

    /// Answers the oldest open ask with the option the digit names.
    /// Runs only while the card itself, and not a child field, holds
    /// the window focus.
    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            return;
        }
        if event.keystroke.modifiers != Modifiers::default() {
            return;
        }
        let digit: usize = event.keystroke.key.parse().unwrap_or(0);
        let Some((session_id, ask)) = self.first_ask(cx) else {
            return;
        };
        let Some(option) = digit
            .checked_sub(1)
            .and_then(|index| ask.options.get(index))
            .cloned()
        else {
            return;
        };
        let decision = PermissionDecision::Option(option.option_id);
        self.store.update(cx, |store, cx| {
            store.reply_permission(&session_id, ask.request_id, &decision);
            cx.notify();
        });
        self.feedback.remove(&ask.request_id);
        cx.notify();
    }

    /// One ask, rendered as offered.
    fn ask_view(
        &mut self,
        session_id: &str,
        ask: &PermissionAsk,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().colors;
        let byline = self.store.read(cx).state().asker_byline(session_id);
        let child = byline
            .as_ref()
            .is_some_and(|byline| byline.starts_with("sub agent"));
        let title = ask
            .tool_call
            .title
            .clone()
            .unwrap_or_else(|| ask.tool_call.tool_call_id.clone());
        let mut head = h_flex()
            .gap_2()
            .items_center()
            .child(
                Icon::new(if child {
                    IconName::Network
                } else {
                    IconName::ShieldQuestionMark
                })
                .text_color(theme.warning),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(SharedString::from(title)),
            );
        if let Some(byline) = byline {
            head = head.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(byline)),
            );
        }
        let mut card = v_flex()
            .id(SharedString::from(format!("approval-{}", ask.request_id)))
            .w_full()
            .gap_2()
            .p_3()
            .rounded(px(6.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(head);
        if let Some(subject) = subject_of(ask) {
            card = card.child(
                div()
                    .text_size(px(12.))
                    .font_family("JetBrains Mono")
                    .text_color(theme.foreground)
                    .child(SharedString::from(subject)),
            );
        }
        if let Some(reason) = reason_of(ask) {
            card = card.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(reason)),
            );
        }
        let mut options = v_flex().gap_1();
        for (index, option) in ask.options.iter().enumerate() {
            let number = index + 1;
            let danger = matches!(
                option.kind,
                PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
            );
            let store = self.store.clone();
            let session = session_id.to_owned();
            let request_id = ask.request_id;
            let option_id = option.option_id.clone();
            options = options.child(
                h_flex()
                    .id(SharedString::from(format!(
                        "approval-{request_id}-{number}"
                    )))
                    .test_support()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .rounded(px(4.))
                    .hover(|row| row.bg(theme.list_hover))
                    .aria_label(SharedString::from(format!("{number} {}", option.name)))
                    .on_click(move |_, _, cx| {
                        store.update(cx, |store, cx| {
                            store.reply_permission(
                                &session,
                                request_id,
                                &PermissionDecision::Option(option_id.clone()),
                            );
                            cx.notify();
                        });
                    })
                    .child(
                        div()
                            .w(px(16.))
                            .rounded(px(3.))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(px(10.))
                            .text_color(theme.muted_foreground)
                            .child(number.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(if danger {
                                theme.danger
                            } else {
                                theme.foreground
                            })
                            .child(SharedString::from(option.name.clone())),
                    ),
            );
        }
        card = card.child(options);
        if let Some(input) = self.feedback.get(&ask.request_id) {
            card = card.child(
                h_flex()
                    .id(SharedString::from(format!(
                        "approval-feedback-{}",
                        ask.request_id
                    )))
                    .w_full()
                    .child(Input::new(input).flex_1()),
            );
        }
        card
    }
}

impl Render for ApprovalCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let asks: Vec<(String, PermissionAsk)> = self
            .store
            .read(cx)
            .state()
            .open_asks()
            .into_iter()
            .map(|(session_id, ask)| (session_id.to_owned(), ask.clone()))
            .collect();
        let mut surface = v_flex()
            .id("approval")
            .test_support()
            .w_full()
            .gap_2()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }));
        for (session_id, ask) in asks {
            surface = surface.child(self.ask_view(&session_id, &ask, cx));
        }
        surface
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, Window};

    use super::{ApprovalCard, reason_of, subject_of};
    use crate::store::{Command, Store};
    use crate::transport::State;
    use kage_client::wire::{PermissionOption, ToolCallUpdate};
    use kage_client::{Frame, PermissionAsk, TranscriptItem};

    /// An initialize answer with everything the gate accepts.
    fn init_answer() -> Frame {
        Frame::Success {
            id: 1,
            result: serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": {"steer": true},
                "agentInfo": {"name": "kage", "version": "0.1.0"},
            }),
        }
    }

    /// Carries out the boot commands the shell would.
    fn run_commands(store: &mut Store) {
        for command in store.take_commands() {
            match command {
                Command::Handshake { replay_sessions } => store.handshake(replay_sessions),
                Command::NewSession => store.new_session(),
                Command::ReplayPrompt => {
                    let _ = store.prompt("fix the null check");
                }
            }
        }
    }

    /// A store through the handshake with one open session "s1".
    fn booted_store() -> Store {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer());
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: 2,
            result: serde_json::json!({"sessionId": "s1"}),
        });
        let _ = store.take_outgoing();
        store
    }

    /// A `session/request_permission` ask with three options.
    fn ask_frame(session: &str, request_id: u64) -> Frame {
        Frame::Request {
            id: request_id,
            method: "session/request_permission".into(),
            params: serde_json::json!({
                "sessionId": session,
                "toolCall": {"toolCallId": "call-sh", "title": "shell", "kind": "execute",
                    "status": "pending", "rawInput": {"command": "cargo test"}},
                "options": [
                    {"optionId": "allow", "name": "Allow shell", "kind": "allow_once"},
                    {"optionId": "allow_session", "name": "Allow shell for this session",
                        "kind": "allow_always"},
                    {"optionId": "reject", "name": "Reject shell", "kind": "reject_once"},
                ],
            }),
        }
    }

    /// A store with one open ask on "s1".
    fn asked_store() -> Store {
        let mut store = booted_store();
        store.absorb(ask_frame("s1", 101));
        store
    }

    /// Opens a window on an approval card over `store`.
    fn window_on(
        cx: &mut TestAppContext,
        store: Entity<Store>,
    ) -> (Entity<ApprovalCard>, &mut gpui_kit::VisualTestContext) {
        cx.update(gpui_kit::init);
        cx.add_window_view(|window: &mut Window, cx| ApprovalCard::new(store.clone(), window, cx))
    }

    #[test]
    fn subject_and_reason_come_from_the_delivered_input_only() {
        let ask = |raw_input: Option<serde_json::Value>| PermissionAsk {
            request_id: 1,
            tool_call: ToolCallUpdate {
                tool_call_id: "call-sh".into(),
                title: Some("shell".into()),
                raw_input,
                ..ToolCallUpdate::default()
            },
            options: Vec::new(),
        };
        let shell = ask(Some(serde_json::json!({"command": "cargo test"})));
        assert_eq!(subject_of(&shell).as_deref(), Some("cargo test"));
        assert_eq!(reason_of(&shell), None, "nothing remains to show");

        let agent = ask(Some(
            serde_json::json!({"description": "list files", "prompt": "list"}),
        ));
        assert_eq!(subject_of(&agent).as_deref(), Some("list files"));
        assert_eq!(reason_of(&agent).as_deref(), Some("prompt: list"));

        let bare = ask(None);
        assert_eq!(subject_of(&bare), None);
        assert_eq!(reason_of(&bare), None);
    }

    #[test]
    fn the_feedback_field_targets_the_first_reject_option() {
        let ask = PermissionAsk {
            request_id: 1,
            tool_call: ToolCallUpdate::default(),
            options: vec![
                PermissionOption {
                    option_id: "approve".into(),
                    name: "Approve".into(),
                    kind: kage_client::wire::PermissionOptionKind::AllowOnce,
                },
                PermissionOption {
                    option_id: "revise".into(),
                    name: "Revise".into(),
                    kind: kage_client::wire::PermissionOptionKind::RejectOnce,
                },
                PermissionOption {
                    option_id: "reject".into(),
                    name: "Reject".into(),
                    kind: kage_client::wire::PermissionOptionKind::RejectOnce,
                },
            ],
        };
        assert_eq!(super::reject_option_id(&ask), Some("revise"));
        let allow_only = PermissionAsk {
            request_id: 2,
            tool_call: ToolCallUpdate::default(),
            options: vec![PermissionOption {
                option_id: "ok".into(),
                name: "Ok".into(),
                kind: kage_client::wire::PermissionOptionKind::AllowOnce,
            }],
        };
        assert_eq!(super::reject_option_id(&allow_only), None);
    }

    #[gpui_kit::test]
    fn the_card_renders_exactly_the_offered_options_numbered_from_one(cx: &mut TestAppContext) {
        let store = cx.new(|_| asked_store());
        let (_view, visual) = window_on(cx, store);
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, _| {
            assert_eq!(window.find("approval-101-1").label(), Some("1 Allow shell"));
            assert_eq!(
                window.find("approval-101-2").label(),
                Some("2 Allow shell for this session"),
            );
            assert_eq!(
                window.find("approval-101-3").label(),
                Some("3 Reject shell"),
            );
            assert!(
                window.try_find("approval-101-4").is_none(),
                "nothing beyond the offered options is rendered"
            );
        });
    }

    #[gpui_kit::test]
    fn pressing_three_answers_with_the_third_option_and_records_it(cx: &mut TestAppContext) {
        let store = cx.new(|_| asked_store());
        let (_view, visual) = window_on(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));
        assert_eq!(
            visual.update(|window, _| window.find("approval").focused()),
            Some(true),
            "a first ask takes the window focus, so the digits answer"
        );

        visual.update(|window, cx| window.press("3", cx));
        let outgoing = visual.update(|_, cx| store.update(cx, |store, _| store.take_outgoing()));
        assert_eq!(
            outgoing.last(),
            Some(&Frame::Success {
                id: 101,
                result: serde_json::json!({
                    "outcome": {"outcome": "selected", "optionId": "reject"},
                }),
            }),
            "the third number key answers with the third offered option id"
        );
        visual.update(|_, cx| {
            let session = store.read(cx).state().session("s1").unwrap();
            let Some(TranscriptItem::Decision {
                subject,
                label,
                allowed,
                feedback,
            }) = session.items.last()
            else {
                panic!(
                    "the transcript gained no decision row: {:?}",
                    session.items.last()
                );
            };
            assert_eq!(
                (subject.as_str(), label.as_str(), *allowed),
                ("shell", "Reject shell", false),
            );
            assert_eq!(feedback.as_deref(), None);
        });
        assert!(
            visual.update(|window, _| window.try_find("approval-101-1").is_none()),
            "the answered ask closed the card"
        );
    }

    #[gpui_kit::test]
    fn a_digit_outside_the_card_focus_never_answers(cx: &mut TestAppContext) {
        let store = cx.new(|_| asked_store());
        let (view, visual) = window_on(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));

        visual.update(|window, cx| {
            let input = view
                .read(cx)
                .feedback
                .get(&101)
                .expect("the ask offers a reject, so the field rendered")
                .clone();
            input.update(cx, |state, cx| state.focus(window, cx));
        });
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|window, cx| window.press("1", cx));
        assert!(
            visual.update(|_, cx| store
                .update(cx, |store, _| store.take_outgoing())
                .is_empty()),
            "a digit typed into the feedback field types, it never answers"
        );
        let text = visual.update(|_, cx| {
            let input = view.read(cx).feedback.get(&101).unwrap().clone();
            input.read(cx).value().to_string()
        });
        assert_eq!(text, "1", "the digit went into the field instead");
    }

    #[gpui_kit::test]
    fn reject_with_feedback_rides_the_meta_channel_and_the_record_quotes_it(
        cx: &mut TestAppContext,
    ) {
        let store = cx.new(|_| asked_store());
        let (view, visual) = window_on(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));

        visual.update(|window, cx| {
            let input = view.read(cx).feedback.get(&101).unwrap().clone();
            input.update(cx, |state, cx| state.focus(window, cx));
        });
        visual.update(|window, cx| window.input("use rustfmt first", cx));
        visual.update(|window, cx| window.press("enter", cx));

        let outgoing = visual.update(|_, cx| store.update(cx, |store, _| store.take_outgoing()));
        assert_eq!(
            outgoing.last(),
            Some(&Frame::Success {
                id: 101,
                result: serde_json::json!({
                    "outcome": {"outcome": "selected", "optionId": "reject"},
                    "_meta": {"kage": {"planReview": {"revision": "use rustfmt first"}}},
                }),
            }),
            "the feedback rides the extended answer"
        );
        visual.update(|_, cx| {
            let session = store.read(cx).state().session("s1").unwrap();
            let Some(TranscriptItem::Decision {
                subject,
                label,
                allowed,
                feedback,
            }) = session.items.last()
            else {
                panic!(
                    "the transcript gained no decision row: {:?}",
                    session.items.last()
                );
            };
            assert_eq!(
                (subject.as_str(), label.as_str(), *allowed),
                ("shell", "Reject shell", false),
            );
            assert_eq!(feedback.as_deref(), Some("use rustfmt first"));
        });
    }

    #[gpui_kit::test]
    fn a_withdrawn_ask_closes_the_card_without_answering(cx: &mut TestAppContext) {
        let store = cx.new(|_| asked_store());
        let (_view, visual) = window_on(cx, store.clone());
        visual.update(|window, cx| window.render_frame(cx));
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store.absorb(Frame::Notification {
                    method: "$/cancel_request".into(),
                    params: serde_json::json!({"requestId": 101}),
                });
            });
        });
        visual.update(|window, cx| window.render_frame(cx));
        assert!(
            visual.update(|window, _| window.try_find("approval-101-1").is_none()),
            "the answered-elsewhere ask closed the card"
        );
        assert!(
            visual.update(|_, cx| store
                .read(cx)
                .state()
                .session("s1")
                .unwrap()
                .permissions
                .is_empty()),
            "nothing stays pending"
        );
        visual.update(|window, cx| window.press("1", cx));
        assert!(
            visual.update(|_, cx| store
                .update(cx, |store, _| store.take_outgoing())
                .is_empty()),
            "a closed card answers nothing"
        );
    }
}
