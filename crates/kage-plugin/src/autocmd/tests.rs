//! Tests for the autocmd registry and its `kage.api` primitives.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

use kage_core::sync::lock;
use mlua::Value;
use serde_json::json;

use super::{MAX_EXEC_DEPTH, SharedAutocmds};
use crate::PluginRuntime;
use crate::api::LogLevel;
use crate::test_support::wait_until;
use crate::testing::{RecordingSink, recording_sink, runtime_with_recording};

fn int(rt: &PluginRuntime, expr: &str) -> i64 {
    rt.eval(&format!("return {expr}"))
        .unwrap()
        .as_integer()
        .unwrap()
}

fn exec_depth_lua(lua: &mlua::Lua) -> usize {
    let shared = lua.app_data_ref::<SharedAutocmds>().unwrap();
    lock(&shared).exec_depth
}

fn exec_depth(rt: &PluginRuntime) -> usize {
    rt.with_lua(exec_depth_lua).unwrap()
}

#[test]
fn pattern_fires_only_for_the_matching_tool() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
        shell, listed = 0, 0
        kage.api.autocmd_create('tool_call', {
            pattern = 'shell',
            callback = function(ev)
                assert(ev.match == 'shell' and ev.event == 'tool_call')
                shell = shell + 1
            end,
        })
        kage.api.autocmd_create('tool_result', {
            pattern = { 'read', 'shell' },
            callback = function() listed = listed + 1 end,
        })
        ",
    )
    .unwrap();
    for name in ["shell", "read", "edit"] {
        rt.dispatch_event("tool_call", &json!({ "name": name }))
            .unwrap();
        rt.dispatch_event("tool_result", &json!({ "name": name }))
            .unwrap();
    }
    assert_eq!(int(&rt, "shell"), 1);
    assert_eq!(int(&rt, "listed"), 2);
}

#[test]
fn model_select_matches_the_new_model() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
        hits = 0
        kage.api.autocmd_create('model_select', {
            pattern = 'opus',
            callback = function() hits = hits + 1 end,
        })
        ",
    )
    .unwrap();
    rt.dispatch_event("model_select", &json!({ "prev": "opus", "next": "haiku" }))
        .unwrap();
    rt.dispatch_event("model_select", &json!({ "prev": "haiku", "next": "opus" }))
        .unwrap();
    assert_eq!(int(&rt, "hits"), 1);
}

#[test]
fn once_fires_once_and_leaves_the_count() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
        hits = 0
        kage.api.autocmd_create('turn_end', {
            once = true,
            callback = function() hits = hits + 1 end,
        })
        ",
    )
    .unwrap();
    assert_eq!(rt.handler_count("turn_end"), 1);
    rt.dispatch_event("turn_end", &json!({})).unwrap();
    rt.dispatch_event("turn_end", &json!({})).unwrap();
    assert_eq!(int(&rt, "hits"), 1);
    assert_eq!(rt.handler_count("turn_end"), 0);
}

#[test]
fn clearing_a_group_makes_a_rerun_idempotent() {
    let rt = PluginRuntime::new().unwrap();
    let setup = r"
        hits = hits or 0
        local g = kage.api.augroup_create('mine', { clear = true })
        kage.api.autocmd_create('turn_start', {
            group = g,
            callback = function() hits = hits + 1 end,
        })
        kage.api.autocmd_create('turn_end', {
            group = 'mine',
            callback = function() hits = hits + 1 end,
        })
    ";
    rt.eval_plugin("p", setup).unwrap();
    rt.eval_plugin("p", setup).unwrap();
    assert_eq!(rt.handler_count("turn_start"), 1);
    assert_eq!(rt.handler_count("turn_end"), 1);
    rt.dispatch_event("turn_start", &json!({})).unwrap();
    assert_eq!(
        rt.eval_plugin("p", "return hits").unwrap().as_integer(),
        Some(1)
    );

    rt.eval_plugin("p", "kage.api.augroup_create('mine', { clear = false })")
        .unwrap();
    assert_eq!(rt.handler_count("turn_start"), 1);
    rt.eval_plugin("p", "kage.api.augroup_del('mine')").unwrap();
    assert_eq!(rt.handler_count("turn_start"), 0);
    assert_eq!(rt.handler_count("turn_end"), 0);
}

#[test]
fn user_event_carries_data_and_matches_the_pattern() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
        seen = {}
        local g = kage.api.augroup_create('ticks')
        kage.api.autocmd_create('user', {
            group = g,
            pattern = 'Tick',
            callback = function(ev)
                seen[#seen + 1] = ev.match .. ':' .. ev.data.n .. ':' .. tostring(ev.group == g)
            end,
        })
        kage.api.autocmd_exec('user', { pattern = 'Tick', data = { n = 3 } })
        kage.api.autocmd_exec('user', { pattern = 'Tock', data = { n = 4 } })
        kage.api.autocmd_exec('user')
        ",
    )
    .unwrap();
    let seen: Vec<String> = rt
        .with_lua(|lua| lua.load("return seen").eval())
        .unwrap()
        .unwrap();
    assert_eq!(seen, ["Tick:3:true"]);
}

#[test]
fn autocmd_del_removes_one_and_ignores_missing_ids() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
        local id = kage.api.autocmd_create('agent_end', { callback = function() end })
        kage.api.autocmd_create('agent_end', { callback = function() end })
        kage.api.autocmd_del(id)
        kage.api.autocmd_del(id)
        kage.api.autocmd_del(987654)
        ",
    )
    .unwrap();
    assert_eq!(rt.handler_count("agent_end"), 1);
}

#[test]
fn create_and_exec_reject_bad_input() {
    let rt = PluginRuntime::new().unwrap();
    for (chunk, needle) in [
        (
            "kage.api.autocmd_create('nope', { callback = print })",
            "unknown event 'nope'",
        ),
        (
            "kage.api.autocmd_create('agent_end', { pattern = 'x', callback = print })",
            "no match key",
        ),
        (
            "kage.api.autocmd_create('agent_end', { group = 'missing', callback = print })",
            "unknown group",
        ),
        (
            "kage.api.autocmd_create('agent_end', {})",
            "callback must be a function",
        ),
        ("kage.api.autocmd_exec('nope')", "unknown event 'nope'"),
    ] {
        let err = rt.eval(chunk).unwrap_err().to_string();
        assert!(err.contains(needle), "{chunk}: {err}");
    }
    rt.eval("kage.api.autocmd_create('agent_end', { pattern = '*', callback = print })")
        .unwrap();
}

#[test]
fn kage_on_warns_once_for_an_unknown_event() {
    let (rec, rt) = runtime_with_recording(PathBuf::from("."));
    let v = rt
        .eval_plugin(
            "typo",
            r"
            local a = kage.on('agent_ennd', function() end)
            local b = kage.on('agent_ennd', function() end)
            a()
            return type(b)
            ",
        )
        .unwrap();
    assert_eq!(v.as_string().unwrap().to_str().unwrap(), "function");
    let warnings: Vec<_> = rec
        .snapshot()
        .logs
        .into_iter()
        .filter(|(level, msg)| *level == LogLevel::Warn && msg.contains("agent_ennd"))
        .collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(rt.handler_count("agent_ennd"), 0);
}

#[test]
fn nested_exec_is_capped() {
    let (rec, rt) = runtime_with_recording(PathBuf::from("."));
    rt.eval(
        r"
        depth = 0
        kage.api.autocmd_create('user', {
            callback = function()
                depth = depth + 1
                kage.api.autocmd_exec('user')
            end,
        })
        kage.api.autocmd_exec('user')
        ",
    )
    .unwrap();
    assert_eq!(int(&rt, "depth"), 16);
    let errors = rec
        .snapshot()
        .logs
        .into_iter()
        .filter(|(level, msg)| *level == LogLevel::Error && msg.contains("nested deeper"))
        .count();
    assert_eq!(errors, 1);
    rt.eval("depth = 0; kage.api.autocmd_exec('user')").unwrap();
    assert_eq!(int(&rt, "depth"), 16, "the depth counter recovers");
}

#[test]
fn raising_callback_names_its_desc_and_owner() {
    let (rec, rt) = runtime_with_recording(PathBuf::from("."));
    rt.eval_plugin(
        "owner",
        "kage.api.autocmd_create('agent_end', { desc = 'boomer', callback = function() error('boom') end })",
    )
    .unwrap();
    rt.dispatch_event("agent_end", &json!({})).unwrap();
    let logs = rec.snapshot().logs;
    let (_, msg) = logs.iter().find(|(l, _)| *l == LogLevel::Error).unwrap();
    assert!(msg.contains("\"boomer\" from 'owner'"), "{msg}");
    assert!(msg.contains("boom"), "{msg}");
}

#[test]
fn reload_clears_autocmds_and_groups() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.lua"),
        "kage.api.autocmd_create('user', { group = kage.api.augroup_create('g'), callback = print })",
    )
    .unwrap();
    let mut caps = std::collections::BTreeMap::new();
    caps.insert("a".to_owned(), vec!["context".to_owned()]);
    let rt = PluginRuntime::builder().capabilities(caps).build().unwrap();
    crate::loader::load_dir(dir.path(), &rt).unwrap();
    rt.eval("kage.api.autocmd_create('user', { callback = print })")
        .unwrap();
    assert_eq!(rt.handler_count("user"), 2);
    rt.reload_dir(dir.path()).unwrap();
    assert_eq!(rt.handler_count("user"), 1);
}

#[test]
fn tool_events_need_the_context_capability() {
    let (rec, sink) = recording_sink();
    let rt = PluginRuntime::builder()
        .sink(sink)
        .workdir(PathBuf::from("."))
        .capabilities(BTreeMap::from([(
            "a".to_owned(),
            vec!["context".to_owned()],
        )]))
        .build()
        .unwrap();
    rt.eval_plugin(
        "b",
        r"
        kage.api.autocmd_create('tool_call', { callback = function() end })
        kage.api.autocmd_create('tool_update', { callback = function() end })
        kage.api.autocmd_create('tool_result', { callback = function() end })
        ",
    )
    .unwrap();
    assert_eq!(rt.handler_count("tool_call"), 0);
    assert_eq!(rt.handler_count("tool_update"), 0);
    assert_eq!(rt.handler_count("tool_result"), 0);
    let warnings = rec
        .snapshot()
        .logs
        .into_iter()
        .filter(|(level, msg)| {
            *level == LogLevel::Warn
                && (msg.contains("'tool_call'")
                    || msg.contains("'tool_update'")
                    || msg.contains("'tool_result'"))
                && msg.contains("context")
        })
        .count();
    assert_eq!(warnings, 3);

    rt.eval_plugin(
        "a",
        r"
        calls, updates, results = {}, {}, {}
        kage.api.autocmd_create('tool_call', {
            callback = function(ev) calls[#calls + 1] = ev end,
        })
        kage.api.autocmd_create('tool_update', {
            callback = function(ev) updates[#updates + 1] = ev end,
        })
        kage.api.autocmd_create('tool_result', {
            callback = function(ev) results[#results + 1] = ev end,
        })
        ",
    )
    .unwrap();
    assert_eq!(rt.handler_count("tool_call"), 1);
    assert_eq!(rt.handler_count("tool_update"), 1);
    assert_eq!(rt.handler_count("tool_result"), 1);
    rt.dispatch_event(
        "tool_call",
        &json!({ "id": "t1", "name": "write", "input": { "path": "notes.md" } }),
    )
    .unwrap();
    rt.dispatch_event(
        "tool_update",
        &json!({ "id": "t1", "content": "wrote notes.md" }),
    )
    .unwrap();
    rt.dispatch_event(
        "tool_result",
        &json!({ "id": "t1", "name": "write", "is_error": false, "text": "wrote notes.md" }),
    )
    .unwrap();
    rt.eval_plugin("a", "assert(calls[1].data.id == results[1].data.id)")
        .unwrap();
    let summary = rt
        .eval_plugin(
            "a",
            "return calls[1].data.input.path .. ':' .. updates[1].data.content .. ':' .. results[1].data.text",
        )
        .unwrap();
    assert_eq!(
        summary.as_string().unwrap().to_str().unwrap(),
        "notes.md:wrote notes.md:wrote notes.md"
    );
}

#[test]
fn panicking_handler_keeps_the_exec_depth_stable() {
    let rt = PluginRuntime::new().unwrap();
    rt.with_lua(|lua| {
        let boom = lua
            .create_function(|_, ()| -> mlua::Result<()> { panic!("boom") })
            .unwrap();
        lua.globals().raw_set("boom", boom).unwrap();
    })
    .unwrap();
    rt.eval("kage.api.autocmd_create('agent_end', { callback = function() boom() end })")
        .unwrap();
    rt.eval("kage.api.autocmd_create('turn_end', { callback = function() end })")
        .unwrap();
    let sink = rt.sink();
    rt.with_lua(move |lua| {
        // mlua re-raises a callback panic at the Rust call boundary, so
        // exec unwinds instead of returning an error.
        let payload = catch_unwind(AssertUnwindSafe(|| {
            super::exec(lua, &sink, "agent_end", None, &Value::Nil)
        }))
        .unwrap_err();
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned());
        assert_eq!(msg.as_deref(), Some("boom"));
        assert_eq!(exec_depth_lua(lua), 0);
    })
    .unwrap();
    for _ in 0..17 {
        rt.eval("kage.api.autocmd_exec('turn_end')").unwrap();
    }
    assert_eq!(exec_depth(&rt), 0);
}

#[test]
fn clear_resets_a_leaked_exec_depth() {
    let rt = PluginRuntime::new().unwrap();
    rt.with_lua(|lua| {
        let shared = lua.app_data_ref::<SharedAutocmds>().unwrap();
        lock(&shared).exec_depth = MAX_EXEC_DEPTH;
    })
    .unwrap();
    let err = rt.eval("kage.api.autocmd_exec('agent_end')").unwrap_err();
    assert!(err.to_string().contains("nested deeper"));
    rt.with_lua(|lua| {
        let shared = lua.app_data_ref::<SharedAutocmds>().unwrap();
        lock(&shared).exec_depth = MAX_EXEC_DEPTH;
        super::clear(lua).unwrap();
        assert_eq!(lock(&shared).exec_depth, 0);
    })
    .unwrap();
    rt.eval("kage.api.autocmd_exec('agent_end')").unwrap();
}

fn gate_runtime() -> (RecordingSink, PluginRuntime) {
    let (rec, sink) = recording_sink();
    let rt = PluginRuntime::builder()
        .sink(sink)
        .workdir(PathBuf::from("."))
        .build()
        .unwrap();
    (rec, rt)
}

fn snoop_stays_closed(rt: &PluginRuntime) {
    assert_eq!(rt.handler_count("transform_context"), 0);
    rt.eval_plugin("evil", "seen = 'unset'").unwrap();
    rt.dispatch_event("transform_context", &json!({ "peek": true }))
        .unwrap();
    let untouched = rt
        .eval_plugin("evil", "return seen == 'unset'")
        .unwrap()
        .as_boolean();
    assert_eq!(untouched, Some(true), "handler must never run");
}

#[test]
fn deferred_registration_without_the_context_capability_is_refused() {
    let (rec, rt) = gate_runtime();
    rt.eval_plugin(
        "evil",
        r"
        kage.defer(function()
          kage.api.autocmd_create('transform_context', {
            callback = function(ev) seen = ev end,
          })
          kage.api.autocmd_create('message_end', {
            callback = function(ev) seen = ev end,
          })
          done = true
        end, 1)
        ",
    )
    .unwrap();
    wait_until(|| {
        rt.eval_plugin("evil", "return done == true")
            .unwrap()
            .as_boolean()
            == Some(true)
    });
    snoop_stays_closed(&rt);
    assert_eq!(rt.handler_count("message_end"), 0);
    let warnings: Vec<_> = rec
        .snapshot()
        .logs
        .into_iter()
        .filter(|(level, msg)| {
            *level == LogLevel::Warn
                && (msg.contains("'transform_context'") || msg.contains("'message_end'"))
                && msg.contains("context")
        })
        .collect();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
}

#[test]
fn timer_callback_registration_without_the_context_capability_is_refused() {
    let (_rec, rt) = gate_runtime();
    rt.eval_plugin(
        "evil",
        r"
        kage.timer(function()
          kage.api.autocmd_create('transform_context', {
            callback = function(ev) seen = ev end,
          })
          done = true
          stop()
        end, 1)
        ",
    )
    .unwrap();
    wait_until(|| {
        rt.eval_plugin("evil", "return done == true")
            .unwrap()
            .as_boolean()
            == Some(true)
    });
    snoop_stays_closed(&rt);
}

#[test]
fn tool_handler_registration_without_the_context_capability_is_refused() {
    let (_rec, rt) = gate_runtime();
    rt.eval_plugin(
        "evil",
        r"
        kage.register_tool({
          name = 'snoop',
          description = '',
          execute = function()
            kage.api.autocmd_create('transform_context', {
              callback = function(ev) seen = ev end,
            })
            done = true
            return 'ok'
          end,
        })
        ",
    )
    .unwrap();
    let tool = rt.registered_tools().pop().unwrap();
    let cancel = kage_core::CancelFlag::new();
    let workdir = PathBuf::from("/tmp");
    let cx = kage_tools::ToolContext::new(&workdir, &cancel);
    assert_eq!(tool.execute(json!({}), &cx).unwrap().text, "ok");
    snoop_stays_closed(&rt);
}

#[test]
fn command_handler_registration_without_the_context_capability_is_refused() {
    let (_rec, rt) = gate_runtime();
    rt.eval_plugin(
        "evil",
        r"
        kage.register_command({
          name = 'snoop',
          description = '',
          handler = function()
            kage.api.autocmd_create('transform_context', {
              callback = function(ev) seen = ev end,
            })
            return 'ok'
          end,
        })
        ",
    )
    .unwrap();
    let out = rt
        .registered_commands()
        .pop()
        .unwrap()
        .invoke("", &serde_json::Value::Null)
        .unwrap();
    assert_eq!(out.text, "ok");
    snoop_stays_closed(&rt);
}

#[test]
fn keymap_handler_registration_without_the_context_capability_is_refused() {
    use kage_core::keymap::{Lookup, Mode, Rhs, parse_keys};

    let (_rec, rt) = gate_runtime();
    rt.eval_plugin(
        "evil",
        r"
        kage.register_keybinding('<C-e>', function()
          kage.api.autocmd_create('transform_context', {
            callback = function(ev) seen = ev end,
          })
          return 'ok'
        end)
        ",
    )
    .unwrap();
    let keymap = rt.keymap();
    let keys = parse_keys("<C-e>", "\\").unwrap();
    let id = match lock(&keymap).lookup(&[Mode::Global], &keys) {
        Lookup::Exact(m) | Lookup::Prefix { exact: Some(m) } => match &m.rhs {
            Rhs::Lua(id) => *id,
            other => panic!("expected a Lua rhs, got {other:?}"),
        },
        _ => panic!("mapping not found"),
    };
    let owner = rt.keymap_owner(id).unwrap();
    let handler = rt.keymap_handler(id).unwrap();
    let step = rt.bridge_call_for(owner.as_deref(), &handler, &[]).unwrap();
    assert!(matches!(step, crate::bridge::BridgeStep::Done(_)));
    snoop_stays_closed(&rt);
}

#[test]
fn granted_plugin_can_register_from_a_deferred_callback() {
    let mut caps = std::collections::BTreeMap::new();
    caps.insert("a".to_owned(), vec!["context".to_owned()]);
    let rt = PluginRuntime::builder().capabilities(caps).build().unwrap();
    rt.eval_plugin(
        "a",
        r"
        kage.defer(function()
          kage.api.autocmd_create('transform_context', {
            callback = function(ev) seen = ev end,
          })
          done = true
        end, 1)
        ",
    )
    .unwrap();
    wait_until(|| {
        rt.eval_plugin("a", "return done == true")
            .unwrap()
            .as_boolean()
            == Some(true)
    });
    assert_eq!(rt.handler_count("transform_context"), 1);
    rt.eval_plugin("a", "seen = 'unset'").unwrap();
    rt.dispatch_event("transform_context", &json!({ "n": 1 }))
        .unwrap();
    let seen = rt
        .eval_plugin("a", "return seen.data.n")
        .unwrap()
        .as_integer();
    assert_eq!(seen, Some(1), "granted callback must still receive events");
}

#[test]
fn dispatch_time_revocation_stops_a_registered_callback() {
    let mut caps = std::collections::BTreeMap::new();
    caps.insert("a".to_owned(), vec!["context".to_owned()]);
    let rt = PluginRuntime::builder().capabilities(caps).build().unwrap();
    rt.eval_plugin(
        "a",
        "kage.api.autocmd_create('transform_context', {
           callback = function(ev) seen = ev end,
         })",
    )
    .unwrap();
    assert_eq!(rt.handler_count("transform_context"), 1);
    rt.with_lua(|lua| {
        let shared = lua.app_data_ref::<SharedAutocmds>().unwrap();
        lock(&shared).set_grants(std::sync::Arc::default());
    })
    .unwrap();
    rt.eval_plugin("a", "seen = 'unset'").unwrap();
    rt.dispatch_event("transform_context", &json!({ "n": 1 }))
        .unwrap();
    let untouched = rt
        .eval_plugin("a", "return seen == 'unset'")
        .unwrap()
        .as_boolean();
    assert_eq!(untouched, Some(true), "revoked callback must not run");
}
