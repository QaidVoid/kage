use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use kage_core::config::KeybindingsConfig;
use kage_core::keymap::{Keymap, Lookup, Mapping, Mode, Rhs, parse_keys};
use kage_core::options::{OptionSource, OptionValue};
use kage_core::sync::lock;
use tempfile::tempdir;

use crate::api::LogLevel;
use crate::bridge::BridgeStep;
use crate::loader::load_all;
use crate::testing::{RecordingSink, recording_sink};
use crate::{PluginRuntime, SharedKeymap};

fn mapping(keymap: &SharedKeymap, mode: Mode, lhs: &str) -> Option<Mapping> {
    let keys = parse_keys(lhs, "\\").unwrap();
    match lock(keymap).lookup(&[mode], &keys) {
        Lookup::Exact(m) | Lookup::Prefix { exact: Some(m) } => Some(m.clone()),
        _ => None,
    }
}

fn rhs(keymap: &SharedKeymap, mode: Mode, lhs: &str) -> Option<Rhs> {
    mapping(keymap, mode, lhs).map(|m| m.rhs)
}

fn lua_id(keymap: &SharedKeymap, mode: Mode, lhs: &str) -> u64 {
    match rhs(keymap, mode, lhs) {
        Some(Rhs::Lua(id)) => id,
        other => panic!("expected a Lua rhs for {lhs}, got {other:?}"),
    }
}

fn recording_runtime() -> (RecordingSink, PluginRuntime) {
    let (rec, sink) = recording_sink();
    (rec, PluginRuntime::builder().sink(sink).build().unwrap())
}

fn toml(bindings: &[(&str, &str)], unknown: &[&str]) -> KeybindingsConfig {
    KeybindingsConfig {
        bindings: bindings
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        unknown: unknown
            .iter()
            .map(|k| ((*k).to_owned(), serde_json::json!("x")))
            .collect::<BTreeMap<_, _>>(),
        ..KeybindingsConfig::default()
    }
}

fn write(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).unwrap();
}

#[test]
fn set_accepts_every_rhs_kind_with_desc_and_group() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "local map, act = kage.keymap.set, kage.action
         map('g', '<C-p>', act.OpenModelPicker, { desc = 'models', group = 'pickers' })
         map('b', '<PageDown>', act.scroll(20))
         map({ 'n', 'b' }, '<leader>t', ':theme set dark')
         map('i', '<C-l>', function() return 'hi' end)
         map('n', 'x', '<Nop>')",
    )
    .unwrap();
    let km = rt.keymap();
    let picker = mapping(&km, Mode::Global, "<C-p>").unwrap();
    assert_eq!(
        picker.rhs,
        Rhs::Action {
            name: "OpenModelPicker",
            arg: None
        }
    );
    assert_eq!(picker.desc.as_deref(), Some("models"));
    assert_eq!(picker.group.as_deref(), Some("pickers"));
    assert_eq!(picker.owner, "lua");
    assert_eq!(
        rhs(&km, Mode::Buffer, "<PageDown>"),
        Some(Rhs::Action {
            name: "Scroll",
            arg: Some(20)
        })
    );
    for mode in [Mode::Normal, Mode::Buffer] {
        assert_eq!(
            rhs(&km, mode, "\\t"),
            Some(Rhs::Command("theme set dark".into()))
        );
    }
    assert_eq!(rhs(&km, Mode::Normal, "x"), Some(Rhs::Nop));

    let handler = rt
        .keymap_handler(lua_id(&km, Mode::Insert, "<C-l>"))
        .unwrap();
    assert_eq!(
        rt.bridge_call(&handler, &[]).unwrap(),
        BridgeStep::Done(serde_json::json!("hi"))
    );
}

#[test]
fn bad_mode_notation_and_rhs_raise() {
    let rt = PluginRuntime::new().unwrap();
    for (source, wanted) in [
        ("kage.keymap.set('q', 'x', '<Nop>')", "unknown mode 'q'"),
        (
            "kage.keymap.set('n', '<Nope>', '<Nop>')",
            "unknown key <Nope>",
        ),
        ("kage.keymap.set('n', 'x', 'theme set dark')", "rhs must be"),
        ("kage.keymap.set('n', 'x', kage.opt)", "rhs must be"),
        ("kage.keymap.set(1, 'x', '<Nop>')", "mode must be a letter"),
        ("kage.keymap.del('n', 'x')", "no mapping for x in mode n"),
    ] {
        let err = rt.eval(source).unwrap_err().to_string();
        assert!(err.contains(wanted), "{source}: {err}");
    }
    assert!(lock(&rt.keymap()).entries().is_empty());
}

#[test]
fn leader_expands_with_the_option_at_set_time() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "kage.keymap.set('n', '<leader>a', '<Nop>')
         kage.opt.leader = '<Space>'
         kage.keymap.set('n', '<leader>b', '<Nop>')",
    )
    .unwrap();
    let km = rt.keymap();
    assert!(mapping(&km, Mode::Normal, "\\a").is_some());
    assert!(mapping(&km, Mode::Normal, "<Space>b").is_some());
    assert!(mapping(&km, Mode::Normal, "\\b").is_none());
}

#[test]
fn replacing_or_deleting_a_lua_rhs_drops_its_handler() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval("kage.keymap.set('g', '<C-l>', function() end)")
        .unwrap();
    let km = rt.keymap();
    let first = lua_id(&km, Mode::Global, "<C-l>");
    rt.eval("kage.keymap.set('g', '<C-l>', function() end)")
        .unwrap();
    let second = lua_id(&km, Mode::Global, "<C-l>");
    assert_ne!(first, second);
    assert!(rt.keymap_handler(first).is_err());
    assert!(rt.keymap_handler(second).is_ok());
    rt.eval("kage.keymap.del('g', '<C-l>')").unwrap();
    assert!(rt.keymap_handler(second).is_err());
}

#[test]
fn kage_action_is_read_only_and_names_every_action() {
    let rt = PluginRuntime::new().unwrap();
    let names = rt
        .eval(
            "local n = 0
             for _ in pairs(kage.action) do n = n + 1 end
             return n",
        )
        .unwrap();
    assert_eq!(
        names.as_integer(),
        i64::try_from(kage_core::keymap::ACTIONS.len()).ok()
    );
    assert!(rt.eval("kage.action.Bogus = 1").is_err());
    assert!(
        rt.eval_plugin(
            "p",
            "return type(kage.action.scroll) == 'function' \
             and type(kage.action.OpenHelp) == 'userdata'"
        )
        .unwrap()
        .as_boolean()
        .unwrap()
    );
}

#[test]
fn owners_follow_the_load_layer_and_init_lua_wins() {
    let plugins = tempdir().unwrap();
    write(
        plugins.path(),
        "p.lua",
        "kage.keymap.set('g', '<C-p>', ':one')
         kage.keymap.set('g', '<C-t>', ':plugin')",
    );
    let user = tempdir().unwrap();
    write(
        user.path(),
        "init.lua",
        "kage.keymap.set('g', '<C-t>', ':mine')
         kage.keymap.del('g', '<C-s>')",
    );
    let rt = PluginRuntime::builder()
        .user_dir(Some(user.path().to_path_buf()))
        .build()
        .unwrap();
    let report = load_all(Some(plugins.path()), &rt).unwrap();
    assert!(report.all_ok(), "{report:?}");
    let km = rt.keymap();
    assert_eq!(mapping(&km, Mode::Global, "<C-p>").unwrap().owner, "p");
    let user_map = mapping(&km, Mode::Global, "<C-t>").unwrap();
    assert_eq!(user_map.owner, "init.lua");
    assert_eq!(user_map.rhs, Rhs::Command("mine".into()));
    assert!(mapping(&km, Mode::Global, "<C-s>").is_none());
    assert_eq!(
        mapping(&km, Mode::Global, "<F3>").unwrap().owner,
        "defaults"
    );
}

#[test]
fn embedded_defaults_map_actions_with_descriptions() {
    let rt = PluginRuntime::new().unwrap();
    load_all(None, &rt).unwrap();
    let km: Keymap = lock(&rt.keymap()).clone();
    let entries = km.entries();
    assert!(entries.len() > 30, "{}", entries.len());
    for entry in entries {
        assert_eq!(entry.mapping.owner, "defaults");
        assert!(matches!(entry.mapping.rhs, Rhs::Action { .. }));
        assert!(entry.mapping.desc.is_some() && entry.mapping.group.is_some());
    }
}

#[test]
fn toml_bindings_apply_after_plugins_and_before_init_lua() {
    let plugins = tempdir().unwrap();
    write(
        plugins.path(),
        "p.lua",
        "kage.keymap.set('g', '<C-t>', function() end)",
    );
    let user = tempdir().unwrap();
    write(
        user.path(),
        "init.lua",
        "kage.keymap.set('g', '<C-k>', ':from init')",
    );
    let rt = PluginRuntime::builder()
        .user_dir(Some(user.path().to_path_buf()))
        .keybindings(toml(
            &[
                ("ctrl+t", "theme set dark"),
                ("<leader>m", "action:OpenModelPicker"),
                ("ctrl+k", "compact"),
            ],
            &[],
        ))
        .build()
        .unwrap();
    let report = load_all(Some(plugins.path()), &rt).unwrap();
    assert!(report.keymap_errors.is_empty(), "{report:?}");
    let km = rt.keymap();
    let toml_map = mapping(&km, Mode::Global, "<C-t>").unwrap();
    assert_eq!(toml_map.owner, "config.toml");
    assert_eq!(toml_map.rhs, Rhs::Command("theme set dark".into()));
    assert_eq!(
        rhs(&km, Mode::Global, "\\m"),
        Some(Rhs::Action {
            name: "OpenModelPicker",
            arg: None
        })
    );
    assert_eq!(
        mapping(&km, Mode::Global, "<C-k>").unwrap().owner,
        "init.lua"
    );
}

#[test]
fn bad_toml_entries_and_the_flat_form_are_reported() {
    let (rec, sink) = recording_sink();
    let rt = PluginRuntime::builder()
        .sink(sink)
        .keybindings(toml(
            &[
                ("ctrl+t", "action:Nope"),
                ("<Bogus>", "compact"),
                ("ctrl+j", "action:Scroll"),
                ("ctrl+k", "compact"),
            ],
            &["ctrl+x"],
        ))
        .build()
        .unwrap();
    let report = load_all(None, &rt).unwrap();
    assert!(!report.all_ok());
    let errors = report.keymap_errors.join("\n");
    assert_eq!(report.keymap_errors.len(), 4, "{errors}");
    for wanted in [
        "unknown key `ctrl+x`",
        "`ctrl+t` = \"action:Nope\"",
        "`<Bogus>`",
        "`ctrl+j` = \"action:Scroll\"",
    ] {
        assert!(errors.contains(wanted), "missing {wanted}: {errors}");
    }
    assert!(mapping(&rt.keymap(), Mode::Global, "<C-k>").is_some());
    let logged = rec
        .snapshot()
        .logs
        .iter()
        .filter(|(level, msg)| *level == LogLevel::Error && msg.starts_with("keybindings:"))
        .count();
    assert_eq!(logged, 4);
}

#[test]
fn reload_clears_mappings_and_handlers() {
    let plugins = tempdir().unwrap();
    write(
        plugins.path(),
        "p.lua",
        "kage.register_keybinding('ctrl+g', function() end)",
    );
    let rt = PluginRuntime::new().unwrap();
    load_all(Some(plugins.path()), &rt).unwrap();
    let km = rt.keymap();
    let before = lua_id(&km, Mode::Global, "<C-g>");
    rt.eval("kage.keymap.set('g', '<C-y>', '<Nop>')").unwrap();
    rt.reload_all(Some(plugins.path())).unwrap();
    assert!(mapping(&km, Mode::Global, "<C-y>").is_none());
    let after = lua_id(&km, Mode::Global, "<C-g>");
    assert_ne!(before, after);
    assert!(rt.keymap_handler(before).is_err());
    assert!(mapping(&km, Mode::Global, "<C-p>").is_some());
}

#[test]
fn register_keybinding_maps_in_global_mode_and_off_removes_it() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "off = kage.register_keybinding({ key = 'Ctrl+Shift+P', description = 'reload' },
           function() return 'fired' end)",
    )
    .unwrap();
    let km = rt.keymap();
    let bound = mapping(&km, Mode::Global, "<C-S-p>").unwrap();
    assert_eq!(bound.desc.as_deref(), Some("reload"));
    assert_eq!(bound.group.as_deref(), Some("plugins"));
    let Rhs::Lua(id) = bound.rhs else {
        panic!("expected a Lua rhs");
    };
    let handler = rt.keymap_handler(id).unwrap();
    assert_eq!(
        rt.bridge_call(&handler, &[]).unwrap(),
        BridgeStep::Done(serde_json::json!("fired"))
    );
    rt.eval("off() off()").unwrap();
    assert!(mapping(&km, Mode::Global, "<C-S-p>").is_none());
    assert!(rt.keymap_handler(id).is_err());
}

#[test]
fn register_keybinding_off_leaves_a_later_mapping_alone() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "local off = kage.register_keybinding('f5', function() end)
         kage.keymap.set('g', '<F5>', ':later')
         off()",
    )
    .unwrap();
    assert_eq!(
        rhs(&rt.keymap(), Mode::Global, "<F5>"),
        Some(Rhs::Command("later".into()))
    );
}

#[test]
fn register_keybinding_rejects_malformed_specs() {
    let rt = PluginRuntime::new().unwrap();
    for source in [
        "kage.register_keybinding('ctrl+nope+', function() end)",
        "kage.register_keybinding({ description = 'x' }, function() end)",
        "kage.register_keybinding(5, function() end)",
    ] {
        let err = rt.eval(source).unwrap_err().to_string();
        assert!(err.contains("register_keybinding"), "{source}: {err}");
    }
}

#[test]
fn a_hatch_mapping_outside_user_config_warns() {
    let (rec, rt) = recording_runtime();
    rt.eval_plugin("p", "kage.register_keybinding('ctrl+q', function() end)")
        .unwrap();
    rt.eval_plugin("q", "kage.keymap.set('i', '<C-c>', '<Nop>')")
        .unwrap();
    let warns: Vec<String> = rec
        .snapshot()
        .logs
        .into_iter()
        .filter(|(level, _)| *level == LogLevel::Warn)
        .map(|(_, msg)| msg)
        .collect();
    assert_eq!(warns.len(), 2, "{warns:?}");
    assert!(warns[0].contains("<C-q> from 'p' never fires"), "{warns:?}");
    assert!(warns[1].contains("<C-c> from 'q'"), "{warns:?}");
    assert!(mapping(&rt.keymap(), Mode::Global, "<C-q>").is_some());
}

#[test]
fn leader_from_toml_seeds_toml_bindings() {
    let rt = PluginRuntime::builder()
        .keybindings(toml(&[("<leader>x", "compact")], &[]))
        .build()
        .unwrap();
    lock(&rt.options())
        .set("leader", OptionValue::Str(",".into()), OptionSource::Toml)
        .unwrap();
    load_all(None, &rt).unwrap();
    let keys = parse_keys(",x", "\\").unwrap();
    assert!(matches!(
        lock(&rt.keymap()).lookup(&[Mode::Global], &keys),
        Lookup::Exact(_)
    ));
}
