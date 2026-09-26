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
    let paths: Vec<&'static str> = surface().funcs.iter().map(|f| f.path).collect();
    let failures = rt
        .with_lua(move |lua| {
            paths
                .into_iter()
                .filter_map(|path| resolve_function(lua, path).err())
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert!(failures.is_empty(), "{failures:#?}");
}

fn resolve_function(lua: &mlua::Lua, path: &str) -> Result<(), String> {
    let mut segments = path.split('.');
    let root = segments.next().expect("path has a root");
    let mut value: mlua::Value = lua
        .globals()
        .get(root)
        .map_err(|e| format!("global `{root}` missing: {e}"))?;
    for seg in segments {
        let table = match value {
            mlua::Value::Table(t) => t,
            other => return Err(format!("{path}: `{seg}` parent is {other:?}, not a table")),
        };
        value = table
            .get(seg)
            .map_err(|e| format!("{path}: segment `{seg}` missing: {e}"))?;
    }
    match value {
        mlua::Value::Function(_) => Ok(()),
        other => Err(format!("{path} resolved to {other:?}, expected a function")),
    }
}

#[test]
fn every_func_has_a_since_within_the_api_version() {
    let s = surface();
    let max = u32::try_from(crate::api::API_VERSION).unwrap();
    for f in s.funcs.iter().chain(s.gated.iter().map(|g| &g.func)) {
        assert!(
            (1..=max).contains(&f.since),
            "{} has since {} outside 1..={max}",
            f.path,
            f.since
        );
    }
    assert!(
        s.funcs
            .iter()
            .filter(|f| f.path.starts_with("kage.api."))
            .all(|f| f.since == 2)
    );
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
            "crypto".to_owned(),
            "context".to_owned(),
            "provider".to_owned(),
            "fs_write".to_owned(),
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
