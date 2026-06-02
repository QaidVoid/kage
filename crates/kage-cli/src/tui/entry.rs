//! Interactive TUI entry point: `run_tui`.

#[allow(clippy::wildcard_imports)] // tui split: shares the parent module scope
use super::*;

/// Drop into the interactive TUI. Returns the appropriate process exit
/// code once the user quits.
#[allow(clippy::too_many_lines)]
pub fn run_tui(model: Option<&str>, system: &str) -> ExitCode {
    let mut registry = crate::build_provider_registry();
    let provisional_model = model.map_or_else(|| crate::default_model(&registry), str::to_owned);

    // The buffer must exist before we build the plugin runtime so we can
    // hand the runtime a sink that routes notify/log into the buffer
    // instead of stderr (which would corrupt the alt screen).
    let buffer = shared_buffer();
    let toasts = shared_toasts();
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // First TUI launch after an upgrade gets a one-line scrollback notice.
    // A read or write failure here is non-fatal: the worst case is missing
    // the notice on this boot.
    let current_version = env!("CARGO_PKG_VERSION");
    match crate::state::record_version_seen(current_version) {
        Ok(Some(prev)) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom(
                    "kage:notify",
                    format!("kage updated: {prev} -> {current_version}"),
                    false,
                );
            }
        }
        Ok(None) => {}
        Err(err) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", format!("state: {err}"), false);
            }
        }
    }
    // Load user/project config and map the loop-tunable subset onto
    // the real LoopConfig. A malformed config is surfaced as an inline
    // error block rather than silently falling back to defaults.
    let app_config = match kage_core::config::Config::load_layered(&workdir) {
        Ok(c) => c,
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", format!("config: {e}"), false);
            }
            kage_core::config::Config::default()
        }
    };
    let loop_cfg = LoopConfig {
        compaction_threshold: app_config.loop_settings.compaction_threshold,
        ..LoopConfig::default()
    };
    // Build the plugin runtime against a bare prompt first; skills land
    // below once plugins have had a chance to contribute extra dirs via
    // `resources_discover`.
    let bare_prompt =
        crate::runtime_env::build_system_prompt(system, &workdir, &provisional_model, &[]);
    // Resolve once up-front so the same path is shared by initial load,
    // the file-system watcher, and the worker's reload handler.
    let plugins_dir_path = match crate::plugins_dir() {
        Ok(dir) => Some(dir),
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", e, false);
            }
            None
        }
    };
    let plugin_runtime = match plugins_dir_path.as_ref() {
        Some(dir) => match setup_runtime_with_sink(
            dir,
            &workdir,
            &provisional_model,
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
        None => None,
    };
    if let Some(rt) = plugin_runtime.as_ref() {
        crate::plugins::merge_plugin_providers(rt, &mut registry);
        crate::acp_glue::set_runtime(rt);
    }

    if registry.ids().count() == 0 {
        eprintln!(
            "kage: no provider credentials found. Run `kage auth login` to save \
             one, or export an env var (ANTHROPIC_API_KEY, OPENAI_API_KEY, \
             GEMINI_API_KEY, ZAI_API_KEY, ZAI_CODING_API_KEY)."
        );
        return ExitCode::from(1);
    }
    // Recompute the default against the merged registry so a last-used
    // plugin-provided model resolves on restart.
    let qualified_model = match model {
        Some(m) => m.to_owned(),
        None => crate::default_model(&registry),
    };
    let bare_model = match registry.resolve(&qualified_model) {
        Ok(r) => r.model.clone(),
        Err(e) => {
            eprintln!("kage: cannot resolve model {qualified_model}: {e}");
            return ExitCode::from(1);
        }
    };
    let registry = Arc::new(registry);

    let skills = crate::load_skills(&workdir, plugin_runtime.as_deref());
    let system_prompt =
        crate::runtime_env::build_system_prompt(system, &workdir, &qualified_model, &skills);
    let system = system_prompt.as_str();

    let mut tools = kage_tools::builtin_registry();
    let mut plugin_command_listing: Vec<kage_tui::command::PluginCommand> = Vec::new();
    let mut plugin_widgets: Vec<Arc<kage_plugin::LuaWidget>> = Vec::new();
    let mut plugin_autocomplete: Vec<Arc<kage_plugin::LuaAutocompleteProvider>> = Vec::new();
    let mut plugin_status: Option<kage_plugin::SharedStatus> = None;
    let mut plugin_usage: Option<kage_plugin::SharedUsage> = None;
    let mut plugin_compact_request: Option<kage_plugin::SharedCompactRequest> = None;
    let mut plugin_session_list: Option<kage_plugin::SharedSessionList> = None;
    let mut plugin_fork_request: Option<kage_plugin::SharedForkRequest> = None;
    let mut plugin_switch_request: Option<kage_plugin::SharedSwitchRequest> = None;
    let mut plugin_theme: Option<(
        kage_plugin::SharedThemeState,
        kage_plugin::SharedThemeRequest,
    )> = None;
    let mut plugin_chrome: Option<(kage_plugin::SharedChrome, kage_plugin::SharedChrome)> = None;
    let mut plugin_terminal_hooks: Option<kage_plugin::RegisteredTerminalHooks> = None;
    let mut plugin_keybinding_chords: Vec<String> = Vec::new();
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
                aliases: cmd.aliases().to_vec(),
                is_override: false,
                description: cmd.description().to_owned(),
                args: cmd.args().iter().map(translate_plugin_arg).collect(),
            });
        }
        for cmd in rt.registered_command_overrides() {
            plugin_command_listing.push(kage_tui::command::PluginCommand {
                name: cmd.name().to_owned(),
                aliases: cmd.aliases().to_vec(),
                is_override: true,
                description: cmd.description().to_owned(),
                args: cmd.args().iter().map(translate_plugin_arg).collect(),
            });
        }
        for renderer in rt.registered_block_renderers() {
            let kind = renderer.kind().to_owned();
            let factory = std::sync::Arc::new(
                kage_tui::view::plugin_block::PluginBlockFactory::new(renderer),
            );
            match kage_tui::view::registry::builtin_kind_from_name(&kind) {
                Some(builtin) => {
                    kage_tui::view::registry::register_builtin(builtin, factory);
                }
                None => kage_tui::view::registry::register_custom(kind, factory),
            }
        }
        plugin_widgets = rt.registered_widgets();
        plugin_autocomplete = rt.registered_autocomplete_providers();
        plugin_status = Some(rt.shared_status());
        plugin_usage = Some(rt.shared_usage());
        plugin_compact_request = Some(rt.shared_compact_request());
        plugin_session_list = Some(rt.shared_session_list());
        plugin_fork_request = Some(rt.shared_fork_request());
        plugin_switch_request = Some(rt.shared_switch_request());
        plugin_theme = Some((rt.shared_theme_state(), rt.shared_theme_request()));
        plugin_chrome = Some((rt.shared_header(), rt.shared_footer()));
        plugin_terminal_hooks = Some(rt.shared_terminal_hooks());
        plugin_keybinding_chords = rt
            .registered_keybindings()
            .iter()
            .map(|kb| kb.chord().to_owned())
            .collect();
    }
    let (mcp_manager, mcp_errors) =
        crate::mcp::spawn_and_register(&mut tools, &workdir, plugin_runtime.as_deref());
    for (server, err) in mcp_errors {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom("kage:error", format!("mcp `{server}`: {err}"), false);
        }
    }
    let cancel = CancelFlag::new();
    let mut initial_cx = AgentContext::new(bare_model, system).with_workdir(&workdir);
    if let Some(window) = crate::runtime_env::context_window_for(&registry, &qualified_model) {
        initial_cx = initial_cx.with_context_window(window);
    }
    if let Some(out) = crate::runtime_env::max_output_tokens_for(&registry, &qualified_model) {
        initial_cx = initial_cx.with_max_output_tokens(out);
    }
    let cx = Arc::new(Mutex::new(initial_cx));
    let (tx, rx) = mpsc::channel::<RunRequest>();
    // The worker keeps its own sender so it can re-queue any user
    // prompts left in the steering queue after a run finishes (e.g.
    // because the model emitted text without tool calls, exiting the
    // inner loop before the next steering drain could pick them up).
    let tx_worker = tx.clone();
    // The watcher thread (spawned below) holds its own clone so a
    // plugin file change wakes the worker even when the user is idle.
    let tx_watcher = tx.clone();
    let steering = kage_tui::shared_steering();
    let (dialog_tx, dialog_rx) = mpsc::channel::<PluginDialog>();

    // Plan a session up-front but defer creating the file until the
    // first prompt actually lands. Otherwise quitting or resuming
    // immediately would leave an empty header-only stub on disk.
    let (session_path, session_header) = match crate::plan_session(&qualified_model, system) {
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
    let has_plugin_runtime = plugin_runtime.is_some();
    let worker = spawn_worker(WorkerConfig {
        registry: Arc::clone(&registry),
        active_qualified: Arc::clone(&active_qualified),
        tools,
        mcp_manager,
        cx: Arc::clone(&cx),
        buffer: buffer.clone(),
        cancel: cancel.clone(),
        plugin_runtime,
        rx,
        session_path: session_path.clone(),
        session_header: session_header.clone(),
        session_usage: session_usage.clone(),
        toasts: toasts.clone(),
        dialog_tx,
        loop_cfg,
        steering: steering.clone(),
        tx_self: tx_worker,
        plugins_dir: plugins_dir_path.clone(),
    });

    // Hot-reload watcher: a small thread owns the FS watcher, polls it
    // every 150ms, and fires `ReloadPlugins` through the worker channel
    // when a `.lua` file under the plugins dir changes. The thread exits
    // when its `send` fails (channel disconnected at TUI shutdown).
    if has_plugin_runtime && let Some(dir) = plugins_dir_path.as_ref() {
        let dir = dir.clone();
        let tx = tx_watcher;
        let buf = buffer.clone();
        thread::spawn(move || {
            let watcher = match kage_plugin::PluginWatcher::new(dir) {
                Ok(w) => w,
                Err(err) => {
                    if let Ok(mut b) = buf.lock() {
                        b.push_custom("kage:error", format!("plugin watcher: {err}"), false);
                    }
                    return;
                }
            };
            loop {
                thread::sleep(std::time::Duration::from_millis(150));
                if watcher.poll() && tx.send(RunRequest::ReloadPlugins).is_err() {
                    return;
                }
            }
        });
    } else {
        // `tx_watcher` is unused when there is no plugin runtime; drop
        // it explicitly so the channel still closes when the App exits.
        drop(tx_watcher);
    }

    let mut tui = match Tui::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("kage: failed to enter raw mode: {e}");
            return ExitCode::from(1);
        }
    };
    let mut app = App::new(buffer.clone(), tx);
    app.set_model_choices(model_choices);
    app.set_history(crate::history::load());
    app.set_status_model(Arc::clone(&active_qualified));
    app.set_plugin_commands(plugin_command_listing);
    app.set_plugin_widgets(plugin_widgets);
    app.set_plugin_autocomplete(plugin_autocomplete);
    app.set_editor_modeless(matches!(
        app_config.ui.editor,
        kage_core::config::EditorMode::Modeless
    ));
    app.set_workdir(workdir.clone());
    if let Ok(dir) = crate::themes_dir() {
        app.set_themes_dir(dir);
    }
    app.apply_startup_theme(&app_config.ui.theme);
    kage_tui::layout::set_input_bounds(
        app_config.ui.input_min_lines,
        app_config.ui.input_max_lines,
    );
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
    if let Some(req) = plugin_switch_request {
        app.set_plugin_switch_request(req);
    }
    if let Some((state, request)) = plugin_theme {
        app.set_plugin_theme(state, request);
    }
    if let Some((header, footer)) = plugin_chrome {
        app.set_plugin_chrome(header, footer);
    }
    if let Some(hooks) = plugin_terminal_hooks {
        app.set_plugin_terminal_hooks(hooks);
    }
    app.set_plugin_dialog(dialog_rx);
    app.set_plugin_keybindings(plugin_keybinding_chords);
    let keybinding_errors = app.set_config_keybindings(
        app_config
            .keybindings
            .bindings
            .iter()
            .map(|(chord, command)| (chord.clone(), command.clone()))
            .collect(),
    );
    for err in keybinding_errors {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom("kage:error", err, false);
        }
    }
    app.set_cancel_flag(cancel.clone());
    app.set_toasts(toasts.clone());
    app.set_session_usage(session_usage);
    app.set_steering_queue(steering.clone());
    if let Some(p) = session_path.as_ref() {
        let path = p.lock().expect("session path mutex poisoned").clone();
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            app.set_status_session_id(stem.chars().take(8).collect());
        }
    }
    if let Ok(dir) = crate::sessions_dir() {
        let tree_dir = dir.clone();
        let tree_sp = session_path.clone();
        let lister_workdir = workdir.clone();
        app.set_session_lister(Box::new(move |all| {
            list_session_choices(&dir, &lister_workdir, all)
        }));
        app.set_session_tree_source(Box::new(move || {
            list_session_nodes(&tree_dir, tree_sp.as_ref())
        }));
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
