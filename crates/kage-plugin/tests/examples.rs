//! Verify the bundled example plugins load and behave as documented.
//!
//! `plugins/examples/*.lua` ship with the repo so a clean install can copy
//! them to `~/.config/kage/plugins/`. These tests load each example into a fresh
//! [`PluginRuntime`] and exercise the events/handlers each one declares.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use kage_plugin::{
    BridgePrep, BridgeStep, CommandOutput, HostLog, LogLevel, PluginRuntime, SharedHostLog,
    SwitchTarget,
};
use serde_json::json;

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("plugins")
        .join("examples")
}

#[derive(Default)]
struct Recording {
    notifies: Vec<String>,
    errors: Vec<String>,
}

#[derive(Clone)]
struct Forwarder(std::sync::Arc<std::sync::Mutex<Recording>>);

impl HostLog for Forwarder {
    fn notify(&mut self, message: &str) {
        self.0.lock().unwrap().notifies.push(message.to_owned());
    }
    fn log(&mut self, level: LogLevel, message: &str) {
        if level == LogLevel::Error {
            self.0.lock().unwrap().errors.push(message.to_owned());
        }
    }
}

fn forwarding_sink() -> (std::sync::Arc<std::sync::Mutex<Recording>>, SharedHostLog) {
    let rec = std::sync::Arc::new(std::sync::Mutex::new(Recording::default()));
    let sink: SharedHostLog = std::sync::Arc::new(std::sync::Mutex::new(Box::new(Forwarder(
        rec.clone(),
    ))
        as Box<dyn HostLog + Send>));
    (rec, sink)
}

#[test]
fn tps_example_emits_summary_on_agent_end() {
    let (rec, sink) = forwarding_sink();
    let rt = PluginRuntime::builder().sink(sink).build().unwrap();
    let path = examples_dir().join("tps.lua");
    let source = std::fs::read_to_string(&path).expect("read tps.lua");
    rt.eval(&source).expect("tps.lua loads");

    rt.dispatch_event("agent_start", &json!({})).unwrap();
    rt.dispatch_event(
        "message_end",
        &json!({"usage": {"input": 100, "output": 50}}),
    )
    .unwrap();
    rt.dispatch_event(
        "message_end",
        &json!({"usage": {"input": 100, "output": 25}}),
    )
    .unwrap();
    // Force at least one millisecond of elapsed time so the throughput
    // formula stays in the "real elapsed" branch.
    std::thread::sleep(std::time::Duration::from_millis(5));
    rt.dispatch_event("agent_end", &json!({})).unwrap();

    let r = rec.lock().unwrap();
    assert!(r.errors.is_empty(), "no plugin errors: {:?}", r.errors);
    let summary = r
        .notifies
        .iter()
        .find(|s| s.starts_with("tps:"))
        .expect("tps summary fired");
    assert!(summary.contains("75 tokens"), "summary text: {summary}");
    assert!(summary.contains("tok/s"), "summary text: {summary}");
}

#[test]
fn transform_demo_redacts_secrets_in_user_text() {
    let (rec, sink) = forwarding_sink();
    let rt = PluginRuntime::builder().sink(sink).build().unwrap();
    let source = std::fs::read_to_string(examples_dir().join("transform_demo.lua"))
        .expect("read transform_demo.lua");
    rt.eval(&source).expect("transform_demo.lua loads");

    let history = json!([
        {
            "role": "user",
            "id": "01M1",
            "parent": null,
            "ts": "2026-05-19T10:00:00+00:00",
            "content": [{"type": "text", "text": "use sk-test-abc123 and Bearer xyz_456"}]
        },
        {
            "role": "assistant",
            "id": "01M2",
            "parent": null,
            "ts": "2026-05-19T10:00:01+00:00",
            "content": [{"type": "text", "text": "ignore assistant sk-leak"}]
        }
    ]);
    let out = rt.dispatch_transform("transform_context", history).unwrap();
    let user_text = out[0]["content"][0]["text"].as_str().unwrap();
    assert!(
        user_text.contains("[redacted]") && !user_text.contains("sk-test-abc123"),
        "user text should be scrubbed, got {user_text:?}"
    );
    assert!(
        !user_text.contains("xyz_456"),
        "bearer not scrubbed: {user_text:?}"
    );
    let assistant_text = out[1]["content"][0]["text"].as_str().unwrap();
    assert!(
        assistant_text.contains("sk-leak"),
        "assistant text must be left alone, got {assistant_text:?}"
    );
    let r = rec.lock().unwrap();
    assert!(r.errors.is_empty(), "no plugin errors: {:?}", r.errors);
    assert!(
        r.notifies.iter().any(|s| s.starts_with("redact: scrubbed")),
        "got {:?}",
        r.notifies
    );
}

#[test]
fn transform_demo_appends_date_to_system_prompt() {
    let (_rec, sink) = forwarding_sink();
    let rt = PluginRuntime::builder().sink(sink).build().unwrap();
    let source = std::fs::read_to_string(examples_dir().join("transform_demo.lua")).unwrap();
    rt.eval(&source).unwrap();

    let req = json!({
        "model": "mock:m",
        "messages": [],
        "system": "you are helpful"
    });
    let out = rt
        .dispatch_transform("before_provider_request", req)
        .unwrap();
    let system = out["system"].as_str().unwrap();
    assert!(system.starts_with("you are helpful"), "got {system:?}");
    assert!(
        system.contains("Today is "),
        "missing date stamp in {system:?}"
    );

    let out2 = rt
        .dispatch_transform("before_provider_request", out)
        .unwrap();
    let system2 = out2["system"].as_str().unwrap();
    assert_eq!(
        system2.matches("Today is ").count(),
        1,
        "running the hook twice must not append the stamp twice: {system2:?}"
    );
}

#[test]
fn git_status_announces_branch_on_agent_start() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().join(".git");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature-x\n").unwrap();

    let (rec, sink) = forwarding_sink();
    let rt = PluginRuntime::builder()
        .sink(sink)
        .workdir(dir.path().to_path_buf())
        .build()
        .unwrap();
    let path = examples_dir().join("git-status.lua");
    let source = std::fs::read_to_string(&path).expect("read git-status.lua");
    rt.eval(&source).expect("git-status.lua loads");

    rt.dispatch_event("agent_start", &json!({})).unwrap();

    let r = rec.lock().unwrap();
    assert!(r.errors.is_empty(), "no plugin errors: {:?}", r.errors);
    assert!(
        r.notifies.iter().any(|s| s == "git: feature-x"),
        "expected branch notify, got {:?}",
        r.notifies
    );
}

#[test]
fn git_status_handles_detached_head() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().join(".git");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(
        git_dir.join("HEAD"),
        "abc1234567890def1234567890abcdef12345678\n",
    )
    .unwrap();

    let (rec, sink) = forwarding_sink();
    let rt = PluginRuntime::builder()
        .sink(sink)
        .workdir(dir.path().to_path_buf())
        .build()
        .unwrap();
    let source = std::fs::read_to_string(examples_dir().join("git-status.lua")).unwrap();
    rt.eval(&source).unwrap();
    rt.dispatch_event("agent_start", &json!({})).unwrap();

    let r = rec.lock().unwrap();
    assert!(
        r.notifies
            .iter()
            .any(|s| s.starts_with("git: abc1234") && s.contains("detached")),
        "got {:?}",
        r.notifies
    );
}

#[test]
fn git_status_reports_not_a_repo_when_head_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (rec, sink) = forwarding_sink();
    let rt = PluginRuntime::builder()
        .sink(sink)
        .workdir(dir.path().to_path_buf())
        .build()
        .unwrap();
    let source = std::fs::read_to_string(examples_dir().join("git-status.lua")).unwrap();
    rt.eval(&source).unwrap();
    rt.dispatch_event("agent_start", &json!({})).unwrap();

    let r = rec.lock().unwrap();
    assert!(
        r.notifies.iter().any(|s| s == "git: not a repo"),
        "got {:?}",
        r.notifies
    );
}

fn load_select_demo(sink: SharedHostLog) -> PluginRuntime {
    let rt = PluginRuntime::builder().sink(sink).build().unwrap();
    let source = std::fs::read_to_string(examples_dir().join("select_demo.lua"))
        .expect("read select_demo.lua");
    rt.eval(&source).expect("select_demo.lua loads");
    rt
}

fn pick_color_args(rt: &PluginRuntime) -> kage_plugin::BridgeArgs {
    let cmd = rt
        .registered_commands()
        .into_iter()
        .find(|c| c.name() == "pick-color")
        .expect("pick-color command registered");
    match cmd.prepare_bridge("", &json!(null)).unwrap() {
        BridgePrep::Ready(bargs) => bargs,
        BridgePrep::ArgError(out) => panic!("unexpected arg error: {}", out.text),
    }
}

#[test]
fn select_demo_suspends_then_returns_the_resumed_color() {
    let (rec, sink) = forwarding_sink();
    let rt = load_select_demo(sink);
    let bargs = pick_color_args(&rt);

    let step = rt.bridge_call(&bargs.handler, &bargs.args).unwrap();
    match step {
        BridgeStep::Suspended(req) => {
            assert_eq!(req.kind, "ui.select");
            assert_eq!(
                req.payload,
                json!({ "title": "Pick a color", "items": ["red", "green", "blue"] })
            );
        }
        BridgeStep::Done(v) => panic!("expected suspend, got Done({v})"),
    }

    let done = rt.bridge_resume(&json!("green")).unwrap();
    let BridgeStep::Done(value) = done else {
        panic!("expected Done after resume");
    };
    assert_eq!(CommandOutput::from_json(&value).text, "you picked green");
    let r = rec.lock().unwrap();
    assert!(r.errors.is_empty(), "no plugin errors: {:?}", r.errors);
    assert!(
        r.notifies.iter().any(|s| s == "pick-color: green"),
        "got {:?}",
        r.notifies
    );
}

#[test]
fn select_demo_cancel_path_returns_cancelled() {
    let (_rec, sink) = forwarding_sink();
    let rt = load_select_demo(sink);
    let bargs = pick_color_args(&rt);

    assert!(matches!(
        rt.bridge_call(&bargs.handler, &bargs.args).unwrap(),
        BridgeStep::Suspended(_)
    ));
    let BridgeStep::Done(value) = rt.bridge_cancel().unwrap() else {
        panic!("expected Done after cancel");
    };
    assert_eq!(CommandOutput::from_json(&value).text, "cancelled");
}

fn confirm_delete_args(rt: &PluginRuntime) -> kage_plugin::BridgeArgs {
    let cmd = rt
        .registered_commands()
        .into_iter()
        .find(|c| c.name() == "confirm-delete")
        .expect("confirm-delete command registered");
    match cmd.prepare_bridge("", &json!(null)).unwrap() {
        BridgePrep::Ready(bargs) => bargs,
        BridgePrep::ArgError(out) => panic!("unexpected arg error: {}", out.text),
    }
}

#[test]
fn select_demo_confirm_true_path_deletes() {
    let (rec, sink) = forwarding_sink();
    let rt = load_select_demo(sink);
    let bargs = confirm_delete_args(&rt);

    match rt.bridge_call(&bargs.handler, &bargs.args).unwrap() {
        BridgeStep::Suspended(req) => {
            assert_eq!(req.kind, "ui.confirm");
            assert_eq!(
                req.payload,
                json!({ "title": "Delete everything?", "message": "This cannot be undone." })
            );
        }
        BridgeStep::Done(v) => panic!("expected suspend, got Done({v})"),
    }

    let BridgeStep::Done(value) = rt.bridge_resume(&json!(true)).unwrap() else {
        panic!("expected Done after resume");
    };
    assert_eq!(CommandOutput::from_json(&value).text, "deleting");
    let r = rec.lock().unwrap();
    assert!(
        r.notifies.iter().any(|s| s == "confirm-delete: confirmed"),
        "got {:?}",
        r.notifies
    );
}

#[test]
fn select_demo_confirm_false_path_keeps() {
    let (_rec, sink) = forwarding_sink();
    let rt = load_select_demo(sink);
    let bargs = confirm_delete_args(&rt);

    assert!(matches!(
        rt.bridge_call(&bargs.handler, &bargs.args).unwrap(),
        BridgeStep::Suspended(_)
    ));
    let BridgeStep::Done(value) = rt.bridge_resume(&json!(false)).unwrap() else {
        panic!("expected Done after resume");
    };
    assert_eq!(CommandOutput::from_json(&value).text, "kept");
}

fn ask_name_args(rt: &PluginRuntime) -> kage_plugin::BridgeArgs {
    let cmd = rt
        .registered_commands()
        .into_iter()
        .find(|c| c.name() == "ask-name")
        .expect("ask-name command registered");
    match cmd.prepare_bridge("", &json!(null)).unwrap() {
        BridgePrep::Ready(bargs) => bargs,
        BridgePrep::ArgError(out) => panic!("unexpected arg error: {}", out.text),
    }
}

#[test]
fn select_demo_input_returns_greeting() {
    let (rec, sink) = forwarding_sink();
    let rt = load_select_demo(sink);
    let bargs = ask_name_args(&rt);

    match rt.bridge_call(&bargs.handler, &bargs.args).unwrap() {
        BridgeStep::Suspended(req) => {
            assert_eq!(req.kind, "ui.input");
            assert_eq!(
                req.payload,
                json!({ "title": "What is your name?", "placeholder": "e.g. Ada" })
            );
        }
        BridgeStep::Done(v) => panic!("expected suspend, got Done({v})"),
    }

    let BridgeStep::Done(value) = rt.bridge_resume(&json!("Ada")).unwrap() else {
        panic!("expected Done after resume");
    };
    assert_eq!(CommandOutput::from_json(&value).text, "hello Ada");
    let r = rec.lock().unwrap();
    assert!(
        r.notifies.iter().any(|s| s == "ask-name: Ada"),
        "got {:?}",
        r.notifies
    );
}

#[test]
fn select_demo_input_cancel_is_anonymous() {
    let (_rec, sink) = forwarding_sink();
    let rt = load_select_demo(sink);
    let bargs = ask_name_args(&rt);

    assert!(matches!(
        rt.bridge_call(&bargs.handler, &bargs.args).unwrap(),
        BridgeStep::Suspended(_)
    ));
    let BridgeStep::Done(value) = rt.bridge_cancel().unwrap() else {
        panic!("expected Done after cancel");
    };
    assert_eq!(CommandOutput::from_json(&value).text, "anonymous");
}

fn compose_note_args(rt: &PluginRuntime) -> kage_plugin::BridgeArgs {
    let cmd = rt
        .registered_commands()
        .into_iter()
        .find(|c| c.name() == "compose-note")
        .expect("compose-note command registered");
    match cmd.prepare_bridge("", &json!(null)).unwrap() {
        BridgePrep::Ready(bargs) => bargs,
        BridgePrep::ArgError(out) => panic!("unexpected arg error: {}", out.text),
    }
}

#[test]
fn select_demo_editor_returns_saved_note() {
    let (rec, sink) = forwarding_sink();
    let rt = load_select_demo(sink);
    let bargs = compose_note_args(&rt);

    match rt.bridge_call(&bargs.handler, &bargs.args).unwrap() {
        BridgeStep::Suspended(req) => {
            assert_eq!(req.kind, "ui.editor");
            assert_eq!(
                req.payload,
                json!({ "title": "Compose a note", "prefill": "TODO: " })
            );
        }
        BridgeStep::Done(v) => panic!("expected suspend, got Done({v})"),
    }

    let BridgeStep::Done(value) = rt.bridge_resume(&json!("TODO: ship it")).unwrap() else {
        panic!("expected Done after resume");
    };
    assert_eq!(CommandOutput::from_json(&value).text, "TODO: ship it");
    let r = rec.lock().unwrap();
    assert!(
        r.notifies
            .iter()
            .any(|s| s == "compose-note: saved 13 chars"),
        "got {:?}",
        r.notifies
    );
}

#[test]
fn select_demo_editor_cancel_is_discarded() {
    let (_rec, sink) = forwarding_sink();
    let rt = load_select_demo(sink);
    let bargs = compose_note_args(&rt);

    assert!(matches!(
        rt.bridge_call(&bargs.handler, &bargs.args).unwrap(),
        BridgeStep::Suspended(_)
    ));
    let BridgeStep::Done(value) = rt.bridge_cancel().unwrap() else {
        panic!("expected Done after cancel");
    };
    assert_eq!(CommandOutput::from_json(&value).text, "discarded");
}

#[test]
fn select_demo_keybinding_drives_a_dialog() {
    let (rec, sink) = forwarding_sink();
    let rt = load_select_demo(sink);

    let bindings = rt.registered_keybindings();
    let kb = bindings
        .iter()
        .find(|kb| kb.chord() == "ctrl+alt+k")
        .expect("ctrl+alt+k registered");
    assert_eq!(kb.description(), "Quick color pick");

    let handler = kb.handler().unwrap();
    match rt.bridge_call(&handler, &[]).unwrap() {
        BridgeStep::Suspended(req) => assert_eq!(req.kind, "ui.select"),
        BridgeStep::Done(v) => panic!("expected suspend, got Done({v})"),
    }
    let BridgeStep::Done(value) = rt.bridge_resume(&json!("green")).unwrap() else {
        panic!("expected Done after resume");
    };
    assert_eq!(CommandOutput::from_json(&value).text, "green");
    let r = rec.lock().unwrap();
    assert!(
        r.notifies.iter().any(|s| s == "quick-pick: green"),
        "got {:?}",
        r.notifies
    );
}

#[test]
fn ui_extras_registers_chrome_autocomplete_and_raw_input() {
    let (_rec, sink) = forwarding_sink();
    let rt = PluginRuntime::builder().sink(sink).build().unwrap();
    let source =
        std::fs::read_to_string(examples_dir().join("ui_extras.lua")).expect("read ui_extras.lua");
    rt.eval(&source).expect("ui_extras.lua loads");

    // 1. Header / footer chrome both registered and renderable.
    let header = rt.header_chrome().expect("header registered");
    let footer = rt.footer_chrome().expect("footer registered");
    assert!(!header.render(80).is_empty());
    assert!(!footer.render(80).is_empty());

    // 2. Autocomplete provider answers only on a ":" trigger.
    let providers = rt.registered_autocomplete_providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].name(), "emoji");
    let hit = providers[0].complete(":t", ":t", 2);
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].value, ":tada:");
    assert!(providers[0].complete("plain", "plain", 5).is_empty());

    // 3. Raw-input observer is registered and never consumes.
    let hooks = rt.registered_terminal_hooks();
    assert_eq!(hooks.len(), 1);
    assert!(!hooks[0].handle(&json!({
        "code": "char", "char": "x", "ctrl": true, "alt": false, "shift": false
    })));

    // The runtime exposed an off-switch command.
    assert!(
        rt.registered_commands()
            .iter()
            .any(|c| c.name() == "ui-extras-off")
    );
}

/// Initialize a git work tree with one commit so `git stash create`
/// has a base to diff against, then leave a tracked file dirty.
fn init_git_repo(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?} failed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "Tester"]);
    std::fs::write(dir.join("tracked.txt"), "v1\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "init"]);
    std::fs::write(dir.join("tracked.txt"), "v2\n").unwrap();
}

fn run_status_command(rt: &PluginRuntime) -> String {
    let cmd = rt
        .registered_commands()
        .into_iter()
        .find(|c| c.name() == "rewind-status")
        .expect("rewind-status registered");
    let bargs = match cmd.prepare_bridge("", &json!(null)).unwrap() {
        BridgePrep::Ready(bargs) => bargs,
        BridgePrep::ArgError(out) => panic!("unexpected arg error: {}", out.text),
    };
    match rt.bridge_call(&bargs.handler, &bargs.args).unwrap() {
        BridgeStep::Done(v) => CommandOutput::from_json(&v).text,
        BridgeStep::Suspended(_) => panic!("rewind-status should not suspend"),
    }
}

#[test]
fn rewind_records_a_checkpoint_per_turn_when_granted() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path());

    let (rec, sink) = forwarding_sink();
    let mut caps = BTreeMap::new();
    caps.insert(
        "rewind".to_owned(),
        vec!["session_write".to_owned(), "exec".to_owned()],
    );
    let rt = PluginRuntime::builder()
        .sink(sink)
        .workdir(dir.path().to_path_buf())
        .capabilities(caps)
        .build()
        .unwrap();
    let source =
        std::fs::read_to_string(examples_dir().join("rewind.lua")).expect("read rewind.lua");
    rt.eval_plugin("rewind", &source).expect("rewind.lua loads");

    rt.set_session_entries(vec![
        json!({ "id": "01A", "kind": "message", "role": "user", "ts": "2026-05-19T10:00:00+00:00" }),
        json!({ "id": "01B", "kind": "message", "role": "assistant", "ts": "2026-05-19T10:00:05+00:00" }),
    ]);
    assert!(run_status_command(&rt).contains("0 checkpoint"));

    rt.dispatch_event("turn_end", &json!({})).unwrap();

    let r = rec.lock().unwrap();
    assert!(r.errors.is_empty(), "no plugin errors: {:?}", r.errors);
    drop(r);
    let status = run_status_command(&rt);
    assert!(
        status.contains("1 checkpoint") && status.contains("files=true"),
        "status was {status:?}"
    );
}

#[test]
fn rewind_disables_itself_without_session_write() {
    let (rec, sink) = forwarding_sink();
    let rt = PluginRuntime::builder().sink(sink).build().unwrap();
    let source =
        std::fs::read_to_string(examples_dir().join("rewind.lua")).expect("read rewind.lua");
    rt.eval_plugin("rewind", &source).expect("rewind.lua loads");

    assert!(
        !rt.registered_commands()
            .iter()
            .any(|c| c.name() == "rewind"),
        "rewind must register no commands without session_write"
    );
    let r = rec.lock().unwrap();
    assert!(
        r.notifies.iter().any(|s| s.contains("rewind: disabled")),
        "expected a disabled notice, got {:?}",
        r.notifies
    );
}

fn run_plain_command(rt: &PluginRuntime, name: &str) -> String {
    let cmd = rt
        .registered_commands()
        .into_iter()
        .find(|c| c.name() == name)
        .unwrap_or_else(|| panic!("{name} registered"));
    let bargs = match cmd.prepare_bridge("", &json!(null)).unwrap() {
        BridgePrep::Ready(bargs) => bargs,
        BridgePrep::ArgError(out) => panic!("unexpected arg error: {}", out.text),
    };
    match rt.bridge_call(&bargs.handler, &bargs.args).unwrap() {
        BridgeStep::Done(v) => CommandOutput::from_json(&v).text,
        BridgeStep::Suspended(_) => panic!("{name} should not suspend"),
    }
}

#[test]
fn undo_forks_back_before_the_last_user_prompt() {
    let (rec, sink) = forwarding_sink();
    let mut caps = BTreeMap::new();
    // session_write only: /undo exercises the conversation path
    // without needing a git work tree.
    caps.insert("rewind".to_owned(), vec!["session_write".to_owned()]);
    let rt = PluginRuntime::builder()
        .sink(sink)
        .capabilities(caps)
        .build()
        .unwrap();
    let source =
        std::fs::read_to_string(examples_dir().join("rewind.lua")).expect("read rewind.lua");
    rt.eval_plugin("rewind", &source).expect("rewind.lua loads");

    rt.set_session_entries(vec![
        json!({ "id": "01H0", "kind": "header", "ts": "2026-05-19T10:00:00+00:00" }),
        json!({ "id": "01U1", "kind": "message", "role": "user", "ts": "2026-05-19T10:00:01+00:00" }),
        json!({ "id": "01A1", "kind": "message", "role": "assistant", "ts": "2026-05-19T10:00:02+00:00" }),
        json!({ "id": "01U2", "kind": "message", "role": "user", "ts": "2026-05-19T10:00:03+00:00" }),
        json!({ "id": "01A2", "kind": "message", "role": "assistant", "ts": "2026-05-19T10:00:04+00:00" }),
    ]);

    let out = run_plain_command(&rt, "undo");
    assert!(out.starts_with("undone to 01A1"), "command said {out:?}");
    // The last exchange (01U2 + 01A2) is dropped: the fork lands on
    // the entry just before the most recent user message.
    assert_eq!(
        rt.take_switch_request(),
        Some(SwitchTarget::PendingFork("01A1".to_owned()))
    );
    let r = rec.lock().unwrap();
    assert!(
        r.notifies
            .iter()
            .any(|s| s.contains("undo: reverted last exchange")),
        "expected an undo notice, got {:?}",
        r.notifies
    );
}

#[test]
fn undo_with_only_one_exchange_has_nothing_to_undo() {
    let (_rec, sink) = forwarding_sink();
    let mut caps = BTreeMap::new();
    caps.insert("rewind".to_owned(), vec!["session_write".to_owned()]);
    let rt = PluginRuntime::builder()
        .sink(sink)
        .capabilities(caps)
        .build()
        .unwrap();
    let source =
        std::fs::read_to_string(examples_dir().join("rewind.lua")).expect("read rewind.lua");
    rt.eval_plugin("rewind", &source).expect("rewind.lua loads");

    rt.set_session_entries(vec![
        json!({ "id": "01H0", "kind": "header", "ts": "2026-05-19T10:00:00+00:00" }),
        json!({ "id": "01U1", "kind": "message", "role": "user", "ts": "2026-05-19T10:00:01+00:00" }),
        json!({ "id": "01A1", "kind": "message", "role": "assistant", "ts": "2026-05-19T10:00:02+00:00" }),
    ]);

    assert_eq!(run_plain_command(&rt, "undo"), "nothing to undo");
    assert_eq!(rt.take_switch_request(), None);
}
