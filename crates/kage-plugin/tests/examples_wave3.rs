//! Wave-3 behavior tests for the bundled Lua examples (plans/verify/27).
//!
//! Companion to `examples.rs`, kept in its own file so each example
//! suite stays independently owned. Pins the demo-card payload shape
//! (F1), the rewind exec-failure tolerance (F2), redo surviving turns
//! (F3), the honest rewind scope on a scratch git repo (F4), and the
//! sorted `ui_extras` completions (F10).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use kage_plugin::{
    BridgePrep, BridgeStep, CommandOutput, HostLog, LogLevel, PendingSessionOp, PluginRuntime,
    SharedHostLog,
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

fn load_example(rt: &PluginRuntime, file: &str, plugin: &str) {
    let path = examples_dir().join(file);
    let source = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {file}: {e}"));
    rt.eval_plugin(plugin, &source)
        .unwrap_or_else(|e| panic!("{file} loads: {e}"));
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

/// A runtime granting `session_write` and `exec` to the `rewind`
/// plugin, anchored at `workdir` when given.
fn rewind_runtime(sink: SharedHostLog, workdir: Option<PathBuf>) -> PluginRuntime {
    let mut capabilities = BTreeMap::new();
    capabilities.insert(
        "rewind".to_owned(),
        vec!["session_write".to_owned(), "exec".to_owned()],
    );
    let builder = PluginRuntime::builder()
        .sink(sink)
        .capabilities(capabilities);
    match workdir {
        Some(dir) => builder.workdir(dir).build(),
        None => builder.build(),
    }
    .unwrap()
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

/// A git work tree with one commit of `tracked.txt`, so `git stash
/// create` has a base to diff against.
fn init_git_repo(dir: &Path, content: &str) {
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
    std::fs::write(dir.join("tracked.txt"), content).unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "init"]);
}

fn rewind_entries() -> Vec<serde_json::Value> {
    vec![
        json!({ "id": "01H0", "kind": "header", "ts": "2026-05-19T10:00:00+00:00" }),
        json!({ "id": "01U1", "kind": "message", "role": "user", "ts": "2026-05-19T10:00:01+00:00" }),
        json!({ "id": "01A1", "kind": "message", "role": "assistant", "ts": "2026-05-19T10:00:02+00:00" }),
    ]
}

#[test]
fn card_entry_payload_matches_the_field_the_renderer_reads() {
    let (rec, sink) = forwarding_sink();
    let mut capabilities = BTreeMap::new();
    capabilities.insert(
        "block_renderer_demo".to_owned(),
        vec!["session_write".to_owned()],
    );
    let rt = PluginRuntime::builder()
        .sink(sink)
        .capabilities(capabilities)
        .build()
        .unwrap();
    load_example(&rt, "block_renderer_demo.lua", "block_renderer_demo");

    let cmd = rt
        .registered_commands()
        .into_iter()
        .find(|c| c.name() == "card")
        .expect("card command registered");
    let bargs = match cmd.prepare_bridge("hello", &json!(null)).unwrap() {
        BridgePrep::Ready(bargs) => bargs,
        BridgePrep::ArgError(out) => panic!("unexpected arg error: {}", out.text),
    };
    match rt.bridge_call(&bargs.handler, &bargs.args).unwrap() {
        BridgeStep::Done(v) => {
            assert_eq!(CommandOutput::from_json(&v).text, "rendered card: hello");
        }
        BridgeStep::Suspended(_) => panic!("card with a title must not suspend"),
    }
    let ops = rt.take_pending_session_ops();
    assert_eq!(ops.len(), 1);
    let data = match &ops[0] {
        PendingSessionOp::AppendCustom { kind, data } => {
            assert_eq!(kind, "demo:card");
            data.clone()
        }
        other @ PendingSessionOp::SetLabel { .. } => {
            panic!("expected AppendCustom, got {other:?}")
        }
    };
    assert_eq!(data["title"], "hello");

    let renderers = rt.registered_block_renderers();
    assert_eq!(renderers.len(), 1);

    // The stored entry shape paints through the renderer's `title`
    // branch.
    let mut stored = data.clone();
    stored["kind"] = json!("demo:card");
    stored["width"] = json!(40);
    let lines = renderers[0].render(&stored).expect("idle runtime renders");
    assert_eq!(lines[1].spans[1].text, "hello");

    // The host custom-block payload shape (`text`, what a future
    // bridge delivers) paints through the renderer's `text` branch.
    let bridged = json!({ "kind": "demo:card", "text": "hello", "width": 40 });
    let lines = renderers[0].render(&bridged).expect("idle runtime renders");
    assert_eq!(lines[1].spans[1].text, "hello");

    let r = rec.lock().unwrap();
    assert!(r.errors.is_empty(), "no plugin errors: {:?}", r.errors);
}

#[test]
fn rewind_survives_a_raising_exec_with_one_warning() {
    let (rec, sink) = forwarding_sink();
    let rt = rewind_runtime(sink, None);
    load_example(&rt, "rewind.lua", "rewind");
    rt.eval_plugin(
        "rewind",
        "kage.exec = function() error('kage.exec: spawn git: gone') end",
    )
    .unwrap();

    let mut entries = rewind_entries();
    entries.extend([
        json!({ "id": "01U2", "kind": "message", "role": "user", "ts": "2026-05-19T10:00:03+00:00" }),
        json!({ "id": "01A2", "kind": "message", "role": "assistant", "ts": "2026-05-19T10:00:04+00:00" }),
    ]);
    rt.set_session_entries(entries);

    rt.dispatch_event("turn_end", &json!({})).unwrap();
    let out = run_plain_command(&rt, "undo");
    assert!(out.starts_with("undone to 01A1"), "undo said {out:?}");

    let r = rec.lock().unwrap();
    assert!(r.errors.is_empty(), "no error logs: {:?}", r.errors);
    let warns = r
        .notifies
        .iter()
        .filter(|s| s.contains("git unavailable"))
        .count();
    assert_eq!(warns, 1, "warn once, got {:?}", r.notifies);
}

#[test]
fn redo_survives_a_turn_between_undo_and_redo() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path(), "v1\n");
    std::fs::write(dir.path().join("tracked.txt"), "v1.5\n").unwrap();

    let (rec, sink) = forwarding_sink();
    let rt = rewind_runtime(sink, Some(dir.path().to_path_buf()));
    load_example(&rt, "rewind.lua", "rewind");

    rt.set_session_entries(rewind_entries());
    rt.dispatch_event("turn_end", &json!({})).unwrap();

    std::fs::write(dir.path().join("tracked.txt"), "v2\n").unwrap();
    let mut entries = rewind_entries();
    entries.extend([
        json!({ "id": "01U2", "kind": "message", "role": "user", "ts": "2026-05-19T10:00:03+00:00" }),
        json!({ "id": "01A2", "kind": "message", "role": "assistant", "ts": "2026-05-19T10:00:04+00:00" }),
    ]);
    rt.set_session_entries(entries);
    rt.dispatch_event("turn_end", &json!({})).unwrap();

    let out = run_plain_command(&rt, "undo");
    assert!(out.starts_with("undone to 01A1"), "undo said {out:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
        "v1.5\n"
    );

    // A prompt lands between /undo and /redo; the redo entry must
    // survive the turn_end.
    rt.dispatch_event("turn_end", &json!({})).unwrap();
    assert_eq!(run_plain_command(&rt, "redo"), "re-applied");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
        "v2\n"
    );

    let r = rec.lock().unwrap();
    assert!(r.errors.is_empty(), "no plugin errors: {:?}", r.errors);
}

#[test]
fn undo_restores_tracked_changes_and_leaves_untracked_files() {
    let dir = tempfile::tempdir().unwrap();
    init_git_repo(dir.path(), "v1\n");
    std::fs::write(dir.path().join("tracked.txt"), "v1.5\n").unwrap();

    let (rec, sink) = forwarding_sink();
    let rt = rewind_runtime(sink, Some(dir.path().to_path_buf()));
    load_example(&rt, "rewind.lua", "rewind");

    rt.set_session_entries(rewind_entries());
    rt.dispatch_event("turn_end", &json!({})).unwrap();

    // The agent edits the tracked file and creates a new one.
    std::fs::write(dir.path().join("tracked.txt"), "v2\n").unwrap();
    std::fs::write(dir.path().join("new.txt"), "added\n").unwrap();
    let mut entries = rewind_entries();
    entries.extend([
        json!({ "id": "01U2", "kind": "message", "role": "user", "ts": "2026-05-19T10:00:03+00:00" }),
        json!({ "id": "01A2", "kind": "message", "role": "assistant", "ts": "2026-05-19T10:00:04+00:00" }),
    ]);
    rt.set_session_entries(entries);
    rt.dispatch_event("turn_end", &json!({})).unwrap();

    let out = run_plain_command(&rt, "undo");
    assert!(out.starts_with("undone to 01A1"), "undo said {out:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
        "v1.5\n",
        "tracked modification restored"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
        "added\n",
        "untracked files are outside the snapshot scope by design"
    );

    let r = rec.lock().unwrap();
    assert!(r.errors.is_empty(), "no plugin errors: {:?}", r.errors);
    assert!(
        r.notifies
            .iter()
            .any(|s| s.contains("tracked modifications + conversation")),
        "honest scope notice, got {:?}",
        r.notifies
    );
    assert!(
        r.notifies
            .iter()
            .all(|s| !s.contains("files + conversation")),
        "no overclaiming notice, got {:?}",
        r.notifies
    );
}

#[test]
fn ui_extras_completions_arrive_sorted() {
    let (_rec, sink) = forwarding_sink();
    let rt = PluginRuntime::builder().sink(sink).build().unwrap();
    let source =
        std::fs::read_to_string(examples_dir().join("ui_extras.lua")).expect("read ui_extras.lua");
    rt.eval(&source).expect("ui_extras.lua loads");

    let providers = rt.registered_autocomplete_providers();
    assert_eq!(providers.len(), 1);
    let got = providers[0].complete(":", ":", 1);
    let values: Vec<_> = got.iter().map(|item| item.value.as_str()).collect();
    assert_eq!(values, [":bug:", ":rocket:", ":tada:"]);
}
