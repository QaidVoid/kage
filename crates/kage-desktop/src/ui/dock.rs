//! The dock row above the composer: the goal pill with its popover,
//! the plan pill that leads to the plan card under review, the running
//! swarm, background agents and todos pills, and the queued prompt
//! rows.
//!
//! Every pill derives its state from the active session the store
//! holds, so the dock counts only what frames delivered and hides
//! what is incomputable. The approval card is a separate view the
//! shell mounts beside this row, and the plan review itself is
//! answered on its card in the transcript.

use crate::ui::forms::ShowCrisp as _;
use kage_client::Session;
use kage_client::wire::NoticeTone;
use kage_client::wire::{ContentBlock, SubagentState};
use serde_json::Value;

use crate::store::Store;

/// The goal option's id in the session's config options.
const GOAL_OPTION: &str = "goal";

/// The prefix a success notice carries when it reports the goal met.
const GOAL_MET_PREFIX: &str = "goal met: ";

/// The columns a queued prompt's row shows before it truncates.
const QUEUE_TEXT_COLUMNS: usize = 80;

/// The goal pill's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalState {
    /// The goal text the option holds.
    pub text: String,
    /// Whether a success notice reported the goal met.
    pub met: bool,
}

/// The goal state of a session, when its option holds a goal.
#[must_use]
pub fn goal_state(session: &Session) -> Option<GoalState> {
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
            kage_client::TranscriptItem::Notice {
                tone: NoticeTone::Success,
                text,
            } if text.starts_with(GOAL_MET_PREFIX)
        )
    });
    Some(GoalState { text, met })
}

/// The swarm pill's state, from the session's subagent tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmState {
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
        Some(SubagentState::Unknown) => "unknown",
    }
}

/// The background agents pill's state: how many of the session's
/// background agents run, and how many it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundState {
    /// Running or paused.
    pub live: usize,
    /// Every background agent the session announced.
    pub total: usize,
}

/// The background agents of a session, while it has any.
#[must_use]
pub fn background_state(session: &Session) -> Option<BackgroundState> {
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
pub fn swarm_state(session: &Session) -> Option<SwarmState> {
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
pub struct TodosState {
    /// Entries marked completed.
    pub done: usize,
    /// Every entry the plan carries.
    pub total: usize,
}

/// The todos state of a session, when its latest plan has entries.
#[must_use]
pub fn todos_state(session: &Session) -> Option<TodosState> {
    let entries = plan_entries(session)?;
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
pub fn todo_entries(session: &Session) -> Vec<(String, String)> {
    plan_entries(session)
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

/// The entries of the latest plan item, or `None` when the transcript
/// holds none.
fn plan_entries(session: &Session) -> Option<&[Value]> {
    session.items.iter().rev().find_map(|item| match item {
        kage_client::TranscriptItem::Plan { entries } => Some(entries.as_slice()),
        _ => None,
    })
}

/// One queue row: a held prompt and its place in the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRow {
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
pub fn queue_rows(session: &Session) -> Vec<QueueRow> {
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

/// Saves the goal text to the store: an empty or blank text clears the
/// goal, which is how the popover's Clear button works.
pub fn save_goal(store: &mut Store, text: &str) -> bool {
    let goal = text.trim();
    if goal.is_empty() {
        store.set_option("goal", "")
    } else {
        store.set_option("goal", goal)
    }
}

/// The pill height, the old row's 28px.
const PILL_H: f32 = 28.0;

/// The todos mini bar's track width.
const TODO_BAR: f32 = 36.0;

/// The pill label the plan pill carries before a plan arrives.
const PLAN_WAITING: &str = "Plan mode: the plan will show up here";

/// The pill label the plan pill carries while a plan waits.
const PLAN_PENDING: &str = "Plan: pending review";

/// What the dock row asks the shell to carry out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockAction {
    /// Steer the queued prompt at `index` into the running turn.
    Steer(usize),
    /// Fill the composer with the queued prompt and leave the queue.
    Edit(usize),
    /// Remove the queued prompt at `index`.
    Remove(usize),
    /// Bring the plan card under review into view in the transcript.
    ScrollToPlan,
    /// Show the background agents in the workbench's agents list.
    OpenBackgroundAgents,
    /// Open the goal editor over the goal pill.
    OpenGoal,
}

/// Draws the dock row: the pills the session's state names, then the
/// queued prompt sheet. Both read the store each frame, so the row
/// holds no session data of its own.
pub fn show(ui: &mut egui::Ui, store: &Store) -> Vec<DockAction> {
    let palette = crate::theme::Palette::active_of(ui.ctx());
    let mut actions = Vec::new();
    let plan_on = store.plan_on();
    let (goal, review, swarm, background, todos) = {
        let session = store.active_session();
        (
            session.and_then(goal_state),
            session.and_then(crate::store::plan_review),
            session.and_then(swarm_state),
            session.and_then(background_state),
            session.and_then(todos_state),
        )
    };
    let queue = store.active_session().map(queue_rows).unwrap_or_default();
    let steer_tip = steer_tip(store);

    ui.vertical(|ui| {
        ui.set_width(ui.available_width());
        let pills_on = goal.is_some()
            || review.is_some()
            || plan_on
            || swarm.is_some()
            || background.is_some()
            || todos.is_some();
        if pills_on {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(crate::theme::SP_3, crate::theme::SP_3);
                if let Some(goal) = &goal {
                    goal_pill(ui, palette, goal, &mut actions);
                }
                if review.is_some() || plan_on {
                    plan_pill(ui, palette, review.is_some(), &mut actions);
                }
                if let Some(swarm) = &swarm {
                    swarm_pill(ui, palette, swarm);
                }
                if let Some(background) = background {
                    background_pill(ui, palette, background, &mut actions);
                }
                if let Some(todos) = todos {
                    let entries = store.active_session().map(todo_entries).unwrap_or_default();
                    todos_pill(ui, palette, todos, entries);
                }
            });
            ui.add_space(crate::theme::SP_4);
        }
        if !queue.is_empty() {
            egui::Frame::NONE
                .fill(palette.surface)
                .stroke(egui::Stroke::new(1.0, palette.line))
                .corner_radius(egui::CornerRadius::same(crate::theme::R_LG as u8))
                .show_crisp(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for (index, row) in queue.iter().enumerate() {
                        queue_row(ui, palette, row, index > 0, steer_tip, &mut actions);
                    }
                });
            ui.add_space(crate::theme::SP_4);
        }
    });
    actions
}

/// Why a queued prompt cannot be steered right now, if it cannot.
fn steer_tip(store: &Store) -> Option<&'static str> {
    if !store.state().steer_available() {
        return Some("steering not advertised by the agent");
    }
    if !store
        .active_session()
        .is_some_and(|session| session.running)
    {
        return Some("nothing is running to steer into");
    }
    None
}

/// The goal pill: the goal text, the met check, and the popover that
/// edits or clears it.
fn goal_pill(
    ui: &mut egui::Ui,
    palette: &crate::theme::Palette,
    goal: &GoalState,
    actions: &mut Vec<DockAction>,
) {
    let (icon, color) = if goal.met {
        (crate::icons::Icon::CircleCheck, palette.ok)
    } else {
        (crate::icons::Icon::Target, palette.accent)
    };
    let inner = egui::Frame::NONE
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.line))
        .corner_radius(egui::CornerRadius::same(crate::theme::R_FULL as u8))
        .inner_margin(egui::Margin::symmetric(10, 0))
        .show_crisp(ui, |ui| {
            ui.set_min_height(PILL_H);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = crate::theme::SP_3;
                crate::icons::icon(ui, icon, 12.0, color);
                ui.label(
                    egui::RichText::new(goal.text.clone())
                        .size(crate::theme::FS_XS)
                        .color(color),
                );
            })
            .response
        })
        .inner;
    let rect = inner.rect;
    let id = egui::Id::new("dock-goal-pill").with(rect.top().to_bits());
    let response = ui.interact(rect, id, egui::Sense::click());
    if response.clicked() {
        actions.push(DockAction::OpenGoal);
    }
    let _ = response.on_hover_text("the session goal; click to edit or clear it");
}

/// The plan pill: under review it carries the success palette and
/// brings the plan card into view; in plan mode before any plan it
/// says where the plan will show up.
fn plan_pill(
    ui: &mut egui::Ui,
    palette: &crate::theme::Palette,
    pending: bool,
    actions: &mut Vec<DockAction>,
) {
    let (bg, line, fg, label) = if pending {
        (
            palette.ok_soft,
            palette.ok_bd,
            palette.ok,
            PLAN_PENDING.to_owned(),
        )
    } else {
        (
            palette.surface,
            palette.line,
            palette.muted,
            PLAN_WAITING.to_owned(),
        )
    };
    let inner = egui::Frame::NONE
        .fill(bg)
        .stroke(egui::Stroke::new(1.0, line))
        .corner_radius(egui::CornerRadius::same(crate::theme::R_FULL as u8))
        .inner_margin(egui::Margin::symmetric(10, 0))
        .show_crisp(ui, |ui| {
            ui.set_min_height(PILL_H);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = crate::theme::SP_3;
                crate::icons::icon(ui, crate::icons::Icon::PenLine, 12.0, fg);
                ui.label(
                    egui::RichText::new(label)
                        .size(crate::theme::FS_XS)
                        .color(fg),
                );
            })
            .response
        })
        .inner;
    let rect = inner.rect;
    let id = egui::Id::new("dock-plan-pill").with(rect.top().to_bits());
    let response = ui.interact(rect, id, egui::Sense::click());
    if pending && response.clicked() {
        actions.push(DockAction::ScrollToPlan);
    }
    let _ = response.on_hover_text(if pending {
        "a plan waits for review; click to find it in the transcript"
    } else {
        "plan mode is on"
    });
}

/// The swarm pill: the done/total count, the paused count, and the
/// member list a click opens above it.
fn swarm_pill(ui: &mut egui::Ui, palette: &crate::theme::Palette, swarm: &SwarmState) {
    let response = pill(ui, palette, "dock-swarm-pill", |ui| {
        crate::icons::icon(ui, crate::icons::Icon::Waypoints, 12.0, palette.done);
        ui.label(
            egui::RichText::new("Swarm")
                .size(crate::theme::FS_XS)
                .color(palette.done),
        );
        ui.label(
            egui::RichText::new(format!("{}/{}", swarm.done, swarm.total))
                .size(crate::theme::FS_XS)
                .family(crate::theme::family(crate::theme::FAMILY_MONO_REGULAR))
                .color(palette.ink_strong),
        );
        if swarm.paused > 0 {
            ui.label(
                egui::RichText::new(format!("{} paused", swarm.paused))
                    .size(crate::theme::FS_XS)
                    .color(palette.done),
            );
        }
    })
    .on_hover_text("delegated members and their states");
    egui::Popup::menu(&response)
        .align(egui::RectAlign::TOP_START)
        .gap(crate::theme::SP_3)
        .frame(
            egui::Frame::NONE
                .fill(palette.menu)
                .stroke(egui::Stroke::new(1.0, palette.line))
                .corner_radius(egui::CornerRadius::same(crate::theme::R_LG as u8))
                .inner_margin(egui::Margin::same(5)),
        )
        .show(|ui| {
            ui.set_width(SWARM_MENU_W - 10.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            for (name, word) in &swarm.members {
                let color = match *word {
                    "paused" => palette.warn,
                    "failed" => palette.danger,
                    "done" => palette.ok,
                    _ => palette.muted,
                };
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(9, 4))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = crate::theme::SP_4;
                            let word = egui::RichText::new(*word)
                                .size(crate::theme::FS_2XS)
                                .color(color);
                            let word_w = egui::WidgetText::from(word.clone())
                                .into_galley(
                                    ui,
                                    Some(egui::TextWrapMode::Extend),
                                    f32::INFINITY,
                                    egui::TextStyle::Body,
                                )
                                .size()
                                .x;
                            ui.scope(|ui| {
                                ui.set_max_width(
                                    ui.available_width() - word_w - crate::theme::SP_4,
                                );
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(name.clone())
                                            .size(crate::theme::FS_XS)
                                            .color(palette.ink),
                                    )
                                    .truncate(),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| ui.label(word),
                            );
                        });
                    });
            }
        });
}

/// The swarm member list's width.
const SWARM_MENU_W: f32 = 240.0;

/// The background agents pill: a spinner and how many run while any
/// does, else how many there were.
fn background_pill(
    ui: &mut egui::Ui,
    palette: &crate::theme::Palette,
    state: BackgroundState,
    actions: &mut Vec<DockAction>,
) {
    let noun = |n: usize| if n == 1 { "agent" } else { "agents" };
    let label = if state.live > 0 {
        format!("{} background {} running", state.live, noun(state.live))
    } else {
        format!("{} background {}", state.total, noun(state.total))
    };
    let response = pill(ui, palette, "dock-background-pill", |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = crate::theme::SP_3;
            let icon = if state.live > 0 {
                crate::icons::Icon::LoaderCircle
            } else {
                crate::icons::Icon::Bot
            };
            crate::icons::icon(ui, icon, 12.0, palette.muted);
            ui.label(
                egui::RichText::new(label)
                    .size(crate::theme::FS_XS)
                    .color(palette.muted),
            );
        });
    });
    if response.clicked() {
        actions.push(DockAction::OpenBackgroundAgents);
    }
}

/// The todos pill: the done/total count, its mini bar, and the entry
/// list a hover opens.
fn todos_pill(
    ui: &mut egui::Ui,
    palette: &crate::theme::Palette,
    todos: TodosState,
    entries: Vec<(String, String)>,
) {
    let count = format!("{}/{}", todos.done, todos.total);
    let response = pill(ui, palette, "dock-todos-pill", |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = crate::theme::SP_3;
            crate::icons::icon(ui, crate::icons::Icon::ListTodo, 12.0, palette.muted);
            ui.label(
                egui::RichText::new("Progress")
                    .size(crate::theme::FS_XS)
                    .color(palette.muted),
            );
            ui.label(
                egui::RichText::new(count)
                    .size(crate::theme::FS_XS)
                    .family(crate::theme::family(crate::theme::FAMILY_MONO_REGULAR))
                    .color(palette.ink_strong),
            );
            // The mini bar: a 36 by 4 track with the done share in the
            // success green, laid out after the count.
            let (track, _) =
                ui.allocate_exact_size(egui::vec2(TODO_BAR, 4.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(track, egui::CornerRadius::same(2), palette.fill_hover);
            let fill = TODO_BAR * todos.done as f32 / todos.total.max(1) as f32;
            ui.painter().rect_filled(
                egui::Rect::from_min_size(track.min, egui::vec2(fill, 4.0)),
                egui::CornerRadius::same(2),
                palette.ok,
            );
        });
    });
    let _ = response.on_hover_ui(|ui| {
        for (text, status) in &entries {
            let (icon, color, ink) = match status.as_str() {
                "completed" => (crate::icons::Icon::CircleCheck, palette.ok, palette.muted),
                "in_progress" => (
                    crate::icons::Icon::LoaderCircle,
                    palette.accent,
                    palette.ink_strong,
                ),
                _ => (crate::icons::Icon::Circle, palette.faint, palette.ink),
            };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = crate::theme::SP_4;
                crate::icons::icon(ui, icon, 13.0, color);
                let mut text = egui::RichText::new(text.clone())
                    .size(crate::theme::FS_SM)
                    .color(ink);
                if status == "completed" {
                    text = text.strikethrough();
                }
                ui.label(text);
            });
        }
    });
}

/// One queue row with its three actions. `steer_tip` disables the
/// steer button with the reason when a steer cannot go out, and
/// `separator` draws the hairline above every row after the first.
fn queue_row(
    ui: &mut egui::Ui,
    palette: &crate::theme::Palette,
    row: &QueueRow,
    separator: bool,
    steer_tip: Option<&'static str>,
    actions: &mut Vec<DockAction>,
) {
    let response = egui::Frame::NONE
        .fill(egui::Color32::TRANSPARENT)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(34.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = crate::theme::SP_4;
                ui.add_space(crate::theme::SP_5);
                ui.label(
                    egui::RichText::new("Queued")
                        .size(crate::theme::FS_2XS)
                        .color(palette.faint),
                );
                let text = egui::RichText::new(row.text.clone())
                    .size(crate::theme::FS_SM)
                    .color(palette.ink);
                ui.add(egui::Label::new(text).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let tones = crate::ui::forms::IconTones {
                        fill: egui::Color32::TRANSPARENT,
                        hover_fill: palette.hover,
                        ink: palette.muted,
                        hover_ink: palette.ink,
                    };
                    let button = |ui: &mut egui::Ui, label: &str, icon| {
                        crate::ui::forms::icon_btn(
                            ui,
                            label,
                            icon,
                            crate::theme::CTL_ICO,
                            12.0,
                            crate::theme::R_SM,
                            tones,
                        )
                    };
                    if button(ui, "Remove", crate::icons::Icon::X)
                        .on_hover_text("Remove")
                        .clicked()
                    {
                        actions.push(DockAction::Remove(row.index));
                    }
                    if button(ui, "Edit", crate::icons::Icon::Pencil)
                        .on_hover_text("fills the composer draft and leaves the queue")
                        .clicked()
                    {
                        actions.push(DockAction::Edit(row.index));
                    }
                    let steer = ui
                        .scope(|ui| {
                            if steer_tip.is_some() {
                                ui.set_opacity(0.45);
                            }
                            button(ui, "Steer now", crate::icons::Icon::CornerDownLeft)
                        })
                        .inner;
                    if steer_tip.is_none() && steer.clicked() {
                        actions.push(DockAction::Steer(row.index));
                    }
                    let _ = steer.on_hover_text(steer_tip.unwrap_or("Steer now"));
                });
            });
        })
        .response;
    if separator {
        ui.painter().hline(
            response.rect.x_range(),
            response.rect.top(),
            egui::Stroke::new(1.0, palette.subtle),
        );
    }
}

/// One pill named `name`: 28px tall, fully round, bordered, in the
/// surface color. Each pill keeps its own name, so two pills on one
/// row never share their click and hover state.
fn pill(
    ui: &mut egui::Ui,
    palette: &crate::theme::Palette,
    name: &str,
    body: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let inner = egui::Frame::NONE
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.line))
        .corner_radius(egui::CornerRadius::same(crate::theme::R_FULL as u8))
        .inner_margin(egui::Margin::symmetric(10, 0))
        .show_crisp(ui, |ui| {
            ui.set_min_height(PILL_H);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = crate::theme::SP_3;
                body(ui);
            })
            .response
        })
        .inner;
    ui.interact(inner.rect, ui.id().with(name), egui::Sense::click())
}

#[cfg(test)]
mod tests {
    use super::{
        BackgroundState, GoalState, TodosState, background_state, goal_state, prompt_text,
        queue_rows, swarm_state, todo_entries, todos_state, truncate_text,
    };
    use kage_client::wire::{ContentBlock, SubagentState};
    use kage_client::{Session, Subagent, TranscriptItem};
    use serde_json::json;

    /// A session with `agents` and `items` set, everything else empty.
    fn session() -> Session {
        Session {
            id: "s1".to_owned(),
            opened: true,
            parent: None,
            cwd: None,
            external_agent: None,
            title: None,
            updated_at: None,
            items: Vec::new(),
            usage: Default::default(),
            mode: None,
            config_options: Vec::new(),
            commands: Vec::new(),
            mcp: Default::default(),
            permissions: Vec::new(),
            queue: Vec::new(),
            running: false,
            in_turn: false,
            compacting: false,
            agents: Default::default(),
            plan: None,
            last_stop: None,
            draft: None,
        }
    }

    /// A config option frame carrying `id` at `value`.
    fn option(id: &str, value: &str) -> kage_client::wire::SessionConfigOption {
        kage_client::wire::SessionConfigOption {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            category: None,
            kind: kage_client::wire::SessionConfigKind::Select,
            current_value: value.to_owned(),
            options: Vec::new(),
        }
    }

    #[test]
    fn the_goal_pill_hides_until_a_goal_is_set() {
        let mut session = session();
        assert_eq!(goal_state(&session), None, "no option, no pill");
        session.config_options.push(option("goal", ""));
        assert_eq!(goal_state(&session), None, "an empty goal is no goal");
        session.config_options[0].current_value = "ship the port".to_owned();
        assert_eq!(
            goal_state(&session),
            Some(GoalState {
                text: "ship the port".to_owned(),
                met: false
            })
        );
    }

    #[test]
    fn a_success_notice_marks_the_goal_met() {
        let mut session = session();
        session.config_options.push(option("goal", "ship the port"));
        session.items.push(TranscriptItem::Notice {
            tone: kage_client::wire::NoticeTone::Success,
            text: "goal met: shipped".to_owned(),
        });
        let state = goal_state(&session).expect("the goal is set");
        assert!(state.met, "the success notice met it");
    }

    #[test]
    fn a_plain_success_notice_does_not_meet_the_goal() {
        let mut session = session();
        session.config_options.push(option("goal", "ship the port"));
        session.items.push(TranscriptItem::Notice {
            tone: kage_client::wire::NoticeTone::Success,
            text: "shipped".to_owned(),
        });
        let state = goal_state(&session).expect("the goal is set");
        assert!(!state.met, "only the goal notice meets it");
    }

    #[test]
    fn the_swarm_pill_counts_the_foreground_members() {
        let mut session = session();
        let mut running = Subagent {
            name: Some("explore".to_owned()),
            task: None,
            capabilities: None,
            state: None,
            swarm: None,
            reason: None,
            tool_call_id: None,
            usage: None,
            model: None,
            background: false,
        };
        session.agents.insert("a1".to_owned(), running.clone());
        running.state = Some(SubagentState::Paused);
        session.agents.insert("a2".to_owned(), running);
        let state = swarm_state(&session).expect("two members run or pause");
        assert_eq!(state.total, 2);
        assert_eq!(state.paused, 1);
        assert_eq!(state.done, 0);
        assert_eq!(state.members[0], ("explore".to_owned(), "running"));
        assert_eq!(state.members[1], ("explore".to_owned(), "paused"));
    }

    #[test]
    fn a_finished_swarm_hides_its_pill() {
        let mut session = session();
        session.agents.insert(
            "a1".to_owned(),
            Subagent {
                name: None,
                task: None,
                capabilities: None,
                state: Some(SubagentState::Completed),
                swarm: None,
                reason: None,
                tool_call_id: None,
                usage: None,
                model: None,
                background: false,
            },
        );
        assert_eq!(swarm_state(&session), None, "nothing live, no pill");
    }

    #[test]
    fn background_agents_have_their_own_pill() {
        let mut session = session();
        let agent = Subagent {
            name: None,
            task: None,
            capabilities: None,
            state: Some(SubagentState::Running),
            swarm: None,
            reason: None,
            tool_call_id: None,
            usage: None,
            model: None,
            background: true,
        };
        session.agents.insert("b1".to_owned(), agent.clone());
        session.agents.insert("b2".to_owned(), agent);
        assert_eq!(
            background_state(&session),
            Some(BackgroundState { live: 2, total: 2 })
        );
        assert_eq!(swarm_state(&session), None, "the swarm pill stays closed");
    }

    #[test]
    fn the_todos_pill_counts_the_latest_plan() {
        let mut session = session();
        assert_eq!(todos_state(&session), None, "no plan, no pill");
        session.items.push(TranscriptItem::Plan {
            entries: vec![
                json!({"content": "first", "status": "completed"}),
                json!({"content": "second", "status": "pending"}),
            ],
        });
        assert_eq!(
            todos_state(&session),
            Some(TodosState { done: 1, total: 2 })
        );
        assert_eq!(
            todo_entries(&session),
            vec![
                ("first".to_owned(), "completed".to_owned()),
                ("second".to_owned(), "pending".to_owned())
            ]
        );
    }

    #[test]
    fn the_queue_rows_truncate_and_keep_the_full_text() {
        let mut session = session();
        session.queue.push(kage_client::QueuedPrompt {
            prompt: vec![ContentBlock::text("hold this")],
        });
        session.queue.push(kage_client::QueuedPrompt {
            prompt: vec![ContentBlock::text("x".repeat(90))],
        });
        let rows = queue_rows(&session);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].index, 0);
        assert_eq!(rows[0].text, "hold this");
        assert_eq!(rows[0].full, "hold this");
        assert_eq!(
            rows[1].text.chars().count(),
            81,
            "80 columns and an ellipsis"
        );
        assert_eq!(rows[1].full.len(), 90);
    }

    #[test]
    fn a_prompts_text_blocks_join_with_a_space() {
        let prompt = vec![ContentBlock::text("first"), ContentBlock::text("second")];
        assert_eq!(prompt_text(&prompt), "first second");
    }

    #[test]
    fn text_cuts_at_the_column_with_an_ellipsis() {
        assert_eq!(truncate_text("short", 10), "short");
        assert_eq!(truncate_text("abcdefghij", 3), "abc\u{2026}");
    }

    /// A store with one open session, so an option write rides the
    /// wire and the answer is readable.
    fn store_with_session() -> crate::store::Store {
        let mut store = crate::store::Store::new("/w", false);
        store.set_connect(crate::transport::State::Connected);
        store.handshake(false);
        store.new_session();
        let _ = store.take_outgoing();
        store.absorb(kage_client::Frame::Success {
            id: kage_client::RequestId::Number(2),
            result: serde_json::json!({"sessionId": "s1"}),
        });
        let _ = store.take_outgoing();
        store
    }

    /// The value the last option write for `id` carried.
    fn last_option_write(store: &mut crate::store::Store, id: &str) -> Option<String> {
        store
            .take_outgoing()
            .into_iter()
            .filter_map(|frame| match frame {
                kage_client::Frame::Request { method, params, .. }
                    if method == "session/set_config_option" =>
                {
                    (params.get("configId").and_then(serde_json::Value::as_str) == Some(id)).then(
                        || {
                            params
                                .get("value")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_owned()
                        },
                    )
                }
                _ => None,
            })
            .next_back()
    }

    #[test]
    fn a_goal_save_trims_and_writes_the_option() {
        let mut store = store_with_session();
        assert!(super::save_goal(&mut store, "  ship the port  "));
        assert_eq!(
            last_option_write(&mut store, "goal").as_deref(),
            Some("ship the port"),
            "the text is trimmed on the way in"
        );
    }

    #[test]
    fn a_blank_goal_clears_the_option() {
        let mut store = store_with_session();
        assert!(super::save_goal(&mut store, "ship the port"));
        let _ = last_option_write(&mut store, "goal");
        assert!(super::save_goal(&mut store, "   "));
        assert_eq!(
            last_option_write(&mut store, "goal").as_deref(),
            Some(""),
            "a blank text clears the goal"
        );
    }
}
