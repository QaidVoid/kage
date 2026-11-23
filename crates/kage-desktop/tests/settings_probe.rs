//! Headless E07 probes: the settings dialog draws its nav and every
//! page, the config pages read a recorded snapshot, and its controls
//! write through to the store. A page that draws but cannot be
//! switched is the E05 frame bug over again, so each nav row is
//! pressed by the pointer and the page's own widgets are read.

use egui_kittest::{Harness, kittest::Queryable};
use kage_desktop::store::Store;
use kage_desktop::ui::settings::{Section, SettingsEvent, SettingsView};

mod testutil;

/// The configuration snapshot the recorded engine answers with.
fn config_answer() -> serde_json::Value {
    serde_json::json!({
        "providers": {
            "custom": {"lab": {"base_url": "http://lab:8080", "api_key_env": "LAB_KEY",
                "models": [{"id": "m1", "name": "Lab One", "context": 128000}]}},
            "deepseek": {"api_key_env": "DS", "headers": {"X-Org": "x"}}
        },
        "mcp": {"servers": {
            "lab": {"command": "lab-mcp", "args": ["--port", "9"]},
            "web": {"url": "https://mcp.example.com"}
        }},
        "permissions": {
            "tools": {"shell": {"default": "ask", "allow": ["cargo *"], "deny": ["rm *"]}}
        },
        "plugins": {
            "dir": "/plug",
            "enabled": ["lab"],
            "capabilities": {"lab": ["net"], "web": ["fs"]},
            "config": {"lab": {"key": 1}}
        }
    })
}

/// The probe's harness state: the store, the dialog, what it asked
/// for, the config requests already answered, and the one-frame flag.
type Probe = (
    Store,
    SettingsView,
    Vec<SettingsEvent>,
    Vec<kage_client::RequestId>,
    bool,
);

/// A harness over the settings dialog with `store`'s session open.
fn harness_for(store: Store) -> Harness<'static, Probe> {
    Harness::builder()
        .with_size(egui::Vec2::new(1000.0, 1000.0))
        .build_ui_state(
            |ui, (store, settings, events, answered, installed): &mut Probe| {
                if testutil::theme_installed(installed, ui.ctx()) {
                    return;
                }
                events.extend(settings.show(
                    ui,
                    store,
                    &kage_desktop::ui::settings::UpdateState::default(),
                ));
                // The shell asks for a snapshot while a config page
                // shows; the probe does the same, so the recorded
                // answer lands in the store.
                if settings.is_open() && settings.section().reads_config() {
                    store.ask_config();
                    let outgoing = store.take_outgoing();
                    let mut answers: Vec<(kage_client::RequestId, serde_json::Value)> = Vec::new();
                    for frame in &outgoing {
                        if let kage_client::Frame::Request { id, method, .. } = frame
                            && method == "_kage/config/get"
                            && !answered.contains(id)
                        {
                            answered.push(id.clone());
                            answers.push((id.clone(), config_answer()));
                        }
                    }
                    for (id, result) in answers {
                        store.absorb(kage_client::Frame::Success { id, result });
                    }
                }
            },
            (store, SettingsView::new(), Vec::new(), Vec::new(), false),
        )
}

// The nav label and the page title of the General page share a word,
// so a count is the honest query; testutil owns the accesskit rules.

/// A store with one open session.
fn booted() -> Store {
    let mut store = Store::new("/w", false);
    store.set_connect(kage_desktop::transport::State::Connected);
    store.handshake(false);
    store.new_session();
    let _ = store.take_outgoing();
    store.absorb(kage_client::Frame::Success {
        id: kage_client::RequestId::Number(2),
        result: serde_json::json!({"sessionId": "s1"}),
    });
    let _ = store.take_outgoing();
    store.set_active("s1");
    store
}

/// A store whose session offers the model and mode options, so the
/// config pages read them beside the snapshot the harness answers.
fn configured() -> Store {
    let mut store = booted();
    store.absorb(kage_client::Frame::Notification {
        method: "session/update".to_owned(),
        params: serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "config_option_update",
                "configOptions": [{
                "id": "model", "name": "Model", "type": "select", "currentValue": "anthropic:a",
                "options": [
                    {"value": "anthropic:a", "name": "Claude A"},
                    {"value": "anthropic:b", "name": "Claude B"},
                    {"value": "lab:m1", "name": "Lab One"}
                ]
            }, {
                "id": "mode", "name": "Mode", "type": "select", "currentValue": "ask",
                "options": [
                    {"value": "plan", "name": "Plan"},
                    {"value": "ask", "name": "Ask"},
                    {"value": "allow", "name": "Allow"}
                ]
            }]
            }
        }),
    });
    store
}

#[test]
fn the_dialog_draws_its_nav_and_the_general_page() {
    let mut harness = harness_for(booted());
    harness.state_mut().1.open(Section::General);
    harness.run();
    harness.run();
    for section in Section::ALL {
        // The General page's own title repeats the nav label, so the
        // open page reads twice; every other section reads once.
        let wanted = if section == Section::General { 2 } else { 1 };
        assert_eq!(
            testutil::count(&harness, section.label()),
            wanted,
            "the nav row {} draws",
            section.label()
        );
    }
    assert_eq!(
        testutil::count(&harness, "Appearance"),
        1,
        "the first group draws"
    );
    assert_eq!(
        testutil::count(&harness, "Enter sends"),
        1,
        "the input toggle draws"
    );
    assert_eq!(
        testutil::count(&harness, "Group sessions by project"),
        1,
        "the session toggle draws"
    );
}

#[test]
fn a_nav_click_switches_the_page() {
    let mut harness = harness_for(booted());
    harness.state_mut().1.open(Section::General);
    harness.run();
    harness.run();
    harness.get_by_label("About").click();
    harness.run();
    assert_eq!(
        testutil::count(&harness, "Protocol"),
        1,
        "the About page draws"
    );
    assert_eq!(
        testutil::count(&harness, "Appearance"),
        0,
        "the General page is gone"
    );
    assert_eq!(
        harness.state().1.section(),
        Section::About,
        "the dialog moved to About"
    );
}

#[test]
fn the_close_button_closes_the_dialog() {
    let mut harness = harness_for(booted());
    harness.state_mut().1.open(Section::General);
    harness.run();
    harness.run();
    harness.get_by_label("Close").click();
    harness.run();
    assert!(
        !harness.state().1.is_open(),
        "the close button closes the dialog"
    );
}

#[test]
fn dead_space_inside_the_card_keeps_the_dialog_open() {
    // The scrim behind the card closes the dialog, so the card has to
    // answer its own dead space before the click reaches the scrim.
    let mut harness = harness_for(booted());
    harness.state_mut().1.open(Section::General);
    harness.run();
    harness.run();
    // A press over the nav column's empty foot, inside the card.
    let dead = egui::pos2(120.0, 700.0);
    harness.hover_at(dead);
    harness.drag_at(dead);
    harness.drop_at(dead);
    harness.run();
    harness.run();
    assert!(
        harness.state().1.is_open(),
        "a dead-space press inside the card does not close the dialog"
    );
    // A press on the scrim, outside the card, still closes it.
    let outside = egui::pos2(10.0, 990.0);
    harness.hover_at(outside);
    harness.drag_at(outside);
    harness.drop_at(outside);
    harness.run();
    harness.run();
    assert!(
        !harness.state().1.is_open(),
        "a press outside the card closes the dialog"
    );
}

#[test]
fn the_providers_page_reads_the_snapshot_and_the_session() {
    let mut harness = harness_for(configured());
    harness.state_mut().1.open(Section::Providers);
    harness.run();
    harness.run();
    assert_eq!(
        testutil::count(&harness, "Ready \u{b7} 3"),
        1,
        "the page counts the providers it lists"
    );
    for provider in ["lab", "anthropic", "deepseek"] {
        assert_eq!(
            testutil::count(&harness, provider),
            1,
            "the {provider} row draws"
        );
    }
}

#[test]
fn the_mcp_page_lists_the_configured_servers() {
    let mut harness = harness_for(configured());
    harness.state_mut().1.open(Section::Mcp);
    harness.run();
    harness.run();
    for server in ["lab", "web"] {
        assert_eq!(
            testutil::count(&harness, server),
            1,
            "the {server} server's row draws"
        );
    }
    assert_eq!(
        testutil::count(&harness, "Add server"),
        1,
        "the page offers to add one"
    );
}

#[test]
fn the_permissions_page_shows_the_modes_and_the_rules() {
    let mut harness = harness_for(configured());
    harness.state_mut().1.open(Section::Permissions);
    harness.run();
    harness.run();
    assert_eq!(
        testutil::count(&harness, "Ask"),
        1,
        "the session's mode cards draw"
    );
    assert_eq!(
        testutil::count(&harness, "Plan"),
        0,
        "the plan mode is not a card"
    );
    assert_eq!(
        testutil::count(&harness, "shell"),
        1,
        "the configured tool rule draws"
    );
}

#[test]
fn the_plugins_page_lists_the_configured_plugins() {
    let mut harness = harness_for(configured());
    harness.state_mut().1.open(Section::Plugins);
    harness.run();
    harness.run();
    for row in ["Plugin directory", "Loaded plugins", "lab", "web"] {
        assert_eq!(testutil::count(&harness, row), 1, "the {row} row draws");
    }
    harness.get_by_label("lab").click();
    harness.run();
    assert_eq!(
        testutil::count(&harness, "net"),
        1,
        "an opened plugin lists the capabilities it may be granted"
    );
}

#[test]
fn the_keyboard_page_lists_the_shell_bindings() {
    let mut harness = harness_for(booted());
    harness.state_mut().1.open(Section::Keyboard);
    harness.run();
    harness.run();
    assert_eq!(
        testutil::count(&harness, "Command palette"),
        1,
        "the app bindings draw"
    );
    assert_eq!(
        testutil::count(&harness, "Newline"),
        1,
        "the composer keys draw"
    );
    assert_eq!(
        testutil::count(&harness, "Ctrl") + testutil::count(&harness, "Cmd"),
        10,
        "the modifier caps draw"
    );
    assert_eq!(
        testutil::count(&harness, "K"),
        1,
        "the palette's key cap draws"
    );
    assert_eq!(
        testutil::count(&harness, "Esc"),
        3,
        "the interrupt's two caps draw"
    );
    assert_eq!(
        testutil::count(&harness, "Answer approval"),
        1,
        "the approval digits row draws"
    );
}

#[test]
fn a_toggle_click_reports_its_preference_field() {
    let mut harness = harness_for(booted());
    harness.state_mut().1.open(Section::General);
    harness.run();
    harness.run();
    harness.hover_at(egui::pos2(600.0, 400.0));
    harness.run();
    harness.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -2000.0),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::NONE,
    });
    harness.run_steps(30);
    harness.get_by_label("Ask before enabling swarm").click();
    harness.run();
    let (store, _, events, ..) = harness.state();
    assert!(
        events.contains(&SettingsEvent::Flip("confirm_swarm")),
        "the toggle reports the field it writes: {events:?}"
    );
    assert!(
        store.prefs().confirm_swarm,
        "the store's preference flipped"
    );
}

#[test]
fn a_mode_card_click_asks_for_that_mode() {
    let mut harness = harness_for(configured());
    harness.state_mut().1.open(Section::Permissions);
    harness.run();
    harness.run();
    harness.get_by_label("Allow").click();
    harness.run();
    let (_, _, events, ..) = harness.state();
    assert!(
        events.contains(&SettingsEvent::Mode("allow".to_owned())),
        "the card asks for the allow mode: {events:?}"
    );
}

#[test]
fn an_empty_archive_says_so() {
    let mut harness = harness_for(booted());
    harness.state_mut().1.open(Section::Archived);
    harness.run();
    harness.run();
    assert_eq!(
        testutil::count(
            &harness,
            "Nothing archived. Archive a session from its \u{2026} menu."
        ),
        1,
        "an empty archive says so"
    );
}

fn option(
    name: &str,
    kind: &str,
    value: serde_json::Value,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "name": name,
        "toml": format!("agent.{name}"),
        "doc": format!("The {name} option."),
        "kind": kind,
        "default": value,
        "value": value,
    });
    if let serde_json::Value::Object(extra) = extra {
        entry.as_object_mut().expect("an object").extend(extra);
    }
    entry
}

#[test]
fn the_agent_page_draws_the_engine_options() {
    let mut store = booted();
    store.ask_engine_options();
    let id = store
        .take_outgoing()
        .into_iter()
        .rev()
        .find_map(|frame| match frame {
            kage_client::Frame::Request { id, .. } => Some(id),
            _ => None,
        })
        .expect("the options request went out");
    store.absorb(kage_client::Frame::Success {
        id,
        result: serde_json::json!({ "options": [
            option("thinking_level", "choice", serde_json::json!("medium"), serde_json::json!({"values": ["low", "medium", "xhigh"]})),
            option("compaction_threshold", "fraction", serde_json::json!(0.8), serde_json::json!({})),
            option("agent_max_depth", "int", serde_json::json!(3), serde_json::json!({"min": 1, "max": 8})),
            option("agent_max_turns", "int", serde_json::json!(0), serde_json::json!({"min": 0, "max": 500})),
            option("swarm_timeout_ms", "int", serde_json::json!(600000), serde_json::json!({"min": 60000, "max": 3600000})),
        ]}),
    });
    let mut harness = harness_for(store);
    harness.state_mut().1.open(Section::Agents);
    harness.run();
    harness.run();
    if std::env::var_os("KAGE_PROBE_PNG").is_some() {
        let image = harness.render().expect("the page renders");
        image
            .save("/tmp/settings-agents-options.png")
            .expect("the page image saves");
    }
    assert!(
        harness.query_all_by_label("Max depth").count() > 0,
        "the page draws the engine's agent options"
    );
}

#[test]
fn the_plugin_field_takes_the_keys_and_enter_installs() {
    let mut harness = harness_for(configured());
    harness.state_mut().1.open(Section::Plugins);
    harness.run();
    harness.run();
    harness.get_by_label("Install a plugin").click();
    harness.run();
    assert!(
        harness.state().1.holds_escape(),
        "the field answers Escape before the dialog"
    );
    harness.event(egui::Event::Text("https://x.test/clock.lua".to_owned()));
    harness.run();
    assert!(
        testutil::field_holds(&harness, "https://x.test/clock.lua"),
        "the field had the keys the frame it showed"
    );
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        testutil::field_values(&harness).is_empty(),
        "Enter installs and the field goes"
    );
    assert_eq!(
        testutil::count(&harness, "Installing\u{2026}"),
        1,
        "the install is in flight"
    );
}

#[test]
fn escape_drops_the_plugin_field_and_the_rule_field() {
    let mut harness = harness_for(configured());
    harness.state_mut().1.open(Section::Plugins);
    harness.run();
    harness.run();
    harness.get_by_label("Install a plugin").click();
    harness.run();
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(
        testutil::field_values(&harness).is_empty(),
        "Escape cancels the install field"
    );
    assert!(
        !harness.state().1.holds_escape(),
        "the dialog has Escape back"
    );
    assert!(harness.state().1.is_open(), "the dialog stays open");

    harness.get_by_label("Permissions").click();
    harness.run();
    harness.run();
    harness.get_by_label("Add rule").click();
    harness.run();
    assert!(
        harness.state().1.holds_escape(),
        "the rule field answers Escape before the dialog"
    );
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(
        testutil::field_values(&harness).is_empty(),
        "Escape drops the rule field"
    );
    assert!(
        !harness.state().1.holds_escape(),
        "the dialog has Escape back"
    );
}

#[test]
fn opening_the_about_page_clears_the_update_badge() {
    let mut store = booted();
    store.update_prefs(|prefs| prefs.update_badge = true);
    let mut harness = harness_for(store);
    harness.state_mut().1.open(Section::General);
    harness.run();
    harness.run();
    harness.get_by_label("About").click();
    harness.run();
    harness.run();
    assert!(
        !harness.state().0.prefs().update_badge,
        "the About page was seen, so the badge clears"
    );
}
