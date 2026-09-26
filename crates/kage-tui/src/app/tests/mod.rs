//! Integration tests for the App event and render loop.

use std::sync::mpsc;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use kage_core::config::KeybindingsConfig;
use kage_core::keymap::Keymap;

use super::*;
use crate::events::shared_buffer;

mod agents;
mod approvals;
mod bindings;
mod commands;
mod completion;
mod dialogs;
mod edits;
mod mcp;
mod mouse;
mod pickers;
mod plugins;
mod prompts;
mod screen;
mod scroll;
mod search;

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

fn last_block_text(buffer: &SharedBuffer) -> String {
    match buffer.lock().unwrap().blocks().last() {
        Some(crate::buffer::Block::Custom { text, .. }) => text.clone(),
        other => panic!("expected a custom block, got {other:?}"),
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

fn shell_start(id: &str) -> kage_core::protocol::Event {
    kage_core::LoopEvent::ToolCallStart {
        id: kage_core::ToolCallId::new(id),
        name: "shell".into(),
        input_partial: serde_json::json!({ "command": "ls" }),
    }
    .into()
}

fn permission_request(id: &str, request: u64) -> kage_core::protocol::Event {
    kage_core::protocol::HostEvent::PermissionRequested {
        request_id: kage_core::protocol::RequestId(request),
        tool_call_id: Some(kage_core::ToolCallId::new(id)),
        tool: "shell".into(),
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

/// Render `app` into a fresh 60x10 terminal.
fn render_app(app: &mut App) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
    app.render_into(&mut terminal).unwrap();
    terminal
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

fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        app.handle_key(key(c));
    }
}

fn defaults_app() -> App {
    let (tx, _rx) = mpsc::channel();
    app_with_defaults(shared_buffer(), tx)
}

fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

/// Send `batch` as envelopes of `session` and drain them.
fn send_to(
    app: &mut App,
    events: &mpsc::Sender<kage_core::protocol::Envelope>,
    session: kage_core::SessionId,
    batch: Vec<kage_core::protocol::Event>,
) {
    for (seq, event) in batch.into_iter().enumerate() {
        events
            .send(envelope(session, seq as u64 + 1, event))
            .unwrap();
    }
    app.drain_engine_events();
}

/// Run the main session's `agent` call `call` and announce the agent it
/// starts, which has not run yet. Returns the agent's session.
fn spawn_agent(
    app: &mut App,
    events: &mpsc::Sender<kage_core::protocol::Envelope>,
    call: &str,
    agent: &str,
) -> kage_core::SessionId {
    let input = serde_json::json!({
        "agent": agent,
        "description": format!("{agent} task"),
        "prompt": "go",
    });
    feed(
        app,
        events,
        vec![
            kage_core::LoopEvent::ToolCallStart {
                id: kage_core::ToolCallId::new(call),
                name: "agent".into(),
                input_partial: input,
            }
            .into(),
            kage_core::LoopEvent::ToolExecutionStart {
                id: kage_core::ToolCallId::new(call),
            }
            .into(),
        ],
    );
    let child = kage_core::SessionId::new();
    let spawned = kage_core::protocol::HostEvent::AgentSpawned {
        parent: app.active_session.unwrap(),
        tool_call_id: kage_core::ToolCallId::new(call),
        agent: agent.into(),
        description: format!("{agent} task"),
    };
    send_to(app, events, child, vec![spawned.into()]);
    child
}

fn run_ended(outcome: kage_core::protocol::RunOutcome) -> kage_core::protocol::Event {
    kage_core::protocol::HostEvent::RunEnded { outcome }.into()
}

/// The pinned rows as `(depth, agent, state)`.
fn pinned(app: &App) -> Vec<(usize, String, crate::view::AgentRowState)> {
    app.agent_rows()
        .into_iter()
        .map(|row| (row.depth, row.agent, row.state))
        .collect()
}

fn rendered(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    app.render_into(&mut terminal).unwrap();
    snapshot_rows(&terminal)
}
