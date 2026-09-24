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
//!
//! One renderer paints every block of its kind, so output is retained
//! per distinct payload. The host's render call never runs Lua: a
//! payload seen before returns its retained lines at once. A new
//! payload is computed on the Lua owner thread, waiting briefly only
//! when the owner is idle; while the owner is busy it yields no lines
//! and the redraw flag is raised once the owner frees up. A renderer is
//! a pure function of its payload, so retained lines are never
//! refreshed on a timer.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use kage_core::sync::lock;

use mlua::{Function, Lua, RegistryKey, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua};
use crate::chrome::{ChromeLine, parse_lines};
use crate::error::PluginError;
use crate::host::{self, LuaHost, WeakHost};
use crate::retained::COLD_WAIT;
use crate::watchdog;

/// Payloads retained per generation. Two generations are kept, so a
/// renderer holds at most twice this many outputs.
const GENERATION: usize = 256;

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
    host: LuaHost,
    sink: SharedHostLog,
    handler_key: Arc<RegistryKey>,
    cache: Arc<Mutex<BlockCache>>,
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

    /// Styled lines for a host-built `block` payload (a JSON object the
    /// host shapes per block variant: always `kind` + `width`, plus
    /// `text` / `name` / `output` / `folded` / ... as relevant).
    ///
    /// Returns retained lines for a payload rendered before, or `None`
    /// while the payload is not computed yet because the owner thread is
    /// busy. Empty lines mean the Lua renderer raised (logged to the
    /// sink) or returned a non-conforming value, so the host can make the
    /// failure visible.
    #[must_use]
    pub fn render(&self, payload: &serde_json::Value) -> Option<Vec<ChromeLine>> {
        let key = payload_key(payload);
        let mut cache = lock(&self.cache);
        if let Some(lines) = cache.get(key) {
            return Some(lines);
        }
        if cache.pending.contains(&key) {
            return None;
        }
        if !self.host.is_idle() {
            self.host.note_missed_render();
            return None;
        }
        cache.pending.insert(key);
        drop(cache);

        let target = Arc::clone(&self.cache);
        let redraw = self.host.redraw_flag();
        let blocks = self.host.blocks_flag();
        let kind = self.kind.clone();
        let sink = Arc::clone(&self.sink);
        let handler = Arc::clone(&self.handler_key);
        let payload = payload.clone();
        let queued = self.host.queue(move |lua| {
            let lines = render_block(lua, &kind, &sink, &handler, &payload);
            let mut cache = lock(&target);
            cache.pending.remove(&key);
            cache.insert(key, lines);
            blocks.store(true, Ordering::SeqCst);
            redraw.store(true, Ordering::SeqCst);
        });
        let Ok(done) = queued else {
            lock(&self.cache).pending.remove(&key);
            return None;
        };
        let _ = done.recv_timeout(COLD_WAIT);
        lock(&self.cache).get(key)
    }
}

fn render_block(
    lua: &Lua,
    kind: &str,
    sink: &SharedHostLog,
    handler_key: &RegistryKey,
    payload: &serde_json::Value,
) -> Vec<ChromeLine> {
    let fail = |e: &dyn std::fmt::Display| {
        let mut s = lock(sink);
        s.log(
            LogLevel::Error,
            &format!("plugin block renderer `{kind}`: {e}"),
        );
        Vec::new()
    };
    let func: Function = match lua.registry_value(handler_key) {
        Ok(f) => f,
        Err(e) => return fail(&e),
    };
    let block = match json_to_lua(lua, payload) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    match watchdog::run(lua, watchdog::BUDGET, || func.call::<Value>(block)) {
        Ok(value) => parse_lines(&value),
        Err(e) => fail(&e),
    }
}

fn payload_key(payload: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    payload.to_string().hash(&mut hasher);
    hasher.finish()
}

/// Retained lines by payload hash, bounded by dropping the older of two
/// generations when the current one fills up.
#[derive(Default)]
struct BlockCache {
    current: HashMap<u64, Vec<ChromeLine>>,
    previous: HashMap<u64, Vec<ChromeLine>>,
    pending: HashSet<u64>,
}

impl BlockCache {
    fn get(&mut self, key: u64) -> Option<Vec<ChromeLine>> {
        if let Some(lines) = self.current.get(&key) {
            return Some(lines.clone());
        }
        let lines = self.previous.remove(&key)?;
        self.insert(key, lines.clone());
        Some(lines)
    }

    fn insert(&mut self, key: u64, lines: Vec<ChromeLine>) {
        if self.current.len() >= GENERATION {
            self.previous = std::mem::take(&mut self.current);
        }
        self.current.insert(key, lines);
    }
}

/// Install `kage.register_block_renderer(kind, fn|nil)` on the
/// running Lua state. A function registers (or replaces) the
/// renderer for `kind`; `nil` removes it.
///
/// # Errors
///
/// Returns [`PluginError`] if the `kage` global is missing.
pub(crate) fn install_block_renderers(
    lua: &Lua,
    host: WeakHost,
    sink: SharedHostLog,
    registered: &SharedBlockRenderers,
) -> Result<(), PluginError> {
    let registered = Arc::downgrade(registered);
    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "register_block_renderer",
        lua.create_function(move |lua, (kind, handler): (String, Value)| {
            if kind.is_empty() {
                return Err(mlua::Error::external(
                    "kage.register_block_renderer: `kind` is required",
                ));
            }
            let registered = host::upgrade(&registered)?;
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
                            host: host.upgrade()?,
                            sink: sink.clone(),
                            handler_key: Arc::new(key),
                            cache: Arc::default(),
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
        let lines = r
            .render(&serde_json::json!({
                "kind": "demo:card", "text": "hello", "width": 42
            }))
            .unwrap();
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
                .is_some_and(|lines| lines.is_empty())
        );
    }
}
