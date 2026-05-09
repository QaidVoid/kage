//! Verify the bundled example plugins load and behave as documented.
//!
//! `plugins/examples/*.lua` ship with the repo so a clean install can copy
//! them to `~/.kage/plugins/`. These tests load each example into a fresh
//! [`PluginRuntime`] and exercise the events/handlers each one declares.

use std::path::PathBuf;

use kage_plugin::{HostLog, LogLevel, PluginRuntime, SharedHostLog};
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
