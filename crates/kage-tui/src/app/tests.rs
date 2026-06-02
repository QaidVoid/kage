//! Integration tests for the App event and render loop.

use std::sync::mpsc;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::events::shared_buffer;

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

#[test]
fn partial_selection_copies_only_highlighted_cells_not_whole_block() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    {
        let mut buf = buffer.lock().unwrap();
        buf.begin_thinking();
        buf.append_thinking_delta("alpha beta gamma delta epsilon");
    }
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();

    // Render once with a full-buffer selection so capture_and_overlay
    // populates captured_rows. The selection coords cover the entire
    // visible area.
    app.screen_selection = Some(((0, 0), (999, 59)));
    app.render_into(&mut terminal).unwrap();

    // Locate "gamma" in the captured grid by column (cell) index,
    // not byte offset, so a multi-byte chrome glyph on the row
    // cannot skew the mapping.
    let needle: Vec<char> = "gamma".chars().collect();
    let mut sel = None;
    for (&vrow, cells) in &app.captured_rows {
        let chars: Vec<char> = cells.iter().map(|c| c.ch).collect();
        if let Some(at) = chars
            .windows(needle.len())
            .position(|w| w == needle.as_slice())
        {
            let lo = u16::try_from(at).unwrap();
            let hi = u16::try_from(at + needle.len() - 1).unwrap();
            sel = Some(((vrow, lo), (vrow, hi)));
            break;
        }
    }
    let (anchor, cursor) = sel.expect("thinking text was never painted");
    app.screen_selection = Some((anchor, cursor));
    app.render_into(&mut terminal).unwrap();

    assert_eq!(app.extract_selection_text(), "gamma");
}

#[test]
fn right_click_opens_context_menu_on_the_block_then_esc_and_no_block_close_it() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    {
        let mut buf = buffer.lock().unwrap();
        buf.push_user("hello there");
    }
    let backend = TestBackend::new(40, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    app.render_into(&mut terminal).unwrap();

    // A screen row and the buffer-pane geometry that block 0
    // painted into.
    let (row, area_x, below) = {
        let buf = buffer.lock().unwrap();
        let y0 = buf.last_area_y();
        let y1 = y0.saturating_add(buf.last_area_height());
        let row = (y0..y1)
            .find(|&y| buf.block_at_screen_row(y) == Some(0))
            .expect("block 0 painted");
        (row, buf.last_area_x(), y1)
    };

    // Right press over the block opens a menu targeting it.
    app.open_context_menu(area_x + 1, row);
    assert_eq!(
        app.context_menu.as_ref().map(ContextMenu::block_idx),
        Some(0)
    );

    // Esc closes it.
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.dispatch_context_menu_key(esc), None);
    assert!(app.context_menu.is_none());

    // Reopened, a right press below the buffer pane (over no
    // block) dismisses instead of opening.
    app.open_context_menu(area_x + 1, row);
    assert!(app.context_menu.is_some());
    app.open_context_menu(area_x + 1, below);
    assert!(app.context_menu.is_none());
}

#[test]
fn ctrl_q_exits_immediately() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let exit = app.handle_key(ctrl('q'));
    assert_eq!(exit, Some(AppExit::Quit));
}

#[test]
fn config_keybinding_runs_bound_command() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let errs = app.set_config_keybindings(vec![("ctrl+g".into(), "quit".into())]);
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(app.handle_key(ctrl('g')), Some(AppExit::Quit));
}

#[test]
fn config_can_reclaim_ctrl_q_from_the_quit_hatch() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    if let Ok(mut buf) = buffer.lock() {
        buf.push_custom("note", "x", false);
    }
    let _ = app.set_config_keybindings(vec![("ctrl+q".into(), "clear".into())]);
    // ctrl+q no longer quits: it runs the bound `clear` instead.
    assert_eq!(app.handle_key(ctrl('q')), None);
    assert!(
        buffer.lock().unwrap().blocks().is_empty(),
        "clear ran via ctrl+q"
    );
}

#[test]
fn set_config_keybindings_reports_unparseable_chord() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let errs = app.set_config_keybindings(vec![
        ("totally bogus chord".into(), "quit".into()),
        ("ctrl+g".into(), "help".into()),
    ]);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].contains("totally bogus chord"), "{}", errs[0]);
    assert_eq!(app.config_keybindings.len(), 1, "good binding kept");
}

#[test]
fn keybindings_command_lists_config_and_reserved() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    let _ = app.set_config_keybindings(vec![("ctrl+t".into(), "theme set tokyo-night".into())]);
    app.push_keybindings();
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last() {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("ctrl+t"), "{rendered}");
    assert!(rendered.contains("theme set tokyo-night"), "{rendered}");
    assert!(rendered.contains("reserved"), "{rendered}");
}

#[test]
fn plugin_command_alias_resolves_to_canonical_invoke() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.set_plugin_commands(vec![PluginCommand {
        name: "git-status".into(),
        aliases: vec!["gst".into(), "gs".into()],
        is_override: false,
        description: "show git status".into(),
        args: Vec::new(),
    }]);
    let registry = cmdline_registry(&app.plugin_command_specs);
    let res = app.run_command_validated("gst --short", &registry);
    assert!(matches!(res, CommandResult::Done(None)));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::InvokePluginCommand { name, args } => {
            assert_eq!(name, "git-status", "alias mapped to canonical");
            assert_eq!(args, "--short");
        }
        other => panic!("expected InvokePluginCommand, got {other:?}"),
    }
}

#[test]
fn plugin_command_alias_shadowing_builtin_is_dropped() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.set_plugin_commands(vec![PluginCommand {
        name: "mycmd".into(),
        aliases: vec!["help".into()],
        is_override: false,
        description: "tries to shadow :help via alias".into(),
        args: Vec::new(),
    }]);
    assert!(
        app.plugin_commands.is_empty(),
        "a command whose alias collides with a builtin is rejected whole"
    );
}

#[test]
fn override_command_shadows_builtin_and_dispatches_first() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.set_plugin_commands(vec![PluginCommand {
        name: "help".into(),
        aliases: Vec::new(),
        is_override: true,
        description: "my help".into(),
        args: Vec::new(),
    }]);
    assert_eq!(
        app.plugin_commands.len(),
        1,
        "override kept despite builtin"
    );
    let registry = cmdline_registry(&app.plugin_command_specs);
    let res = app.run_command_validated("help", &registry);
    assert!(matches!(res, CommandResult::Done(None)));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::InvokePluginCommand { name, .. } => {
            assert_eq!(name, "help", "override won over builtin :help");
        }
        other => panic!("expected plugin invoke, got {other:?}"),
    }
}

#[test]
fn events_command_lists_known_hooks_by_kind() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
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
fn submitting_a_prompt_pushes_user_block_and_request() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    // Default mode is Insert; type "hi" and press Enter.
    app.handle_key(key('h'));
    app.handle_key(key('i'));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let req = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    assert_eq!(
        req,
        RunRequest::Submit {
            text: "hi".into(),
            images: Vec::new()
        }
    );
    let buf = buffer.lock().unwrap();
    assert!(matches!(
        buf.blocks().last(),
        Some(crate::buffer::Block::User { text }) if text == "hi"
    ));
    assert_eq!(app.input().mode(), Mode::Insert);
}

#[test]
fn ctrl_c_in_normal_emits_cancel_request() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    // Switch to Normal first; default is Insert.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(ctrl('c'));
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel));
}

#[test]
fn ctrl_c_flips_registered_cancel_flag_synchronously() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    // Switch to Normal first; default is Insert.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let flag = CancelFlag::new();
    app.set_cancel_flag(flag.clone());
    assert!(!flag.is_cancelled());
    app.handle_key(ctrl('c'));
    assert!(
        flag.is_cancelled(),
        "Ctrl-C should flip the cancel flag on the foreground thread"
    );
}

#[test]
fn cancel_command_flips_registered_cancel_flag_synchronously() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let flag = CancelFlag::new();
    app.set_cancel_flag(flag.clone());
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();
    let result = app.run_command_validated("cancel", &registry);
    assert!(
        matches!(result, CommandResult::Done(None)),
        "expected Done(None), got {result:?}"
    );
    assert!(
        flag.is_cancelled(),
        ":cancel should flip the cancel flag on the foreground thread"
    );
}

#[test]
fn render_into_paints_status_and_buffer() {
    let buffer = shared_buffer();
    if let Ok(mut buf) = buffer.lock() {
        buf.push_user("hello");
    }
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
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
    let mut app = App::new(buffer, tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval("kage.ui.set_header(function() return 'PLUGINHEADER' end)")
        .unwrap();
    app.set_plugin_chrome(rt.shared_header(), rt.shared_footer());
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
    let mut app = App::new(buffer, tx);
    app.set_session_usage(crate::usage::shared_session_usage());
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval("kage.ui.set_footer(function() return 'PLUGINFOOTER' end)")
        .unwrap();
    app.set_plugin_chrome(rt.shared_header(), rt.shared_footer());
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    let bottom = rows.last().unwrap();
    assert!(bottom.contains("PLUGINFOOTER"), "bottom row: {bottom:?}");
}

#[test]
fn autocomplete_popup_opens_and_tab_accepts() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.add_autocomplete_provider({
                name = 'demo',
                complete = function(prefix, _ctx)
                    if prefix == '' or prefix:sub(-3) == 'bar' then return {} end
                    return { { value = prefix .. 'bar' } }
                end,
            })
            ",
    )
    .unwrap();
    app.set_plugin_autocomplete(rt.registered_autocomplete_providers());
    app.handle_key(key('f'));
    app.handle_key(key('o'));
    assert!(app.input_completion.is_some(), "popup should open");
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.input().text(), "fobar");
    assert!(app.input_completion.is_none(), "popup closes after accept");
}

#[test]
fn autocomplete_respects_explicit_range() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.add_autocomplete_provider({
                name = 'at',
                complete = function(prefix, ctx)
                    if prefix:sub(1, 1) ~= '@' then return {} end
                    return { { value = '@README.md', range = { 0, ctx.cursor } } }
                end,
            })
            ",
    )
    .unwrap();
    app.set_plugin_autocomplete(rt.registered_autocomplete_providers());
    app.handle_key(key('@'));
    assert!(app.input_completion.is_some());
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.input().text(), "@README.md");
}

#[test]
fn builtin_at_file_completion_without_plugins() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "x").unwrap();
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.set_workdir(dir.path().to_path_buf());
    app.handle_key(key('@'));
    assert!(app.input_completion.is_some(), "@ opens file completion");
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.input().text(), "@README.md");
}

#[test]
fn terminal_input_hook_consumes_matching_key() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
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
    let mut app = App::new(buffer, tx);
    let rt = kage_plugin::PluginRuntime::new().unwrap();
    rt.eval("kage.on_terminal_input(function() return true end)")
        .unwrap();
    app.set_plugin_terminal_hooks(rt.shared_terminal_hooks());
    assert_eq!(app.handle_key(ctrl('q')), Some(AppExit::Quit));
}

#[test]
fn terminal_input_off_stops_consuming() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
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
fn autocomplete_inert_without_providers() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.handle_key(key('h'));
    app.handle_key(key('i'));
    assert!(app.input_completion.is_none());
    assert_eq!(app.input().text(), "hi");
}

#[test]
fn tree_command_without_source_reports_unavailable() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    assert!(app.dispatch_builtin("tree", "").is_none());
    assert!(app.session_tree.is_none());
    let buf = buffer.lock().unwrap();
    assert!(matches!(
        buf.blocks().last(),
        Some(crate::buffer::Block::Custom { kind, .. }) if kind == "kage:error"
    ));
}

#[test]
fn tree_command_opens_and_enter_dispatches_resume() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.set_session_tree_source(Box::new(|| {
        vec![
            crate::overlay::SessionNode {
                id: "root".into(),
                path: "/s/root.jsonl".into(),
                parent: None,
                label: "root".into(),
                is_current: true,
            },
            crate::overlay::SessionNode {
                id: "child".into(),
                path: "/s/child.jsonl".into(),
                parent: Some("root".into()),
                label: "child".into(),
                is_current: false,
            },
        ]
    }));
    assert!(app.dispatch_builtin("tree", "").is_none());
    assert!(app.session_tree.is_some());
    // Selection starts on the current session (root); Enter resumes.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.session_tree.is_none());
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::ResumeSession(std::path::PathBuf::from(
            "/s/root.jsonl"
        )))
    );
}

#[test]
fn tree_d_key_dispatches_delete_request() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.set_session_tree_source(Box::new(|| {
        vec![crate::overlay::SessionNode {
            id: "only".into(),
            path: "/s/only.jsonl".into(),
            parent: None,
            label: "only".into(),
            is_current: false,
        }]
    }));
    app.dispatch_builtin("tree", "");
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::DeleteSession(std::path::PathBuf::from(
            "/s/only.jsonl"
        )))
    );
}

#[test]
fn set_editor_modeless_flips_the_input_editor() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
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
fn settings_command_opens_overlay_and_esc_closes_it() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    // `:settings` opens the modal (reads config read-only; never
    // writes, so this is safe in a test).
    assert!(app.dispatch_builtin("settings", "").is_none());
    assert!(app.settings_overlay.is_some());
    // While open it owns the keyboard; Esc cancels without persist.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.settings_overlay.is_none());
    assert_eq!(app.input().text(), "", "esc went to the overlay, not input");
}

fn snapshot_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buf = terminal.backend().buffer();
    let mut out = Vec::new();
    for y in 0..buf.area.height {
        let mut row = String::new();
        for x in 0..buf.area.width {
            row.push_str(buf[(x, y)].symbol());
        }
        out.push(row.trim_end().to_owned());
    }
    out
}

#[test]
fn pasted_text_lands_in_input_area_with_newline_preserved() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
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
fn scrolling_up_freezes_viewport_when_more_content_arrives() {
    let buffer = shared_buffer();
    if let Ok(mut buf) = buffer.lock() {
        for i in 0..20 {
            buf.push_user(format!("line{i}"));
        }
    }
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    // Default mode is Insert; switch to Normal for scrolling keys.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    // Stage C made k/j/G pane-aware: switch to buffer pane so
    // scrolling keys hit the buffer instead of moving the input
    // cursor.
    app.input.set_focused_pane(Pane::Buffer);
    // User scrolls up by 5 from the bottom.
    for _ in 0..5 {
        app.handle_key(key('k'));
    }
    let scroll_after_user = buffer.lock().unwrap().scroll();
    assert_eq!(scroll_after_user, 5);
    // Streaming delta arrives.
    if let Ok(mut buf) = buffer.lock() {
        buf.append_assistant_delta("new\nstreaming\ncontent");
    }
    // User's scroll position is preserved.
    assert_eq!(buffer.lock().unwrap().scroll(), 5);
    // Pressing G snaps back to bottom (auto-follow rearmed).
    app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
    assert_eq!(buffer.lock().unwrap().scroll(), 0);
    assert!(buffer.lock().unwrap().is_following());
}

#[test]
fn history_walk_replaces_input_text() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
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
    let mut app = App::new(buffer.clone(), tx);
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

// --- PN.9 validation error tests ---

fn builtin_registry() -> Vec<&'static CommandSpec> {
    BUILTIN_COMMANDS.iter().collect()
}

#[test]
fn validated_unknown_command_returns_error_with_suggestion() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let registry = builtin_registry();
    let result = app.run_command_validated("quut", &registry);
    match result {
        CommandResult::ValidationError(msg) => {
            assert!(msg.contains("unknown command: quut"), "got {msg:?}");
            assert!(
                msg.contains("did you mean"),
                "should suggest closest match, got {msg:?}"
            );
        }
        other @ CommandResult::Done(_) => {
            panic!("expected ValidationError, got {other:?}");
        }
    }
}

#[test]
fn validated_invalid_choice_returns_error() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
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
    let mut app = App::new(buffer, tx);
    let registry = builtin_registry();
    // "model" without a required <id> argument
    let result = app.run_command_validated("model", &registry);
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
    let mut app = App::new(buffer, tx);
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
    let mut app = App::new(buffer, tx);
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
    let mut app = App::new(buffer, tx);
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
    let mut app = App::new(buffer, tx);
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
    let mut app = App::new(buffer, tx);
    let registry = builtin_registry();
    // "mouse" without arg is valid (optional arg)
    let result = app.run_command_validated("mouse", &registry);
    assert!(
        matches!(result, CommandResult::Done(_)),
        "mouse with no arg should be valid, got {result:?}"
    );
}

// --- PN.10 keystroke-level e2e tests ---
//
// These tests drive the modal state machine with raw `KeyEvent`s
// and confirm that `:` and `/` both reach `run_command_validated`
// through `dispatch_key`. They are the last gate against regressing
// either pathway after the unification done in PN.6.

fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        app.handle_key(key(c));
    }
}

#[test]
fn colon_keystrokes_dispatch_quit_handler() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
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
    let mut app = App::new(buffer, tx);
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

#[test]
fn colon_tab_completes_to_lcp_and_opens_popup() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(key(':'));
    app.handle_key(key('m'));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let cl = app.cmdline.as_ref().expect("cmdline open");
    assert_eq!(cl.text(), "mo", "tab should extend to LCP of model/mouse");
    assert!(cl.popup_open(), "popup should be visible after LCP step");
    assert_eq!(cl.selected(), None, "LCP step does not pre-select a row");
}

#[test]
fn slash_tab_completes_to_lcp_and_opens_popup() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.handle_key(key('/'));
    app.handle_key(key('m'));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let sp = app.slash_palette.as_ref().expect("palette open");
    let cl = sp.cmdline();
    assert_eq!(cl.text(), "mo", "tab should extend to LCP of model/mouse");
    assert!(cl.popup_open(), "popup should be visible after LCP step");
}

#[test]
fn colon_bad_arg_keeps_cmdline_open_with_error() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
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
    let mut app = App::new(buffer, tx);
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

fn select_item(label: &str, value: serde_json::Value) -> kage_plugin::SelectItem {
    kage_plugin::SelectItem {
        label: label.to_owned(),
        value,
        detail: None,
    }
}

#[test]
fn plugin_dialog_pick_sends_selected_item_value() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let (dtx, drx) = mpsc::channel();
    app.set_plugin_dialog(drx);
    let (reply_tx, reply_rx) = mpsc::channel();
    dtx.send(PluginDialog::Select {
        title: "Pick".to_owned(),
        items: vec![
            select_item("alpha", serde_json::json!("A")),
            select_item("beta", serde_json::json!(42)),
        ],
        reply: reply_tx,
    })
    .unwrap();

    app.drain_plugin_dialog();
    assert!(app.plugin_overlay.is_some());
    assert!(app.active_dialog.is_some());

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!(42)));
    assert!(app.plugin_overlay.is_none());
    assert!(app.active_dialog.is_none());
}

#[test]
fn plugin_dialog_cancel_sends_none() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let (dtx, drx) = mpsc::channel();
    app.set_plugin_dialog(drx);
    let (reply_tx, reply_rx) = mpsc::channel();
    dtx.send(PluginDialog::Select {
        title: "Pick".to_owned(),
        items: vec![select_item("only", serde_json::json!("x"))],
        reply: reply_tx,
    })
    .unwrap();

    app.drain_plugin_dialog();
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(reply_rx.recv().unwrap(), None);
    assert!(app.plugin_overlay.is_none());
    assert!(app.active_dialog.is_none());
}

#[test]
fn plugin_dialog_empty_items_resolves_to_none_without_a_picker() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let (dtx, drx) = mpsc::channel();
    app.set_plugin_dialog(drx);
    let (reply_tx, reply_rx) = mpsc::channel();
    dtx.send(PluginDialog::Select {
        title: "Empty".to_owned(),
        items: Vec::new(),
        reply: reply_tx,
    })
    .unwrap();

    app.drain_plugin_dialog();

    assert!(app.plugin_overlay.is_none());
    assert_eq!(reply_rx.recv().unwrap(), None);
}

#[test]
fn plugin_dialog_not_drained_while_another_overlay_is_open() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let (dtx, drx) = mpsc::channel();
    app.set_plugin_dialog(drx);
    app.picker = Some(OverlayPicker::new("busy", vec![PickItem::simple("x")]));
    app.picker_kind = Some(PickerKind::Model);
    let (reply_tx, _reply_rx) = mpsc::channel();
    dtx.send(PluginDialog::Select {
        title: "later".to_owned(),
        items: vec![select_item("a", serde_json::json!("a"))],
        reply: reply_tx,
    })
    .unwrap();

    app.drain_plugin_dialog();

    assert_eq!(app.picker_kind, Some(PickerKind::Model));
    assert!(app.plugin_overlay.is_none());
    assert!(app.active_dialog.is_none());
}

fn open_confirm(app: &mut App) -> std::sync::mpsc::Receiver<Option<serde_json::Value>> {
    let (dtx, drx) = mpsc::channel();
    app.set_plugin_dialog(drx);
    let (reply_tx, reply_rx) = mpsc::channel();
    dtx.send(PluginDialog::Confirm {
        title: "Delete?".to_owned(),
        message: "are you sure".to_owned(),
        reply: reply_tx,
    })
    .unwrap();
    app.drain_plugin_dialog();
    assert!(app.plugin_overlay.is_some());
    reply_rx
}

#[test]
fn plugin_confirm_yes_resumes_with_true() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let reply_rx = open_confirm(&mut app);

    app.handle_key(key('y'));

    assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!(true)));
    assert!(app.plugin_overlay.is_none());
    assert!(app.active_dialog.is_none());
}

#[test]
fn plugin_confirm_no_resumes_with_false() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let reply_rx = open_confirm(&mut app);

    app.handle_key(key('n'));

    assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!(false)));
}

#[test]
fn plugin_confirm_cancel_resumes_with_false() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let reply_rx = open_confirm(&mut app);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!(false)));
    assert!(app.plugin_overlay.is_none());
    assert!(app.active_dialog.is_none());
}

fn open_input(app: &mut App) -> std::sync::mpsc::Receiver<Option<serde_json::Value>> {
    let (dtx, drx) = mpsc::channel();
    app.set_plugin_dialog(drx);
    let (reply_tx, reply_rx) = mpsc::channel();
    dtx.send(PluginDialog::Input {
        title: "Your name".to_owned(),
        placeholder: Some("e.g. Ada".to_owned()),
        reply: reply_tx,
    })
    .unwrap();
    app.drain_plugin_dialog();
    assert!(app.plugin_overlay.is_some());
    reply_rx
}

#[test]
fn plugin_input_submit_resumes_with_text() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let reply_rx = open_input(&mut app);

    app.handle_key(key('A'));
    app.handle_key(key('d'));
    app.handle_key(key('a'));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!("Ada")));
    assert!(app.plugin_overlay.is_none());
    assert!(app.active_dialog.is_none());
}

#[test]
fn plugin_input_cancel_resumes_with_nil() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let reply_rx = open_input(&mut app);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(reply_rx.recv().unwrap(), None);
    assert!(app.plugin_overlay.is_none());
    assert!(app.active_dialog.is_none());
}

fn open_editor(app: &mut App) -> std::sync::mpsc::Receiver<Option<serde_json::Value>> {
    let (dtx, drx) = mpsc::channel();
    app.set_plugin_dialog(drx);
    let (reply_tx, reply_rx) = mpsc::channel();
    dtx.send(PluginDialog::Editor {
        title: "Compose".to_owned(),
        prefill: Some("hi".to_owned()),
        reply: reply_tx,
    })
    .unwrap();
    app.drain_plugin_dialog();
    assert!(app.plugin_overlay.is_some());
    reply_rx
}

#[test]
fn plugin_editor_ctrl_s_resumes_with_buffer() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let reply_rx = open_editor(&mut app);

    app.handle_key(key('!'));
    app.handle_key(ctrl('s'));

    assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!("hi!")));
    assert!(app.plugin_overlay.is_none());
    assert!(app.active_dialog.is_none());
}

#[test]
fn plugin_editor_cancel_resumes_with_nil() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let reply_rx = open_editor(&mut app);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(reply_rx.recv().unwrap(), None);
    assert!(app.plugin_overlay.is_none());
    assert!(app.active_dialog.is_none());
}

#[test]
fn plugin_keybinding_dispatches_invoke_request() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.set_plugin_keybindings(vec!["ctrl+g".to_owned()]);

    app.handle_key(ctrl('g'));

    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::InvokePluginKeybinding {
            chord: "ctrl+g".to_owned()
        })
    );
}

#[test]
fn unbound_chord_does_not_dispatch_keybinding() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.set_plugin_keybindings(vec!["ctrl+g".to_owned()]);

    app.handle_key(ctrl('h'));

    assert!(!matches!(
        rx.try_recv(),
        Ok(RunRequest::InvokePluginKeybinding { .. })
    ));
}

#[test]
fn open_overlay_suppresses_plugin_keybinding() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.set_plugin_keybindings(vec!["ctrl+g".to_owned()]);
    app.picker = Some(OverlayPicker::new("busy", vec![PickItem::simple("x")]));
    app.picker_kind = Some(PickerKind::Model);

    app.handle_key(ctrl('g'));

    assert!(rx.try_recv().is_err(), "picker should swallow the chord");
}

#[test]
fn plugin_theme_drain_applies_request_and_refresh_populates_snapshot() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let state: kage_plugin::SharedThemeState =
        std::sync::Arc::new(std::sync::Mutex::new(kage_plugin::ThemeState::default()));
    let request: kage_plugin::SharedThemeRequest = std::sync::Arc::new(std::sync::Mutex::new(None));
    app.set_plugin_theme(state.clone(), request.clone());

    // The dedicated snapshot refresh populates current + available.
    app.refresh_plugin_theme_state();
    {
        let s = state.lock().unwrap();
        assert!(!s.current.is_empty());
        assert!(s.available.iter().any(|n| n == "tokyo-night"));
    }

    // Queue a switch; the drain applies it on this thread but does
    // not touch the snapshot.
    *request.lock().unwrap() = Some("tokyo-night".to_owned());
    app.drain_plugin_theme();
    assert_eq!(crate::theme::current().name, "tokyo-night");
    assert!(request.lock().unwrap().is_none(), "request was drained");

    // The next refresh reflects the applied theme in the snapshot.
    app.refresh_plugin_theme_state();
    assert_eq!(state.lock().unwrap().current, "tokyo-night");
}

#[test]
fn pasting_an_image_path_attaches_instead_of_inserting_text() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let dir = std::env::temp_dir().join(format!("kage-paste-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("shot.png");
    std::fs::write(&png, [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1]).unwrap();

    app.handle_paste(&png.to_string_lossy());
    assert_eq!(app.input.attached().len(), 1, "image path attached");
    assert!(
        app.input.text().contains("[image #1 shot.png"),
        "an editable marker is inserted, not the raw path: {:?}",
        app.input.text()
    );
    assert!(
        !app.input.text().contains(&*dir.to_string_lossy()),
        "the path itself was not pasted as text: {:?}",
        app.input.text()
    );

    app.handle_paste("just some text");
    assert_eq!(app.input.attached().len(), 1, "no new attachment");
    assert!(
        app.input.text().contains("just some text"),
        "non-image paste inserted verbatim"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn session_picker_defaults_to_cwd_and_ctrl_a_toggles_all() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    // cwd scope -> only "here"; all scope -> "here" + "elsewhere".
    app.set_session_lister(Box::new(|all| {
        if all {
            vec![PickItem::simple("here"), PickItem::simple("elsewhere")]
        } else {
            vec![PickItem::simple("here")]
        }
    }));

    app.session_scope_all = false;
    app.open_session_picker(false);
    assert_eq!(app.picker_kind, Some(PickerKind::Session));
    // cwd scope: Enter resolves the only cwd session.
    app.dispatch_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::ResumeSession(p) => assert_eq!(p, std::path::PathBuf::from("here")),
        other => panic!("expected ResumeSession(here), got {other:?}"),
    }

    // Reopen, toggle to all dirs with Ctrl+A, pick the 2nd row.
    app.session_scope_all = false;
    app.open_session_picker(false);
    assert!(app.dispatch_picker_key(ctrl('a')).is_none());
    assert!(app.session_scope_all, "Ctrl+A switched to all dirs");
    // Ungrouped rows sort alphabetically, so row 0 is now
    // "elsewhere" - which only exists in the all-dirs dataset,
    // proving the toggle re-listed with the wider scope.
    app.dispatch_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::ResumeSession(p) => {
            assert_eq!(p, std::path::PathBuf::from("elsewhere"));
        }
        other => panic!("expected ResumeSession(elsewhere), got {other:?}"),
    }
    // Toggle back to cwd-only.
    app.session_scope_all = false;
    app.open_session_picker(false);
    assert!(app.dispatch_picker_key(ctrl('a')).is_none());
    assert!(app.session_scope_all);
    assert!(app.dispatch_picker_key(ctrl('a')).is_none());
    assert!(!app.session_scope_all, "Ctrl+A toggles back");
}
