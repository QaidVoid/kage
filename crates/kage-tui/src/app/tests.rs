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
fn config_action_binding_fires_builtin_action() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let errs = app.set_config_keybindings(vec![("ctrl+g".into(), "action:BeginCommand".into())]);
    assert!(errs.is_empty(), "{errs:?}");
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
fn set_config_keybindings_reports_unknown_action_name() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let errs = app.set_config_keybindings(vec![
        ("ctrl+g".into(), "action:Nonsense".into()),
        ("ctrl+h".into(), "action:".into()),
        ("ctrl+i".into(), "quit".into()),
    ]);
    assert_eq!(errs.len(), 2, "{errs:?}");
    assert!(errs[0].contains("action:Nonsense"), "{}", errs[0]);
    assert!(errs[0].contains("action:"), "{}", errs[0]);
    assert!(errs[1].contains("`action:`"), "{}", errs[1]);
    assert_eq!(app.config_keybindings.len(), 1, "good binding kept");
}

#[test]
fn config_action_binding_wins_over_builtin_handler() {
    let buffer = shared_buffer();
    if let Ok(mut buf) = buffer.lock() {
        buf.append_thinking_delta("step one");
        buf.finish_streaming();
    }
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    // ctrl+o builtin is ToggleFold on the focused block; the binding
    // must take the action path instead.
    let _ = app.set_config_keybindings(vec![("ctrl+o".into(), "action:BeginCommand".into())]);
    assert_eq!(app.handle_key(ctrl('o')), None);
    assert!(app.cmdline.is_some(), "the bound action ran");
    if let Ok(buf) = buffer.lock() {
        assert!(
            matches!(
                buf.blocks()[0],
                crate::buffer::Block::Thinking { folded: false, .. }
            ),
            "the builtin fold toggle did not run"
        );
    }
}

#[test]
fn keybindings_command_lists_action_bindings() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    let _ = app.set_config_keybindings(vec![("ctrl+g".into(), "action:BeginCommand".into())]);
    app.push_keybindings();
    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last() {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    };
    assert!(rendered.contains("ctrl+g"), "{rendered}");
    assert!(rendered.contains("action:BeginCommand"), "{rendered}");
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
fn set_plugin_commands_reuses_leaked_specs_on_unchanged_reload() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let cmd = || PluginCommand {
        name: "greet".into(),
        aliases: vec!["hi".into()],
        is_override: false,
        description: "say hello".into(),
        args: vec![crate::command::OwnedArgSpec::Text {
            name: "who".into(),
            optional: false,
            hint: "<who>".into(),
        }],
    };
    app.set_plugin_commands(vec![cmd()]);
    let first = app.plugin_command_specs[0];
    assert_eq!(app.plugin_commands_leaked.len(), 1);

    // Re-registering an identical set (the hot-reload case) reuses
    // the leaked spec instead of leaking a second copy.
    app.set_plugin_commands(vec![cmd()]);
    assert!(
        std::ptr::eq(app.plugin_command_specs[0], first),
        "equal reload reuses the leaked spec"
    );
    assert_eq!(app.plugin_commands_leaked.len(), 1);

    // A changed command leaks one fresh spec, which the next
    // unchanged reload then reuses.
    let mut changed = cmd();
    changed.description = "say hello loudly".into();
    app.set_plugin_commands(vec![changed.clone()]);
    assert_eq!(
        app.plugin_commands_leaked.len(),
        2,
        "changed command leaks a fresh spec"
    );
    assert!(!std::ptr::eq(app.plugin_command_specs[0], first));
    app.set_plugin_commands(vec![changed]);
    assert_eq!(
        app.plugin_commands_leaked.len(),
        2,
        "changed reload reuses too"
    );
}

#[test]
fn drain_plugin_refresh_reseeds_commands_and_widgets() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let (rtx, rrx) = mpsc::channel();
    app.set_plugin_refresh(rrx);

    app.set_plugin_commands(vec![PluginCommand {
        name: "old".into(),
        aliases: Vec::new(),
        is_override: false,
        description: "pre-reload".into(),
        args: Vec::new(),
    }]);
    let pre = app.plugin_command_specs[0];

    rtx.send(PluginRefresh {
        commands: vec![PluginCommand {
            name: "zznew".into(),
            aliases: vec!["zzn".into()],
            is_override: false,
            description: "post-reload".into(),
            args: Vec::new(),
        }],
        widgets: Vec::new(),
    })
    .unwrap();
    assert!(app.drain_plugin_refresh(), "a queued snapshot applies");
    assert!(
        !std::ptr::eq(app.plugin_command_specs[0], pre),
        "commands re-seeded from the snapshot"
    );
    assert_eq!(app.plugin_commands.len(), 1);
    assert_eq!(app.plugin_commands[0].0, "zznew");
    assert!(
        app.plugin_texts_dirty,
        "widget reseed marks the text cache dirty"
    );
    assert!(!app.drain_plugin_refresh(), "drained channel is a no-op");
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
fn steering_submit_while_run_in_flight_queues_and_holds_channel() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let steering = crate::events::shared_steering();
    let usage = crate::usage::shared_session_usage();
    usage.lock().unwrap().working = true;
    app.set_steering_queue(steering.clone());
    app.set_session_usage(usage);

    app.handle_submit("later".into());

    assert_eq!(
        steering.lock().unwrap().pop_front().as_deref(),
        Some("later"),
        "mid-run text submit must land in the steering queue"
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "nothing may be sent on the worker channel while queued"
    );
}

#[test]
fn steering_submit_when_idle_takes_channel_path() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let steering = crate::events::shared_steering();
    app.set_steering_queue(steering.clone());
    app.set_session_usage(crate::usage::shared_session_usage());

    app.handle_submit("now".into());

    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::Submit { text, images } => {
            assert_eq!(text, "now");
            assert!(images.is_empty());
        }
        other => panic!("expected Submit, got {other:?}"),
    }
    assert!(steering.lock().unwrap().is_empty());
}

#[test]
fn steering_submit_with_images_takes_channel_even_mid_run() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    let steering = crate::events::shared_steering();
    let usage = crate::usage::shared_session_usage();
    usage.lock().unwrap().working = true;
    app.set_steering_queue(steering.clone());
    app.set_session_usage(usage);
    app.input.attach_image(crate::image::AttachedImage {
        source: kage_core::ImageSource::Base64 {
            data: "AAAA".into(),
        },
        mime: "image/png".into(),
        label: "shot.png".into(),
        bytes: 3,
    });

    app.handle_submit("look".into());

    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::Submit { text, images } => {
            assert_eq!(text, "look");
            assert_eq!(images.len(), 1);
        }
        other => panic!("expected Submit with images, got {other:?}"),
    }
    assert!(steering.lock().unwrap().is_empty());
}

#[test]
fn drain_clipboard_attach_attaches_completed_result() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.attach_tx
        .send(Ok(crate::image::AttachedImage {
            source: kage_core::ImageSource::Base64 {
                data: "AAAA".into(),
            },
            mime: "image/png".into(),
            label: "shot.png".into(),
            bytes: 3,
        }))
        .unwrap();

    app.drain_clipboard_attach();

    let attached = app.input().attached();
    assert_eq!(attached.len(), 1, "async attach landed on the input");
    assert_eq!(attached[0].1.label, "shot.png");
}

#[test]
fn drain_clipboard_attach_surfaces_errors_inline() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    app.attach_tx
        .send(Err("no image on the clipboard".into()))
        .unwrap();

    app.drain_clipboard_attach();

    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last() {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected an error block, got {other:?}"),
    };
    assert!(rendered.contains("no image on the clipboard"), "{rendered}");
}

#[test]
fn submit_after_worker_drop_paints_error_block() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel::<RunRequest>();
    let mut app = App::new(buffer.clone(), tx);
    drop(rx);

    app.handle_submit("lost".into());

    let buf = buffer.lock().unwrap();
    let rendered = match buf.blocks().last() {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected an error block, got {other:?}"),
    };
    assert!(rendered.contains("worker has stopped"), "{rendered}");
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
fn ctrl_c_in_insert_cancels_instead_of_typing_c() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.handle_key(key('x')); // default mode is Insert
    app.handle_key(ctrl('c'));
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel));
    assert_eq!(app.input().text(), "x", "ctrl+c must not type 'c'");
}

#[test]
fn ctrl_c_interrupts_over_an_open_cmdline() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(key(':'));
    assert!(app.cmdline.is_some(), "cmdline should be open");
    app.handle_key(ctrl('c'));
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel));
    assert!(
        app.cmdline.is_some(),
        "interrupt must not close the cmdline"
    );
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

/// Open `:tree` on a one-node fixture and press `d` on it. Returns
/// the app and the request receiver to assert against.
fn tree_delete_fixture() -> (App, mpsc::Receiver<RunRequest>) {
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
    (app, rx)
}

#[test]
fn tree_d_key_asks_before_deleting_and_n_declines() {
    let (mut app, rx) = tree_delete_fixture();
    // The tree closes and a confirmation opens; nothing is deleted yet.
    assert!(app.session_tree.is_none());
    assert!(app.plugin_overlay.is_some());
    assert!(app.pending_tree_delete.is_some());
    assert!(rx.try_recv().is_err());

    // `n` declines: the dialog closes and still nothing is deleted.
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(app.plugin_overlay.is_none());
    assert!(app.pending_tree_delete.is_none());
    assert!(rx.try_recv().is_err());
}

#[test]
fn tree_delete_confirm_yes_sends_delete_request() {
    let (mut app, rx) = tree_delete_fixture();
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(app.plugin_overlay.is_none());
    assert!(app.pending_tree_delete.is_none());
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::DeleteSession(std::path::PathBuf::from(
            "/s/only.jsonl"
        )))
    );
}

#[test]
fn tree_delete_confirm_esc_cancels_without_deleting() {
    let (mut app, rx) = tree_delete_fixture();
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.plugin_overlay.is_none());
    assert!(app.pending_tree_delete.is_none());
    assert!(rx.try_recv().is_err());
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
    // Prime the renderer so `last_virtual_top` reflects a real frame;
    // the scroll anchor derives from it while following.
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    app.render_into(&mut terminal).unwrap();
    let vt0 = buffer.lock().unwrap().last_virtual_top();
    // Default mode is Insert; switch to Normal for scrolling keys.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    // Stage C made k/j/G pane-aware: switch to buffer pane so
    // scrolling keys hit the buffer instead of moving the input
    // cursor.
    app.input.set_focused_pane(Pane::Buffer);
    // User scrolls up by 5 rows: an absolute anchor below the bottom.
    for _ in 0..5 {
        app.handle_key(key('k'));
    }
    let pinned = vt0.saturating_sub(5);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(pinned));
    // Streaming delta arrives and the frame repaints.
    if let Ok(mut buf) = buffer.lock() {
        buf.append_assistant_delta("new\nstreaming\ncontent");
    }
    app.render_into(&mut terminal).unwrap();
    // The viewport stayed exactly where the user was reading.
    assert_eq!(buffer.lock().unwrap().scroll(), Some(pinned));
    assert_eq!(buffer.lock().unwrap().last_virtual_top(), pinned);
    // Pressing G snaps back to bottom (auto-follow re-armed).
    app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
    assert_eq!(buffer.lock().unwrap().scroll(), None);
    assert!(buffer.lock().unwrap().is_following());
}

#[test]
fn focusing_an_offscreen_block_scrolls_it_into_view() {
    let buffer = shared_buffer();
    if let Ok(mut buf) = buffer.lock() {
        for i in 0..20 {
            buf.push_user(format!("line{i}"));
        }
    }
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    app.render_into(&mut terminal).unwrap();
    // The bottom of the buffer is on screen; block 0 is not.
    assert!(buffer.lock().unwrap().screen_rows_of(0).is_none());

    // Focusing block 0 brings it into view on the next frame.
    buffer.lock().unwrap().set_focus(Some(0));
    app.render_into(&mut terminal).unwrap();
    assert_eq!(buffer.lock().unwrap().scroll(), Some(0));
    assert_eq!(buffer.lock().unwrap().last_virtual_top(), 0);
    assert!(buffer.lock().unwrap().screen_rows_of(0).is_some());

    // Focusing the newest block from the top clamps to the bottom and
    // re-arms follow.
    buffer.lock().unwrap().set_focus(Some(19));
    app.render_into(&mut terminal).unwrap();
    assert!(buffer.lock().unwrap().is_following());
    assert!(buffer.lock().unwrap().screen_rows_of(19).is_some());
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
    let _guard = crate::theme::theme_test_lock();
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
    crate::theme::reset_current_for_tests();
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

/// Four assistant blocks; "needle" appears in blocks 1 and 3.
fn search_fixture() -> (App, SharedBuffer) {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let app = App::new(buffer.clone(), tx);
    {
        let mut buf = buffer.lock().unwrap();
        for text in ["alpha", "needle one", "gamma", "needle two"] {
            buf.append_assistant_delta(text);
            buf.finish_streaming();
        }
        buf.set_focus(Some(0));
    }
    (app, buffer)
}

#[test]
fn search_jump_walks_matches_without_wrapping() {
    let (mut app, buffer) = search_fixture();
    app.search_pattern = Some("needle".into());

    app.jump_to_search_match(true);
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
    app.jump_to_search_match(true);
    assert_eq!(buffer.lock().unwrap().focus(), Some(3));

    // Forward past the last match is a no-op, not a wrap.
    app.jump_to_search_match(true);
    assert_eq!(buffer.lock().unwrap().focus(), Some(3));

    app.jump_to_search_match(false);
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));

    // Backward past the first match is a no-op too.
    app.jump_to_search_match(false);
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
}

#[test]
fn search_cache_refreshes_when_pattern_changes() {
    let (mut app, _buffer) = search_fixture();
    app.search_pattern = Some("needle".into());
    app.refresh_search_matches();
    assert_eq!(app.search_matches(), &[1, 3]);
    // Fixture pins focus on block 0, which is not a match.
    assert_eq!(app.compute_search_match_count(), Some((0, 2)));

    // Same buffer version, different pattern: the cache must not go
    // stale (the counter is a binary search over these indices).
    app.search_pattern = Some("gamma".into());
    app.refresh_search_matches();
    assert_eq!(app.search_matches(), &[2]);
    assert_eq!(app.compute_search_match_count(), Some((0, 1)));

    // No match anywhere: empty list, count of zero.
    app.search_pattern = Some("zzz".into());
    assert_eq!(app.compute_search_match_count(), Some((0, 0)));
}

#[test]
fn search_cache_rebuilds_after_scrollback_compaction() {
    let (mut app, buffer) = search_fixture();
    app.search_pattern = Some("needle".into());
    app.refresh_search_matches();
    assert_eq!(app.search_matches(), &[1, 3]);

    // Drop block 0: compaction shifts indices and bumps the buffer
    // version, so the cached match list must rebuild to [0, 2].
    // Focus pointed at the dropped block and falls back to the
    // transcript bottom (block 2), which is itself a match.
    assert_eq!(buffer.lock().unwrap().compact_to(3), 1);
    app.refresh_search_matches();
    assert_eq!(app.search_matches(), &[0, 2]);
    assert_eq!(app.compute_search_match_count(), Some((2, 2)));
}

#[test]
fn search_count_is_none_without_a_pattern() {
    let (mut app, _buffer) = search_fixture();
    assert_eq!(app.compute_search_match_count(), None);
    app.jump_to_search_match(true);
    // Jump with no pattern is a no-op; the fixture pinned block 0.
    assert_eq!(app.buffer.lock().unwrap().focus(), Some(0));
}

#[test]
fn noh_command_clears_search_highlighting() {
    let (mut app, _buffer) = search_fixture();
    app.search_pattern = Some("needle".into());
    assert_eq!(app.compute_search_match_count(), Some((0, 2)));

    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();
    let result = app.run_command_validated("noh", &registry);
    assert!(
        matches!(result, CommandResult::Done(None)),
        "expected Done(None), got {result:?}"
    );

    assert!(app.search_pattern.is_none());
    assert_eq!(app.compute_search_match_count(), None);
    // The cached match list empties on the next refresh.
    app.refresh_search_matches();
    assert!(app.search_matches().is_empty());
}

fn mouse_event(
    kind: ratatui::crossterm::event::MouseEventKind,
) -> ratatui::crossterm::event::MouseEvent {
    ratatui::crossterm::event::MouseEvent {
        kind,
        column: 5,
        row: 5,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn modal_open_reflects_every_modal_field() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);
    assert!(!app.modal_open());

    app.cmdline = Some(CommandLine::new());
    assert!(app.modal_open());
    app.cmdline = None;

    app.search_line = Some(CommandLine::new());
    assert!(app.modal_open());
    app.search_line = None;

    app.picker = Some(OverlayPicker::new("pick", Vec::new()));
    assert!(app.modal_open());
    app.picker = None;

    app.session_tree = Some(SessionTreeOverlay::new(Vec::new()));
    assert!(app.modal_open());
    app.session_tree = None;

    app.settings_overlay = Some(SettingsOverlay::new(SettingsInit {
        themes: Vec::new(),
        theme: String::new(),
        models: Vec::new(),
        model: String::new(),
        mouse: false,
        threshold: 0.8,
        keybindings: Vec::new(),
        editor_modeless: false,
    }));
    assert!(app.modal_open());
    app.settings_overlay = None;

    app.slash_palette = Some(SlashPalette::new(Vec::new(), SlashContext::default()));
    assert!(app.modal_open());
    app.slash_palette = None;

    app.plugin_overlay = Some(Box::new(crate::overlay::widget::EmptyOverlayWidget));
    assert!(app.modal_open());
    app.plugin_overlay = None;

    assert!(!app.modal_open());
}

#[test]
fn mouse_events_are_swallowed_while_modal_is_open() {
    use ratatui::crossterm::event::MouseButton;
    use ratatui::crossterm::event::MouseEventKind;

    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    app.set_scroll(4);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(4));

    // Picker open: the wheel must not move the hidden buffer.
    app.picker = Some(OverlayPicker::new("pick", Vec::new()));
    app.handle_mouse_event(mouse_event(MouseEventKind::ScrollDown));
    assert_eq!(buffer.lock().unwrap().scroll(), Some(4));

    // Same while the `:` cmdline is open, and a right-click must not
    // stack a context menu on top of the modal.
    app.picker = None;
    app.cmdline = Some(CommandLine::new());
    app.handle_mouse_event(mouse_event(MouseEventKind::ScrollDown));
    assert_eq!(buffer.lock().unwrap().scroll(), Some(4));
    app.handle_mouse_event(mouse_event(MouseEventKind::Down(MouseButton::Right)));
    assert!(app.context_menu.is_none());

    // No modal: the same wheel event scrolls the buffer again.
    app.cmdline = None;
    app.handle_mouse_event(mouse_event(MouseEventKind::ScrollDown));
    #[allow(clippy::cast_sign_loss)]
    let expected = 4 + MOUSE_SCROLL_LINES as usize;
    assert_eq!(buffer.lock().unwrap().scroll(), Some(expected));
}

#[test]
fn paste_routes_to_the_active_overlay() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(buffer, tx);

    // Cmdline open: the paste lands in the command line.
    app.cmdline = Some(CommandLine::new());
    app.handle_paste("theme ");
    assert_eq!(app.cmdline.as_ref().unwrap().text(), "theme ");
    app.cmdline = None;

    // Slash palette open: the paste lands in the palette.
    app.slash_palette = Some(SlashPalette::new(Vec::new(), SlashContext::default()));
    app.handle_paste("the");
    assert_eq!(app.slash_palette.as_ref().unwrap().cmdline().text(), "the");
    app.slash_palette = None;

    // A modal without a text field swallows the paste instead of
    // letting it fall through to the hidden main input.
    app.picker = Some(OverlayPicker::new("pick", Vec::new()));
    app.handle_paste("leak");
    assert_eq!(app.input().text(), "");
    app.picker = None;

    // Search line open: the paste lands there.
    let (tx2, _rx2) = mpsc::channel();
    let mut app2 = App::new(shared_buffer(), tx2);
    app2.search_line = Some(CommandLine::new());
    app2.handle_paste("pat");
    assert_eq!(app2.search_line.as_ref().unwrap().text(), "pat");

    // Nothing open: the main input receives it verbatim.
    app.handle_paste("plain");
    assert_eq!(app.input().text(), "plain");
}
