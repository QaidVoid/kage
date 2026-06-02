//! Tests for plugin command registration.

use serde_json::json;

use crate::PluginRuntime;

#[test]
fn register_command_appends_to_registry() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.register_command({
                name = 'echo',
                description = 'shouts the args back',
                handler = function(args, ctx) return 'echo:' .. args end,
            })
            ",
    )
    .unwrap();
    let commands = rt.registered_commands();
    assert_eq!(commands.len(), 1);
    let cmd = &commands[0];
    assert_eq!(cmd.name(), "echo");
    assert_eq!(cmd.description(), "shouts the args back");
    assert!(cmd.aliases().is_empty());
}

#[test]
fn register_command_records_aliases() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.register_command({
                name = 'git-status',
                aliases = { 'gst', 'gs' },
                description = 'show git status',
                handler = function() return 'ok' end,
            })
            ",
    )
    .unwrap();
    let commands = rt.registered_commands();
    assert_eq!(commands[0].aliases(), ["gst", "gs"]);
}

#[test]
fn override_command_is_separate_from_register_command() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.register_command({
                name = 'mine', description = 'a', handler = function() end,
            })
            kage.override_command({
                name = 'help', aliases = { 'h' },
                description = 'my help', handler = function() return 'hi' end,
            })
            ",
    )
    .unwrap();
    let regular = rt.registered_commands();
    assert_eq!(regular.len(), 1);
    assert_eq!(regular[0].name(), "mine");
    let overrides = rt.registered_command_overrides();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[0].name(), "help");
    assert_eq!(overrides[0].aliases(), ["h"]);
}

#[test]
fn register_command_rejects_non_array_aliases() {
    let rt = PluginRuntime::new().unwrap();
    assert!(
        rt.eval(
            r"
                kage.register_command({
                    name = 'x', description = 'y',
                    aliases = 'not-an-array',
                    handler = function() end,
                })
                ",
        )
        .is_err()
    );
}

#[test]
fn invoke_passes_args_and_context() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.register_command({
                name = 'cwd',
                description = 'returns ctx.cwd plus args',
                handler = function(args, ctx) return ctx.cwd .. ' :: ' .. args end,
            })
            ",
    )
    .unwrap();
    let cmd = rt.registered_commands().pop().unwrap();
    let out = cmd.invoke("hello", &json!({"cwd": "/home/x"})).unwrap();
    assert!(!out.is_error);
    assert_eq!(out.text, "/home/x :: hello");
}

#[test]
fn invoke_handles_lua_error() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.register_command({
                name = 'broken',
                description = '',
                handler = function() error('nope') end,
            })
            ",
    )
    .unwrap();
    let cmd = rt.registered_commands().pop().unwrap();
    let out = cmd.invoke("", &json!(null)).unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("nope"));
}

#[test]
fn omitting_args_field_registers_argless_command() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.register_command({
                name = 'noargs',
                description = '',
                handler = function() return 'ok' end,
            })
            ",
    )
    .unwrap();
    let cmd = rt.registered_commands().pop().unwrap();
    assert!(cmd.args().is_empty());
    let out = cmd.invoke("", &json!(null)).unwrap();
    assert_eq!(out.text, "ok");
}

#[test]
fn args_schema_surfaces_via_getter() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
            r"
            kage.register_command({
                name = 'cmd',
                description = '',
                args = {
                    { name = 'target', kind = 'text', optional = true, hint = 'thing' },
                    { name = 'mode', kind = 'choice', choices = {'fast', 'slow'}, optional = false },
                },
                handler = function() return 'ok' end,
            })
            ",
        )
        .unwrap();
    let cmd = rt.registered_commands().pop().unwrap();
    let args = cmd.args();
    assert_eq!(args.len(), 2);
    match &args[0] {
        crate::commands::PluginArgSpec::Text {
            name,
            optional,
            hint,
        } => {
            assert_eq!(name, "target");
            assert!(*optional);
            assert_eq!(hint, "thing");
        }
        other => panic!("expected Text, got {other:?}"),
    }
    match &args[1] {
        crate::commands::PluginArgSpec::Choice {
            name,
            values,
            optional,
        } => {
            assert_eq!(name, "mode");
            assert_eq!(values, &vec!["fast".to_owned(), "slow".to_owned()]);
            assert!(!*optional);
        }
        other => panic!("expected Choice, got {other:?}"),
    }
}

#[test]
fn parsed_args_table_reaches_handler() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.register_command({
                name = 'go',
                description = '',
                args = {
                    { name = 'mode', kind = 'choice', choices = {'up', 'down'}, optional = false },
                    { name = 'count', kind = 'text', optional = true, hint = 'n' },
                },
                handler = function(_args, _ctx, parsed)
                    return parsed.mode .. ':' .. (parsed.count or 'none')
                end,
            })
            ",
    )
    .unwrap();
    let cmd = rt.registered_commands().pop().unwrap();
    let out = cmd.invoke("up three times", &json!(null)).unwrap();
    assert_eq!(out.text, "up:three times");
}

#[test]
fn invalid_choice_yields_command_output_error() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.register_command({
                name = 'g',
                description = '',
                args = {
                    { name = 'dir', kind = 'choice', choices = {'n', 's'}, optional = false },
                },
                handler = function() return 'ok' end,
            })
            ",
    )
    .unwrap();
    let cmd = rt.registered_commands().pop().unwrap();
    let out = cmd.invoke("east", &json!(null)).unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("east"));
}

#[test]
fn flag_arg_parses_yes_to_true() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.register_command({
                name = 'v',
                description = '',
                args = { { name = 'on', kind = 'flag' } },
                handler = function(_args, _ctx, parsed)
                    return tostring(parsed.on)
                end,
            })
            ",
    )
    .unwrap();
    let cmd = rt.registered_commands().pop().unwrap();
    let out = cmd.invoke("yes", &json!(null)).unwrap();
    assert_eq!(out.text, "true");
}

#[test]
fn unknown_arg_kind_rejects_registration() {
    let rt = PluginRuntime::new().unwrap();
    let res = rt.eval(
        r"
            kage.register_command({
                name = 'bad',
                description = '',
                args = { { name = 'x', kind = 'mysterious' } },
                handler = function() return 'ok' end,
            })
            ",
    );
    assert!(res.is_err(), "expected registration error for unknown kind");
}

#[test]
fn handler_returning_table_carries_structured() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        r"
            kage.register_command({
                name = 't',
                description = '',
                handler = function() return { text = 'ok', structured = { n = 1 } } end,
            })
            ",
    )
    .unwrap();
    let cmd = rt.registered_commands().pop().unwrap();
    let out = cmd.invoke("", &json!(null)).unwrap();
    assert_eq!(out.text, "ok");
    assert_eq!(out.structured.as_ref().unwrap()["n"], 1);
}
