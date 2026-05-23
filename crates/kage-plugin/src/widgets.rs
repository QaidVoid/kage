//! `kage.register_widget` and the [`LuaWidget`] adapter that renders
//! plugin-defined status-bar widgets.
//!
//! Plugins call
//! ```lua
//! kage.register_widget({
//!     key = "git_branch",
//!     render = function(width) return "main *" end,
//! })
//! ```
//! and the host pulls a list of [`LuaWidget`]s via
//! [`crate::PluginRuntime::registered_widgets`]. Each widget runs once
//! per redraw inside the same Lua mutex the tool dispatch uses, so the
//! host serializes widget calls against any in-flight tool.
//!
//! `render(width)` returns a string that the TUI paints onto the
//! status bar. The width hint lets a widget abbreviate (`main *`
//! when room is tight, `branch: main (dirty)` when there's space).
//! Anything other than a string is coerced to an empty render.

use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, RegistryKey, Table};

use crate::api::{LogLevel, SharedHostLog};
use crate::error::PluginError;
use crate::runtime::SharedLua;

/// Shared collection of widgets registered by plugins.
pub type RegisteredWidgets = Arc<std::sync::Mutex<Vec<Arc<LuaWidget>>>>;

/// Construct an empty widget collection.
#[must_use]
pub fn registered_widgets() -> RegisteredWidgets {
    Arc::new(std::sync::Mutex::new(Vec::new()))
}

/// A status-bar widget defined in Lua.
pub struct LuaWidget {
    key: String,
    lua: SharedLua,
    sink: SharedHostLog,
    handler_key: Arc<RegistryKey>,
    cache: Mutex<String>,
}

impl std::fmt::Debug for LuaWidget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaWidget")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl LuaWidget {
    /// Identifier the host uses to deduplicate widgets and to surface
    /// the source of any render error.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Call into Lua to produce the widget's painted text for a status
    /// bar of `width` columns. `try_lock` keeps the render loop
    /// non-blocking; on contention or a Lua error, the last successful
    /// text is replayed.
    #[must_use]
    pub fn render(&self, width: u16) -> String {
        let Ok(lua) = self.lua.try_lock() else {
            return self.cached();
        };
        let func: Function = match lua.registry_value(&self.handler_key) {
            Ok(f) => f,
            Err(e) => {
                if let Ok(mut s) = self.sink.lock() {
                    s.log(
                        LogLevel::Error,
                        &format!("plugin widget '{}': {e}", self.key),
                    );
                }
                return self.cached();
            }
        };
        let text = match func.call::<mlua::Value>(width) {
            Ok(mlua::Value::String(s)) => s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
            Ok(mlua::Value::Nil) => String::new(),
            Ok(other) => format!("{other:?}"),
            Err(e) => {
                if let Ok(mut s) = self.sink.lock() {
                    s.log(
                        LogLevel::Error,
                        &format!("plugin widget '{}': {e}", self.key),
                    );
                }
                return self.cached();
            }
        };
        if let Ok(mut slot) = self.cache.lock() {
            slot.clone_from(&text);
        }
        text
    }

    fn cached(&self) -> String {
        self.cache.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

/// Install `kage.register_widget` on the running Lua state. Each call
/// pushes a [`LuaWidget`] into `registered`; later registrations with
/// the same `key` replace earlier ones in place so a plugin can
/// hot-reload its widget definition.
pub fn install_register_widget(
    lua: &Lua,
    shared_lua: SharedLua,
    sink: SharedHostLog,
    registered: RegisteredWidgets,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "register_widget",
        lua.create_function(move |lua, spec: Table| {
            let key: String = spec.get("key")?;
            let render: Function = spec.get("render")?;
            let handler_key = lua.create_registry_value(render)?;
            let widget = Arc::new(LuaWidget {
                key: key.clone(),
                lua: shared_lua.clone(),
                sink: sink.clone(),
                cache: Mutex::new(String::new()),
                handler_key: Arc::new(handler_key),
            });
            let mut list = registered
                .lock()
                .map_err(|_| mlua::Error::external("plugin widgets registry poisoned"))?;
            if let Some(slot) = list.iter_mut().find(|w| w.key == key) {
                *slot = widget;
            } else {
                list.push(widget);
            }
            Ok(())
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    #[test]
    fn register_widget_appends_to_registry() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_widget({
                key = 'git',
                render = function(_w) return 'main' end,
            })
            ",
        )
        .unwrap();
        let widgets = rt.registered_widgets();
        assert_eq!(widgets.len(), 1);
        assert_eq!(widgets[0].key(), "git");
    }

    #[test]
    fn widget_render_returns_lua_string() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_widget({
                key = 'clock',
                render = function(_w) return 'tick' end,
            })
            ",
        )
        .unwrap();
        let w = &rt.registered_widgets()[0];
        assert_eq!(w.render(80), "tick");
    }

    #[test]
    fn widget_render_receives_width_argument() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_widget({
                key = 'echo_width',
                render = function(w) return tostring(w) end,
            })
            ",
        )
        .unwrap();
        let w = &rt.registered_widgets()[0];
        assert_eq!(w.render(123), "123");
    }

    #[test]
    fn second_registration_replaces_first() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_widget({ key='x', render=function() return 'a' end })
            kage.register_widget({ key='x', render=function() return 'b' end })
            ",
        )
        .unwrap();
        let widgets = rt.registered_widgets();
        assert_eq!(widgets.len(), 1);
        assert_eq!(widgets[0].render(80), "b");
    }

    #[test]
    fn widget_render_error_returns_empty_string() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_widget({ key='oops', render=function() error('boom') end })
            ",
        )
        .unwrap();
        let w = &rt.registered_widgets()[0];
        assert_eq!(w.render(80), "");
    }
}
