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
