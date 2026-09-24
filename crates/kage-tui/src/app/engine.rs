//! Applying engine events to the App: buffer, modeline, notices, and
//! permission prompts.

#[allow(clippy::wildcard_imports)] // impl-split submodule shares the parent module scope
use super::*;

use kage_core::protocol::{Envelope, Event, HostEvent, NoticeLevel, RequestId};

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
                let mut buf = lock(&self.buffer);
                buf.clear();
                crate::events::populate_from_history(&mut buf, &messages, &durations);
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
                tool,
                subject,
                ..
            } => self.permission_queue.push_back((request_id, tool, subject)),
            HostEvent::PermissionResolved { request_id } => self.drop_permission(request_id),
            HostEvent::RunEnded { .. } => {
                self.permission_queue.clear();
                self.permission_overlay = None;
                self.pending_permission = None;
            }
            HostEvent::RunStarted | HostEvent::TitleChanged { .. } => {}
        }
    }

    /// Show the oldest waiting permission request once the screen is
    /// free. Returns whether a prompt opened.
    fn open_next_permission(&mut self) -> bool {
        if self.picker.is_some()
            || self.plugin_overlay.is_some()
            || self.permission_overlay.is_some()
        {
            return false;
        }
        let Some((request_id, tool, subject)) = self.permission_queue.pop_front() else {
            return false;
        };
        self.permission_overlay = Some(crate::overlay::PermissionOverlay::new(tool, subject));
        self.pending_permission = Some(request_id);
        true
    }

    /// Forget a request that was answered elsewhere or abandoned.
    fn drop_permission(&mut self, request_id: RequestId) {
        self.permission_queue.retain(|(id, _, _)| *id != request_id);
        if self.pending_permission == Some(request_id) {
            self.permission_overlay = None;
            self.pending_permission = None;
        }
    }

    /// Send the decision for the prompt on screen, then show the next one.
    pub(crate) fn answer_permission(&mut self, decision: PermissionDecision) {
        if let Some(request_id) = self.pending_permission.take() {
            let _ = self.send_request(RunRequest::ResolvePermission {
                request_id,
                decision,
            });
        }
    }
}
