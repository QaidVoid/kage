//! MCP servers: prompt commands, resource completion, the MCP picker and
//! login.

use super::*;

fn mcp_catalog() -> Vec<kage_core::protocol::McpServerInfo> {
    use kage_core::protocol::{
        McpPrompt, McpPromptArgument, McpResource, McpResourceTemplate, McpServerInfo,
        McpServerStatus,
    };
    let argument = |name: &str, required| McpPromptArgument {
        name: name.into(),
        description: None,
        required,
    };
    let server = |name: &str, status| McpServerInfo {
        name: name.into(),
        status,
        tools: 0,
        resources: Vec::new(),
        templates: Vec::new(),
        prompts: Vec::new(),
    };
    vec![
        McpServerInfo {
            tools: 11,
            resources: vec![
                McpResource {
                    uri: "test://static/resource/1".into(),
                    name: "Resource 1".into(),
                    description: None,
                    mime_type: Some("text/plain".into()),
                },
                McpResource {
                    uri: "test://static/resource/2".into(),
                    name: "Resource 2".into(),
                    description: None,
                    mime_type: None,
                },
            ],
            templates: vec![McpResourceTemplate {
                uri_template: "test://static/resource/{id}".into(),
                name: "Static resource".into(),
                description: None,
            }],
            prompts: vec![
                McpPrompt {
                    name: "simple_prompt".into(),
                    description: Some("A prompt without arguments".into()),
                    arguments: Vec::new(),
                },
                McpPrompt {
                    name: "complex_prompt".into(),
                    description: Some("A prompt with arguments".into()),
                    arguments: vec![argument("temperature", true), argument("style", false)],
                },
            ],
            ..server("everything", McpServerStatus::Connected)
        },
        server("linear", McpServerStatus::NeedsAuth),
        server(
            "broken",
            McpServerStatus::Failed {
                error: "spawn `nope`: No such file or directory".into(),
            },
        ),
    ]
}

/// An App fed by an engine event channel that received [`mcp_catalog`].
fn mcp_app() -> (
    App,
    mpsc::Receiver<RunRequest>,
    mpsc::Sender<kage_core::protocol::Envelope>,
) {
    let (mut app, rx, events) = app_with_events();
    let servers = mcp_catalog();
    feed(
        &mut app,
        &events,
        vec![kage_core::protocol::HostEvent::McpServers { servers }.into()],
    );
    (app, rx, events)
}

fn completion_values(app: &App) -> Vec<String> {
    app.input_completion
        .as_ref()
        .map(|c| c.items().iter().map(|i| i.value.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn at_completion_offers_mcp_servers_then_their_resources() {
    let (mut app, _rx, _events) = mcp_app();
    type_str(&mut app, "what is in @every");
    assert_eq!(completion_values(&app), ["@everything:"]);
    app.handle_key(code(KeyCode::Tab));
    assert_eq!(app.input.text(), "what is in @everything:");
    assert_eq!(
        completion_values(&app),
        [
            "@everything:test://static/resource/1",
            "@everything:test://static/resource/2",
            "@everything:test://static/resource/{id}",
        ]
    );
    app.handle_key(code(KeyCode::Tab));
    assert_eq!(
        app.input.text(),
        "what is in @everything:test://static/resource/1"
    );
}

#[test]
fn accepting_a_template_puts_the_cursor_on_its_placeholder() {
    let (mut app, _rx, _events) = mcp_app();
    type_str(&mut app, "read @everything:");
    app.handle_key(code(KeyCode::Down));
    app.handle_key(code(KeyCode::Down));
    app.handle_key(code(KeyCode::Tab));
    let text = "read @everything:test://static/resource/{id}";
    assert_eq!(app.input.text(), text);
    assert_eq!(app.input.cursor(), text.find('{').unwrap());
    assert!(app.input_completion.is_none());
}

#[test]
fn the_palette_lists_mcp_prompts_with_their_hint_and_tag() {
    let (mut app, _rx, _events) = mcp_app();
    app.apply(InputAction::OpenCommandPalette);
    let palette = app.slash_palette.as_mut().unwrap();
    for c in "everything:".chars() {
        crate::overlay::OverlayWidget::handle_key(palette, key(c));
    }
    let rows: Vec<(String, String)> = palette
        .cmdline()
        .completions()
        .items
        .iter()
        .map(|c| (c.label(), c.description.clone().unwrap_or_default()))
        .collect();
    assert_eq!(
        rows,
        [
            (
                "everything:simple_prompt".to_owned(),
                "A prompt without arguments  [mcp]".to_owned()
            ),
            (
                "everything:complex_prompt <temperature> [style]".to_owned(),
                "A prompt with arguments  [mcp]".to_owned()
            ),
        ]
    );
}

#[test]
fn palette_tab_adds_a_space_so_arguments_do_not_merge() {
    let (mut app, _rx, _events) = mcp_app();
    for (typed, after_tab) in [
        ("everything:co", "everything:complex_prompt "),
        ("hel", "help "),
    ] {
        app.apply(InputAction::OpenCommandPalette);
        let palette = app.slash_palette.as_mut().unwrap();
        for c in typed.chars() {
            crate::overlay::OverlayWidget::handle_key(palette, key(c));
        }
        crate::overlay::OverlayWidget::handle_key(palette, code(KeyCode::Tab));
        assert_eq!(palette.cmdline().text(), after_tab);
        for c in "0.7".chars() {
            crate::overlay::OverlayWidget::handle_key(palette, key(c));
        }
        assert_eq!(palette.cmdline().text(), format!("{after_tab}0.7"));
        app.slash_palette = None;
    }
}

#[test]
fn enter_on_a_prompt_with_required_arguments_waits_for_them() {
    let (mut app, rx, _events) = mcp_app();
    app.apply(InputAction::OpenCommandPalette);
    for c in "everything:co".chars() {
        let palette = app.slash_palette.as_mut().unwrap();
        crate::overlay::OverlayWidget::handle_key(palette, key(c));
    }
    app.handle_key(code(KeyCode::Enter));
    let palette = app.slash_palette.as_ref().expect("palette stays open");
    assert_eq!(palette.cmdline().text(), "everything:complex_prompt ");
    assert!(rx.try_recv().is_err(), "nothing runs");
    for c in "0.7".chars() {
        app.handle_key(key(c));
    }
    app.handle_key(code(KeyCode::Enter));
    assert!(app.slash_palette.is_none());
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::Submit { text, .. }) if text == "/everything:complex_prompt 0.7"
    ));

    app.apply(InputAction::OpenCommandPalette);
    for c in "everything:si".chars() {
        app.handle_key(key(c));
    }
    app.handle_key(code(KeyCode::Enter));
    assert!(app.slash_palette.is_none());
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::Submit { text, .. }) if text == "/everything:simple_prompt"
    ));
}

#[test]
fn builtin_and_plugin_names_win_over_mcp_prompts() {
    let (mut app, rx, _events) = mcp_app();
    app.set_plugin_commands(vec![PluginCommand {
        name: "everything:simple_prompt".into(),
        aliases: Vec::new(),
        is_override: false,
        description: "a plugin".into(),
        args: Vec::new(),
    }]);
    let names: Vec<&str> = app.command_registry().iter().map(|s| s.name).collect();
    assert_eq!(
        names
            .iter()
            .filter(|n| **n == "everything:simple_prompt")
            .count(),
        1
    );
    assert!(names.contains(&"everything:complex_prompt"));
    let registry = app.command_registry();
    let _ = app.run_command_validated("everything:simple_prompt", &registry);
    assert!(matches!(
        rx.try_recv(),
        Ok(RunRequest::InvokePluginCommand { name, .. }) if name == "everything:simple_prompt"
    ));
    assert!(
        app.mcp_command_specs
            .iter()
            .all(|s| crate::command::find_builtin_command(s.name).is_none())
    );
}

#[test]
fn running_an_mcp_prompt_during_a_run_sends_a_queued_prompt() {
    let (mut app, rx, _events) = mcp_app();
    lock(app.session_usage.as_ref().unwrap()).working = true;
    app.apply(InputAction::OpenCommandPalette);
    let palette = app.slash_palette.as_mut().unwrap();
    for c in "everything:complex_prompt 0.7 terse".chars() {
        crate::overlay::OverlayWidget::handle_key(palette, key(c));
    }
    app.handle_key(code(KeyCode::Enter));
    assert!(app.slash_palette.is_none());
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::Submit {
            text: "/everything:complex_prompt 0.7 terse".into(),
            images: Vec::new(),
            queue: true,
            session: None,
        })
    );
}

#[test]
fn drafts_the_engine_expands_are_queued_never_steered() {
    let (mut app, rx, _events) = mcp_app();
    lock(app.session_usage.as_ref().unwrap()).working = true;
    let queued = |app: &mut App, text: &str| {
        app.handle_submit(text.into(), false);
        match rx.try_recv() {
            Ok(RunRequest::Submit { queue, .. }) => queue,
            other => panic!("expected a submit, got {other:?}"),
        }
    };
    assert!(queued(&mut app, "what is in @everything:test://x, then?"));
    assert!(queued(&mut app, "  /everything:simple_prompt"));
    assert!(queued(&mut app, "is @broken:x up"));
    assert!(!queued(&mut app, "plain text about everything:x"));
    assert!(!queued(&mut app, "/linear:prompt is not live"));
    assert!(!queued(&mut app, "@unknown:x and @everything: alone"));
}

#[test]
fn a_resource_block_renders_one_attached_line_at_80_columns() {
    let (mut app, _rx, events) = mcp_app();
    let uri = "test://static/resource/1";
    let message = kage_core::Message::new(
        kage_core::Role::User,
        vec![
            kage_core::Content::Text {
                text: format!("what is in @everything:{uri}"),
            },
            kage_core::Content::Text {
                text: kage_core::resource_block::render(
                    uri,
                    Some("everything"),
                    Some("text/plain"),
                    &"fixture contents ".repeat(80),
                ),
            },
        ],
        None,
    );
    feed(
        &mut app,
        &events,
        vec![kage_core::LoopEvent::MessageAppended { message }.into()],
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let rows = snapshot_rows(&terminal);
    let at = rows
        .iter()
        .position(|r| r.contains("> what is in"))
        .expect("prompt row");
    assert_eq!(
        rows[at..at + 2],
        [
            "\u{258E} > what is in @everything:test://static/resource/1".to_owned(),
            "\u{258E}   attached everything:test://static/resource/1 (1 KB)".to_owned(),
        ],
        "{rows:#?}"
    );
    assert!(!rows.iter().any(|r| r.contains("fixture")), "{rows:#?}");
}

#[test]
fn the_mcp_picker_lists_every_status_and_enter_restarts() {
    let (mut app, rx, _events) = mcp_app();
    let registry = app.command_registry();
    let _ = app.run_command_validated("mcp", &registry);
    assert_eq!(app.picker_kind, Some(PickerKind::Mcp));
    for (w, h) in [(120, 36), (80, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        app.render_into(&mut terminal).unwrap();
        let rows = snapshot_rows(&terminal);
        for want in [
            "everything  connected    11 tools \u{B7} 2 prompts \u{B7} 2 resources \u{B7} 1 template",
            "linear      needs login  enter to log in",
            "broken      failed       spawn `nope`: No such file or directory",
        ] {
            assert!(rows.iter().any(|r| r.contains(want)), "{want}\n{rows:#?}");
        }
    }
    app.handle_key(code(KeyCode::Enter));
    assert!(app.picker.is_none());
    assert_eq!(
        rx.try_recv(),
        Ok(RunRequest::RestartMcp("everything".into()))
    );
    let _ = app.run_command_validated("mcp restart broken", &registry);
    assert_eq!(rx.try_recv(), Ok(RunRequest::RestartMcp("broken".into())));
    assert!(matches!(
        app.run_command_validated("mcp restart", &registry),
        CommandResult::ValidationError(_)
    ));
}

#[test]
fn logging_in_to_an_mcp_server_queues_the_flow() {
    let (mut app, rx, _events) = mcp_app();
    let registry = app.command_registry();
    let _ = app.run_command_validated("mcp login broken", &registry);
    assert_eq!(app.pending_login, None, "no flow queued without a runner");
    assert!(last_block_text(&app.buffer).contains("mcp login: unavailable"));

    app.set_mcp_login_runner(std::sync::Arc::new(|_| Ok(())));
    let _ = app.run_command_validated("mcp", &registry);
    app.handle_key(code(KeyCode::Down));
    app.handle_key(code(KeyCode::Enter));
    assert!(rx.try_recv().is_err(), "a login row restarts nothing yet");
    assert_eq!(
        app.pending_login,
        Some(crate::app::PendingLogin::Mcp("linear".to_owned()))
    );
    app.pending_login = None;
    let _ = app.run_command_validated("mcp login x", &registry);
    assert_eq!(
        app.pending_login,
        Some(crate::app::PendingLogin::Mcp("x".to_owned()))
    );
}

#[test]
fn a_finished_mcp_login_restarts_the_server() {
    let (mut app, rx, _events) = mcp_app();
    let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
    let log = std::sync::Arc::clone(&asked);
    app.set_mcp_login_runner(std::sync::Arc::new(move |server| {
        log.lock().unwrap().push(server.to_owned());
        if server == "linear" {
            Ok(())
        } else {
            Err("authorization discovery: no metadata".to_owned())
        }
    }));

    app.run_login_flow(crate::app::PendingLogin::Mcp("linear".to_owned()));
    assert_eq!(rx.try_recv(), Ok(RunRequest::RestartMcp("linear".into())));

    app.run_login_flow(crate::app::PendingLogin::Mcp("broken".to_owned()));
    assert!(rx.try_recv().is_err(), "a failed login restarts nothing");
    assert!(last_block_text(&app.buffer).contains("mcp login broken: authorization discovery"));
    assert_eq!(*asked.lock().unwrap(), ["linear", "broken"]);
}

#[test]
fn the_palette_lists_mcp() {
    let mut app = defaults_app();
    app.apply(InputAction::OpenCommandPalette);
    let palette = app.slash_palette.as_mut().unwrap();
    for c in "mc".chars() {
        crate::overlay::OverlayWidget::handle_key(palette, key(c));
    }
    let values: Vec<&str> = palette
        .cmdline()
        .completions()
        .items
        .iter()
        .map(|c| c.value.as_str())
        .collect();
    assert_eq!(values, ["mcp"]);
    let item = &palette.cmdline().completions().items[0];
    assert_eq!(item.label(), "mcp [restart|login]");
    assert!(
        item.description
            .as_deref()
            .is_some_and(|d| d.starts_with("list MCP servers")),
        "{item:?}"
    );
}
