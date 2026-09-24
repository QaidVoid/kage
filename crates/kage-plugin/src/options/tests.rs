use std::sync::{Arc, Mutex};

use kage_core::config::{Config, EditorMode};
use kage_core::options::{OptionSource, OptionStore, OptionValue};
use kage_core::sync::lock;

use crate::{PluginRuntime, SharedOptions};

fn runtime(store: &SharedOptions) -> PluginRuntime {
    PluginRuntime::builder()
        .options(Arc::clone(store))
        .theme_names(Arc::new(|| vec!["default".into(), "tokyo-night".into()]))
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

#[test]
fn unknown_theme_raises_with_the_valid_names() {
    let store = SharedOptions::default();
    let rt = runtime(&store);
    let err = rt.eval("kage.opt.theme = 'nope'").unwrap_err().to_string();
    assert!(err.contains("\"default\", \"tokyo-night\""), "{err}");
    rt.eval("kage.opt.theme = 'tokyo-night'").unwrap();
    assert_eq!(
        lock(&store).get("theme"),
        Some(&OptionValue::Str("tokyo-night".into()))
    );
}

#[test]
fn unknown_names_and_bad_values_raise() {
    let store = SharedOptions::default();
    let rt = runtime(&store);
    let err = rt.eval("return kage.opt.nope").unwrap_err().to_string();
    assert!(err.contains("theme, mouse"), "{err}");
    let err = rt
        .eval("kage.opt.editor = 'emacs'")
        .unwrap_err()
        .to_string();
    assert!(err.contains("\"vim\", \"modeless\""), "{err}");
    let err = rt.eval("kage.opt.mouse = {}").unwrap_err().to_string();
    assert!(err.contains("a boolean"), "{err}");
    assert!(lock(&store).take_changes().is_empty());
}

#[test]
fn option_set_fires_with_payload_and_pattern_filters() {
    let store = SharedOptions::default();
    let rt = runtime(&store);
    let seen = eval_str(
        &rt,
        r#"
        local seen = {}
        kage.api.autocmd_create("option_set", {
          pattern = "editor",
          callback = function(ev)
            local d = ev.data
            seen[#seen + 1] = table.concat({ ev.match, d.name, d.old, d.new, d.source }, " ")
          end,
        })
        kage.opt.mouse = false
        kage.api.option_set("editor", "vim")
        return table.concat(seen, ";")
        "#,
    );
    assert_eq!(seen, "editor editor modeless vim lua");
    let names: Vec<&str> = lock(&store).take_changes().iter().map(|c| c.name).collect();
    assert_eq!(names, ["mouse", "editor"]);
}

#[test]
fn toml_values_appear_with_source_toml() {
    let mut cfg = Config::default();
    cfg.ui.editor = EditorMode::Vim;
    let (store, errors) = OptionStore::from_config(&cfg);
    assert!(errors.is_empty());
    let store = Arc::new(Mutex::new(store));
    let rt = runtime(&store);
    let got = eval_str(
        &rt,
        r#"
        local v, src = kage.api.option_get("editor")
        local m, msrc = kage.api.option_get("mouse")
        return table.concat({ kage.opt.editor, v, src, tostring(m), msrc }, " ")
        "#,
    );
    assert_eq!(got, "vim vim toml true default");
}

#[test]
fn plugins_and_user_config_share_the_store() {
    let store = SharedOptions::default();
    let rt = runtime(&store);
    rt.eval_plugin("p", "kage.opt.input_max_lines = 12")
        .unwrap();
    let read = rt
        .eval_plugin("q", "return kage.opt.input_max_lines")
        .unwrap();
    assert_eq!(read.as_integer(), Some(12));
    assert_eq!(
        lock(&store).source("input_max_lines"),
        Some(OptionSource::Lua)
    );
}

#[test]
fn host_set_validates_at_once_and_fires_on_the_owner_thread() {
    let store = SharedOptions::default();
    let rt = runtime(&store);
    rt.eval(
        r#"
        seen = {}
        kage.api.autocmd_create("option_set", {
          callback = function(ev) seen[#seen + 1] = ev.data.source end,
        })
        "#,
    )
    .unwrap();
    assert!(
        rt.set_option("theme", OptionValue::Str("nope".into()))
            .is_err()
    );
    assert!(rt.set_option("mouse", OptionValue::Int(1)).is_err());
    rt.set_option("mouse", OptionValue::Bool(false)).unwrap();
    assert_eq!(eval_str(&rt, "return table.concat(seen, ' ')"), "runtime");
    let changes = lock(&store).take_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].new, OptionValue::Bool(false));
    assert_eq!(changes[0].source, OptionSource::Runtime);
}

#[test]
fn an_option_set_handler_that_loops_is_capped() {
    let store = SharedOptions::default();
    let rt = runtime(&store);
    rt.eval(
        r#"
        kage.api.autocmd_create("option_set", {
          callback = function(ev) kage.opt.mouse = not ev.data.new end,
        })
        kage.opt.mouse = false
        "#,
    )
    .unwrap();
    let sets = lock(&store).take_changes().len();
    assert!((2..=17).contains(&sets), "{sets} sets");
}
