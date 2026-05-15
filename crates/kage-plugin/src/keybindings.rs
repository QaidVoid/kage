//! `kage.register_keybinding(spec, handler)` and the keybinding
//! registry.
//!
//! A plugin binds a chord to a Lua handler:
//! ```lua
//! kage.register_keybinding("ctrl+shift+x", function() kage.notify("hi") end)
//! kage.register_keybinding({ key = "f5", description = "reload" }, reload)
//! ```
//! The handler runs on the host's worker thread through the coroutine
//! bridge, exactly like a plugin command, so it may call blocking
//! `kage.ui.*` dialogs.
//!
//! Chords are normalised to a canonical lowercase form here
//! (`shift+Ctrl+X` -> `ctrl+shift+x`); the host parses that same form
//! into a terminal key matcher. Conflicts with built-in bindings are
//! resolved in favour of the plugin (the host checks plugin chords
//! first), but binding a [`RESERVED_CHORDS`] chord logs a warning so
//! the author knows they shadowed something load-bearing.

use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, RegistryKey, Table, Value};

use crate::api::{LogLevel, SharedHostLog};
use crate::error::PluginError;
use crate::runtime::SharedLua;

/// Canonical chords the host treats as load-bearing built-ins. Binding
/// one still works (plugin wins) but emits a host-log warning.
pub const RESERVED_CHORDS: &[&str] = &["ctrl+q"];

/// Modifier tokens accepted in a chord, mapped to their canonical
/// spelling. Canonical order is ctrl, alt, shift, super.
const MODIFIER_ALIASES: &[(&str, &str)] = &[
    ("ctrl", "ctrl"),
    ("control", "ctrl"),
    ("alt", "alt"),
    ("option", "alt"),
    ("opt", "alt"),
    ("shift", "shift"),
    ("super", "super"),
    ("cmd", "super"),
    ("command", "super"),
    ("meta", "super"),
    ("win", "super"),
];

/// Canonical-order list used when re-joining modifiers.
const MODIFIER_ORDER: &[&str] = &["ctrl", "alt", "shift", "super"];

/// Named (non-character) key tokens the host knows how to match.
/// `f1`..`f12` are accepted in addition to these.
pub const NAMED_KEYS: &[&str] = &[
    "enter",
    "esc",
    "tab",
    "space",
    "backspace",
    "delete",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pageup",
    "pagedown",
    "insert",
];

/// One keybinding registered by a plugin.
pub struct LuaKeybinding {
    chord: String,
    description: String,
    lua: SharedLua,
    handler_key: Arc<RegistryKey>,
}

impl std::fmt::Debug for LuaKeybinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaKeybinding")
            .field("chord", &self.chord)
            .finish_non_exhaustive()
    }
}

impl LuaKeybinding {
    /// Canonical chord string (e.g. `ctrl+shift+x`).
    #[must_use]
    pub fn chord(&self) -> &str {
        &self.chord
    }

    /// Optional human description shown in keybinding listings.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Fetch the handler function for a bridged invocation. The host
    /// runs it through [`crate::PluginRuntime::bridge_call`] with no
    /// arguments, so it may call blocking `kage.ui.*` dialogs.
    pub fn handler(&self) -> Result<Function, PluginError> {
        let lua = self.lua.lock().expect("plugin lua mutex poisoned");
        Ok(lua.registry_value(&self.handler_key)?)
    }
}

/// Shared keybinding registry, cloned into the Lua callback so
/// registrations accumulate in one place.
pub type RegisteredKeybindings = Arc<Mutex<Vec<Arc<LuaKeybinding>>>>;

/// Construct an empty keybinding registry.
#[must_use]
pub fn registered_keybindings() -> RegisteredKeybindings {
    Arc::new(Mutex::new(Vec::new()))
}

/// Normalise a chord string to canonical form, or return a message
/// describing why it is invalid.
///
/// Accepts `+`-separated tokens, case-insensitive, modifiers in any
/// order. The final token must be a single character or a known named
/// key ([`NAMED_KEYS`] or `f1`..`f12`). Output orders modifiers
/// ctrl, alt, shift, super and lowercases everything.
pub fn normalize_chord(raw: &str) -> Result<String, String> {
    let parts: Vec<String> = raw.split('+').map(|p| p.trim().to_lowercase()).collect();
    if parts.iter().any(String::is_empty) {
        return Err(format!("malformed chord `{raw}` (empty token)"));
    }
    let Some((key, mods)) = parts.split_last() else {
        return Err(format!("empty chord `{raw}`"));
    };
    let mut seen: Vec<&str> = Vec::new();
    for token in mods {
        let canonical = MODIFIER_ALIASES
            .iter()
            .find(|(alias, _)| alias == token)
            .map(|(_, c)| *c)
            .ok_or_else(|| format!("unknown modifier `{token}` in `{raw}`"))?;
        if !seen.contains(&canonical) {
            seen.push(canonical);
        }
    }
    if !is_valid_key(key) {
        return Err(format!("unknown key `{key}` in `{raw}`"));
    }
    let mut out: Vec<&str> = MODIFIER_ORDER
        .iter()
        .copied()
        .filter(|m| seen.contains(m))
        .collect();
    out.push(key);
    Ok(out.join("+"))
}

/// Whether `key` is an accepted non-modifier key token.
fn is_valid_key(key: &str) -> bool {
    if key.chars().count() == 1 {
        return true;
    }
    if NAMED_KEYS.contains(&key) {
        return true;
    }
    if let Some(n) = key.strip_prefix('f') {
        if let Ok(n) = n.parse::<u8>() {
            return (1..=12).contains(&n);
        }
    }
    false
}

/// Install `kage.register_keybinding` on the running Lua state.
pub fn install_register_keybinding(
    lua: &Lua,
    shared_lua: SharedLua,
    sink: SharedHostLog,
    registered: RegisteredKeybindings,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "register_keybinding",
        lua.create_function(move |lua, (spec, handler): (Value, Function)| {
            let (raw_chord, description) = match spec {
                Value::String(s) => (s.to_str()?.to_owned(), String::new()),
                Value::Table(t) => {
                    let chord: String = t.get("key").map_err(|_| {
                        mlua::Error::external(
                            "kage.register_keybinding: spec table needs a string `key`",
                        )
                    })?;
                    let description: String = match t.get::<Value>("description")? {
                        Value::Nil => String::new(),
                        Value::String(s) => s.to_str()?.to_owned(),
                        other => {
                            return Err(mlua::Error::external(format!(
                                "kage.register_keybinding: `description` must be a string, \
                                 got {other:?}"
                            )));
                        }
                    };
                    (chord, description)
                }
                other => {
                    return Err(mlua::Error::external(format!(
                        "kage.register_keybinding: spec must be a chord string or table, \
                         got {other:?}"
                    )));
                }
            };
            let chord = normalize_chord(&raw_chord)
                .map_err(|e| mlua::Error::external(format!("kage.register_keybinding: {e}")))?;
            if RESERVED_CHORDS.contains(&chord.as_str())
                && let Ok(mut s) = sink.lock()
            {
                s.log(
                    LogLevel::Warn,
                    &format!(
                        "plugin keybinding `{chord}` shadows a built-in binding; \
                         the plugin handler will run instead"
                    ),
                );
            }
            let key = lua.create_registry_value(handler)?;
            registered
                .lock()
                .map_err(|_| mlua::Error::external("plugin keybindings registry poisoned"))?
                .push(Arc::new(LuaKeybinding {
                    chord,
                    description,
                    lua: shared_lua.clone(),
                    handler_key: Arc::new(key),
                }));
            Ok(())
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginRuntime;

    #[test]
    fn normalize_orders_and_lowercases_modifiers() {
        assert_eq!(normalize_chord("Shift+Ctrl+X").unwrap(), "ctrl+shift+x");
        assert_eq!(normalize_chord("ctrl+s").unwrap(), "ctrl+s");
        assert_eq!(normalize_chord("F5").unwrap(), "f5");
        assert_eq!(normalize_chord("alt+enter").unwrap(), "alt+enter");
    }

    #[test]
    fn normalize_dedups_and_maps_aliases() {
        assert_eq!(normalize_chord("control+ctrl+a").unwrap(), "ctrl+a");
        assert_eq!(normalize_chord("cmd+space").unwrap(), "super+space");
        assert_eq!(normalize_chord("option+tab").unwrap(), "alt+tab");
    }

    #[test]
    fn normalize_rejects_unknown_tokens() {
        assert!(normalize_chord("hyper+x").is_err());
        assert!(normalize_chord("ctrl+banana").is_err());
        assert!(normalize_chord("ctrl+").is_err());
        assert!(normalize_chord("").is_err());
        assert!(normalize_chord("f13").is_err());
    }

    #[test]
    fn register_string_spec_appends_canonical_chord() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.register_keybinding('Ctrl+Shift+P', function() end)")
            .unwrap();
        let bound = rt.registered_keybindings();
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].chord(), "ctrl+shift+p");
        assert_eq!(bound[0].description(), "");
    }

    #[test]
    fn register_table_spec_keeps_description() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.register_keybinding({ key = 'f5', description = 'reload' }, function() end)")
            .unwrap();
        let bound = rt.registered_keybindings();
        assert_eq!(bound[0].chord(), "f5");
        assert_eq!(bound[0].description(), "reload");
    }

    #[test]
    fn register_rejects_malformed_chord() {
        let rt = PluginRuntime::new().unwrap();
        let err = rt
            .eval("kage.register_keybinding('ctrl+nope+', function() end)")
            .unwrap_err();
        assert!(err.to_string().contains("register_keybinding"));
    }

    #[test]
    fn reserved_chord_logs_a_warning_but_still_registers() {
        use crate::api::HostLog;
        #[derive(Default)]
        struct Rec {
            warns: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        }
        impl HostLog for Rec {
            fn notify(&mut self, _: &str) {}
            fn log(&mut self, level: LogLevel, message: &str) {
                if level == LogLevel::Warn {
                    self.warns.lock().unwrap().push(message.to_owned());
                }
            }
        }
        let warns = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: SharedHostLog = std::sync::Arc::new(std::sync::Mutex::new(Box::new(Rec {
            warns: warns.clone(),
        })
            as Box<dyn HostLog + Send>));
        let rt = PluginRuntime::builder().sink(sink).build().unwrap();
        rt.eval("kage.register_keybinding('ctrl+q', function() end)")
            .unwrap();
        assert_eq!(rt.registered_keybindings().len(), 1);
        let warns = warns.lock().unwrap();
        assert!(
            warns
                .iter()
                .any(|w| w.contains("ctrl+q") && w.contains("shadows")),
            "got {warns:?}"
        );
    }

    #[test]
    fn handler_round_trips_through_the_bridge() {
        use crate::bridge::BridgeStep;
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.register_keybinding('ctrl+g', function() return 'fired' end)")
            .unwrap();
        let bound = rt.registered_keybindings();
        let handler = bound[0].handler().unwrap();
        assert_eq!(
            rt.bridge_call(&handler, &[]).unwrap(),
            BridgeStep::Done(serde_json::json!("fired"))
        );
    }
}
