//! `kage.theme.list` / `kage.theme.set` / `kage.theme.current`:
//! plugin-facing theme manager.
//!
//! Reading is synchronous off a host-maintained snapshot
//! ([`SharedThemeState`]); the host refreshes `current` and
//! `available` on its redraw cadence. Switching is deferred: a plugin
//! writes the requested name into [`SharedThemeRequest`] and the host
//! drains it between turns, validating and applying it through the
//! same path as `:theme set`. Lets a plugin auto-toggle light/dark on
//! a system event without owning the theme registry.

use std::sync::{Arc, Mutex};

use mlua::{Lua, Table, Value};

use crate::error::PluginError;

/// Host-maintained theme snapshot the read APIs return.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThemeState {
    /// Active theme name.
    pub current: String,
    /// Names a plugin may pass to `kage.theme.set`, in host order.
    pub available: Vec<String>,
}

/// Shared theme snapshot. The host overwrites it as the active theme
/// or theme list changes; plugins read it via `kage.theme.current()`
/// and `kage.theme.list()`.
pub type SharedThemeState = Arc<Mutex<ThemeState>>;

/// Construct an empty theme snapshot.
#[must_use]
pub fn shared_theme_state() -> SharedThemeState {
    Arc::new(Mutex::new(ThemeState::default()))
}

/// Pending theme-switch request. `Some(name)` means a plugin asked
/// the host to switch; the host validates and applies it, then
/// clears the slot.
pub type SharedThemeRequest = Arc<Mutex<Option<String>>>;

/// Construct an empty theme-request slot.
#[must_use]
pub fn shared_theme_request() -> SharedThemeRequest {
    Arc::new(Mutex::new(None))
}

/// Install `kage.theme.{list,set,current}` on the running Lua state.
pub fn install_theme(
    lua: &Lua,
    state: SharedThemeState,
    request: SharedThemeRequest,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let theme = lua.create_table()?;

    let current_state = state.clone();
    theme.set(
        "current",
        lua.create_function(move |_, ()| {
            Ok(current_state
                .lock()
                .map(|s| s.current.clone())
                .unwrap_or_default())
        })?,
    )?;

    let list_state = state;
    theme.set(
        "list",
        lua.create_function(move |lua, ()| {
            let names = list_state
                .lock()
                .map(|s| s.available.clone())
                .unwrap_or_default();
            let arr = lua.create_table()?;
            for (idx, name) in names.into_iter().enumerate() {
                arr.set(idx + 1, name)?;
            }
            Ok(arr)
        })?,
    )?;

    let request_slot = request;
    theme.set(
        "set",
        lua.create_function(move |_, name: Value| {
            let Value::String(name) = name else {
                return Err(mlua::Error::external(
                    "kage.theme.set: name must be a string",
                ));
            };
            let name = name.to_str()?.to_owned();
            if name.is_empty() {
                return Err(mlua::Error::external(
                    "kage.theme.set: name must be non-empty",
                ));
            }
            let mut slot = request_slot
                .lock()
                .map_err(|_| mlua::Error::external("plugin theme request poisoned"))?;
            *slot = Some(name);
            Ok(())
        })?,
    )?;

    kage.set("theme", theme)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    #[test]
    fn current_reads_the_host_snapshot() {
        let rt = PluginRuntime::new().unwrap();
        if let Ok(mut s) = rt.shared_theme_state().lock() {
            s.current = "kage-dark".to_owned();
            s.available = vec!["kage-dark".to_owned(), "ansi".to_owned()];
        }
        let ok: bool = rt
            .eval("return kage.theme.current() == 'kage-dark'")
            .unwrap()
            .as_boolean()
            .unwrap_or(false);
        assert!(ok);
    }

    #[test]
    fn list_returns_available_names() {
        let rt = PluginRuntime::new().unwrap();
        if let Ok(mut s) = rt.shared_theme_state().lock() {
            s.available = vec!["a".to_owned(), "b".to_owned()];
        }
        let n: i64 = rt
            .eval("return #kage.theme.list()")
            .unwrap()
            .as_integer()
            .unwrap_or(0);
        assert_eq!(n, 2);
        let ok: bool = rt
            .eval("local t = kage.theme.list(); return t[1] == 'a' and t[2] == 'b'")
            .unwrap()
            .as_boolean()
            .unwrap_or(false);
        assert!(ok);
    }

    #[test]
    fn set_queues_a_request_the_host_drains() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.theme.set('solarized')").unwrap();
        assert_eq!(rt.take_theme_request().as_deref(), Some("solarized"));
        // Drained: a second take is empty.
        assert_eq!(rt.take_theme_request(), None);
    }

    #[test]
    fn set_rejects_non_string_and_empty() {
        let rt = PluginRuntime::new().unwrap();
        assert!(rt.eval("kage.theme.set(42)").is_err());
        assert!(rt.eval("kage.theme.set('')").is_err());
        assert_eq!(rt.take_theme_request(), None);
    }
}
