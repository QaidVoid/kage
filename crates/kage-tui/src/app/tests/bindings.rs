//! Key handling: the keymap, config and Lua bindings, the editor modes
//! and interrupts.

use super::*;

/// Put the App in vim normal mode with `pane` focused.
fn normal(app: &mut App, pane: Pane) {
    app.handle_key(code(KeyCode::Esc));
    app.input.set_focused_pane(pane);
    assert_eq!(app.input.mode(), Mode::Normal);
}

#[test]
fn ctrl_q_exits_immediately() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let exit = app.handle_key(ctrl('q'));
    assert_eq!(exit, Some(AppExit::Quit));
}

#[test]
fn config_keybinding_runs_bound_command() {
    let (mut app, _rx, _) = app_with_config("", &[("ctrl+g", "quit")]);
    assert_eq!(app.handle_key(ctrl('g')), Some(AppExit::Quit));
}

#[test]
fn config_can_reclaim_ctrl_q_from_the_quit_hatch() {
    let (mut app, _rx, buffer) = app_with_config("", &[("ctrl+q", "clear")]);
    if let Ok(mut buf) = buffer.lock() {
        buf.push_custom("note", "x", false);
    }
    // ctrl+q no longer quits: it runs the bound `clear` instead.
    assert_eq!(app.handle_key(ctrl('q')), None);
    assert!(
        buffer.lock().unwrap().blocks().is_empty(),
        "clear ran via ctrl+q"
    );
}

#[test]
fn ctrl_q_yields_only_to_a_user_owned_mapping() {
    let mut km = default_keymap();
    let lhs = kage_core::keymap::parse_keys("<C-q>", "\\").unwrap();
    let mapping = |owner: &str| kage_core::keymap::Mapping {
        rhs: Rhs::Command("clear".into()),
        desc: None,
        group: None,
        owner: owner.to_owned(),
    };
    km.set(
        kage_core::keymap::Mode::Global,
        lhs.clone(),
        mapping("plugin"),
    );
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let shared = Arc::new(Mutex::new(km));
    app.set_keymap(Arc::clone(&shared));
    assert_eq!(app.handle_key(ctrl('q')), Some(AppExit::Quit));
    lock(&shared).set(kage_core::keymap::Mode::Global, lhs, mapping("init.lua"));
    assert_eq!(app.handle_key(ctrl('q')), None);
}

#[test]
fn init_lua_can_reclaim_ctrl_c_in_one_mode() {
    let (mut app, rx, _) = app_with_config("kage.keymap.set('i', '<C-c>', ':clear')", &[]);
    let usage = crate::usage::shared_session_usage();
    app.set_session_usage(usage.clone());
    app.handle_key(key('x'));
    app.handle_key(ctrl('c'));
    assert_eq!(
        app.handle_key(ctrl('c')),
        None,
        "the hatch never armed quit"
    );
    assert!(rx.try_recv().is_err(), "insert mode ran the mapping");
    assert_eq!(app.input.text(), "x", "the mapping kept the draft");
    lock(&usage).working = true;
    normal(&mut app, Pane::Input);
    app.input.clear_draft();
    app.handle_key(ctrl('c'));
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::Cancel { session: None }),
        "normal mode kept the hatch"
    );
}

#[test]
fn keybindings_command_lists_the_table_per_mode_with_owners() {
    let (mut app, _rx, buffer) = app_with_config(
        "kage.keymap.set('b', '<PageDown>', kage.action.scroll(20))",
        &[
            ("ctrl+t", "theme set tokyo-night"),
            ("ctrl+g", "action:BeginCommand"),
        ],
    );
    app.push_keybindings();
    let rendered = last_block_text(&buffer);
    for wanted in [
        "g: any editing state",
        "ctrl+t",
        ":theme set tokyo-night",
        "action:BeginCommand",
        "config.toml",
        "b: vim normal mode, conversation pane",
        "action:Scroll(20)",
        "init.lua",
        "action:OpenModelPicker",
        "defaults",
        "built in (editor grammar",
        "ctrl+q         quit",
    ] {
        assert!(rendered.contains(wanted), "missing {wanted}: {rendered}");
    }
    assert!(!rendered.contains("reserved"), "{rendered}");
}

#[test]
fn config_action_binding_fires_builtin_action() {
    let (mut app, _rx, _) = app_with_config("", &[("ctrl+g", "action:BeginCommand")]);
    assert_eq!(app.handle_key(ctrl('g')), None);
    assert!(
        app.cmdline.is_some(),
        "the bound action opened the command line"
    );
    // The binding consumed the key before builtin insert handling,
    // so the editor never saw the char.
    assert_eq!(app.input().text(), "");
}

#[test]
fn config_action_binding_wins_over_builtin_handler() {
    let (mut app, _rx, buffer) = app_with_config("", &[("ctrl+o", "action:BeginCommand")]);
    if let Ok(mut buf) = buffer.lock() {
        buf.append_thinking_delta("step one");
        buf.finish_streaming();
    }
    // ctrl+o in insert is the grammar's fold toggle; the binding must
    // take the action path instead.
    assert_eq!(app.handle_key(ctrl('o')), None);
    assert!(app.cmdline.is_some(), "the bound action ran");
    if let Ok(buf) = buffer.lock() {
        assert!(
            matches!(
                buf.blocks()[0],
                crate::buffer::Block::Thinking { folded: true, .. }
            ),
            "the builtin fold toggle did not run"
        );
    }
}

#[test]
fn a_bad_mapped_command_reports_inline() {
    let (mut app, _rx, buffer) = app_with_config("", &[("ctrl+g", "nosuchcommand")]);
    assert_eq!(app.handle_key(ctrl('g')), None);
    let text = format!("{:?}", buffer.lock().unwrap().blocks().last());
    assert!(text.contains("mapping `:nosuchcommand`"), "{text}");
}

/// An App with defaults and a usage snapshot whose working flag the
/// test flips.
fn app_with_usage() -> (
    App,
    mpsc::Receiver<RunRequest>,
    crate::usage::SharedSessionUsage,
) {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    let usage = crate::usage::shared_session_usage();
    app.set_session_usage(usage.clone());
    (app, rx, usage)
}

#[test]
fn ctrl_c_in_normal_interrupts_a_run() {
    let (mut app, rx, usage) = app_with_usage();
    lock(&usage).working = true;
    app.handle_key(code(KeyCode::Esc));
    app.handle_key(ctrl('c'));
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel { session: None }));
}

#[test]
fn ctrl_c_in_insert_clears_the_draft_instead_of_typing_c() {
    let (mut app, rx, usage) = app_with_usage();
    lock(&usage).working = true;
    app.handle_key(key('x'));
    app.handle_key(ctrl('c'));
    assert!(rx.try_recv().is_err(), "a draft is cleared, not the run");
    assert_eq!(app.input().text(), "");
    app.handle_key(code(KeyCode::Up));
    assert_eq!(app.input().text(), "x");
}

#[test]
fn esc_with_a_draft_clears_it_and_up_restores_it() {
    let (mut app, rx, usage) = app_with_usage();
    app.set_editor_modeless(true);
    lock(&usage).working = true;
    for c in "fix it".chars() {
        app.handle_key(key(c));
    }
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(app.input().text(), "");
    assert!(rx.try_recv().is_err(), "the run keeps going");
    assert_eq!(app.footer_hint(), "draft cleared, up restores it");
    app.handle_paste("x");
    assert_ne!(app.footer_hint(), "draft cleared, up restores it");
    app.handle_key(code(KeyCode::Esc));
    app.handle_key(code(KeyCode::Up));
    assert_eq!(app.input().text(), "x");
    app.handle_key(code(KeyCode::Up));
    assert_eq!(app.input().text(), "fix it");
    assert_ne!(app.footer_hint(), "draft cleared, up restores it");
}

#[test]
fn esc_on_an_empty_draft_while_working_interrupts() {
    let (mut app, rx, usage) = app_with_usage();
    app.set_editor_modeless(true);
    lock(&usage).working = true;
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel { session: None }));
}

#[test]
fn idle_esc_on_an_empty_draft_sends_nothing() {
    let (mut app, rx, _usage) = app_with_usage();
    app.set_editor_modeless(true);
    assert_eq!(app.handle_key(code(KeyCode::Esc)), None);
    assert_eq!(app.handle_key(code(KeyCode::Esc)), None);
    assert!(rx.try_recv().is_err());
    assert_eq!(app.footer_hint(), "? for shortcuts \u{B7} / for commands");
}

#[test]
fn ctrl_c_twice_within_the_window_quits() {
    let (mut app, rx, _usage) = app_with_usage();
    assert_eq!(app.handle_key(ctrl('c')), None, "one press only arms");
    assert_eq!(app.footer_hint(), "ctrl+c again to quit");
    assert_eq!(app.handle_key(ctrl('c')), Some(AppExit::Quit));
    assert!(rx.try_recv().is_err());
}

#[test]
fn an_armed_quit_lapses_or_yields_to_another_key() {
    let (mut app, _rx, _usage) = app_with_usage();
    app.handle_key(ctrl('c'));
    app.escalation = Some((keys::Escalation::QuitArmed, Instant::now()));
    assert_eq!(app.handle_key(ctrl('c')), None, "the window passed");
    app.handle_key(code(KeyCode::Left));
    assert_eq!(app.handle_key(ctrl('c')), None, "another key disarmed it");
    assert_eq!(app.handle_key(ctrl('c')), Some(AppExit::Quit));
}

#[test]
fn tab_queues_only_while_working() {
    let (mut app, rx, usage) = app_with_usage();
    app.handle_key(key('a'));
    app.handle_key(code(KeyCode::Tab));
    assert!(rx.try_recv().is_err(), "idle tab sends nothing");
    assert_eq!(app.input().text(), "a");
    lock(&usage).working = true;
    app.handle_key(code(KeyCode::Tab));
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::Submit {
            text: "a".into(),
            images: Vec::new(),
            queue: true,
            session: None,
        })
    );
    assert_eq!(app.input().text(), "");
    assert_eq!(app.input().history(), ["a"]);
}

#[test]
fn terminal_input_hook_consumes_matching_key() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.on_terminal_input(function(ev)
                return ev.code == 'char' and ev.char == 'x'
            end)
            ",
    )
    .unwrap();
    app.set_plugin_terminal_hooks(rt.shared_terminal_hooks());
    app.handle_key(key('x'));
    assert_eq!(app.input().text(), "", "x consumed by hook");
    app.handle_key(key('y'));
    assert_eq!(app.input().text(), "y", "y passes through");
}

#[test]
fn terminal_input_hook_cannot_block_ctrl_q() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval("kage.on_terminal_input(function() return true end)")
        .unwrap();
    app.set_plugin_terminal_hooks(rt.shared_terminal_hooks());
    assert_eq!(app.handle_key(ctrl('q')), Some(AppExit::Quit));
}

#[test]
fn terminal_input_hook_cannot_swallow_approval_panel_keys() {
    let (mut app, _rx, events) = app_with_events();
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval("kage.on_terminal_input(function() return true end)")
        .unwrap();
    app.set_plugin_terminal_hooks(rt.shared_terminal_hooks());
    feed(
        &mut app,
        &events,
        vec![shell_start("c1"), permission_request("c1", 1)],
    );
    assert!(app.approval_panel.is_some());
    std::thread::sleep(crate::overlay::approval::TYPE_AHEAD_GUARD);
    app.handle_key(key('4'));
    assert!(app.approval_panel.is_none(), "4 reached the panel");
}

#[test]
fn terminal_input_off_stops_consuming() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval(
        r"
            _G.off = kage.on_terminal_input(function() return true end)
            ",
    )
    .unwrap();
    app.set_plugin_terminal_hooks(rt.shared_terminal_hooks());
    app.handle_key(key('a'));
    assert_eq!(app.input().text(), "", "hook swallows everything");
    rt.eval("_G.off()").unwrap();
    app.handle_key(key('b'));
    assert_eq!(app.input().text(), "b", "off restores normal input");
}

#[test]
fn set_editor_modeless_flips_the_input_editor() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    // Default is vim-modal: Esc enters Normal.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.input().mode(), Mode::Normal);

    app.set_editor_modeless(true);
    assert!(app.input().is_modeless());
    // In modeless, Esc cancels the turn instead of switching modes.
    app.input.force_normal(); // prove set_modeless re-pins insert too
    app.set_editor_modeless(true);
    assert_eq!(app.input().mode(), Mode::Insert);
}

#[test]
fn lua_option_sets_apply_on_the_next_tick() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let store = kage_plugin::SharedOptions::default();
    let rt = Arc::new(
        kage_plugin::PluginRuntime::builder()
            .options(Arc::clone(&store))
            .build()
            .unwrap(),
    );
    let setter = Arc::clone(&rt);
    app.set_options(
        store,
        Some(Box::new(move |name, value| {
            setter.set_option(name, value).map_err(|e| e.to_string())
        })),
    );
    assert!(app.input().is_modeless(), "the default editor applies");

    rt.eval(
        "seen = {}
         kage.on('option_set', function(d) seen[#seen + 1] = d.name .. ':' .. d.source end)
         kage.opt.editor = 'vim'
         kage.opt.mouse = false",
    )
    .unwrap();
    assert!(app.input().is_modeless(), "nothing applies before the tick");
    assert!(app.apply_option_changes());
    assert!(!app.input().is_modeless());
    assert_eq!(app.pending_mouse_capture.take(), Some(false));

    app.run_mouse_command("toggle");
    let seen = rt.eval("return table.concat(seen, ' ')").unwrap();
    assert_eq!(
        seen.as_string().unwrap().to_string_lossy(),
        "editor:lua mouse:lua mouse:runtime"
    );
    assert!(app.apply_option_changes());
    assert_eq!(app.pending_mouse_capture.take(), Some(true));
}

#[test]
fn modeless_question_mark_on_empty_prompt_opens_help() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_editor_modeless(true);
    assert!(app.dispatch_key(key('?')).is_none());
    assert!(app.help_overlay.is_some(), "`?` opens the keys reference");
}

#[test]
fn modeless_question_mark_with_text_types_literally() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_editor_modeless(true);
    app.dispatch_key(key('h'));
    app.dispatch_key(key('?'));
    assert!(app.help_overlay.is_none(), "`?` stays literal with text");
    assert!(app.input().text().ends_with('?'));
}

#[test]
fn pasted_text_lands_in_input_area_with_newline_preserved() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.handle_key(key('i'));
    app.input.paste("first\nsecond");
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    assert!(rows.iter().any(|r| r.contains("first")));
    assert!(rows.iter().any(|r| r.contains("second")));
}

#[test]
fn history_walk_replaces_input_text() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_history(vec!["older".into(), "newer".into()]);
    // Default mode is Insert; no need to press 'i'.
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input().text(), "newer");
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input().text(), "older");
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.input().text(), "newer");
}

#[test]
fn fold_all_then_unfold_all_toggles_folds() {
    let buffer = shared_buffer();
    if let Ok(mut buf) = buffer.lock() {
        buf.append_thinking_delta("step one");
        buf.finish_streaming();
    }
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    // Default mode is Insert; switch to Normal for zM/zR.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    // zM folds all
    app.handle_key(key('z'));
    app.handle_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE));
    if let Ok(buf) = buffer.lock() {
        assert!(matches!(
            buf.blocks()[0],
            crate::buffer::Block::Thinking { folded: true, .. }
        ));
    }
    // zR opens all
    app.handle_key(key('z'));
    app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
    if let Ok(buf) = buffer.lock() {
        assert!(matches!(
            buf.blocks()[0],
            crate::buffer::Block::Thinking { folded: false, .. }
        ));
    }
}

#[test]
fn lua_mapping_dispatches_invoke_request() {
    let (mut app, rx, _) = app_with_config(
        "kage.register_keybinding('ctrl+t', function() end)
         kage.keymap.set('i', '<C-l>', function() end)",
        &[],
    );
    app.handle_key(ctrl('t'));
    let Ok(RunRequest::InvokeKeymap { id: first }) = rx.try_recv() else {
        panic!("expected an InvokeKeymap request");
    };
    app.handle_key(ctrl('l'));
    let Ok(RunRequest::InvokeKeymap { id: second }) = rx.try_recv() else {
        panic!("expected an InvokeKeymap request");
    };
    assert_ne!(first, second);
    app.handle_key(ctrl('h'));
    assert!(rx.try_recv().is_err(), "an unmapped chord sends nothing");
}

#[test]
fn open_overlay_suppresses_mappings() {
    let (mut app, rx, _) = app_with_config("kage.keymap.set('g', '<C-t>', function() end)", &[]);
    app.picker = Some(OverlayPicker::new("busy", vec![PickItem::simple("x")]));
    app.picker_kind = Some(PickerKind::Model);

    app.handle_key(ctrl('t'));

    assert!(rx.try_recv().is_err(), "picker should swallow the chord");
}

fn ctrl_code(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

// Keys `_defaults.lua` maps, resolved through an App holding the
// embedded defaults.

#[test]
fn jk_arrows_and_capital_g_scroll_in_the_buffer_pane() {
    let mut app = defaults_app();
    normal(&mut app, Pane::Buffer);
    for (k, action) in [
        (key('j'), InputAction::Scroll(1)),
        (code(KeyCode::Down), InputAction::Scroll(1)),
        (key('k'), InputAction::Scroll(-1)),
        (code(KeyCode::Up), InputAction::Scroll(-1)),
        (key('G'), InputAction::ScrollToBottom),
        (key('y'), InputAction::Yank),
        (key('Y'), InputAction::YankFocusedBlock),
        (key('v'), InputAction::EnterVisual),
    ] {
        assert_eq!(routes(&mut app, k), input(action), "{k:?}");
    }
}

#[test]
fn jk_and_capital_g_stay_grammar_in_the_input_pane() {
    let mut app = defaults_app();
    app.input.paste("first\nsecond");
    normal(&mut app, Pane::Input);
    assert!(routes(&mut app, key('k')).is_empty());
    assert!(app.input().cursor() < 6);
    assert!(routes(&mut app, key('G')).is_empty());
    assert_eq!(app.input().cursor(), 12);
}

#[test]
fn gg_scrolls_to_top_in_the_buffer_pane_and_moves_the_cursor_in_the_input_pane() {
    let mut app = defaults_app();
    normal(&mut app, Pane::Buffer);
    assert!(routes(&mut app, key('g')).is_empty());
    assert!(app.keymap_deadline().is_some(), "g waits for more");
    assert_eq!(routes(&mut app, key('g')), input(InputAction::ScrollToTop));
    assert!(app.keymap_deadline().is_none());

    let mut app = defaults_app();
    app.input.paste("hello");
    normal(&mut app, Pane::Input);
    assert_eq!(app.input().cursor(), 5);
    assert!(routes(&mut app, key('g')).is_empty());
    assert!(routes(&mut app, key('g')).is_empty());
    assert_eq!(app.input().cursor(), 0);
}

#[test]
fn a_lone_g_times_out_into_the_grammar() {
    let mut app = defaults_app();
    app.input.paste("hello");
    normal(&mut app, Pane::Input);
    let start = Instant::now();
    app.route_editor_key(key('g'), start);
    assert!(
        app.tick_keymap(start + Duration::from_millis(999))
            .is_empty()
    );
    assert!(!app.input.has_pending());
    app.tick_keymap(start + Duration::from_secs(1));
    assert!(app.input.has_pending(), "g replayed into the grammar");
    assert!(routes(&mut app, key('g')).is_empty());
    assert_eq!(app.input().cursor(), 0);
}

#[test]
fn z_then_x_does_nothing() {
    let mut app = defaults_app();
    app.input.paste("hello");
    normal(&mut app, Pane::Input);
    routes(&mut app, key('0'));
    assert!(routes(&mut app, key('z')).is_empty());
    assert!(routes(&mut app, key('x')).is_empty());
    assert_eq!(app.input().text(), "hello");
    assert!(routes(&mut app, key('x')).is_empty());
    assert_eq!(app.input().text(), "ello", "a later x is a delete again");
}

#[test]
fn z_prefix_fold_keys_in_normal_mode() {
    for pane in [Pane::Input, Pane::Buffer] {
        let mut app = defaults_app();
        normal(&mut app, pane);
        for (suffix, action) in [
            ('o', InputAction::ToggleFold),
            ('c', InputAction::ToggleFold),
            ('R', InputAction::UnfoldAll),
            ('M', InputAction::FoldAll),
        ] {
            assert!(routes(&mut app, key('z')).is_empty());
            assert_eq!(routes(&mut app, key(suffix)), input(action));
        }
    }
}

#[test]
fn normal_mode_command_keys_in_both_panes() {
    for pane in [Pane::Input, Pane::Buffer] {
        let mut app = defaults_app();
        normal(&mut app, pane);
        for (k, action) in [
            (key(':'), InputAction::BeginCommand),
            (key('/'), InputAction::BeginSearch),
            (key('?'), InputAction::OpenHelp),
            (key('['), InputAction::FocusPrev),
            (key(']'), InputAction::FocusNext),
            (key('n'), InputAction::SearchNext),
            (key('N'), InputAction::SearchPrev),
            (ctrl('o'), InputAction::ToggleFold),
            (ctrl('w'), InputAction::CyclePane),
            (code(KeyCode::PageUp), InputAction::Scroll(-10)),
            (code(KeyCode::PageDown), InputAction::Scroll(10)),
        ] {
            assert_eq!(routes(&mut app, k), input(action), "{pane:?} {k:?}");
        }
        assert!(routes(&mut app, key('g')).is_empty());
        assert_eq!(routes(&mut app, key('w')), input(InputAction::CyclePane));
    }
}

#[test]
fn shift_tab_cycles_thinking_in_every_editing_state() {
    let backtab = code(KeyCode::BackTab);
    let shift_tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
    let mut app = defaults_app();
    assert_eq!(
        routes(&mut app, backtab),
        input(InputAction::CycleThinkingLevel)
    );
    normal(&mut app, Pane::Input);
    assert_eq!(
        routes(&mut app, shift_tab),
        input(InputAction::CycleThinkingLevel)
    );
    app.input.set_modeless(true);
    assert_eq!(
        routes(&mut app, backtab),
        input(InputAction::CycleThinkingLevel)
    );
    let tab = routes(&mut app, code(KeyCode::Tab));
    assert!(!tab.contains(&Routed::Input(InputAction::CycleThinkingLevel)));
}

#[test]
fn ctrl_s_opens_the_session_picker_in_every_editing_state() {
    let mut app = defaults_app();
    let picker = input(InputAction::OpenSessionPicker);
    assert_eq!(routes(&mut app, ctrl('s')), picker);
    normal(&mut app, Pane::Input);
    assert_eq!(routes(&mut app, ctrl('s')), picker);
    app.input.set_focused_pane(Pane::Buffer);
    assert_eq!(routes(&mut app, ctrl('s')), picker);
    app.input.set_modeless(true);
    assert_eq!(routes(&mut app, ctrl('s')), picker);
}

#[test]
fn deleting_the_default_ctrl_s_lets_it_reach_the_grammar() {
    let (mut app, _rx, _) = app_with_config("kage.keymap.del('g', '<C-s>')", &[]);
    assert!(routes(&mut app, ctrl('s')).is_empty());
    assert_eq!(app.handle_key(ctrl('s')), None);
    assert!(app.picker.is_none());
    assert_eq!(app.input().text(), "");
    assert_eq!(
        routes(&mut app, ctrl('p')),
        input(InputAction::OpenModelPicker)
    );
}

#[test]
fn a_nop_mapping_swallows_a_grammar_key() {
    let (mut app, _rx, _) = app_with_config("kage.keymap.set('i', 'x', '<Nop>')", &[]);
    assert!(routes(&mut app, key('x')).is_empty());
    assert!(routes(&mut app, key('y')).is_empty());
    assert_eq!(app.input().text(), "y");
}

#[test]
fn insert_and_modeless_scroll_and_focus_keys() {
    for modeless in [false, true] {
        let mut app = defaults_app();
        app.input.set_modeless(modeless);
        for (k, action) in [
            (ctrl_code(KeyCode::Down), InputAction::Scroll(1)),
            (ctrl_code(KeyCode::Up), InputAction::Scroll(-1)),
            (ctrl_code(KeyCode::Home), InputAction::ScrollToTop),
            (ctrl_code(KeyCode::End), InputAction::ScrollToBottom),
            (code(KeyCode::PageUp), InputAction::Scroll(-10)),
            (code(KeyCode::PageDown), InputAction::Scroll(10)),
            (ctrl('p'), InputAction::OpenModelPicker),
            (ctrl('n'), InputAction::FocusNext),
            (alt('p'), InputAction::FocusPrev),
            (alt('n'), InputAction::FocusNext),
            (code(KeyCode::F(3)), InputAction::OpenJumpPicker),
            (ctrl('v'), InputAction::AttachClipboardImage),
        ] {
            assert_eq!(routes(&mut app, k), input(action), "{modeless} {k:?}");
        }
        assert_eq!(app.input().text(), "", "no mapped key typed text");
    }
}

#[test]
fn a_pending_leader_fires_its_exact_match_after_timeoutlen() {
    let (mut app, _rx, _) = app_with_config(
        "kage.keymap.set('n', '<leader>', kage.action.OpenHelp)
         kage.keymap.set('n', '<leader>m', kage.action.OpenModelPicker)",
        &[],
    );
    normal(&mut app, Pane::Input);
    let start = Instant::now();
    assert!(app.route_editor_key(key('\\'), start).is_empty());
    assert_eq!(app.keymap_deadline(), Some(start + Duration::from_secs(1)));
    assert!(
        app.tick_keymap(start + Duration::from_millis(999))
            .is_empty()
    );
    assert_eq!(
        app.tick_keymap(start + Duration::from_secs(1)),
        input(InputAction::OpenHelp)
    );

    assert!(app.route_editor_key(key('\\'), start).is_empty());
    assert_eq!(
        app.route_editor_key(key('m'), start),
        input(InputAction::OpenModelPicker)
    );

    app.apply_option("timeoutlen", &OptionValue::Int(200), false);
    app.route_editor_key(key('\\'), start);
    assert_eq!(
        app.keymap_deadline(),
        Some(start + Duration::from_millis(200))
    );
}

#[test]
fn a_pending_sequence_is_dropped_when_a_modal_opens() {
    let (mut app, _rx, _) = app_with_config("kage.keymap.set('n', '<leader>', ':quit')", &[]);
    normal(&mut app, Pane::Input);
    let start = Instant::now();
    app.route_editor_key(key('\\'), start);
    app.open_help();
    assert!(app.tick_keymap(start + Duration::from_secs(2)).is_empty());
    assert!(app.keymap_deadline().is_none());
}

#[test]
fn completion_keeps_ctrl_n_while_open() {
    let (mut app, _rx, buffer) = app_with_config("kage.keymap.set('i', '<C-n>', ':clear')", &[]);
    buffer.lock().unwrap().push_custom("note", "x", false);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval(
        "kage.add_autocomplete_provider({
           name = 'two',
           complete = function(prefix)
             if prefix == '' then return {} end
             return { { value = prefix .. '1' }, { value = prefix .. '2' } }
           end,
         })",
    )
    .unwrap();
    app.set_plugin_autocomplete(rt.registered_autocomplete_providers());
    app.handle_key(key('f'));
    assert_eq!(
        app.input_completion
            .as_ref()
            .map(InputCompletion::selected_index),
        Some(0)
    );
    app.handle_key(ctrl('n'));
    assert_eq!(
        app.input_completion
            .as_ref()
            .map(InputCompletion::selected_index),
        Some(1),
        "the popup took ctrl+n"
    );
    assert_eq!(
        buffer.lock().unwrap().blocks().len(),
        1,
        "the mapping did not run"
    );
    app.handle_key(code(KeyCode::Esc));
    assert!(app.input_completion.is_none());
    app.handle_key(ctrl('n'));
    assert!(
        buffer.lock().unwrap().blocks().is_empty(),
        "closed, the mapping runs"
    );
}

#[test]
fn help_rows_come_from_the_live_keymap() {
    fn help_descs(app: &App) -> std::collections::BTreeSet<String> {
        app.help_overlay
            .as_ref()
            .expect("help open")
            .mapped_rows()
            .into_iter()
            .map(|(_, desc)| desc.to_owned())
            .collect()
    }
    let defaults = default_keymap();
    let descs = |modes: &[kage_core::keymap::Mode]| -> std::collections::BTreeSet<String> {
        defaults
            .entries()
            .iter()
            .filter(|e| modes.is_empty() || modes.contains(&e.mode))
            .filter_map(|e| e.mapping.desc.clone())
            .collect()
    };

    let mut app = defaults_app();
    app.open_help();
    assert_eq!(help_descs(&app), descs(&[]));

    let mut app = defaults_app();
    app.input.set_modeless(true);
    app.open_help();
    assert_eq!(help_descs(&app), descs(EditState::Insert.modes()));

    let (mut app, _rx, _) = app_with_config(
        "kage.keymap.set('g', '<C-t>', ':theme set dark', { desc = 'dark theme' })
         kage.keymap.del('g', '<C-s>')",
        &[],
    );
    app.open_help();
    let rows = app.help_overlay.as_ref().unwrap().mapped_rows();
    assert!(rows.contains(&("ctrl+t", "dark theme")), "{rows:?}");
    assert!(!rows.iter().any(|(lhs, _)| *lhs == "ctrl+s"), "{rows:?}");
}
