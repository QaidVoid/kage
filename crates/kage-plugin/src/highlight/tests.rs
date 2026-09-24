use std::sync::{Arc, Mutex};

use kage_core::options::{OptionSource, OptionStore, OptionValue};
use kage_core::sync::lock;

use super::FakeThemes;
use crate::testing::recording_sink;
use crate::{LogLevel, PluginRuntime};

fn runtime() -> PluginRuntime {
    PluginRuntime::builder()
        .themes(Arc::new(FakeThemes))
        .build()
        .unwrap()
}

fn eval_str(rt: &PluginRuntime, source: &str) -> String {
    rt.eval(source)
        .unwrap()
        .as_string()
        .map(mlua::String::to_string_lossy)
        .unwrap_or_default()
}

fn store_with_theme(theme: &str) -> Arc<Mutex<OptionStore>> {
    let mut store = OptionStore::default();
    store
        .set("theme", OptionValue::Str(theme.into()), OptionSource::Toml)
        .unwrap();
    store.take_changes();
    Arc::new(Mutex::new(store))
}

#[test]
fn hl_get_returns_the_spec_as_set_or_resolved() {
    let rt = runtime();
    let got = eval_str(
        &rt,
        r##"
        kage.api.hl_set("Mine", { link = "KageMuted", bold = true })
        local set = kage.api.hl_get("Mine")
        local eff = kage.api.hl_get("Mine", { link = false })
        kage.api.hl_set("Styled", { fg = "#010203", bg = "red", italic = true, reverse = true })
        local s = kage.api.hl_get("Styled")
        return table.concat({
          set.link, tostring(set.bold), tostring(set.fg),
          eff.fg, tostring(eff.link),
          s.fg, s.bg, tostring(s.italic), tostring(s.reverse), tostring(s.bold),
          tostring(kage.api.hl_get("Missing")),
        }, " ")
        "##,
    );
    assert_eq!(
        got,
        "KageMuted true nil #000001 nil #010203 red true true nil nil"
    );
}

#[test]
fn hl_set_rejects_bad_specs() {
    let rt = runtime();
    for bad in [
        "kage.api.hl_set('A', { fg = 'teal' })",
        "kage.api.hl_set('A', { colour = 'red' })",
        "kage.api.hl_set('A', { bold = 'yes' })",
        "kage.api.hl_set('A', { fg = 1 })",
        "kage.api.hl_set('', { fg = 'red' })",
    ] {
        let err = rt.eval(bad).unwrap_err().to_string();
        assert!(err.contains("hl_set"), "{bad}: {err}");
    }
    assert_eq!(
        eval_str(&rt, "return tostring(kage.api.hl_get('A'))"),
        "nil"
    );
}

#[test]
fn a_link_follows_a_theme_switch() {
    let rt = runtime();
    rt.eval("kage.api.hl_set('Mine', { link = 'KageMuted' })")
        .unwrap();
    let fg = "return kage.api.hl_get('Mine', { link = false }).fg";
    assert_eq!(eval_str(&rt, fg), "#000001");
    rt.eval("kage.opt.theme = 'tokyo-night'").unwrap();
    assert_eq!(eval_str(&rt, fg), "#000002");
}

#[test]
fn a_switch_resets_kage_overrides_and_keeps_others() {
    let rt = runtime();
    rt.eval(
        "kage.api.hl_set('KageMuted', { fg = '#ffffff' })
         kage.api.hl_set('MyGroup', { fg = 'red' })",
    )
    .unwrap();
    let generation = lock(&rt.highlights()).generation();
    rt.eval("kage.opt.theme = 'tokyo-night'").unwrap();
    assert!(lock(&rt.highlights()).generation() > generation);
    assert_eq!(
        eval_str(&rt, "return kage.api.hl_get('KageMuted').fg"),
        "#000002"
    );
    assert_eq!(eval_str(&rt, "return kage.api.hl_get('MyGroup').fg"), "red");
}

#[test]
fn color_scheme_fires_after_each_load_and_on_each_switch() {
    let rt = runtime();
    rt.eval(
        r#"
        seen, pinned = {}, 0
        kage.api.autocmd_create("color_scheme", {
          callback = function(ev) seen[#seen + 1] = ev.match .. "=" .. ev.data.name end,
        })
        kage.api.autocmd_create("color_scheme", {
          pattern = "tokyo-night",
          callback = function() pinned = pinned + 1 end,
        })
        "#,
    )
    .unwrap();
    crate::load_all(None, &rt).unwrap();
    let seen = "return table.concat(seen, ' ') .. ' ' .. pinned";
    assert_eq!(eval_str(&rt, seen), "default=default 0");

    rt.set_option("theme", OptionValue::Str("tokyo-night".into()))
        .unwrap();
    rt.eval("kage.opt.theme = 'tokyo-night'").unwrap();
    assert_eq!(
        eval_str(&rt, seen),
        "default=default tokyo-night=tokyo-night 1"
    );
    rt.eval("kage.theme.set('default')").unwrap();
    assert_eq!(
        eval_str(&rt, seen),
        "default=default tokyo-night=tokyo-night default=default 1"
    );
}

#[test]
fn a_color_scheme_autocmd_reapplies_kage_overrides() {
    let rt = runtime();
    rt.eval(
        r##"
        kage.api.autocmd_create("color_scheme", {
          callback = function() kage.api.hl_set("KageMuted", { fg = "#abcdef" }) end,
        })
        kage.opt.theme = "tokyo-night"
        "##,
    )
    .unwrap();
    assert_eq!(
        eval_str(&rt, "return kage.api.hl_get('KageMuted').fg"),
        "#abcdef"
    );
}

#[test]
fn the_build_loads_the_configured_theme() {
    let rt = PluginRuntime::builder()
        .options(store_with_theme("tokyo-night"))
        .themes(Arc::new(FakeThemes))
        .build()
        .unwrap();
    let hl = rt.highlights();
    assert_eq!(lock(&hl).theme(), "tokyo-night");
    assert_eq!(
        lock(&hl).resolve("KageMuted").fg.as_deref(),
        Some("#000002")
    );
}

#[test]
fn an_unloadable_theme_falls_back_to_default_and_logs() {
    let (rec, sink) = recording_sink();
    let rt = PluginRuntime::builder()
        .sink(sink)
        .options(store_with_theme("gone"))
        .themes(Arc::new(FakeThemes))
        .build()
        .unwrap();
    assert_eq!(lock(&rt.highlights()).theme(), "default");
    let logs = rec.snapshot().logs;
    assert!(
        logs.iter()
            .any(|(level, msg)| *level == LogLevel::Error && msg.contains("unknown theme `gone`")),
        "{logs:?}"
    );
}

#[test]
fn without_a_resolver_the_table_stays_empty() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval("kage.opt.theme = 'anything'").unwrap();
    assert_eq!(lock(&rt.highlights()).generation(), 0);
}
