//! Tests for plugin event dispatch.

use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;
use crate::api::{self, HostLog, SharedHostLog, default_host_log};

fn fresh_lua_with_kage() -> Lua {
    let lua = Lua::new();
    api::install(&lua, default_host_log(), json!({})).unwrap();
    install_subscriptions(&lua).unwrap();
    lua
}

#[test]
fn handlers_run_in_registration_order() {
    let lua = fresh_lua_with_kage();
    lua.load(
        r"
            log = {}
            kage.on('agent_end', function(ev) log[#log + 1] = 'a:' .. ev.foo end)
            kage.on('agent_end', function(ev) log[#log + 1] = 'b:' .. ev.foo end)
            ",
    )
    .exec()
    .unwrap();
    dispatch(
        &lua,
        "agent_end",
        &json!({"foo": "bar"}),
        &default_host_log(),
    )
    .unwrap();
    let log: Vec<String> = lua.globals().get::<Vec<String>>("log").unwrap();
    assert_eq!(log, vec!["a:bar".to_owned(), "b:bar".to_owned()]);
}

#[test]
fn unsubscribed_event_is_silent() {
    let lua = fresh_lua_with_kage();
    dispatch(&lua, "nobody_listens", &json!({}), &default_host_log()).unwrap();
}

#[test]
fn handler_count_reflects_subscriptions() {
    let lua = fresh_lua_with_kage();
    assert_eq!(handler_count(&lua, "tool_call"), 0);
    lua.load("kage.on('tool_call', function() end)")
        .exec()
        .unwrap();
    lua.load("kage.on('tool_call', function() end)")
        .exec()
        .unwrap();
    assert_eq!(handler_count(&lua, "tool_call"), 2);
}

#[test]
fn handler_error_is_logged_but_does_not_stop_other_handlers() {
    #[derive(Default)]
    struct Recording {
        errors: Vec<String>,
    }
    impl HostLog for Recording {
        fn notify(&mut self, _: &str) {}
        fn log(&mut self, level: LogLevel, msg: &str) {
            if level == LogLevel::Error {
                self.errors.push(msg.to_owned());
            }
        }
    }
    let rec = Arc::new(Mutex::new(Recording::default()));
    let sink: SharedHostLog = {
        #[derive(Clone)]
        struct Forwarder(Arc<Mutex<Recording>>);
        impl HostLog for Forwarder {
            fn notify(&mut self, _: &str) {}
            fn log(&mut self, level: LogLevel, msg: &str) {
                self.0.lock().unwrap().log(level, msg);
            }
        }
        Arc::new(Mutex::new(
            Box::new(Forwarder(rec.clone())) as Box<dyn HostLog + Send>
        ))
    };

    let lua = Lua::new();
    api::install(&lua, sink.clone(), json!({})).unwrap();
    install_subscriptions(&lua).unwrap();

    lua.load(
        r"
            ran = 0
            kage.on('agent_end', function() error('boom') end)
            kage.on('agent_end', function() ran = ran + 1 end)
            ",
    )
    .exec()
    .unwrap();
    dispatch(&lua, "agent_end", &json!({}), &sink).unwrap();

    let ran: i64 = lua.globals().get("ran").unwrap();
    assert_eq!(ran, 1, "second handler ran despite first throwing");
    let errs = rec.lock().unwrap();
    assert_eq!(errs.errors.len(), 1);
    assert!(errs.errors[0].contains("agent_end"));
    assert!(errs.errors[0].contains("boom"));
}

#[test]
fn turn_lifecycle_events_dispatch_by_name() {
    let lua = fresh_lua_with_kage();
    lua.load(
        r"
            seen = {}
            kage.on('before_agent_start', function(ev)
                seen[#seen + 1] = 'before:' .. ev.system_prompt .. '|' .. ev.first_user_message
            end)
            kage.on('turn_start', function(ev)
                seen[#seen + 1] = 'turn_start:' .. ev.index
            end)
            kage.on('turn_end', function(ev)
                seen[#seen + 1] = 'turn_end:' .. ev.index .. ':' .. tostring(ev.had_tool_calls)
            end)
            ",
    )
    .exec()
    .unwrap();

    dispatch(
        &lua,
        "before_agent_start",
        &json!({"system_prompt": "be helpful", "first_user_message": "hi"}),
        &default_host_log(),
    )
    .unwrap();
    dispatch(
        &lua,
        "turn_start",
        &json!({"index": 0}),
        &default_host_log(),
    )
    .unwrap();
    dispatch(
        &lua,
        "turn_end",
        &json!({"index": 0, "had_tool_calls": false}),
        &default_host_log(),
    )
    .unwrap();

    let seen: Vec<String> = lua.globals().get("seen").unwrap();
    assert_eq!(
        seen,
        vec![
            "before:be helpful|hi".to_owned(),
            "turn_start:0".to_owned(),
            "turn_end:0:false".to_owned(),
        ]
    );
}

#[test]
fn streaming_events_dispatch_by_name() {
    let lua = fresh_lua_with_kage();
    lua.load(
        r"
            chunks = {}
            kage.on('message_start', function(ev) chunks[#chunks + 1] = 'start:' .. ev.id end)
            kage.on('message_update', function(ev)
                chunks[#chunks + 1] = 'delta:' .. ev.delta
            end)
            ",
    )
    .exec()
    .unwrap();

    dispatch(
        &lua,
        "message_start",
        &json!({"id": "m1"}),
        &default_host_log(),
    )
    .unwrap();
    dispatch(
        &lua,
        "message_update",
        &json!({"id": "m1", "delta": "Hel"}),
        &default_host_log(),
    )
    .unwrap();
    dispatch(
        &lua,
        "message_update",
        &json!({"id": "m1", "delta": "lo"}),
        &default_host_log(),
    )
    .unwrap();

    let chunks: Vec<String> = lua.globals().get("chunks").unwrap();
    assert_eq!(
        chunks,
        vec![
            "start:m1".to_owned(),
            "delta:Hel".to_owned(),
            "delta:lo".to_owned(),
        ]
    );
}

#[test]
fn dispatch_transform_chains_handler_returns() {
    let lua = fresh_lua_with_kage();
    lua.load(
        r"
            kage.on('transform_context', function(payload)
                payload.tag = 'first'
                return payload
            end)
            kage.on('transform_context', function(payload)
                payload.tag = payload.tag .. ',second'
                return payload
            end)
            ",
    )
    .exec()
    .unwrap();
    let out = dispatch_transform(
        &lua,
        "transform_context",
        json!({"tag": "init"}),
        &default_host_log(),
    )
    .unwrap();
    assert_eq!(out["tag"], "first,second");
}

#[test]
fn dispatch_transform_passthrough_when_handler_returns_nil() {
    let lua = fresh_lua_with_kage();
    lua.load(r"kage.on('transform_context', function(payload) return nil end)")
        .exec()
        .unwrap();
    let out = dispatch_transform(
        &lua,
        "transform_context",
        json!({"keep": true}),
        &default_host_log(),
    )
    .unwrap();
    assert_eq!(out["keep"], true);
}

#[test]
fn dispatch_predicate_returns_true_when_any_handler_votes_stop() {
    let lua = fresh_lua_with_kage();
    lua.load(
        r"
            kage.on('should_stop_after_turn', function() return false end)
            kage.on('should_stop_after_turn', function() return true end)
            kage.on('should_stop_after_turn', function() error('never reached') end)
            ",
    )
    .exec()
    .unwrap();
    let stop = dispatch_predicate(
        &lua,
        "should_stop_after_turn",
        &json!({"index": 0}),
        &default_host_log(),
    )
    .unwrap();
    assert!(stop);
}

#[test]
fn dispatch_predicate_returns_false_when_no_handlers() {
    let lua = fresh_lua_with_kage();
    let stop = dispatch_predicate(
        &lua,
        "should_stop_after_turn",
        &json!({}),
        &default_host_log(),
    )
    .unwrap();
    assert!(!stop);
}

#[test]
fn dispatch_session_op_default_proceeds() {
    let lua = fresh_lua_with_kage();
    let decision = dispatch_session_op(
        &lua,
        "session_before_switch",
        "session-id-1",
        &default_host_log(),
    )
    .unwrap();
    assert_eq!(decision, SessionOpDecision::Proceed);
}

#[test]
fn dispatch_session_op_picks_up_cancel() {
    let lua = fresh_lua_with_kage();
    lua.load(
        r"
            kage.on('session_before_switch', function(target)
                if target == 'locked' then return { cancel = 'busy' } end
            end)
            ",
    )
    .exec()
    .unwrap();
    let d =
        dispatch_session_op(&lua, "session_before_switch", "locked", &default_host_log()).unwrap();
    assert_eq!(
        d,
        SessionOpDecision::Cancel {
            reason: "busy".into()
        }
    );
    let d =
        dispatch_session_op(&lua, "session_before_switch", "other", &default_host_log()).unwrap();
    assert_eq!(d, SessionOpDecision::Proceed);
}

#[test]
fn dispatch_session_op_picks_up_patch() {
    let lua = fresh_lua_with_kage();
    lua.load(
        r"
            kage.on('session_before_fork', function(at)
                return { patch = at .. '-renamed' }
            end)
            ",
    )
    .exec()
    .unwrap();
    let d = dispatch_session_op(&lua, "session_before_fork", "abc", &default_host_log()).unwrap();
    assert_eq!(d, SessionOpDecision::Patch("abc-renamed".into()));
}

#[test]
fn dispatch_session_op_first_decision_wins() {
    let lua = fresh_lua_with_kage();
    lua.load(
        r"
            kage.on('session_before_fork', function(_) return nil end)
            kage.on('session_before_fork', function(_) return { cancel = 'first' } end)
            kage.on('session_before_fork', function(_) return { cancel = 'never' } end)
            ",
    )
    .exec()
    .unwrap();
    let d = dispatch_session_op(&lua, "session_before_fork", "x", &default_host_log()).unwrap();
    assert_eq!(
        d,
        SessionOpDecision::Cancel {
            reason: "first".into()
        }
    );
}

#[test]
fn dispatch_resources_discover_aggregates_paths() {
    let lua = fresh_lua_with_kage();
    lua.load(
        r"
            kage.on('resources_discover', function()
                return { skills = { '/a/skills', '/b/skills' }, themes = { '/c/themes' } }
            end)
            kage.on('resources_discover', function()
                return { templates = { '/d/templates' } }
            end)
            ",
    )
    .exec()
    .unwrap();
    let entries = dispatch_resources_discover(&lua, &default_host_log()).unwrap();
    assert_eq!(
        entries.skills,
        vec![PathBuf::from("/a/skills"), PathBuf::from("/b/skills")]
    );
    assert_eq!(entries.templates, vec![PathBuf::from("/d/templates")]);
    assert_eq!(entries.themes, vec![PathBuf::from("/c/themes")]);
}

#[test]
fn dispatch_resources_discover_handles_no_handlers() {
    let lua = fresh_lua_with_kage();
    let entries = dispatch_resources_discover(&lua, &default_host_log()).unwrap();
    assert!(entries.skills.is_empty());
    assert!(entries.templates.is_empty());
    assert!(entries.themes.is_empty());
}

#[test]
fn payload_arrives_as_lua_table() {
    let lua = fresh_lua_with_kage();
    lua.load(
        r"
            captured = nil
            kage.on('tool_call', function(ev) captured = ev end)
            ",
    )
    .exec()
    .unwrap();
    dispatch(
        &lua,
        "tool_call",
        &json!({"name": "echo", "args": {"k": 1}}),
        &default_host_log(),
    )
    .unwrap();
    let name: String = lua.load("return captured.name").eval().unwrap();
    let k: i64 = lua.load("return captured.args.k").eval().unwrap();
    assert_eq!(name, "echo");
    assert_eq!(k, 1);
}
