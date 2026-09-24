//! Tests for `kage.schedule`, `kage.defer` and `kage.timer`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::PluginRuntime;
use crate::api::LogLevel;
use crate::testing::{RecordingSink, recording_sink, runtime_with_recording};

fn int(rt: &PluginRuntime, expr: &str) -> i64 {
    rt.eval(&format!("return {expr}"))
        .unwrap()
        .as_integer()
        .unwrap()
}

/// Poll `expr` until it reaches `at_least`, failing after five seconds.
fn wait_until(rt: &PluginRuntime, expr: &str, at_least: i64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while int(rt, expr) < at_least {
        assert!(Instant::now() < deadline, "{expr} never reached {at_least}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn errors(rec: &RecordingSink) -> Vec<String> {
    rec.snapshot()
        .logs
        .into_iter()
        .filter(|(level, _)| *level == LogLevel::Error)
        .map(|(_, msg)| msg)
        .collect()
}

#[test]
fn schedule_runs_after_the_job_and_before_the_next_queued_job() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval("order = {}").unwrap();
    rt.host
        .submit(|lua| {
            lua.load(
                "kage.schedule(function() table.insert(order, 'scheduled') end)
                 table.insert(order, 'job')",
            )
            .exec()
            .unwrap();
        })
        .unwrap();
    rt.host
        .submit(|lua| {
            lua.load("table.insert(order, 'next')").exec().unwrap();
        })
        .unwrap();
    let order = rt
        .with_lua(|lua| lua.load("return order").eval::<Vec<String>>().unwrap())
        .unwrap();
    assert_eq!(order, ["job", "scheduled", "next"]);
}

#[test]
fn defer_fires_once_near_its_deadline_and_stop_cancels_it() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "fired, cancelled = 0, 0
         start = kage.now_ms()
         kage.defer(function() fired = fired + 1 at = kage.now_ms() end, 100)
         local stop = kage.defer(function() cancelled = cancelled + 1 end, 50)
         stop()
         stop()",
    )
    .unwrap();
    wait_until(&rt, "fired", 1);
    let waited = int(&rt, "at - start");
    assert!((95..1000).contains(&waited), "fired after {waited} ms");
    thread::sleep(Duration::from_millis(200));
    assert_eq!(int(&rt, "fired"), 1);
    assert_eq!(int(&rt, "cancelled"), 0);
}

#[test]
fn timer_repeats_at_the_floor_interval_until_stopped() {
    let rt = PluginRuntime::new().unwrap();
    rt.eval(
        "ticks, start = 0, kage.now_ms()
         stop = kage.timer(function()
           ticks = ticks + 1
           if ticks == 3 then
             last = kage.now_ms()
             stop()
           end
         end, 1)",
    )
    .unwrap();
    wait_until(&rt, "ticks", 3);
    let elapsed = int(&rt, "last - start");
    assert!(elapsed >= 145, "three ticks took only {elapsed} ms");
    thread::sleep(Duration::from_millis(200));
    assert_eq!(int(&rt, "ticks"), 3);
}

#[test]
fn a_raising_timer_stops_and_logs_once() {
    let (rec, rt) = runtime_with_recording(PathBuf::from("."));
    rt.eval_plugin(
        "ticker",
        "kage.timer(function() kage.notify('tick') error('tick failed') end, 50)",
    )
    .unwrap();
    thread::sleep(Duration::from_millis(400));
    assert_eq!(rec.snapshot().notifications, ["tick"]);
    let errors = errors(&rec);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].starts_with("kage.timer callback from 'ticker' raised and was stopped")
            && errors[0].contains("tick failed"),
        "{errors:?}"
    );
}

#[test]
fn a_blocking_dialog_in_a_callback_raises_instead_of_suspending() {
    let (rec, rt) = runtime_with_recording(PathBuf::from("."));
    rt.eval("kage.schedule(function() kage.ui.confirm('title', 'message') end)")
        .unwrap();
    rt.eval("return 1").unwrap();
    assert!(!rt.bridge_is_suspended());
    let errors = errors(&rec);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("yield"), "{errors:?}");
}

#[test]
fn a_timer_never_runs_while_a_job_runs() {
    let rt = PluginRuntime::new().unwrap();
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let (b, o, c) = (Arc::clone(&busy), Arc::clone(&overlaps), Arc::clone(&calls));
    rt.with_lua(move |lua| {
        let probe = lua
            .create_function(move |_, ()| {
                if b.load(Ordering::SeqCst) {
                    o.fetch_add(1, Ordering::SeqCst);
                }
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
        lua.globals().set("probe", probe).unwrap();
    })
    .unwrap();
    rt.eval("kage.timer(probe, 50)").unwrap();
    let flag = Arc::clone(&busy);
    rt.with_lua(move |_| {
        flag.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(300));
        flag.store(false, Ordering::SeqCst);
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while calls.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the timer never ran");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(overlaps.load(Ordering::SeqCst), 0);
}

#[test]
fn reload_cancels_old_timers_and_keeps_new_ones() {
    let (rec, sink) = recording_sink();
    let rt = PluginRuntime::builder()
        .sink(sink)
        .defaults("kage.defer(function() kage.notify('fresh') end, 100)")
        .build()
        .unwrap();
    rt.eval(
        "kage.timer(function() kage.notify('timer') end, 100)
         kage.defer(function() kage.notify('defer') end, 100)
         kage.schedule(function() kage.notify('scheduled') end)",
    )
    .unwrap();
    rt.reload_all(None).unwrap();
    thread::sleep(Duration::from_millis(400));
    assert_eq!(rec.snapshot().notifications, ["scheduled", "fresh"]);
}
