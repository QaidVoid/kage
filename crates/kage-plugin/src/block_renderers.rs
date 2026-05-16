//! `kage.register_block_renderer(kind, fn)`: plugin-owned rendering
//! for a custom conversation block.
//!
//! A plugin that pushes `Block::Custom { kind = "myplugin:card" }`
//! entries (via `kage.session.append_entry` or a tool that emits a
//! custom block) can fully own how that kind draws. The renderer
//! function receives `{ kind, text, width }` and returns the same
//! styled-line shape as [`crate::chrome`] (`kage.ui.set_header`): a
//! string, a span table, or an array of either. The host bridges the
//! result through the `BlockWidget` registry, so a plugin can
//! re-skin a block in pure Lua - the Emacs-style overhaul seam.
//!
//! This deliberately reuses [`ChromeLine`] / [`crate::chrome`]'s
//! parser so authors learn one return shape for every plugin-drawn
//! surface (header, footer, block).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua};
use crate::chrome::{ChromeLine, parse_lines};
use crate::error::PluginError;
use crate::runtime::SharedLua;

/// Shared map of custom block kind -> its Lua renderer. The host
/// snapshots this after load and registers each into the TUI's
/// block-renderer registry.
pub type SharedBlockRenderers = Arc<Mutex<BTreeMap<String, Arc<LuaBlockRenderer>>>>;

/// Construct an empty block-renderer map.
#[must_use]
pub fn shared_block_renderers() -> SharedBlockRenderers {
    Arc::new(Mutex::new(BTreeMap::new()))
}

/// A custom-block renderer defined in Lua.
pub struct LuaBlockRenderer {
    kind: String,
    lua: SharedLua,
    sink: SharedHostLog,
    handler_key: Arc<mlua::RegistryKey>,
}

impl std::fmt::Debug for LuaBlockRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaBlockRenderer")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl LuaBlockRenderer {
    /// The custom block kind this renderer paints.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Call into Lua with a host-built `block` payload (a JSON object
    /// the host shapes per block variant: always `kind` + `width`,
    /// plus `text` / `name` / `output` / `folded` / ... as relevant).
    /// A Lua error, poisoned mutex, or non-conforming return logs to
    /// the sink and yields no lines; the host then paints the
    /// built-in block so a broken renderer never blanks the
    /// conversation silently.
    #[must_use]
    pub fn render(&self, payload: &serde_json::Value) -> Vec<ChromeLine> {
        let Ok(lua) = self.lua.lock() else {
            return Vec::new();
        };
        let func: Function = match lua.registry_value(&self.handler_key) {
            Ok(f) => f,
            Err(e) => {
                self.log_error(&e);
                return Vec::new();
            }
        };
        let block = match json_to_lua(&lua, payload) {
            Ok(v) => v,
            Err(e) => {
                self.log_error(&e);
                return Vec::new();
            }
        };
        match func.call::<Value>(block) {
            Ok(value) => parse_lines(&value),
            Err(e) => {
                self.log_error(&e);
                Vec::new()
            }
        }
    }

    fn log_error(&self, e: &dyn std::fmt::Display) {
        if let Ok(mut s) = self.sink.lock() {
            s.log(
                LogLevel::Error,
                &format!("plugin block renderer `{}`: {e}", self.kind),
            );
        }
    }
}

/// Install `kage.register_block_renderer(kind, fn|nil)` on the
/// running Lua state. A function registers (or replaces) the
/// renderer for `kind`; `nil` removes it.
///
/// # Errors
///
/// Returns [`PluginError`] if the `kage` global is missing.
pub fn install_block_renderers(
    lua: &Lua,
    shared_lua: SharedLua,
    sink: SharedHostLog,
    registered: SharedBlockRenderers,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "register_block_renderer",
        lua.create_function(move |lua, (kind, handler): (String, Value)| {
            if kind.is_empty() {
                return Err(mlua::Error::external(
                    "kage.register_block_renderer: `kind` is required",
                ));
            }
            let mut map = registered
                .lock()
                .map_err(|_| mlua::Error::external("plugin block renderers map poisoned"))?;
            match handler {
                Value::Nil => {
                    map.remove(&kind);
                    Ok(())
                }
                Value::Function(f) => {
                    let key = lua.create_registry_value(f)?;
                    map.insert(
                        kind.clone(),
                        Arc::new(LuaBlockRenderer {
                            kind,
                            lua: shared_lua.clone(),
                            sink: sink.clone(),
                            handler_key: Arc::new(key),
                        }),
                    );
                    Ok(())
                }
                _ => Err(mlua::Error::external(
                    "kage.register_block_renderer: expected a function or nil",
                )),
            }
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    #[test]
    fn register_block_renderer_records_and_renders() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r#"
            kage.register_block_renderer("demo:card", function(b)
                return "[" .. b.kind .. "] " .. b.text .. " @" .. b.width
            end)
            "#,
        )
        .unwrap();
        let map = rt.registered_block_renderers();
        assert_eq!(map.len(), 1);
        let r = &map[0];
        assert_eq!(r.kind(), "demo:card");
        let lines = r.render(&serde_json::json!({
            "kind": "demo:card", "text": "hello", "width": 42
        }));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].text, "[demo:card] hello @42");
    }

    #[test]
    fn nil_unregisters_a_renderer() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r#"
            kage.register_block_renderer("k", function() return "x" end)
            kage.register_block_renderer("k", nil)
            "#,
        )
        .unwrap();
        assert!(rt.registered_block_renderers().is_empty());
    }

    #[test]
    fn empty_kind_is_rejected() {
        let rt = PluginRuntime::new().unwrap();
        assert!(
            rt.eval(r#"kage.register_block_renderer("", function() end)"#)
                .is_err()
        );
    }

    #[test]
    fn broken_renderer_yields_no_lines_not_a_panic() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(r#"kage.register_block_renderer("b", function() error("boom") end)"#)
            .unwrap();
        let map = rt.registered_block_renderers();
        assert!(
            map[0]
                .render(&serde_json::json!({ "kind": "b", "text": "t" }))
                .is_empty()
        );
    }
}
