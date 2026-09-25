//! Footer hints, the working row, the pinned agent list and the
//! other chrome around the conversation.

use super::*;

impl App {
    /// Snapshot the slot specs for one frame and report the frame's
    /// width and editor mode to the plugin runtime.
    pub(crate) fn slot_frame(&self, width: u16) -> kage_plugin::SlotSpecs {
        let Some(slots) = &self.slots else {
            return kage_plugin::SlotSpecs::default();
        };
        slots.report(width, mode_label(self.input.mode()));
        slots.specs()
    }

    /// The footer hint: the pending keys of a mapping sequence, else
    /// the keys of the open panel or overlay, else what the next keys
    /// do in the current state. Kept short so it fits at 80 columns
    /// next to the session facts: while the main session has agents,
    /// the key that lists them takes the place of the queue or help
    /// hint of a run in flight.
    pub(crate) fn footer_hint(&mut self) -> String {
        let keys = self.sequencer.pending();
        if !keys.is_empty() {
            return format!("{} ...", crate::keymap::key_labels(keys));
        }
        if let Some(hint) = self.layer_hint() {
            return hint;
        }
        let now = Instant::now();
        let note = self
            .escalation
            .filter(|(_, until)| *until > now && !self.input.has_draft());
        match note {
            Some((keys::Escalation::QuitArmed, _)) => return "ctrl+c again to quit".to_owned(),
            Some((keys::Escalation::DraftCleared, _)) => {
                return "draft cleared, up restores it".to_owned();
            }
            None => {}
        }
        let working = self.is_run_in_flight();
        let draft = !self.input.text().is_empty();
        if self.input.shell_armed() {
            return if draft {
                "enter to run the command"
            } else {
                "backspace to leave shell mode"
            }
            .to_owned();
        }
        if !draft && let Some(hint) = self.agent_hint(working) {
            return hint;
        }
        let label =
            |app: &mut Self, action, what| app.key_label(action).map(|key| format!("{key} {what}"));
        let queue = label(self, "QueuePrompt", "to queue").filter(|_| working);
        let queues = label(self, "QueuePrompt", "queues").filter(|_| working);
        let has_agents = working
            && self.active_session.is_some_and(|main| {
                self.agents.under(main).iter().any(|(_, node)| {
                    matches!(
                        node.state,
                        kage_core::protocol::AgentState::Queued
                            | kage_core::protocol::AgentState::Running
                    )
                })
            });
        let agents = label(self, "OpenAgents", "for agents").filter(|_| has_agents);
        let (queue, queues, agents) = (queue.as_deref(), queues.as_deref(), agents.as_deref());
        let mut parts: Vec<&str> = Vec::new();
        if self.input.is_modeless() {
            match (working, draft) {
                (true, false) => {
                    parts.extend(agents.or(queue).into_iter().chain(["esc to interrupt"]));
                }
                (true, true) => {
                    parts.push("enter steers");
                    parts.extend(queues);
                    parts.push("esc clears");
                }
                (false, true) => parts.extend(["enter to send", "shift+enter for a newline"]),
                (false, false) if self.search_pattern.is_some() => {
                    parts.extend(["esc to clear the search", "? for shortcuts"]);
                }
                (false, false) => parts.extend(["? for shortcuts", "/ for commands"]),
            }
            return parts.join(HINT_SEP);
        }
        let help = label(self, "OpenHelp", "for shortcuts");
        let commands = label(self, "BeginCommand", "for commands");
        match self.input.mode() {
            Mode::Normal => {
                match (working, draft) {
                    (_, true) => parts.push("ctrl+c to clear"),
                    (true, false) => parts.push("ctrl+c to interrupt"),
                    (false, false) => {}
                }
                parts.push("i to type");
                parts.extend(agents.filter(|_| !draft).or(help.as_deref()));
                if !working && !draft {
                    parts.extend(commands.as_deref());
                }
            }
            Mode::Insert => match (working, draft) {
                (true, false) => {
                    parts.extend(agents.or(queue).into_iter().chain(["ctrl+c to interrupt"]));
                }
                (true, true) => {
                    parts.push("enter steers");
                    parts.extend(queues);
                    parts.push("ctrl+c clears");
                }
                (false, true) => parts.extend(["enter to send", "esc for normal mode"]),
                (false, false) => parts.push("esc for normal mode"),
            },
            Mode::Visual => parts.push("esc to leave visual mode"),
        }
        parts.join(HINT_SEP)
    }

    /// The footer hint of the layer above the editor that takes the
    /// keys, in the order [`Self::dispatch_key`] offers them: a plugin
    /// dialog, the agents overlay, the approval panel, the context
    /// menu, a picker or dialog, the palette, the `:` and search lines,
    /// then the completion popup.
    fn layer_hint(&self) -> Option<String> {
        use crate::overlay::OverlayWidget;
        if let Some(overlay) = self.plugin_overlay.as_deref() {
            return Some(overlay.footer_hint().to_owned());
        }
        if let Some(overlay) = &self.agents_overlay {
            return Some(overlay.footer_hint().to_owned());
        }
        if let Some(panel) = &self.approval_panel {
            return Some(panel.hint());
        }
        let overlay: Option<&dyn OverlayWidget> = if let Some(p) = &self.picker {
            Some(p)
        } else if let Some(s) = &self.settings_overlay {
            Some(s)
        } else if let Some(t) = &self.session_tree {
            Some(t)
        } else if let Some(h) = &self.help_overlay {
            Some(h)
        } else if let Some(p) = &self.slash_palette {
            Some(p)
        } else {
            None
        };
        if let Some(overlay) = overlay.filter(|_| self.context_menu.is_none()) {
            return Some(overlay.footer_hint().to_owned());
        }
        let parts: &[&str] = if self.context_menu.is_some() {
            &["enter to run", "esc to close"]
        } else if self.cmdline.is_some() {
            &["tab to complete", "enter to run", "esc to close"]
        } else if self.search_line.is_some() {
            &["up/down for matches", "enter to keep", "esc to cancel"]
        } else if self.input_completion.is_some() {
            &["enter to complete", "esc to close"]
        } else {
            return None;
        };
        Some(parts.join(HINT_SEP))
    }

    /// The footer hint of an agent view with an empty draft: how to
    /// steer or message the agent, how to go back, and how to stop it
    /// while `working`. An agent of a resumed session only goes back.
    /// `None` in the main view and in visual mode.
    fn agent_hint(&self, working: bool) -> Option<String> {
        self.focus?;
        let stop = "ctrl+c to stop";
        let read_only = self.focused_read_only();
        let parts = match (self.input.is_modeless(), self.input.mode(), working) {
            (true, _, _) | (false, Mode::Normal, _) if read_only => vec!["esc to go back"],
            (false, Mode::Insert, _) if read_only => vec!["ctrl+c to go back"],
            (true, _, true) => vec!["enter to steer", "esc to go back", stop],
            (true, _, false) => vec!["enter to send", "esc to go back"],
            (false, Mode::Insert, true) => vec!["enter to steer", stop],
            (false, Mode::Insert, false) => vec!["enter to send", "ctrl+c to go back"],
            (false, Mode::Normal, true) => vec![stop, "esc to go back", "i to type"],
            (false, Mode::Normal, false) => vec!["esc to go back", "i to type"],
            (false, Mode::Visual, _) => return None,
        };
        Some(parts.join(HINT_SEP))
    }

    /// The key that runs `action` in the current editing state, as
    /// `ctrl+p`. A mapping from `init.lua` or `config.toml` wins over
    /// the defaults. Cached per keymap generation and editing state.
    pub(crate) fn key_label(&mut self, action: &'static str) -> Option<String> {
        let keymap = lock(&self.keymap);
        let state = self.edit_state();
        let key = (keymap.generation(), state);
        if self.key_labels.key != Some(key) {
            self.key_labels = KeyLabels {
                key: Some(key),
                labels: Vec::new(),
            };
        }
        if let Some((_, label)) = self.key_labels.labels.iter().find(|(a, _)| *a == action) {
            return label.clone();
        }
        let modes = state.modes();
        let entries = keymap.entries();
        let runs = |e: &&kage_core::keymap::Entry<'_>| {
            modes.contains(&e.mode)
                && matches!(e.mapping.rhs, Rhs::Action { name, .. } if name == action)
        };
        let label = entries
            .iter()
            .filter(runs)
            .find(|e| e.mapping.user_owned())
            .or_else(|| entries.iter().find(runs))
            .map(|e| crate::keymap::key_labels(e.lhs));
        self.key_labels.labels.push((action, label.clone()));
        label
    }

    /// The working row text while a run of the session on screen is in
    /// flight: what it is doing, the run's elapsed time and, in the
    /// main view when the next key would reach the editor with an empty
    /// draft, the key that interrupts the run. At `width` columns what
    /// it is doing is cut first, so the time and the key stay.
    pub(crate) fn activity_label(&self, buffer: &crate::Buffer, width: u16) -> Option<String> {
        use kage_core::protocol::AgentState;
        let started = match self.focus {
            None => self.run_started?,
            Some(session) => {
                self.agents
                    .get(session)
                    .filter(|n| n.state == AgentState::Running)?
                    .started?
            }
        };
        let approving = self.pending_permission.is_some();
        let doing = if approving {
            "Waiting for your approval".to_owned()
        } else {
            current_work(buffer, self.live_agents())
        };
        let elapsed = started.elapsed();
        let elapsed = if elapsed.as_secs() < 60 {
            format!("{}s", elapsed.as_secs())
        } else {
            view::tool_view::format_elapsed(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        };
        let tail = if self.focus.is_some() || approving || !self.input.text().is_empty() {
            format!(" ({elapsed})")
        } else {
            let key = if self.input.is_modeless() {
                "esc"
            } else {
                "ctrl+c"
            };
            format!(" ({elapsed}, {key} to interrupt)")
        };
        let room = usize::from(width).saturating_sub(ACTIVITY_INDENT + tail.len());
        let doing = view::truncate_to_width(&doing, room, "...");
        Some(format!("{doing}{tail}"))
    }

    /// How many agents directly under the session on screen are queued
    /// or running. Their own agents are theirs to wait for.
    fn live_agents(&self) -> usize {
        use kage_core::protocol::AgentState;
        self.view_root().map_or(0, |root| {
            self.agents
                .under(root)
                .into_iter()
                .filter(|(depth, node)| {
                    *depth == 1 && matches!(node.state, AgentState::Queued | AgentState::Running)
                })
                .count()
        })
    }

    /// The pinned list: the queued, running and waiting agents under
    /// the session on screen in tree order, with what each does now.
    /// Direct agents follow the order of their cards in the
    /// conversation, which can differ from the order they started in.
    /// Empty while the approval panel is open.
    pub(crate) fn agent_rows(&self) -> Vec<view::AgentRow> {
        use kage_core::protocol::AgentState;
        let Some(root) = self.view_root().filter(|_| self.approval_panel.is_none()) else {
            return Vec::new();
        };
        let cards: std::collections::HashMap<String, usize> = lock(&self.buffer)
            .blocks()
            .iter()
            .enumerate()
            .filter_map(|(i, block)| match block {
                crate::Block::ToolCall { call_id, name, .. } if name == "agent" => {
                    Some((call_id.clone(), i))
                }
                _ => None,
            })
            .collect();
        let mut card = usize::MAX;
        let mut nodes: Vec<_> = self
            .agents
            .under(root)
            .into_iter()
            .map(|(depth, node)| {
                if depth == 1 {
                    card = cards
                        .get(&node.tool_call_id.0)
                        .copied()
                        .unwrap_or(usize::MAX);
                }
                (card, depth, node)
            })
            .collect();
        nodes.sort_by_key(|(card, _, _)| *card);
        nodes
            .into_iter()
            .filter_map(|(_, depth, node)| {
                let state = match node.state {
                    AgentState::Queued => view::AgentRowState::Queued,
                    AgentState::Running if node.waiting > 0 => view::AgentRowState::Waiting,
                    AgentState::Running => view::AgentRowState::Running,
                    AgentState::Done | AgentState::Failed | AgentState::Cancelled => return None,
                };
                let activity = match (state, self.agent_buffers.get(&node.session)) {
                    (view::AgentRowState::Running, Some(buffer)) => {
                        super::engine::agent_activity(&lock(buffer))
                    }
                    _ => String::new(),
                };
                Some(view::AgentRow {
                    session: node.session,
                    depth,
                    agent: node.agent.clone(),
                    description: node.description.clone(),
                    state,
                    activity,
                    elapsed_ms: node
                        .elapsed()
                        .map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
                })
            })
            .collect()
    }

    /// The agents overlay's rows: the main session, then every agent
    /// under it in tree order, live or finished.
    pub(crate) fn agents_overlay_rows(&self) -> Vec<crate::overlay::AgentsRow> {
        use crate::overlay::{AgentsRow, AgentsRowState};
        use kage_core::protocol::AgentState;
        let Some(main) = self.active_session else {
            return Vec::new();
        };
        let ms = |d: std::time::Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let usage = self.session_usage_snapshot().unwrap_or_default();
        let running = self.session_running(None);
        let title = self
            .slots
            .as_ref()
            .and_then(|slots| lock(&slots.ui_state()).session_title.clone())
            .or_else(|| first_prompt(&lock(&self.root_buffer)))
            .unwrap_or_else(|| "main session".to_owned());
        let mut rows = vec![AgentsRow {
            session: None,
            depth: 0,
            name: "kage".to_owned(),
            title,
            state: if running {
                AgentsRowState::Running
            } else {
                AgentsRowState::Idle
            },
            activity: String::new(),
            elapsed_ms: self
                .run_started
                .filter(|_| running)
                .map(|t| ms(t.elapsed())),
            tokens: usage.total_tokens(),
            cost: usage.total_cost,
        }];
        rows.extend(self.agents.under(main).into_iter().map(|(depth, node)| {
            let state = match node.state {
                AgentState::Queued => AgentsRowState::Queued,
                AgentState::Running if node.waiting > 0 => AgentsRowState::Waiting,
                AgentState::Running => AgentsRowState::Running,
                AgentState::Done => AgentsRowState::Done,
                AgentState::Failed => AgentsRowState::Failed,
                AgentState::Cancelled => AgentsRowState::Stopped,
            };
            let activity = match (state, self.agent_buffers.get(&node.session)) {
                (AgentsRowState::Running, Some(buffer)) => {
                    super::engine::agent_activity(&lock(buffer))
                }
                _ => String::new(),
            };
            AgentsRow {
                session: Some(node.session),
                depth,
                name: node.agent.clone(),
                title: node.description.clone(),
                state,
                activity,
                elapsed_ms: node.elapsed().map(ms),
                tokens: node.usage.total.input + node.usage.total.output,
                cost: node.usage.cost,
            }
        }));
        rows
    }

    /// What the `breadcrumb` component shows for the agent on screen:
    /// its ancestry, task, state, time, tokens and tool count. `None` in
    /// the main view.
    pub(crate) fn breadcrumb(&self) -> Option<view::Breadcrumb> {
        use kage_core::protocol::AgentState;
        let node = self.agents.get(self.focus?)?;
        let mut trail = vec![node.agent.clone()];
        let mut parent = node.parent;
        while let Some(up) = self.agents.get(parent) {
            trail.insert(0, up.agent.clone());
            parent = up.parent;
        }
        let state = match node.state {
            AgentState::Queued => "queued",
            AgentState::Running if node.waiting > 0 => "waiting",
            AgentState::Running => "running",
            AgentState::Done => "done",
            AgentState::Failed => "failed",
            AgentState::Cancelled => "stopped",
        };
        Some(view::Breadcrumb {
            trail,
            description: node.description.clone(),
            state,
            elapsed_ms: node
                .elapsed()
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            tokens: node.usage.total.input + node.usage.total.output,
            tool_calls: if node.restored {
                self.agent_buffers.get(&node.session).map_or(0, |buffer| {
                    let calls = lock(buffer)
                        .blocks()
                        .iter()
                        .filter(|b| matches!(b, crate::Block::ToolCall { .. }))
                        .count();
                    u32::try_from(calls).unwrap_or(u32::MAX)
                })
            } else {
                node.tool_calls
            },
        })
    }

    /// The empty draft's placeholder in an agent view: steer the agent
    /// while it runs, else message it, or say that an agent of a
    /// resumed session cannot be messaged. `None` in the main view.
    pub(crate) fn agent_placeholder(&self) -> Option<String> {
        let agent = self.focused_agent()?;
        Some(if self.focused_read_only() {
            format!("{agent} cannot be messaged after a resume")
        } else if self.is_run_in_flight() {
            format!("Steer {agent}")
        } else {
            format!("Message {agent} (the reply stays in this agent)")
        })
    }

    /// The active model's id: the engine's last report, else the
    /// model the host started with.
    pub(crate) fn model_id(&self, usage: Option<&crate::usage::SessionUsage>) -> Option<String> {
        match usage.map(|u| u.model.as_str()).filter(|m| !m.is_empty()) {
            Some(id) => Some(id.to_owned()),
            None => self.status_model.as_ref().map(|m| lock(m).clone()),
        }
    }

    /// The model picker's label for the model `id`, else the id.
    pub(crate) fn model_label(&self, id: Option<&str>) -> Option<String> {
        let id = id?;
        let label = self
            .model_choices
            .iter()
            .find(|item| item.value == id)
            .map_or(id, |item| item.label.as_str());
        Some(label.to_owned())
    }

    /// Keys for the start card's change hints, while it shows.
    pub(crate) fn start_keys(&mut self) -> view::StartKeys {
        view::StartKeys {
            model: self.key_label("OpenModelPicker"),
            thinking: self.key_label("CycleThinkingLevel"),
            sessions: self.key_label("OpenSessionPicker"),
        }
    }
}

/// Separator between the parts of a footer hint.
const HINT_SEP: &str = " \u{B7} ";

/// Columns the `activity` component paints before its text.
const ACTIVITY_INDENT: usize = 2;

/// Labels from [`App::key_label`], valid for one keymap generation and
/// editing state.
#[derive(Debug, Default)]
pub(crate) struct KeyLabels {
    key: Option<(u64, EditState)>,
    labels: Vec<(&'static str, Option<String>)>,
}

/// The first line of the first prompt in `buffer`, which names a
/// session until its title arrives.
fn first_prompt(buffer: &crate::Buffer) -> Option<String> {
    buffer.blocks().iter().find_map(|block| match block {
        crate::Block::User { text } => text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned),
        _ => None,
    })
}

/// What the current run is doing, from the newest blocks back to the
/// prompt that started it: running a tool, thinking, waiting for its
/// `agents` live agents, or just working.
fn current_work(buffer: &crate::Buffer, agents: usize) -> String {
    use crate::view::tool_view::{ToolPhase, describe};
    for block in buffer.blocks().iter().rev() {
        match block {
            crate::Block::User { .. } => break,
            crate::Block::ToolCall {
                name,
                phase: ToolPhase::Running,
                ..
            } if name == "agent" => {}
            crate::Block::ToolCall {
                name,
                input,
                phase: ToolPhase::Running,
                ..
            } => {
                let label = describe(name, input);
                return format!("{} {}", label.verb_live, label.target)
                    .trim_end()
                    .to_owned();
            }
            crate::Block::Thinking { live: true, .. } if agents == 0 => {
                return "Thinking".to_owned();
            }
            _ => {}
        }
    }
    match agents {
        0 => "Working".to_owned(),
        1 => "Waiting for 1 agent".to_owned(),
        n => format!("Waiting for {n} agents"),
    }
}
