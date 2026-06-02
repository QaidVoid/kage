//! `kage` Lua module: small surface plugins use to ask the host for
//! anything they cannot do safely on their own.
//!
//! v0.1 ships:
//! * `kage.now_ms()` returns wall-clock milliseconds since the Unix epoch.
//! * `kage.ui.notify(message, level?)` surfaces a one-line user-visible
//!   notification; `level` is `"info"` (default), `"warning"`, or
//!   `"error"`. Non-info levels are also recorded via the log sink so
//!   the severity is not lost. `kage.notify` is a back-compat alias for
//!   the same function (the pre-PE.B name).
//! * `kage.log(level, message)` records a structured log line; `level` is
//!   one of `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"`.
//! * `kage.config()` returns a copy of the host-supplied configuration
//!   table; `kage.plugin_config()` returns the calling plugin's own
//!   `[plugins.config.<stem>]` slice (empty on the base surface).
//! * `kage.api_version()` returns the integer surface generation and
//!   `kage.host_version()` the host crate version string;
//!   `kage.requires{ api = N }` raises at load time when the host is
//!   older than the generation the plugin needs.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mlua::{Lua, Table, Value};
use serde_json::json;

use crate::error::PluginError;

/// Generation of the `kage` plugin API surface.
///
/// Bumped whenever a binding is added or removed so a plugin can guard
/// with [`kage.requires`](requires) and fail loudly at load against an
/// incompatible host instead of erroring deep inside a missing binding.
/// Exposed to Lua as `kage.api_version()`.
pub const API_VERSION: i64 = 1;

/// Severity tier for [`HostLog::log`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    /// Tracing-grade verbosity.
    Trace,
    /// Debug-grade information.
    Debug,
    /// Routine progress messages.
    Info,
    /// Recoverable concerns the user might want to know about.
    Warn,
    /// Failures the host should surface prominently.
    Error,
}

impl LogLevel {
    /// Parse a Lua-supplied level string. Unknown values map to
    /// [`LogLevel::Info`] so a typo never silently drops a log line.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "trace" => Self::Trace,
            "debug" => Self::Debug,
            "warn" | "warning" => Self::Warn,
            "error" => Self::Error,
            _ => Self::Info,
        }
    }
}

/// Host sink for plugin notifications and log lines.
pub trait HostLog: Send {
    /// Show a short user-visible message. Print mode renders these to
    /// stderr; the TUI will pop a transient overlay.
    fn notify(&mut self, message: &str);
    /// Record a structured log line.
    fn log(&mut self, level: LogLevel, message: &str);
}

/// Default sink that writes notifications to stderr and drops log lines.
#[derive(Debug, Default)]
pub struct StderrHostLog;

impl HostLog for StderrHostLog {
    fn notify(&mut self, message: &str) {
        eprintln!("kage: {message}");
    }
    fn log(&mut self, level: LogLevel, message: &str) {
        eprintln!("kage [{level:?}] {message}");
    }
}

/// Shared, mutable handle to a [`HostLog`] used by Lua callbacks.
pub type SharedHostLog = Arc<Mutex<Box<dyn HostLog + Send>>>;

/// Construct a default shared host log backed by [`StderrHostLog`].
#[must_use]
pub fn default_host_log() -> SharedHostLog {
    Arc::new(Mutex::new(
        Box::new(StderrHostLog) as Box<dyn HostLog + Send>
    ))
}

/// Install the `kage` table on `lua`'s globals, wired to `sink` for
/// plugin-driven notifications and to `config` for `kage.config()`.
pub fn install(
    lua: &Lua,
    sink: SharedHostLog,
    config: serde_json::Value,
) -> Result<(), PluginError> {
    let kage = lua.create_table()?;

    kage.set("now_ms", lua.create_function(now_ms)?)?;
    kage.set("sleep_ms", lua.create_function(sleep_ms)?)?;
    kage.set("api_version", lua.create_function(|_, ()| Ok(API_VERSION))?)?;
    kage.set(
        "host_version",
        lua.create_function(|_, ()| Ok(env!("CARGO_PKG_VERSION")))?,
    )?;
    // `kage.requires{ api = N }` asserts host compatibility at load
    // time: when the host's API_VERSION is older than the generation the
    // plugin needs, raise so a stale plugin fails loudly up front.
    kage.set(
        "requires",
        lua.create_function(|_, spec: Table| {
            if let Some(required) = spec.get::<Option<i64>>("api")?
                && required > API_VERSION
            {
                return Err(mlua::Error::external(format!(
                    "kage.requires: plugin needs api_version >= {required}, \
                     host provides {API_VERSION}"
                )));
            }
            Ok(())
        })?,
    )?;

    let notify_sink = sink.clone();
    let notify = lua.create_function(move |_, (msg, level): (String, Option<String>)| {
        let escalate = match level.as_deref().unwrap_or("info") {
            "info" => None,
            "warning" | "warn" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            other => {
                return Err(mlua::Error::external(format!(
                    "kage.ui.notify: level must be \"info\", \"warning\", or \"error\", \
                     got \"{other}\""
                )));
            }
        };
        if let Ok(mut s) = notify_sink.lock() {
            s.notify(&msg);
            if let Some(level) = escalate {
                s.log(level, &msg);
            }
        }
        Ok(())
    })?;
    let ui = lua.create_table()?;
    ui.set("notify", notify.clone())?;
    kage.set("ui", ui)?;
    // Back-compat: `kage.notify(message)` predates the `kage.ui.*`
    // grouping. Same function; the optional level arg is additive.
    kage.set("notify", notify)?;

    let log_sink = sink;
    kage.set(
        "log",
        lua.create_function(move |_, (level, msg): (String, String)| {
            if let Ok(mut s) = log_sink.lock() {
                s.log(LogLevel::parse(&level), &msg);
            }
            Ok(())
        })?,
    )?;

    let config_for_lua = config;
    kage.set(
        "config",
        lua.create_function(move |lua, ()| json_to_lua(lua, &config_for_lua))?,
    )?;

    // Base `kage.plugin_config()` returns an empty table: only a loaded
    // plugin has per-plugin settings, attached on its proxy in
    // `plugin_env`. This base binding keeps the path resolvable for the
    // host eval surface and the anti-drift spec check.
    kage.set(
        "plugin_config",
        lua.create_function(|lua, ()| lua.create_table())?,
    )?;

    let json = lua.create_table()?;
    json.set(
        "decode",
        lua.create_function(|lua, raw: String| {
            let value: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| mlua::Error::external(format!("kage.json.decode: {e}")))?;
            json_to_lua(lua, &value)
        })?,
    )?;
    json.set(
        "encode",
        lua.create_function(|_, value: Value| {
            let v = lua_to_json(value)?;
            serde_json::to_string(&v)
                .map_err(|e| mlua::Error::external(format!("kage.json.encode: {e}")))
        })?,
    )?;
    kage.set("json", json)?;

    lua.globals().set("kage", kage)?;
    Ok(())
}

fn now_ms(_: &Lua, (): ()) -> mlua::Result<i64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    let ms = u128::from(now.as_secs()) * 1000 + u128::from(now.subsec_millis());
    i64::try_from(ms).map_err(|_| mlua::Error::external("timestamp overflows i64"))
}

/// Cap on one [`sleep_ms`] call. Plugins that need to wait longer loop
/// the sleep so the agent thread checks its own state between naps and
/// a host-side cancel never blocks behind a multi-second sleep.
const SLEEP_MS_MAX: i64 = 500;

fn sleep_ms(_: &Lua, ms: i64) -> mlua::Result<()> {
    if ms < 0 {
        return Err(mlua::Error::external(
            "kage.sleep_ms: duration must be non-negative",
        ));
    }
    if ms > SLEEP_MS_MAX {
        return Err(mlua::Error::external(format!(
            "kage.sleep_ms: duration {ms} exceeds cap of {SLEEP_MS_MAX} ms; \
             loop the sleep instead so cancellation stays responsive"
        )));
    }
    if ms > 0 {
        let duration = u64::try_from(ms).map_err(|e| mlua::Error::external(e.to_string()))?;
        std::thread::sleep(std::time::Duration::from_millis(duration));
    }
    Ok(())
}

/// Convert a `serde_json::Value` into the equivalent Lua [`Value`].
///
/// JSON arrays become 1-indexed Lua tables. Object keys are stringified.
/// Numeric values that fit in `i64` are returned as integers; anything
/// else falls through to `f64`.
pub fn json_to_lua(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
    Ok(match value {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else {
                Value::Number(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::String(lua.create_string(s)?),
        serde_json::Value::Array(items) => {
            let table = lua.create_table()?;
            for (idx, item) in items.iter().enumerate() {
                table.set(idx + 1, json_to_lua(lua, item)?)?;
            }
            Value::Table(table)
        }
        serde_json::Value::Object(map) => {
            let table = lua.create_table()?;
            for (k, v) in map {
                table.set(k.as_str(), json_to_lua(lua, v)?)?;
            }
            Value::Table(table)
        }
    })
}

/// Convert a Lua [`Value`] back into `serde_json::Value`. Functions,
/// userdata, light userdata, threads, and errors are unsupported and
/// surface as `serde_json::Value::Null`.
pub fn lua_to_json(value: Value) -> mlua::Result<serde_json::Value> {
    Ok(match value {
        Value::Boolean(b) => json!(b),
        Value::Integer(i) => json!(i),
        Value::Number(n) => json!(n),
        Value::String(s) => json!(s.to_str()?.to_owned()),
        Value::Table(t) => table_to_json(&t)?,
        Value::Error(err) => json!(err.to_string()),
        Value::Nil
        | Value::Function(_)
        | Value::Thread(_)
        | Value::UserData(_)
        | Value::LightUserData(_)
        | Value::Other(_) => serde_json::Value::Null,
    })
}

fn table_to_json(table: &Table) -> mlua::Result<serde_json::Value> {
    // A Lua table is "arrayish" when all keys are positive integers from
    // 1..=N. Otherwise, treat it as an object with stringified keys.
    let len = table.raw_len();
    if len > 0 {
        let mut all_dense = true;
        for i in 1..=len {
            let v: Value = table.raw_get(i)?;
            if matches!(v, Value::Nil) {
                all_dense = false;
                break;
            }
        }
        if all_dense {
            let mut arr = Vec::with_capacity(len);
            for i in 1..=len {
                arr.push(lua_to_json(table.raw_get(i)?)?);
            }
            return Ok(serde_json::Value::Array(arr));
        }
    }
    let mut map = serde_json::Map::new();
    for pair in table.clone().pairs::<Value, Value>() {
        let (k, v) = pair?;
        let key = match k {
            Value::String(s) => s.to_str()?.to_owned(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => n.to_string(),
            other => format!("{other:?}"),
        };
        map.insert(key, lua_to_json(v)?);
    }
    Ok(serde_json::Value::Object(map))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use mlua::Lua;
    use serde_json::json;

    use super::*;

    #[derive(Default)]
    struct Recording {
        notifies: Vec<String>,
        logs: Vec<(LogLevel, String)>,
    }
    impl HostLog for Recording {
        fn notify(&mut self, message: &str) {
            self.notifies.push(message.to_owned());
        }
        fn log(&mut self, level: LogLevel, message: &str) {
            self.logs.push((level, message.to_owned()));
        }
    }

    fn install_recording(lua: &Lua) -> Arc<Mutex<Recording>> {
        let rec = Arc::new(Mutex::new(Recording::default()));
        let sink: SharedHostLog = {
            #[derive(Clone)]
            struct Forwarder(Arc<Mutex<Recording>>);
            impl HostLog for Forwarder {
                fn notify(&mut self, message: &str) {
                    self.0.lock().unwrap().notify(message);
                }
                fn log(&mut self, level: LogLevel, message: &str) {
                    self.0.lock().unwrap().log(level, message);
                }
            }
            Arc::new(Mutex::new(
                Box::new(Forwarder(rec.clone())) as Box<dyn HostLog + Send>
            ))
        };
        install(
            lua,
            sink,
            json!({"model": "anthropic:claude", "cwd": "/work"}),
        )
        .unwrap();
        rec
    }

    #[test]
    fn now_ms_returns_recent_timestamp() {
        let lua = Lua::new();
        install(&lua, default_host_log(), json!({})).unwrap();
        let ms: i64 = lua.load("return kage.now_ms()").eval().unwrap();
        // After 2020-01-01.
        assert!(ms > 1_577_836_800_000);
    }

    #[test]
    fn api_version_is_exposed() {
        let lua = Lua::new();
        install(&lua, default_host_log(), json!({})).unwrap();
        let v: i64 = lua.load("return kage.api_version()").eval().unwrap();
        assert_eq!(v, API_VERSION);
        let host: String = lua.load("return kage.host_version()").eval().unwrap();
        assert!(!host.is_empty());
    }

    #[test]
    fn requires_passes_for_current_or_older_api() {
        let lua = Lua::new();
        install(&lua, default_host_log(), json!({})).unwrap();
        lua.load("kage.requires({ api = 1 })").exec().unwrap();
        lua.load("kage.requires({})").exec().unwrap();
    }

    #[test]
    fn requires_raises_for_newer_api() {
        let lua = Lua::new();
        install(&lua, default_host_log(), json!({})).unwrap();
        let res = lua.load("kage.requires({ api = 999 })").exec();
        assert!(res.is_err());
    }

    #[test]
    fn sleep_ms_waits_at_least_requested_duration() {
        let lua = Lua::new();
        install(&lua, default_host_log(), json!({})).unwrap();
        let start = std::time::Instant::now();
        lua.load("kage.sleep_ms(10)").exec().unwrap();
        assert!(start.elapsed() >= std::time::Duration::from_millis(10));
    }

    #[test]
    fn sleep_ms_rejects_negative_duration() {
        let lua = Lua::new();
        install(&lua, default_host_log(), json!({})).unwrap();
        assert!(lua.load("kage.sleep_ms(-1)").exec().is_err());
    }

    #[test]
    fn sleep_ms_rejects_above_cap() {
        let lua = Lua::new();
        install(&lua, default_host_log(), json!({})).unwrap();
        assert!(lua.load("kage.sleep_ms(10000)").exec().is_err());
    }

    #[test]
    fn sleep_ms_zero_is_a_noop() {
        let lua = Lua::new();
        install(&lua, default_host_log(), json!({})).unwrap();
        lua.load("kage.sleep_ms(0)").exec().unwrap();
    }

    #[test]
    fn notify_and_log_route_to_sink() {
        let lua = Lua::new();
        let rec = install_recording(&lua);
        lua.load("kage.notify('hello'); kage.log('warn', 'careful')")
            .exec()
            .unwrap();
        let r = rec.lock().unwrap();
        assert_eq!(r.notifies, vec!["hello".to_owned()]);
        assert_eq!(r.logs, vec![(LogLevel::Warn, "careful".to_owned())]);
    }

    #[test]
    fn ui_notify_info_only_notifies() {
        let lua = Lua::new();
        let rec = install_recording(&lua);
        lua.load("kage.ui.notify('hi')").exec().unwrap();
        let r = rec.lock().unwrap();
        assert_eq!(r.notifies, vec!["hi".to_owned()]);
        assert!(r.logs.is_empty());
    }

    #[test]
    fn ui_notify_warning_and_error_also_log() {
        let lua = Lua::new();
        let rec = install_recording(&lua);
        lua.load("kage.ui.notify('careful', 'warning'); kage.ui.notify('boom', 'error')")
            .exec()
            .unwrap();
        let r = rec.lock().unwrap();
        assert_eq!(r.notifies, vec!["careful".to_owned(), "boom".to_owned()]);
        assert_eq!(
            r.logs,
            vec![
                (LogLevel::Warn, "careful".to_owned()),
                (LogLevel::Error, "boom".to_owned()),
            ]
        );
    }

    #[test]
    fn ui_notify_rejects_unknown_level() {
        let lua = Lua::new();
        install_recording(&lua);
        let err = lua.load("kage.ui.notify('x', 'loud')").exec().unwrap_err();
        assert!(err.to_string().contains("level"));
    }

    #[test]
    fn notify_alias_matches_ui_notify() {
        let lua = Lua::new();
        let rec = install_recording(&lua);
        lua.load("kage.notify('legacy'); kage.notify('warned', 'warning')")
            .exec()
            .unwrap();
        let r = rec.lock().unwrap();
        assert_eq!(r.notifies, vec!["legacy".to_owned(), "warned".to_owned()]);
        assert_eq!(r.logs, vec![(LogLevel::Warn, "warned".to_owned())]);
    }

    #[test]
    fn config_returns_table_view_of_host_config() {
        let lua = Lua::new();
        install_recording(&lua);
        let model: String = lua.load("return kage.config().model").eval().unwrap();
        assert_eq!(model, "anthropic:claude");
        let cwd: String = lua.load("return kage.config().cwd").eval().unwrap();
        assert_eq!(cwd, "/work");
    }

    #[test]
    fn config_round_trips_nested_structures() {
        let lua = Lua::new();
        install(
            &lua,
            default_host_log(),
            json!({"nested": {"flags": [true, false]}}),
        )
        .unwrap();
        let v: bool = lua
            .load("return kage.config().nested.flags[1]")
            .eval()
            .unwrap();
        assert!(v);
    }

    #[test]
    fn unknown_log_level_falls_back_to_info() {
        assert_eq!(LogLevel::parse("zzz"), LogLevel::Info);
        assert_eq!(LogLevel::parse("warning"), LogLevel::Warn);
    }

    #[test]
    fn lua_table_round_trips_through_json_helpers() {
        let lua = Lua::new();
        let table: mlua::Table = lua
            .load("return { name = 'kage', tags = { 'rust', 'lua' } }")
            .eval()
            .unwrap();
        let json = lua_to_json(mlua::Value::Table(table)).unwrap();
        assert_eq!(json["name"], "kage");
        assert_eq!(json["tags"][0], "rust");
        let back = json_to_lua(&lua, &json).unwrap();
        if let mlua::Value::Table(t) = back {
            let name: String = t.get("name").unwrap();
            assert_eq!(name, "kage");
        } else {
            panic!("expected table");
        }
    }
}
