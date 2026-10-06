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

/// No "Since API N" prose in the api reference may name a generation
/// above `API_VERSION`: a reader who trusts it writes a `requires`
/// check every load fails.
#[test]
fn api_md_never_advertises_a_generation_above_the_api_version() {
    let md = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/plugins/api.md"),
    )
    .expect("docs/plugins/api.md sits beside the crate");
    let max = crate::api::API_VERSION;
    let mut claimed = 0;
    for (at, _) in md.match_indices("Since API ") {
        let after = &md[at + "Since API ".len()..];
        let number: String = after.chars().take_while(char::is_ascii_digit).collect();
        // "Since API N" prose (the generic form) names no generation.
        if number.is_empty() {
            continue;
        }
        let parsed: i64 = number.parse().unwrap_or(0);
        let line = md[..at].matches('\n').count() + 1;
        assert!(
            (1..=max).contains(&parsed),
            "docs/plugins/api.md:{line} says \"Since API {number}\" but the host provides {max}"
        );
        claimed += 1;
    }
    assert!(
        claimed > 0,
        "no Since API claims found; the doc-lint lost its target"
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

/// The stub's `kage.Capability` alias is hand-maintained in the spec,
/// so the generator's drift gate cannot catch a variant list that lags
/// the enum (this is exactly how `crypto`, `context`, `provider` and
/// `fs_write` went missing). Pin the two together: same names, no
/// extras, none missing.
#[test]
fn capability_alias_covers_every_capability() {
    let alias = surface()
        .aliases
        .iter()
        .find(|a| a.name == "kage.Capability")
        .expect("kage.Capability alias is declared");
    let mut declared: Vec<&str> = alias.variants.to_vec();
    declared.sort_unstable();
    let mut wire: Vec<&str> = crate::capabilities::Capability::ALL
        .iter()
        .map(|c| c.name())
        .collect();
    wire.sort_unstable();
    assert_eq!(
        declared, wire,
        "kage.Capability alias and Capability::ALL disagree"
    );
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
