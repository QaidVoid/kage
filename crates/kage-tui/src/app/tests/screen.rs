//! The status bar, footer hints, slots, the start card, the working row,
//! themes and toasts.

use super::*;

#[test]
fn render_into_paints_status_and_buffer() {
    let buffer = shared_buffer();
    if let Ok(mut buf) = buffer.lock() {
        buf.push_user("hello");
    }
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    app.render_into(&mut terminal).unwrap();
    let buf = terminal.backend().buffer();
    let mut found_user = false;
    for y in 0..buf.area.height {
        let mut row = String::new();
        for x in 0..buf.area.width {
            row.push_str(buf[(x, y)].symbol());
        }
        // The bubble pads the text with surrounding spaces; the
        // exact prefix glyph is renderer-internal.
        if row.contains(" hello ") {
            found_user = true;
        }
    }
    assert!(found_user);
}

#[test]
fn plugin_header_replaces_builtin_status_bar() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval("kage.ui.set_header(function() return 'PLUGINHEADER' end)")
        .unwrap();
    app.set_slots(rt.slots());
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    assert!(rows[0].contains("PLUGINHEADER"), "top row: {:?}", rows[0]);
    assert!(
        !rows[0].contains("kage"),
        "builtin label leaked: {:?}",
        rows[0]
    );
}

#[test]
fn plugin_footer_replaces_builtin_modeline() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_session_usage(crate::usage::shared_session_usage());
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval("kage.ui.set_footer(function() return 'PLUGINFOOTER' end)")
        .unwrap();
    app.set_slots(rt.slots());
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    let bottom = rows.last().unwrap();
    assert!(bottom.contains("PLUGINFOOTER"), "bottom row: {bottom:?}");
}

#[test]
fn default_slot_specs_paint_exactly_the_built_in_chrome() {
    let usage = crate::usage::SessionUsage {
        model: "fake:m".into(),
        input_tokens: 1200,
        context_window: 200_000,
        working: true,
        thinking_level: Some(kage_core::ThinkingLevel::High),
        ..crate::usage::SessionUsage::default()
    };
    let frame = |slots: Option<kage_plugin::Slots>| {
        let buffer = shared_buffer();
        lock(&buffer).push_user("hello");
        let (tx, _rx) = mpsc::channel();
        let mut app = app_with_defaults(buffer, tx);
        app.set_status_model(Arc::new(Mutex::new("fake:m".to_owned())));
        app.set_status_session_id("01abcdef".to_owned());
        let shared = crate::usage::shared_session_usage();
        *lock(&shared) = usage.clone();
        app.set_session_usage(shared);
        if let Some(slots) = slots {
            app.set_slots(slots);
        }
        render_app(&mut app).backend().buffer().clone()
    };
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    kage_plugin::load_all(None, &rt).unwrap();
    let (from_specs, built_in) = loop {
        let tick = crate::view::spinner_frame_index();
        let pair = (frame(Some(rt.slots())), frame(None));
        if tick == crate::view::spinner_frame_index() {
            break pair;
        }
    };
    assert_eq!(from_specs, built_in);
}

#[test]
fn slot_components_recompute_on_events_and_never_on_frames() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_session_usage(crate::usage::shared_session_usage());
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval(
        "n = 0
         kage.ui.set_slot('footer', { left = { {
             events = { 'turn_end' },
             render = function(ctx) n = n + 1 return 'N' .. n .. ' w' .. ctx.width end,
         } } })",
    )
    .unwrap();
    app.set_slots(rt.slots());
    let count = || rt.eval("return n").unwrap().as_i64().unwrap();
    let mut terminal = render_app(&mut app);
    assert_eq!(count(), 2, "the new width recomputes once");
    for _ in 0..5 {
        app.render_into(&mut terminal).unwrap();
    }
    assert_eq!(count(), 2);
    rt.dispatch_event("turn_end", &serde_json::json!({}))
        .unwrap();
    assert_eq!(count(), 3);
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    assert!(rows.last().unwrap().contains("N3 w60"), "{rows:?}");
}

#[test]
fn set_header_nil_restores_the_built_in_header() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    kage_plugin::load_all(None, &rt).unwrap();
    app.set_slots(rt.slots());
    rt.eval("kage.ui.set_header(function() return 'TAKEOVER' end)")
        .unwrap();
    assert!(snapshot_rows(&render_app(&mut app))[0].contains("TAKEOVER"));
    rt.eval("kage.ui.set_header(nil)").unwrap();
    let rows = snapshot_rows(&render_app(&mut app));
    assert!(rows.iter().all(|r| !r.contains("TAKEOVER")), "{rows:?}");
    lock(&rt.slots().ui_state()).session_title = Some("the title".to_owned());
    let rows = snapshot_rows(&render_app(&mut app));
    assert_eq!(rows[0], " the title", "{rows:?}");
}

#[test]
fn a_start_spec_paints_on_an_empty_buffer_only() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval(
        "kage.ui.set_slot('start', { lines = {
             { text = 'HELLO START' },
             { render = function() return 'from lua' end },
         } })",
    )
    .unwrap();
    app.set_slots(rt.slots());
    let rows = snapshot_rows(&render_app(&mut app));
    assert!(rows.iter().any(|r| r.contains("HELLO START")), "{rows:?}");
    assert!(rows.iter().any(|r| r.contains("from lua")), "{rows:?}");
    lock(&buffer).push_user("hi");
    let rows = snapshot_rows(&render_app(&mut app));
    assert!(!rows.iter().any(|r| r.contains("HELLO START")), "{rows:?}");
}

#[test]
fn the_footer_hint_shows_a_pending_key_sequence_first() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.handle_key(code(KeyCode::Esc));
    let rows = snapshot_rows(&render_app(&mut app));
    assert_eq!(
        rows.last().unwrap(),
        "  i to type \u{B7} ? for shortcuts \u{B7} : for commands"
    );
    app.handle_key(key('g'));
    let rows = snapshot_rows(&render_app(&mut app));
    assert_eq!(rows.last().unwrap(), "  g ...");
}

#[test]
fn the_footer_hint_follows_the_editor_state() {
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    app.set_editor_modeless(true);
    let usage = crate::usage::shared_session_usage();
    app.set_session_usage(usage.clone());
    assert_eq!(app.footer_hint(), "? for shortcuts \u{B7} / for commands");
    app.handle_key(key('x'));
    assert_eq!(
        app.footer_hint(),
        "enter to send \u{B7} shift+enter for a newline"
    );
    lock(&usage).working = true;
    assert_eq!(
        app.footer_hint(),
        "enter steers \u{B7} tab queues \u{B7} esc clears"
    );
    app.handle_key(code(KeyCode::Backspace));
    assert_eq!(app.footer_hint(), "tab to queue \u{B7} esc to interrupt");
    app.set_editor_modeless(false);
    assert_eq!(app.footer_hint(), "tab to queue \u{B7} ctrl+c to interrupt");
    app.handle_key(key('x'));
    assert_eq!(
        app.footer_hint(),
        "enter steers \u{B7} tab queues \u{B7} ctrl+c clears"
    );
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(
        app.footer_hint(),
        "ctrl+c to clear \u{B7} i to type \u{B7} ? for shortcuts"
    );
    lock(&usage).working = false;
    app.handle_key(ctrl('c'));
    app.handle_key(ctrl('c'));
    assert_eq!(app.footer_hint(), "ctrl+c again to quit");
}

#[test]
fn key_label_follows_a_remap_of_the_model_picker() {
    let (mut app, _rx, _buffer) = app_with_config("", &[]);
    assert_eq!(app.key_label("OpenModelPicker").as_deref(), Some("ctrl+p"));
    assert_eq!(
        app.key_label("CycleThinkingLevel").as_deref(),
        Some("shift+tab")
    );
    let (mut app, _rx, _buffer) = app_with_config(
        "kage.keymap.set('g', '<M-m>', kage.action.OpenModelPicker)",
        &[],
    );
    assert_eq!(app.key_label("OpenModelPicker").as_deref(), Some("alt+m"));
    assert_eq!(app.key_label("NoSuchAction"), None);
}

#[test]
fn the_card_hint_follows_a_remap_of_the_model_picker() {
    let (mut app, _rx, _buffer) = app_with_config(
        "kage.keymap.set('g', '<M-m>', kage.action.OpenModelPicker)",
        &[],
    );
    let rows = snapshot_rows(&render_app(&mut app));
    let model = rows.iter().find(|r| r.starts_with("   model ")).unwrap();
    assert!(model.ends_with("alt+m to change"), "{rows:#?}");
}

#[test]
fn a_user_start_spec_replaces_the_card_until_set_to_nil() {
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    kage_plugin::load_all(None, &rt).unwrap();
    app.set_slots(rt.slots());
    let has_card = |rows: &[String]| rows.iter().any(|r| r.starts_with("   permissions "));
    assert!(has_card(&snapshot_rows(&render_app(&mut app))));
    rt.eval("kage.ui.set_slot('start', { lines = { { text = 'MINE' } } })")
        .unwrap();
    let rows = snapshot_rows(&render_app(&mut app));
    assert!(!has_card(&rows), "{rows:#?}");
    assert!(rows.iter().any(|r| r == "   MINE"), "{rows:#?}");
    rt.eval("kage.ui.set_slot('start', nil)").unwrap();
    assert!(has_card(&snapshot_rows(&render_app(&mut app))));
}

#[test]
fn start_sessions_are_listed_on_session_changes_only() {
    let (mut app, _rx, events) = app_with_events();
    let listed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = Arc::clone(&listed);
    app.set_session_lister(Box::new(move |all| {
        assert!(!all);
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (0..5).map(|i| PickItem::simple(format!("s{i}"))).collect()
    }));
    let session = |title: &str| PickItem::simple(title).with_label(title);
    app.set_start_info(view::StartInfo {
        sessions: ["a", "b", "c", "d"].map(session).to_vec(),
        ..view::StartInfo::default()
    });
    let sessions = |app: &App| {
        app.start_info
            .as_ref()
            .unwrap()
            .sessions
            .iter()
            .map(|s| s.value.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(sessions(&app), ["a", "b", "c"]);
    let mut terminal = render_app(&mut app);
    app.render_into(&mut terminal).unwrap();
    assert_eq!(listed.load(std::sync::atomic::Ordering::SeqCst), 0);
    events
        .send(envelope(
            kage_core::SessionId::new(),
            1,
            kage_core::protocol::HostEvent::SessionChanged {
                path: "/tmp/s.jsonl".into(),
                title: None,
                messages: Vec::new(),
            },
        ))
        .unwrap();
    app.drain_engine_events();
    assert_eq!(listed.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(sessions(&app), ["s0", "s1", "s2"]);
}

#[test]
fn the_activity_row_shows_while_working_with_elapsed_seconds() {
    let buffer = shared_buffer();
    lock(&buffer).push_user("hello");
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    app.set_session_usage(crate::usage::shared_session_usage());
    let (events_tx, events_rx) = mpsc::channel();
    app.set_engine_events(events_rx);
    let session = kage_core::SessionId::new();
    let run_event = |seq, event: kage_core::protocol::HostEvent| {
        events_tx.send(envelope(session, seq, event)).unwrap();
    };
    let ended = || kage_core::protocol::HostEvent::RunEnded {
        outcome: kage_core::protocol::RunOutcome::Completed,
    };
    let idle = snapshot_rows(&render_app(&mut app));
    assert!(idle.iter().all(|r| !r.contains("Working")), "{idle:?}");
    run_event(1, kage_core::protocol::HostEvent::RunStarted);
    app.drain_engine_events();
    let rows = snapshot_rows(&render_app(&mut app));
    let row = rows
        .iter()
        .position(|r| r.starts_with("  Working (0s, ctrl+c to interrupt)"));
    let row = row.unwrap_or_else(|| panic!("{rows:?}"));
    assert!(rows[row + 1].starts_with('\u{2500}'), "{rows:?}");
    app.run_started = Instant::now().checked_sub(Duration::from_secs(14));
    lock(&buffer).push_tool_call("c1", "bash", serde_json::json!({ "command": "cargo test" }));
    lock(&buffer).set_tool_phase("c1", crate::view::tool_view::ToolPhase::Running);
    let rows = snapshot_rows(&render_app(&mut app));
    assert!(
        rows.iter()
            .any(|r| r == "  Running cargo test (14s, ctrl+c to interrupt)"),
        "{rows:?}"
    );
    run_event(2, ended());
    run_event(3, kage_core::protocol::HostEvent::RunStarted);
    app.drain_engine_events();
    assert!(app.run_started.unwrap().elapsed() < Duration::from_secs(1));
    run_event(4, ended());
    app.drain_engine_events();
    let rows = snapshot_rows(&render_app(&mut app));
    assert!(rows.iter().all(|r| !r.contains("to interrupt")), "{rows:?}");
}

#[test]
fn the_palette_follows_the_highlight_table_without_a_turn_boundary() {
    let _guard = crate::theme::theme_test_lock();
    let rt = kage_plugin::PluginRuntime::builder()
        .themes(Arc::new(crate::theme::Themes::new(None)))
        .build()
        .unwrap();
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_highlights(rt.highlights());
    assert_eq!(crate::theme::current().name, "default");

    rt.eval("kage.theme.set('tokyo-night')").unwrap();
    assert!(app.refresh_highlights());
    assert!(!app.refresh_highlights());
    let tokyo = crate::theme::Theme::tokyo_night();
    assert_eq!(crate::theme::current().name, "tokyo-night");
    assert_eq!(crate::theme::current().user_bg, tokyo.user_bg);

    rt.eval("kage.api.hl_set('KageUserBubble', { bg = '#010203' })")
        .unwrap();
    assert!(app.refresh_highlights());
    assert_eq!(
        crate::theme::current().user_bg,
        ratatui::style::Color::Rgb(1, 2, 3)
    );
    crate::theme::reset_current_for_tests();
}

#[test]
fn draw_snapshot_is_reused_while_the_buffer_is_unchanged() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);

    // First take: no resident snapshot yet, so this is a fresh clone.
    let (snap, version) = app.take_draw_snapshot();
    app.park_draw_snapshot(snap, version);

    // Unchanged version: the parked snapshot comes back at the same
    // version, ready to redraw verbatim.
    let (snap2, version2) = app.take_draw_snapshot();
    assert_eq!(version, version2, "nothing mutated between takes");
    app.park_draw_snapshot(snap2, version2);

    // A mutation bumps the version and forces a fresh clone.
    app.buffer.lock().unwrap().push_user("bump");
    let (_, version3) = app.take_draw_snapshot();
    assert_ne!(version2, version3, "mutation invalidates the snapshot");
}

#[test]
fn the_footer_hint_names_the_keys_of_the_open_overlay() {
    let mut app = defaults_app();
    app.set_editor_modeless(true);
    app.handle_key(key('/'));
    assert_eq!(
        app.footer_hint(),
        "tab to complete \u{B7} enter to run \u{B7} esc to close"
    );
    app.handle_key(code(KeyCode::Esc));
    assert!(app.slash_palette.is_none());
    app.handle_key(key('?'));
    assert_eq!(app.footer_hint(), "up/down to scroll \u{B7} esc to close");
    app.handle_key(code(KeyCode::Esc));
    assert!(app.help_overlay.is_none());
    app.handle_key(key('!'));
    assert_eq!(app.footer_hint(), "backspace to leave shell mode");
    type_str(&mut app, "ls");
    assert_eq!(app.footer_hint(), "enter to run the command");
}

#[test]
fn common_footer_hints_fit_at_80_columns_beside_the_session_facts() {
    let (mut app, _rx, events) = app_with_events();
    app.set_editor_modeless(true);
    app.set_model_choices(vec![PickItem::simple("fake:m").with_label("Fake")]);
    {
        let mut usage = lock(app.session_usage.as_ref().unwrap());
        usage.model = "fake:m".into();
        usage.input_tokens = 14_000;
        usage.current_context = 24_000;
        usage.context_window = 200_000;
        usage.permission_mode = Some(kage_core::permissions::PermissionAction::Ask);
        usage.working = true;
    }
    let facts = "  Fake \u{B7} ask mode \u{B7} 12% ctx \u{B7} 14k tok";
    let check = |app: &mut App| {
        let hint = app.footer_hint();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.render_into(&mut terminal).unwrap();
        let footer = snapshot_rows(&terminal).pop().unwrap();
        assert!(footer.starts_with(&format!("  {hint}")), "{footer:?}");
        assert!(footer.ends_with(facts), "{footer:?}");
    };
    check(&mut app);
    type_str(&mut app, "check the snapshot tests too");
    check(&mut app);
    feed(&mut app, &events, vec![permission_request("c1", 1)]);
    assert!(app.approval_panel.is_some());
    check(&mut app);
    app.answer_permission(PermissionDecision::AllowOnce);
    app.handle_key(code(KeyCode::Esc));
    let child = spawn_agent(&mut app, &events, "a1", "explore");
    send_to(
        &mut app,
        &events,
        child,
        vec![kage_core::protocol::HostEvent::RunStarted.into()],
    );
    app.focus_agent(child);
    check(&mut app);
    send_to(
        &mut app,
        &events,
        child,
        vec![run_ended(kage_core::protocol::RunOutcome::Completed)],
    );
    check(&mut app);
}

#[test]
fn at_60_columns_the_session_facts_give_way_to_the_hint() {
    let (mut app, _rx, events) = app_with_events();
    app.set_editor_modeless(true);
    app.set_model_choices(vec![PickItem::simple("fake:m").with_label("Fake")]);
    {
        let mut usage = lock(app.session_usage.as_ref().unwrap());
        usage.model = "fake:m".into();
        usage.input_tokens = 57;
        usage.context_window = 200_000;
        usage.permission_mode = Some(kage_core::permissions::PermissionAction::Ask);
    }
    let check = |app: &mut App| {
        let hint = app.footer_hint();
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        app.render_into(&mut terminal).unwrap();
        let footer = snapshot_rows(&terminal).pop().unwrap();
        assert!(footer.starts_with(&format!("  {hint}")), "{footer:?}");
        assert!(footer.ends_with("0% ctx \u{B7} 57 tok"), "{footer:?}");
    };
    check(&mut app);
    type_str(&mut app, "check the snapshot tests too");
    check(&mut app);
    feed(&mut app, &events, vec![permission_request("c1", 1)]);
    check(&mut app);
}

#[test]
fn the_working_row_cuts_the_command_before_its_time_and_key() {
    let buffer = shared_buffer();
    lock(&buffer).push_user("hello");
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    app.set_editor_modeless(true);
    app.run_started = Instant::now().checked_sub(Duration::from_secs(2));
    let command = "for i in 1 2 3 4 5 6 7 8 9 10; do echo $i; sleep 1; done; echo all done";
    lock(&buffer).push_tool_call("c1", "bash", serde_json::json!({ "command": command }));
    lock(&buffer).set_tool_phase("c1", crate::view::tool_view::ToolPhase::Running);
    let label = app.activity_label(&lock(&buffer), 80).unwrap();
    assert!(label.starts_with("Running for i in"), "{label}");
    assert!(label.ends_with("... (2s, esc to interrupt)"), "{label}");
    assert_eq!(label.len(), 78, "{label}");
}

#[test]
fn a_long_start_tip_wraps_instead_of_clipping() {
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    kage_plugin::load_all(None, &rt).unwrap();
    app.set_slots(rt.slots());
    rt.eval(
        "kage.ui.set_slot('start', { lines = {
             { text = 'Tip: one two three four five six seven eight nine' },
         } })",
    )
    .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    assert!(
        rows.iter()
            .any(|r| r == "   Tip: one two three four five six"),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|r| r == "   seven eight nine"), "{rows:#?}");
}

#[test]
fn a_theme_switch_repaints_existing_notice_rows() {
    let _guard = crate::theme::theme_test_lock();
    let rt = kage_plugin::PluginRuntime::builder()
        .themes(Arc::new(crate::theme::Themes::new(None)))
        .build()
        .unwrap();
    let buffer = shared_buffer();
    lock(&buffer).push_custom("kage:notify", "Interrupted", false);
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_highlights(rt.highlights());
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    let notice_fg = |terminal: &Terminal<TestBackend>| {
        let buf = terminal.backend().buffer();
        let row = (0..buf.area.height)
            .find(|&y| snapshot_rows(terminal)[usize::from(y)].contains("Interrupted"))
            .expect("notice row");
        let x = (0..buf.area.width)
            .find(|&x| buf[(x, row)].symbol() == "I")
            .expect("notice text");
        buf[(x, row)].fg
    };
    app.render_into(&mut terminal).unwrap();
    assert_eq!(notice_fg(&terminal), crate::theme::current().muted_fg);

    rt.eval("kage.theme.set('tokyo-night')").unwrap();
    assert!(app.refresh_highlights());
    app.render_into(&mut terminal).unwrap();
    let tokyo = crate::theme::current().muted_fg;
    crate::theme::reset_current_for_tests();
    assert_ne!(tokyo, crate::theme::Theme::default().muted_fg);
    assert_eq!(notice_fg(&terminal), tokyo);
}

#[test]
fn without_truecolor_no_frame_cell_keeps_a_24_bit_color() {
    use ratatui::style::Color;
    let buffer = shared_buffer();
    {
        let mut buf = lock(&buffer);
        buf.push_user("hello");
        buf.append_assistant_delta("a reply with `code`");
        buf.finish_streaming();
        buf.push_custom("kage:error", "boom", false);
    }
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    let colors = |terminal: &Terminal<TestBackend>| {
        let cells = &terminal.backend().buffer().content;
        cells
            .iter()
            .flat_map(|c| [c.fg, c.bg])
            .collect::<Vec<Color>>()
    };
    app.render_into(&mut terminal).unwrap();
    assert!(
        colors(&terminal)
            .iter()
            .any(|c| matches!(c, Color::Rgb(..)))
    );

    app.color_depth = crate::theme::ColorDepth::Ansi256;
    app.render_into(&mut terminal).unwrap();
    let colors256 = colors(&terminal);
    assert!(!colors256.iter().any(|c| matches!(c, Color::Rgb(..))));
    assert!(colors256.iter().any(|c| matches!(c, Color::Indexed(_))));

    app.color_depth = crate::theme::ColorDepth::Ansi16;
    app.render_into(&mut terminal).unwrap();
    assert!(
        !colors(&terminal)
            .iter()
            .any(|c| matches!(c, Color::Rgb(..) | Color::Indexed(_)))
    );
}

#[test]
fn a_toast_never_covers_the_conversation() {
    let buffer = shared_buffer();
    lock(&buffer).push_custom(
        "kage:error",
        "init.lua: lua error: syntax error: [string \"init.lua\"]:3: unexpected symbol near 'end' \
         while loading the user configuration",
        false,
    );
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let toasts = crate::toast::shared_toasts();
    app.set_toasts(toasts.clone());
    let render = |app: &mut App| {
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        app.render_into(&mut terminal).unwrap();
        snapshot_rows(&terminal)
    };
    let plain = render(&mut app);
    crate::toast::push_toast(&toasts, Toast::info("switched to fake:m"));
    let rows = render(&mut app);
    let notice = plain.iter().take_while(|r| !r.is_empty()).count();
    assert!(notice >= 2, "{plain:#?}");
    assert_eq!(rows[..notice], plain[..notice], "{rows:#?}");
    assert!(
        rows.iter().any(|r| r.contains("switched to fake:m")),
        "{rows:#?}"
    );
}
