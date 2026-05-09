//! Interactive TUI mode for kage.
//!
//! [`run_tui`] glues [`kage_tui::App`] (main thread) to a worker thread
//! that runs the agent loop. Submitted prompts arrive via an `mpsc`
//! channel; for each prompt the worker pushes the user message into
//! [`AgentContext::history`], runs the loop with [`TuiHooks`] mirroring
//! events into the shared buffer, and resets the cancel flag for the
//! next turn.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use kage_core::{CancelFlag, Content, Message, Role};
use kage_loop::{AgentContext, LoopConfig, NoopHooks, run};
use kage_plugin::PluginRuntime;
use kage_provider::ProviderRegistry;
use kage_session::SessionSummary;
use kage_tools::ToolRegistry;
use kage_tui::{
    App, PickItem, RunRequest, SharedBuffer, Tui, TuiHooks, buffer_host_log, populate_from_history,
    shared_buffer,
};

use crate::plugins::{PluginEventHooks, setup_runtime_with_sink};

/// Drop into the interactive TUI. Returns the appropriate process exit
/// code once the user quits.
pub fn run_tui(model: &str, system: &str) -> ExitCode {
    let registry = Arc::new(crate::build_provider_registry());
    if registry.ids().count() == 0 {
        eprintln!(
            "kage: no provider credentials found. Run `kage auth login` to save \
             one, or export an env var (ANTHROPIC_API_KEY, OPENAI_API_KEY, \
             GEMINI_API_KEY, ZAI_API_KEY, ZAI_CODING_API_KEY)."
        );
        return ExitCode::from(1);
    }
    let bare_model = match registry.resolve(model) {
        Ok(r) => r.model.clone(),
        Err(e) => {
            eprintln!("kage: cannot resolve model {model}: {e}");
            return ExitCode::from(1);
        }
    };
    let qualified_model = model.to_owned();

    // The buffer must exist before we build the plugin runtime so we can
    // hand the runtime a sink that routes notify/log into the buffer
    // instead of stderr (which would corrupt the alt screen).
    let buffer = shared_buffer();
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let plugin_runtime = match crate::plugins_dir() {
        Ok(dir) => match setup_runtime_with_sink(
            &dir,
            &workdir,
            model,
            system,
            buffer_host_log(buffer.clone()),
        ) {
            Ok(rt) => rt,
            Err(e) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom("kage:error", e, false);
                }
                None
            }
        },
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", e, false);
            }
            None
        }
    };

    let mut tools = kage_tools::builtin_registry();
    if let Some(rt) = plugin_runtime.as_ref() {
        for tool in rt.registered_tools() {
            tools.register(tool);
        }
    }
    let cancel = CancelFlag::new();
    let cx = Arc::new(Mutex::new(
        AgentContext::new(bare_model, system).with_workdir(&workdir),
    ));
    let (tx, rx) = mpsc::channel::<RunRequest>();

    let active_qualified = Arc::new(Mutex::new(qualified_model.clone()));
    let model_choices = available_model_items(&registry, &qualified_model);
    if let Err(err) = crate::state::record_last_model(&qualified_model) {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom("kage:error", format!("state: {err}"), false);
        }
    }

    let worker = spawn_worker(WorkerConfig {
        registry: Arc::clone(&registry),
        active_qualified: Arc::clone(&active_qualified),
        tools,
        cx: Arc::clone(&cx),
        buffer: buffer.clone(),
        cancel: cancel.clone(),
        plugin_runtime,
        rx,
    });

    let mut tui = match Tui::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("kage: failed to enter raw mode: {e}");
            return ExitCode::from(1);
        }
    };
    let mut app = App::new(buffer, tx);
    app.set_model_choices(model_choices);
    app.set_history(crate::history::load());
    if let Ok(dir) = crate::sessions_dir() {
        app.set_session_lister(Box::new(move || list_session_choices(&dir)));
    }
    let result = app.run(&mut tui);
    drop(tui);
    drop(app);
    let _ = worker.join();

    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: tui error: {e}");
            ExitCode::from(1)
        }
    }
}

struct WorkerConfig {
    registry: Arc<ProviderRegistry>,
    active_qualified: Arc<Mutex<String>>,
    tools: ToolRegistry,
    cx: Arc<Mutex<AgentContext>>,
    buffer: SharedBuffer,
    cancel: CancelFlag,
    plugin_runtime: Option<Arc<PluginRuntime>>,
    rx: mpsc::Receiver<RunRequest>,
}

#[allow(clippy::too_many_lines)]
fn spawn_worker(cfg: WorkerConfig) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let WorkerConfig {
            registry,
            active_qualified,
            tools,
            cx,
            buffer,
            cancel,
            plugin_runtime,
            rx,
        } = cfg;
        let loop_cfg = LoopConfig::default();

        for req in rx {
            match req {
                RunRequest::Submit(text) => {
                    cancel.reset();
                    if let Err(err) = crate::history::append(&text)
                        && let Ok(mut buf) = buffer.lock()
                    {
                        buf.push_custom("kage:error", format!("history: {err}"), false);
                    }
                    // Re-resolve the model on every turn so a switch
                    // request between turns takes effect immediately.
                    let qualified = active_qualified
                        .lock()
                        .expect("active model mutex poisoned")
                        .clone();
                    let resolved = match registry.resolve(&qualified) {
                        Ok(r) => r,
                        Err(e) => {
                            if let Ok(mut buf) = buffer.lock() {
                                buf.push_custom(
                                    "kage:error",
                                    format!("model {qualified} unavailable: {e}"),
                                    false,
                                );
                            }
                            continue;
                        }
                    };
                    let provider = Arc::clone(resolved.provider);
                    let bare_model = resolved.model.clone();
                    let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
                    cx_guard.model = bare_model;
                    let parent = cx_guard.history.last().map(|m| m.id);
                    cx_guard.history.push(Message::new(
                        Role::User,
                        vec![Content::Text { text }],
                        parent,
                    ));
                    let ok = if let Some(rt) = plugin_runtime.as_ref() {
                        let inner = TuiHooks::new(NoopHooks, buffer.clone());
                        let mut hooks = PluginEventHooks::new(inner, Arc::clone(rt));
                        hooks.dispatch_agent_start();
                        let res = run(
                            provider.as_ref(),
                            &tools,
                            &mut cx_guard,
                            &loop_cfg,
                            &mut hooks,
                            &cancel,
                            |_| {},
                        );
                        hooks.dispatch_agent_end(res.is_ok());
                        res.is_ok()
                    } else {
                        let mut hooks = TuiHooks::new(NoopHooks, buffer.clone());
                        run(
                            provider.as_ref(),
                            &tools,
                            &mut cx_guard,
                            &loop_cfg,
                            &mut hooks,
                            &cancel,
                            |_| {},
                        )
                        .is_ok()
                    };
                    if ok && let Err(err) = crate::state::record_last_model(&qualified) {
                        if let Ok(mut buf) = buffer.lock() {
                            buf.push_custom("kage:error", format!("state: {err}"), false);
                        }
                    }
                }
                RunRequest::ResumeSession(path) => {
                    handle_resume(&registry, &active_qualified, &cx, &buffer, &path);
                }
                RunRequest::Cancel => cancel.cancel(),
                RunRequest::SwitchModel(new_model) => {
                    // Validate before switching so a typo doesn't break
                    // the next turn silently.
                    match registry.resolve(&new_model) {
                        Ok(_) => {
                            active_qualified
                                .lock()
                                .expect("active model mutex poisoned")
                                .clone_from(&new_model);
                            if let Ok(mut buf) = buffer.lock() {
                                buf.push_custom(
                                    "kage:notify",
                                    format!("switched to {new_model}"),
                                    false,
                                );
                            }
                            if let Err(err) = crate::state::record_last_model(&new_model)
                                && let Ok(mut buf) = buffer.lock()
                            {
                                buf.push_custom("kage:error", format!("state: {err}"), false);
                            }
                        }
                        Err(e) => {
                            if let Ok(mut buf) = buffer.lock() {
                                buf.push_custom(
                                    "kage:error",
                                    format!("cannot switch to {new_model}: {e}"),
                                    false,
                                );
                            }
                        }
                    }
                }
            }
        }
    })
}

/// Build the picker rows for the `Ctrl+R` session picker. Listing
/// happens at picker-open time so newly recorded sessions appear
/// without needing to restart the TUI.
fn list_session_choices(dir: &std::path::Path) -> Vec<PickItem> {
    let Ok(summaries) = kage_session::list(dir) else {
        return Vec::new();
    };
    summaries
        .into_iter()
        .map(|s| {
            let label = format_session_label(&s);
            PickItem::simple(s.path.to_string_lossy().into_owned()).with_label(label)
        })
        .collect()
}

/// One-line description of a session for the picker: short id, last
/// user prompt (truncated), and an updated-at timestamp.
fn format_session_label(s: &SessionSummary) -> String {
    let id = s.id.to_string();
    let short_id: String = id.chars().take(8).collect();
    let preview = s.last_user_prompt.as_deref().map_or_else(
        || "(no user prompt)".to_owned(),
        |t| {
            let single_line = t.replace('\n', " ");
            truncate(&single_line, 60)
        },
    );
    let updated = s.updated_at.format("%Y-%m-%d %H:%M");
    format!("{short_id}  {preview:<60}  {updated}")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let cut: String = s.chars().take(max.saturating_sub(3)).collect();
    format!("{cut}...")
}

/// Replay `path` into the live TUI: clear the buffer, replace
/// `cx.history` with the recorded one, point `active_qualified` at the
/// session's model (validating it resolves), and repopulate the
/// buffer so the user sees the prior conversation rendered with the
/// current TUI styling.
fn handle_resume(
    registry: &Arc<ProviderRegistry>,
    active_qualified: &Arc<Mutex<String>>,
    cx: &Arc<Mutex<AgentContext>>,
    buffer: &SharedBuffer,
    path: &std::path::Path,
) {
    let replay = match kage_session::replay(path) {
        Ok(r) => r,
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom(
                    "kage:error",
                    format!("resume {}: {e}", path.display()),
                    false,
                );
            }
            return;
        }
    };
    let resolved = match registry.resolve(&replay.model) {
        Ok(r) => r,
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom(
                    "kage:error",
                    format!("resume: model {} unavailable: {e}", replay.model),
                    false,
                );
            }
            return;
        }
    };
    {
        let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
        cx_guard.history.clone_from(&replay.history);
        cx_guard.model.clone_from(&resolved.model);
    }
    active_qualified
        .lock()
        .expect("active model mutex poisoned")
        .clone_from(&replay.model);
    if let Ok(mut buf) = buffer.lock() {
        buf.clear();
        populate_from_history(&mut buf, &replay.history);
        let id = replay.header.session.to_string();
        let short: String = id.chars().take(8).collect();
        buf.push_custom(
            "kage:notify",
            format!("resumed session {short} on {}", replay.model),
            false,
        );
    }
}

/// Build the picker rows the App offers when the user hits `Ctrl+P`.
/// Iterates registered providers and pulls each one's catalog model
/// list; the active model is marked with `*`.
fn available_model_items(registry: &ProviderRegistry, active: &str) -> Vec<kage_tui::PickItem> {
    let mut items: Vec<kage_tui::PickItem> = Vec::new();
    let mut provider_ids: Vec<&str> = registry.ids().collect();
    provider_ids.sort_unstable();
    for provider_id in provider_ids {
        let catalog_provider = kage_provider::catalog::provider(provider_id);
        let display_name = catalog_provider.map_or(provider_id, |p| p.name);
        let models = catalog_provider.map_or::<&[_], _>(&[], |p| p.models);
        for model in models {
            let value = format!("{provider_id}:{}", model.id);
            let label = format!("{display_name:<20}  {}", model.name);
            let badge = if value == active { '*' } else { ' ' };
            items.push(
                kage_tui::PickItem::simple(value)
                    .with_label(label)
                    .with_badge(badge),
            );
        }
    }
    items
}
