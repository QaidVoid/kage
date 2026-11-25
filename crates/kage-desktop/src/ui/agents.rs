//! The agent and swarm cards an `agent` or `swarm` call renders as.
//!
//! A card reads the subagent records the parent session holds, joined
//! to the call by the `toolCallId` each `subagent_update` names, and the
//! child sessions the engine streams. Every count is engine state:
//! elapsed time is what this client watched or else the run time the
//! engine reported, tokens are what the child reported spending, and a
//! field the wire never carried stays blank.

use crate::ui::forms::ShowCrisp as _;
use std::time::Duration;

use kage_client::agent_text::{self, AgentText, Report};
use kage_client::wire::{NoticeTone, SubagentState};
use kage_client::{Session, Subagent, TranscriptItem};

use crate::store::Store;
use crate::theme::{
    FAMILY_BODY_SEMIBOLD, FAMILY_MONO_REGULAR, FS_SM, FS_XS, Palette, R_LG, family,
};
use crate::ui::transcript::model::tool_verb;

/// Where one subagent stands, as the cards label it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
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

/// The card a delivered agent report renders as: who finished, how it
/// ended, its run time and the first lines of its reply. Reports
/// whether the card was clicked, which opens the agent.
pub fn report_card(ui: &mut egui::Ui, n: usize, report: &Report, palette: &Palette) -> bool {
    const SHOWN: usize = 6;
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
    let frame = egui::Frame::NONE
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.line))
        .corner_radius(egui::CornerRadius::same(R_LG as u8))
        .show_crisp(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(format!("{} finished", report.name))
                        .size(FS_SM)
                        .family(family(FAMILY_BODY_SEMIBOLD))
                        .color(palette.ink_strong),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(12.0);
                    if let Some(ms) = report.run_ms {
                        ui.label(
                            egui::RichText::new(crate::clock::span(Duration::from_millis(ms)))
                                .size(FS_XS)
                                .family(family(FAMILY_MONO_REGULAR))
                                .color(palette.faint),
                        );
                    }
                    egui::Frame::NONE
                        .fill(palette.fill)
                        .corner_radius(egui::CornerRadius::same(u8::MAX))
                        .inner_margin(egui::Margin::symmetric(9, 3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                crate::icons::icon(ui, phase.icon(), 12.0, phase.color(palette));
                                ui.label(
                                    egui::RichText::new(word)
                                        .size(FS_XS)
                                        .color(phase.color(palette)),
                                );
                            });
                        });
                });
            });
            if !reply.trim().is_empty() {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(reply).size(FS_XS).color(palette.muted),
                        )
                        .wrap(),
                    );
                });
            }
        });
    ui.interact(
        frame.response.rect,
        ui.id().with(("report", n)),
        egui::Sense::click(),
    )
    .clicked()
}

impl Phase {
    /// The phase `agent` stands in.
    #[must_use]
    pub fn of(agent: &Subagent) -> Self {
        match agent.state {
            None | Some(SubagentState::Running) => Self::Running,
            Some(SubagentState::Paused) => Self::Paused,
            Some(SubagentState::Completed) => Self::Done,
            // A state this build cannot name is final; reading it as a
            // failure keeps the member visible instead of silently done.
            Some(SubagentState::Unknown) | Some(SubagentState::Failed) => Self::Failed,
            Some(SubagentState::Cancelled) => Self::Cancelled,
        }
    }

    /// Whether the agent still works or waits to.
    #[must_use]
    pub fn live(self) -> bool {
        matches!(self, Self::Running | Self::Paused)
    }

    /// The label the card carries.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Paused => "Rate limited",
            Self::Done => "Finished",
            Self::Failed => "Error",
            Self::Cancelled => "Cancelled",
        }
    }

    /// The card's color for the phase.
    #[must_use]
    pub fn color(self, palette: &Palette) -> egui::Color32 {
        match self {
            Self::Running => palette.accent,
            Self::Paused => palette.warn,
            Self::Done => palette.ok,
            Self::Failed => palette.danger,
            Self::Cancelled => palette.faint,
        }
    }

    /// The glyph the card's state chip carries.
    #[must_use]
    pub fn icon(self) -> crate::icons::Icon {
        match self {
            Self::Running => crate::icons::Icon::LoaderCircle,
            Self::Paused => crate::icons::Icon::CirclePause,
            Self::Done => crate::icons::Icon::CircleCheck,
            Self::Failed => crate::icons::Icon::CircleX,
            Self::Cancelled => crate::icons::Icon::CircleSlash,
        }
    }
}

/// What a card shows about one subagent.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentFacts {
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
    /// Its reported USD amount, which may be a partial subtotal.
    pub cost: Option<f64>,
    /// Pricing quality of the reported USD amount.
    pub cost_status: kage_client::wire::CostStatus,
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
#[must_use]
pub fn children_of<'a>(parent: &'a Session, call_id: &str) -> Vec<(&'a String, &'a Subagent)> {
    let mut children: Vec<_> = parent
        .agents
        .iter()
        .filter(|(_, agent)| agent.tool_call_id.as_deref() == Some(call_id))
        .collect();
    children.sort_by_key(|(_, agent)| agent.swarm.as_ref().map(|swarm| swarm.index));
    children
}

/// The facts of child `id` of `parent`.
#[must_use]
pub fn agent_facts(store: &Store, parent: &Session, id: &str, agent: &Subagent) -> AgentFacts {
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
        cost: usage.map(|usage| usage.cost),
        cost_status: usage.map(|usage| usage.cost_status).unwrap_or_default(),
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

/// The first line of `text`, trimmed, or `None` when it is blank.
#[must_use]
pub fn first_line(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    Some(line.trim().to_owned())
}

/// A token count as the cards write it: `12480` becomes `12.5k`.
#[must_use]
pub fn tokens(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let n = n as f64;
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else {
        format!("{}", n as u64)
    }
}

/// Paints `name`'s avatar into `rect`: its first letter, past any
/// `kage-` prefix, on the soft fill in the name's hue.
pub fn paint_avatar(ui: &egui::Ui, rect: egui::Rect, name: &str, palette: &Palette) {
    let hues = &palette.avatars;
    let hue = match crate::ui::transcript::tool_rows::design_hash(name) % 8 {
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
    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same(crate::theme::R_MD as u8),
        palette.fill,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        letter,
        egui::FontId::new(12.0, crate::theme::family(crate::theme::FAMILY_MONO_BOLD)),
        hue.fg,
    );
}

/// The two-letter avatar tile `name` leads with: its ascii letters,
/// uppercased, so `explore` reads `EX`.
#[must_use]
pub fn avatar_tile(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphabetic)
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

/// The meta line a card carries under its title: the elapsed time, the
/// tokens, the qualified cost and the model, each omitted when absent.
#[must_use]
pub fn meta_line(facts: &AgentFacts) -> String {
    let mut parts = Vec::new();
    if let Some(elapsed) = facts.elapsed {
        parts.push(format_duration(elapsed));
    }
    if let Some(tokens) = facts.tokens {
        parts.push(format!("{} tok", tokens_of(tokens)));
    }
    if let Some(cost) = crate::cost::label(facts.cost, "$", 4, facts.cost_status) {
        parts.push(cost);
    }
    if let Some(model) = &facts.model {
        parts.push(model.clone());
    }
    parts.join(" \u{b7} ")
}

/// The token count the meta line writes.
fn tokens_of(n: u64) -> String {
    tokens(n)
}

/// A duration as the cards write it: `1.2s`, `45s`, `3m 05s`.
#[must_use]
pub fn format_duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let minutes = elapsed.as_secs() / 60;
    let seconds = elapsed.as_secs() % 60;
    format!("{minutes}m {seconds:02}s")
}

/// What a card asks the shell to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// Open the child session `id` in the workbench's detail tab.
    Open(String),
    /// Stop the child session `id`.
    Stop(String),
    /// Steer the child session `id` with `text`.
    Steer(String, String),
}

/// Draws one agent card and reports what its controls asked for.
pub fn agent_card(
    ui: &mut egui::Ui,
    palette: &Palette,
    facts: &AgentFacts,
    running: bool,
) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let color = facts.phase.color(palette);
    let title = format!("{} {}", avatar_tile(&facts.name), facts.name);
    let meta = meta_line(facts);
    let response = egui::Frame::NONE
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.line))
        .corner_radius(egui::CornerRadius::same(crate::theme::R_LG as u8))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show_crisp(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                // The avatar tile.
                let tile = egui::Frame::NONE
                    .fill(palette.fill)
                    .corner_radius(egui::CornerRadius::same(7))
                    .inner_margin(egui::Margin::symmetric(5, 3))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(avatar_tile(&facts.name))
                                .size(10.0)
                                .family(crate::theme::family(crate::theme::FAMILY_MONO_REGULAR))
                                .color(palette.muted),
                        );
                    });
                let _ = tile;
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(
                            egui::RichText::new(title)
                                .size(crate::theme::FS_SM)
                                .color(palette.ink_strong),
                        );
                        // The phase chip.
                        let chip = egui::RichText::new(facts.phase.label())
                            .size(crate::theme::FS_2XS)
                            .color(color);
                        let galley = ui.painter().layout_no_wrap(
                            chip.text().to_owned(),
                            egui::FontId::new(
                                crate::theme::FS_2XS,
                                crate::theme::family(crate::theme::FAMILY_BODY_SEMIBOLD),
                            ),
                            color,
                        );
                        let width = galley.size().x + 12.0;
                        let chip_rect =
                            egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, 16.0));
                        ui.painter().rect_filled(
                            chip_rect,
                            egui::CornerRadius::same(8),
                            palette.fill,
                        );
                        ui.painter().galley(
                            chip_rect.center() - galley.size() / 2.0,
                            galley,
                            color,
                        );
                        ui.allocate_rect(chip_rect, egui::Sense::hover());
                    });
                    if !meta.is_empty() {
                        ui.label(
                            egui::RichText::new(meta)
                                .size(crate::theme::FS_XS)
                                .color(palette.faint),
                        );
                    }
                    // The task or the latest tool line.
                    let detail = if facts.phase.live() {
                        facts.last.clone().unwrap_or_else(|| facts.task.clone())
                    } else {
                        facts.result.clone().unwrap_or_else(|| facts.task.clone())
                    };
                    if !detail.is_empty() {
                        ui.label(
                            egui::RichText::new(detail)
                                .size(crate::theme::FS_XS)
                                .color(palette.muted),
                        );
                    }
                    if let Some(reason) = &facts.reason {
                        ui.label(
                            egui::RichText::new(reason.clone())
                                .size(crate::theme::FS_XS)
                                .color(palette.warn),
                        );
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if facts.stoppable
                        && running
                        && crate::ui::forms::icon_btn(
                            ui,
                            "Stop this agent",
                            crate::icons::Icon::Square,
                            24.0,
                            12.0,
                            crate::theme::R_MD,
                            crate::ui::forms::IconTones {
                                fill: egui::Color32::TRANSPARENT,
                                hover_fill: palette.fill_hover,
                                ink: palette.muted,
                                hover_ink: palette.muted,
                            },
                        )
                        .on_hover_text("Stop this agent")
                        .clicked()
                    {
                        events.push(AgentEvent::Stop(facts.id.clone()));
                    }
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("Open")
                                .size(crate::theme::FS_XS)
                                .color(palette.muted),
                        ))
                        .on_hover_text("Open this agent's transcript")
                        .clicked()
                    {
                        events.push(AgentEvent::Open(facts.id.clone()));
                    }
                });
            });
        })
        .response;
    if response.clicked() {
        events.push(AgentEvent::Open(facts.id.clone()));
    }
    events
}

/// The swarm counts a card carries over its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwarmCounts {
    /// Members that reached an end state the card counts as done.
    pub done: usize,
    /// Every member announced so far.
    pub total: usize,
    /// Members paused mid task.
    pub paused: usize,
    /// Members still working or waiting.
    pub live: usize,
}

impl SwarmCounts {
    /// The counts `members` carry.
    #[must_use]
    pub fn of(members: &[AgentFacts]) -> Self {
        Self {
            done: members
                .iter()
                .filter(|facts| {
                    matches!(facts.phase, Phase::Done | Phase::Failed | Phase::Cancelled)
                })
                .count(),
            total: members.len(),
            paused: members
                .iter()
                .filter(|facts| facts.phase == Phase::Paused)
                .count(),
            live: members.iter().filter(|facts| facts.phase.live()).count(),
        }
    }

    /// The count line the card carries: `2/5 done`.
    #[must_use]
    pub fn line(self) -> String {
        format!("{}/{} done", self.done, self.total)
    }
}

/// Draws the swarm card: its count line over the member cards.
pub fn swarm_card(
    ui: &mut egui::Ui,
    palette: &Palette,
    members: &[AgentFacts],
    running: bool,
) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let counts = SwarmCounts::of(members);
    egui::Frame::NONE
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.line))
        .corner_radius(egui::CornerRadius::same(crate::theme::R_LG as u8))
        .inner_margin(egui::Margin::same(12))
        .show_crisp(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                crate::icons::icon(ui, crate::icons::Icon::Waypoints, 14.0, palette.done);
                ui.label(
                    egui::RichText::new("Swarm")
                        .size(crate::theme::FS_SM)
                        .color(palette.done),
                );
                ui.label(
                    egui::RichText::new(counts.line())
                        .size(crate::theme::FS_XS)
                        .family(crate::theme::family(crate::theme::FAMILY_MONO_REGULAR))
                        .color(palette.ink_strong),
                );
                if counts.paused > 0 {
                    ui.label(
                        egui::RichText::new(format!("{} paused", counts.paused))
                            .size(crate::theme::FS_XS)
                            .color(palette.warn),
                    );
                }
            });
        });
    for facts in members {
        events.extend(agent_card(ui, palette, facts, running));
    }
    events
}

/// The report the parent recorded for child `id`, when its message
/// reached this session.
#[must_use]
pub fn report_of(items: &[TranscriptItem], id: &str) -> Option<Report> {
    items.iter().rev().find_map(|item| match item {
        TranscriptItem::User { content, .. } => content.iter().find_map(|block| {
            let text = block.as_text()?;
            agent_text::split(text)?
                .1
                .into_iter()
                .find_map(|part| match part {
                    AgentText::Report(report) if report.session == id => Some(report),
                    _ => None,
                })
        }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AgentFacts, Phase, SwarmCounts, avatar_tile, children_of, first_line, format_duration,
        meta_line, report_of, tokens,
    };
    use kage_client::wire::SubagentState;
    use kage_client::{Session, Subagent, TranscriptItem};
    use serde_json::json;

    fn agent(name: Option<&str>, state: Option<SubagentState>) -> Subagent {
        Subagent {
            name: name.map(str::to_owned),
            task: None,
            capabilities: None,
            state,
            swarm: None,
            reason: None,
            tool_call_id: None,
            usage: None,
            model: None,
            background: false,
        }
    }

    #[test]
    fn a_missing_state_is_running_and_an_unknown_one_is_failed() {
        assert_eq!(Phase::of(&agent(None, None)), Phase::Running);
        assert_eq!(
            Phase::of(&agent(None, Some(SubagentState::Running))),
            Phase::Running
        );
        assert_eq!(
            Phase::of(&agent(None, Some(SubagentState::Paused))),
            Phase::Paused
        );
        assert_eq!(
            Phase::of(&agent(None, Some(SubagentState::Completed))),
            Phase::Done
        );
        assert_eq!(
            Phase::of(&agent(None, Some(SubagentState::Failed))),
            Phase::Failed
        );
        assert_eq!(
            Phase::of(&agent(None, Some(SubagentState::Unknown))),
            Phase::Failed,
            "an unnameable state stays visible as a failure"
        );
        assert_eq!(
            Phase::of(&agent(None, Some(SubagentState::Cancelled))),
            Phase::Cancelled
        );
    }

    #[test]
    fn the_live_phases_are_running_and_paused() {
        assert!(Phase::Running.live());
        assert!(Phase::Paused.live());
        assert!(!Phase::Done.live());
        assert!(!Phase::Failed.live());
        assert!(!Phase::Cancelled.live());
    }

    #[test]
    fn the_avatar_tile_takes_two_letters() {
        assert_eq!(avatar_tile("explore"), "EX");
        assert_eq!(avatar_tile("z.ai"), "ZA");
        assert_eq!(avatar_tile("x"), "X");
    }

    #[test]
    fn the_first_line_is_the_first_non_blank_one() {
        assert_eq!(first_line("").as_deref(), None);
        assert_eq!(first_line("  \n  ").as_deref(), None);
        assert_eq!(
            first_line("\n\n  the answer  \nmore").as_deref(),
            Some("the answer")
        );
    }

    #[test]
    fn token_counts_read_like_the_cards() {
        assert_eq!(tokens(0), "0");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(12_480), "12.5k");
        assert_eq!(tokens(2_000_000), "2.0M");
    }

    #[test]
    fn durations_read_like_the_cards() {
        use std::time::Duration;
        assert_eq!(format_duration(Duration::from_millis(1200)), "1.2s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45.0s");
        assert_eq!(format_duration(Duration::from_secs(185)), "3m 05s");
    }

    #[test]
    fn the_meta_line_drops_what_is_unknown() {
        let facts = AgentFacts {
            id: "s1".to_owned(),
            name: "explore".to_owned(),
            task: String::new(),
            phase: Phase::Running,
            reason: None,
            elapsed: Some(std::time::Duration::from_secs(45)),
            tokens: None,
            cost: None,
            cost_status: Default::default(),
            model: None,
            last: None,
            result: None,
            stoppable: false,
            item: None,
            background: false,
        };
        assert_eq!(meta_line(&facts), "45.0s");
        let facts = AgentFacts {
            model: Some("openai/gpt-5".to_owned()),
            ..facts
        };
        assert_eq!(meta_line(&facts), "45.0s \u{b7} openai/gpt-5");
    }

    #[test]
    fn agent_cost_labels_keep_quality_and_known_zero() {
        use kage_client::wire::{CostStatus, SubagentUsage};

        let store = crate::store::Store::new(".", false);
        let parent = Session::default();
        let mut agent = agent(None, None);
        agent.usage = Some(SubagentUsage {
            cost: 1.2,
            cost_status: CostStatus::Estimated,
            ..Default::default()
        });
        let facts = super::agent_facts(&store, &parent, "c1", &agent);
        assert_eq!(facts.cost_status, CostStatus::Estimated);
        assert_eq!(meta_line(&facts), "~$1.2000");
        agent.usage.as_mut().unwrap().cost_status = CostStatus::Incomplete;
        let facts = super::agent_facts(&store, &parent, "c1", &agent);
        assert_eq!(meta_line(&facts), "$1.2000 (partial)");
        agent.usage.as_mut().unwrap().cost = 0.0;
        let facts = super::agent_facts(&store, &parent, "c1", &agent);
        assert_eq!(meta_line(&facts), "cost unknown");
        agent.usage.as_mut().unwrap().cost_status = CostStatus::Recorded;
        let facts = super::agent_facts(&store, &parent, "c1", &agent);
        assert_eq!(facts.cost, Some(0.0));
        assert_eq!(meta_line(&facts), "$0.0000");
        agent.usage = None;
        let facts = super::agent_facts(&store, &parent, "c1", &agent);
        assert_eq!(facts.cost_status, CostStatus::Recorded);
        assert_eq!(meta_line(&facts), "");
    }

    #[test]
    fn the_swarm_counts_split_the_members() {
        let facts = |phase| AgentFacts {
            id: "s1".to_owned(),
            name: "explore".to_owned(),
            task: String::new(),
            phase,
            reason: None,
            elapsed: None,
            tokens: None,
            cost: None,
            cost_status: Default::default(),
            model: None,
            last: None,
            result: None,
            stoppable: false,
            item: None,
            background: false,
        };
        let members = vec![
            facts(Phase::Running),
            facts(Phase::Paused),
            facts(Phase::Done),
            facts(Phase::Failed),
        ];
        let counts = SwarmCounts::of(&members);
        assert_eq!(counts.total, 4);
        assert_eq!(counts.done, 2, "done and failed both count as done");
        assert_eq!(counts.paused, 1);
        assert_eq!(counts.live, 2);
        assert_eq!(counts.line(), "2/4 done");
    }

    #[test]
    fn the_children_of_a_call_come_in_launch_order() {
        let mut parent = Session::default();
        let mut late = agent(Some("late"), None);
        late.swarm = Some(kage_client::wire::SubagentSwarm {
            id: "b".to_owned(),
            item: "second".to_owned(),
            index: 1,
            total: 2,
        });
        late.tool_call_id = Some("call".to_owned());
        let mut early = agent(Some("early"), None);
        early.swarm = Some(kage_client::wire::SubagentSwarm {
            id: "b".to_owned(),
            item: "first".to_owned(),
            index: 0,
            total: 2,
        });
        early.tool_call_id = Some("call".to_owned());
        let mut other = agent(Some("other"), None);
        other.tool_call_id = Some("another".to_owned());
        parent.agents.insert("b".to_owned(), late);
        parent.agents.insert("a".to_owned(), early);
        parent.agents.insert("c".to_owned(), other);
        let kids = children_of(&parent, "call");
        assert_eq!(kids.len(), 2, "only the call's own children");
        assert_eq!(kids[0].1.name.as_deref(), Some("early"), "launch order");
        assert_eq!(kids[1].1.name.as_deref(), Some("late"));
    }

    #[test]
    fn a_report_reaches_the_card_through_its_message() {
        let mut parent = Session::default();
        parent.items.push(TranscriptItem::User {
            content: vec![kage_client::wire::ContentBlock::text(
                "<agent name=\"explore\" session=\"child\" state=\"completed\">\nthe reply\n</agent>",
            )],
            steered: false,
        });
        let report = report_of(&parent.items, "child").expect("the report is there");
        assert_eq!(report.session, "child");
        assert_eq!(report.state, "completed");
        assert_eq!(first_line(&report.body).as_deref(), Some("the reply"));
        assert!(report_of(&parent.items, "nope").is_none());
        let _ = json!({"unused": true});
    }
}
