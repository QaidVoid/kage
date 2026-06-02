//! Tests for the plugin runtime and sandbox.

use super::*;

#[test]
fn sandbox_strips_dangerous_globals() {
    let rt = PluginRuntime::new().unwrap();
    for (path, key) in SANDBOX_REMOVALS {
        let chunk = if path.is_empty() {
            format!("return {key} == nil")
        } else {
            format!("return {path} == nil or {path}.{key} == nil")
        };
        let lua = rt.lock_lua();
        let v: bool = lua.load(&chunk).eval().unwrap_or(false);
        assert!(v, "sandbox failed to remove {path}.{key}");
    }
}

#[test]
fn benign_library_functions_still_work() {
    let rt = PluginRuntime::new().unwrap();
    let lua = rt.lock_lua();
    let v: i64 = lua.load("return string.len('hello')").eval().unwrap();
    assert_eq!(v, 5);
    let v: f64 = lua.load("return math.sqrt(81)").eval().unwrap();
    assert!((v - 9.0).abs() < 1e-9);
}

#[test]
fn os_execute_call_errors_after_sandboxing() {
    let rt = PluginRuntime::new().unwrap();
    let lua = rt.lock_lua();
    let res: Result<mlua::Value, _> = lua.load("return os.execute('echo hi')").eval();
    assert!(res.is_err());
}

#[test]
fn dofile_and_loadfile_are_unreachable() {
    let rt = PluginRuntime::new().unwrap();
    let lua = rt.lock_lua();
    for chunk in ["dofile('/etc/passwd')", "loadfile('/etc/passwd')"] {
        let res: Result<mlua::Value, _> = lua.load(chunk).eval();
        assert!(res.is_err(), "expected error from {chunk}");
    }
}

#[test]
fn eval_returns_lua_values() {
    let rt = PluginRuntime::new().unwrap();
    let v: mlua::Value = rt.eval("return 21 * 2").unwrap();
    assert_eq!(v.as_integer(), Some(42));
}

#[test]
fn plugin_config_is_per_plugin_and_isolated() {
    let mut cfg = BTreeMap::new();
    cfg.insert(
        "alpha".to_owned(),
        serde_json::json!({ "key": "alpha-val" }),
    );
    let rt = PluginRuntime::builder().plugin_config(cfg).build().unwrap();

    // The named plugin reads its own slice.
    let v = rt
        .eval_plugin("alpha", "return kage.plugin_config().key")
        .unwrap();
    assert_eq!(v.as_string().unwrap().to_str().unwrap(), "alpha-val");

    // A plugin with no slice gets an empty table, never alpha's.
    let v = rt
        .eval_plugin("beta", "return kage.plugin_config().key == nil")
        .unwrap();
    assert_eq!(v.as_boolean(), Some(true));
}

#[test]
fn store_persists_per_plugin_and_isolates() {
    let dir = tempfile::tempdir().unwrap();
    let rt = PluginRuntime::builder()
        .state_dir(Some(dir.path().to_path_buf()))
        .build()
        .unwrap();

    rt.eval_plugin("alpha", "kage.store.set('n', 41)").unwrap();
    // A fresh runtime over the same dir reads the persisted value.
    let rt2 = PluginRuntime::builder()
        .state_dir(Some(dir.path().to_path_buf()))
        .build()
        .unwrap();
    let v = rt2
        .eval_plugin("alpha", "return kage.store.get('n') + 1")
        .unwrap();
    assert_eq!(v.as_integer(), Some(42));

    // Another plugin sees its own empty store, not alpha's key.
    let v = rt2
        .eval_plugin("beta", "return kage.store.get('n') == nil")
        .unwrap();
    assert_eq!(v.as_boolean(), Some(true));

    // delete removes the key.
    rt2.eval_plugin("alpha", "kage.store.delete('n')").unwrap();
    let v = rt2
        .eval_plugin("alpha", "return kage.store.get('n') == nil")
        .unwrap();
    assert_eq!(v.as_boolean(), Some(true));
}

#[test]
fn store_without_state_dir_raises() {
    let rt = PluginRuntime::new().unwrap();
    let res = rt.eval_plugin("p", "kage.store.set('k', 1)");
    assert!(res.is_err());
}

#[test]
fn reload_dir_clears_prior_registrations() {
    use std::fs;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.lua"),
        "kage.register_command({ name='a', description='', handler=function() end })",
    )
    .unwrap();
    let rt = PluginRuntime::new().unwrap();
    rt.reload_dir(dir.path()).unwrap();
    assert_eq!(rt.registered_commands().len(), 1);

    // Replace the plugin with one that registers a different command.
    fs::write(
        dir.path().join("a.lua"),
        "kage.register_command({ name='b', description='', handler=function() end })",
    )
    .unwrap();
    rt.reload_dir(dir.path()).unwrap();
    let cmds = rt.registered_commands();
    assert_eq!(cmds.len(), 1, "old registration should not survive");
    assert_eq!(cmds[0].name(), "b");
}

#[test]
fn eval_plugin_isolates_globals_between_plugins() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval_plugin("a", "shared = 'from-a'").unwrap();
    // A second plugin must not see plugin a's top-level global.
    let v = rt.eval_plugin("b", "return shared").unwrap();
    assert!(v.is_nil(), "plugin b saw plugin a's global: {v:?}");
    // Nor does it leak into the shared globals the host evals on.
    assert!(rt.eval("return shared").unwrap().is_nil());
}

#[test]
fn eval_plugin_reuses_one_env_per_name() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval_plugin("p", "counter = 1").unwrap();
    let v = rt
        .eval_plugin("p", "counter = counter + 1; return counter")
        .unwrap();
    assert_eq!(v.as_integer(), Some(2), "same name must reuse its env");
    let other = rt.eval_plugin("q", "return counter").unwrap();
    assert!(other.is_nil(), "a different plugin must get a fresh env");
}

#[test]
fn eval_plugin_closes_global_escapes() {
    let rt = PluginRuntime::new().unwrap();
    let v = rt
        .eval_plugin(
            "esc",
            "return load == nil and loadstring == nil and require == nil \
                 and package == nil and debug == nil",
        )
        .unwrap();
    assert_eq!(v.as_boolean(), Some(true), "escape globals still reachable");
    // `_G` must be the plugin's own env, so writes through it cannot
    // reach the real globals the host evaluates against.
    rt.eval_plugin("esc2", "_G.leaked = 42").unwrap();
    assert!(rt.eval("return leaked").unwrap().is_nil());
}

#[test]
fn eval_plugin_still_reaches_base_kage_and_stdlib() {
    let rt = PluginRuntime::new().unwrap();
    let len = rt.eval_plugin("std", "return string.len('abcd')").unwrap();
    assert_eq!(
        len.as_integer(),
        Some(4),
        "stdlib unreachable in plugin env"
    );
    rt.eval_plugin(
        "reg",
        "kage.register_command({ name='z', description='', handler=function() end })",
    )
    .unwrap();
    let cmds = rt.registered_commands();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].name(), "z");
}

#[test]
fn eval_plugin_event_handlers_dispatch_with_plugin_env() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval_plugin(
        "ev",
        "hits = 0; kage.on('agent_start', function() hits = hits + 1 end)",
    )
    .unwrap();
    rt.dispatch_event("agent_start", &serde_json::json!({}))
        .unwrap();
    rt.dispatch_event("agent_start", &serde_json::json!({}))
        .unwrap();
    // The handler closes over plugin `ev`'s env, so its mutations
    // land there and survive across dispatches and re-evals.
    let v = rt.eval_plugin("ev", "return hits").unwrap();
    assert_eq!(v.as_integer(), Some(2));
}
