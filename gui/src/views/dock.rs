//! The dock row above the composer: the goal pill with its popover,
//! the plan pill that leads to the plan card under review, the running
//! swarm, background agents and todos pills, and the queued prompt
//! rows.
//!
//! Every pill derives its state from the active session the store
//! holds, so the dock counts only what frames delivered and hides
//! what is incomputable. The approval card is a separate view the shell
//! mounts beside this row, and the plan review itself is answered on
//! its card in the transcript.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::base::Selectable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AnyElement, AppContext as _, Context, Div, Entity, EventEmitter, FontWeight,
    InteractiveElement, Interactivity, IntoElement, ParentElement, Render, SharedString, Stateful,
    StatefulInteractiveElement, StyleRefinement, Styled, Window, div, px,
};
use kage_client::wire::{ContentBlock, NoticeTone, SubagentState};
use kage_client::{Session, TranscriptItem};
use serde_json::Value;

use crate::store::{Store, StoreHandle as _, plan_review};
use crate::theme::{
    CTL_ICO, FONT_MONO, FS_2XS, FS_BASE, FS_SM, FS_XS, Palette, R_FULL, R_LG, R_SM, SP_3, SP_4,
    SP_5,
};
use crate::views::deferred::Deferred;
use gpui_kit::base::ElementExt as _;

/// The config option id the goal pill reads and edits.
const GOAL_OPTION: &str = "goal";
/// The prefix of the success notice that reports a met goal.
const GOAL_MET_PREFIX: &str = "goal met: ";
/// Characters a queue row shows before the ellipsis.
const QUEUE_TEXT_COLUMNS: usize = 80;
/// The width of the todos mini bar, in pixels.
const TODO_BAR: f32 = 36.0;

/// A trigger that hosts a popover while styled as a dock pill. The
/// popover machinery asks its trigger for the selectable contract;
/// the pill keeps its open-state styling on the view's own flag
/// instead.
struct Pill {
    element: Stateful<Div>,
}

impl Pill {
    /// Wraps an id-carrying stateful div as a popover trigger.
    fn new(id: &'static str) -> Self {
        Self {
            element: div().id(id).flex(),
        }
    }
}

impl Selectable for Pill {
    fn selected(self, _: bool) -> Self {
        self
    }

    fn is_selected(&self) -> bool {
        false
    }
}

impl Styled for Pill {
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl InteractiveElement for Pill {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.element.interactivity()
    }
}

impl StatefulInteractiveElement for Pill {}

impl ParentElement for Pill {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.element.extend(elements);
    }
}

impl IntoElement for Pill {
    type Element = Stateful<Div>;

    fn into_element(self) -> Self::Element {
        self.element
    }
}

/// The pill shape the dock pills share: 28px tall, fully round, a
/// hairline border over the surface, muted 12px text that lifts onto
/// the raised fill, inks and takes the stronger border while hovered
/// or open.
fn pill<E: InteractiveElement + Styled>(el: E, pal: &Palette) -> E {
    let (raised, ink, line_strong) = (pal.raised, pal.ink, pal.line_strong);
    el.flex()
        .h(px(28.))
        .px(px(10.))
        .gap(px(SP_3))
        .flex_none()
        .max_w(px(320.))
        .items_center()
        .rounded(px(R_FULL))
        .border_1()
        .border_color(pal.line)
        .bg(pal.surface)
        .text_size(px(FS_XS))
        .text_color(pal.muted)
        .hover(move |style| style.bg(raised).border_color(line_strong).text_color(ink))
}

/// The mono count a pill carries after its label.
fn pill_count(text: String, pal: &Palette) -> Div {
    div()
        .flex_none()
        .font_family(FONT_MONO)
        .text_color(pal.ink_strong)
        .child(SharedString::from(text))
}

/// The todos mini bar: a 36 by 4 track with the done share in the
/// success green.
fn mini_bar(done: usize, total: usize, pal: &Palette) -> Div {
    let fill = TODO_BAR * done as f32 / total.max(1) as f32;
    div()
        .w(px(TODO_BAR))
        .h(px(4.))
        .flex_none()
        .rounded(px(2.))
        .bg(pal.fill_hover)
        .overflow_hidden()
        .child(div().w(px(fill)).h_full().bg(pal.ok))
}

/// One 26px icon button as the queue rows carry: a muted glyph that
/// inks over the hover fill.
fn icon_btn<E: InteractiveElement + ParentElement + Styled>(
    el: E,
    icon: IconName,
    pal: &Palette,
) -> E {
    let (hover, ink) = (pal.hover, pal.ink);
    el.size(px(CTL_ICO))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(R_SM))
        .text_color(pal.muted)
        .hover(move |style| style.bg(hover).text_color(ink))
        .child(Icon::new(icon).with_size(px(12.)))
}

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

/// The background agents pill's state: how many of the session's
/// background agents run, and how many it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackgroundState {
    /// Running or paused.
    pub live: usize,
    pub total: usize,
}

/// The background agents of a session, while it has any.
#[must_use]
pub(crate) fn background_state(session: &Session) -> Option<BackgroundState> {
    let background: Vec<_> = session
        .agents
        .values()
        .filter(|agent| agent.background)
        .collect();
    (!background.is_empty()).then(|| BackgroundState {
        live: background
            .iter()
            .filter(|agent| {
                matches!(
                    agent.state,
                    None | Some(SubagentState::Running | SubagentState::Paused)
                )
            })
            .count(),
        total: background.len(),
    })
}

/// The swarm state of a session, while it still has running or paused
/// members. A member never given a state is running. Background agents
/// have a pill of their own.
#[must_use]
pub(crate) fn swarm_state(session: &Session) -> Option<SwarmState> {
    let foreground: Vec<_> = session
        .agents
        .iter()
        .filter(|(_, agent)| !agent.background)
        .collect();
    let total = foreground.len();
    if total == 0 {
        return None;
    }
    let members: Vec<(String, &'static str)> = foreground
        .into_iter()
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

/// The latest plan's entries as text and status, in plan order.
#[must_use]
pub(crate) fn todo_entries(session: &Session) -> Vec<(String, String)> {
    session
        .items
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::Plan { entries } => Some(entries),
            _ => None,
        })
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    let text = entry
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let status = entry
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("pending");
                    (text.to_owned(), status.to_owned())
                })
                .collect()
        })
        .unwrap_or_default()
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
    /// Show the background agents in the workbench's agents list.
    OpenBackgroundAgents,
}

/// The pills row the shell mounts above its composer.
pub struct DockRow {
    store: Entity<Store>,
    /// Whether the goal popover shows.
    goal_open: bool,
    /// The goal text field inside the goal popover.
    goal_input: Entity<InputState>,
    /// The goal text, held until the popover's field has been laid out.
    /// The field mounts only when the popover opens, so a write on the
    /// opening frame lands on an element that has never been laid out;
    /// see [`crate::views::deferred`].
    goal_mirror: Deferred,
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
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            goal_open: false,
            goal_input,
            goal_mirror: Deferred::new(),
        }
    }

    /// Saves `text` into the goal option; an empty text clears it.
    fn save_goal(&mut self, text: &str, cx: &mut Context<Self>) {
        self.store
            .act(cx, |store| store.set_option(GOAL_OPTION, text));
        self.goal_open = false;
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
        self.store.act(cx, |store| {
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
        self.store.act(cx, |store| {
            store.withdraw_queued(index);
        });
        cx.notify();
    }

    /// The goal pill and its popover, in its active or met color.
    fn goal_pill(
        &self,
        goal: &GoalState,
        pal: &'static Palette,
        cx: &Context<Self>,
        goal_laid_out: Rc<Cell<bool>>,
    ) -> AnyElement {
        let this = cx.entity();
        let goal_input = self.goal_input.clone();
        let release = cx.entity().downgrade();
        let text = goal.text.clone();
        let met = goal.met;
        let (icon, icon_color) = if met {
            (IconName::CircleCheck, pal.ok)
        } else {
            (IconName::Target, pal.accent)
        };
        let trigger = pill(Pill::new("dock-goal-pill"), pal)
            .tooltip(|window, cx| {
                Tooltip::new("the session goal; click to edit or clear it").build(window, cx)
            })
            .when(self.goal_open, |pill| {
                pill.bg(pal.raised)
                    .border_color(pal.line_strong)
                    .text_color(pal.ink)
            })
            .child(Icon::new(icon).with_size(px(12.)).text_color(icon_color))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(goal.text.clone())),
            );
        Popover::new("dock-goal")
            .trigger(trigger)
            .anchor(Anchor::BottomLeft)
            .open(self.goal_open)
            .on_open_change({
                let text = text.clone();
                let this = this.clone();
                move |open, window, cx| {
                    let text = text.clone();
                    this.update(cx, |this, cx| {
                        this.goal_open = *open;
                        if *open {
                            let goal_input = this.goal_input.clone();
                            this.goal_mirror.set(text, |text| {
                                goal_input.update(cx, |state, cx| {
                                    state.set_value(text, window, cx);
                                });
                            });
                        }
                        cx.notify();
                    });
                }
            })
            .content(move |_, _, _| {
                let goal_laid_out = goal_laid_out.clone();
                let release = release.clone();
                v_flex()
                    .w(px(340.))
                    .p(px(5.))
                    .bg(pal.menu)
                    .border_1()
                    .border_color(pal.line)
                    .rounded(px(R_LG))
                    .shadow(pal.shadow_menu.clone())
                    .child(
                        div()
                            .px(px(9.))
                            .pt(px(6.))
                            .pb(px(3.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(FS_2XS))
                            .text_color(pal.faint)
                            .child(if met { "Goal met" } else { "Goal" }),
                    )
                    .child(
                        div()
                            .px(px(9.))
                            .pt(px(4.))
                            .pb(px(10.))
                            .text_size(px(FS_BASE))
                            .text_color(pal.ink_strong)
                            .child(SharedString::from(text.clone())),
                    )
                    .child(
                        div()
                            .px(px(9.))
                            .pb(px(8.))
                            .text_size(px(FS_XS))
                            .text_color(pal.muted)
                            .child(
                                "The agent re-checks the goal after each turn and keeps \
                                 going until it is met.",
                            ),
                    )
                    .child(div().h(px(1.)).mx(px(2.)).my(px(4.)).bg(pal.subtle))
                    .child(
                        h_flex()
                            .px(px(2.))
                            .pb(px(2.))
                            .gap(px(SP_4))
                            .on_prepaint(move |_, _, cx| {
                                goal_laid_out.set(true);
                                let _ = release.update(cx, |_, cx| cx.notify());
                            })
                            .child(Input::new(&goal_input).flex_1())
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

    /// The plan pill: under review it carries the attention palette
    /// and brings the plan card into view; in plan mode before any plan
    /// it says where the plan will show up.
    fn plan_pill(&self, pending: bool, pal: &'static Palette, cx: &Context<Self>) -> AnyElement {
        let this = cx.entity();
        let pill = h_flex()
            .id("dock-plan-pill")
            .h(px(28.))
            .px(px(10.))
            .gap(px(SP_3))
            .flex_none()
            .max_w(px(320.))
            .items_center()
            .rounded(px(R_FULL))
            .border_1()
            .text_size(px(FS_XS))
            .child(
                Icon::new(IconName::PenLine)
                    .with_size(px(12.))
                    .text_color(pal.accent),
            );
        if !pending {
            return pill
                .border_color(pal.line)
                .bg(pal.surface)
                .text_color(pal.muted)
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child("Plan mode: the plan will show up here"),
                )
                .into_any_element();
        }
        pill.border_color(pal.ok_bd)
            .bg(pal.ok_soft)
            .text_color(pal.ok)
            .cursor_pointer()
            .tooltip(|window, cx| {
                Tooltip::new("a plan waits for review; click to find it in the transcript")
                    .build(window, cx)
            })
            .child(div().min_w_0().truncate().child("Plan: pending review"))
            .on_click(move |_, _, cx| {
                this.update(cx, |this, cx| this.request_scroll_to_plan(cx));
            })
            .into_any_element()
    }

    /// The swarm pill, in the violet swarm color with its mono count,
    /// and the member list popover. The violet stays on through the
    /// hover lift, as the design pins the pill's color inline.
    /// The background agents pill: a spinner and how many run while any
    /// does, else how many there were. A click lists them.
    fn background_pill(
        &self,
        state: BackgroundState,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let noun = |n: usize| if n == 1 { "agent" } else { "agents" };
        let (icon, label) = if state.live > 0 {
            (
                IconName::LoaderCircle,
                format!("{} background {} running", state.live, noun(state.live)),
            )
        } else {
            (
                IconName::Bot,
                format!("{} background {}", state.total, noun(state.total)),
            )
        };
        pill(div().id("dock-background-pill"), pal)
            .cursor_pointer()
            .on_click(cx.listener(|_, _, _, cx| cx.emit(DockEvent::OpenBackgroundAgents)))
            .child(Icon::new(icon).with_size(px(12.)))
            .child(SharedString::from(label))
            .into_any_element()
    }

    fn swarm_pill(&self, swarm: &SwarmState, pal: &'static Palette) -> AnyElement {
        let count = format!("{}/{}", swarm.done, swarm.total);
        let (raised, line_strong) = (pal.raised, pal.line_strong);
        let trigger = Pill::new("dock-swarm-pill")
            .h(px(28.))
            .px(px(10.))
            .gap(px(SP_3))
            .flex_none()
            .max_w(px(320.))
            .items_center()
            .rounded(px(R_FULL))
            .border_1()
            .border_color(pal.line)
            .bg(pal.surface)
            .text_size(px(FS_XS))
            .text_color(pal.done)
            .tooltip(|window, cx| {
                Tooltip::new("delegated members and their states").build(window, cx)
            })
            .hover(move |style| style.bg(raised).border_color(line_strong))
            .child(Icon::new(IconName::Waypoints).with_size(px(12.)))
            .child("Swarm")
            .child(pill_count(count, pal))
            .children(
                (swarm.paused > 0)
                    .then(|| div().child(SharedString::from(format!("{} paused", swarm.paused)))),
            );
        let members = swarm.members.clone();
        Popover::new("dock-swarm")
            .appearance(false)
            .trigger(trigger)
            .anchor(Anchor::BottomLeft)
            .content(move |_, _, _| {
                v_flex()
                    .w(px(240.))
                    .p(px(5.))
                    .bg(pal.menu)
                    .border_1()
                    .border_color(pal.line)
                    .rounded(px(R_LG))
                    .shadow(pal.shadow_menu.clone())
                    .children(members.iter().map(|(name, word)| {
                        let color = match *word {
                            "paused" => pal.warn,
                            "failed" => pal.danger,
                            "done" => pal.ok,
                            _ => pal.muted,
                        };
                        h_flex()
                            .w_full()
                            .px(px(9.))
                            .py(px(4.))
                            .gap(px(SP_4))
                            .items_center()
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_size(px(FS_XS))
                                    .text_color(pal.ink)
                                    .child(SharedString::from(name.clone())),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(FS_2XS))
                                    .text_color(color)
                                    .child(*word),
                            )
                    }))
            })
            .into_any_element()
    }

    /// The todos pill with done/total and its mini bar, opening the
    /// todo list above it.
    fn todos_pill(
        &self,
        todos: &TodosState,
        entries: Vec<(String, String)>,
        pal: &'static Palette,
    ) -> AnyElement {
        let (raised, line_strong) = (pal.raised, pal.line_strong);
        let trigger = Pill::new("dock-todos-pill")
            .h(px(28.))
            .px(px(10.))
            .gap(px(SP_3))
            .flex_none()
            .items_center()
            .rounded(px(R_FULL))
            .border_1()
            .border_color(pal.line)
            .bg(pal.surface)
            .text_size(px(FS_XS))
            .text_color(pal.muted)
            .hover(move |style| style.bg(raised).border_color(line_strong))
            .child(Icon::new(IconName::ListTodo).with_size(px(12.)))
            .child("Progress")
            .child(pill_count(format!("{}/{}", todos.done, todos.total), pal))
            .child(mini_bar(todos.done, todos.total, pal));
        Popover::new("dock-todos")
            .appearance(false)
            .trigger(trigger)
            .anchor(Anchor::BottomLeft)
            .content(move |_, _, _| {
                v_flex()
                    .w(px(340.))
                    .p(px(5.))
                    .bg(pal.menu)
                    .border_1()
                    .border_color(pal.line)
                    .rounded(px(R_LG))
                    .shadow(pal.shadow_menu.clone())
                    .child(
                        div()
                            .px(px(9.))
                            .pt(px(6.))
                            .pb(px(4.))
                            .text_size(px(FS_2XS))
                            .text_color(pal.faint)
                            .child("TODOS"),
                    )
                    .children(entries.iter().map(|(text, status)| {
                        let (icon, color, ink) = match status.as_str() {
                            "completed" => (IconName::CircleCheck, pal.ok, pal.muted),
                            "in_progress" => (IconName::LoaderCircle, pal.accent, pal.ink_strong),
                            _ => (IconName::Circle, pal.faint, pal.ink),
                        };
                        h_flex()
                            .w_full()
                            .px(px(9.))
                            .py(px(5.))
                            .gap(px(SP_4))
                            .items_start()
                            .child(
                                Icon::new(icon)
                                    .mt(px(2.))
                                    .with_size(px(13.))
                                    .text_color(color),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .text_size(px(FS_SM))
                                    .text_color(ink)
                                    .when(status == "completed", |line| line.line_through())
                                    .child(SharedString::from(text.clone())),
                            )
                    }))
            })
            .into_any_element()
    }

    /// One queue row with its three actions. `steer_tip` disables the
    /// steer button with the reason when a steer cannot go out, and
    /// `separator` draws the hairline above every row after the
    /// first.
    fn queue_row(
        &self,
        row: &QueueRow,
        separator: bool,
        steer_tip: Option<&'static str>,
        pal: &'static Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let this = cx.entity();
        let index = row.index;
        let mut unit = h_flex()
            .id(SharedString::from(format!("dock-queue-row-{index}")))
            .when(separator, |line| line.border_t_1().border_color(pal.subtle))
            .w_full()
            .h(px(34.))
            .flex_none()
            .pl(px(SP_5))
            .pr(px(6.))
            .gap(px(SP_4))
            .items_center()
            .text_size(px(FS_SM))
            .child(
                Icon::new(IconName::List)
                    .with_size(px(12.))
                    .text_color(pal.faint),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(FS_2XS))
                    .text_color(pal.faint)
                    .child("Queued"),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_color(pal.ink)
                    .child(SharedString::from(row.text.clone())),
            );
        let steer = icon_btn(
            div().id(SharedString::from(format!("dock-queue-steer-{index}"))),
            IconName::CornerDownLeft,
            pal,
        )
        .tooltip(|window, cx| Tooltip::new("Steer now").build(window, cx));
        unit = unit.child(match steer_tip {
            Some(why) => steer
                .opacity(0.45)
                .cursor_default()
                .tooltip(move |window, cx| Tooltip::new(why).build(window, cx)),
            None => steer.on_click({
                let this = this.clone();
                move |_, _, cx| {
                    this.update(cx, |this, cx| this.steer_row(index, cx));
                }
            }),
        });
        unit = unit.child(
            icon_btn(
                div().id(SharedString::from(format!("dock-queue-edit-{index}"))),
                IconName::Pencil,
                pal,
            )
            .tooltip(|window, cx| {
                Tooltip::new("fills the composer draft and leaves the queue").build(window, cx)
            })
            .on_click({
                let row = row.clone();
                let this = this.clone();
                move |_, _, cx| {
                    this.update(cx, |this, cx| this.edit_row(&row, cx));
                }
            }),
        );
        unit = unit.child(
            icon_btn(
                div().id(SharedString::from(format!("dock-queue-remove-{index}"))),
                IconName::X,
                pal,
            )
            .tooltip(|window, cx| Tooltip::new("Remove").build(window, cx))
            .on_click({
                let this = this.clone();
                move |_, _, cx| {
                    this.update(cx, |this, cx| this.remove_row(index, cx));
                }
            }),
        );
        unit
    }
}

impl Render for DockRow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Text held while these fields were unlaid lands here, on the
        // first render after each field's element prepainted.
        let goal_input = self.goal_input.clone();
        self.goal_mirror.flush(|text| {
            goal_input.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        let goal_laid_out = self.goal_mirror.laid_out().flag();
        let pal = Palette::active(cx);
        let plan_on = self.store.read(cx).plan_on();
        let (goal, review, swarm, background, todos, queue) = {
            let session = self.store.read(cx).active_session();
            (
                session.and_then(goal_state),
                session.and_then(plan_review),
                session.and_then(swarm_state),
                session.and_then(background_state),
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

        let mut dock = v_flex().w_full();
        let pills_on = goal.is_some()
            || review.is_some()
            || plan_on
            || swarm.is_some()
            || background.is_some()
            || todos.is_some();
        if pills_on {
            let mut pills = h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .gap(px(SP_3))
                .mb(px(SP_4));
            if let Some(goal) = &goal {
                pills = pills.child(self.goal_pill(goal, pal, cx, goal_laid_out.clone()));
            }
            if review.is_some() || plan_on {
                pills = pills.child(self.plan_pill(review.is_some(), pal, cx));
            }
            if let Some(swarm) = &swarm {
                pills = pills.child(self.swarm_pill(swarm, pal));
            }
            if let Some(background) = background {
                pills = pills.child(self.background_pill(background, pal, cx));
            }
            if let Some(todos) = &todos {
                let entries = self
                    .store
                    .read(cx)
                    .active_session()
                    .map(todo_entries)
                    .unwrap_or_default();
                pills = pills.child(self.todos_pill(todos, entries, pal));
            }
            dock = dock.child(pills);
        }
        if !queue.is_empty() {
            let mut sheet = v_flex()
                .w_full()
                .mb(px(SP_4))
                .border_1()
                .border_color(pal.line)
                .rounded(px(R_LG))
                .bg(pal.surface)
                .overflow_hidden();
            for (index, row) in queue.iter().enumerate() {
                sheet = sheet.child(self.queue_row(row, index > 0, steer_tip, pal, cx));
            }
            dock = dock.child(sheet);
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
        BackgroundState, DockEvent, DockRow, GoalState, QueueRow, SwarmState, TodosState,
        background_state, goal_state, prompt_text, queue_rows, swarm_state, todos_state,
        truncate_text,
    };
    use crate::store::{Command, PlanChoice, PlanReviewState, Store, plan_review};
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
        let _ = store.take_outgoing();
        store.new_session();
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: 3,
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
            plan: None,
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
    fn background_agents_have_their_own_pill_and_leave_the_swarm() {
        let mut session = Session::new("s1");
        assert!(background_state(&session).is_none());
        for (id, state) in [
            ("a", SubagentState::Running),
            ("b", SubagentState::Completed),
        ] {
            session.agents.insert(
                id.into(),
                Subagent {
                    background: true,
                    ..agent_with(Some(state))
                },
            );
        }
        assert_eq!(
            background_state(&session),
            Some(BackgroundState { live: 1, total: 2 })
        );
        assert!(
            swarm_state(&session).is_none(),
            "background agents are no swarm"
        );
        session
            .agents
            .insert("c".into(), agent_with(Some(SubagentState::Running)));
        assert_eq!(swarm_state(&session).map(|swarm| swarm.total), Some(1));
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
                call_id: "call_exit_plan".into(),
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
        let (_dock, visual) = window_on(cx, store.clone());
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
                .and_then(plan_review)
                .expect("the ask opens the review");
            assert_eq!(review.request_id, 7);
            assert_eq!(review.approve.as_deref(), Some("approve"));
            assert_eq!(review.revise.as_deref(), Some("revise"));
            assert_eq!(review.reject.as_deref(), Some("reject"));
        });

        visual.update(|_, cx| {
            store.update(cx, |store, _| store.review_plan(PlanChoice::Approve, None));
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
                assert!(store.active_session().and_then(plan_review).is_some());
            });
            store.update(cx, |store, _| store.review_plan(PlanChoice::Reject, None));
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
        let (_dock, visual) = window_on(cx, store.clone());
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                assert!(store.submit("plan it").is_some());
                store.absorb(review_ask(7));
                let _ = store.take_outgoing();
            });
        });
        visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store.review_plan(PlanChoice::Revise, Some("cover the tests too"));
            });
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
