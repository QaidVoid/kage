//! Tests for the autocmd registry and its `kage.api` primitives.

use std::path::PathBuf;

use serde_json::json;

use crate::PluginRuntime;
use crate::api::LogLevel;
use crate::testing::runtime_with_recording;

fn int(rt: &PluginRuntime, expr: &str) -> i64 {
    rt.eval(&format!("return {expr}"))
        .unwrap()
        .as_integer()
        .unwrap()
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
