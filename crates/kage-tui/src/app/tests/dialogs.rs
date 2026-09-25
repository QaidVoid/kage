//! Plugin dialogs: select, confirm, input and editor.

use super::*;

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
