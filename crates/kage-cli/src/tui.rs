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
use kage_loop::{AgentContext, LoopConfig, NoopHooks, force_compact, run};
use kage_plugin::PluginRuntime;
use kage_provider::ProviderRegistry;
use kage_session::{SessionId, SessionReader, SessionSummary, SessionWriter};
use kage_tools::ToolRegistry;
use kage_tui::{
    App, PickItem, RunRequest, SharedBuffer, SharedSessionUsage, SharedToasts, Toast, Tui,
    TuiHooks, buffer_host_log, populate_from_history, push_toast, shared_buffer,
    shared_session_usage, shared_toasts,
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
    let toasts = shared_toasts();
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Build the plugin runtime against a bare prompt first; skills land
    // below once plugins have had a chance to contribute extra dirs via
    // `resources_discover`.
    let bare_prompt = crate::runtime_env::build_system_prompt(system, &workdir, model, &[]);
    let plugin_runtime = match crate::plugins_dir() {
        Ok(dir) => match setup_runtime_with_sink(
            &dir,
            &workdir,
            model,
            &bare_prompt,
            buffer_host_log(buffer.clone(), toasts.clone()),
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
    let skills = crate::load_skills(&workdir, plugin_runtime.as_deref());
    let system_prompt = crate::runtime_env::build_system_prompt(system, &workdir, model, &skills);
    let system = system_prompt.as_str();

    let mut tools = kage_tools::builtin_registry();
    let mut plugin_command_listing: Vec<kage_tui::command::PluginCommand> = Vec::new();
    let mut plugin_widgets: Vec<Arc<kage_plugin::LuaWidget>> = Vec::new();
    let mut plugin_status: Option<kage_plugin::SharedStatus> = None;
    let mut plugin_usage: Option<kage_plugin::SharedUsage> = None;
    let mut plugin_compact_request: Option<kage_plugin::SharedCompactRequest> = None;
    let mut plugin_session_list: Option<kage_plugin::SharedSessionList> = None;
    let mut plugin_fork_request: Option<kage_plugin::SharedForkRequest> = None;
    if let Some(rt) = plugin_runtime.as_ref() {
        for tool in rt.registered_tools() {
            tools.register(tool);
        }
        for tool in rt.registered_tool_overrides() {
            if tools.get(tool.name()).is_none()
                && let Ok(mut buf) = buffer.lock()
            {
                buf.push_custom(
                    "kage:error",
                    format!(
                        "override_tool: no tool named `{}` to override; treating as new registration",
                        tool.name()
                    ),
                    false,
                );
            }
            tools.register(tool);
        }
        for cmd in rt.registered_commands() {
            plugin_command_listing.push(kage_tui::command::PluginCommand {
                name: cmd.name().to_owned(),
                description: cmd.description().to_owned(),
                args: cmd.args().iter().map(translate_plugin_arg).collect(),
            });
        }
        plugin_widgets = rt.registered_widgets();
        plugin_status = Some(rt.shared_status());
        plugin_usage = Some(rt.shared_usage());
        plugin_compact_request = Some(rt.shared_compact_request());
        plugin_session_list = Some(rt.shared_session_list());
        plugin_fork_request = Some(rt.shared_fork_request());
    }
    let cancel = CancelFlag::new();
    let mut initial_cx = AgentContext::new(bare_model, system).with_workdir(&workdir);
    if let Some(window) = crate::runtime_env::context_window_for(model) {
        initial_cx = initial_cx.with_context_window(window);
    }
    if let Some(out) = crate::runtime_env::max_output_tokens_for(model) {
        initial_cx = initial_cx.with_max_output_tokens(out);
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
        snap.thinking_level = cx_guard.thinking_level;
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
        toasts: toasts.clone(),
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
    app.set_plugin_widgets(plugin_widgets);
    if let Some(status) = plugin_status {
        app.set_plugin_status(status);
    }
    if let Some(usage) = plugin_usage {
        app.set_plugin_usage(usage);
    }
    if let Some(req) = plugin_compact_request {
        app.set_plugin_compact_request(req);
    }
    if let Some(list) = plugin_session_list {
        app.set_plugin_session_list(list);
    }
    if let Some(req) = plugin_fork_request {
        app.set_plugin_fork_request(req);
    }
    app.set_cancel_flag(cancel.clone());
    app.set_toasts(toasts.clone());
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
    /// Shared toast queue. The worker pushes into it for model
    /// switches, session resume confirmations, and other async
    /// notifications that should appear as overlays rather than
    /// inline conversation noise.
    toasts: SharedToasts,
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
            toasts,
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
                    if let Some(window) = crate::runtime_env::context_window_for(&qualified) {
                        cx_guard.context_window = window;
                    }
                    cx_guard.max_output_tokens =
                        crate::runtime_env::max_output_tokens_for(&qualified);
                    let parent = cx_guard.history.last().map(|m| m.id);
                    let user_msg = Message::new(Role::User, vec![Content::Text { text }], parent);
                    cx_guard.history.push(user_msg.clone());
                    let writer_for_turn = open_writer_for_turn(
                        session_path.as_ref(),
                        session_header.as_ref(),
                        &buffer,
                    );
                    let context_window = cx_guard.context_window;
                    if let Ok(mut snap) = session_usage.lock() {
                        snap.working = true;
                    }
                    let ok = run_with_hooks(
                        provider.as_ref(),
                        &tools,
                        &mut cx_guard,
                        loop_cfg,
                        &cancel,
                        &buffer,
                        plugin_runtime.as_ref(),
                        writer_for_turn,
                        &user_msg,
                        session_usage.clone(),
                        qualified.clone(),
                        context_window,
                    );
                    if let Ok(mut snap) = session_usage.lock() {
                        snap.working = false;
                    }
                    if ok && let Err(err) = crate::state::record_last_model(&qualified) {
                        if let Ok(mut buf) = buffer.lock() {
                            buf.push_custom("kage:error", format!("state: {err}"), false);
                        }
                    }
                }
                RunRequest::ResumeSession(path) => {
                    let path = match consult_session_op(
                        plugin_runtime.as_ref(),
                        "session_before_switch",
                        &path.display().to_string(),
                        &buffer,
                        &toasts,
                    ) {
                        Some(target) => PathBuf::from(target),
                        None => continue,
                    };
                    handle_resume(
                        &registry,
                        &active_qualified,
                        &cx,
                        &buffer,
                        session_path.as_ref(),
                        &session_usage,
                        &toasts,
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
                RunRequest::ForkSession { at } => {
                    let Some(at) = consult_session_op(
                        plugin_runtime.as_ref(),
                        "session_before_fork",
                        &at,
                        &buffer,
                        &toasts,
                    ) else {
                        continue;
                    };
                    handle_plugin_fork(session_path.as_ref(), &buffer, &toasts, &at);
                }
                RunRequest::CompactNow => {
                    cancel.reset();
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
                                    format!("compact: model {qualified} unavailable: {e}"),
                                    false,
                                );
                            }
                            continue;
                        }
                    };
                    let provider = Arc::clone(resolved.provider);
                    let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
                    let writer_for_turn = open_writer_for_turn(
                        session_path.as_ref(),
                        session_header.as_ref(),
                        &buffer,
                    );
                    if let Ok(mut snap) = session_usage.lock() {
                        snap.working = true;
                    }
                    let ran = run_compact_with_hooks(
                        provider.as_ref(),
                        &mut cx_guard,
                        &cancel,
                        &buffer,
                        plugin_runtime.as_ref(),
                        writer_for_turn,
                    );
                    if let Ok(mut snap) = session_usage.lock() {
                        snap.working = false;
                    }
                    match ran {
                        Ok(true) => {}
                        Ok(false) => {
                            push_toast(
                                &toasts,
                                Toast::info("compact: not enough history yet".to_owned()),
                            );
                        }
                        Err(e) => {
                            if let Ok(mut buf) = buffer.lock() {
                                buf.push_custom(
                                    "kage:error",
                                    format!("compact failed: {e}"),
                                    false,
                                );
                            }
                        }
                    }
                }
                RunRequest::CycleThinkingLevel => {
                    let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
                    let prev = cx_guard.thinking_level.unwrap_or_default();
                    let next = prev.cycle();
                    cx_guard.thinking_level = Some(next);
                    drop(cx_guard);
                    if let Ok(mut snap) = session_usage.lock() {
                        snap.thinking_level = Some(next);
                    }
                    push_toast(
                        &toasts,
                        Toast::info(format!("thinking level: {}", next.label())),
                    );
                    if let Some(rt) = plugin_runtime.as_ref() {
                        let _ = rt.dispatch_event(
                            "thinking_level_select",
                            &serde_json::json!({
                                "prev": prev.as_str(),
                                "next": next.as_str(),
                                "source": "cycle",
                            }),
                        );
                    }
                    if let Some(mut writer) = open_writer_for_turn(
                        session_path.as_ref(),
                        session_header.as_ref(),
                        &buffer,
                    ) && let Err(err) =
                        writer.append(&kage_session::SessionEntry::ThinkingLevelChange(
                            kage_session::ThinkingLevelChange {
                                id: kage_session::EntryId::new(),
                                ts: chrono::Utc::now(),
                                level: next.as_str().to_owned(),
                            },
                        ))
                        && let Ok(mut buf) = buffer.lock()
                    {
                        buf.push_custom(
                            "kage:error",
                            format!("session: append thinking_level: {err}"),
                            false,
                        );
                    }
                }
                RunRequest::SwitchModel(new_model) => {
                    // Validate before switching so a typo doesn't break
                    // the next turn silently.
                    match registry.resolve(&new_model) {
                        Ok(_) => {
                            let prev = active_qualified
                                .lock()
                                .expect("active model mutex poisoned")
                                .clone();
                            active_qualified
                                .lock()
                                .expect("active model mutex poisoned")
                                .clone_from(&new_model);
                            push_toast(&toasts, Toast::info(format!("switched to {new_model}")));
                            if let Some(rt) = plugin_runtime.as_ref() {
                                let _ = rt.dispatch_event(
                                    "model_select",
                                    &serde_json::json!({
                                        "prev": prev,
                                        "next": new_model,
                                        "source": "set",
                                    }),
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

/// Consult plugin handlers for a session-op event before running an
/// action. Returns the (possibly patched) target string if the action
/// should proceed, or `None` if a plugin vetoed.
///
/// On veto, this also pushes a toast plus an inline error block so the
/// user sees the reason without diving into logs. With no plugin runtime
/// or no subscribers, returns `Some(target.to_owned())` immediately.
fn consult_session_op(
    runtime: Option<&Arc<PluginRuntime>>,
    event: &str,
    target: &str,
    buffer: &SharedBuffer,
    toasts: &SharedToasts,
) -> Option<String> {
    let Some(rt) = runtime else {
        return Some(target.to_owned());
    };
    if rt.handler_count(event) == 0 {
        return Some(target.to_owned());
    }
    match rt.dispatch_session_op(event, target) {
        Ok(kage_plugin::SessionOpDecision::Proceed) => Some(target.to_owned()),
        Ok(kage_plugin::SessionOpDecision::Patch(next)) => Some(next),
        Ok(kage_plugin::SessionOpDecision::Cancel { reason }) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", format!("{event}: {reason}"), false);
            }
            push_toast(
                toasts,
                Toast::info(format!("session op cancelled: {reason}")),
            );
            None
        }
        Err(err) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom(
                    "kage:error",
                    format!("{event}: plugin dispatch failed: {err}"),
                    false,
                );
            }
            Some(target.to_owned())
        }
    }
}

/// Extract the first text block from a user message, joined with newlines
/// if there are multiple. Returns an empty string when the message carries
/// no text (image-only, tool-result-only, etc.).
fn first_user_text(msg: &Message) -> String {
    let mut out = String::new();
    for block in &msg.content {
        if let Content::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

/// Run the agent loop with the right hook chain: TUI display innermost,
/// optional session recording in the middle, optional plugin dispatch
/// outermost, plus the [`UsageHooks`] wrapper at the very edge so the
/// modeline updates every `MessageEnd`. Returns whether the loop
/// completed successfully so the caller knows whether to bump
/// `last_model` state.
#[allow(clippy::too_many_arguments)]
fn run_with_hooks(
    provider: &dyn kage_provider::Provider,
    tools: &ToolRegistry,
    cx: &mut AgentContext,
    loop_cfg: LoopConfig,
    cancel: &CancelFlag,
    buffer: &SharedBuffer,
    plugin_runtime: Option<&Arc<PluginRuntime>>,
    writer: Option<SessionWriter>,
    user_msg: &Message,
    session_usage: SharedSessionUsage,
    qualified_model: String,
    context_window: u64,
) -> bool {
    use crate::usage_hooks::UsageHooks;
    let tui_hooks = TuiHooks::new(NoopHooks, buffer.clone());
    match (writer, plugin_runtime) {
        (Some(w), Some(rt)) => {
            let mut recorded = SessionRecordingHooks::new(tui_hooks, w);
            recorded.record_user_message(user_msg);
            let plugin_hooks = PluginEventHooks::new(recorded, Arc::clone(rt));
            plugin_hooks.dispatch_before_agent_start(&cx.system_prompt, &first_user_text(user_msg));
            plugin_hooks.dispatch_agent_start();
            let mut wrapped =
                UsageHooks::new(plugin_hooks, session_usage, qualified_model, context_window);
            let res = run(provider, tools, cx, loop_cfg, &mut wrapped, cancel, |_| {});
            wrapped.into_inner().dispatch_agent_end(res.is_ok());
            res.is_ok()
        }
        (Some(w), None) => {
            let mut recorded = SessionRecordingHooks::new(tui_hooks, w);
            recorded.record_user_message(user_msg);
            let mut wrapped =
                UsageHooks::new(recorded, session_usage, qualified_model, context_window);
            run(provider, tools, cx, loop_cfg, &mut wrapped, cancel, |_| {}).is_ok()
        }
        (None, Some(rt)) => {
            let plugin_hooks = PluginEventHooks::new(tui_hooks, Arc::clone(rt));
            plugin_hooks.dispatch_before_agent_start(&cx.system_prompt, &first_user_text(user_msg));
            plugin_hooks.dispatch_agent_start();
            let mut wrapped =
                UsageHooks::new(plugin_hooks, session_usage, qualified_model, context_window);
            let res = run(provider, tools, cx, loop_cfg, &mut wrapped, cancel, |_| {});
            wrapped.into_inner().dispatch_agent_end(res.is_ok());
            res.is_ok()
        }
        (None, None) => {
            let mut wrapped =
                UsageHooks::new(tui_hooks, session_usage, qualified_model, context_window);
            run(provider, tools, cx, loop_cfg, &mut wrapped, cancel, |_| {}).is_ok()
        }
    }
}

/// Run an unconditional compaction pass through the same hook stack
/// `run_with_hooks` uses, so the resulting `LoopEvent::Compaction`
/// is mirrored to the buffer and recorded to the session file. The
/// usage hook is intentionally skipped: compaction does not change
/// the active model or window, just the saved budget.
fn run_compact_with_hooks(
    provider: &dyn kage_provider::Provider,
    cx: &mut AgentContext,
    cancel: &CancelFlag,
    buffer: &SharedBuffer,
    plugin_runtime: Option<&Arc<PluginRuntime>>,
    writer: Option<SessionWriter>,
) -> Result<bool, kage_core::LoopError> {
    let tui_hooks = TuiHooks::new(NoopHooks, buffer.clone());
    match (writer, plugin_runtime) {
        (Some(w), Some(rt)) => {
            let recorded = SessionRecordingHooks::new(tui_hooks, w);
            let mut plugin_hooks = PluginEventHooks::new(recorded, Arc::clone(rt));
            force_compact(cx, provider, cancel, &mut plugin_hooks, &mut |_| {})
        }
        (Some(w), None) => {
            let mut recorded = SessionRecordingHooks::new(tui_hooks, w);
            force_compact(cx, provider, cancel, &mut recorded, &mut |_| {})
        }
        (None, Some(rt)) => {
            let mut plugin_hooks = PluginEventHooks::new(tui_hooks, Arc::clone(rt));
            force_compact(cx, provider, cancel, &mut plugin_hooks, &mut |_| {})
        }
        (None, None) => {
            let mut hooks = tui_hooks;
            force_compact(cx, provider, cancel, &mut hooks, &mut |_| {})
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

/// Bridge a plugin runtime arg-spec entry over to the TUI's owned
/// arg-spec enum so [`kage_tui::App::set_plugin_commands`] can leak
/// it into a `&'static ArgSpec` for the completion engine.
fn translate_plugin_arg(arg: &kage_plugin::PluginArgSpec) -> kage_tui::command::OwnedArgSpec {
    use kage_plugin::PluginArgSpec as P;
    use kage_tui::command::OwnedArgSpec as O;
    match arg {
        P::Text {
            name,
            optional,
            hint,
        } => O::Text {
            name: name.clone(),
            optional: *optional,
            hint: hint.clone(),
        },
        P::Choice {
            name,
            values,
            optional,
        } => O::Choice {
            name: name.clone(),
            values: values.clone(),
            optional: *optional,
        },
        P::Path { name, optional } => O::Path {
            name: name.clone(),
            optional: *optional,
        },
        P::Session { name, optional } => O::Session {
            name: name.clone(),
            optional: *optional,
        },
        P::Flag { name } => O::Flag { name: name.clone() },
    }
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
/// full context; a toast flags the substitution.
/// Read the JSONL session at `path` and return the id of its final
/// non-header entry, or `None` if the file holds only a header.
fn find_last_entry(
    path: &std::path::Path,
) -> Result<Option<kage_session::EntryId>, kage_session::SessionError> {
    let reader = SessionReader::iter(path)?;
    let mut last = None;
    for item in reader {
        let entry = item?;
        if !matches!(entry, kage_session::SessionEntry::Header(_)) {
            last = Some(entry.id());
        }
    }
    Ok(last)
}

/// Handle a plugin-initiated [`RunRequest::ForkSession`]. Copies the
/// current session up through entry `at` (or the latest entry when
/// `at` is empty) into a fresh session file and pushes a toast so the
/// user can see the new id. Errors surface as `kage:error` blocks.
fn handle_plugin_fork(
    session_path: Option<&Arc<Mutex<PathBuf>>>,
    buffer: &SharedBuffer,
    toasts: &SharedToasts,
    at: &str,
) {
    let Some(sp) = session_path else {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "fork: no active session to fork".to_owned(),
                false,
            );
        }
        return;
    };
    let src_path = sp.lock().expect("session path mutex poisoned").clone();
    if !src_path.exists() {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "fork: current session has no committed entries yet".to_owned(),
                false,
            );
        }
        return;
    }
    let entry = if at.is_empty() {
        match find_last_entry(&src_path) {
            Ok(Some(id)) => id,
            Ok(None) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom(
                        "kage:error",
                        "fork: current session has no entries to fork at".to_owned(),
                        false,
                    );
                }
                return;
            }
            Err(e) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom("kage:error", format!("fork: {e}"), false);
                }
                return;
            }
        }
    } else {
        match kage_session::resolve_entry_prefix(&src_path, at) {
            Ok(id) => id,
            Err(e) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom("kage:error", format!("fork: {e}"), false);
                }
                return;
            }
        }
    };
    let Some(dir) = src_path.parent() else {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "fork: session path has no parent directory".to_owned(),
                false,
            );
        }
        return;
    };
    let new_session = SessionId::new();
    let dst = dir.join(format!("{new_session}.jsonl"));
    if let Err(e) = kage_session::fork(&src_path, &dst, new_session, entry) {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom("kage:error", format!("fork failed: {e}"), false);
        }
        return;
    }
    let short: String = new_session.to_string().chars().take(8).collect();
    push_toast(toasts, Toast::info(format!("forked session: {short}")));
}

#[allow(clippy::too_many_arguments)]
fn handle_resume(
    registry: &Arc<ProviderRegistry>,
    active_qualified: &Arc<Mutex<String>>,
    cx: &Arc<Mutex<AgentContext>>,
    buffer: &SharedBuffer,
    session_path: Option<&Arc<Mutex<PathBuf>>>,
    session_usage: &SharedSessionUsage,
    toasts: &SharedToasts,
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
    let context_window;
    let resumed_level = replay
        .thinking_level
        .as_deref()
        .and_then(kage_loop::ThinkingLevel::parse);
    {
        let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
        cx_guard.history.clone_from(&replay.history);
        cx_guard.model = bare_model;
        if let Some(window) = crate::runtime_env::context_window_for(&qualified_model) {
            cx_guard.context_window = window;
        }
        cx_guard.max_output_tokens = crate::runtime_env::max_output_tokens_for(&qualified_model);
        cx_guard.thinking_level = resumed_level;
        cx_guard.budget.used_input = replay.usage_total.input;
        cx_guard.budget.used_output = replay.usage_total.output;
        cx_guard.budget.used_cache_read = replay.usage_total.cache_read;
        cx_guard.budget.used_cache_write = replay.usage_total.cache_write;
        cx_guard.budget.current_context = replay.usage_total.last_context;
        context_window = cx_guard.context_window;
    }
    if let Ok(mut snap) = session_usage.lock() {
        snap.model.clone_from(&qualified_model);
        snap.context_window = context_window;
        snap.input_tokens = replay.usage_total.input;
        snap.output_tokens = replay.usage_total.output;
        snap.cache_read_tokens = replay.usage_total.cache_read;
        snap.cache_write_tokens = replay.usage_total.cache_write;
        snap.current_context = replay.usage_total.last_context;
        snap.thinking_level = resumed_level;
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
    let id = replay.header.session.to_string();
    let short: String = id.chars().take(8).collect();
    if let Ok(mut buf) = buffer.lock() {
        buf.clear();
        populate_from_history(&mut buf, &replay.history, &replay.tool_durations);
    }
    push_toast(
        toasts,
        Toast::info(format!("resumed session {short} on {qualified_model}")),
    );
    if let Some(note) = fallback_note {
        push_toast(toasts, Toast::info(note));
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
