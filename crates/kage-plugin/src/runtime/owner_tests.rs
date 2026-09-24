//! Tests for the Lua owner thread: render paths never block on it,
//! input hooks are bounded, dispatch stays ordered, reload runs there.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use kage_core::CancelFlag;
use kage_tools::{ToolContext, ToolError};
use serde_json::json;

use super::*;
use crate::api::LogLevel;
use crate::testing::runtime_with_recording;

const SLOW_TOOL: &str = r"
    kage.register_tool({
        name = 'slow',
        description = '',
        schema = {},
        execute = function() kage.sleep_ms(300) return 'done' end,
    })
";

fn tool_named(rt: &PluginRuntime, name: &str) -> Arc<dyn kage_tools::Tool> {
    rt.registered_tools()
        .into_iter()
        .find(|t| t.name() == name)
        .unwrap()
}

fn run_tool(tool: &Arc<dyn kage_tools::Tool>, cancel: &CancelFlag) -> Result<String, ToolError> {
    let workdir = PathBuf::from(".");
    tool.execute(json!({}), &ToolContext::new(&workdir, cancel))
        .map(|out| out.text)
}

/// Start the slow tool on another thread and return once the owner
/// thread has it queued or running.
fn occupy_owner(rt: &PluginRuntime) -> JoinHandle<String> {
    let tool = tool_named(rt, "slow");
    let running = thread::spawn(move || run_tool(&tool, &CancelFlag::new()).unwrap());
    while rt.host.is_idle() {
        thread::sleep(Duration::from_millis(1));
    }
    running
}

fn wait_for(flag: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !flag.swap(false, Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "flag never set");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn widget_render_returns_retained_text_while_a_lua_tool_runs() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(SLOW_TOOL).unwrap();
    rt.eval(
        "kage.register_widget({ key = 'w', render = function(width) return 'w' .. width end })",
    )
    .unwrap();
    let widget = rt.registered_widgets().pop().unwrap();
    assert_eq!(widget.render(10), "w10");
    let redraw = rt.redraw_flag();
    redraw.store(false, Ordering::SeqCst);

    let running = occupy_owner(&rt);
    let start = Instant::now();
    assert_eq!(widget.render(20), "w10");
    assert!(
        start.elapsed() < Duration::from_millis(50),
        "render blocked for {:?}",
        start.elapsed()
    );
    assert_eq!(running.join().unwrap(), "done");

    wait_for(&redraw);
    assert_eq!(widget.render(20), "w20");
}

#[test]
fn chrome_render_returns_retained_lines_while_a_lua_tool_runs() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(SLOW_TOOL).unwrap();
    rt.eval("kage.ui.set_footer(function(width) return 'f' .. width end)")
        .unwrap();
    let footer = rt.footer_chrome().unwrap();
    assert_eq!(footer.render(10)[0].spans[0].text, "f10");

    let running = occupy_owner(&rt);
    let start = Instant::now();
    assert_eq!(footer.render(30)[0].spans[0].text, "f10");
    assert!(start.elapsed() < Duration::from_millis(50));
    running.join().unwrap();
}

#[test]
fn block_render_never_waits_on_a_busy_owner() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(SLOW_TOOL).unwrap();
    rt.eval("kage.register_block_renderer('k', function(b) return b.text end)")
        .unwrap();
    let renderer = rt.registered_block_renderers().pop().unwrap();
    let block = |text: &str| json!({ "kind": "k", "text": text, "width": 40 });
    assert_eq!(renderer.render(&block("a"))[0].spans[0].text, "a");
    let redraw = rt.redraw_flag();
    redraw.store(false, Ordering::SeqCst);

    let running = occupy_owner(&rt);
    let start = Instant::now();
    assert_eq!(renderer.render(&block("a"))[0].spans[0].text, "a");
    assert!(renderer.render(&block("b")).is_empty());
    assert!(start.elapsed() < Duration::from_millis(50));
    running.join().unwrap();

    wait_for(&redraw);
    assert_eq!(renderer.render(&block("b"))[0].spans[0].text, "b");
}

#[test]
fn slow_terminal_hook_lets_the_key_through_and_warns_once() {
    let (rec, rt) = runtime_with_recording(PathBuf::from("."));
    rt.eval(
        r"
        kage.on_terminal_input(function(ev)
            if ev.code == 'slow' then kage.sleep_ms(100) end
            return true
        end)
        ",
    )
    .unwrap();
    let hook = rt.registered_terminal_hooks().pop().unwrap();
    assert!(hook.handle(&json!({ "code": "enter" })));

    for _ in 0..2 {
        let start = Instant::now();
        assert!(!hook.handle(&json!({ "code": "slow" })));
        assert!(start.elapsed() < Duration::from_millis(80));
    }
    let warnings: Vec<_> = rec
        .snapshot()
        .logs
        .into_iter()
        .filter(|(level, msg)| *level == LogLevel::Warn && msg.contains("on_terminal_input"))
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");

    while !rt.host.is_idle() {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(hook.handle(&json!({ "code": "enter" })));
}

#[test]
fn lua_tool_honors_cancel_while_waiting_for_a_busy_owner() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(SLOW_TOOL).unwrap();
    rt.eval(
        r"
        kage.register_tool({
            name = 'fast',
            description = '',
            schema = {},
            execute = function() fast_ran = true return 'ok' end,
        })
        ",
    )
    .unwrap();
    let running = occupy_owner(&rt);
    let cancel = CancelFlag::new();
    let flag = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        flag.cancel();
    });
    let start = Instant::now();
    let result = run_tool(&tool_named(&rt, "fast"), &cancel);
    assert!(matches!(result, Err(ToolError::Cancelled)), "{result:?}");
    assert!(start.elapsed() < Duration::from_millis(150));
    running.join().unwrap();
    assert!(
        rt.eval("return fast_ran").unwrap().is_nil(),
        "a cancelled tool call must not run later"
    );
}

#[test]
fn dispatch_event_preserves_call_order_and_side_effects() {
    let (rec, rt) = runtime_with_recording(PathBuf::from("."));
    rt.eval(
        r"
        seen = {}
        kage.on('turn_start', function(p) table.insert(seen, p.index) end)
        kage.on('turn_end', function(p) kage.notify('end ' .. p.index) end)
        ",
    )
    .unwrap();
    for index in 0..200 {
        rt.dispatch_event("turn_start", &json!({ "index": index }))
            .unwrap();
    }
    let seen = rt
        .with_lua(|lua| lua.load("return seen").eval::<Vec<i64>>())
        .unwrap()
        .unwrap();
    assert_eq!(seen, (0..200).collect::<Vec<_>>());

    rt.dispatch_event("turn_end", &json!({ "index": 7 }))
        .unwrap();
    assert_eq!(rec.snapshot().notifications, ["end 7"]);
}

#[test]
fn reload_dir_replaces_handlers_and_render_surfaces() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = |tag: &str| {
        format!(
            "kage.register_widget({{ key = 'w', render = function() return '{tag}' end }})
             kage.on('turn_start', function() kage.notify('{tag}') end)"
        )
    };
    std::fs::write(dir.path().join("a.lua"), plugin("v1")).unwrap();
    let (rec, rt) = runtime_with_recording(PathBuf::from("."));
    crate::loader::load_dir(dir.path(), &rt).unwrap();
    assert_eq!(rt.registered_widgets()[0].render(80), "v1");

    std::fs::write(dir.path().join("a.lua"), plugin("v2")).unwrap();
    let report = rt.reload_dir(dir.path()).unwrap();
    assert_eq!(report.loaded.len(), 1);
    assert_eq!(rt.handler_count("turn_start"), 1);
    let widgets = rt.registered_widgets();
    assert_eq!(widgets.len(), 1);
    assert_eq!(widgets[0].render(80), "v2");
    rt.dispatch_event("turn_start", &json!({})).unwrap();
    assert_eq!(rec.snapshot().notifications, ["v2"]);
}

#[test]
fn render_surfaces_outlive_the_runtime_handle() {
    let renderer = {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.register_block_renderer('k', function(b) return b.text end)")
            .unwrap();
        rt.registered_block_renderers().pop().unwrap()
    };
    let lines = renderer.render(&json!({ "kind": "k", "text": "still here" }));
    assert_eq!(lines[0].spans[0].text, "still here");
}
