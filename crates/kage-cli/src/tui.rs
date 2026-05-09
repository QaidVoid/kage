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
use kage_session::{SessionSummary, SessionWriter};
use kage_tools::ToolRegistry;
use kage_tui::{
    App, PickItem, RunRequest, SharedBuffer, SharedSessionUsage, Tui, TuiHooks, buffer_host_log,
    populate_from_history, shared_buffer, shared_session_usage,
};

use crate::plugins::{PluginEventHooks, setup_runtime_with_sink};
use crate::session::SessionRecordingHooks;

/// Drop into the interactive TUI. Returns the appropriate process exit
/// code once the user quits.
#[allow(clippy::too_many_lines)]
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
    let system_prompt = crate::runtime_env::build_system_prompt(system, &workdir, model);
    let system = system_prompt.as_str();
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
    let mut plugin_command_listing: Vec<(String, String)> = Vec::new();
    if let Some(rt) = plugin_runtime.as_ref() {
        for tool in rt.registered_tools() {
            tools.register(tool);
        }
        for cmd in rt.registered_commands() {
            plugin_command_listing.push((cmd.name().to_owned(), cmd.description().to_owned()));
        }
    }
    let cancel = CancelFlag::new();
    let mut initial_cx = AgentContext::new(bare_model, system).with_workdir(&workdir);
    if let Some(window) = crate::runtime_env::context_window_for(model) {
        initial_cx = initial_cx.with_context_window(window);
    }
    let cx = Arc::new(Mutex::new(initial_cx));
    let (tx, rx) = mpsc::channel::<RunRequest>();

    // Plan a session up-front but defer creating the file until the
    // first prompt actually lands. Otherwise quitting or resuming
    // immediately would leave an empty header-only stub on disk.
    let (session_path, session_header) = match crate::plan_session(model, system) {
        Ok((path, header)) => (
            Some(Arc::new(Mutex::new(path))),
            Some(Arc::new(Mutex::new(Some(header)))),
        ),
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", format!("session: {e}"), false);
            }
            (None, None)
        }
    };

    let active_qualified = Arc::new(Mutex::new(qualified_model.clone()));
    let model_choices = available_model_items(&registry, &qualified_model);
    if let Err(err) = crate::state::record_last_model(&qualified_model) {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom("kage:error", format!("state: {err}"), false);
        }
    }

    let session_usage = shared_session_usage();
    // Seed initial usage snapshot so the modeline shows the model
    // and the catalog-reported context window before any turn runs.
    if let Ok(mut snap) = session_usage.lock()
        && let Ok(cx_guard) = cx.lock()
    {
        snap.model.clone_from(&qualified_model);
        snap.context_window = cx_guard.context_window;
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
        session_path: session_path.clone(),
        session_header: session_header.clone(),
        session_usage: session_usage.clone(),
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
    app.set_status_model(Arc::clone(&active_qualified));
    app.set_plugin_commands(plugin_command_listing);
    app.set_session_usage(session_usage);
    if let Some(p) = session_path.as_ref() {
        let path = p.lock().expect("session path mutex poisoned").clone();
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            app.set_status_session_id(stem.chars().take(8).collect());
        }
    }
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
    /// Path to the session file the worker appends to. Shared with
    /// the resume handler so `Ctrl+R` can swap the file in place.
    session_path: Option<Arc<Mutex<PathBuf>>>,
    /// Header to write the first time the planned session file is
    /// created. After creation, this is taken (`Some(_) -> None`) and
    /// subsequent turns reopen the existing file in append mode.
    session_header: Option<Arc<Mutex<Option<kage_session::Header>>>>,
    /// Shared session-usage snapshot the modeline reads from. The
    /// worker updates it after every turn from `cx.budget` so the
    /// modeline reflects live token totals without polling.
    session_usage: SharedSessionUsage,
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
            session_path,
            session_header,
            session_usage,
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
                    let user_msg = Message::new(Role::User, vec![Content::Text { text }], parent);
                    cx_guard.history.push(user_msg.clone());
                    let writer_for_turn = open_writer_for_turn(
                        session_path.as_ref(),
                        session_header.as_ref(),
                        &buffer,
                    );
                    let ok = run_with_hooks(
                        provider.as_ref(),
                        &tools,
                        &mut cx_guard,
                        &loop_cfg,
                        &cancel,
                        &buffer,
                        plugin_runtime.as_ref(),
                        writer_for_turn,
                        &user_msg,
                    );
                    // Update the modeline snapshot from the loop's
                    // post-turn budget. Done while we still hold the
                    // context lock so partial reads are impossible.
                    if let Ok(mut snap) = session_usage.lock() {
                        snap.model.clone_from(&qualified);
                        snap.context_window = cx_guard.context_window;
                        snap.input_tokens = cx_guard.budget.used_input;
                        snap.output_tokens = cx_guard.budget.used_output;
                        snap.cache_read_tokens = cx_guard.budget.used_cache_read;
                        snap.cache_write_tokens = cx_guard.budget.used_cache_write;
                    }
                    if ok && let Err(err) = crate::state::record_last_model(&qualified) {
                        if let Ok(mut buf) = buffer.lock() {
                            buf.push_custom("kage:error", format!("state: {err}"), false);
                        }
                    }
                }
                RunRequest::ResumeSession(path) => {
                    handle_resume(
                        &registry,
                        &active_qualified,
                        &cx,
                        &buffer,
                        session_path.as_ref(),
                        &path,
                    );
                }
                RunRequest::InvokePluginCommand { name, args } => {
                    if let Some(rt) = plugin_runtime.as_ref() {
                        let cmd = rt
                            .registered_commands()
                            .into_iter()
                            .find(|c| c.name() == name);
                        if let Some(cmd) = cmd {
                            match cmd.invoke(&args, &serde_json::Value::Null) {
                                Ok(out) if !out.text.is_empty() => {
                                    if let Ok(mut buf) = buffer.lock() {
                                        buf.push_custom(
                                            if out.is_error {
                                                "kage:error"
                                            } else {
                                                "kage:plugin"
                                            },
                                            out.text,
                                            false,
                                        );
                                    }
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    if let Ok(mut buf) = buffer.lock() {
                                        buf.push_custom(
                                            "kage:error",
                                            format!("plugin command {name}: {e}"),
                                            false,
                                        );
                                    }
                                }
                            }
                        } else if let Ok(mut buf) = buffer.lock() {
                            buf.push_custom(
                                "kage:error",
                                format!("no plugin command: {name}"),
                                false,
                            );
                        }
                    }
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

/// Open or create the session file for the duration of one turn.
///
/// The TUI plans a session id+path at startup but defers writing the
/// header file until the first prompt actually lands. If the path
/// already exists (resumed session, or this is a follow-up turn) we
/// open it in append mode. If not, we consume the planned header,
/// create the file with it, and let subsequent turns hit the open
/// branch.
fn open_writer_for_turn(
    session_path: Option<&Arc<Mutex<PathBuf>>>,
    session_header: Option<&Arc<Mutex<Option<kage_session::Header>>>>,
    buffer: &SharedBuffer,
) -> Option<SessionWriter> {
    let path_arc = session_path?;
    let path = path_arc
        .lock()
        .expect("session path mutex poisoned")
        .clone();
    if !path.exists() {
        let header =
            session_header.and_then(|h| h.lock().expect("session header mutex poisoned").take())?;
        return match SessionWriter::create(path.clone(), header) {
            Ok(w) => Some(w),
            Err(e) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom(
                        "kage:error",
                        format!("session: create {}: {e}", path.display()),
                        false,
                    );
                }
                None
            }
        };
    }
    match SessionWriter::open(&path) {
        Ok(w) => Some(w),
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom(
                    "kage:error",
                    format!("session: open {}: {e}", path.display()),
                    false,
                );
            }
            None
        }
    }
}

/// Run the agent loop with the right hook chain: TUI display innermost,
/// optional session recording in the middle, optional plugin dispatch
/// outermost. Returns whether the loop completed successfully so the
/// caller knows whether to bump `last_model` state.
#[allow(clippy::too_many_arguments)]
fn run_with_hooks(
    provider: &dyn kage_provider::Provider,
    tools: &ToolRegistry,
    cx: &mut AgentContext,
    loop_cfg: &LoopConfig,
    cancel: &CancelFlag,
    buffer: &SharedBuffer,
    plugin_runtime: Option<&Arc<PluginRuntime>>,
    writer: Option<SessionWriter>,
    user_msg: &Message,
) -> bool {
    let tui_hooks = TuiHooks::new(NoopHooks, buffer.clone());
    match (writer, plugin_runtime) {
        (Some(w), Some(rt)) => {
            let mut recorded = SessionRecordingHooks::new(tui_hooks, w);
            recorded.record_user_message(user_msg);
            let mut hooks = PluginEventHooks::new(recorded, Arc::clone(rt));
            hooks.dispatch_agent_start();
            let res = run(provider, tools, cx, loop_cfg, &mut hooks, cancel, |_| {});
            hooks.dispatch_agent_end(res.is_ok());
            res.is_ok()
        }
        (Some(w), None) => {
            let mut hooks = SessionRecordingHooks::new(tui_hooks, w);
            hooks.record_user_message(user_msg);
            run(provider, tools, cx, loop_cfg, &mut hooks, cancel, |_| {}).is_ok()
        }
        (None, Some(rt)) => {
            let mut hooks = PluginEventHooks::new(tui_hooks, Arc::clone(rt));
            hooks.dispatch_agent_start();
            let res = run(provider, tools, cx, loop_cfg, &mut hooks, cancel, |_| {});
            hooks.dispatch_agent_end(res.is_ok());
            res.is_ok()
        }
        (None, None) => {
            let mut hooks = tui_hooks;
            run(provider, tools, cx, loop_cfg, &mut hooks, cancel, |_| {}).is_ok()
        }
    }
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
/// `cx.history` with the recorded one, point the worker at this file
/// for future appends, and repopulate the buffer so the user sees the
/// prior conversation rendered with the current TUI styling.
///
/// If the session's recorded model is no longer resolvable (provider
/// not authed in this run, model removed from the catalog), the
/// resume keeps the currently active model rather than failing. The
/// replay history still loads so the substitute model continues with
/// full context; a `kage:notify` flags the substitution.
fn handle_resume(
    registry: &Arc<ProviderRegistry>,
    active_qualified: &Arc<Mutex<String>>,
    cx: &Arc<Mutex<AgentContext>>,
    buffer: &SharedBuffer,
    session_path: Option<&Arc<Mutex<PathBuf>>>,
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
    let active_now = active_qualified
        .lock()
        .expect("active model mutex poisoned")
        .clone();
    let (qualified_model, bare_model, fallback_note) = match registry.resolve(&replay.model) {
        Ok(r) => (replay.model.clone(), r.model.clone(), None),
        Err(_) => match registry.resolve(&active_now) {
            Ok(r) => (
                active_now.clone(),
                r.model.clone(),
                Some(format!(
                    "session model {} unavailable; using {} instead",
                    replay.model, active_now
                )),
            ),
            Err(e) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom(
                        "kage:error",
                        format!("resume: no resolvable model ({e})"),
                        false,
                    );
                }
                return;
            }
        },
    };
    {
        let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
        cx_guard.history.clone_from(&replay.history);
        cx_guard.model = bare_model;
    }
    active_qualified
        .lock()
        .expect("active model mutex poisoned")
        .clone_from(&qualified_model);
    if let Some(sp) = session_path {
        sp.lock()
            .expect("session path mutex poisoned")
            .clone_from(&path.to_path_buf());
    }
    if let Ok(mut buf) = buffer.lock() {
        buf.clear();
        populate_from_history(&mut buf, &replay.history, &replay.tool_durations);
        let id = replay.header.session.to_string();
        let short: String = id.chars().take(8).collect();
        buf.push_custom(
            "kage:notify",
            format!("resumed session {short} on {qualified_model}"),
            false,
        );
        if let Some(note) = fallback_note {
            buf.push_custom("kage:notify", note, false);
        }
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
