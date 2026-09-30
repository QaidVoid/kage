//! The dock row above the composer: the goal pill with its popover,
//! the plan review pill with its three answers, the running swarm and
//! todos pills, and the queued prompt rows.
//!
//! Every pill derives its state from the active session the store
//! holds, so the dock counts only what frames delivered and hides
//! what is incomputable. Two pieces of the full dock are absent on
//! purpose: the background-agents pill, which waits for the wire to
//! carry background agent events, and the approval card, a separate
//! view the shell mounts beside this row. A revise answer picks the
//! ask's own revise option and rides the client's feedback channel,
//! which carries the typed text to the agent under
//! `_meta.kage.planReview`.

use gpui_kit::AnyElement;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    Anchor, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Window, div, px,
};
use kage_client::wire::{
    ContentBlock, NoticeTone, PermissionOption, PermissionOptionKind, SubagentState,
};
use kage_client::{PermissionDecision, Session, TranscriptItem};
use serde_json::Value;

use crate::store::Store;

/// The config option id the goal pill reads and edits.
const GOAL_OPTION: &str = "goal";
/// The prefix of the success notice that reports a met goal.
const GOAL_MET_PREFIX: &str = "goal met: ";
/// The tool whose open ask is the plan mode review.
const EXIT_PLAN_TOOL: &str = "exit_plan";
/// Characters a queue row shows before the ellipsis.
const QUEUE_TEXT_COLUMNS: usize = 80;
/// The width of the todos mini bar, in pixels.
const TODO_BAR: f32 = 44.0;

/// The goal pill's state, as the session carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoalState {
    /// The goal text the option holds.
    pub text: String,
    /// Whether a success notice reported the goal met.
    pub met: bool,
}

/// The goal state of a session, when its option holds a goal.
#[must_use]
pub(crate) fn goal_state(session: &Session) -> Option<GoalState> {
    let text = session
        .config_options
        .iter()
        .find(|option| option.id == GOAL_OPTION)
        .map(|option| option.current_value.clone())
        .unwrap_or_default();
    if text.is_empty() {
        return None;
    }
    let met = session.items.iter().any(|item| {
        matches!(
            item,
            TranscriptItem::Notice {
                tone: NoticeTone::Success,
                text,
            } if text.starts_with(GOAL_MET_PREFIX)
        )
    });
    Some(GoalState { text, met })
}

/// The plan review pill's state, from the open exit plan ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanReviewState {
    /// The id the decision answers.
    pub request_id: u64,
    /// The offered approve option, when the ask offers one.
    pub approve: Option<String>,
    /// The offered revise option, when the ask offers one.
    pub revise: Option<String>,
    /// The offered reject option, when the ask offers one.
    pub reject: Option<String>,
}

/// The first offered option whose id or name is `name`, case
/// insensitively. A review ask carries two reject-kind options, so
/// the kind alone cannot tell revise from reject; the option ids the
/// agent reads decisions from can.
fn option_named(options: &[PermissionOption], name: &str) -> Option<String> {
    options
        .iter()
        .find(|option| {
            option.option_id.eq_ignore_ascii_case(name) || option.name.eq_ignore_ascii_case(name)
        })
        .map(|option| option.option_id.clone())
}

/// The open plan review of a session, when one is pending.
#[must_use]
pub(crate) fn plan_review(session: &Session) -> Option<PlanReviewState> {
    let ask = session
        .permissions
        .iter()
        .find(|ask| ask.tool_call.title.as_deref() == Some(EXIT_PLAN_TOOL))?;
    let approve = match ask.option_of(PermissionOptionKind::AllowOnce) {
        Some(id) => Some(id.to_owned()),
        None => option_named(&ask.options, "approve"),
    };
    Some(PlanReviewState {
        request_id: ask.request_id,
        approve,
        revise: option_named(&ask.options, "revise"),
        reject: option_named(&ask.options, "reject"),
    })
}

/// The swarm pill's state, from the session's subagent tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwarmState {
    /// Members that reached an end state the pill counts as done.
    pub done: usize,
    /// Every member announced so far.
    pub total: usize,
    /// Members paused mid task.
    pub paused: usize,
    /// One row per member: its name or id, and its state word.
    pub members: Vec<(String, &'static str)>,
}

/// The state word one member renders with.
fn member_word(state: Option<SubagentState>) -> &'static str {
    match state {
        None | Some(SubagentState::Running) => "running",
        Some(SubagentState::Paused) => "paused",
        Some(SubagentState::Completed) => "done",
        Some(SubagentState::Failed) => "failed",
        Some(SubagentState::Cancelled) => "cancelled",
    }
}

/// The swarm state of a session, while it still has running or paused
/// members. A member never given a state is running.
#[must_use]
pub(crate) fn swarm_state(session: &Session) -> Option<SwarmState> {
    let total = session.agents.len();
    if total == 0 {
        return None;
    }
    let members: Vec<(String, &'static str)> = session
        .agents
        .iter()
        .map(|(id, agent)| {
            (
                agent.name.clone().unwrap_or_else(|| id.clone()),
                member_word(agent.state),
            )
        })
        .collect();
    let live = members
        .iter()
        .filter(|(_, word)| *word == "running" || *word == "paused")
        .count();
    if live == 0 {
        return None;
    }
    Some(SwarmState {
        done: members
            .iter()
            .filter(|(_, word)| *word == "done" || *word == "failed")
            .count(),
        total,
        paused: members.iter().filter(|(_, word)| *word == "paused").count(),
        members,
    })
}

/// The todos pill's state, from the latest plan update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TodosState {
    /// Entries marked completed.
    pub done: usize,
    /// Every entry the plan carries.
    pub total: usize,
}

/// The todos state of a session, when its latest plan has entries.
#[must_use]
pub(crate) fn todos_state(session: &Session) -> Option<TodosState> {
    let entries = session.items.iter().rev().find_map(|item| match item {
        TranscriptItem::Plan { entries } => Some(entries),
        _ => None,
    })?;
    let total = entries.len();
    (total > 0).then(|| TodosState {
        done: entries
            .iter()
            .filter(|entry| entry.get("status").and_then(Value::as_str) == Some("completed"))
            .count(),
        total,
    })
}

/// One queue row: a held prompt and its place in the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueueRow {
    /// The queue position the row acts on.
    pub index: usize,
    /// The prompt text, truncated for the row.
    pub text: String,
    /// The prompt text in full, for the edit action.
    pub full: String,
}

/// The text blocks of a prompt, joined.
fn prompt_text(prompt: &[ContentBlock]) -> String {
    prompt
        .iter()
        .filter_map(|block| block.as_text())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` cut to `columns` characters with an ellipsis when cut.
fn truncate_text(text: &str, columns: usize) -> String {
    if text.chars().count() <= columns {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(columns).collect();
    cut.push('\u{2026}');
    cut
}

/// The queue rows of a session, oldest first.
#[must_use]
pub(crate) fn queue_rows(session: &Session) -> Vec<QueueRow> {
    session
        .queue
        .iter()
        .enumerate()
        .map(|(index, queued)| {
            let full = prompt_text(&queued.prompt);
            QueueRow {
                index,
                text: truncate_text(&full, QUEUE_TEXT_COLUMNS),
                full,
            }
        })
        .collect()
}

/// What the dock asks the shell to carry out in another view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockEvent {
    /// Bring the plan card under review into view in the transcript.
    /// The dock cannot scroll the transcript itself; the shell
    /// subscribes and forwards the request.
    ScrollToPlan,
}

/// The pills row the shell mounts above its composer.
pub struct DockRow {
    store: Entity<Store>,
    /// Whether the goal popover shows.
    goal_open: bool,
    /// Whether the revise text field shows under the pill row.
    revise_open: bool,
    /// The goal text field inside the goal popover.
    goal_input: Entity<InputState>,
    /// The revise text field under the pill row.
    revise_input: Entity<InputState>,
}

impl EventEmitter<DockEvent> for DockRow {}

impl DockRow {
    /// A dock row following `store`. The shell builds it inside its
    /// own view construction, as it builds the composer, and
    /// subscribes to [`DockEvent`] on the returned entity to scroll
    /// the transcript.
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let goal_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Set the goal option; empty clears it")
        });
        let revise_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Tell kage what to change in the plan")
        });
        cx.subscribe_in(
            &goal_input,
            window,
            |this, goal, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = goal.read(cx).value().trim().to_owned();
                    this.save_goal(&text, cx);
                    goal.update(cx, |state, cx| state.set_value("", window, cx));
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &revise_input,
            window,
            |this, revise, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.send_revise(cx);
                    revise.update(cx, |state, cx| state.set_value("", window, cx));
                }
            },
        )
        .detach();
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            goal_open: false,
            revise_open: false,
            goal_input,
            revise_input,
        }
    }

    /// Saves `text` into the goal option; an empty text clears it.
    fn save_goal(&mut self, text: &str, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, _| store.set_option(GOAL_OPTION, text));
        self.goal_open = false;
        cx.notify();
    }

    /// Answers the open plan review with the offered option `choice`
    /// names, exactly the id the ask offered. A revise carries its
    /// text through the client's feedback channel.
    fn decide_plan(&mut self, choice: &str, revision: Option<&str>, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let Some(session_id) = store.read(cx).active_id().map(str::to_owned) else {
            return;
        };
        let Some(review) = store.read(cx).active_session().and_then(plan_review) else {
            return;
        };
        let offered = match choice {
            "approve" => review.approve.clone(),
            "revise" => review.revise.clone(),
            _ => review.reject.clone(),
        };
        let Some(option_id) = offered else {
            return;
        };
        let revision = revision
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        let decision = match revision {
            Some(feedback) => PermissionDecision::Feedback {
                option_id,
                feedback,
            },
            None => PermissionDecision::Option(option_id),
        };
        store.update(cx, |store, _| {
            store.reply_permission(&session_id, review.request_id, &decision);
        });
        self.revise_open = false;
        cx.notify();
    }

    /// Answers the review with its offered approve option.
    fn approve_plan(&mut self, cx: &mut Context<Self>) {
        self.decide_plan("approve", None, cx);
    }

    /// Answers the review with its offered reject option.
    fn reject_plan(&mut self, cx: &mut Context<Self>) {
        self.decide_plan("reject", None, cx);
    }

    /// Answers the review with its offered revise option and the
    /// typed text as its feedback.
    fn send_revise(&mut self, cx: &mut Context<Self>) {
        let text = self.revise_input.read(cx).value().trim().to_owned();
        self.decide_plan("revise", Some(&text), cx);
    }

    /// Shows the revise field under the pill row.
    fn open_revise(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.revise_open = true;
        self.revise_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// Hides the revise field.
    fn close_revise(&mut self, cx: &mut Context<Self>) {
        self.revise_open = false;
        cx.notify();
    }

    /// Asks the shell, through [`DockEvent::ScrollToPlan`], to bring
    /// the plan card into view.
    fn request_scroll_to_plan(&mut self, cx: &mut Context<Self>) {
        cx.emit(DockEvent::ScrollToPlan);
    }

    /// Sends the queued prompt at `index` as a steer on the run in
    /// flight.
    fn steer_row(&mut self, index: usize, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _| {
            let _ = store.steer_queued(index);
        });
        cx.notify();
    }

    /// Fills the session draft with the queued prompt and removes it
    /// from the queue.
    fn edit_row(&mut self, row: &QueueRow, cx: &mut Context<Self>) {
        let text = row.full.clone();
        self.store.update(cx, |store, _| {
            let session = store.active_id().map(str::to_owned);
            store.set_draft(session.as_deref(), &text);
            store.withdraw_queued(row.index);
        });
        cx.notify();
    }

    /// Removes the queued prompt at `index`.
    fn remove_row(&mut self, index: usize, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _| {
            store.withdraw_queued(index);
        });
        cx.notify();
    }

    /// The goal pill and its popover, in its active or met color.
    fn goal_pill(&self, goal: &GoalState, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let this = cx.entity();
        let goal_input = self.goal_input.clone();
        let text = goal.text.clone();
        let trigger = Button::new("dock-goal-pill")
            .label(if goal.met { "goal met" } else { "goal" })
            .xsmall()
            .tooltip("the session goal; click to edit or clear it");
        let trigger = if goal.met {
            trigger.success()
        } else {
            trigger.secondary()
        };
        Popover::new("dock-goal")
            .trigger(trigger)
            .anchor(Anchor::BottomLeft)
            .open(self.goal_open)
            .on_open_change({
                let text = text.clone();
                let goal_input = goal_input.clone();
                let this = this.clone();
                move |open, window, cx| {
                    if *open {
                        goal_input.update(cx, |state, cx| {
                            state.set_value(text.as_str(), window, cx);
                        });
                    }
                    this.update(cx, |this, cx| {
                        this.goal_open = *open;
                        cx.notify();
                    });
                }
            })
            .content(move |_, _, _| {
                v_flex()
                    .w(px(300.))
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(text.clone())),
                    )
                    .child(Input::new(&goal_input).flex_1())
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("dock-goal-save")
                                    .label("Save")
                                    .xsmall()
                                    .on_click({
                                        let this = this.clone();
                                        let goal_input = goal_input.clone();
                                        move |_, _, cx| {
                                            let value =
                                                goal_input.read(cx).value().trim().to_owned();
                                            this.update(cx, |this, cx| {
                                                this.save_goal(&value, cx);
                                            });
                                        }
                                    }),
                            )
                            .child(
                                Button::new("dock-goal-clear")
                                    .label("Clear")
                                    .xsmall()
                                    .ghost()
                                    .on_click({
                                        let this = this.clone();
                                        move |_, _, cx| {
                                            this.update(cx, |this, cx| this.save_goal("", cx));
                                        }
                                    }),
                            ),
                    )
            })
            .into_any_element()
    }

    /// The plan review pill with its three answers and the scroll
    /// request on the pill itself.
    fn plan_pill(&self, review: &PlanReviewState, cx: &Context<Self>) -> AnyElement {
        let this = cx.entity();
        let mut row = h_flex().gap_1().items_center().child(
            Button::new("dock-plan-pill")
                .label("plan review")
                .xsmall()
                .warning()
                .tooltip("a plan waits for review; click to find it in the transcript")
                .on_click({
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |this, cx| this.request_scroll_to_plan(cx));
                    }
                }),
        );
        let approve = Button::new("dock-plan-approve").label("Approve").xsmall();
        row = row.child(match &review.approve {
            Some(_) => approve.on_click({
                let this = this.clone();
                move |_, _, cx| {
                    this.update(cx, |this, cx| this.approve_plan(cx));
                }
            }),
            None => approve
                .disabled(true)
                .tooltip("the ask offers no approve option"),
        });
        let revise = Button::new("dock-plan-revise").label("Revise").xsmall();
        row = row.child(match &review.revise {
            Some(_) => revise.on_click({
                let this = this.clone();
                move |_, window, cx| {
                    this.update(cx, |this, cx| this.open_revise(window, cx));
                }
            }),
            None => revise
                .disabled(true)
                .tooltip("the ask offers no revise option"),
        });
        let reject = Button::new("dock-plan-reject").label("Reject").xsmall();
        row = row.child(match &review.reject {
            Some(_) => reject.on_click({
                let this = this.clone();
                move |_, _, cx| {
                    this.update(cx, |this, cx| this.reject_plan(cx));
                }
            }),
            None => reject
                .disabled(true)
                .tooltip("the ask offers no reject option"),
        });
        row.into_any_element()
    }

    /// The swarm pill, with paused members marked in the label.
    fn swarm_pill(&self, swarm: &SwarmState, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let mut label = format!("swarm {}/{}", swarm.done, swarm.total);
        if swarm.paused > 0 {
            label.push_str(&format!(" \u{b7} {} paused", swarm.paused));
        }
        let trigger = Button::new("dock-swarm-pill")
            .label(SharedString::from(label))
            .xsmall()
            .tooltip("delegated members and their states");
        let trigger = if swarm.paused > 0 {
            trigger.warning()
        } else {
            trigger.secondary()
        };
        let members = swarm.members.clone();
        Popover::new("dock-swarm")
            .trigger(trigger)
            .anchor(Anchor::BottomLeft)
            .content(move |_, _, _| {
                let mut list = v_flex().w(px(240.)).gap_1();
                for (name, word) in &members {
                    let color = match *word {
                        "paused" => theme.warning,
                        "failed" => theme.danger,
                        "done" => theme.success,
                        _ => theme.muted_foreground,
                    };
                    list = list.child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(12.))
                                    .truncate()
                                    .child(SharedString::from(name.clone())),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(color)
                                    .child(SharedString::from(*word)),
                            ),
                    );
                }
                list
            })
            .into_any_element()
    }

    /// The todos pill with done/total and its mini bar.
    fn todos_pill(&self, todos: &TodosState, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let fill = TODO_BAR * todos.done as f32 / todos.total.max(1) as f32;
        Button::new("dock-todos-pill")
            .label(SharedString::from(format!(
                "todos {}/{}",
                todos.done, todos.total
            )))
            .xsmall()
            .tooltip("the plan's todos, done of total")
            .child(
                div()
                    .w(px(TODO_BAR))
                    .h(px(4.))
                    .rounded_full()
                    .bg(theme.border)
                    .child(div().w(px(fill)).h_full().rounded_full().bg(theme.primary)),
            )
            .into_any_element()
    }

    /// One queue row with its three actions. `steer_tip` disables the
    /// steer button with the reason when a steer cannot go out.
    fn queue_row(
        &self,
        row: &QueueRow,
        steer_tip: Option<&'static str>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().colors;
        let this = cx.entity();
        let index = row.index;
        let mut unit = h_flex()
            .id(SharedString::from(format!("dock-queue-row-{index}")))
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .rounded(px(4.))
            .border_1()
            .border_color(theme.border)
            .hover(|line| line.bg(theme.list_hover))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child("queued"),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.))
                    .truncate()
                    .child(SharedString::from(row.text.clone())),
            );
        let steer_button = Button::new(SharedString::from(format!("dock-queue-steer-{index}")))
            .label("Steer now")
            .xsmall()
            .ghost();
        unit = unit.child(match steer_tip {
            Some(why) => steer_button.disabled(true).tooltip(why),
            None => steer_button.on_click({
                let this = this.clone();
                move |_, _, cx| {
                    this.update(cx, |this, cx| this.steer_row(index, cx));
                }
            }),
        });
        unit = unit.child(
            Button::new(SharedString::from(format!("dock-queue-edit-{index}")))
                .label("Edit")
                .xsmall()
                .ghost()
                .tooltip("fills the composer draft and leaves the queue")
                .on_click({
                    let row = row.clone();
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |this, cx| this.edit_row(&row, cx));
                    }
                }),
        );
        unit = unit.child(
            Button::new(SharedString::from(format!("dock-queue-remove-{index}")))
                .label("Remove")
                .xsmall()
                .ghost()
                .on_click({
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |this, cx| this.remove_row(index, cx));
                    }
                }),
        );
        unit.into_any_element()
    }

    /// The inline revise field the Revise action opens.
    fn revise_field(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let this = cx.entity();
        let revise_input = self.revise_input.clone();
        h_flex()
            .id("dock-revise")
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .rounded(px(4.))
            .border_1()
            .border_color(theme.warning)
            .child(Input::new(&revise_input).flex_1())
            .child(
                Button::new("dock-revise-send")
                    .label("Send")
                    .xsmall()
                    .on_click({
                        let this = this.clone();
                        move |_, _, cx| {
                            this.update(cx, |this, cx| this.send_revise(cx));
                        }
                    }),
            )
            .child(
                Button::new("dock-revise-cancel")
                    .label("Cancel")
                    .xsmall()
                    .ghost()
                    .on_click({
                        let this = this.clone();
                        move |_, _, cx| {
                            this.update(cx, |this, cx| this.close_revise(cx));
                        }
                    }),
            )
            .into_any_element()
    }
}

impl Render for DockRow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (goal, review, swarm, todos, queue) = {
            let session = self.store.read(cx).active_session();
            (
                session.and_then(goal_state),
                session.and_then(plan_review),
                session.and_then(swarm_state),
                session.and_then(todos_state),
                session.map(queue_rows).unwrap_or_default(),
            )
        };
        let steer_tip: Option<&'static str> = {
            let store = self.store.read(cx);
            if !store.state().steer_available() {
                Some("steering not advertised by the agent")
            } else if !store
                .active_session()
                .is_some_and(|session| session.running)
            {
                Some("nothing is running to steer into")
            } else {
                None
            }
        };

        let mut dock = v_flex().w_full().px_2().pt_1().gap_1();
        if goal.is_some() || review.is_some() || swarm.is_some() || todos.is_some() {
            let mut pills = h_flex().w_full().flex_wrap().gap_1();
            if let Some(goal) = &goal {
                pills = pills.child(self.goal_pill(goal, cx));
            }
            if let Some(review) = &review {
                pills = pills.child(self.plan_pill(review, cx));
            }
            if let Some(swarm) = &swarm {
                pills = pills.child(self.swarm_pill(swarm, cx));
            }
            if let Some(todos) = &todos {
                pills = pills.child(self.todos_pill(todos, cx));
            }
            dock = dock.child(pills);
        }
        if self.revise_open {
            dock = dock.child(self.revise_field(cx));
        }
        for row in &queue {
            dock = dock.child(self.queue_row(row, steer_tip, cx));
        }
        dock
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext, Window};
    use serde_json::Value;

    use super::{
        DockEvent, DockRow, GoalState, PlanReviewState, QueueRow, SwarmState, TodosState,
        goal_state, plan_review, prompt_text, queue_rows, swarm_state, todos_state, truncate_text,
    };
    use crate::store::{Command, Store};
    use crate::transport::State;
    use kage_client::wire::{
        ContentBlock, NoticeTone, SessionConfigKind, SessionConfigOption, SubagentState,
        ToolCallUpdate,
    };
    use kage_client::{Frame, PermissionAsk, Session, Subagent, TranscriptItem};

    /// An initialize answer with everything the gate accepts,
    /// steering advertised.
    fn init_answer() -> Frame {
        Frame::Success {
            id: 1,
            result: serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "steer": true,
                    "sessionCapabilities": {"close": {}},
                },
                "agentInfo": {"name": "kage", "version": "0.1.0"},
            }),
        }
    }

    /// Carries out `commands` the way the shell does.
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

    /// A store with one open, empty session.
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

    /// A session update frame for `s1`.
    fn update(params: Value) -> Frame {
        Frame::Notification {
            method: "session/update".to_owned(),
            params: serde_json::json!({"sessionId": "s1", "update": params}),
        }
    }

    /// A config option update carrying the goal option.
    fn goal_option(value: &str) -> Frame {
        update(serde_json::json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [{
                "id": "goal", "name": "Goal", "type": "text",
                "currentValue": value, "options": [],
            }],
        }))
    }

    /// A notice update in `tone`.
    fn notice(tone: &str, text: &str) -> Frame {
        update(serde_json::json!({
            "sessionUpdate": "_kage/notice", "tone": tone, "text": text,
        }))
    }

    /// A plan update with the given entries.
    fn plan(entries: Value) -> Frame {
        update(serde_json::json!({"sessionUpdate": "plan", "entries": entries}))
    }

    /// A subagent update announcing `id` in `state`.
    fn subagent_frame(id: &str, state: SubagentState) -> Frame {
        update(serde_json::json!({
            "sessionUpdate": "subagent_update",
            "subagentSessionId": id,
            "name": format!("member-{id}"),
            "state": state,
        }))
    }

    /// A plan mode review ask with the three answers the agent
    /// offers.
    fn review_ask(id: u64) -> Frame {
        Frame::Request {
            id,
            method: "session/request_permission".to_owned(),
            params: serde_json::json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "call_p", "title": "exit_plan"},
                "options": [
                    {"optionId": "approve", "name": "Approve", "kind": "allow_once"},
                    {"optionId": "revise", "name": "Revise", "kind": "reject_once"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                ],
            }),
        }
    }

    /// A permission ask naming `title` with the offered options.
    fn ask_of(id: u64, title: &str, options: &[(&str, &str)]) -> PermissionAsk {
        PermissionAsk {
            request_id: id,
            tool_call: ToolCallUpdate {
                tool_call_id: format!("call_{title}"),
                title: Some(title.to_owned()),
                ..ToolCallUpdate::default()
            },
            options: options
                .iter()
                .map(|(option_id, kind)| kage_client::wire::PermissionOption {
                    option_id: (*option_id).to_owned(),
                    name: option_id.to_uppercase(),
                    kind: serde_json::from_value(serde_json::json!(kind)).expect("a known kind"),
                })
                .collect(),
        }
    }

    /// A subagent record in `state`.
    fn agent_with(state: Option<SubagentState>) -> Subagent {
        Subagent {
            state,
            ..Subagent::default()
        }
    }

    /// The outgoing frames drained, requests kept as method and
    /// params, anything else a test failure.
    fn drain_requests(
        store: &Entity<Store>,
        visual: &mut VisualTestContext,
    ) -> Vec<(String, Value)> {
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store
                    .take_outgoing()
                    .into_iter()
                    .map(|frame| match frame {
                        Frame::Request { method, params, .. } => (method, params),
                        other => panic!("unexpected outgoing frame: {other:?}"),
                    })
                    .collect()
            })
        })
    }

    /// The permission replies drained, anything else a test failure.
    fn drain_replies(store: &Entity<Store>, visual: &mut VisualTestContext) -> Vec<(u64, Value)> {
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store
                    .take_outgoing()
                    .into_iter()
                    .map(|frame| match frame {
                        Frame::Success { id, result } => (id, result),
                        other => panic!("unexpected outgoing frame: {other:?}"),
                    })
                    .collect()
            })
        })
    }

    /// A window on a dock row over `store`.
    fn window_on(
        cx: &mut TestAppContext,
        store: Entity<Store>,
    ) -> (Entity<DockRow>, &mut VisualTestContext) {
        cx.update(gpui_kit::init);
        cx.add_window_view(|window: &mut Window, cx| DockRow::new(store.clone(), window, cx))
    }

    #[test]
    fn the_goal_pill_needs_a_non_empty_option_and_flips_to_met() {
        let mut session = Session::new("s1");
        assert!(goal_state(&session).is_none(), "no option, no pill");

        session.config_options.push(SessionConfigOption {
            id: "goal".into(),
            name: "Goal".into(),
            kind: SessionConfigKind::Text,
            current_value: "ship the release".into(),
            options: Vec::new(),
            description: None,
            category: None,
        });
        assert_eq!(
            goal_state(&session),
            Some(GoalState {
                text: "ship the release".into(),
                met: false,
            })
        );

        session.items.push(TranscriptItem::Notice {
            tone: NoticeTone::Info,
            text: "goal met: shipped".into(),
        });
        assert!(
            !goal_state(&session).unwrap().met,
            "only a success notice met the goal"
        );

        session.items.push(TranscriptItem::Notice {
            tone: NoticeTone::Success,
            text: "goal reached, well done".into(),
        });
        assert!(
            !goal_state(&session).unwrap().met,
            "a success notice without the prefix is not the met mark"
        );

        session.items.push(TranscriptItem::Notice {
            tone: NoticeTone::Success,
            text: "goal met: shipped".into(),
        });
        assert!(goal_state(&session).unwrap().met);

        session.config_options[0].current_value = String::new();
        assert!(
            goal_state(&session).is_none(),
            "an empty option hides the pill again"
        );
    }

    #[test]
    fn queue_text_joins_blocks_and_truncates_with_an_ellipsis() {
        let prompt = vec![
            ContentBlock::text("first part"),
            ContentBlock::text("second part"),
        ];
        assert_eq!(prompt_text(&prompt), "first part second part");
        assert_eq!(truncate_text("short", 80), "short");
        let cut = truncate_text(&"x".repeat(81), 80);
        assert_eq!(cut.chars().count(), 81);
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn swarm_counts_come_from_member_states_and_hide_when_all_end() {
        let mut session = Session::new("s1");
        assert!(swarm_state(&session).is_none(), "no members, no pill");

        for (id, state) in [
            ("a", SubagentState::Running),
            ("b", SubagentState::Paused),
            ("c", SubagentState::Completed),
        ] {
            session.agents.insert(id.into(), agent_with(Some(state)));
        }
        session.agents.insert("d".into(), agent_with(None));
        assert_eq!(
            swarm_state(&session),
            Some(SwarmState {
                done: 1,
                total: 4,
                paused: 1,
                members: vec![
                    ("a".into(), "running"),
                    ("b".into(), "paused"),
                    ("c".into(), "done"),
                    ("d".into(), "running"),
                ],
            }),
            "a member without a state is running"
        );

        for (id, state) in [
            ("a", SubagentState::Failed),
            ("b", SubagentState::Completed),
            ("c", SubagentState::Completed),
            ("d", SubagentState::Cancelled),
        ] {
            session.agents.insert(id.into(), agent_with(Some(state)));
        }
        assert_eq!(
            swarm_state(&session).map(|swarm| (swarm.done, swarm.total, swarm.paused)),
            None,
            "no running or paused member, no pill"
        );
    }

    #[test]
    fn todos_count_from_the_latest_plan_only() {
        let mut session = Session::new("s1");
        assert!(todos_state(&session).is_none());
        session.items.push(TranscriptItem::Plan {
            entries: vec![
                serde_json::json!({"content": "one", "status": "completed"}),
                serde_json::json!({"content": "two", "status": "in_progress"}),
                serde_json::json!({"content": "three", "status": "pending"}),
            ],
        });
        assert_eq!(
            todos_state(&session),
            Some(TodosState { done: 1, total: 3 })
        );
        session.items.push(TranscriptItem::Plan {
            entries: vec![
                serde_json::json!({"content": "one", "status": "completed"}),
                serde_json::json!({"content": "two", "status": "completed"}),
            ],
        });
        assert_eq!(
            todos_state(&session),
            Some(TodosState { done: 2, total: 2 }),
            "the latest plan replaces the older one"
        );
        session.items.push(TranscriptItem::Plan { entries: vec![] });
        assert!(todos_state(&session).is_none(), "no entries, no pill");
    }

    #[test]
    fn the_review_state_names_the_offered_options_of_the_exit_plan_ask() {
        let mut session = Session::new("s1");
        assert!(plan_review(&session).is_none());
        session.permissions.push(ask_of(
            9,
            "shell",
            &[("yes", "allow_once"), ("no", "reject_once")],
        ));
        assert!(
            plan_review(&session).is_none(),
            "an ordinary ask is not a plan review"
        );
        session.permissions.push(ask_of(
            7,
            "exit_plan",
            &[
                ("approve", "allow_once"),
                ("revise", "reject_once"),
                ("reject", "reject_once"),
            ],
        ));
        assert_eq!(
            plan_review(&session),
            Some(PlanReviewState {
                request_id: 7,
                approve: Some("approve".into()),
                revise: Some("revise".into()),
                reject: Some("reject".into()),
            })
        );
    }

    #[gpui_kit::test]
    fn goal_edit_and_clear_send_the_option_frames(cx: &mut TestAppContext) {
        let store = cx.new(|_| booted_store());
        let (dock, visual) = window_on(cx, store.clone());
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store.absorb(goal_option("ship the release"));
            });
            let goal = store
                .read(cx)
                .active_session()
                .and_then(super::goal_state)
                .expect("the option set a goal");
            assert_eq!(goal.text, "ship the release");
        });

        visual.update(|_, cx| {
            dock.update(cx, |dock, cx| dock.save_goal("ship it faster", cx));
        });
        let frames = drain_requests(&store, visual);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, "session/set_config_option");
        assert_eq!(frames[0].1["configId"], "goal");
        assert_eq!(frames[0].1["value"], "ship it faster");

        visual.update(|_, cx| {
            dock.update(cx, |dock, cx| dock.save_goal("", cx));
        });
        let frames = drain_requests(&store, visual);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1["value"], "", "clear saves the empty value");
    }

    #[gpui_kit::test]
    fn the_goal_notice_flips_the_derived_state_and_the_pill_draws(cx: &mut TestAppContext) {
        let store = cx.new(|_| booted_store());
        let (_dock, visual) = window_on(cx, store.clone());
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store.absorb(goal_option("ship the release"));
                assert!(
                    store
                        .active_session()
                        .and_then(super::goal_state)
                        .is_some_and(|goal| !goal.met)
                );
                store.absorb(notice("success", "goal met: shipped"));
            });
            assert!(
                store
                    .read(cx)
                    .active_session()
                    .and_then(super::goal_state)
                    .is_some_and(|goal| goal.met)
            );
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));
    }

    #[gpui_kit::test]
    fn plan_answers_send_exactly_the_offered_option_ids(cx: &mut TestAppContext) {
        let store = cx.new(|_| booted_store());
        let (dock, visual) = window_on(cx, store.clone());
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                assert!(
                    store.submit("plan it").is_some(),
                    "the run starts and stays in flight under the ask"
                );
                store.absorb(review_ask(7));
                let _ = store.take_outgoing();
            });
            let review = store
                .read(cx)
                .active_session()
                .and_then(super::plan_review)
                .expect("the ask opens the review");
            assert_eq!(review.request_id, 7);
            assert_eq!(review.approve.as_deref(), Some("approve"));
            assert_eq!(review.revise.as_deref(), Some("revise"));
            assert_eq!(review.reject.as_deref(), Some("reject"));
        });

        visual.update(|_, cx| {
            dock.update(cx, |dock, cx| dock.approve_plan(cx));
        });
        let replies = drain_replies(&store, visual);
        assert_eq!(
            replies,
            vec![(
                7,
                serde_json::json!({
                    "outcome": {"outcome": "selected", "optionId": "approve"},
                })
            )]
        );
        visual.update(|_, cx| {
            assert!(
                store
                    .read(cx)
                    .active_session()
                    .unwrap()
                    .permissions
                    .is_empty()
            );
        });

        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store.absorb(review_ask(8));
                assert!(
                    store
                        .active_session()
                        .and_then(super::plan_review)
                        .is_some()
                );
            });
            dock.update(cx, |dock, cx| dock.reject_plan(cx));
        });
        let replies = drain_replies(&store, visual);
        assert_eq!(replies.len(), 1);
        assert_eq!(
            replies[0].0, 8,
            "the decision answers the request id the ask carried"
        );
        assert_eq!(
            replies[0].1["outcome"]["optionId"], "reject",
            "the reject decision picks the offered reject option"
        );
    }

    #[gpui_kit::test]
    fn revise_answers_with_the_revise_option_and_delivers_the_text(cx: &mut TestAppContext) {
        let store = cx.new(|_| booted_store());
        let (dock, visual) = window_on(cx, store.clone());
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                assert!(store.submit("plan it").is_some());
                store.absorb(review_ask(7));
                let _ = store.take_outgoing();
            });
        });
        visual.update(|window, cx| {
            dock.update(cx, |dock, cx| {
                dock.open_revise(window, cx);
                dock.revise_input.update(cx, |state, cx| {
                    state.set_value("cover the tests too", window, cx);
                });
            });
            assert!(dock.read(cx).revise_open, "the field stays open to type");
        });
        visual.update(|_, cx| {
            dock.update(cx, |dock, cx| dock.send_revise(cx));
            assert!(!dock.read(cx).revise_open, "a sent revise closes the field");
            let session = store.read(cx).active_session().unwrap();
            assert!(
                session.permissions.is_empty(),
                "the revise decision answered the ask"
            );
        });
        let replies = drain_replies(&store, visual);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].0, 7);
        assert_eq!(
            replies[0].1["outcome"]["optionId"], "revise",
            "the revise decision picks the offered revise option"
        );
        assert_eq!(
            replies[0].1["_meta"]["kage"]["planReview"]["revision"], "cover the tests too",
            "the typed text rides the feedback channel"
        );
    }

    #[gpui_kit::test]
    fn the_plan_pill_emits_the_scroll_request(cx: &mut TestAppContext) {
        let store = cx.new(|_| booted_store());
        let (dock, visual) = window_on(cx, store.clone());
        let seen: Rc<RefCell<Vec<DockEvent>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        visual.update(|_, cx| {
            cx.subscribe(&dock, move |_, event: &DockEvent, _| {
                sink.borrow_mut().push(*event);
            })
            .detach();
        });
        visual.update(|window, cx| {
            store.update(cx, |store, _| {
                store.absorb(review_ask(7));
            });
            window.draw(cx).clear(cx);
        });
        assert!(seen.borrow().is_empty());
        visual.update(|_, cx| {
            dock.update(cx, |dock, cx| dock.request_scroll_to_plan(cx));
        });
        assert_eq!(*seen.borrow(), vec![DockEvent::ScrollToPlan]);
    }

    #[gpui_kit::test]
    fn queue_actions_steer_edit_and_remove_the_rows(cx: &mut TestAppContext) {
        let store = cx.new(|_| booted_store());
        let (dock, visual) = window_on(cx, store.clone());
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                assert!(store.submit("first").is_some(), "the run starts");
                let _ = store.submit("second");
                let _ = store.submit("a third prompt that is held");
                let _ = store.take_outgoing();
            });
            assert_eq!(
                queue_rows(store.read(cx).active_session().unwrap()),
                vec![
                    QueueRow {
                        index: 0,
                        text: "second".into(),
                        full: "second".into(),
                    },
                    QueueRow {
                        index: 1,
                        text: "a third prompt that is held".into(),
                        full: "a third prompt that is held".into(),
                    },
                ]
            );
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));

        visual.update(|_, cx| {
            dock.update(cx, |dock, cx| dock.steer_row(0, cx));
            let frames = store.update(cx, |store, _| store.take_outgoing());
            assert_eq!(frames.len(), 1, "steer now sends one prompt frame");
            match &frames[0] {
                Frame::Request { method, params, .. } => {
                    assert_eq!(method, "session/prompt");
                    assert_eq!(params["delivery"], "steer");
                    assert_eq!(params["prompt"][0]["text"], "second");
                }
                other => panic!("expected the steer, got {other:?}"),
            }
            let session = store.read(cx).active_session().unwrap();
            assert_eq!(session.queue.len(), 1, "the steered row left the queue");
        });

        visual.update(|_, cx| {
            dock.update(cx, |dock, cx| {
                let row = queue_rows(store.read(cx).active_session().unwrap()).remove(0);
                dock.edit_row(&row, cx);
            });
            let session = store.read(cx).active_session().unwrap();
            assert!(session.queue.is_empty(), "the edited row left the queue");
            assert_eq!(
                store.read(cx).draft("s1"),
                Some("a third prompt that is held"),
                "edit fills the draft the composer loads"
            );
        });

        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                let _ = store.submit("remove me");
                let _ = store.take_outgoing();
            });
            dock.update(cx, |dock, cx| dock.remove_row(0, cx));
            assert!(
                store
                    .update(cx, |store, _| store.take_outgoing())
                    .is_empty(),
                "withdrawing sends no frame"
            );
            assert!(store.read(cx).active_session().unwrap().queue.is_empty());
        });
    }

    #[gpui_kit::test]
    fn every_pill_and_row_draws_from_the_delivered_state(cx: &mut TestAppContext) {
        let store = cx.new(|_| booted_store());
        let (_dock, visual) = window_on(cx, store.clone());
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store.absorb(goal_option("ship the release"));
                store.absorb(subagent_frame("a", SubagentState::Running));
                store.absorb(subagent_frame("b", SubagentState::Paused));
                store.absorb(subagent_frame("c", SubagentState::Completed));
                store.absorb(plan(serde_json::json!([
                    {"content": "one", "status": "completed"},
                    {"content": "two", "status": "pending"},
                ])));
                assert!(store.submit("plan it").is_some());
                store.absorb(review_ask(7));
                let _ = store.submit("queued work");
            });
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.update(|_, cx| {
            let session = store.read(cx).active_session().unwrap();
            assert!(goal_state(session).is_some());
            assert!(plan_review(session).is_some());
            assert_eq!(swarm_state(session).unwrap().paused, 1);
            assert_eq!(todos_state(session).unwrap().total, 2);
            assert_eq!(queue_rows(session).len(), 1);
        });
    }
}
