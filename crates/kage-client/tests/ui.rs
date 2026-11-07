//! The plugin UI a client keeps: attach snapshots, the updates that
//! follow them, commands and key mappings, and the dialogs plugins
//! open. These tests drive a [`Client`] the way a host would, feeding
//! it the frames a kage agent sends over `_kage/ui`.

use kage_client::{Change, Client, Frame, RequestId};
use serde_json::{Value, json};

/// The numeric id requests are sent and answered under.
fn n(id: u64) -> RequestId {
    RequestId::Number(id)
}

/// The plugin UI of a project, as `_kage/ui/attach` answers it.
fn snapshot(generation: u64) -> Value {
    json!({
        "generation": generation,
        "keymap": [
            {"mode": "n", "lhs": "ga", "rhs": {"kind": "command", "line": "theme dark"}}
        ],
        "highlights": {"base": {}, "overrides": {}, "theme": "default", "transparent": false},
        "options": {"tabstop": 4},
        "optionSources": {"tabstop": "default"},
        "commands": [{"name": "themes", "description": "list the themes"}],
        "status": {"branch": "main"},
        "blockKinds": ["diff"],
        "autocomplete": false,
        "terminalHooks": false,
    })
}

/// A `_kage/ui/update` of attachment `attach_id` for `cwd`.
fn ui_update(cwd: &str, attach_id: u64, generation: u64, fields: &Value) -> Frame {
    Frame::Notification {
        method: "_kage/ui/update".to_owned(),
        params: {
            let mut params = json!({"cwd": cwd, "attachId": attach_id, "generation": generation});
            let object = params.as_object_mut().unwrap();
            for (key, value) in fields.as_object().unwrap() {
                object.insert(key.clone(), value.clone());
            }
            params
        },
    }
}

/// A client whose attach to `/srv/proj` answered, so later updates
/// apply on top of the snapshot.
fn attached(generation: u64) -> Client {
    let mut client = Client::new();
    let id = client.ui_attach("/srv/proj", None);
    client.handle(Frame::Success {
        id: n(id),
        result: snapshot(generation),
    });
    let _ = client.take_outgoing();
    client
}

#[test]
fn an_attach_snapshot_fills_the_plugin_ui() {
    let mut client = Client::new();
    let id = client.ui_attach("/srv/proj", None);
    let sent = client.take_outgoing();
    assert!(
        matches!(&sent[..], [Frame::Request { method, .. }] if method == "_kage/ui/attach"),
        "the attach goes out as a request: {sent:#?}"
    );
    let changes = client.handle(Frame::Success {
        id: n(id),
        result: snapshot(1),
    });
    assert_eq!(changes, vec![Change::Ui]);
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.cwd, "/srv/proj");
    assert_eq!(ui.generation, 1);
    assert_eq!(ui.commands.len(), 1);
    assert_eq!(ui.status["branch"], "main");
    assert_eq!(ui.block_kinds, vec!["diff".to_owned()]);
    assert!(!ui.autocomplete);
    assert!(!ui.terminal_hooks);
    assert_eq!(ui.attach_id, id);
}

#[test]
fn an_update_applies_only_its_changed_fields() {
    let mut client = attached(1);
    let changes = client.handle(ui_update(
        "/srv/proj",
        1,
        2,
        &json!({"status": {"branch": "feat"}}),
    ));
    assert_eq!(changes, vec![Change::Ui]);
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.generation, 2);
    assert_eq!(ui.status["branch"], "feat");
    assert_eq!(
        ui.options["tabstop"], 4,
        "a field the update left out stays"
    );
}

#[test]
fn a_stale_update_changes_nothing() {
    let mut client = attached(2);
    let changes = client.handle(ui_update(
        "/srv/proj",
        1,
        2,
        &json!({"status": {"branch": "old"}}),
    ));
    assert!(changes.is_empty());
    assert_eq!(
        client.state().plugin_ui.as_ref().unwrap().status["branch"],
        "main"
    );
}

#[test]
fn an_update_for_another_project_is_dropped() {
    let mut client = attached(1);
    let changes = client.handle(ui_update(
        "/srv/other",
        9,
        9,
        &json!({"status": {"branch": "elsewhere"}}),
    ));
    assert!(changes.is_empty());
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.cwd, "/srv/proj");
    assert_eq!(ui.generation, 1);
}

#[test]
fn an_update_beating_its_snapshot_replays_on_it() {
    let mut client = Client::new();
    let id = client.ui_attach("/srv/proj", None);
    client.handle(ui_update(
        "/srv/proj",
        id,
        2,
        &json!({"status": {"branch": "feat"}}),
    ));
    assert!(client.state().plugin_ui.is_none());
    let changes = client.handle(Frame::Success {
        id: n(id),
        result: snapshot(1),
    });
    assert_eq!(changes, vec![Change::Ui]);
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.generation, 2);
    assert_eq!(ui.status["branch"], "feat");
    assert_eq!(
        ui.options["tabstop"], 4,
        "the snapshot fills what the update left out"
    );
}

#[test]
fn the_old_watchers_last_update_dies_with_the_swap() {
    let mut client = attached(1);
    let swap = client.ui_attach("/srv/other", None);
    client.handle(ui_update(
        "/srv/proj",
        1,
        5,
        &json!({"status": {"branch": "late"}}),
    ));
    assert_eq!(client.state().plugin_ui.as_ref().unwrap().cwd, "/srv/proj");
    assert_eq!(client.state().plugin_ui.as_ref().unwrap().generation, 1);

    client.handle(ui_update(
        "/srv/other",
        swap,
        2,
        &json!({"status": {"branch": "next"}}),
    ));
    let changes = client.handle(Frame::Success {
        id: n(swap),
        result: snapshot(1),
    });
    assert_eq!(changes, vec![Change::Ui]);
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.cwd, "/srv/other");
    assert_eq!(ui.status["branch"], "next");
}

#[test]
fn a_failed_attach_keeps_the_ui_it_holds() {
    let mut client = attached(1);
    let again = client.ui_attach("/srv/other", None);
    let changes = client.handle(Frame::Failure {
        id: n(again),
        error: kage_client::RpcError::new(-32603, "the project's plugins did not load"),
    });
    assert!(matches!(&changes[..], [Change::Failed { .. }]));
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.cwd, "/srv/proj");
    assert_eq!(ui.generation, 1);
}

#[test]
fn an_attach_answer_older_than_one_landed_dies() {
    let mut client = Client::new();
    let first = client.ui_attach("/srv/a", None);
    let second = client.ui_attach("/srv/b", None);
    client.handle(ui_update(
        "/srv/a",
        first,
        1,
        &json!({"status": {"branch": "stale"}}),
    ));
    let changes = client.handle(Frame::Success {
        id: n(second),
        result: snapshot(0),
    });
    assert_eq!(changes, vec![Change::Ui]);
    assert_eq!(client.state().plugin_ui.as_ref().unwrap().cwd, "/srv/b");

    let changes = client.handle(Frame::Success {
        id: n(first),
        result: snapshot(0),
    });
    assert!(changes.is_empty());
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.cwd, "/srv/b");
    assert_eq!(
        ui.status["branch"], "main",
        "the stale attach's update died with it"
    );
}

#[test]
fn a_same_cwd_reattach_routes_updates_by_attachment() {
    let mut client = attached(0);
    let again = client.ui_attach("/srv/proj", None);
    // The old watcher's next update arrives before the answer: the old
    // attachment is still the one held, so it applies.
    let changes = client.handle(ui_update(
        "/srv/proj",
        1,
        1,
        &json!({"status": {"branch": "old"}}),
    ));
    assert_eq!(changes, vec![Change::Ui]);
    client.handle(Frame::Success {
        id: n(again),
        result: snapshot(0),
    });
    // After the new snapshot, the old watcher's update is a stray of an
    // attachment this connection replaced.
    let changes = client.handle(ui_update(
        "/srv/proj",
        1,
        2,
        &json!({"status": {"branch": "stale"}}),
    ));
    assert!(changes.is_empty());
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.generation, 0);
    assert_eq!(ui.status["branch"], "main");
    // The new attachment's first update applies.
    let changes = client.handle(ui_update(
        "/srv/proj",
        again,
        1,
        &json!({"status": {"branch": "fresh"}}),
    ));
    assert_eq!(changes, vec![Change::Ui]);
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.generation, 1);
    assert_eq!(ui.status["branch"], "fresh");
}

/// A replacement pending does not silence the attachment still held:
/// its updates keep applying, and keep applying after the replacement
/// failed, since the host kept its watcher.
#[test]
fn the_held_attachments_updates_apply_while_a_replacement_is_pending() {
    let mut client = attached(1);
    let swap = client.ui_attach("/srv/other", None);
    let changes = client.handle(ui_update(
        "/srv/proj",
        1,
        2,
        &json!({"status": {"branch": "mid"}}),
    ));
    assert_eq!(changes, vec![Change::Ui]);
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.cwd, "/srv/proj");
    assert_eq!(ui.status["branch"], "mid");

    let changes = client.handle(Frame::Failure {
        id: n(swap),
        error: kage_client::RpcError::new(-32603, "the project's plugins did not load"),
    });
    assert!(matches!(&changes[..], [Change::Failed { .. }]));
    let changes = client.handle(ui_update(
        "/srv/proj",
        1,
        3,
        &json!({"status": {"branch": "late"}}),
    ));
    assert_eq!(changes, vec![Change::Ui]);
    assert_eq!(
        client.state().plugin_ui.as_ref().unwrap().status["branch"],
        "late"
    );
}

/// A newer attach whose answer arrives first lands under its id, and
/// its updates apply while the older attach is still pending; the
/// older answer, landing later, cannot replace what landed.
#[test]
fn a_newer_attach_lands_while_an_older_one_is_pending() {
    let mut client = Client::new();
    let first = client.ui_attach("/srv/a", None);
    let second = client.ui_attach("/srv/b", None);
    let changes = client.handle(Frame::Success {
        id: n(second),
        result: snapshot(0),
    });
    assert_eq!(changes, vec![Change::Ui]);
    assert_eq!(client.state().plugin_ui.as_ref().unwrap().cwd, "/srv/b");

    let changes = client.handle(ui_update(
        "/srv/b",
        second,
        1,
        &json!({"status": {"branch": "b1"}}),
    ));
    assert_eq!(changes, vec![Change::Ui]);
    assert_eq!(
        client.state().plugin_ui.as_ref().unwrap().status["branch"],
        "b1"
    );

    let changes = client.handle(Frame::Success {
        id: n(first),
        result: snapshot(0),
    });
    assert!(changes.is_empty());
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.cwd, "/srv/b");
    assert_eq!(ui.status["branch"], "b1");
}

/// A newer attach that failed spent its id without landing, so an
/// older attach that already succeeded still lands when its answer
/// arrives late.
#[test]
fn a_failed_newer_attach_lets_an_older_success_land() {
    let mut client = Client::new();
    let first = client.ui_attach("/srv/a", None);
    let second = client.ui_attach("/srv/b", None);
    let changes = client.handle(Frame::Failure {
        id: n(second),
        error: kage_client::RpcError::new(-32603, "the project's plugins did not load"),
    });
    assert!(matches!(&changes[..], [Change::Failed { .. }]));
    let changes = client.handle(Frame::Success {
        id: n(first),
        result: snapshot(0),
    });
    assert_eq!(changes, vec![Change::Ui]);
    let ui = client.state().plugin_ui.as_ref().unwrap();
    assert_eq!(ui.cwd, "/srv/a");
    assert_eq!(ui.attach_id, first);
}

/// The attach goes out with the attachment id it answers under, and
/// with the session whose runtime it pins when one is named.
#[test]
fn an_attach_request_carries_its_attachment_id_and_session() {
    let mut client = Client::new();
    let id = client.ui_attach("/srv/proj", Some("s1"));
    let sent = client.take_outgoing();
    assert!(
        matches!(&sent[..], [Frame::Request { method, params, .. }]
            if method == "_kage/ui/attach"
                && params["attachId"] == id
                && params["sessionId"] == "s1"
                && params["cwd"] == "/srv/proj"),
        "{sent:#?}"
    );
    let id = client.ui_attach("/srv/proj", None);
    let sent = client.take_outgoing();
    assert!(
        matches!(&sent[..], [Frame::Request { params, .. }]
            if params["attachId"] == id && params.get("sessionId").is_none()),
        "an unbound attach names no session: {sent:#?}"
    );
}

#[test]
fn a_dialog_opens_waits_and_answers_once() {
    let mut client = Client::new();
    let changes = client.handle(Frame::Request {
        id: n(7),
        method: "_kage/ui/dialog".to_owned(),
        params: json!({"kind": "ui.select", "payload": {"title": "theme", "items": ["dark", "light"]}}),
    });
    assert_eq!(changes, vec![Change::Dialog]);
    let ask = &client.state().dialogs[0];
    assert_eq!(ask.request_id, n(7));
    assert_eq!(ask.kind, "ui.select");
    assert_eq!(ask.payload["title"], "theme");

    let changes = client.reply_dialog(n(7), json!("light"));
    assert_eq!(changes, vec![Change::Dialog]);
    assert!(client.state().dialogs.is_empty());
    let sent = client.take_outgoing();
    assert!(
        matches!(&sent[..], [Frame::Success { id, result }] if *id == n(7) && result == "light"),
        "the answer carries the dialog's value: {sent:#?}"
    );

    let changes = client.reply_dialog(n(7), json!("dark"));
    assert!(changes.is_empty());
    assert!(
        client.take_outgoing().is_empty(),
        "a double answer sends nothing"
    );
}

#[test]
fn a_re_raised_dialog_replaces_its_echo() {
    let mut client = Client::new();
    client.handle(Frame::Request {
        id: n(7),
        method: "_kage/ui/dialog".to_owned(),
        params: json!({"kind": "ui.input", "payload": {"title": "name", "text": ""}}),
    });
    client.handle(Frame::Request {
        id: n(7),
        method: "_kage/ui/dialog".to_owned(),
        params: json!({"kind": "ui.input", "payload": {"title": "name", "text": "typed"}}),
    });
    assert_eq!(client.state().dialogs.len(), 1);
    assert_eq!(client.state().dialogs[0].payload["text"], "typed");
}

#[test]
fn an_unreadable_dialog_fails_the_request() {
    let mut client = Client::new();
    let changes = client.handle(Frame::Request {
        id: n(7),
        method: "_kage/ui/dialog".to_owned(),
        params: json!({"payload": {"title": "no kind"}}),
    });
    assert!(changes.is_empty());
    assert!(client.state().dialogs.is_empty());
    let sent = client.take_outgoing();
    assert!(
        matches!(&sent[..], [Frame::Failure { id, .. }] if *id == n(7)),
        "the agent learns the dialog was unreadable: {sent:#?}"
    );
}

#[test]
fn a_cancelled_dialog_leaves_the_queue_without_an_answer() {
    let mut client = Client::new();
    client.handle(Frame::Request {
        id: n(7),
        method: "_kage/ui/dialog".to_owned(),
        params: json!({"kind": "ui.confirm", "payload": {"title": "go on?", "message": "publish"}}),
    });
    let changes = client.handle(Frame::Notification {
        method: "$/cancel_request".to_owned(),
        params: json!({"requestId": 7}),
    });
    assert_eq!(changes, vec![Change::Dialog]);
    assert!(client.state().dialogs.is_empty());
    let changes = client.reply_dialog(n(7), json!(true));
    assert!(changes.is_empty());
    assert!(
        client.take_outgoing().is_empty(),
        "a withdrawn dialog is not answered"
    );
}

#[test]
fn a_command_and_a_mapping_answer_as_output() {
    let mut client = attached(1);
    let _ = client.take_outgoing();

    let command = client.ui_command("/srv/proj", "themes", "dark", Some("s1"));
    let sent = client.take_outgoing();
    assert!(
        matches!(&sent[..], [Frame::Request { method, params, .. }]
            if method == "_kage/ui/command"
            && params["cwd"] == "/srv/proj"
            && params["name"] == "themes"
            && params["args"] == "dark"
            && params["sessionId"] == "s1"),
        "the command goes out with its line: {sent:#?}"
    );
    let changes = client.handle(Frame::Success {
        id: n(command),
        result: json!({"text": "dark, light", "isError": true}),
    });
    assert_eq!(
        changes,
        vec![Change::UiOutput {
            cwd: "/srv/proj".to_owned(),
            output: kage_client::wire::UiOutput {
                text: "dark, light".to_owned(),
                is_error: true,
                structured: None,
            },
        }]
    );

    let mapping = client.ui_keymap("/srv/proj", 3, None);
    let sent = client.take_outgoing();
    assert!(
        matches!(&sent[..], [Frame::Request { method, params, .. }]
            if method == "_kage/ui/keymap" && params["id"] == 3),
        "the mapping goes out with its handler id: {sent:#?}"
    );
    let changes = client.handle(Frame::Success {
        id: n(mapping),
        result: json!({"text": "mapped"}),
    });
    assert!(
        matches!(&changes[..], [Change::UiOutput { cwd, output }]
            if cwd == "/srv/proj" && output.text == "mapped"),
        "the mapping's output lands: {changes:#?}"
    );
}

#[test]
fn a_failed_command_reports_the_error() {
    let mut client = attached(1);
    let command = client.ui_command("/srv/proj", "missing", "", None);
    let changes = client.handle(Frame::Failure {
        id: n(command),
        error: kage_client::RpcError::new(-32602, "no such command: missing"),
    });
    assert_eq!(
        changes,
        vec![Change::Failed {
            request: command,
            error: kage_client::RpcError::new(-32602, "no such command: missing"),
        }]
    );
}
