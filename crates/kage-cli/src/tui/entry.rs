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
            let mut buf = lock(&buffer);
            buf.push_custom(
                "kage:notify",
                format!("kage updated: {prev} -> {current_version}"),
                false,
            );
        }
        Ok(None) => {}
        Err(err) => {
            let mut buf = lock(&buffer);
            buf.push_custom("kage:error", format!("state: {err}"), false);
        }
    }
    crate::trust::confirm_project_trust(&workdir);
    // Load user/project config and map the loop-tunable subset onto
    // the real LoopConfig. A malformed config is surfaced as an inline
    // error block rather than silently falling back to defaults.
    let app_config = match kage_core::config::Config::load_layered(&workdir) {
        Ok(c) => c,
        Err(e) => {
            let mut buf = lock(&buffer);
            buf.push_custom("kage:error", format!("config: {e}"), false);
            kage_core::config::Config::default()
        }
    };
    // Structurally broken permission rules are a hard error: kage
    // would silently misapply them otherwise. Mirrors the providers
    // validation in `build_provider_registry`.
    if let Err(e) = app_config.permissions.validate() {
        eprintln!("kage: {e}");
        return ExitCode::from(1);
    }
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
            let mut buf = lock(&buffer);
            buf.push_custom("kage:error", e, false);
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
                let mut buf = lock(&buffer);
                buf.push_custom("kage:error", e, false);
                None
            }
        },
        None => None,
    };
    if let Some(rt) = plugin_runtime.as_ref() {
        crate::plugins::merge_plugin_providers(rt, &mut registry);
        crate::acp_glue::set_runtime(rt);
    }

    let requested_model = model.map_or_else(|| crate::default_model(&registry), str::to_owned);
    if !crate::has_usable_provider(&registry) && registry.resolve(&requested_model).is_err() {
        // First-run onboarding: with no credentials anywhere, walk the
        // new user through `auth login` (its provider picker and key
        // prompt run in the normal terminal before the TUI starts)
        // instead of exiting with instructions. Esc on the picker or
        // an empty key falls back to the env-var guidance and exits.
        eprintln!("kage: welcome! no provider credentials found, let's add one.");
        loop {
            let code = crate::auth::run_login(None, &app_config);
            if code != ExitCode::SUCCESS {
                eprintln!("{}", crate::NO_CREDENTIALS_MESSAGE);
                return ExitCode::from(1);
            }
            registry = crate::build_provider_registry();
            if let Some(rt) = plugin_runtime.as_ref() {
                crate::plugins::merge_plugin_providers(rt, &mut registry);
            }
            if crate::has_usable_provider(&registry) {
                break;
            }
            eprintln!("kage: credential saved, but no provider is usable yet; add another.");
        }
        eprintln!("kage: connected. starting kage...");
    }
    // Recompute the default against the merged registry so a last-used
    // plugin-provided model resolves on restart.
    let qualified_model = match model {
        Some(m) => m.to_owned(),
        None => crate::default_model(&registry),
    };
    if qualified_model.is_empty() {
        eprintln!("{}", crate::NO_MODEL_MESSAGE);
        return ExitCode::from(1);
    }
    let bare_model = match registry.resolve(&qualified_model) {
        Ok(r) => r.model.clone(),
        Err(e) => {
            eprintln!("kage: cannot resolve model {qualified_model}: {e}");
            return ExitCode::from(1);
        }
    };
    if model.is_none()
        && let Some(notice) = crate::default_model_notice(&registry, &qualified_model)
    {
        lock(&buffer).push_custom("kage:notify", notice, false);
    }
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
        plugin_command_listing = support::snapshot_plugin_commands(rt);
        support::register_block_renderers(rt);
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
        let mut buf = lock(&buffer);
        buf.push_custom("kage:error", format!("mcp `{server}`: {err}"), false);
    }
    let mut cx = AgentContext::new(bare_model, system).with_workdir(&workdir);
    if app_config.permissions.confine_paths {
        cx = cx.with_confine_paths();
    }
    if let Some(window) = crate::runtime_env::context_window_for(&registry, &qualified_model) {
        cx = cx.with_context_window(window);
    }
    if let Some(out) = crate::runtime_env::max_output_tokens_for(&registry, &qualified_model) {
        cx = cx.with_max_output_tokens(out);
    }
    if let Some(level) = app_config.ui.thinking_level.as_deref() {
        if let Some(parsed) = kage_provider::ThinkingLevel::parse(level) {
            cx = cx.with_thinking_level(parsed);
        } else {
            let mut buf = lock(&buffer);
            buf.push_custom(
                "kage:error",
                format!("config: unknown ui.thinking_level `{level}`"),
                false,
            );
        }
    }
    let (tx, rx) = mpsc::channel::<RunRequest>();
    let tx_watcher = tx.clone();
    let (dialog_tx, dialog_rx) = mpsc::channel::<PluginDialog>();
    let (plugin_refresh_tx, plugin_refresh_rx) = mpsc::channel::<PluginRefresh>();
    let gate = crate::permissions::PermissionGate::new(app_config.permissions.clone())
        .with_mcp_servers(mcp_manager.server_names().map(str::to_owned).collect());

    {
        let mut buf = lock(&buffer);
        buf.push_custom(
            "kage:help",
            "welcome to kage - ? for keys, / for commands, /settings to theme",
            false,
        );
    }
    let model_choices = available_model_items(&registry, &qualified_model);
    if let Err(err) = crate::state::record_last_model(&qualified_model) {
        let mut buf = lock(&buffer);
        buf.push_custom("kage:error", format!("state: {err}"), false);
    }

    let planned = match crate::plan_session(&qualified_model, system) {
        Ok(planned) => Some(planned),
        Err(e) => {
            let mut buf = lock(&buffer);
            buf.push_custom("kage:error", format!("session: {e}"), false);
            None
        }
    };
    let session_id = planned
        .as_ref()
        .map_or_else(kage_core::SessionId::new, |(_, header)| header.session);
    let mirror = Arc::new(Mutex::new(host::Mirror::new(
        planned.as_ref().map(|(path, _)| path.clone()),
    )));
    let engine = crate::engine::Engine::start(Arc::clone(&registry));
    let (events_tx, events_rx) = mpsc::channel();
    engine.subscribe(Box::new(move |envelope| {
        let _ = events_tx.send(envelope.clone());
    }));
    engine.subscribe(Box::new(host::mirror(
        Arc::clone(&mirror),
        plugin_runtime.clone(),
    )));
    engine.open(crate::engine::SessionSpec {
        id: session_id,
        model: qualified_model.clone(),
        cx,
        recorder: planned.map(|(path, header)| {
            crate::engine::Recorder::planned(path, header, plugin_runtime.clone())
        }),
        tools,
        plugins: plugin_runtime.clone(),
        gate,
        loop_cfg,
        mcp: Some(mcp_manager),
        interactive: true,
        title: true,
    });
    host::Host {
        commander: engine.commander(),
        registry: Arc::clone(&registry),
        plugins: plugin_runtime.clone(),
        plugins_dir: plugins_dir_path.clone(),
        dialog_tx,
        plugin_refresh_tx,
        mirror: Arc::clone(&mirror),
    }
    .spawn(rx);

    // Hot-reload watcher: polls the plugins dir every 150ms and asks
    // for a reload when a `.lua` file changes. It ends with the process.
    if plugin_runtime.is_some()
        && let Some(dir) = plugins_dir_path.as_ref()
    {
        let dir = dir.clone();
        let buf = buffer.clone();
        thread::spawn(move || {
            let watcher = match kage_plugin::PluginWatcher::new(dir) {
                Ok(w) => w,
                Err(err) => {
                    let mut b = lock(&buf);
                    b.push_custom("kage:error", format!("plugin watcher: {err}"), false);
                    return;
                }
            };
            loop {
                thread::sleep(std::time::Duration::from_millis(150));
                if watcher.poll() && tx_watcher.send(RunRequest::ReloadPlugins).is_err() {
                    return;
                }
            }
        });
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
    app.set_status_model(Arc::new(Mutex::new(qualified_model.clone())));
    app.set_engine_events(events_rx);
    app.set_plugin_commands(plugin_command_listing);
    app.set_plugin_widgets(plugin_widgets);
    app.set_plugin_autocomplete(plugin_autocomplete);
    app.set_editor_modeless(matches!(
        app_config.ui.editor,
        kage_core::config::EditorMode::Modeless
    ));
    // `:login` runs the interactive credential flow in the real
    // terminal (the App suspends itself around the call) and then
    // refreshes providers through the worker.
    {
        let config_for_login = app_config.clone();
        app.set_login_runner(std::sync::Arc::new(move |provider| {
            crate::auth::run_login(provider, &config_for_login) == ExitCode::SUCCESS
        }));
    }
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
    if let Some(rt) = plugin_runtime.as_ref() {
        app.set_plugin_redraw(rt.redraw_flag(), rt.blocks_flag());
    }
    app.set_plugin_dialog(dialog_rx);
    app.set_plugin_refresh(plugin_refresh_rx);
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
        let mut buf = lock(&buffer);
        buf.push_custom("kage:error", err, false);
    }
    app.set_toasts(toasts.clone());
    app.set_session_usage(shared_session_usage());
    app.set_status_session_id(session_id.to_string().chars().take(8).collect());
    if let Ok(dir) = crate::sessions_dir() {
        let tree_dir = dir.clone();
        let tree_mirror = Arc::clone(&mirror);
        let lister_workdir = workdir.clone();
        app.set_session_lister(Box::new(move |all| {
            list_session_choices(&dir, &lister_workdir, all)
        }));
        app.set_session_tree_source(Box::new(move || {
            list_session_nodes(&tree_dir, lock(&tree_mirror).path())
        }));
    }
    let result = app.run(&mut tui);
    drop(tui);
    drop(app);
    engine.shutdown();

    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: tui error: {e}");
            ExitCode::from(1)
        }
    }
}
