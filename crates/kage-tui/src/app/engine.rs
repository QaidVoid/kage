//! Applying engine events to the App: buffer, modeline, notices, and
//! permission prompts.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

use kage_core::protocol::{Envelope, Event, HostEvent, NoticeLevel, RequestId};

use crate::view::tool_view::ToolPhase;

/// A permission request waiting for, or shown in, the approval prompt.
#[derive(Clone, Debug)]
pub(crate) struct PendingApproval {
    /// Id to answer with.
    pub(crate) request_id: RequestId,
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
        if let Event::Host(HostEvent::SessionChanged { .. }) = &envelope.event {
            self.active_session = Some(envelope.session);
            self.status_session_id = Some(envelope.session.to_string().chars().take(8).collect());
        }
        if *self.active_session.get_or_insert(envelope.session) != envelope.session {
            return;
        }
        match envelope.event {
            Event::Loop(event) => crate::events::apply_loop_event(&mut lock(&self.buffer), &event),
            Event::Host(event) => self.apply_host_event(event),
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
            } => {
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
            HostEvent::Notice { level, text, .. } => {
                let kind = match level {
                    NoticeLevel::Error => "kage:error",
                    NoticeLevel::Info | NoticeLevel::Warning => "kage:notify",
                };
                lock(&self.buffer).push_custom(kind, text, false);
            }
            HostEvent::SessionChanged { messages, .. } => {
                let durations = crate::events::tool_durations(&messages);
                {
                    let mut buf = lock(&self.buffer);
                    buf.clear();
                    crate::events::populate_from_history(&mut buf, &messages, &durations);
                }
                self.refresh_start_sessions();
            }
            HostEvent::ShellFinished {
                command,
                output,
                exit_code,
            } => {
                let exit = exit_code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
                lock(&self.buffer).push_custom(
                    "kage:shell",
                    format!("$ {command}\n{}\n(exit code {exit})", output.trim_end()),
                    false,
                );
            }
            HostEvent::PermissionRequested {
                request_id,
                tool_call_id,
                tool,
                input,
                ..
            } => {
                let tool_call_id = tool_call_id.map(|id| id.to_string());
                if let Some(id) = &tool_call_id {
                    lock(&self.buffer).set_tool_phase(id, ToolPhase::Waiting);
                }
                self.permission_queue.push_back(PendingApproval {
                    request_id,
                    tool_call_id,
                    tool,
                    input,
                });
            }
            HostEvent::PermissionResolved { request_id } => self.drop_permission(request_id),
            HostEvent::RunEnded { .. } => {
                self.permission_queue.clear();
                self.approval_panel = None;
                self.pending_permission = None;
                let mut buf = lock(&self.buffer);
                buf.finish_streaming();
                buf.interrupt_running_tools();
            }
            HostEvent::RunStarted | HostEvent::TitleChanged { .. } => {}
        }
    }

    /// Show the oldest waiting permission request once the screen is
    /// free. Returns whether a panel opened.
    fn open_next_permission(&mut self) -> bool {
        if self.picker.is_some() || self.plugin_overlay.is_some() || self.approval_panel.is_some() {
            return false;
        }
        self.show_next_approval(1)
    }

    /// Close the panel on screen and show the next waiting request, if
    /// any, one place further in the count.
    fn advance_approvals(&mut self) {
        let position = self.approval_panel.take().map_or(1, |p| p.position() + 1);
        self.pending_permission = None;
        self.show_next_approval(position);
    }

    fn show_next_approval(&mut self, position: usize) -> bool {
        let Some(approval) = self.permission_queue.pop_front() else {
            return false;
        };
        self.approval_panel = Some(crate::overlay::ApprovalPanel::new(
            &approval.tool,
            &approval.input,
            position,
            Instant::now(),
        ));
        self.pending_permission = Some(approval);
        true
    }

    /// Forget a request that was answered elsewhere or abandoned. Its
    /// tool call goes on running.
    fn drop_permission(&mut self, request_id: RequestId) {
        let mut dropped = Vec::new();
        self.permission_queue.retain(|a| {
            let keep = a.request_id != request_id;
            if !keep {
                dropped.push(a.tool_call_id.clone());
            }
            keep
        });
        if self
            .pending_permission
            .as_ref()
            .is_some_and(|a| a.request_id == request_id)
        {
            dropped.extend(self.pending_permission.take().map(|a| a.tool_call_id));
            self.advance_approvals();
        }
        let mut buf = lock(&self.buffer);
        for id in dropped.into_iter().flatten() {
            buf.set_tool_phase(&id, ToolPhase::Running);
        }
    }

    /// Send the decision for the panel on screen, then show the next
    /// request. The tool row moves to running, restarting its timer, or
    /// to denied.
    pub(crate) fn answer_permission(&mut self, decision: PermissionDecision) {
        let Some(approval) = self.pending_permission.take() else {
            return;
        };
        let _ = self.send_request(RunRequest::ResolvePermission {
            request_id: approval.request_id,
            decision,
        });
        if let Some(id) = &approval.tool_call_id {
            let phase = if decision == PermissionDecision::Deny {
                ToolPhase::Denied
            } else {
                ToolPhase::Running
            };
            lock(&self.buffer).set_tool_phase(id, phase);
        }
        self.advance_approvals();
    }

    /// Deny the request on screen, then send `text` to the model as a
    /// prompt, so it reads the denial and the instruction together at
    /// the next turn boundary. The draft and its attachments stay.
    pub(crate) fn answer_with_feedback(&mut self, text: String) {
        self.answer_permission(PermissionDecision::Deny);
        let submit = RunRequest::Submit {
            text,
            images: Vec::new(),
        };
        if self.send_request(submit).is_err() {
            self.push_error("submit failed: agent worker has stopped");
        }
    }
}
