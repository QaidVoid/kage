//! Applying engine events to the App: buffer, modeline, notices,
//! permission prompts, and the agent sessions started under the main
//! session.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

use kage_core::protocol::{AgentState, Envelope, Event, HostEvent, NoticeLevel, RequestId};
use kage_core::{LoopEvent, Role, SessionId, ToolCallId};

use crate::view::tool_view::{ToolPhase, agent_stats, describe};

/// A permission request waiting for, or shown in, the approval prompt.
#[derive(Clone, Debug)]
pub(crate) struct PendingApproval {
    /// Id to answer with.
    pub(crate) request_id: RequestId,
    /// The session that asked.
    pub(crate) session: SessionId,
    /// The asking agent's name and task. `None` when the main session
    /// asked.
    pub(crate) agent: Option<(String, String)>,
    /// The gated tool call, whose row shows the approval state.
    pub(crate) tool_call_id: Option<String>,
    /// Tool name.
    pub(crate) tool: String,
    /// The full tool input the panel summarizes.
    pub(crate) input: serde_json::Value,
}

impl App {
    /// Receive the engine events this App renders.
    pub fn set_engine_events(&mut self, rx: std::sync::mpsc::Receiver<Envelope>) {
        self.engine_rx = Some(rx);
    }

    /// Apply every queued engine event and show a waiting permission
    /// prompt if the screen is free. Returns whether anything changed.
    pub(crate) fn drain_engine_events(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = self.engine_rx.take() {
            while let Ok(envelope) = rx.try_recv() {
                self.apply_envelope(envelope);
                changed = true;
            }
            self.engine_rx = Some(rx);
        }
        self.open_next_permission() || changed
    }

    fn apply_envelope(&mut self, envelope: Envelope) {
        if let Event::Host(HostEvent::SessionChanged { .. }) = &envelope.event
            && self.agents.get(envelope.session).is_none()
        {
            self.active_session = Some(envelope.session);
            self.status_session_id = Some(envelope.session.to_string().chars().take(8).collect());
        }
        let main = *self.active_session.get_or_insert(envelope.session);
        if envelope.session != main {
            if self.agents.apply(&envelope) && self.agents.root_of(envelope.session) == main {
                self.apply_agent_envelope(envelope);
            }
            return;
        }
        match envelope.event {
            Event::Loop(event) => {
                if let LoopEvent::MessageAppended { message } = &event
                    && message.role == Role::User
                {
                    self.pending_delivered(None);
                }
                crate::events::apply_loop_event(&mut lock(&self.root_buffer), &event);
                if let LoopEvent::ToolCallEnd { id, .. } = &event {
                    self.card_ended(main, id);
                }
            }
            Event::Host(event) => self.apply_host_event(event),
        }
    }

    /// Drop the pending row of the prompt the engine just delivered to
    /// `session` (`None` for the main session). Rows are counted, not
    /// matched by text, since an `input` plugin may rewrite a prompt.
    /// The engine delivers steers first.
    fn pending_delivered(&mut self, session: Option<SessionId>) {
        let rows = || {
            self.pending
                .iter()
                .enumerate()
                .filter(|(_, (s, _))| *s == session)
        };
        let at = rows()
            .find(|(_, (_, p))| !p.queued)
            .or_else(|| rows().next())
            .map(|(at, _)| at);
        if let Some(at) = at {
            self.pending.remove(at);
        }
    }

    fn apply_host_event(&mut self, event: HostEvent) {
        match event {
            HostEvent::StateChanged { state } => {
                if let Some(model) = &self.status_model {
                    lock(model).clone_from(&state.model);
                }
                if let Some(usage) = &self.session_usage {
                    let mut usage = lock(usage);
                    usage.model = state.model;
                    usage.thinking_level = Some(state.thinking);
                    usage.permission_mode = state.permission_mode;
                    usage.working = state.working;
                }
            }
            HostEvent::UsageUpdated { usage: totals } => {
                if let Some(usage) = &self.session_usage {
                    let mut usage = lock(usage);
                    usage.input_tokens = totals.total.input;
                    usage.output_tokens = totals.total.output;
                    usage.cache_read_tokens = totals.total.cache_read;
                    usage.cache_write_tokens = totals.total.cache_write;
                    usage.current_context = totals.context_used;
                    usage.context_window = totals.context_window;
                    usage.total_cost = totals.cost;
                }
            }
            HostEvent::Notice {
                level,
                text,
                transient: true,
            } => self.toast(level, text),
            HostEvent::Notice { level, text, .. } => push_notice(&self.root_buffer, level, text),
            HostEvent::SessionChanged { messages, .. } => {
                self.set_focus(None);
                self.drafts.clear();
                self.pending.clear();
                self.agents.clear();
                self.agent_buffers.clear();
                self.agents
                    .restore(self.active_session.unwrap_or_default(), &messages);
                let durations = crate::events::tool_durations(&messages);
                {
                    let mut buf = lock(&self.root_buffer);
                    buf.clear();
                    crate::events::populate_from_history(&mut buf, &messages, &durations);
                }
                self.on_session_changed();
            }
            HostEvent::ShellFinished {
                command,
                output,
                exit_code,
            } => push_shell(&self.root_buffer, &command, &output, exit_code),
            HostEvent::PermissionRequested {
                request_id,
                tool_call_id,
                tool,
                input,
                ..
            } => {
                let session = self.active_session.unwrap_or_default();
                self.push_approval(PendingApproval {
                    request_id,
                    session,
                    agent: None,
                    tool_call_id: tool_call_id.map(|id| id.to_string()),
                    tool,
                    input,
                });
            }
            HostEvent::PermissionResolved { request_id } => self.drop_permission(request_id),
            HostEvent::RunStarted => self.run_started = Some(Instant::now()),
            HostEvent::RunEnded { .. } => {
                self.run_started = None;
                self.end_run(self.active_session.unwrap_or_default());
            }
            HostEvent::TitleChanged { .. }
            | HostEvent::AgentSpawned { .. }
            | HostEvent::McpServers { .. } => {}
        }
    }

    /// Apply an envelope of an agent under the main session to that
    /// agent's buffer, and refresh its card when the event changes what
    /// the card shows.
    fn apply_agent_envelope(&mut self, envelope: Envelope) {
        let session = envelope.session;
        if let Event::Host(HostEvent::AgentSpawned { .. }) = &envelope.event {
            self.agent_buffers
                .insert(session, crate::events::shared_buffer());
        }
        let Some(buffer) = self.agent_buffers.get(&session).map(Arc::clone) else {
            return;
        };
        let card = match envelope.event {
            Event::Loop(event) => {
                if let LoopEvent::MessageAppended { message } = &event
                    && message.role == Role::User
                {
                    self.pending_delivered(Some(session));
                }
                crate::events::apply_loop_event(&mut lock(&buffer), &event);
                if let LoopEvent::ToolCallEnd { id, .. } = &event {
                    self.card_ended(session, id);
                }
                matches!(
                    event,
                    LoopEvent::ToolCallStart { .. }
                        | LoopEvent::ToolExecutionStart { .. }
                        | LoopEvent::ToolCallEnd { .. }
                )
            }
            Event::Host(event) => self.apply_agent_host_event(session, &buffer, event),
        };
        if card {
            self.update_card(session);
        }
    }

    /// Apply a host event of agent `session`, whose buffer is `buffer`.
    /// Its state and usage only reach the agent tree, never the footer.
    /// Returns whether the agent's card may have changed.
    fn apply_agent_host_event(
        &mut self,
        session: SessionId,
        buffer: &SharedBuffer,
        event: HostEvent,
    ) -> bool {
        match event {
            HostEvent::PermissionRequested {
                request_id,
                tool_call_id,
                tool,
                input,
                ..
            } => {
                let agent = self
                    .agents
                    .get(session)
                    .map(|node| (node.agent.clone(), node.description.clone()));
                self.push_approval(PendingApproval {
                    request_id,
                    session,
                    agent,
                    tool_call_id: tool_call_id.map(|id| id.to_string()),
                    tool,
                    input,
                });
                true
            }
            HostEvent::PermissionResolved { request_id } => {
                self.drop_permission(request_id);
                true
            }
            HostEvent::RunStarted => {
                self.set_card_phase(session, ToolPhase::Running);
                true
            }
            HostEvent::RunEnded { .. } => {
                self.end_run(session);
                self.set_card_time(session);
                true
            }
            HostEvent::Notice {
                level,
                text,
                transient: true,
            } => {
                let agent = self
                    .agents
                    .get(session)
                    .map_or("agent", |n| n.agent.as_str());
                let text = format!("{agent}: {text}");
                self.toast(level, text);
                false
            }
            HostEvent::Notice { level, text, .. } => {
                push_notice(buffer, level, text);
                false
            }
            HostEvent::ShellFinished {
                command,
                output,
                exit_code,
            } => {
                push_shell(buffer, &command, &output, exit_code);
                false
            }
            HostEvent::AgentSpawned { .. } => {
                self.set_card_phase(session, ToolPhase::Queued);
                true
            }
            HostEvent::UsageUpdated { .. } => true,
            HostEvent::StateChanged { .. }
            | HostEvent::TitleChanged { .. }
            | HostEvent::SessionChanged { .. }
            | HostEvent::McpServers { .. } => false,
        }
    }

    /// The buffer that shows `session`: an agent's own buffer, else the
    /// main one.
    fn buffer_of(&self, session: SessionId) -> SharedBuffer {
        self.agent_buffers
            .get(&session)
            .map_or_else(|| Arc::clone(&self.root_buffer), Arc::clone)
    }

    /// Point the transcript, the working row, the header and the input
    /// at agent `session`, when it is an agent under the main session.
    /// An agent of a resumed session first loads its stored transcript.
    pub(crate) fn focus_agent(&mut self, session: SessionId) {
        if !self.agent_buffers.contains_key(&session)
            && self.agents.get(session).is_some_and(|n| n.restored)
        {
            let Some(messages) = self.agent_loader.as_ref().and_then(|load| load(session)) else {
                self.notify("the agent's transcript could not be read");
                return;
            };
            let buffer = crate::events::shared_buffer();
            {
                let mut buf = lock(&buffer);
                let durations = crate::events::tool_durations(&messages);
                crate::events::populate_from_history(&mut buf, &messages, &durations);
            }
            self.agent_buffers.insert(session, buffer);
        }
        if self.agent_buffers.contains_key(&session) {
            self.set_focus(Some(session));
        }
    }

    /// Go back one level from the agent on screen: to its parent when
    /// that is an agent, else to the main view.
    pub(crate) fn leave_agent(&mut self) {
        let parent = self
            .focus
            .and_then(|session| self.agents.get(session))
            .map(|node| node.parent)
            .filter(|parent| self.agent_buffers.contains_key(parent));
        self.set_focus(parent);
    }

    /// Show agent `focus`, or the main session for `None` (or an unknown
    /// agent). Each view keeps its own draft, and each buffer its own
    /// scroll, folds and block focus, so coming back finds the view as
    /// it was left. The search, the mouse selection, the context menu
    /// and the completion popup start over.
    pub(crate) fn set_focus(&mut self, focus: Option<SessionId>) {
        let target = focus.and_then(|s| self.agent_buffers.get(&s).map(|b| (s, Arc::clone(b))));
        let focus = target.as_ref().map(|(s, _)| *s);
        if focus == self.focus {
            return;
        }
        self.search_line = None;
        self.search_origin = None;
        self.search_pattern = None;
        self.refresh_search_matches();
        self.clear_selection();
        self.mouse_drag_anchor = None;
        self.context_menu = None;
        self.input_completion = None;
        let draft = self.drafts.remove(&focus).unwrap_or_default();
        let left = self.input.swap_draft(draft);
        self.drafts.insert(self.focus, left);
        self.focus = focus;
        self.buffer = target.map_or_else(|| Arc::clone(&self.root_buffer), |(_, b)| b);
        self.draw_snapshot = None;
        lock(&self.buffer).invalidate_all_heights();
    }

    /// Write agent `session`'s card into the `agent` tool row of its
    /// parent: what the agent does now, then its tool count and tokens.
    fn update_card(&self, session: SessionId) {
        let Some(node) = self.agents.get(session) else {
            return;
        };
        let asking = self
            .pending_permission
            .iter()
            .chain(&self.permission_queue)
            .find(|a| a.session == session);
        let activity = if node.state == AgentState::Queued {
            "queued".to_owned()
        } else if let Some(approval) = asking {
            let label = describe(&approval.tool, &approval.input);
            let subject = if approval.tool == "bash" {
                format!("$ {}", label.target)
            } else {
                format!("{} {}", label.verb, label.target)
            };
            format!("Waiting for approval: {}", subject.trim_end())
        } else {
            agent_activity(&lock(&self.buffer_of(session)))
        };
        let tokens = node.usage.total.input + node.usage.total.output;
        let card = format!("{activity}\n{}", agent_stats(node.tool_calls, tokens));
        lock(&self.buffer_of(node.parent)).set_tool_progress(&node.tool_call_id.0, card);
    }

    /// Move agent `session`'s card to `phase`: queued while the running
    /// limit holds the agent back, running from its run's start, so the
    /// card's timer counts the agent's own run like its other views. A
    /// card whose result is in stays as it is.
    fn set_card_phase(&self, session: SessionId, phase: ToolPhase) {
        if let Some(node) = self.agents.get(session) {
            lock(&self.buffer_of(node.parent)).set_tool_phase(&node.tool_call_id.0, phase);
        }
    }

    /// The result of `parent`'s call `id` arrived. When it started an
    /// agent, its card takes the agent's time.
    fn card_ended(&self, parent: SessionId, id: &ToolCallId) {
        let agent = self
            .agents
            .under(parent)
            .into_iter()
            .rev()
            .find(|(depth, node)| *depth == 1 && node.tool_call_id == *id)
            .map(|(_, node)| node.session);
        if let Some(agent) = agent {
            self.set_card_time(agent);
        }
    }

    /// Show agent `session`'s own run time on its finished card, the
    /// time the overlay, the pinned list and the breadcrumb show. Only
    /// the newest agent of its parent's call id owns the card, since
    /// providers may reuse call ids across turns.
    fn set_card_time(&self, session: SessionId) {
        let Some(node) = self.agents.get(session) else {
            return;
        };
        let owner = self
            .agents
            .under(node.parent)
            .into_iter()
            .rev()
            .find(|(depth, n)| *depth == 1 && n.tool_call_id == node.tool_call_id)
            .map(|(_, n)| n.session);
        if let (Some(elapsed), Some(true)) = (node.elapsed(), owner.map(|o| o == session)) {
            let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
            lock(&self.buffer_of(node.parent)).set_tool_duration(&node.tool_call_id.0, ms);
        }
    }

    fn toast(&self, level: NoticeLevel, text: String) {
        if let Some(toasts) = &self.toasts {
            let kind = match level {
                NoticeLevel::Info => ToastKind::Info,
                NoticeLevel::Warning => ToastKind::Warning,
                NoticeLevel::Error => ToastKind::Error,
            };
            toast::push_toast(
                toasts,
                Toast::with_kind(text, kind, toast::DEFAULT_TOAST_DURATION),
            );
        }
    }

    /// Queue `approval` for the panel and show its call as waiting.
    fn push_approval(&mut self, approval: PendingApproval) {
        if let Some(id) = &approval.tool_call_id {
            lock(&self.buffer_of(approval.session)).set_tool_phase(id, ToolPhase::Waiting);
        }
        self.permission_queue.push_back(approval);
    }

    /// Close out a finished run of `session`: drop its approvals and
    /// stop its streaming and its tool rows. Other sessions' approvals
    /// stay.
    fn end_run(&mut self, session: SessionId) {
        self.permission_queue.retain(|a| a.session != session);
        if self
            .pending_permission
            .as_ref()
            .is_some_and(|a| a.session == session)
        {
            self.advance_approvals();
        }
        let buffer = self.buffer_of(session);
        let mut buf = lock(&buffer);
        buf.finish_streaming();
        buf.interrupt_running_tools();
    }

    /// Show the oldest waiting permission request once the screen is
    /// free. Returns whether a panel opened.
    fn open_next_permission(&mut self) -> bool {
        if self.picker.is_some() || self.plugin_overlay.is_some() || self.approval_panel.is_some() {
            return false;
        }
        self.show_next_approval()
    }

    /// Close the panel on screen and show the next waiting request, if
    /// any.
    fn advance_approvals(&mut self) {
        self.approval_panel = None;
        self.pending_permission = None;
        self.show_next_approval();
    }

    fn show_next_approval(&mut self) -> bool {
        let Some(approval) = self.permission_queue.pop_front() else {
            return false;
        };
        self.approval_panel = Some(crate::overlay::ApprovalPanel::new(
            &approval.tool,
            &approval.input,
            approval
                .agent
                .as_ref()
                .map(|(name, task)| (name.as_str(), task.as_str())),
            Instant::now(),
        ));
        self.pending_permission = Some(approval);
        true
    }

    /// Forget a request that was answered elsewhere or abandoned. Its
    /// tool call goes back to the queue until the loop runs it.
    fn drop_permission(&mut self, request_id: RequestId) {
        let mut dropped = Vec::new();
        self.permission_queue.retain(|a| {
            let keep = a.request_id != request_id;
            if !keep {
                dropped.push((a.session, a.tool_call_id.clone()));
            }
            keep
        });
        if self
            .pending_permission
            .as_ref()
            .is_some_and(|a| a.request_id == request_id)
        {
            dropped.extend(
                self.pending_permission
                    .take()
                    .map(|a| (a.session, a.tool_call_id)),
            );
            self.advance_approvals();
        }
        for (session, id) in dropped {
            if let Some(id) = id {
                lock(&self.buffer_of(session)).set_tool_phase(&id, ToolPhase::Queued);
            }
        }
    }

    /// Send the decision for the panel on screen, then show the next
    /// request. The tool row moves to denied, or to approved until the
    /// loop reports that it runs. Allowing the tool for the session or
    /// always also approves the waiting requests for the same tool,
    /// which the new rule covers.
    pub(crate) fn answer_permission(&mut self, decision: PermissionDecision) {
        let Some(approval) = self.pending_permission.take() else {
            return;
        };
        let mut answered = vec![(approval.clone(), decision)];
        if matches!(
            decision,
            PermissionDecision::AllowSession | PermissionDecision::AllowAlways
        ) {
            let (same, rest) = std::mem::take(&mut self.permission_queue)
                .into_iter()
                .partition(|a| a.tool == approval.tool);
            self.permission_queue = rest;
            answered.extend(
                same.into_iter()
                    .map(|a: PendingApproval| (a, PermissionDecision::AllowOnce)),
            );
        }
        for (approval, decision) in answered {
            let _ = self.send_request(RunRequest::ResolvePermission {
                request_id: approval.request_id,
                decision,
            });
            if let Some(id) = &approval.tool_call_id {
                let phase = if decision == PermissionDecision::Deny {
                    ToolPhase::Denied
                } else {
                    ToolPhase::Approved
                };
                lock(&self.buffer_of(approval.session)).set_tool_phase(id, phase);
            }
        }
        self.advance_approvals();
    }

    /// Deny the request on screen, then send `text` as a prompt to the
    /// session that asked, so it reads the denial and the instruction
    /// together at the next turn boundary. The draft and its
    /// attachments stay.
    pub(crate) fn answer_with_feedback(&mut self, text: String) {
        let agent = self
            .pending_permission
            .as_ref()
            .filter(|a| a.agent.is_some())
            .map(|a| a.session);
        self.answer_permission(PermissionDecision::Deny);
        self.send_prompt(text, Vec::new(), false, agent);
    }
}

/// Show a non-transient notice as a block in `buffer`.
fn push_notice(buffer: &SharedBuffer, level: NoticeLevel, text: String) {
    let kind = match level {
        NoticeLevel::Error => "kage:error",
        NoticeLevel::Info | NoticeLevel::Warning => "kage:notify",
    };
    lock(buffer).push_custom(kind, text, false);
}

/// Show a finished shell escape as a block in `buffer`.
fn push_shell(buffer: &SharedBuffer, command: &str, output: &str, exit_code: Option<i32>) {
    let exit = exit_code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
    lock(buffer).push_custom(
        "kage:shell",
        format!("$ {command}\n{}\n(exit code {exit})", output.trim_end()),
        false,
    );
}

/// What an agent's card says it does: its running tool, else its latest
/// tool, described with the verb of the tool's phase.
pub(super) fn agent_activity(buffer: &crate::Buffer) -> String {
    let mut latest = None;
    for block in buffer.blocks().iter().rev() {
        match block {
            crate::Block::ToolCall {
                name, input, phase, ..
            } => {
                if *phase == ToolPhase::Running {
                    latest = Some((name, input, *phase));
                    break;
                }
                latest.get_or_insert((name, input, *phase));
            }
            crate::Block::ToolResult { .. } => {}
            _ if latest.is_some() => break,
            _ => {}
        }
    }
    let Some((name, input, phase)) = latest else {
        return "Working".to_owned();
    };
    let label = describe(name, input);
    format!("{} {}", label.verb_for(phase, false), label.target)
        .trim_end()
        .to_owned()
}
