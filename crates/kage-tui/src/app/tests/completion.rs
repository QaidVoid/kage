//! Input completion: plugin autocomplete providers and the built-in
//! `@file` completion.

use super::*;

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
fn history_recall_keeps_up_out_of_the_completion_popup() {
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
    app.set_history(vec!["older prompt".into(), "newer".into()]);
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input().text(), "newer");
    assert!(
        app.input_completion.is_none(),
        "a recall must not open the popup"
    );
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        app.input().text(),
        "older prompt",
        "Up keeps walking history"
    );
    // Real editing ends the history walk and the popup returns.
    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert!(app.input_completion.is_some(), "editing reopens the popup");
}

#[test]
fn enter_accepts_the_highlighted_completion_before_sending() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "x").unwrap();
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_workdir(dir.path().to_path_buf());
    for c in "check @RE".chars() {
        app.handle_key(key(c));
    }
    assert!(app.input_completion.is_some());
    assert!(
        app.footer_hint().contains("enter to complete"),
        "{}",
        app.footer_hint()
    );

    app.handle_key(code(KeyCode::Enter));
    assert_eq!(app.input().text(), "check @README.md");
    assert!(rx.try_recv().is_err(), "the partial path was sent");
    assert!(
        app.input_completion.is_none(),
        "a complete path offers nothing"
    );

    app.handle_key(code(KeyCode::Enter));
    assert!(rx.try_recv().is_ok(), "a completed path sends");
}

#[test]
fn slash_completes_command_names_in_the_prompt() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    for c in "/sw".chars() {
        app.handle_key(key(c));
    }
    let sp = app.input_completion.as_ref().expect("slash completion");
    let values: Vec<&str> = sp.items().iter().map(|i| i.value.as_str()).collect();
    assert!(values.contains(&"swarm"), "{values:?}");
}

#[test]
fn slash_rows_carry_the_argument_hint_beside_the_description() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    for c in "/sw".chars() {
        app.handle_key(key(c));
    }
    let sp = app.input_completion.as_ref().expect("slash completion");
    let swarm = sp
        .items()
        .iter()
        .find(|i| i.value == "swarm")
        .expect("swarm offered");
    let detail = swarm.detail.as_deref().expect("swarm detail");
    assert!(detail.starts_with("[on|off|<task>]"), "{detail}");
    assert!(detail.contains("turn swarm mode on or off"), "{detail}");
}

#[test]
fn slash_completion_with_no_args_offers_everything_then_names_filter() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.handle_key(key('/'));
    let all = app
        .input_completion
        .as_ref()
        .expect("bare slash completes")
        .items()
        .len();
    assert!(all > 3, "{all}");
    for c in "mod".chars() {
        app.handle_key(key(c));
    }
    let first = app
        .input_completion
        .as_ref()
        .expect("slash completion")
        .items()
        .first()
        .cloned()
        .unwrap();
    assert_eq!(first.value, "model");
    assert_eq!(first.label, "model");
    assert!(first.detail.as_deref().is_some_and(|d| !d.is_empty()));
}

#[test]
fn slash_argument_positions_complete_against_the_spec() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    for c in "/mouse ".chars() {
        app.handle_key(key(c));
    }
    let sp = app.input_completion.as_ref().expect("mouse arg completion");
    let values: Vec<&str> = sp.items().iter().map(|i| i.value.as_str()).collect();
    assert!(
        values.iter().all(|v| ["on", "off", "toggle"].contains(v)),
        "{values:?}"
    );
}

#[test]
fn enter_accepts_a_slash_completion_then_the_next_enter_sends() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_model_choices(vec![PickItem::simple("fake:m")]);
    for c in "/mod".chars() {
        app.handle_key(key(c));
    }
    assert!(app.input_completion.is_some());
    app.handle_key(code(KeyCode::Enter));
    assert_eq!(app.input().text(), "/model");
    assert!(rx.try_recv().is_err(), "the bare command was sent");

    app.handle_key(code(KeyCode::Enter));
    assert!(app.picker.is_some(), "/model opens the model picker");
}

#[test]
fn slash_completion_stops_once_arguments_begin() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    for c in "/swarm on".chars() {
        app.handle_key(key(c));
    }
    assert!(app.input_completion.is_none());
}
