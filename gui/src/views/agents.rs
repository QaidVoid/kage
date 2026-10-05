//! The agent and swarm cards an `agent` or `swarm` call renders as.
//!
//! A card reads the subagent records the parent session holds, joined
//! to the call by the `toolCallId` each `subagent_update` names, and the
//! child sessions the engine streams. Every count is engine state:
//! elapsed time is what this client watched or else the run time the
//! engine reported, tokens are what the child reported spending, and a
//! field the wire never carried stays blank.

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Div, ElementId, Entity, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    div, px, relative,
};
use kage_client::agent_text::{self, AgentText, Report};
use kage_client::wire::{NoticeTone, SubagentState};
use kage_client::{Session, Subagent, ToolCallItem, TranscriptItem};

use crate::store::{Store, StoreHandle as _};
use crate::theme::{FONT_MONO, FS_SM, FS_XS, Palette, R_FULL, R_LG, R_MD, SP_4};
use crate::views::constellation::{Star, constellation};
use crate::views::kit::{self, BtnTone};
use crate::views::transcript::{TranscriptEvent, TranscriptView, tool_verb};

/// Where one subagent stands, as the cards label it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Working on its task.
    Running,
    /// Waiting out a rate limit.
    Paused,
    /// Finished its task.
    Done,
    /// Could not finish.
    Failed,
    /// Stopped.
    Cancelled,
}

impl Phase {
    fn of(agent: &Subagent) -> Self {
        match agent.state {
            None | Some(SubagentState::Running) => Self::Running,
            Some(SubagentState::Paused) => Self::Paused,
            Some(SubagentState::Completed) => Self::Done,
            // A state this build cannot name is final; reading it as a
            // failure keeps the member visible instead of silently done.
            Some(SubagentState::Unknown) => Self::Failed,
            Some(SubagentState::Failed) => Self::Failed,
            Some(SubagentState::Cancelled) => Self::Cancelled,
        }
    }

    /// Whether the agent still works or waits to.
    pub(crate) fn live(self) -> bool {
        matches!(self, Self::Running | Self::Paused)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Paused => "Rate limited",
            Self::Done => "Finished",
            Self::Failed => "Error",
            Self::Cancelled => "Cancelled",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Running => IconName::LoaderCircle,
            Self::Paused => IconName::CirclePause,
            Self::Done => IconName::CircleCheck,
            Self::Failed => IconName::CircleX,
            Self::Cancelled => IconName::CircleSlash,
        }
    }

    pub(crate) fn color(self, pal: &Palette) -> Hsla {
        match self {
            Self::Running => pal.accent,
            Self::Paused => pal.warn,
            Self::Done => pal.ok,
            Self::Failed => pal.danger,
            Self::Cancelled => pal.faint,
        }
    }
}

/// What a card shows about one subagent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentFacts {
    /// The child session id.
    pub id: String,
    /// The agent definition's name.
    pub name: String,
    /// The task the model wrote for it.
    pub task: String,
    /// Where it stands.
    pub phase: Phase,
    /// Why it paused, while paused.
    pub reason: Option<String>,
    /// How long it ran, as watched or as the engine reported.
    pub elapsed: Option<Duration>,
    /// The tokens it read and wrote, once it reported any.
    pub tokens: Option<u64>,
    /// What it cost in USD, once priced.
    pub cost: Option<f64>,
    /// The model it runs.
    pub model: Option<String>,
    /// Its latest tool line, while it works.
    pub last: Option<String>,
    /// Its final answer's first line, once it ended.
    pub result: Option<String>,
    /// Whether the agent may be stopped.
    pub stoppable: bool,
    /// The swarm item it works on, and its launch index.
    pub item: Option<(String, u32)>,
    /// Whether it runs in the background.
    pub background: bool,
}

/// The children the call `call_id` started, in launch order.
pub(crate) fn children_of<'a>(
    parent: &'a Session,
    call_id: &str,
) -> Vec<(&'a String, &'a Subagent)> {
    let mut children: Vec<_> = parent
        .agents
        .iter()
        .filter(|(_, agent)| agent.tool_call_id.as_deref() == Some(call_id))
        .collect();
    children.sort_by_key(|(_, agent)| agent.swarm.as_ref().map(|swarm| swarm.index));
    children
}

/// The facts of child `id` of `parent`.
pub(crate) fn agent_facts(
    store: &Store,
    parent: &Session,
    id: &str,
    agent: &Subagent,
) -> AgentFacts {
    let child = store.state().session(id);
    let usage = agent.usage.as_ref();
    let phase = Phase::of(agent);
    let last = child
        .filter(|_| phase.live())
        .and_then(|child| {
            child.items.iter().rev().find_map(|item| match item {
                TranscriptItem::ToolCall(call) => Some(call),
                _ => None,
            })
        })
        .map(|call| {
            let (verb, target) = tool_verb(call);
            format!("{verb} {target}").trim().to_owned()
        });
    // The final answer, or for an agent that never gave one the error
    // it ended on.
    let watched = child.filter(|_| !phase.live()).and_then(|child| {
        child.items.iter().rev().find_map(|item| match item {
            TranscriptItem::Assistant { text } => first_line(text),
            TranscriptItem::Notice {
                tone: NoticeTone::Error,
                text,
            } => first_line(text),
            _ => None,
        })
    });
    let result = watched.or_else(|| {
        agent
            .tool_call_id
            .as_deref()
            .filter(|_| !phase.live())
            .and_then(|call| recorded_reply(parent, call, id))
    });
    AgentFacts {
        id: id.to_owned(),
        name: agent.name.clone().unwrap_or_else(|| "agent".to_owned()),
        task: agent.task.clone().unwrap_or_default(),
        phase,
        reason: agent.reason.clone(),
        elapsed: store
            .timings(&parent.id)
            .and_then(|times| times.agent(id))
            .or_else(|| {
                usage
                    .and_then(|usage| usage.run_ms)
                    .map(Duration::from_millis)
            }),
        tokens: usage
            .map(|usage| usage.input + usage.output)
            .or_else(|| child.map(|child| child.usage.used))
            .filter(|used| *used > 0),
        cost: usage.map(|usage| usage.cost).filter(|cost| *cost > 0.0),
        model: agent.model.clone(),
        last,
        result,
        stoppable: phase.live() && agent.capabilities.is_some_and(|caps| caps.cancel),
        item: agent
            .swarm
            .as_ref()
            .map(|swarm| (swarm.item.clone(), swarm.index)),
        background: agent.background,
    }
}

/// The first line of the reply the parent recorded for child `id`: in
/// the `<agent session="id" ...>` report its call `call_id` returned,
/// or for a background agent in the report that reached the parent
/// later as a message. A card shows it while the child's own
/// transcript is not loaded.
fn recorded_reply(parent: &Session, call_id: &str, id: &str) -> Option<String> {
    let delivered = parent.items.iter().rev().find_map(|item| match item {
        TranscriptItem::User { content, .. } => content.iter().find_map(|block| {
            let text = block.as_text()?;
            agent_text::split(text)?
                .1
                .into_iter()
                .find_map(|part| match part {
                    AgentText::Report(report) if report.session == id => first_line(&report.body),
                    _ => None,
                })
        }),
        _ => None,
    });
    if delivered.is_some() {
        return delivered;
    }
    let text = parent.items.iter().find_map(|item| match item {
        TranscriptItem::ToolCall(call) if call.tool_call_id == call_id => Some(call.text()),
        _ => None,
    })?;
    let at = text.find(&format!("session=\"{id}\""))?;
    let body = &text[at..];
    let body = &body[body.find('>')? + 1..];
    first_line(body.split("</agent>").next()?)
}

/// The card an agent's report renders as when it reaches its parent as
/// a message: who finished and how, its run time and the reply. A click
/// opens the agent.
pub(crate) fn report_card(
    n: usize,
    report: &Report,
    view: &Entity<TranscriptView>,
    cx: &App,
) -> AnyElement {
    const SHOWN: usize = 6;
    let pal = Palette::active(cx);
    let hover = pal.hover;
    let phase = match report.state.as_str() {
        "completed" => Phase::Done,
        "failed" => Phase::Failed,
        "cancelled" => Phase::Cancelled,
        _ => Phase::Running,
    };
    let word = match report.limit.as_deref() {
        Some("turns") => "Turn limit",
        Some("time") => "Timed out",
        Some("budget") => "Over budget",
        _ => phase.label(),
    };
    let open = {
        let view = view.clone();
        let id = report.session.clone();
        move |_: &gpui_kit::ClickEvent, _: &mut gpui_kit::Window, cx: &mut App| {
            let id = id.clone();
            view.update(cx, |_, cx| cx.emit(TranscriptEvent::OpenAgent(id)));
        }
    };
    let lines: Vec<&str> = report.body.lines().collect();
    let mut reply = lines
        .iter()
        .take(SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > SHOWN {
        let more = lines.len() - SHOWN;
        reply.push_str(&format!(
            "\n... {more} more {}",
            if more == 1 { "line" } else { "lines" }
        ));
    }
    v_flex()
        .id(ElementId::named_usize("report", n))
        .w_full()
        .border_1()
        .border_color(pal.line)
        .rounded(px(R_LG))
        .bg(pal.surface)
        .overflow_hidden()
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_click(open)
        .child(
            h_flex()
                .gap(px(10.))
                .items_center()
                .px(px(12.))
                .py(px(SP_4))
                .child(avatar(&report.name, pal))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(FS_SM))
                        .text_color(pal.ink_strong)
                        .child(SharedString::from(format!("{} finished", report.name))),
                )
                .child(
                    h_flex()
                        .gap(px(5.))
                        .items_center()
                        .whitespace_nowrap()
                        .text_size(px(FS_XS))
                        .text_color(phase.color(pal))
                        .child(Icon::new(phase.icon()).with_size(px(12.)))
                        .child(word),
                )
                .when_some(report.run_ms, |row, ms| {
                    row.child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(11.))
                            .text_color(pal.faint)
                            .child(crate::clock::span(Duration::from_millis(ms))),
                    )
                }),
        )
        .when(!reply.trim().is_empty(), |card| {
            card.child(
                div()
                    .px(px(12.))
                    .py(px(SP_4))
                    .border_t_1()
                    .border_color(pal.subtle)
                    .text_size(px(FS_XS))
                    .text_color(pal.muted)
                    .child(gpui_kit::base::SelectableText::new(
                        ElementId::named_usize("report-reply", n),
                        reply,
                    )),
            )
        })
        .into_any_element()
}

/// The first non-empty line of `text`, without inline code ticks.
fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.replace('`', ""))
}

/// A short token count: `840`, `2.3k`, `1.2M`.
pub(crate) fn tokens(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// The avatar: the name's first letter on the fill, in the name's hue.
pub(crate) fn avatar(name: &str, pal: &Palette) -> Div {
    let hues = &pal.avatars;
    let hue = match crate::views::kit::design_hash(name) % 8 {
        0 => hues.amber,
        1 => hues.rose,
        2 => hues.orange,
        3 => hues.emerald,
        4 => hues.cyan,
        5 => hues.sky,
        6 => hues.violet,
        _ => hues.pink,
    };
    let letter = name
        .trim_start_matches("kage-")
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    div()
        .size(px(26.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(R_MD))
        .bg(pal.fill)
        .font_family(FONT_MONO)
        .font_weight(FontWeight::BOLD)
        .text_size(px(12.))
        .text_color(hue.fg)
        .child(letter)
}

/// The state label with its icon, in the state's color.
pub(crate) fn state_chip(facts: &AgentFacts, pal: &Palette) -> Div {
    h_flex()
        .gap(px(5.))
        .items_center()
        .whitespace_nowrap()
        .text_size(px(FS_XS))
        .text_color(facts.phase.color(pal))
        .child(Icon::new(facts.phase.icon()).with_size(px(12.)))
        .child(facts.phase.label())
}

/// `4s · 2.3k tok`, from what was measured.
pub(crate) fn meta_line(facts: &AgentFacts) -> String {
    [
        facts.elapsed.map(crate::clock::span),
        facts.tokens.map(|n| format!("{} tok", tokens(n))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" \u{b7} ")
}

/// The card an `agent` call renders as: avatar, name and kind, the task
/// or the latest tool line, the state with elapsed time and tokens, a
/// stop button while it runs, and the result once it ended.
pub(crate) fn agent_card(
    ix: usize,
    facts: &AgentFacts,
    store: &Entity<Store>,
    view: &Entity<TranscriptView>,
    cx: &App,
) -> AnyElement {
    let pal = Palette::active(cx);
    let hover = pal.hover;
    let sub = match (&facts.last, facts.phase.live()) {
        (Some(last), true) => last.clone(),
        _ => facts.task.clone(),
    };
    let open = {
        let view = view.clone();
        let id = facts.id.clone();
        move |_: &gpui_kit::ClickEvent, _: &mut gpui_kit::Window, cx: &mut App| {
            let id = id.clone();
            view.update(cx, |_, cx| cx.emit(TranscriptEvent::OpenAgent(id)));
        }
    };
    let mut head = h_flex()
        .id("head")
        .gap(px(10.))
        .items_center()
        .min_h(px(44.))
        .px(px(12.))
        .py(px(SP_4))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_click(open)
        .child(avatar(&facts.name, pal))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap(px(SP_4))
                        .items_center()
                        .child(
                            div()
                                .truncate()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_size(px(FS_SM))
                                .text_color(pal.ink_strong)
                                .child(SharedString::from(facts.name.clone())),
                        )
                        .child(
                            div()
                                .px(px(7.))
                                .py(px(1.))
                                .rounded(px(R_FULL))
                                .bg(if facts.background {
                                    pal.accent_soft
                                } else {
                                    pal.fill
                                })
                                .text_size(px(11.))
                                .text_color(if facts.background {
                                    pal.accent
                                } else {
                                    pal.muted
                                })
                                .child(if facts.background {
                                    SharedString::from("background")
                                } else {
                                    SharedString::from(facts.name.clone())
                                }),
                        ),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(FS_XS))
                        .text_color(pal.faint)
                        .child(SharedString::from(sub)),
                ),
        )
        .child(
            v_flex()
                .items_end()
                .gap(px(2.))
                .child(state_chip(facts, pal))
                .child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(11.))
                        .text_color(pal.faint)
                        .child(meta_line(facts)),
                ),
        );
    if facts.stoppable {
        let store = store.clone();
        let id = facts.id.clone();
        let fill_hover = pal.fill_hover;
        head = head.child(
            div()
                .id("stop")
                .size(px(24.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(R_MD))
                .text_color(pal.muted)
                .hover(move |style| style.bg(fill_hover))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    store.act(cx, |store| store.cancel_session(&id));
                })
                .child(Icon::new(IconName::Square).with_size(px(12.))),
        );
    } else {
        head = head.child(
            Icon::new(IconName::ChevronRight)
                .with_size(px(14.))
                .text_color(pal.faint),
        );
    }
    let mut card = v_flex()
        .id(ElementId::named_usize("row-agent", ix))
        .w_full()
        .border_1()
        .border_color(pal.line)
        .rounded(px(R_LG))
        .bg(pal.surface)
        .overflow_hidden()
        .child(head);
    if let Some(result) = facts.result.as_ref().filter(|_| !facts.phase.live()) {
        card = card.child(
            h_flex()
                .gap(px(SP_4))
                .items_center()
                .px(px(12.))
                .py(px(SP_4))
                .border_t_1()
                .border_color(pal.subtle)
                .text_size(px(FS_XS))
                .text_color(pal.muted)
                .child(
                    Icon::new(if facts.phase == Phase::Done {
                        IconName::CircleCheck
                    } else {
                        IconName::CircleX
                    })
                    .with_size(px(12.)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(result.clone())),
                ),
        );
    }
    card.into_any_element()
}

/// How a swarm's members stand, counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SwarmCounts {
    pub running: usize,
    pub paused: usize,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub total: usize,
}

impl SwarmCounts {
    pub(crate) fn of(members: &[AgentFacts]) -> Self {
        let mut counts = Self {
            total: members.len(),
            ..Self::default()
        };
        for member in members {
            match member.phase {
                Phase::Running => counts.running += 1,
                Phase::Paused => counts.paused += 1,
                Phase::Done => counts.done += 1,
                Phase::Failed => counts.failed += 1,
                Phase::Cancelled => counts.cancelled += 1,
            }
        }
        counts
    }

    /// Whether a member still works or waits to.
    pub(crate) fn live(self) -> bool {
        self.running + self.paused > 0
    }
}

/// What the swarm card draws besides its members.
pub(crate) struct SwarmView<'a> {
    /// The transcript item index of the `swarm` call.
    pub ix: usize,
    /// The `swarm` call.
    pub call: &'a ToolCallItem,
    /// Its members, in launch order.
    pub members: &'a [AgentFacts],
    /// Whether the member list shows.
    pub open: bool,
    /// Whether the prompt template shows.
    pub template_open: bool,
    /// How long this client watched the batch run, once it ended.
    pub took: Option<Duration>,
    /// The clock the constellation twinkles on, when it draws.
    pub field: Option<web_time::Instant>,
}

/// The card a `swarm` call renders as: the batch head with live counts,
/// the progress bar, and while open the member rows and the footer with
/// the prompt template, the summary and Retry for failed members.
pub(crate) fn swarm_card(
    swarm: &SwarmView<'_>,
    store: &Entity<Store>,
    view: &Entity<TranscriptView>,
    cx: &App,
) -> AnyElement {
    let pal = Palette::active(cx);
    let ix = swarm.ix;
    let counts = SwarmCounts::of(swarm.members);
    let input = swarm.call.input.as_ref();
    let text = |key: &str| {
        input
            .and_then(|input| input.get(key))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let description = Some(text("description"))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "Swarm".to_owned());
    let agent = text("agent");
    let sub = if agent.is_empty() {
        format!("{} agents", counts.total)
    } else {
        format!("{agent} \u{b7} {} agents", counts.total)
    };
    let hover = pal.hover;
    let toggle_view = view.clone();
    let mut head = h_flex()
        .id("head")
        .gap(px(10.))
        .items_center()
        .min_h(px(44.))
        .px(px(12.))
        .py(px(SP_4))
        .cursor_pointer()
        .bg(pal.done_soft)
        .hover(move |style| style.bg(hover))
        .on_click(move |_, _, cx| {
            toggle_view.update(cx, |view, cx| view.toggle_swarm(ix, cx));
        })
        .child(
            div()
                .size(px(28.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(R_MD))
                .bg(pal.done_soft)
                .text_color(pal.done)
                .child(Icon::new(IconName::Waypoints).with_size(px(14.))),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(FS_SM))
                        .text_color(pal.ink_strong)
                        .child(SharedString::from(description)),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(FS_XS))
                        .text_color(pal.faint)
                        .child(SharedString::from(sub)),
                ),
        );
    if counts.live() {
        let mut live = format!("{} running", counts.running);
        if counts.paused > 0 {
            live.push_str(&format!(", {} limited", counts.paused));
        }
        head = head.child(
            h_flex()
                .gap(px(5.))
                .items_center()
                .whitespace_nowrap()
                .text_size(px(FS_XS))
                .text_color(pal.accent)
                .child(Icon::new(IconName::LoaderCircle).with_size(px(12.)))
                .child(live),
        );
    }
    let ended = counts.done + counts.failed + counts.cancelled;
    head = head
        .child(
            h_flex()
                .font_family(FONT_MONO)
                .text_size(px(FS_SM))
                .text_color(pal.ink_strong)
                .child(ended.to_string())
                .child(
                    div()
                        .text_color(pal.faint)
                        .child(format!(" / {}", counts.total)),
                ),
        )
        .child(
            Icon::new(if swarm.open {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            })
            .with_size(px(14.))
            .text_color(pal.faint),
        );
    let share = |n: usize| {
        if counts.total == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            let share = n as f32 / counts.total as f32;
            share
        }
    };
    let bar = h_flex()
        .h(px(3.))
        .w_full()
        .bg(pal.fill)
        .overflow_hidden()
        .child(div().h_full().w(relative(share(counts.done))).bg(pal.ok))
        .child(
            div()
                .h_full()
                .w(relative(share(counts.failed)))
                .bg(pal.danger),
        )
        .child(
            div()
                .h_full()
                .w(relative(share(counts.running + counts.paused)))
                .bg(pal.done)
                .opacity(0.55),
        );
    let mut card = v_flex()
        .id(ElementId::named_usize("row-swarm", ix))
        .w_full()
        .border_1()
        .border_color(pal.done_bd)
        .rounded(px(R_LG))
        .bg(pal.surface)
        .overflow_hidden()
        .child(head);
    card = match swarm.field {
        Some(origin) => {
            let stars = swarm
                .members
                .iter()
                .map(|member| Star {
                    id: member.id.clone(),
                    item: member
                        .item
                        .as_ref()
                        .map_or_else(|| member.name.clone(), |(item, _)| item.clone()),
                    phase: member.phase,
                })
                .collect();
            let open_view = view.clone();
            card.child(
                div()
                    .px(px(10.))
                    .pt(px(2.))
                    .pb(px(6.))
                    .border_t_1()
                    .border_color(pal.subtle)
                    .child(constellation(
                        stars,
                        None,
                        origin,
                        move |id, _, cx| {
                            open_view.update(cx, |_, cx| cx.emit(TranscriptEvent::OpenAgent(id)));
                        },
                        cx,
                    )),
            )
        }
        None => card.child(bar),
    };
    if !swarm.open {
        return card.into_any_element();
    }
    let mut rows = v_flex()
        .id("members")
        .max_h(px(320.))
        .overflow_y_scroll()
        .border_t_1()
        .border_color(pal.subtle);
    for (n, member) in swarm.members.iter().enumerate() {
        let open_view = view.clone();
        let id = member.id.clone();
        let line = if member.phase.live() {
            member
                .reason
                .clone()
                .or_else(|| member.last.clone())
                .unwrap_or_else(|| "starting".to_owned())
        } else {
            member.result.clone().unwrap_or_default()
        };
        let (item, index) = member
            .item
            .clone()
            .unwrap_or_else(|| (member.name.clone(), u32::try_from(n).unwrap_or(0)));
        rows = rows.child(
            h_flex()
                .id(ElementId::named_usize("swarm-member", ix * 1000 + n))
                .h(px(36.))
                .px(px(12.))
                .gap(px(10.))
                .items_center()
                .cursor_pointer()
                .text_size(px(FS_SM))
                .hover(move |style| style.bg(hover))
                .when(n > 0, |row| row.border_t_1().border_color(pal.subtle))
                .on_click(move |_, _, cx| {
                    let id = id.clone();
                    open_view.update(cx, |_, cx| cx.emit(TranscriptEvent::OpenAgent(id)));
                })
                .child(
                    div()
                        .w(px(18.))
                        .flex_none()
                        .font_family(FONT_MONO)
                        .text_size(px(11.))
                        .text_color(pal.faint)
                        .child(format!("{:02}", index + 1)),
                )
                .child(
                    div()
                        .flex_initial()
                        .min_w(px(60.))
                        .max_w(gpui_kit::relative(0.6))
                        .truncate()
                        .text_color(pal.ink)
                        .child(SharedString::from(item)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(FONT_MONO)
                        .text_size(px(11.))
                        .text_color(pal.faint)
                        .child(SharedString::from(line)),
                )
                .child(
                    div()
                        .w(px(44.))
                        .flex_none()
                        .text_right()
                        .font_family(FONT_MONO)
                        .text_size(px(11.))
                        .text_color(pal.faint)
                        .child(member.tokens.map(tokens).unwrap_or_default()),
                )
                .child(div().w(px(92.)).flex_none().child(state_chip(member, pal))),
        );
    }
    card = card.child(rows);
    let template = swarm
        .call
        .swarm
        .as_ref()
        .and_then(|meta| meta.template.clone())
        .or_else(|| Some(text("prompt_template")).filter(|text| !text.is_empty()));
    if swarm.template_open
        && let Some(template) = &template
    {
        let mut line = h_flex()
            .flex_wrap()
            .px(px(12.))
            .py(px(SP_4))
            .border_t_1()
            .border_color(pal.subtle)
            .font_family(FONT_MONO)
            .text_size(px(FS_XS))
            .text_color(pal.muted);
        for (n, part) in template.split("{{item}}").enumerate() {
            if n > 0 {
                line = line.child(div().text_color(pal.done).child("{{item}}"));
            }
            line = line.child(SharedString::from(part.to_owned()));
        }
        card = card.child(line);
    }
    let mut summary = format!("{} finished", counts.done);
    if counts.failed > 0 {
        summary.push_str(&format!(", {} failed", counts.failed));
    }
    if counts.cancelled > 0 {
        summary.push_str(&format!(", {} aborted", counts.cancelled));
    }
    if let Some(took) = swarm.took {
        summary.push_str(&format!(" \u{b7} {}", crate::clock::span(took)));
    }
    let mut foot = h_flex()
        .gap(px(SP_4))
        .items_center()
        .px(px(12.))
        .py(px(SP_4))
        .border_t_1()
        .border_color(pal.subtle)
        .text_size(px(FS_XS))
        .text_color(pal.muted);
    if template.is_some() {
        let template_view = view.clone();
        foot = foot.child(
            kit::btn_sm(format!("swarm-template-{ix}"), BtnTone::Plain, pal)
                .on_click(move |_, _, cx| {
                    template_view.update(cx, |view, cx| view.toggle_template(ix, cx));
                })
                .child(Icon::new(IconName::File).with_size(px(12.)))
                .child(if swarm.template_open {
                    "Hide template"
                } else {
                    "Prompt template"
                }),
        );
    }
    foot = foot.child(div().flex_1()).child(summary);
    let retry: Vec<String> = swarm
        .members
        .iter()
        .filter(|member| matches!(member.phase, Phase::Failed | Phase::Cancelled))
        .map(|member| member.id.clone())
        .collect();
    if !counts.live() && !retry.is_empty() {
        let store = store.clone();
        let n = retry.len();
        foot = foot.child(
            kit::btn_sm(format!("swarm-retry-{ix}"), BtnTone::Plain, pal)
                .on_click(move |_, _, cx| {
                    store.act(cx, |store| store.resume_swarm(&retry));
                })
                .child(Icon::new(IconName::RefreshCw).with_size(px(12.)))
                .child(format!("Retry {n}")),
        );
    }
    let agents_view = view.clone();
    foot = foot.child(
        kit::btn_sm(format!("swarm-agents-{ix}"), BtnTone::Plain, pal)
            .on_click(move |_, _, cx| {
                agents_view.update(cx, |_, cx| cx.emit(TranscriptEvent::OpenAgents));
            })
            .child(Icon::new(IconName::Users).with_size(px(12.)))
            .child("Agents"),
    );
    card.child(foot).into_any_element()
}

#[cfg(test)]
mod tests {
    use kage_client::wire::{
        ContentBlock, MessageChunk, ToolCallContent, ToolCallStatus, ToolKind,
    };
    use kage_client::{Session, ToolCallItem, TranscriptItem};

    use super::{recorded_reply, tokens};
    use crate::views::kit::design_hash;

    #[test]
    fn a_card_reads_the_reply_its_call_recorded() {
        let output = "completed: 2, failed: 0, cancelled: 0\n\
            <swarm description=\"d\" item=\"a\">\n<agent name=\"general\" session=\"c1\" \
            state=\"completed\" tools=\"1\">\nfirst `answer`\nmore\n</agent>\n</swarm>\n\
            <swarm description=\"d\" item=\"b\">\n<agent name=\"general\" session=\"c2\" \
            state=\"failed\" tools=\"0\">\n\nboom\n</agent>\n</swarm>";
        let mut parent = Session::new("p");
        parent.items.push(TranscriptItem::ToolCall(ToolCallItem {
            tool_call_id: "call_s".into(),
            title: "swarm".into(),
            kind: ToolKind::Other,
            status: ToolCallStatus::Completed,
            input: None,
            swarm: None,
            content: vec![ToolCallContent::Content(MessageChunk {
                content: ContentBlock::text(output),
                meta: None,
            })],
            raw_output: None,
            took_ms: None,
        }));
        assert_eq!(
            recorded_reply(&parent, "call_s", "c1").as_deref(),
            Some("first answer")
        );
        assert_eq!(
            recorded_reply(&parent, "call_s", "c2").as_deref(),
            Some("boom")
        );
        assert_eq!(recorded_reply(&parent, "call_s", "c3"), None);
        assert_eq!(recorded_reply(&parent, "other", "c1"), None);
    }

    #[test]
    fn token_counts_shorten_like_the_design() {
        assert_eq!(tokens(840), "840");
        assert_eq!(tokens(2_300), "2.3k");
        assert_eq!(tokens(1_200_000), "1.2M");
    }

    #[test]
    fn the_hash_matches_the_design_so_hues_agree() {
        // JavaScript: [...'explore'].reduce((x, c) => (x * 31 + c.charCodeAt(0)) | 0, 0)
        assert_eq!(design_hash("explore"), 1_309_148_525);
        assert_eq!(design_hash(""), 0);
    }
}
