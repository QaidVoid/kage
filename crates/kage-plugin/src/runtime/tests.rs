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
        let v = rt
            .with_lua(move |lua| lua.load(&chunk).eval::<bool>().unwrap_or(false))
            .unwrap();
        assert!(v, "sandbox failed to remove {path}.{key}");
    }
}

#[test]
fn io_library_is_gone_entirely() {
    let rt = PluginRuntime::new().unwrap();
    // The partial strip used to leave io.read/io.write/io.close
    // reachable; the whole table must go.
    for chunk in [
        "return io == nil",
        "return io == nil or io.read == nil",
        "return io == nil or io.write == nil",
    ] {
        let v = rt.eval(chunk).unwrap();
        assert_eq!(v.as_boolean(), Some(true), "chunk: {chunk}");
    }
}

#[test]
fn memory_limit_aborts_exhaustion_and_stays_usable() {
    let rt = PluginRuntime::builder()
        .memory_limit(8 * 1024 * 1024)
        .build()
        .unwrap();
    let err = rt
        .eval("local t = {} for i = 1, 100 do t[i] = string.rep('x', 1024 * 1024) end")
        .unwrap_err();
    assert!(matches!(err, PluginError::Lua(_)), "got: {err:?}");
    let v = rt.eval("return 6 * 7").unwrap();
    assert_eq!(v.as_integer(), Some(42));
}

#[test]
fn benign_library_functions_still_work() {
    let rt = PluginRuntime::new().unwrap();
    let v = rt.eval("return string.len('hello')").unwrap();
    assert_eq!(v.as_integer(), Some(5));
    let v = rt.eval("return math.sqrt(81)").unwrap();
    assert!((v.as_number().unwrap() - 9.0).abs() < 1e-9);
}

#[test]
fn os_execute_call_errors_after_sandboxing() {
    let rt = PluginRuntime::new().unwrap();
    assert!(rt.eval("return os.execute('echo hi')").is_err());
}

#[test]
fn dofile_and_loadfile_are_unreachable() {
    let rt = PluginRuntime::new().unwrap();
    for chunk in ["dofile('/etc/passwd')", "loadfile('/etc/passwd')"] {
        assert!(rt.eval(chunk).is_err(), "expected error from {chunk}");
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
fn reload_dir_drops_pending_session_requests() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval("kage.session.fork('e1'); kage.compact()").unwrap();
    let empty = tempfile::tempdir().unwrap();
    rt.reload_dir(empty.path()).unwrap();
    assert_eq!(rt.take_fork_request(), None);
    assert_eq!(rt.take_compact_request(), None);
}

#[test]
fn reload_dir_clears_acp_and_mcp_registrations() {
    use std::fs;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.lua"),
        "kage.request_capabilities({'exec'}) \
         kage.acp.add_agent({ name='a', command='a' }) \
         kage.on_acp_permission(function() return true end) \
         kage.mcp.add_server({ name='m', command='m' }) \
         kage.mcp.restart('m')",
    )
    .unwrap();
    let mut caps = BTreeMap::new();
    caps.insert("a".to_owned(), vec!["exec".to_owned()]);
    let rt = PluginRuntime::builder().capabilities(caps).build().unwrap();
    rt.reload_dir(dir.path()).unwrap();
    assert_eq!(rt.registered_acp_agents().len(), 1);
    assert_eq!(rt.registered_mcp_servers().len(), 1);
    assert_eq!(rt.take_mcp_restarts(), ["m"]);
    // Re-queue so the reload below must drop it.
    rt.eval_plugin("a", "kage.mcp.restart('m')").unwrap();
    assert_eq!(rt.acp_permission(&serde_json::json!({})), Some(true));

    // Reload with the plugins gone: nothing may survive.
    let empty = tempfile::tempdir().unwrap();
    rt.reload_dir(empty.path()).unwrap();
    assert!(
        rt.registered_acp_agents().is_empty(),
        "stale acp agents survived reload"
    );
    assert!(
        rt.registered_mcp_servers().is_empty(),
        "stale mcp servers survived reload"
    );
    assert!(
        rt.take_mcp_restarts().is_empty(),
        "stale mcp restart queue survived reload"
    );
    assert_eq!(
        rt.acp_permission(&serde_json::json!({})),
        None,
        "stale acp permission handler survived reload"
    );
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

#[test]
fn watchdog_aborts_runaway_eval() {
    let rt = PluginRuntime::builder()
        .script_budget(20_000_000)
        .build()
        .unwrap();
    let start = std::time::Instant::now();
    let err = rt.eval("while true do end").unwrap_err();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "watchdog took too long: {:?}",
        start.elapsed()
    );
    assert!(matches!(err, PluginError::Lua(_)), "got: {err:?}");
    // The runtime stays usable after an abort.
    let v = rt.eval("return 6 * 7").unwrap();
    assert_eq!(v.as_integer(), Some(42));
}

#[test]
fn watchdog_aborts_runaway_wrapped_coroutine() {
    // Lua hooks are per-thread and are not inherited by coroutines, so
    // the stock `coroutine.wrap` used to escape the instruction budget
    // entirely and hang the owner thread forever.
    let rt = PluginRuntime::builder()
        .script_budget(20_000_000)
        .build()
        .unwrap();
    let start = std::time::Instant::now();
    let err = rt
        .eval("local co = coroutine.wrap(function() while true do end end); co()")
        .unwrap_err();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "watchdog took too long: {:?}",
        start.elapsed()
    );
    assert!(matches!(err, PluginError::Lua(_)), "got: {err:?}");
    let v = rt.eval("return 6 * 7").unwrap();
    assert_eq!(v.as_integer(), Some(42));
}

#[test]
fn watchdog_aborts_runaway_plugin_env_coroutine() {
    // The plugin env sees a per-plugin copy of `coroutine`; the copies
    // must carry the hooked create/wrap too.
    let rt = PluginRuntime::builder()
        .script_budget(20_000_000)
        .build()
        .unwrap();
    let start = std::time::Instant::now();
    let err = rt
        .eval_plugin(
            "runaway",
            "local co = coroutine.create(function() while true do end end); \
             assert(coroutine.resume(co))",
        )
        .unwrap_err();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "watchdog took too long: {:?}",
        start.elapsed()
    );
    assert!(matches!(err, PluginError::Lua(_)), "got: {err:?}");
}

#[test]
fn hooked_wrap_keeps_stock_resume_semantics() {
    let rt = PluginRuntime::new().unwrap();
    // Yields pass through, resumes deliver their arguments, the return
    // value arrives without the leading boolean, and errors propagate.
    let v = rt
        .eval(
            "local co = coroutine.wrap(function(a) \
                 local b = coroutine.yield(a + 1); return b * 2 \
             end); \
             local x = co(1); \
             local y = co(10); \
             return x + y",
        )
        .unwrap();
    assert_eq!(v.as_integer(), Some(22));
    assert!(
        rt.eval("local co = coroutine.wrap(function() error('boom') end); co()")
            .is_err()
    );
    let v = rt
        .eval("return coroutine.create ~= nil and coroutine.wrap ~= nil")
        .unwrap();
    assert_eq!(v.as_boolean(), Some(true));
}

#[test]
fn shared_tables_reject_new_keys_and_mt_swaps() {
    let rt = PluginRuntime::new().unwrap();
    assert!(
        rt.eval("kage.definitely_unused_key = 1").is_err(),
        "new key on shared table must fail"
    );
    assert!(
        rt.eval("setmetatable(string, {})").is_err(),
        "metatable swap must fail"
    );
    let v = rt.eval("return getmetatable(string) == false").unwrap();
    assert!(
        v.as_boolean() == Some(true),
        "metatable must be protected from inspection: {v:?}"
    );
}

#[test]
fn plugin_stdlib_mutations_stay_private() {
    let rt = PluginRuntime::new().unwrap();
    // A plugin assigning into a shared table poisons only its own view.
    rt.eval_plugin("a", "string.format = function() return 'pwned' end")
        .unwrap();
    let v = rt
        .eval_plugin("a", "return string.format('%d', 7)")
        .unwrap();
    assert_eq!(v.as_string().unwrap().to_str().unwrap(), "pwned");
    // Other plugins and the host still see the real stdlib.
    let v = rt
        .eval_plugin("b", "return string.format('%d', 7)")
        .unwrap();
    assert_eq!(
        v.as_string().unwrap().to_str().unwrap(),
        "7",
        "mutation leaked to another plugin"
    );
    let v = rt.eval("return string.format('%d', 7)").unwrap();
    assert_eq!(
        v.as_string().unwrap().to_str().unwrap(),
        "7",
        "mutation leaked to host globals"
    );
    // Same for the kage surface: a plugin's shadow never reaches base.
    rt.eval_plugin("a", "kage.shadow_probe = 1").unwrap();
    assert!(rt.eval("return kage.shadow_probe").unwrap().is_nil());
}

#[test]
fn plugin_replacing_a_kage_sub_table_function_stays_private() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval_plugin(
        "evil",
        "kage.ui.set_header = function() error('hijacked') end
         kage.api.autocmd_create = nil
         kage.json.encode = nil",
    )
    .unwrap();
    rt.eval_plugin(
        "good",
        "kage.ui.set_header(function() return 'good' end)
         kage.api.autocmd_create('agent_end', { callback = function() end })
         assert(kage.json.encode({}) == '[]' or kage.json.encode({}) == '{}')",
    )
    .unwrap();
    assert!(rt.slots().spec(crate::SlotName::Header).is_some());
    assert_eq!(rt.handler_count("agent_end"), 1);
    let v = rt
        .eval("return pcall(kage.ui.set_header, nil) and kage.api.autocmd_create ~= nil")
        .unwrap();
    assert_eq!(v.as_boolean(), Some(true), "host globals were modified");
}

#[test]
fn plugin_cannot_walk_metatables_back_to_the_shared_tables() {
    let rt = PluginRuntime::new().unwrap();
    let v = rt
        .eval_plugin(
            "p",
            "return getmetatable(_G) == false and getmetatable(kage) == false
                and getmetatable(kage.ui) == nil and getmetatable('') == false",
        )
        .unwrap();
    assert_eq!(v.as_boolean(), Some(true));
    assert!(rt.eval_plugin("p", "setmetatable(_G, nil)").is_err());

    let mut caps = BTreeMap::new();
    caps.insert("w".to_owned(), vec!["session_write".to_owned()]);
    let rt = PluginRuntime::builder().capabilities(caps).build().unwrap();
    let v = rt
        .eval_plugin(
            "w",
            "kage.request_capabilities({ 'session_write' })
             return getmetatable(kage.session) == false",
        )
        .unwrap();
    assert_eq!(v.as_boolean(), Some(true));
}

#[test]
fn kage_api_and_stdlib_tables_are_read_only_in_the_base() {
    let rt = PluginRuntime::new().unwrap();
    for chunk in [
        "kage.api.extra = 1",
        "kage.ui.extra = 1",
        "kage.json.extra = 1",
        "setmetatable(kage.api, {})",
        "setmetatable(kage.ui, {})",
    ] {
        let err = rt.eval(chunk).unwrap_err().to_string();
        assert!(
            err.contains("read-only") || err.contains("protected metatable"),
            "{chunk}: {err}"
        );
    }
    let err = rt.eval("kage.api.extra = 1").unwrap_err().to_string();
    assert!(err.contains("'kage.api' is read-only"), "{err}");
    let v = rt
        .eval("return type(kage.on) == 'function' and getmetatable(kage.api) == false")
        .unwrap();
    assert_eq!(v.as_boolean(), Some(true));
}
