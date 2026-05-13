//! `kage.context_usage` / `kage.compact`: read-only inspection of the
//! current turn's token usage and a plugin-initiated trigger for
//! compaction.
//!
//! Both APIs route through small shared cells. The host keeps
//! [`SharedUsage`] current after every turn so plugins read the latest
//! numbers without an RPC. [`SharedCompactRequest`] holds an optional
//! pending request the host drains between turns; the inner
//! `Option<String>` is the prompt the plugin supplied (currently
//! advisory: PE.C.4's `on_compact_prepare` hook is the proper place to
//! rewrite the compaction prompt).

use std::sync::{Arc, Mutex};

use mlua::{Lua, Table, Value};

use crate::api::json_to_lua;
use crate::error::PluginError;

/// Shared per-turn token usage snapshot. The host overwrites the
/// inner value after every assistant turn; plugins read it via
/// `kage.context_usage()`.
pub type SharedUsage = Arc<Mutex<serde_json::Value>>;

/// Construct an empty usage snapshot. Returns
/// `serde_json::Value::Null` until the host pushes the first update.
#[must_use]
pub fn shared_usage() -> SharedUsage {
    Arc::new(Mutex::new(serde_json::Value::Null))
}

/// Pending compact request. `Some(prompt)` (with `prompt` possibly
/// empty) means "the plugin asked for a compaction"; the host drains
/// this slot between turns and dispatches its own compact run.
pub type SharedCompactRequest = Arc<Mutex<Option<String>>>;

/// Construct an empty compact-request slot.
#[must_use]
pub fn shared_compact_request() -> SharedCompactRequest {
    Arc::new(Mutex::new(None))
}

/// Install `kage.context_usage()` and `kage.compact(prompt?)` on the
/// running Lua state, wired to the supplied shared cells.
pub fn install_lifecycle(
    lua: &Lua,
    usage: SharedUsage,
    compact: SharedCompactRequest,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;

    let usage_for_lua = usage;
    kage.set(
        "context_usage",
        lua.create_function(move |lua, ()| {
            let guard = usage_for_lua
                .lock()
                .map_err(|_| mlua::Error::external("plugin usage mutex poisoned"))?;
            json_to_lua(lua, &guard)
        })?,
    )?;

    let compact_for_lua = compact;
    kage.set(
        "compact",
        lua.create_function(move |_lua, prompt: Value| {
            let prompt_str = match prompt {
                Value::Nil => String::new(),
                Value::String(s) => s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
                other => {
                    return Err(mlua::Error::external(format!(
                        "kage.compact: expected string or nil prompt, got {other:?}"
                    )));
                }
            };
            let mut slot = compact_for_lua
                .lock()
                .map_err(|_| mlua::Error::external("plugin compact mutex poisoned"))?;
            *slot = Some(prompt_str);
            Ok(())
        })?,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    #[test]
    fn context_usage_returns_table_from_host_snapshot() {
        let rt = PluginRuntime::new().unwrap();
        rt.set_usage(serde_json::json!({
            "model": "anthropic:claude",
            "input_tokens": 123,
            "context_window": 200_000,
        }));
        let model = rt.eval("return kage.context_usage().model").unwrap();
        let model = match model {
            mlua::Value::String(s) => s.to_str().unwrap().to_owned(),
            other => panic!("expected string, got {other:?}"),
        };
        assert_eq!(model, "anthropic:claude");
        let tokens = rt
            .eval("return kage.context_usage().input_tokens")
            .unwrap()
            .as_integer()
            .unwrap();
        assert_eq!(tokens, 123);
    }

    #[test]
    fn context_usage_returns_nil_before_host_update() {
        let rt = PluginRuntime::new().unwrap();
        let v = rt.eval("return kage.context_usage()").unwrap();
        assert!(matches!(v, mlua::Value::Nil));
    }

    #[test]
    fn compact_with_no_arg_sets_empty_request() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.compact()").unwrap();
        let req = rt.take_compact_request();
        assert_eq!(req, Some(String::new()));
    }

    #[test]
    fn compact_with_prompt_carries_the_prompt() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.compact('summarize tightly')").unwrap();
        let req = rt.take_compact_request();
        assert_eq!(req, Some("summarize tightly".to_owned()));
    }

    #[test]
    fn take_compact_request_clears_the_slot() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.compact()").unwrap();
        let _ = rt.take_compact_request();
        assert!(rt.take_compact_request().is_none());
    }

    #[test]
    fn compact_with_non_string_arg_raises() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt.eval("kage.compact(123)").unwrap_err();
        assert!(err.to_string().contains("string or nil"));
    }
}
