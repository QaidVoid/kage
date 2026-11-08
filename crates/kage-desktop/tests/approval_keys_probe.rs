//! The approval card's digit keys in the real shell: a first ask takes
//! the keys away from the composer, a digit typed into a focused field
//! stays text, and the last answer hands the keys back.

use std::sync::{Arc, Mutex};

use egui_kittest::Harness;
use kage_desktop::transport::{Event, EventSender, Link, State, Transport};

mod testutil;

/// The parent session the handshake opens.
const PARENT: &str = "01KA5C0D1NG000000000000000";

/// A transport the probe feeds by hand: it connects at once and hands
/// over whatever frame the probe pushes.
#[derive(Clone, Default)]
struct Scripted {
    /// The shell's inbox, once the shell started the transport.
    events: Arc<Mutex<Option<EventSender>>>,
}

impl Transport for Scripted {
    fn start(&mut self, events: EventSender) {
        let _ = events.try_send(Event::State(State::Connected));
        *self.events.lock().expect("the inbox lock") = Some(events);
    }

    fn send(&self, _frame: kage_client::Frame) {}

    fn close(&self) {}

    fn link(&self) -> Link {
        Link {
            name: "replay",
            detail: "scripted".to_owned(),
        }
    }
}

/// The state a shell harness carries between frames.
struct Rig {
    /// The real window body, built on the harness's own context.
    app: Option<kage_desktop::launch::ShellApp>,
    /// Set once the theme has been installed on a frame that did not
    /// draw.
    installed: bool,
    /// The transport the probe feeds.
    transport: Scripted,
}

/// A harness over the real shell over a hand-fed transport.
fn harness() -> Harness<'static, Rig> {
    Harness::builder()
        .with_size(egui::Vec2::new(1280.0, 800.0))
        .build_ui_state(
            |ui, rig: &mut Rig| {
                if !rig.installed {
                    testutil::install_theme(ui.ctx(), "kage-shadow");
                    rig.installed = true;
                    return;
                }
                let transport = rig.transport.clone();
                let app = rig.app.get_or_insert_with(|| {
                    kage_desktop::launch::ShellApp::new(
                        "/w".to_owned(),
                        true,
                        Box::new(transport),
                        ui.ctx(),
                    )
                });
                eframe::App::ui(app, ui, &mut eframe::Frame::_new_kittest());
            },
            Rig {
                app: None,
                installed: false,
                transport: Scripted::default(),
            },
        )
}

/// The store the shell drives, once the harness started it.
fn store<'a>(harness: &'a Harness<'static, Rig>) -> &'a kage_desktop::store::Store {
    harness
        .state()
        .app
        .as_ref()
        .expect("the shell started")
        .shell
        .store()
}

/// Steps until `ready` sees the state it waits for. The pump thread
/// hands each transport event to the shell whenever the runner
/// schedules it, so the probe observes the frame's effect instead of
/// assuming a frame count; the deadline only keeps a frame that never
/// lands from hanging the run.
fn step_until(
    harness: &mut Harness<'static, Rig>,
    ready: impl Fn(&Harness<'static, Rig>) -> bool,
    what: &str,
) {
    let started = std::time::Instant::now();
    while !ready(harness) {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{what} was never observed"
        );
        std::thread::yield_now();
        harness.step();
    }
}

/// Hands the shell `frame` and steps until `ready` reports the frame
/// absorbed and drawn, so what the probe asserts next is the state
/// the frame produced, not whatever the pump thread reached.
fn push_when(
    harness: &mut Harness<'static, Rig>,
    frame: &serde_json::Value,
    ready: impl Fn(&Harness<'static, Rig>) -> bool,
    what: &str,
) {
    let frame = kage_client::Frame::parse(frame).expect("a JSON-RPC frame");
    let events = harness
        .state()
        .transport
        .events
        .lock()
        .expect("the inbox lock")
        .clone()
        .expect("the shell started the transport");
    events
        .try_send(Event::Frame(frame))
        .expect("the inbox is open");
    step_until(harness, ready, what);
}

/// Plays the recorded handshake and session answers, so the parent
/// session is open and active.
fn opened(harness: &mut Harness<'static, Rig>) {
    for _ in 0..4 {
        harness.step();
    }
    let fixture = include_str!("../../kage-client/tests/fixtures/subagents.jsonl");
    for (n, line) in fixture.lines().take(2).enumerate() {
        let frame = serde_json::from_str(line).expect("a fixture line");
        match n {
            // The initialize answer makes the shell live; the session
            // answer opens the parent the probe talks to.
            0 => push_when(
                harness,
                &frame,
                |h| store(h).state().protocol_version.is_some(),
                "the initialize answer",
            ),
            _ => push_when(
                harness,
                &frame,
                |h| store(h).active_id() == Some(PARENT),
                "the session answer",
            ),
        }
    }
    for _ in 0..20 {
        harness.step();
    }
}

/// Presses the digit `n` the way a keyboard does: the key, then the
/// character it types.
fn digit(harness: &mut Harness<'static, Rig>, n: u8) {
    let key = egui::Key::from_name(&n.to_string()).expect("a digit key");
    for pressed in [true, false] {
        harness.event(egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
    }
    harness.step();
    harness.event(egui::Event::Text(n.to_string()));
    harness.step();
    harness.step();
}

/// The composer's text, as its multiline field reports it.
fn composer_text(harness: &Harness<'static, Rig>) -> String {
    use egui_kittest::kittest::{NodeT as _, Queryable as _};
    harness
        .query_all_by(|node| node.role() == egui::accesskit::Role::MultilineTextInput)
        .find_map(|node| node.accesskit_node().value())
        .unwrap_or_default()
}

/// The widget that holds the keyboard.
fn focused(harness: &Harness<'static, Rig>) -> Option<egui::Id> {
    harness.ctx.memory(|memory| memory.focused())
}

#[test]
fn a_digit_answers_only_while_no_field_has_the_keys() {
    let mut harness = harness();
    opened(&mut harness);
    let composer = egui::Id::new("composer-input");
    assert_eq!(
        focused(&harness),
        Some(composer),
        "the composer starts with the keys"
    );
    push_when(
        &mut harness,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 101, "method": "session/request_permission",
            "params": {
                "sessionId": PARENT,
                "toolCall": {
                    "toolCallId": "call-sh", "title": "shell", "kind": "execute",
                    "status": "pending", "rawInput": {"command": "cargo test"}
                },
                "options": [
                    {"optionId": "allow", "name": "Allow shell", "kind": "allow_once"},
                    {"optionId": "reject", "name": "Reject shell", "kind": "reject_once"}
                ]
            }
        }),
        |h| !store(h).active_asks().is_empty(),
        "the permission ask",
    );
    assert_eq!(focused(&harness), None, "the first ask takes the keys");

    harness
        .ctx
        .memory_mut(|memory| memory.request_focus(composer));
    harness.step();
    digit(&mut harness, 1);
    eprintln!(
        "after digit {:?} {:?}",
        focused(&harness),
        testutil::field_values(&harness)
    );
    assert_eq!(
        testutil::count(&harness, "Allow shell"),
        1,
        "a digit typed into the composer leaves the ask open"
    );
    assert_eq!(composer_text(&harness), "1", "the digit is text");

    harness
        .ctx
        .memory_mut(|memory| memory.surrender_focus(composer));
    harness.step();
    digit(&mut harness, 1);
    assert_eq!(
        testutil::count(&harness, "Allow shell"),
        0,
        "with no field focused the digit answers"
    );
    harness.step();
    assert_eq!(
        focused(&harness),
        Some(composer),
        "the last answer hands the keys back to the composer"
    );
}
