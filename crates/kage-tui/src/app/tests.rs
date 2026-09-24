//! Integration tests for the App event and render loop.

use std::sync::mpsc;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use kage_core::config::KeybindingsConfig;
use kage_core::keymap::Keymap;

use super::*;
use crate::events::shared_buffer;

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn code(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The keymap the embedded `_defaults.lua` builds, loaded once.
fn default_keymap() -> Keymap {
    static DEFAULTS: std::sync::OnceLock<Keymap> = std::sync::OnceLock::new();
    DEFAULTS
        .get_or_init(|| {
            let rt = kage_plugin::PluginRuntime::new().expect("runtime builds");
            let report = kage_plugin::load_all(None, &rt).expect("defaults load");
            assert!(report.all_ok(), "{report:?}");
            lock(&rt.keymap()).clone()
        })
        .clone()
}

/// An App whose keymap holds the embedded defaults, as the TUI builds
/// it with no plugins and no user config.
fn app_with_defaults(buffer: SharedBuffer, tx: Sender<RunRequest>) -> App {
    let mut app = App::new(buffer, tx);
    app.set_keymap(Arc::new(Mutex::new(default_keymap())));
    app
}

/// An App whose keymap comes from a full load: the defaults, then
/// `bindings` as `[keybindings] bindings`, then `init` as `init.lua`.
fn app_with_config(
    init: &str,
    bindings: &[(&str, &str)],
) -> (App, mpsc::Receiver<RunRequest>, SharedBuffer) {
    let user = tempfile::tempdir().unwrap();
    std::fs::write(user.path().join("init.lua"), init).unwrap();
    let keybindings = KeybindingsConfig {
        bindings: bindings
            .iter()
            .map(|(lhs, rhs)| ((*lhs).to_owned(), (*rhs).to_owned()))
            .collect(),
        ..KeybindingsConfig::default()
    };
    let rt = kage_plugin::PluginRuntime::builder()
        .user_dir(Some(user.path().to_path_buf()))
        .keybindings(keybindings)
        .build()
        .unwrap();
    let report = kage_plugin::load_all(None, &rt).unwrap();
    assert!(report.all_ok(), "{report:?}");
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(buffer.clone(), tx);
    app.set_keymap(rt.keymap());
    (app, rx, buffer)
}

/// Resolve `key` through the keymap and the editor grammar without
/// carrying it out.
fn routes(app: &mut App, key: KeyEvent) -> Vec<Routed> {
    app.route_editor_key(key, Instant::now())
}

fn input(action: InputAction) -> Vec<Routed> {
    vec![Routed::Input(action)]
}

/// Put the App in vim normal mode with `pane` focused.
fn normal(app: &mut App, pane: Pane) {
    app.handle_key(code(KeyCode::Esc));
    app.input.set_focused_pane(pane);
    assert_eq!(app.input.mode(), Mode::Normal);
}

fn last_block_text(buffer: &SharedBuffer) -> String {
    match buffer.lock().unwrap().blocks().last() {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
    }
}

#[test]
fn partial_selection_copies_only_highlighted_cells_not_whole_block() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
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
    let mut app = app_with_defaults(buffer.clone(), tx);
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
        Ok(RunRequest::Cancel),
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
        "<C-t>",
        ":theme set tokyo-night",
        "action:BeginCommand",
        "config.toml",
        "b: vim normal mode, conversation pane",
        "action:Scroll(20)",
        "init.lua",
        "action:OpenModelPicker",
        "defaults",
        "built in (editor grammar",
        "<C-q>        quit",
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

#[test]
fn plugin_command_alias_resolves_to_canonical_invoke() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
        autocomplete: Vec::new(),
        models: Vec::new(),
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
    let mut app = app_with_defaults(buffer, tx);
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
fn submitting_a_prompt_sends_it_without_painting() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    // Default mode is Insert; type "hi" and press Enter.
    app.handle_key(key('h'));
    app.handle_key(key('i'));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let req = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    assert_eq!(
        req,
        RunRequest::Submit {
            text: "hi".into(),
            images: Vec::new(),
            queue: false,
        }
    );
    assert!(
        !buffer
            .lock()
            .unwrap()
            .blocks()
            .iter()
            .any(|b| matches!(b, crate::buffer::Block::User { .. })),
        "the user block appears when the engine delivers the prompt"
    );
    assert_eq!(app.input().mode(), Mode::Insert);
}

#[test]
fn submit_while_a_run_is_in_flight_is_still_sent() {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    let usage = crate::usage::shared_session_usage();
    usage.lock().unwrap().working = true;
    app.set_session_usage(usage);

    app.handle_submit("later".into(), false);

    assert_eq!(
        rx.recv_timeout(Duration::from_millis(100)).unwrap(),
        RunRequest::Submit {
            text: "later".into(),
            images: Vec::new(),
            queue: false,
        }
    );
}

#[test]
fn submit_carries_attached_images() {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    app.input.attach_image(crate::image::AttachedImage {
        source: kage_core::ImageSource::Base64 {
            data: "AAAA".into(),
        },
        mime: "image/png".into(),
        label: "shot.png".into(),
        bytes: 3,
    });

    app.handle_submit("look".into(), false);

    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::Submit { text, images, .. } => {
            assert_eq!(text, "look");
            assert_eq!(images.len(), 1);
        }
        other => panic!("expected Submit with images, got {other:?}"),
    }
}

fn envelope(
    session: kage_core::SessionId,
    seq: u64,
    event: impl Into<kage_core::protocol::Event>,
) -> kage_core::protocol::Envelope {
    kage_core::protocol::Envelope {
        session,
        seq,
        event: event.into(),
    }
}

/// An App fed by an engine event channel.
fn app_with_events() -> (
    App,
    mpsc::Receiver<RunRequest>,
    mpsc::Sender<kage_core::protocol::Envelope>,
) {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    let (events_tx, events_rx) = mpsc::channel();
    app.set_engine_events(events_rx);
    app.set_session_usage(crate::usage::shared_session_usage());
    (app, rx, events_tx)
}

#[test]
fn delivered_prompts_paint_user_blocks() {
    let (mut app, _rx, events) = app_with_events();
    let session = kage_core::SessionId::new();
    let message = kage_core::Message::new(
        kage_core::Role::User,
        vec![kage_core::Content::Text { text: "hi".into() }],
        None,
    );
    events
        .send(envelope(
            session,
            1,
            kage_core::LoopEvent::MessageAppended { message },
        ))
        .unwrap();
    assert!(app.drain_engine_events());
    assert!(matches!(
        app.buffer.lock().unwrap().blocks().last(),
        Some(crate::buffer::Block::User { text }) if text == "hi"
    ));
}

#[test]
fn events_from_other_sessions_are_ignored() {
    let (mut app, _rx, events) = app_with_events();
    let (mine, other) = (kage_core::SessionId::new(), kage_core::SessionId::new());
    let notice = |text: &str| kage_core::protocol::HostEvent::Notice {
        level: kage_core::protocol::NoticeLevel::Error,
        text: text.into(),
        transient: false,
    };
    events.send(envelope(mine, 1, notice("mine"))).unwrap();
    events.send(envelope(other, 1, notice("other"))).unwrap();
    app.drain_engine_events();
    let count = app.buffer.lock().unwrap().blocks().len();
    assert_eq!(count, 1);
}

#[test]
fn permission_requests_open_a_prompt_and_answer_through_requests() {
    let (mut app, rx, events) = app_with_events();
    let request_id = kage_core::protocol::RequestId(7);
    events
        .send(envelope(
            kage_core::SessionId::new(),
            1,
            kage_core::protocol::HostEvent::PermissionRequested {
                request_id,
                tool_call_id: None,
                tool: "bash".into(),
                subject: "ls".into(),
                input: serde_json::json!({ "command": "ls" }),
            },
        ))
        .unwrap();
    assert!(app.drain_engine_events());
    assert!(app.approval_panel.is_some());

    app.answer_permission(PermissionDecision::AllowOnce);
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::ResolvePermission {
            request_id,
            decision: PermissionDecision::AllowOnce
        })
    );
}

/// Feed `events` as one session's envelopes and drain them.
fn feed(
    app: &mut App,
    events: &mpsc::Sender<kage_core::protocol::Envelope>,
    batch: Vec<kage_core::protocol::Event>,
) {
    let session = app.active_session.unwrap_or_default();
    for (seq, event) in batch.into_iter().enumerate() {
        events
            .send(envelope(session, seq as u64 + 1, event))
            .unwrap();
    }
    app.drain_engine_events();
}

fn bash_start(id: &str) -> kage_core::protocol::Event {
    kage_core::LoopEvent::ToolCallStart {
        id: kage_core::ToolCallId::new(id),
        name: "bash".into(),
        input_partial: serde_json::json!({ "command": "ls" }),
    }
    .into()
}

fn permission_request(id: &str, request: u64) -> kage_core::protocol::Event {
    kage_core::protocol::HostEvent::PermissionRequested {
        request_id: kage_core::protocol::RequestId(request),
        tool_call_id: Some(kage_core::ToolCallId::new(id)),
        tool: "bash".into(),
        subject: "ls".into(),
        input: serde_json::json!({ "command": "ls" }),
    }
    .into()
}

fn tool_phase(app: &App, id: &str) -> crate::view::tool_view::ToolPhase {
    let buf = app.buffer.lock().unwrap();
    buf.blocks()
        .iter()
        .find_map(|b| match b {
            crate::buffer::Block::ToolCall { call_id, phase, .. } if call_id == id => Some(*phase),
            _ => None,
        })
        .expect("tool call present")
}

#[test]
fn tool_timing_excludes_the_approval_wait() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![bash_start("c1"), permission_request("c1", 1)],
    );
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Waiting);
    std::thread::sleep(std::time::Duration::from_millis(60));
    app.answer_permission(PermissionDecision::AllowOnce);
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Running);
    feed(
        &mut app,
        &events,
        vec![
            kage_core::LoopEvent::ToolCallEnd {
                id: kage_core::ToolCallId::new("c1"),
                output: kage_core::ToolOutput {
                    is_error: false,
                    text: "stdout:\na\nexit: 0".into(),
                    structured: None,
                    terminate: false,
                },
            }
            .into(),
        ],
    );
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Done);
    let buf = app.buffer.lock().unwrap();
    let duration = buf.blocks().iter().find_map(|b| match b {
        crate::buffer::Block::ToolResult { duration_ms, .. } => *duration_ms,
        _ => None,
    });
    assert!(duration.is_some_and(|ms| ms < 60), "{duration:?}");
}

#[test]
fn a_denied_call_reads_denied_after_its_result() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![bash_start("c1"), permission_request("c1", 1)],
    );
    app.answer_permission(PermissionDecision::Deny);
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Denied);
    app.buffer
        .lock()
        .unwrap()
        .push_tool_result("c1", "denied by the user", true);
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Denied);
}

#[test]
fn a_request_resolved_elsewhere_resumes_the_call() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![
            bash_start("c1"),
            bash_start("c2"),
            permission_request("c1", 1),
            permission_request("c2", 2),
        ],
    );
    assert_eq!(tool_phase(&app, "c2"), ToolPhase::Waiting);
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::PermissionResolved {
                request_id: kage_core::protocol::RequestId(2),
            }
            .into(),
        ],
    );
    assert_eq!(tool_phase(&app, "c2"), ToolPhase::Running);
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Waiting);
}

/// A moment past the approval panel's type-ahead guard.
fn past_guard() -> Instant {
    Instant::now() + crate::overlay::approval::TYPE_AHEAD_GUARD + Duration::from_millis(100)
}

fn resolutions(rx: &mpsc::Receiver<RunRequest>) -> Vec<RunRequest> {
    rx.try_iter()
        .filter(|r| {
            matches!(
                r,
                RunRequest::ResolvePermission { .. } | RunRequest::Submit { .. }
            )
        })
        .collect()
}

#[test]
fn the_approval_panel_replaces_the_input_below_the_buffer() {
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![bash_start("c1"), permission_request("c1", 1)],
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    let (top, height) = {
        let buf = app.buffer.lock().unwrap();
        (buf.last_area_y(), buf.last_area_height())
    };
    let bottom = usize::from(top + height);
    let title = rows
        .iter()
        .position(|r| r.contains("Run this command?"))
        .expect("panel title");
    assert!(title >= bottom, "title at {title}, buffer ends at {bottom}");
    assert_eq!(rows[title + 1], "   $ ls");
    assert!(rows[title..].iter().any(|r| r == " > 1. Yes"), "{rows:#?}");
    assert!(
        rows[..bottom]
            .iter()
            .all(|r| !r.contains("1. Yes") && !r.contains("$ ls")),
        "{rows:#?}"
    );
}

#[test]
fn keys_right_after_the_panel_opens_are_dropped() {
    let (mut app, rx, events) = app_with_events();
    feed(&mut app, &events, vec![permission_request("c1", 1)]);
    app.dispatch_key(key('y'));
    assert!(app.approval_panel.is_some());
    assert!(resolutions(&rx).is_empty());
}

#[test]
fn s_allows_the_tool_for_the_session() {
    let (mut app, rx, events) = app_with_events();
    feed(&mut app, &events, vec![permission_request("c1", 1)]);
    app.approval_key_at(key('s'), past_guard());
    assert_eq!(
        resolutions(&rx),
        [RunRequest::ResolvePermission {
            request_id: kage_core::protocol::RequestId(1),
            decision: PermissionDecision::AllowSession,
        }]
    );
    assert!(app.approval_panel.is_none());
}

#[test]
fn feedback_denies_then_submits_the_text() {
    let (mut app, rx, events) = app_with_events();
    feed(&mut app, &events, vec![permission_request("c1", 1)]);
    let now = past_guard();
    app.approval_key_at(key('t'), now);
    for c in "use ls".chars() {
        app.approval_key_at(key(c), now);
    }
    app.approval_key_at(code(KeyCode::Enter), now);
    assert_eq!(
        resolutions(&rx),
        [
            RunRequest::ResolvePermission {
                request_id: kage_core::protocol::RequestId(1),
                decision: PermissionDecision::Deny,
            },
            RunRequest::Submit {
                text: "use ls".to_owned(),
                images: Vec::new(),
                queue: false,
            },
        ]
    );
}

#[test]
fn queued_requests_count_through_the_batch() {
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![
            permission_request("c1", 1),
            permission_request("c2", 2),
            permission_request("c3", 3),
        ],
    );
    let title = |app: &mut App| {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.render_into(&mut terminal).unwrap();
        snapshot_rows(&terminal)
            .into_iter()
            .find(|r| r.contains("Run this command?"))
            .expect("panel title")
    };
    assert!(title(&mut app).contains(" 1 of 3 "));
    app.approval_key_at(key('1'), past_guard());
    assert!(title(&mut app).contains(" 2 of 3 "));
    app.approval_key_at(key('y'), Instant::now());
    assert!(
        title(&mut app).contains(" 2 of 3 "),
        "the next panel guards too"
    );
}

#[test]
fn the_draft_survives_an_approval() {
    let (mut app, _rx, events) = app_with_events();
    type_str(&mut app, "half a thought");
    feed(&mut app, &events, vec![permission_request("c1", 1)]);
    assert!(app.footer_hint().starts_with("1-5 or y s a n t"));
    app.approval_key_at(key('t'), past_guard());
    app.approval_key_at(key('x'), past_guard());
    app.approval_key_at(code(KeyCode::Esc), past_guard());
    app.approval_key_at(key('n'), past_guard());
    assert!(app.approval_panel.is_none());
    assert_eq!(app.input.text(), "half a thought");
}

#[test]
fn answering_moves_the_row_from_waiting_to_running() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![bash_start("c1"), permission_request("c1", 1)],
    );
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Waiting);
    app.approval_key_at(code(KeyCode::Enter), past_guard());
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Running);
}

#[test]
fn run_ended_stops_every_running_tool() {
    use crate::view::tool_view::ToolPhase;
    let (mut app, _rx, events) = app_with_events();
    feed(&mut app, &events, vec![bash_start("c1")]);
    assert!(app.has_running_tool_call());
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::RunEnded {
                outcome: kage_core::protocol::RunOutcome::Cancelled,
            }
            .into(),
        ],
    );
    assert!(!app.has_running_tool_call());
    assert_eq!(tool_phase(&app, "c1"), ToolPhase::Interrupted);
}

#[test]
fn session_changed_rebuilds_the_transcript() {
    let (mut app, _rx, events) = app_with_events();
    app.buffer.lock().unwrap().push_user("stale");
    let message = kage_core::Message::new(
        kage_core::Role::User,
        vec![kage_core::Content::Text {
            text: "restored".into(),
        }],
        None,
    );
    events
        .send(envelope(
            kage_core::SessionId::new(),
            1,
            kage_core::protocol::HostEvent::SessionChanged {
                path: "/tmp/s.jsonl".into(),
                title: None,
                messages: vec![message],
            },
        ))
        .unwrap();
    app.drain_engine_events();
    let buf = app.buffer.lock().unwrap();
    assert_eq!(buf.blocks().len(), 1);
    assert!(matches!(
        buf.blocks().first(),
        Some(crate::buffer::Block::User { text }) if text == "restored"
    ));
}

#[test]
fn state_changes_update_the_modeline() {
    let (mut app, _rx, events) = app_with_events();
    events
        .send(envelope(
            kage_core::SessionId::new(),
            1,
            kage_core::protocol::HostEvent::StateChanged {
                state: kage_core::protocol::SessionState {
                    model: "mock:m".into(),
                    working: true,
                    ..Default::default()
                },
            },
        ))
        .unwrap();
    app.drain_engine_events();
    let usage = app.session_usage_snapshot().unwrap();
    assert_eq!(usage.model, "mock:m");
    assert!(usage.working);
    assert!(app.is_run_in_flight());
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
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel));
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
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel));
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
        })
    );
    assert_eq!(app.input().text(), "");
    assert_eq!(app.input().history(), ["a"]);
}

fn user_message(text: &str) -> kage_core::protocol::Event {
    let message = kage_core::Message::new(
        kage_core::Role::User,
        vec![kage_core::Content::Text { text: text.into() }],
        None,
    );
    kage_core::LoopEvent::MessageAppended { message }.into()
}

fn pending_rows(app: &mut App) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    app.render_into(&mut terminal).unwrap();
    snapshot_rows(&terminal)
        .into_iter()
        .filter(|r| r.starts_with("  > "))
        .collect()
}

#[test]
fn pending_rows_show_until_delivered_steers_first() {
    let (mut app, _rx, events) = app_with_events();
    lock(app.session_usage.as_ref().unwrap()).working = true;
    for c in "later".chars() {
        app.handle_key(key(c));
    }
    app.handle_key(code(KeyCode::Tab));
    for c in "now".chars() {
        app.handle_key(key(c));
    }
    app.handle_key(code(KeyCode::Enter));
    let steer = format!("  > now{}after the current tool call", " ".repeat(24));
    let queue = format!("  > later{}when this run ends", " ".repeat(31));
    assert_eq!(pending_rows(&mut app), [queue.clone(), steer]);
    feed(&mut app, &events, vec![user_message("now")]);
    assert_eq!(pending_rows(&mut app), [queue]);
    feed(
        &mut app,
        &events,
        vec![user_message("rewritten by a plugin")],
    );
    assert!(pending_rows(&mut app).is_empty());
}

#[test]
fn pending_rows_fold_past_three_and_clear_on_session_change() {
    let (mut app, _rx, events) = app_with_events();
    lock(app.session_usage.as_ref().unwrap()).working = true;
    for text in ["one", "two", "three", "four", "five"] {
        app.handle_submit(text.into(), true);
    }
    let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    let first = rows.iter().position(|r| r.starts_with("  > one")).unwrap();
    assert!(rows[first + 2].starts_with("  > three"), "{rows:#?}");
    assert_eq!(rows[first + 3], "  +2 more");
    assert!(rows[first + 4].starts_with('\u{2500}'), "{rows:#?}");
    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::SessionChanged {
                path: std::path::PathBuf::from("/tmp/s.jsonl"),
                title: None,
                messages: Vec::new(),
            }
            .into(),
        ],
    );
    assert!(app.pending.is_empty());
}

#[test]
fn ctrl_c_interrupts_over_an_open_cmdline() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
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
fn cancel_command_sends_a_cancel_request() {
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();
    let result = app.run_command_validated("cancel", &registry);
    assert!(
        matches!(result, CommandResult::Done(None)),
        "expected Done(None), got {result:?}"
    );
    assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel));
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

/// Render `app` into a fresh 60x10 terminal.
fn render_app(app: &mut App) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
    app.render_into(&mut terminal).unwrap();
    terminal
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
        "enter to steer \u{B7} tab to queue \u{B7} esc to clear the draft"
    );
    app.handle_key(code(KeyCode::Backspace));
    assert_eq!(app.footer_hint(), "tab to queue \u{B7} esc to interrupt");
    app.set_editor_modeless(false);
    assert_eq!(
        app.footer_hint(),
        "tab to queue \u{B7} ctrl+c to interrupt \u{B7} esc for normal mode"
    );
    app.handle_key(key('x'));
    assert_eq!(
        app.footer_hint(),
        "enter to steer \u{B7} tab to queue \u{B7} esc for normal mode"
    );
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(
        app.footer_hint(),
        "ctrl+c to clear the draft \u{B7} i to type \u{B7} ? for shortcuts \u{B7} : for commands"
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
fn the_help_overlay_never_overlaps_the_input_rows() {
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(shared_buffer(), tx);
    app.set_editor_modeless(true);
    let blank = snapshot_rows(&render_app(&mut app));
    app.handle_key(key('?'));
    assert!(app.help_overlay.is_some());
    let rows = snapshot_rows(&render_app(&mut app));
    let input_top = blank.len() - 4;
    assert_eq!(rows[input_top..], blank[input_top..], "{rows:?}");
    assert_ne!(rows[..input_top], blank[..input_top], "{rows:?}");
}

#[test]
fn autocomplete_popup_opens_and_tab_accepts() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
fn autocomplete_inert_without_providers() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.handle_key(key('h'));
    app.handle_key(key('i'));
    assert!(app.input_completion.is_none());
    assert_eq!(app.input().text(), "hi");
}

#[test]
fn tree_command_without_source_reports_unavailable() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    assert!(
        app.dispatch_builtin("tree", "", &crate::command::ParsedArgs::new())
            .is_none()
    );
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
    let mut app = app_with_defaults(buffer, tx);
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
    assert!(
        app.dispatch_builtin("tree", "", &crate::command::ParsedArgs::new())
            .is_none()
    );
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
    let mut app = app_with_defaults(buffer, tx);
    app.set_session_tree_source(Box::new(|| {
        vec![crate::overlay::SessionNode {
            id: "only".into(),
            path: "/s/only.jsonl".into(),
            parent: None,
            label: "only".into(),
            is_current: false,
        }]
    }));
    app.dispatch_builtin("tree", "", &crate::command::ParsedArgs::new());
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
fn settings_command_opens_overlay_and_esc_closes_it() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    // `:settings` opens the modal (reads config read-only; never
    // writes, so this is safe in a test).
    assert!(
        app.dispatch_builtin("settings", "", &crate::command::ParsedArgs::new())
            .is_none()
    );
    assert!(app.settings_overlay.is_some());
    // While open it owns the keyboard; Esc cancels without persist.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.settings_overlay.is_none());
    assert_eq!(app.input().text(), "", "esc went to the overlay, not input");
}

#[test]
fn settings_thinking_level_persists_and_sets_the_live_level() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.apply_settings_at(
        &serde_json::json!({ "thinking_level": "high" }),
        Some(path.clone()),
    );
    let cfg = kage_core::config::Config::load(&path).unwrap();
    assert_eq!(cfg.ui.thinking_level.as_deref(), Some("high"));
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::SetThinkingLevel("high".into()))
    );
}

#[test]
fn settings_thinking_level_ignores_unknown_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.apply_settings_at(
        &serde_json::json!({ "thinking_level": "maximum" }),
        Some(path.clone()),
    );
    let cfg = kage_core::config::Config::load(&path).unwrap();
    assert_eq!(cfg.ui.thinking_level, None);
    assert!(rx.try_recv().is_err());
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
fn scrolling_up_freezes_viewport_when_more_content_arrives() {
    let buffer = shared_buffer();
    if let Ok(mut buf) = buffer.lock() {
        for i in 0..20 {
            buf.push_user(format!("line{i}"));
        }
    }
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
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
    let mut app = app_with_defaults(buffer.clone(), tx);
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

// --- PN.9 validation error tests ---

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

#[test]
fn colon_tab_completes_to_lcp_and_opens_popup() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
fn palette_hides_aliases_until_typed() {
    let mut app = defaults_app();
    app.handle_key(key('/'));
    assert!(!palette_values(&app).iter().any(|v| v == "q"));
    app.handle_key(key('q'));
    assert!(palette_values(&app).iter().any(|v| v == "q"));
    assert_eq!(palette_selected(&app).as_deref(), Some("q"));
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
fn model_picker_renders_its_login_note() {
    let mut app = defaults_app();
    app.set_model_choices(vec![PickItem::simple("fake:m")]);
    let _ = app.apply(InputAction::OpenModelPicker);
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    assert!(
        rows.iter().any(|r| r.contains("/login to add a provider")),
        "{rows:#?}"
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
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
    let reply_rx = open_confirm(&mut app);

    app.handle_key(key('n'));

    assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!(false)));
}

#[test]
fn plugin_confirm_cancel_resumes_with_false() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
    let reply_rx = open_editor(&mut app);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(reply_rx.recv().unwrap(), None);
    assert!(app.plugin_overlay.is_none());
    assert!(app.active_dialog.is_none());
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
fn pasting_an_image_path_attaches_instead_of_inserting_text() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
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
    let mut app = app_with_defaults(buffer, tx);
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
    let app = app_with_defaults(buffer.clone(), tx);
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
fn ctrl_f_counts_matches_while_typing() {
    let (mut app, buffer) = search_fixture();
    app.handle_key(ctrl('f'));
    assert!(app.search_line.is_some());
    for c in "needle".chars() {
        app.handle_key(key(c));
    }
    assert!(app.search_line.is_some(), "still open before Enter");
    assert_eq!(app.compute_search_match_count(), Some((1, 2)));
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
    app.handle_key(key('x'));
    assert_eq!(app.compute_search_match_count(), Some((0, 0)));
    assert_eq!(
        buffer.lock().unwrap().focus(),
        Some(0),
        "no match keeps the view"
    );
}

#[test]
fn search_line_down_and_up_walk_matches() {
    let (mut app, buffer) = search_fixture();
    app.handle_key(ctrl('f'));
    for c in "needle".chars() {
        app.handle_key(key(c));
    }
    app.handle_key(code(KeyCode::Down));
    assert_eq!(buffer.lock().unwrap().focus(), Some(3));
    assert_eq!(app.search_line.as_ref().unwrap().text(), "needle");
    app.handle_key(code(KeyCode::Up));
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
    app.handle_key(code(KeyCode::Enter));
    assert!(app.search_line.is_none());
    assert_eq!(app.search_pattern.as_deref(), Some("needle"));
    assert_eq!(buffer.lock().unwrap().focus(), Some(1));
}

#[test]
fn search_from_the_bottom_lands_on_the_latest_match() {
    let (mut app, buffer) = search_fixture();
    buffer.lock().unwrap().set_focus(None);
    app.handle_key(ctrl('f'));
    for c in "needle".chars() {
        app.handle_key(key(c));
    }
    assert_eq!(buffer.lock().unwrap().focus(), Some(3));
}

#[test]
fn esc_restores_the_previous_pattern_and_view() {
    let (mut app, buffer) = search_fixture();
    app.search_pattern = Some("gamma".into());
    buffer.lock().unwrap().set_scroll(2);
    app.handle_key(ctrl('f'));
    for c in "needle".chars() {
        app.handle_key(key(c));
    }
    app.handle_key(code(KeyCode::Down));
    assert_eq!(app.search_pattern.as_deref(), Some("needle"));
    app.handle_key(code(KeyCode::Esc));
    assert!(app.search_line.is_none());
    assert_eq!(app.search_pattern.as_deref(), Some("gamma"));
    let buf = buffer.lock().unwrap();
    assert_eq!(buf.focus(), Some(0));
    assert_eq!(buf.scroll(), Some(2));
}

#[test]
fn a_block_leaving_the_match_set_loses_its_match_rule() {
    let (mut app, buffer) = search_fixture();
    let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
    let rule_of_block_1 = |app: &mut App, terminal: &mut Terminal<TestBackend>| {
        app.render_into(terminal).unwrap();
        let buf = buffer.lock().unwrap();
        let (top, _) = buf.screen_rows_of(1).expect("block 1 painted");
        terminal.backend().buffer()[(0, top)].symbol().to_owned()
    };
    app.search_pattern = Some("needle".into());
    assert_eq!(rule_of_block_1(&mut app, &mut terminal), "\u{258c}");
    app.search_pattern = Some("gamma".into());
    assert_eq!(rule_of_block_1(&mut app, &mut terminal), " ");
}

#[test]
fn pasting_into_the_search_line_searches() {
    let (mut app, buffer) = search_fixture();
    app.handle_key(ctrl('f'));
    app.handle_paste("gamma");
    assert_eq!(app.compute_search_match_count(), Some((1, 1)));
    assert_eq!(buffer.lock().unwrap().focus(), Some(2));
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

/// An App over forty one-line replies, painted into a 40x12 terminal
/// and scrolled to the top, with a selection anchored on the first
/// buffer row.
fn drag_fixture() -> (App, SharedBuffer, Terminal<TestBackend>) {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    {
        let mut buf = buffer.lock().unwrap();
        for i in 0..40 {
            buf.append_assistant_delta(&format!("reply {i}"));
            buf.finish_streaming();
        }
        buf.set_scroll(0);
    }
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let area_y = buffer.lock().unwrap().last_area_y();
    app.mouse_down(area_y, 2);
    app.render_into(&mut terminal).unwrap();
    (app, buffer, terminal)
}

#[test]
fn a_drag_below_the_buffer_scrolls_one_line_and_extends_the_selection() {
    let (mut app, buffer, mut terminal) = drag_fixture();
    let (area_y, height) = {
        let buf = buffer.lock().unwrap();
        (buf.last_area_y(), buf.last_area_height())
    };
    let below = area_y + height + 1;
    app.mouse_drag(below, 5);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(1));
    let (_, cursor) = app.screen_selection.unwrap();
    assert_eq!(cursor, (usize::from(height), 5));

    app.render_into(&mut terminal).unwrap();
    app.mouse_drag(below, 5);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(2));
    let (anchor, cursor) = app.screen_selection.unwrap();
    assert_eq!(anchor, (0, 2));
    assert_eq!(cursor, (usize::from(height) + 1, 5));
}

#[test]
fn a_drag_above_the_top_clamps_and_a_drag_inside_does_not_scroll() {
    let (mut app, buffer, _terminal) = drag_fixture();
    let area_y = buffer.lock().unwrap().last_area_y();
    app.mouse_drag(area_y.saturating_sub(1), 3);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(0));
    assert_eq!(app.screen_selection.unwrap().1, (0, 3));
    app.mouse_drag(area_y + 2, 3);
    assert_eq!(buffer.lock().unwrap().scroll(), Some(0));
    assert_eq!(app.screen_selection.unwrap().1, (2, 3));
}

#[test]
fn a_drag_release_toasts_the_copied_characters() {
    let (mut app, buffer, mut terminal) = drag_fixture();
    app.set_toasts(crate::toast::shared_toasts());
    let area_y = buffer.lock().unwrap().last_area_y();
    app.mouse_drag(area_y, 30);
    app.render_into(&mut terminal).unwrap();
    app.mouse_up(area_y);
    assert!(app.screen_selection.is_none());
    let toasts = app.live_toasts();
    assert!(
        toasts
            .iter()
            .any(|t| t.text.starts_with("copied ") && t.text.ends_with(" characters")),
        "{toasts:?}"
    );
}

#[test]
fn modal_open_reflects_every_modal_field() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
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
        thinking_level: "off".into(),
        from_lua: Vec::new(),
    }));
    assert!(app.modal_open());
    app.settings_overlay = None;

    app.slash_palette = Some(SlashPalette::new(Vec::new(), SlashContext::default()));
    assert!(app.modal_open());
    app.slash_palette = None;

    app.plugin_overlay = Some(Box::new(crate::overlay::widget::EmptyOverlayWidget));
    assert!(app.modal_open());
    app.plugin_overlay = None;

    app.open_help();
    assert!(app.modal_open());
    app.help_overlay = None;

    assert!(!app.modal_open());
}

#[test]
fn mouse_events_are_swallowed_while_modal_is_open() {
    use ratatui::crossterm::event::MouseButton;
    use ratatui::crossterm::event::MouseEventKind;

    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
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
    let mut app = app_with_defaults(buffer, tx);

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
    let mut app2 = app_with_defaults(shared_buffer(), tx2);
    app2.search_line = Some(CommandLine::new());
    app2.handle_paste("pat");
    assert_eq!(app2.search_line.as_ref().unwrap().text(), "pat");

    // Nothing open: the main input receives it verbatim.
    app.handle_paste("plain");
    assert_eq!(app.input().text(), "plain");
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
fn f3_opens_the_jump_picker_and_resolve_focuses_the_block() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    {
        let mut buf = app.buffer.lock().unwrap();
        buf.push_user("find me");
        buf.begin_assistant();
        buf.append_assistant_delta("answer");
    }

    app.dispatch_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
    assert_eq!(app.picker_kind, Some(crate::app::PickerKind::Jump));

    // Rows are newest-first: row 0 is the latest block (the
    // assistant reply at buffer index 1).
    app.dispatch_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.picker.is_none(), "picker closed on resolve");
    assert_eq!(app.buffer.lock().unwrap().effective_focus(), Some(1));
}

#[test]
fn shell_submit_sends_run_shell_without_a_user_block() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer.clone(), tx);
    app.input.set_modeless(true);
    for c in "!ls".chars() {
        app.dispatch_key(key(c));
    }
    app.dispatch_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::RunShell(cmd) => assert_eq!(cmd, "ls"),
        other => panic!("expected RunShell, got {other:?}"),
    }
    assert!(
        !buffer
            .lock()
            .unwrap()
            .blocks()
            .iter()
            .any(|b| matches!(b, crate::buffer::Block::User { .. })),
        "shell submit must not paint a user block"
    );
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
fn jump_picker_lists_the_newest_target_first() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    {
        let mut buf = app.buffer.lock().unwrap();
        buf.push_user("aaa oldest");
        buf.begin_assistant();
        buf.append_assistant_delta("middle");
        buf.push_user("zzz newest");
    }
    app.open_jump_picker();
    app.dispatch_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.buffer.lock().unwrap().effective_focus(), Some(2));
}

#[test]
fn model_picker_with_no_models_explains_how_to_connect() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_toasts(crate::toast::shared_toasts());
    let _ = app.apply(InputAction::OpenModelPicker);
    assert!(app.picker.is_none());
    assert!(
        app.live_toasts()
            .iter()
            .any(|t| t.text.contains("Run /login")),
        "empty model list must explain itself"
    );
}

// Keys that moved from the editor grammar to `_defaults.lua`, resolved
// through an App holding the embedded defaults.

fn defaults_app() -> App {
    let (tx, _rx) = mpsc::channel();
    app_with_defaults(shared_buffer(), tx)
}

fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

fn ctrl_code(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

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
    assert!(rows.contains(&("<C-t>", "dark theme")), "{rows:?}");
    assert!(!rows.iter().any(|(lhs, _)| *lhs == "<C-s>"), "{rows:?}");
}
