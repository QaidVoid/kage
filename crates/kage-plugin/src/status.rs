//! `kage.set_status` / `kage.clear_status`: shared key/value map the
//! host's status bar paints alongside built-in pills.
//!
//! Plugins push transient labels (`git_branch = "main *"`,
//! `lsp = "rust-analyzer ok"`) without owning a renderer; the host
//! reads the map per redraw and concatenates non-empty values into the
//! status bar's right edge. Iteration order is the key's lexicographic
//! sort so output is deterministic across redraws.
//!
//! Use [`kage.register_widget`](crate::widgets) when a plugin needs to
//! react to the available width or render dynamic content per frame;
//! `set_status` is for static labels the plugin updates on events.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mlua::{Lua, Table, Value};

use crate::error::PluginError;

/// Shared status-map handle. The host reads it per redraw; plugins
/// write it from inside `kage.set_status` / `kage.clear_status`.
pub type SharedStatus = Arc<Mutex<BTreeMap<String, String>>>;

/// Construct an empty status map.
#[must_use]
pub fn shared_status() -> SharedStatus {
    Arc::new(Mutex::new(BTreeMap::new()))
}

/// Install `kage.set_status(key, text)` and `kage.clear_status(key)`
/// on the running Lua state, routing writes into `status`.
pub fn install_status(lua: &Lua, status: SharedStatus) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;

    let set_status = status.clone();
    kage.set(
        "set_status",
        lua.create_function(move |_lua, (key, text): (String, Value)| {
            let mut map = set_status
                .lock()
                .map_err(|_| mlua::Error::external("plugin status map poisoned"))?;
            let text_str = match text {
                Value::Nil => {
                    map.remove(&key);
                    return Ok(());
                }
                Value::String(s) => s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
                Value::Boolean(b) => b.to_string(),
                Value::Integer(i) => i.to_string(),
                Value::Number(n) => n.to_string(),
                _ => {
                    return Err(mlua::Error::external(
                        "kage.set_status: text must be a string, number, boolean, or nil",
                    ));
                }
            };
            if text_str.is_empty() {
                map.remove(&key);
            } else {
                map.insert(key, text_str);
            }
            Ok(())
        })?,
    )?;

    let clear_status = status;
    kage.set(
        "clear_status",
        lua.create_function(move |_lua, key: String| {
            let mut map = clear_status
                .lock()
                .map_err(|_| mlua::Error::external("plugin status map poisoned"))?;
            map.remove(&key);
            Ok(())
        })?,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    #[test]
    fn set_status_inserts_value() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.set_status('git', 'main *')").unwrap();
        let s = rt.status_snapshot();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0], ("git".to_owned(), "main *".to_owned()));
    }

    #[test]
    fn clear_status_removes_value() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.set_status('git', 'main')").unwrap();
        rt.eval("kage.clear_status('git')").unwrap();
        assert!(rt.status_snapshot().is_empty());
    }

    #[test]
    fn set_status_nil_clears_entry() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.set_status('git', 'main')").unwrap();
        rt.eval("kage.set_status('git', nil)").unwrap();
        assert!(rt.status_snapshot().is_empty());
    }

    #[test]
    fn set_status_empty_string_clears_entry() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.set_status('git', 'main')").unwrap();
        rt.eval("kage.set_status('git', '')").unwrap();
        assert!(rt.status_snapshot().is_empty());
    }

    #[test]
    fn status_snapshot_sorts_by_key() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.set_status('zeta', '3')
            kage.set_status('alpha', '1')
            kage.set_status('mu', '2')
            ",
        )
        .unwrap();
        let s = rt.status_snapshot();
        let keys: Vec<&str> = s.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn set_status_replaces_existing_value() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.set_status('git', 'main')").unwrap();
        rt.eval("kage.set_status('git', 'feature/x')").unwrap();
        let s = rt.status_snapshot();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].1, "feature/x");
    }

    #[test]
    fn set_status_accepts_number_and_bool() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.set_status('n', 42)").unwrap();
        rt.eval("kage.set_status('b', true)").unwrap();
        let s = rt.status_snapshot();
        let map: std::collections::HashMap<_, _> = s.into_iter().collect();
        assert_eq!(map["n"], "42");
        assert_eq!(map["b"], "true");
    }
}
