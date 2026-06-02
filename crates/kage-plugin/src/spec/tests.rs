//! Tests for the plugin surface spec.

use super::*;
use crate::PluginRuntime;

/// Walk a dotted path (`kage.ui.select`) through the built runtime
/// and confirm it resolves to a Lua function. This is the
/// anti-drift guarantee the old hand-maintained spec lacked: a
/// declared binding that is not actually installed fails CI here.
#[test]
fn every_declared_func_resolves_in_a_built_runtime() {
    let rt = PluginRuntime::new().expect("runtime builds");
    let lua = rt.lock_lua();
    for f in surface().funcs {
        let mut segments = f.path.split('.');
        let root = segments.next().expect("path has a root");
        let mut value: mlua::Value = lua
            .globals()
            .get(root)
            .unwrap_or_else(|e| panic!("global `{root}` missing: {e}"));
        for seg in segments {
            let table = match value {
                mlua::Value::Table(t) => t,
                other => panic!("{}: `{seg}` parent is {other:?}, not a table", f.path),
            };
            value = table
                .get(seg)
                .unwrap_or_else(|e| panic!("{}: segment `{seg}` missing: {e}", f.path));
        }
        assert!(
            matches!(value, mlua::Value::Function(_)),
            "{} resolved to {value:?}, expected a function",
            f.path
        );
    }
}

#[test]
fn surface_has_no_duplicate_func_paths() {
    let s = surface();
    let mut seen = std::collections::BTreeSet::new();
    for path in s
        .funcs
        .iter()
        .map(|f| f.path)
        .chain(s.gated.iter().map(|g| g.func.path))
    {
        assert!(seen.insert(path), "duplicate function path {path}");
    }
}

/// The anti-drift guarantee extended to capability-gated funcs:
/// granted, they resolve on that plugin's proxy; ungranted, they
/// are absent (per-plugin isolation, not a runtime error).
#[test]
fn gated_funcs_resolve_only_when_capability_granted() {
    let mut caps = std::collections::BTreeMap::new();
    caps.insert(
        "trusted".to_owned(),
        vec![
            "session_write".to_owned(),
            "exec".to_owned(),
            "env".to_owned(),
            "net".to_owned(),
        ],
    );
    let rt = PluginRuntime::builder()
        .capabilities(caps)
        .build()
        .expect("runtime builds");

    for g in surface().gated {
        let req = format!(
            "kage.request_capabilities({{'{}'}}); return type({}) == 'function'",
            g.cap, g.func.path
        );
        let granted = rt.eval_plugin("trusted", &req).expect("granted eval");
        assert_eq!(
            granted.as_boolean(),
            Some(true),
            "{} should resolve when {} is granted",
            g.func.path,
            g.cap
        );

        let ungranted = rt
            .eval_plugin("other", &format!("return {} == nil", g.func.path))
            .expect("ungranted eval");
        assert_eq!(
            ungranted.as_boolean(),
            Some(true),
            "{} must be absent without {}",
            g.func.path,
            g.cap
        );
    }
}
