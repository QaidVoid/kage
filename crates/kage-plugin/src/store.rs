//! `kage.store`: per-plugin persistent key-value state.
//!
//! Each plugin gets a private JSON file under the host store directory,
//! addressed by its file stem, so checkpoints, counters, and caches
//! survive a reload or a restart. A plugin sees only its own store; the
//! base surface (host eval, or a runtime built without a store directory)
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
use std::time::{Duration, Instant};

use mlua::{Lua, MultiValue, Table, Value};

use crate::api::{json_to_lua, lua_to_json};
use crate::error::PluginError;

/// One plugin's persisted state: a flat key to JSON-value map.
type StoreMap = BTreeMap<String, serde_json::Value>;

const BASE_ERR: &str =
    "kage.store is available only to a loaded plugin when the host configures a store directory";

/// How long a write waits for another process's lock. The wait blocks
/// the Lua owner thread and the instruction watchdog cannot interrupt
/// it, so it is bounded: a holder that never lets go raises instead of
/// stalling the plugin.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// How often a waiting write retries [`std::fs::File::try_lock`].
const LOCK_POLL: Duration = Duration::from_millis(50);

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
/// directory is created lazily on first write. A write holds a lock on
/// `<stem>.json.lock`, so writes from several runtimes or processes never
/// drop each other's keys, and the wait for that lock is bounded, so a
/// stuck holder raises instead of stalling the plugin.
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
            let value = lua_to_json(value)?;
            update(&set_path, |map| {
                map.insert(key, value);
                true
            })
        })?,
    )?;

    let delete_path = path.clone();
    store.set(
        "delete",
        lua.create_function(move |_, key: String| delete(&delete_path, &key))?,
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

/// Removes `key` from the store. A no-op when it is unset: nothing is
/// created, nothing is written, and write-side setup cannot fail it.
fn delete(path: &Path, key: &str) -> mlua::Result<()> {
    if !load(path)?.contains_key(key) {
        return Ok(());
    }
    update(path, |map| map.remove(key).is_some())
}

/// Applies `change` to the store with its lock held, saving when `change`
/// reports a change. Reads take no lock: a save replaces the file
/// atomically, so a reader sees the old map or the new one.
fn update(path: &Path, change: impl FnOnce(&mut StoreMap) -> bool) -> mlua::Result<()> {
    update_within(path, change, LOCK_WAIT)
}

/// [`update`] that waits at most `wait` for the lock, so tests can
/// shorten the bound.
fn update_within(
    path: &Path,
    change: impl FnOnce(&mut StoreMap) -> bool,
    wait: Duration,
) -> mlua::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            mlua::Error::external(format!("kage.store: mkdir {}: {e}", parent.display()))
        })?;
    }
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| {
            mlua::Error::external(format!("kage.store: open {}: {e}", lock_path.display()))
        })?;
    let deadline = Instant::now() + wait;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(mlua::Error::external(format!(
                    "kage.store: lock {}: {e}",
                    lock_path.display()
                )));
            }
        }
        if Instant::now() >= deadline {
            return Err(mlua::Error::external(format!(
                "kage.store: lock {}: another process holds it",
                lock_path.display()
            )));
        }
        std::thread::sleep(LOCK_POLL);
    }
    let mut map = load(path)?;
    let saved = if change(&mut map) {
        save(path, &map)
    } else {
        Ok(())
    };
    drop(lock);
    saved
}

/// Atomically persist the store map.
fn save(path: &Path, map: &StoreMap) -> mlua::Result<()> {
    let body = serde_json::to_vec_pretty(map)
        .map_err(|e| mlua::Error::external(format!("kage.store: encode: {e}")))?;
    kage_core::fsutil::atomic_write(path, &body)
        .map_err(|e| mlua::Error::external(format!("kage.store: write {}: {e}", path.display())))
}

/// The store file path for `stem` under `state_dir`.
#[must_use]
pub(crate) fn store_path(state_dir: &Path, stem: &str) -> PathBuf {
    state_dir.join(format!("{stem}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writers in parallel, as several runtimes or processes sharing one
    /// plugin's store are, keep every key.
    #[test]
    fn concurrent_writes_keep_every_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path(), "plugin");
        let writers: Vec<_> = (0..4)
            .map(|writer| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for n in 0..25 {
                        update(&path, |map| {
                            map.insert(format!("{writer}-{n}"), n.into());
                            true
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        assert_eq!(load(&path).unwrap().len(), 100);
    }

    /// Deleting an unset key touches nothing: no directories, no lock
    /// file, no rewrite of the store.
    #[test]
    fn deleting_an_unset_key_is_a_pure_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        delete(&nested.join("plugin.json"), "gone").unwrap();
        assert!(!nested.exists(), "no directory is created");

        // An existing store is not locked either when the key is unset.
        let existing = dir.path().join("plugin.json");
        save(&existing, &StoreMap::from([("kept".to_owned(), 1.into())])).unwrap();
        delete(&existing, "gone").unwrap();
        assert_eq!(load(&existing).unwrap().len(), 1, "the store is untouched");
        assert!(
            !dir.path().join("plugin.json.lock").exists(),
            "no lock file is created"
        );

        delete(&existing, "kept").unwrap();
        assert!(load(&existing).unwrap().is_empty());
    }

    /// A write whose lock another process holds gives up within the
    /// wait instead of blocking the Lua thread for good, and goes
    /// through once the holder lets go.
    #[test]
    fn a_held_lock_times_out_instead_of_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path(), "plugin");
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let holder = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .unwrap();
        holder.lock().unwrap();
        let error = update_within(
            &path,
            |map| {
                map.insert("x".to_owned(), 1.into());
                true
            },
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("another process holds it"),
            "{error}"
        );
        drop(holder);
        update_within(
            &path,
            |map| {
                map.insert("x".to_owned(), 1.into());
                true
            },
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(load(&path).unwrap().len(), 1);
    }
}
