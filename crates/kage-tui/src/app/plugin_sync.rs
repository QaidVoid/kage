//! Plugin output, requests and dialogs the App picks up between
//! frames.

use super::*;

impl App {
    pub(crate) fn refresh_plugin_widget_texts(&mut self, width: u16) {
        self.plugin_widget_texts = self
            .plugin_widgets
            .iter()
            .map(|w| w.render(width))
            .collect();
        self.plugin_status_cache.clear();
        if let Some(status) = self.plugin_status.as_ref() {
            let map = lock(status);
            self.plugin_status_cache
                .extend(map.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        if let Some(usage_slot) = self.plugin_usage.as_ref()
            && let Some(snap) = self.session_usage_snapshot()
        {
            let mut slot = lock(usage_slot);
            *slot = serde_json::json!({
                "model": snap.model,
                "input_tokens": snap.input_tokens,
                "output_tokens": snap.output_tokens,
                "cache_read_tokens": snap.cache_read_tokens,
                "cache_write_tokens": snap.cache_write_tokens,
                "current_context": snap.current_context,
                "context_window": snap.context_window,
                "working": snap.working,
            });
        }
    }

    /// Refresh the plugin text caches only when something observable
    /// can have changed: the coarse tick elapsed, the width moved, a
    /// (re)registration marked them dirty, or the plugin runtime
    /// reported fresh output.
    pub(crate) fn refresh_plugin_widget_texts_if_due(&mut self, width: u16) {
        const PLUGIN_TEXT_INTERVAL: Duration = Duration::from_millis(500);
        let due = self.plugin_texts_dirty
            || self.plugin_texts_width != width
            || self
                .plugin_texts_refreshed_at
                .is_none_or(|t| t.elapsed() >= PLUGIN_TEXT_INTERVAL);
        if !due {
            return;
        }
        self.plugin_texts_dirty = false;
        self.plugin_texts_width = width;
        self.plugin_texts_refreshed_at = Some(Instant::now());
        self.refresh_plugin_widget_texts(width);
    }

    /// Whether plugin output changed since the last check. Marks the
    /// text caches for refresh, and re-measures blocks when block
    /// renderer output changed.
    pub(crate) fn take_plugin_redraw(&mut self) -> bool {
        use std::sync::atomic::Ordering;
        let Some((redraw, blocks)) = &self.plugin_redraw else {
            return false;
        };
        if blocks.swap(false, Ordering::Relaxed) {
            lock(&self.buffer).invalidate_all_heights();
        }
        let fresh = redraw.swap(false, Ordering::Relaxed);
        if fresh {
            self.plugin_texts_dirty = true;
        }
        fresh
    }

    /// Drain any pending `kage.compact()` request and forward it as
    /// [`RunRequest::CompactNow`] to the worker. The optional prompt
    /// is currently advisory; a future compaction hook will receive it.
    pub(crate) fn drain_plugin_compact_request(&mut self) {
        let Some(slot) = self.plugin_compact_request.as_ref() else {
            return;
        };
        let pending = lock(slot).take();
        if pending.is_some() {
            let _ = self.send_request(RunRequest::CompactNow);
        }
    }

    /// Drain any pending `kage.session.fork()` request and forward it
    /// as [`RunRequest::ForkSession`] to the worker. The worker copies
    /// the current session through entry `at` into a fresh session
    /// file.
    pub(crate) fn drain_plugin_fork_request(&mut self) {
        let Some(slot) = self.plugin_fork_request.as_ref() else {
            return;
        };
        let pending = lock(slot).take();
        if let Some(at) = pending {
            let _ = self.send_request(RunRequest::ForkSession { at });
        }
    }

    /// Drain any pending `session_write` reseat and relay it as
    /// [`RunRequest::SwitchSession`] so the worker applies it on the
    /// same path as a user-initiated resume/fork.
    pub(crate) fn drain_plugin_switch_request(&mut self) {
        let Some(slot) = self.plugin_switch_request.as_ref() else {
            return;
        };
        let pending = lock(slot).take();
        if let Some(target) = pending {
            let _ = self.send_request(RunRequest::SwitchSession(target));
        }
    }

    /// Recompile the palette when the highlight table changed since the
    /// last compile. Returns whether it did.
    pub(crate) fn refresh_highlights(&mut self) -> bool {
        let Some(shared) = self.highlights.as_ref() else {
            return false;
        };
        let hl = {
            let hl = lock(shared);
            if hl.generation() == self.highlights_generation {
                return false;
            }
            hl.clone()
        };
        self.highlights_generation = hl.generation();
        crate::theme::set_current(crate::theme::Theme::from_groups(&hl));
        let buffers = std::iter::once(&self.root_buffer).chain(self.agent_buffers.values());
        for buffer in buffers {
            lock(buffer).invalidate_all_heights();
        }
        true
    }

    /// Drain one pending blocking [`PluginDialog`] and open its
    /// overlay. Skipped while another overlay (picker or an earlier
    /// plugin dialog) is up: the worker stays parked and the request
    /// is taken on a later tick once the screen is free (the bridge is
    /// single-slot, so at most one is queued). An empty item list
    /// resolves immediately to "cancelled" rather than opening a dead
    /// picker.
    pub(crate) fn drain_plugin_dialog(&mut self) -> bool {
        if self.picker.is_some() || self.plugin_overlay.is_some() {
            return false;
        }
        let Some(rx) = self.dialog_rx.as_ref() else {
            return false;
        };
        let Ok(dialog) = rx.try_recv() else {
            return false;
        };
        match dialog {
            PluginDialog::Select {
                title,
                items,
                reply,
            } => {
                if items.is_empty() {
                    let _ = reply.send(None);
                    return false;
                }
                let picks = items
                    .iter()
                    .enumerate()
                    .map(|(idx, item)| PickItem {
                        value: idx.to_string(),
                        label: item.label.clone(),
                        badge: None,
                        group: None,
                        right: None,
                    })
                    .collect();
                self.plugin_overlay = Some(Box::new(OverlayPicker::new(title, picks)));
                self.active_dialog = Some(PluginDialogState::Select { reply, items });
            }
            PluginDialog::Confirm {
                title,
                message,
                reply,
            } => {
                self.plugin_overlay = Some(Box::new(crate::overlay::ConfirmOverlay::new(
                    title, message,
                )));
                self.active_dialog = Some(PluginDialogState::Confirm { reply });
            }
            PluginDialog::Input {
                title,
                placeholder,
                reply,
            } => {
                let mut overlay = crate::overlay::InputOverlay::new(title);
                if let Some(hint) = placeholder {
                    overlay = overlay.with_placeholder(hint);
                }
                self.plugin_overlay = Some(Box::new(overlay));
                self.active_dialog = Some(PluginDialogState::Input { reply });
            }
            PluginDialog::Editor {
                title,
                prefill,
                reply,
            } => {
                let mut overlay = crate::overlay::EditorOverlay::new(title);
                if let Some(text) = prefill {
                    overlay = overlay.with_prefill(text);
                }
                self.plugin_overlay = Some(Box::new(overlay));
                self.active_dialog = Some(PluginDialogState::Editor { reply });
            }
        }
        true
    }

    /// Apply the newest pending [`PluginRefresh`] snapshot, if any.
    /// Drained between event polls; if the worker pushed more than one
    /// between ticks only the latest is applied. Returns `true` when a
    /// snapshot was applied so the caller can force a repaint.
    pub(crate) fn drain_plugin_refresh(&mut self) -> bool {
        let Some(rx) = self.plugin_refresh_rx.as_ref() else {
            return false;
        };
        let mut latest = None;
        while let Ok(snapshot) = rx.try_recv() {
            latest = Some(snapshot);
        }
        let Some(snapshot) = latest else {
            return false;
        };
        self.set_plugin_commands(snapshot.commands);
        self.set_plugin_widgets(snapshot.widgets);
        self.set_plugin_autocomplete(snapshot.autocomplete);
        lock(&self.buffer).invalidate_all_heights();
        if !snapshot.models.is_empty() {
            self.model_choices = snapshot.models;
        }
        true
    }

    /// Refresh the session-list snapshot read by `kage.session.list`
    /// when a session changed since the last one. Builds
    /// `[{id, value}]` entries from the registered [`SessionLister`].
    pub(crate) fn refresh_plugin_session_list_if_stale(&mut self) {
        if !self.plugin_sessions_stale {
            return;
        }
        self.plugin_sessions_stale = false;
        let Some(slot) = self.plugin_session_list.as_ref() else {
            return;
        };
        let Some(lister) = self.session_lister.as_ref() else {
            return;
        };
        let items = lister(true);
        let entries: Vec<serde_json::Value> = items
            .into_iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.label,
                    "value": p.value,
                })
            })
            .collect();
        let mut s = lock(slot);
        *s = entries;
    }
}
