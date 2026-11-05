//! Process-level regression for Lua output sharing the RPC transport.

use std::process::Command;
use std::sync::{Arc, Mutex};

use kage_plugin::api::{HostLog, LogLevel, SharedHostLog};

use kage_plugin::{PluginRuntime, load_dir};

#[derive(Clone, Default)]
struct RecordingSink(Arc<Mutex<Vec<(LogLevel, String)>>>);

impl HostLog for RecordingSink {
    fn notify(&mut self, message: &str) {
        panic!("print must log, not notify: {message}");
    }

    fn log(&mut self, level: LogLevel, message: &str) {
        self.0.lock().unwrap().push((level, message.to_owned()));
    }
}

#[test]
fn lua_print_uses_configured_sink() {
    let root = tempfile::tempdir().unwrap();
    let plugins = root.path().join("plugins");
    let user = root.path().join("user");
    std::fs::create_dir(&plugins).unwrap();
    std::fs::create_dir(&user).unwrap();
    std::fs::write(plugins.join("example.lua"), "print('plugin startup', 42)").unwrap();
    std::fs::write(user.join("init.lua"), "print('user startup', true)").unwrap();

    let recording = RecordingSink::default();
    let sink: SharedHostLog = Arc::new(Mutex::new(Box::new(recording.clone())));
    let runtime = PluginRuntime::builder()
        .sink(sink)
        .user_dir(Some(user))
        .build()
        .unwrap();
    let report = load_dir(&plugins, &runtime).unwrap();
    assert!(report.all_ok(), "{report:?}");
    assert_eq!(report.loaded.len(), 1);
    assert!(matches!(report.init, Some(Ok(()))));
    runtime.eval("print('shared', nil)").unwrap();
    runtime
        .eval_plugin(
            "other",
            r"
            print('direct', setmetatable({}, { __tostring = function() return 'converted' end }))
            print('outer', setmetatable({}, { __tostring = function()
                print('inner', 7)
                return 'value'
            end }))
            assert(not pcall(print, setmetatable({}, { __tostring = function() return {} end })))
            tostring = function() return 'changed' end
            print('binary', 'a\0b', string.char(255))
            assert(print() == nil)
        ",
        )
        .unwrap();

    let logs = recording.0.lock().unwrap();
    assert_eq!(
        *logs,
        [
            (LogLevel::Info, "plugin startup\t42".to_owned()),
            (LogLevel::Info, "user startup\ttrue".to_owned()),
            (LogLevel::Info, "shared\tnil".to_owned()),
            (LogLevel::Info, "direct\tconverted".to_owned()),
            (LogLevel::Info, "inner\t7".to_owned()),
            (LogLevel::Info, "outer\tvalue".to_owned()),
            (LogLevel::Info, "binary\ta\0b\t\u{fffd}".to_owned()),
            (LogLevel::Info, String::new()),
        ]
    );
}

#[test]
fn lua_print_rejects_oversized_output_without_partial_log() {
    let recording = RecordingSink::default();
    let sink: SharedHostLog = Arc::new(Mutex::new(Box::new(recording.clone())));
    let runtime = PluginRuntime::builder().sink(sink).build().unwrap();
    runtime
        .eval(
            r"
        local part = string.rep('x', 512 * 1024)
        local ok, err = pcall(print, part, part)
        assert(not ok and tostring(err):find('print output exceeds 1048576 bytes', 1, true))
        print('after')
    ",
        )
        .unwrap();
    assert_eq!(
        *recording.0.lock().unwrap(),
        [(LogLevel::Info, "after".to_owned())]
    );
}

#[test]
fn lua_print_stays_off_stdout() {
    const CHILD: &str = "KAGE_PRINT_REGRESSION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let root = tempfile::tempdir().unwrap();
        let plugins = root.path().join("plugins");
        let user = root.path().join("user");
        std::fs::create_dir(&plugins).unwrap();
        std::fs::create_dir(&user).unwrap();
        std::fs::write(
            plugins.join("example.lua"),
            "print('plugin-print-marker', 42)",
        )
        .unwrap();
        std::fs::write(user.join("init.lua"), "print('user-print-marker')").unwrap();
        let runtime = PluginRuntime::builder()
            .user_dir(Some(user))
            .build()
            .unwrap();
        let report = load_dir(&plugins, &runtime).unwrap();
        assert!(report.all_ok(), "{report:?}");
        assert_eq!(report.loaded.len(), 1);
        assert!(matches!(report.init, Some(Ok(()))));
        runtime.eval_plugin("other", "print('direct-print-marker', setmetatable({}, {__tostring = function() return 'converted' end}), nil)").unwrap();
        runtime.eval("print('shared-print-marker')").unwrap();
        runtime
            .eval_plugin(
                "other",
                r"
            tostring = function() return 'changed' end
            print('binary-print-marker', 'a\0b')
            print('nested-print-marker', setmetatable({}, {__tostring = function()
                print('inner-print-marker')
                return 'outer'
            end}))
            assert(not pcall(print, setmetatable({}, {__tostring = function() return {} end})))
            assert(print() == nil)
        ",
            )
            .unwrap();
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lua_print_stays_off_stdout", "--nocapture"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for marker in [
        "plugin-print-marker",
        "user-print-marker",
        "direct-print-marker",
        "shared-print-marker",
        "binary-print-marker",
        "nested-print-marker",
        "inner-print-marker",
    ] {
        assert!(
            !output
                .stdout
                .windows(marker.len())
                .any(|w| w == marker.as_bytes()),
            "print marker leaked to stdout: {marker:?}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output
                .stderr
                .windows(marker.len())
                .any(|w| w == marker.as_bytes()),
            "missing stderr marker: {marker:?}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
