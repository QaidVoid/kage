use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::*;
use crate::PluginRuntime;
use crate::test_support::{occupy, wait_until};

fn component(rt: &PluginRuntime, slot: SlotName) -> Arc<LuaComponent> {
    let spec = rt.slots().spec(slot).expect("slot set");
    match spec.left.first().or(spec.lines.first()) {
        Some(SlotItem::Lua(component)) => Arc::clone(component),
        other => panic!("expected a Lua component, got {other:?}"),
    }
}

fn text(rt: &PluginRuntime, slot: SlotName) -> String {
    component(rt, slot)
        .lines()
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.text.clone()))
        .collect()
}

fn count(rt: &PluginRuntime) -> i64 {
    rt.eval("return n").unwrap().as_i64().unwrap_or(0)
}

#[test]
fn defaults_set_the_built_in_chrome() {
    let rt = PluginRuntime::new().unwrap();
    crate::load_all(None, &rt).unwrap();
    for slot in [
        SlotName::Header,
        SlotName::Activity,
        SlotName::InputPill,
        SlotName::Footer,
    ] {
        assert_eq!(rt.slots().spec(slot), Some(default_spec(slot)), "{slot:?}");
    }
    assert_default_start(&rt.slots().spec(SlotName::Start).expect("start set"));
}

/// `spec` is the default start card up to the tip, which `_defaults.lua`
/// picks at random.
fn assert_default_start(spec: &SlotSpec) {
    let default = default_spec(SlotName::Start);
    let (tip, lines) = spec.lines.split_last().expect("start has lines");
    assert_eq!(lines, &default.lines[..default.lines.len() - 1]);
    let SlotItem::Text(tip) = tip else {
        panic!("expected a tip span, got {tip:?}");
    };
    assert!(tip.text.starts_with("Tip: "), "{tip:?}");
    assert_eq!(tip.hl.as_deref(), Some("KageMuted"));
}

#[test]
fn a_user_start_spec_replaces_the_card_and_nil_restores_it() {
    let rt = PluginRuntime::new().unwrap();
    crate::load_all(None, &rt).unwrap();
    let card = rt.slots().spec(SlotName::Start).expect("start set");
    rt.eval("kage.ui.set_slot('start', { lines = { 'version' } })")
        .unwrap();
    let user = rt.slots().spec(SlotName::Start).unwrap();
    assert_eq!(user.lines, [SlotItem::Builtin("version")]);
    rt.eval("kage.ui.set_slot('start', nil)").unwrap();
    assert_eq!(rt.slots().spec(SlotName::Start), Some(card));
}

#[test]
fn set_header_takes_over_the_row_and_nil_restores_the_defaults() {
    let rt = PluginRuntime::new().unwrap();
    crate::load_all(None, &rt).unwrap();
    rt.eval("kage.ui.set_header(function(w) return 'hi ' .. w end)")
        .unwrap();
    let spec = rt.slots().spec(SlotName::Header).unwrap();
    assert_eq!(spec.left.len(), 1);
    assert!(spec.right.is_empty());
    assert_eq!(text(&rt, SlotName::Header), "hi 80");
    rt.eval("kage.ui.set_header(nil)").unwrap();
    assert_eq!(
        rt.slots().spec(SlotName::Header),
        Some(default_spec(SlotName::Header))
    );
}

#[test]
fn set_header_refreshes_every_500_ms() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval("n = 0 kage.ui.set_header(function() n = n + 1 return 'x' end)")
        .unwrap();
    let c = component(&rt, SlotName::Header);
    assert_eq!(c.interval, Some(500));
    assert_eq!(count(&rt), 1);
}

#[test]
fn set_header_rejects_a_non_function() {
    let rt = PluginRuntime::new().unwrap();
    let err = rt.eval("kage.ui.set_header(42)").unwrap_err().to_string();
    assert!(err.contains("expected a function or nil"), "{err}");
    assert!(rt.slots().spec(SlotName::Header).is_none());
}

#[test]
fn header_and_footer_are_independent() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval("kage.ui.set_footer(function() return 'foot' end)")
        .unwrap();
    assert!(rt.slots().spec(SlotName::Header).is_none());
    assert_eq!(text(&rt, SlotName::Footer), "foot");
}

#[test]
fn render_output_parses_spans_and_lines() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "kage.ui.set_footer(function()
             return {
                 'plain',
                 { { text = 'a' }, { text = 'b', hl = 'KageMuted', dim = true } },
             }
         end)",
    )
    .unwrap();
    let lines = component(&rt, SlotName::Footer).lines();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].spans[0].text, "plain");
    assert_eq!(lines[1].spans[1].hl.as_deref(), Some("KageMuted"));
    assert!(lines[1].spans[1].attrs.dim());
}

#[test]
fn a_failing_render_logs_and_keeps_the_previous_lines() {
    let (rec, rt) = crate::testing::runtime_with_recording(".".into());
    rt.eval(
        "fail = false
         kage.ui.set_slot('footer', { left = { {
             events = { 'turn_end' },
             render = function() if fail then error('boom') end return 'ok' end,
         } } })",
    )
    .unwrap();
    rt.eval("fail = true").unwrap();
    rt.dispatch_event("turn_end", &serde_json::json!({}))
        .unwrap();
    assert_eq!(text(&rt, SlotName::Footer), "ok");
    let logs = rec.snapshot().logs;
    assert!(
        logs.iter().any(|(_, m)| m.contains("boom")),
        "logs: {logs:?}"
    );
}

#[test]
fn a_looping_render_aborts_fast() {
    let rt = PluginRuntime::new().unwrap();
    let start = Instant::now();
    rt.eval("kage.ui.set_header(function() while true do end end)")
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(component(&rt, SlotName::Header).lines().is_empty());
}

#[test]
fn invalid_specs_raise_and_change_nothing() {
    let rt = PluginRuntime::new().unwrap();
    for (source, needle) in [
        (
            "kage.ui.set_slot('header', { left = { 'nope' } })",
            "unknown component 'nope'",
        ),
        ("kage.ui.set_slot('nowhere', {})", "unknown slot 'nowhere'"),
        (
            "kage.ui.set_slot('footer', { left = { { events = { 'nope' }, render = tostring } } })",
            "unknown event 'nope'",
        ),
        (
            "kage.ui.set_slot('footer', { left = { { events = { 'agent_end x' }, render = tostring } } })",
            "no match key",
        ),
        (
            "kage.ui.set_slot('start', { left = { 'brand' } })",
            "takes `lines`",
        ),
        (
            "kage.ui.set_slot('header', { lines = { 'brand' } })",
            "not `lines`",
        ),
        (
            "kage.ui.set_slot('header', { left = { 42 } })",
            "an item is",
        ),
    ] {
        let err = rt.eval(source).unwrap_err().to_string();
        assert!(err.contains(needle), "{source}: {err}");
    }
    assert!(
        SlotName::ALL
            .iter()
            .all(|slot| rt.slots().spec(*slot).is_none())
    );
}

#[test]
fn an_event_component_recomputes_once_per_event_and_never_on_read() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "n = 0
         kage.ui.set_slot('footer', { left = { 'model', {
             events = { 'turn_end', 'user Tick' },
             render = function() n = n + 1 return tostring(n) end,
         } } })",
    )
    .unwrap();
    assert_eq!(count(&rt), 1);
    let spec = rt.slots().spec(SlotName::Footer).unwrap();
    let SlotItem::Lua(c) = &spec.left[1] else {
        panic!("expected a Lua component");
    };
    for _ in 0..100 {
        let _ = c.lines();
        let _ = rt.slots().specs();
    }
    assert_eq!(count(&rt), 1);
    rt.dispatch_event("turn_end", &serde_json::json!({}))
        .unwrap();
    assert_eq!(count(&rt), 2);
    rt.eval("kage.api.autocmd_exec('user', { pattern = 'Other' })")
        .unwrap();
    assert_eq!(count(&rt), 2);
    rt.eval("kage.api.autocmd_exec('user', { pattern = 'Tick' })")
        .unwrap();
    assert_eq!(count(&rt), 3);
    assert_eq!(c.lines()[0].spans[0].text, "3");
}

#[test]
fn replacing_a_slot_drops_the_old_hooks() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "kage.ui.set_slot('footer', { left = { { events = { 'turn_end' }, render = tostring } } })",
    )
    .unwrap();
    assert_eq!(rt.handler_count("turn_end"), 1);
    rt.eval("kage.ui.set_slot('footer', { left = { 'model' } })")
        .unwrap();
    assert_eq!(rt.handler_count("turn_end"), 0);
}

#[test]
fn an_interval_component_ticks() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "n = 0
         kage.ui.set_slot('footer', { left = { {
             interval = 50,
             render = function() n = n + 1 return tostring(n) end,
         } } })",
    )
    .unwrap();
    wait_until(|| count(&rt) >= 3);
}

#[test]
fn reads_never_wait_on_a_busy_owner() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval("kage.ui.set_footer(function(w) return 'f' .. w end)")
        .unwrap();
    let c = component(&rt, SlotName::Footer);
    let busy = occupy(&rt.host);
    let slots = rt.slots();
    slots.report(120, "insert");
    assert!(slots.spec(SlotName::Footer).is_some());
    assert_eq!(c.lines()[0].spans[0].text, "f80");
    busy.assert_held();
    busy.open();
    rt.eval("return 1").unwrap();
    assert_eq!(c.lines()[0].spans[0].text, "f120");
}

#[test]
fn a_width_change_recomputes_once() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "n = 0
         kage.ui.set_slot('header', { left = { {
             render = function(ctx) n = n + 1 return ctx.width .. ' ' .. ctx.mode end,
         } } })",
    )
    .unwrap();
    let redraw = rt.redraw_flag();
    redraw.store(false, Ordering::SeqCst);
    let slots = rt.slots();
    slots.report(100, "normal");
    rt.eval("return 1").unwrap();
    slots.report(100, "insert");
    rt.eval("return 1").unwrap();
    assert_eq!(count(&rt), 2);
    assert!(redraw.load(Ordering::SeqCst));
    assert_eq!(text(&rt, SlotName::Header), "100 normal");
}

#[test]
fn redraw_recomputes_a_slot() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "n = 0
         kage.ui.set_slot('start', { lines = { 'version', {
             render = function() n = n + 1 return tostring(n) end,
         } } })",
    )
    .unwrap();
    rt.eval("kage.api.redraw('header') kage.api.redraw('start')")
        .unwrap();
    assert_eq!(count(&rt), 2);
    rt.eval("kage.api.redraw()").unwrap();
    assert_eq!(count(&rt), 3);
}

#[test]
fn ctx_carries_the_ui_state() {
    let rt = PluginRuntime::new().unwrap();
    {
        let ui = rt.slots().ui_state();
        let mut ui = lock(&ui);
        ui.state.model = "fake:m".into();
        ui.state.permission_mode = Some(kage_core::permissions::PermissionAction::Ask);
        ui.usage = Some(Usage {
            cost: 0.5,
            ..Usage::default()
        });
        ui.session_id = "abc".into();
        ui.cwd = "/w".into();
    }
    rt.eval(
        "kage.ui.set_slot('header', { left = { { render = function(ctx)
             return table.concat({ ctx.model, ctx.thinking, ctx.permission_mode,
                 tostring(ctx.working), tostring(ctx.usage.cost), ctx.session.id, ctx.cwd }, ' ')
         end } } })",
    )
    .unwrap();
    assert_eq!(
        text(&rt, SlotName::Header),
        "fake:m off ask false 0.5 abc /w"
    );
}

#[test]
fn reload_drops_user_specs_and_hooks() {
    let rt = PluginRuntime::new().unwrap();
    crate::load_all(None, &rt).unwrap();
    rt.eval(
        "kage.ui.set_slot('header', { left = { { events = { 'turn_end' }, render = tostring } } })
         kage.ui.set_slot('start', { lines = { 'version' } })",
    )
    .unwrap();
    rt.reload_all(None).unwrap();
    assert_eq!(rt.handler_count("turn_end"), 0);
    assert_eq!(
        rt.slots().spec(SlotName::Header),
        Some(default_spec(SlotName::Header))
    );
    assert_default_start(&rt.slots().spec(SlotName::Start).expect("start set"));
}

#[test]
fn builtin_components_match_the_generated_alias() {
    let alias = crate::spec::surface()
        .aliases
        .iter()
        .find(|a| a.name == "kage.Component")
        .unwrap();
    assert_eq!(alias.variants, BUILTIN_COMPONENTS);
}
