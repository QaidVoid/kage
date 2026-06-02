//! `kage.store`: per-plugin persistent key-value state.
//!
//! Each plugin gets a private JSON file under the host state dir,
//! addressed by its file stem, so checkpoints, counters, and caches
//! survive a reload or a restart. A plugin sees only its own store; the
//! base surface (host eval, or a runtime built without a state dir)
//! exposes stubs that raise, so the binding resolves but no state can
//! leak between plugins.
//!
//! ```lua
//! kage.store.set("count", (kage.store.get("count") or 0) + 1)
//! local keys = kage.store.keys()
//! kage.store.delete("stale")
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mlua::{Lua, MultiValue, Table, Value};

use crate::api::{json_to_lua, lua_to_json};
use crate::error::PluginError;

/// One plugin's persisted state: a flat key to JSON-value map.
type StoreMap = BTreeMap<String, serde_json::Value>;

const BASE_ERR: &str =
    "kage.store is available only to a loaded plugin when the host configures a state dir";

/// Install a base `kage.store` whose operations raise [`BASE_ERR`].
///
/// A loaded plugin's proxy gets a real store from [`install_for_plugin`];
/// these base stubs keep `kage.store.*` resolvable for the host eval
/// surface and the anti-drift spec check without exposing shared state.
pub(crate) fn install_base(lua: &Lua) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let store = lua.create_table()?;
    for name in ["get", "set", "delete", "keys"] {
        store.set(
            name,
            lua.create_function(|_, _: MultiValue| -> mlua::Result<Value> {
                Err(mlua::Error::external(BASE_ERR))
            })?,
        )?;
    }
    kage.set("store", store)?;
    Ok(())
}

/// Attach a real `kage.store` onto a single plugin's `kage` proxy,
/// backed by `path` (`<state_dir>/<stem>.json`). Each call loads and
/// rewrites the file so state is durable across reloads; the parent
/// directory is created lazily on first write.
pub(crate) fn install_for_plugin(lua: &Lua, pkage: &Table, path: PathBuf) -> mlua::Result<()> {
    let store = lua.create_table()?;

    let get_path = path.clone();
    store.set(
        "get",
        lua.create_function(move |lua, key: String| {
            let map = load(&get_path)?;
            match map.get(&key) {
                Some(value) => json_to_lua(lua, value),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let set_path = path.clone();
    store.set(
        "set",
        lua.create_function(move |_, (key, value): (String, Value)| {
            let mut map = load(&set_path)?;
            map.insert(key, lua_to_json(value)?);
            save(&set_path, &map)
        })?,
    )?;

    let delete_path = path.clone();
    store.set(
        "delete",
        lua.create_function(move |_, key: String| {
            let mut map = load(&delete_path)?;
            if map.remove(&key).is_some() {
                save(&delete_path, &map)?;
            }
            Ok(())
        })?,
    )?;

    let keys_path = path;
    store.set(
        "keys",
        lua.create_function(move |lua, ()| {
            let map = load(&keys_path)?;
            let out = lua.create_table()?;
            for (index, key) in map.keys().enumerate() {
                out.set(index + 1, key.clone())?;
            }
            Ok(out)
        })?,
    )?;

    pkage.set("store", store)?;
    Ok(())
}

/// Load the store file, treating a missing file as an empty map. A
/// corrupt file is surfaced as an error rather than silently reset.
fn load(path: &Path) -> mlua::Result<StoreMap> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| {
            mlua::Error::external(format!("kage.store: parse {}: {e}", path.display()))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StoreMap::new()),
        Err(e) => Err(mlua::Error::external(format!(
            "kage.store: read {}: {e}",
            path.display()
        ))),
    }
}

/// Atomically persist the store map, creating the parent dir on demand.
fn save(path: &Path, map: &StoreMap) -> mlua::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            mlua::Error::external(format!("kage.store: mkdir {}: {e}", parent.display()))
        })?;
    }
    let body = serde_json::to_vec_pretty(map)
        .map_err(|e| mlua::Error::external(format!("kage.store: encode: {e}")))?;
    kage_tools::atomic::atomic_write(path, &body)
        .map_err(|e| mlua::Error::external(format!("kage.store: write {}: {e}", path.display())))
}

/// The store file path for `stem` under `state_dir`.
#[must_use]
pub(crate) fn store_path(state_dir: &Path, stem: &str) -> PathBuf {
    state_dir.join(format!("{stem}.json"))
}
