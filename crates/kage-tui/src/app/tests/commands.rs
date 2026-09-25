//! Commands: validation, the `:` line and the `/` palette, and the built-
//! in commands.

use super::*;

#[test]
fn events_command_lists_known_hooks_by_kind() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    app.push_events();
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last() {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("tool_call"), "{rendered}");
    assert!(rendered.contains("transform_context"), "{rendered}");
    assert!(rendered.contains("should_stop_after_turn"), "{rendered}");
    assert!(rendered.contains("predicate:"), "{rendered}");
}

#[test]
fn usage_command_renders_totals_cache_and_context_bar() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    let usage = crate::usage::shared_session_usage();
    app.set_session_usage(usage.clone());
    {
        let mut u = lock(&usage);
        u.model = "p:m".to_owned();
        u.input_tokens = 650_200_000;
        u.output_tokens = 1_800_000;
        u.cache_read_tokens = 12_400_000;
        u.cache_write_tokens = 3_100_000;
        u.current_context = 190_000;
        u.context_window = 250_000;
        u.total_cost = 1.23;
    }
    app.push_usage();
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last() {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("Session usage"), "{rendered}");
    assert!(rendered.contains("p:m"), "{rendered}");
    assert!(rendered.contains("input 650.2M"), "{rendered}");
    assert!(rendered.contains("output 1.8M"), "{rendered}");
    assert!(rendered.contains("cache read 12.4M"), "{rendered}");
    assert!(rendered.contains("cache write 3.1M"), "{rendered}");
    assert!(rendered.contains("total 652M"), "{rendered}");
    assert!(rendered.contains("($1.23)"), "{rendered}");
    assert!(rendered.contains("Context window"), "{rendered}");
    assert!(rendered.contains("76%"), "{rendered}");
    assert!(rendered.contains("(190k / 250k)"), "{rendered}");
    let bar = "\u{2588}".repeat(15) + &"\u{2591}".repeat(5);
    assert!(rendered.contains(&bar), "{rendered}");
}

#[test]
fn usage_command_without_window_or_cost() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    app.push_usage();
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last() {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("input 0"), "{rendered}");
    assert!(rendered.contains("(unknown window)"), "{rendered}");
    assert!(!rendered.contains('$'), "{rendered}");
}

#[test]
fn ctrl_c_interrupts_over_an_open_cmdline() {
    let (mut app, rx, _events) = app_with_events();
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(key(':'));
    assert!(app.cmdline.is_some(), "cmdline should be open");
    lock(app.session_usage.as_ref().unwrap()).working = true;
    app.handle_key(ctrl('c'));
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel { session: None }));
    assert!(
        app.cmdline.is_some(),
        "interrupt must not close the cmdline"
    );
    lock(app.session_usage.as_ref().unwrap()).working = false;
    app.handle_key(ctrl('c'));
    assert!(app.cmdline.is_none(), "idle, ctrl+c closes the cmdline");
    assert!(rx.try_recv().is_err());
}

#[test]
fn cancel_command_sends_a_cancel_request() {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();
    let result = app.run_command_validated("cancel", &registry);
    assert!(
        matches!(result, CommandResult::Done(None)),
        "expected Done(None), got {result:?}"
    );
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel { session: None }));
}

#[test]
fn permission_command_dispatches_mode_override() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();

    let result = app.run_command_validated("permission ask", &registry);
    assert!(matches!(result, CommandResult::Done(None)));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::SetPermissionMode(mode) => {
            assert_eq!(mode, Some(kage_core::permissions::PermissionAction::Ask));
        }
        other => panic!("expected SetPermissionMode, got {other:?}"),
    }

    let _ = app.run_command_validated("permission deny", &registry);
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::SetPermissionMode(mode) => {
            assert_eq!(mode, Some(kage_core::permissions::PermissionAction::Deny));
        }
        other => panic!("expected SetPermissionMode, got {other:?}"),
    }

    let _ = app.run_command_validated("permission default", &registry);
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::SetPermissionMode(mode) => assert_eq!(mode, None),
        other => panic!("expected SetPermissionMode, got {other:?}"),
    }
}

#[test]
fn permission_command_rejects_unknown_mode_without_request() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();

    let result = app.run_command_validated("permission bogus", &registry);
    assert!(
        matches!(result, CommandResult::ValidationError(_)),
        "arg validation should reject an unknown mode, got {result:?}"
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "no request should be sent for an invalid mode"
    );
}

#[test]
fn permission_command_without_arg_reports_current_mode() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();

    let result = app.run_command_validated("permission", &registry);
    assert!(matches!(result, CommandResult::Done(None)));
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last() {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("permission mode: default"), "{rendered}");
}

fn builtin_registry() -> Vec<&'static CommandSpec> {
    BUILTIN_COMMANDS.iter().collect()
}

#[test]
fn validated_unknown_command_returns_error_with_suggestion() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    let result = app.run_command_validated("quut", &registry);
    match result {
        CommandResult::ValidationError(msg) => {
            assert!(msg.contains("unknown command: quut"), "got {msg:?}");
            assert!(
                msg.contains("did you mean /quit?"),
                "should suggest closest match with the / sigil, got {msg:?}"
            );
        }
        other @ CommandResult::Done(_) => {
            panic!("expected ValidationError, got {other:?}");
        }
    }
}

#[test]
fn the_colon_line_suggests_with_its_own_prefix() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.handle_key(code(KeyCode::Esc));
    app.handle_key(key(':'));
    type_str(&mut app, "quut");
    app.handle_key(code(KeyCode::Enter));
    let error = app.cmdline.as_ref().and_then(|c| c.error()).unwrap_or("");
    assert_eq!(error, "unknown command: quut (did you mean :quit?)");
}

#[test]
fn validated_invalid_choice_returns_error() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    // "mouse mayb" is invalid: "mayb" is not in [on, off, toggle]
    let result = app.run_command_validated("mouse mayb", &registry);
    match result {
        CommandResult::ValidationError(msg) => {
            assert!(
                msg.contains("state"),
                "error should mention the arg name, got {msg:?}"
            );
        }
        other @ CommandResult::Done(_) => {
            panic!("expected ValidationError, got {other:?}");
        }
    }
}

#[test]
fn validated_missing_required_arg_returns_error() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    let result = app.run_command_validated("fold", &registry);
    match result {
        CommandResult::ValidationError(msg) => {
            assert!(
                msg.contains("missing"),
                "error should mention missing arg, got {msg:?}"
            );
        }
        other @ CommandResult::Done(_) => {
            panic!("expected ValidationError, got {other:?}");
        }
    }
}

#[test]
fn validated_valid_command_returns_done() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    let result = app.run_command_validated("help", &registry);
    assert!(
        matches!(result, CommandResult::Done(_)),
        "expected Done, got {result:?}"
    );
}

#[test]
fn validated_quit_returns_done_with_exit() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    let result = app.run_command_validated("quit", &registry);
    assert!(
        matches!(result, CommandResult::Done(Some(AppExit::Quit))),
        "expected Done(Some(Quit)), got {result:?}"
    );
}

#[test]
fn validated_subcommand_validates_against_leaf_spec() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    // "theme set" without required <name> arg should error
    let result = app.run_command_validated("theme set", &registry);
    match result {
        CommandResult::ValidationError(msg) => {
            assert!(
                msg.contains("missing"),
                "error should mention missing arg, got {msg:?}"
            );
        }
        other @ CommandResult::Done(_) => {
            panic!("expected ValidationError, got {other:?}");
        }
    }
}

#[test]
fn validated_empty_input_returns_done_none() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    let result = app.run_command_validated("", &registry);
    assert!(
        matches!(result, CommandResult::Done(None)),
        "expected Done(None), got {result:?}"
    );
}

#[test]
fn validated_optional_arg_missing_is_ok() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let registry = builtin_registry();
    // "mouse" without arg is valid (optional arg)
    let result = app.run_command_validated("mouse", &registry);
    assert!(
        matches!(result, CommandResult::Done(_)),
        "mouse with no arg should be valid, got {result:?}"
    );
}

// These tests drive the modal state machine with raw `KeyEvent`s
// and confirm that `:` and `/` both reach `run_command_validated`
// through `dispatch_key`.

#[test]
fn colon_keystrokes_dispatch_quit_handler() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    // Default mode is Insert; switch to Normal so `:` is bound.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let exit = app.handle_key(key(':'));
    assert!(exit.is_none());
    assert!(app.cmdline.is_some(), "':' should open the command line");
    type_str(&mut app, "quit");
    let exit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(exit, Some(AppExit::Quit));
    assert!(app.cmdline.is_none(), "successful submit closes cmdline");
}

#[test]
fn slash_keystrokes_dispatch_quit_handler() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    // Default mode is Insert. `/` only opens the palette when the
    // input buffer is empty; that is the case for a fresh App.
    let exit = app.handle_key(key('/'));
    assert!(exit.is_none());
    assert!(
        app.slash_palette.is_some(),
        "'/' should open the slash palette"
    );
    type_str(&mut app, "quit");
    let exit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(exit, Some(AppExit::Quit));
    assert!(
        app.slash_palette.is_none(),
        "successful submit closes the palette"
    );
}

/// Two plugin commands whose names share the prefix `zz-`, so the tab
/// tests do not depend on which builtins start alike.
fn lcp_commands(app: &mut App) {
    let command = |name: &str| PluginCommand {
        name: name.into(),
        aliases: Vec::new(),
        is_override: false,
        description: String::new(),
        args: Vec::new(),
    };
    app.set_plugin_commands(vec![command("zz-one"), command("zz-two")]);
}

#[test]
fn colon_tab_completes_to_lcp_and_opens_popup() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    lcp_commands(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(key(':'));
    app.handle_key(key('z'));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let cl = app.cmdline.as_ref().expect("cmdline open");
    assert_eq!(cl.text(), "zz-", "tab should extend to the LCP");
    assert!(cl.popup_open(), "popup should be visible after LCP step");
    assert_eq!(cl.selected(), None, "LCP step does not pre-select a row");
}

#[test]
fn slash_tab_completes_to_lcp_and_opens_popup() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    lcp_commands(&mut app);
    app.handle_key(key('/'));
    app.handle_key(key('z'));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let sp = app.slash_palette.as_ref().expect("palette open");
    let cl = sp.cmdline();
    assert_eq!(cl.text(), "zz-", "tab should extend to the LCP");
    assert_eq!(palette_selected(&app).as_deref(), Some("zz-one"));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let cl = app.slash_palette.as_ref().expect("palette open").cmdline();
    assert_eq!(
        cl.text(),
        "zz-one ",
        "the next tab inserts the highlighted row"
    );
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let cl = app.slash_palette.as_ref().expect("palette open").cmdline();
    assert_eq!(cl.text(), "zz-two ", "then tab cycles");
}

#[test]
fn palette_tab_completes_a_typed_alias_to_its_command() {
    let mut app = defaults_app();
    app.handle_key(key('/'));
    type_str(&mut app, "perm");
    assert_eq!(palette_values(&app), ["permission"]);
    assert_eq!(palette_selected(&app).as_deref(), Some("permission"));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let cl = app.slash_palette.as_ref().expect("palette open").cmdline();
    assert_eq!(cl.text(), "permission ");
}

#[test]
fn colon_bad_arg_keeps_cmdline_open_with_error() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(key(':'));
    type_str(&mut app, "mouse mayb");
    let exit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(exit.is_none());
    let cl = app.cmdline.as_ref().expect("cmdline stays open on bad arg");
    assert!(cl.error().is_some(), "validation error should be set");
}

#[test]
fn slash_bad_arg_keeps_palette_open_with_error() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.handle_key(key('/'));
    type_str(&mut app, "mouse mayb");
    let exit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(exit.is_none());
    let sp = app
        .slash_palette
        .as_ref()
        .expect("palette stays open on bad arg");
    assert!(
        sp.cmdline().error().is_some(),
        "validation error should be set on the palette"
    );
}

fn palette_values(app: &App) -> Vec<String> {
    let sp = app.slash_palette.as_ref().expect("palette open");
    let items = &sp.cmdline().completions().items;
    items.iter().map(|c| c.value.clone()).collect()
}

fn palette_selected(app: &App) -> Option<String> {
    let cl = app.slash_palette.as_ref().expect("palette open").cmdline();
    cl.selected()
        .map(|i| cl.completions().items[i].value.clone())
}

#[test]
fn palette_opens_with_model_selected_on_top() {
    let mut app = defaults_app();
    app.handle_key(key('/'));
    assert_eq!(palette_values(&app)[0], "model");
    assert_eq!(palette_selected(&app).as_deref(), Some("model"));
}

#[test]
fn palette_shows_an_alias_only_without_its_command() {
    let mut app = defaults_app();
    app.handle_key(key('/'));
    assert!(!palette_values(&app).iter().any(|v| v == "q"));
    app.handle_key(key('q'));
    assert!(!palette_values(&app).iter().any(|v| v == "q"));
    assert_eq!(palette_selected(&app).as_deref(), Some("quit"));
    app.handle_key(code(KeyCode::Backspace));
    app.handle_key(key('/'));
    type_str(&mut app, "img");
    assert_eq!(palette_values(&app), ["img"]);
}

#[test]
fn palette_first_down_selects_the_second_row() {
    let mut app = defaults_app();
    app.handle_key(key('/'));
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(palette_selected(&app), palette_values(&app).get(1).cloned());
}

#[test]
fn palette_enter_on_open_runs_the_model_picker() {
    let mut app = defaults_app();
    app.set_model_choices(vec![PickItem::simple("fake:m")]);
    app.handle_key(key('/'));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.slash_palette.is_none());
    assert!(app.picker.is_some(), "bare /model opens the picker");
    assert_eq!(app.picker_kind, Some(PickerKind::Model));
}

#[test]
fn slash_reload_sends_reload_plugins() {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    app.handle_key(key('/'));
    type_str(&mut app, "reload");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(rx.try_recv(), Ok(RunRequest::ReloadPlugins));
}

#[test]
fn login_command_defers_to_the_run_loop_via_pending_login() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_login_runner(std::sync::Arc::new(|_| true));
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();

    let result = app.run_command_validated("login", &registry);
    assert!(matches!(result, CommandResult::Done(None)));
    assert_eq!(app.pending_login, Some(crate::app::PendingLogin::Picker));

    let result = app.run_command_validated("login anthropic", &registry);
    assert!(matches!(result, CommandResult::Done(None)));
    assert_eq!(
        app.pending_login,
        Some(crate::app::PendingLogin::Provider("anthropic".to_owned()))
    );
}

#[test]
fn login_command_errors_without_a_runner() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();

    let result = app.run_command_validated("login", &registry);
    assert!(matches!(result, CommandResult::Done(None)));
    assert_eq!(app.pending_login, None, "no flow queued without a runner");
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last() {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("login: unavailable"), "{rendered}");
}

#[test]
fn export_uses_the_unquoted_parsed_path() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let res = app.run_command_validated("export \"a b.md\"", &builtin_registry());
    assert!(matches!(res, CommandResult::Done(None)));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::ExportSession(dest) => {
            assert_eq!(dest, Some(std::path::PathBuf::from("a b.md")));
        }
        other => panic!("expected ExportSession, got {other:?}"),
    }
}

#[test]
fn enter_on_a_command_missing_its_argument_names_it() {
    let mut app = defaults_app();
    app.set_editor_modeless(true);
    app.handle_key(key('/'));
    type_str(&mut app, "theme set");
    app.handle_key(code(KeyCode::Enter));
    let palette = app.slash_palette.as_ref().expect("the palette stays open");
    assert_eq!(palette.cmdline().text(), "theme set ");
    assert_eq!(
        palette.cmdline().error(),
        Some("missing required argument `name`")
    );
    let rows = rendered(&mut app, 80, 24);
    assert!(
        rows.iter()
            .any(|r| r.contains("! missing required argument `name`")),
        "{rows:#?}"
    );
    type_str(&mut app, "d");
    assert!(
        app.slash_palette
            .as_ref()
            .unwrap()
            .cmdline()
            .error()
            .is_none()
    );
}

#[test]
fn palette_descriptions_line_up_after_the_argument_hints() {
    let mut app = defaults_app();
    app.set_editor_modeless(true);
    app.handle_key(key('/'));
    type_str(&mut app, "m");
    let rows = rendered(&mut app, 100, 30);
    let row = |name: &str| {
        rows.iter()
            .find(|r| r.trim_start().starts_with(name))
            .unwrap_or_else(|| panic!("{name}: {rows:#?}"))
            .clone()
    };
    let model = row("model [id]");
    let mouse = row("mouse [on|off|toggle]");
    let mcp = row("mcp [restart|login]");
    let column = |r: &str, desc: &str| r.find(desc).unwrap_or_else(|| panic!("{r}"));
    let at = column(&model, "switch to provider:model");
    assert_eq!(column(&mouse, "toggle mouse capture"), at, "{rows:#?}");
    assert_eq!(column(&mcp, "list MCP servers"), at, "{rows:#?}");
}

#[test]
fn answers_to_commands_land_in_the_conversation() {
    let (mut app, _rx, _events) = app_with_events();
    let registry = builtin_registry();
    for (line, want) in [
        ("theme current", "theme: "),
        ("mcp", "Add one under [mcp.servers.<name>] in config.toml"),
        ("agents", "no agents in this session yet"),
        ("permission", "permission mode: default"),
    ] {
        let result = app.run_command_validated(line, &registry);
        assert!(matches!(result, CommandResult::Done(None)), "{line}");
        let text = last_block_text(&app.buffer);
        assert!(text.contains(want), "{line}: {text}");
    }
}
