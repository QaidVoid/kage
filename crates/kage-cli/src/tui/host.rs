//! Translates the App's requests into engine commands.
//!
//! The engine runs the agent. This thread handles what stays on the TUI
//! side: plugin commands and keybindings (which may open dialogs in the
//! App), provider refresh, plugin reload, and the plugin events and
//! toasts that accompany model, thinking, and permission changes.

#[allow(clippy::wildcard_imports)] // tui split: shares the parent module scope
use super::*;

use kage_core::ThinkingLevel;
use kage_core::permissions::PermissionAction;
use kage_core::protocol::{
    Command, CommandKind, Delivery, Envelope, Event, HostEvent, NoticeLevel, RunOutcome,
    SessionState,
};

use crate::engine::Commander;

/// What the TUI host knows about the active session, kept current by
/// [`mirror`].
pub(crate) struct Mirror {
    state: SessionState,
    path: Option<PathBuf>,
}

impl Mirror {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            state: SessionState::default(),
            path,
        }
    }

    /// The active session's file.
    pub(crate) fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }
}

/// A bus subscriber that keeps `mirror` current, records the last used
/// model after a completed run, and refreshes the plugin view of the
/// session file.
pub(crate) fn mirror(
    mirror: Arc<Mutex<Mirror>>,
    plugins: Option<Arc<PluginRuntime>>,
) -> impl FnMut(&Envelope) + Send {
    move |envelope| match &envelope.event {
        Event::Host(HostEvent::StateChanged { state }) => lock(&mirror).state = state.clone(),
        Event::Host(HostEvent::SessionChanged { path, .. }) => {
            lock(&mirror).path = Some(path.clone());
            refresh_session_entries(plugins.as_ref(), Some(path));
        }
        Event::Host(HostEvent::RunEnded { outcome }) => {
            let mirror = lock(&mirror);
            if *outcome == RunOutcome::Completed {
                let _ = crate::state::record_last_model(&mirror.state.model);
            }
            refresh_session_entries(plugins.as_ref(), mirror.path.as_deref());
        }
        _ => {}
    }
}

/// Everything the request thread needs.
pub(crate) struct Host {
    pub commander: Commander,
    pub registry: Arc<ProviderRegistry>,
    pub plugins: Option<Arc<PluginRuntime>>,
    pub plugins_dir: Option<PathBuf>,
    pub dialog_tx: mpsc::Sender<PluginDialog>,
    pub plugin_refresh_tx: mpsc::Sender<PluginRefresh>,
    pub mirror: Arc<Mutex<Mirror>>,
}

impl Host {
    /// Serve App requests until the App goes away.
    pub(crate) fn spawn(mut self, requests: mpsc::Receiver<RunRequest>) {
        thread::spawn(move || {
            for request in requests {
                self.handle(request);
            }
        });
    }

    fn send(&self, kind: CommandKind) {
        self.commander.send(Command::active(kind));
    }

    fn notify(&self, text: String) {
        self.commander.publish(HostEvent::Notice {
            level: NoticeLevel::Info,
            text,
            transient: true,
        });
    }

    fn error(&self, text: String) {
        self.commander.publish(HostEvent::Notice {
            level: NoticeLevel::Error,
            text,
            transient: false,
        });
    }

    fn plugin_event(&self, name: &str, payload: &serde_json::Value) {
        if let Some(rt) = &self.plugins {
            let _ = rt.dispatch_event(name, payload);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle(&mut self, request: RunRequest) {
        match request {
            RunRequest::Submit { text, images } => {
                if let Err(err) = crate::history::append(&text) {
                    self.error(format!("history: {err}"));
                }
                let mut content = Vec::with_capacity(1 + images.len());
                if !text.is_empty() || images.is_empty() {
                    content.push(Content::Text { text });
                }
                content.extend(images.into_iter().map(|img| Content::Image {
                    source: img.source,
                    mime: img.mime,
                }));
                self.send(CommandKind::Prompt {
                    content,
                    delivery: Delivery::Steer,
                });
            }
            RunRequest::Cancel => self.send(CommandKind::Cancel),
            RunRequest::ResolvePermission {
                request_id,
                decision,
            } => self.send(CommandKind::ResolvePermission {
                request_id,
                decision,
            }),
            RunRequest::SwitchModel(model) => self.switch_model(&model),
            RunRequest::CycleThinkingLevel => {
                let next = lock(&self.mirror).state.thinking.cycle();
                self.set_thinking(next, "cycle");
            }
            RunRequest::SetThinkingLevel(value) => match ThinkingLevel::parse(&value) {
                Some(level) => self.set_thinking(level, "settings"),
                None => self.error(format!("settings: unknown thinking level: {value}")),
            },
            RunRequest::SetPermissionMode(mode) => self.set_permission_mode(mode),
            RunRequest::CompactNow => self.send(CommandKind::Compact),
            RunRequest::RunShell(command) => self.send(CommandKind::Shell { command }),
            RunRequest::NewSession => self.send(CommandKind::NewSession),
            RunRequest::CloneSession => self.send(CommandKind::Clone),
            RunRequest::ExportSession(path) => self.send(CommandKind::Export { path }),
            RunRequest::ForkSessionFile(path) => self.send(CommandKind::ForkFile { path }),
            RunRequest::DeleteSession(path) => self.send(CommandKind::DeleteSession { path }),
            RunRequest::ResumeSession(path) => {
                let target = path.display().to_string();
                if let Some(dest) = self.consult("session_before_switch", &target) {
                    self.send(CommandKind::LoadSession {
                        path: PathBuf::from(dest),
                    });
                }
            }
            RunRequest::ForkSession { at } => {
                if let Some(at) = self.consult("session_before_fork", &at) {
                    self.send(CommandKind::Fork {
                        at: (!at.is_empty()).then_some(at),
                        switch: false,
                    });
                }
            }
            RunRequest::SwitchSession(SwitchTarget::Session(target)) => {
                match resolve_switch_target(&target) {
                    Ok(path) => {
                        let target = path.display().to_string();
                        if let Some(dest) = self.consult("session_before_switch", &target) {
                            self.send(CommandKind::LoadSession {
                                path: PathBuf::from(dest),
                            });
                        }
                    }
                    Err(err) => self.error(format!("switch: {err}")),
                }
            }
            RunRequest::SwitchSession(SwitchTarget::PendingFork(at)) => {
                if let Some(at) = self.consult("session_before_switch", &at) {
                    self.send(CommandKind::Fork {
                        at: (!at.is_empty()).then_some(at),
                        switch: true,
                    });
                }
            }
            RunRequest::InvokePluginCommand { name, args } => {
                let Some(rt) = self.plugins.clone() else {
                    return;
                };
                let command = rt
                    .registered_command_overrides()
                    .into_iter()
                    .chain(rt.registered_commands())
                    .find(|c| c.name() == name);
                match command {
                    Some(cmd) => {
                        let output =
                            run_bridged_command(&rt, &cmd, &args, &self.dialog_tx, &self.commander);
                        self.show_plugin_output(output);
                    }
                    None => self.error(format!("no plugin command: {name}")),
                }
            }
            RunRequest::InvokeKeymap { id } => {
                let Some(rt) = self.plugins.clone() else {
                    return;
                };
                let output = run_bridged_keymap(&rt, id, &self.dialog_tx, &self.commander);
                self.show_plugin_output(output);
            }
            RunRequest::RefreshProviders => self.refresh_providers(),
            RunRequest::ReloadPlugins => self.reload_plugins(),
        }
    }

    fn switch_model(&self, model: &str) {
        if let Err(err) = self.registry.resolve(model) {
            self.error(format!("cannot switch to {model}: {err}"));
            return;
        }
        let prev = lock(&self.mirror).state.model.clone();
        self.send(CommandKind::SetModel {
            model: model.to_owned(),
        });
        self.notify(format!("switched to {model}"));
        self.plugin_event(
            "model_select",
            &serde_json::json!({ "prev": prev, "next": model, "source": "set" }),
        );
        if let Err(err) = crate::state::record_last_model(model) {
            self.error(format!("state: {err}"));
        }
    }

    fn set_thinking(&self, level: ThinkingLevel, source: &str) {
        let prev = lock(&self.mirror).state.thinking;
        self.send(CommandKind::SetThinking { level });
        self.notify(format!("thinking level: {}", level.label()));
        self.plugin_event(
            "thinking_level_select",
            &serde_json::json!({
                "prev": prev.as_str(),
                "next": level.as_str(),
                "source": source,
            }),
        );
    }

    fn set_permission_mode(&self, mode: Option<PermissionAction>) {
        let label = |m: Option<PermissionAction>| match m {
            Some(PermissionAction::Ask) => "ask",
            Some(PermissionAction::Deny) => "deny",
            _ => "default",
        };
        let prev = lock(&self.mirror).state.permission_mode;
        self.send(CommandKind::SetPermissionMode { mode });
        self.notify(match mode {
            Some(_) => format!("permission mode: {}", label(mode)),
            None => "permission mode: default (configured rules)".to_owned(),
        });
        self.plugin_event(
            "permission_mode_select",
            &serde_json::json!({
                "prev": label(prev),
                "next": label(mode),
                "source": "command",
            }),
        );
    }

    fn show_plugin_output(&self, output: Option<CommandOutput>) {
        let Some(output) = output.filter(|o| !o.text.is_empty()) else {
            return;
        };
        self.commander.publish(HostEvent::Notice {
            level: if output.is_error {
                NoticeLevel::Error
            } else {
                NoticeLevel::Info
            },
            text: output.text,
            transient: false,
        });
    }

    /// Let plugins veto or rewrite a session operation's target.
    fn consult(&self, event: &str, target: &str) -> Option<String> {
        let Some(rt) = &self.plugins else {
            return Some(target.to_owned());
        };
        if rt.handler_count(event) == 0 {
            return Some(target.to_owned());
        }
        match rt.dispatch_session_op(event, target) {
            Ok(kage_plugin::SessionOpDecision::Proceed) => Some(target.to_owned()),
            Ok(kage_plugin::SessionOpDecision::Patch(next)) => Some(next),
            Ok(kage_plugin::SessionOpDecision::Cancel { reason }) => {
                self.error(format!("{event}: {reason}"));
                None
            }
            Err(err) => {
                self.error(format!("{event}: plugin dispatch failed: {err}"));
                Some(target.to_owned())
            }
        }
    }

    fn refresh_providers(&mut self) {
        let active_ok = self.rebuild_registry();
        let active = lock(&self.mirror).state.model.clone();
        self.publish_plugin_refresh(&active);
        if active_ok {
            self.notify("providers refreshed".to_owned());
        } else {
            self.notify("providers refreshed; pick a model (/model)".to_owned());
        }
    }

    /// Rebuild the provider registry with plugin providers and hand it to
    /// the engine. Returns whether the active model still resolves.
    fn rebuild_registry(&mut self) -> bool {
        let mut fresh = crate::build_provider_registry();
        if let Some(rt) = &self.plugins {
            crate::plugins::merge_plugin_providers(rt, &mut fresh);
        }
        let active_ok = fresh.resolve(&lock(&self.mirror).state.model).is_ok();
        self.registry = Arc::new(fresh);
        self.commander.set_registry(Arc::clone(&self.registry));
        active_ok
    }

    /// Re-read plugins and `init.lua` from disk and republish everything
    /// they contribute: tools, block renderers, providers, commands,
    /// widgets, and autocomplete. Keymaps live in the table the App
    /// shares with the runtime, so the reload updates them in place.
    fn reload_plugins(&mut self) {
        let Some(rt) = self.plugins.clone() else {
            return;
        };
        let reload = rt.reload_all(self.plugins_dir.as_deref());
        self.commander.reload_plugin_tools();
        super::support::register_block_renderers(&rt);
        self.rebuild_registry();
        let active = lock(&self.mirror).state.model.clone();
        self.publish_plugin_refresh(&active);
        let report = match reload {
            Ok(report) => report,
            Err(err) => {
                self.error(format!("plugin reload: {err}"));
                return;
            }
        };
        let init = match report.init {
            Some(Ok(())) => ", init.lua ok",
            Some(Err(_)) => ", init.lua failed",
            None => "",
        };
        for err in &report.keymap_errors {
            self.error(err.clone());
        }
        if report.failed.is_empty() {
            self.notify(format!(
                "plugins reloaded ({} loaded{init})",
                report.loaded.len()
            ));
        } else {
            self.notify(format!(
                "plugins reloaded ({} ok, {} failed{init})",
                report.loaded.len(),
                report.failed.len()
            ));
            for (path, err) in report.failed {
                self.error(format!("plugin {}: {err}", path.display()));
            }
        }
    }

    /// Hand the App fresh plugin contributions and model choices.
    fn publish_plugin_refresh(&self, active_model: &str) {
        let rt = self.plugins.as_ref();
        let _ = self.plugin_refresh_tx.send(PluginRefresh {
            commands: rt
                .map(|rt| snapshot_plugin_commands(rt))
                .unwrap_or_default(),
            widgets: rt.map(|rt| rt.registered_widgets()).unwrap_or_default(),
            autocomplete: rt
                .map(|rt| rt.registered_autocomplete_providers())
                .unwrap_or_default(),
            models: available_model_items(&self.registry, active_model),
        });
    }
}
