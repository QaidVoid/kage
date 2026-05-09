//! Sandboxed Lua runtime hosting kage plugins.
//!
//! [`PluginRuntime`] wraps an [`mlua::Lua`] state with a small allowlist
//! over the standard library. Every function that touches the host
//! filesystem, spawns processes, or loads native shared libraries is
//! removed before any plugin code runs. Plugins that need filesystem or
//! network access must go through the `kage` table (added in later tasks),
//! which routes through the same guards as built-in tools.
//!
//! The runtime is host-driven: nothing runs unless the host calls
//! [`PluginRuntime::eval`] or one of the typed dispatch helpers added
//! later. A plugin cannot start a thread or schedule a callback on its
//! own.
//!
//! See `crates/kage-plugin/src/runtime.rs` source for the exact list of
//! removed bindings.
//!
//! # Sandbox scope
//!
//! The sandbox prevents accidental filesystem and process access from
//! plugin code; it is not a security boundary against actively malicious
//! Lua. Only run plugins you trust.

use mlua::Lua;

use crate::error::PluginError;

/// A Lua VM with the dangerous standard-library bindings stripped.
#[derive(Debug)]
pub struct PluginRuntime {
    lua: Lua,
}

impl PluginRuntime {
    /// Build a fresh runtime with all kage sandbox restrictions applied.
    ///
    /// The standard library is loaded first, then the entries listed in
    /// [`SANDBOX_REMOVALS`] are deleted from the globals tree.
    pub fn new() -> Result<Self, PluginError> {
        let lua = Lua::new();
        apply_sandbox(&lua)?;
        Ok(Self { lua })
    }

    /// Borrow the underlying Lua state. Useful for tests and for higher
    /// layers in this crate that wire in the `kage` API table.
    #[must_use]
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Execute a chunk of Lua source against this runtime. Returns the
    /// chunk's return value as a Lua [`mlua::Value`].
    pub fn eval(&self, source: &str) -> Result<mlua::Value, PluginError> {
        Ok(self.lua.load(source).eval::<mlua::Value>()?)
    }
}

/// Pairs of `(table_path, key)` removed from the standard library on
/// runtime construction. `table_path` is dot-separated starting from the
/// globals table; an empty path means "drop the global by `key`".
pub const SANDBOX_REMOVALS: &[(&str, &str)] = &[
    // Process spawning and shell access.
    ("os", "execute"),
    ("os", "exit"),
    ("os", "remove"),
    ("os", "rename"),
    ("os", "tmpname"),
    ("os", "getenv"),
    ("os", "setlocale"),
    // Process-spawning io helpers; `io.open` will be replaced with a
    // safe wrapper in T6.8.
    ("io", "popen"),
    ("io", "open"),
    ("io", "tmpfile"),
    ("io", "input"),
    ("io", "output"),
    ("io", "lines"),
    // Native code loading.
    ("package", "loadlib"),
    ("package", "cpath"),
    // Bytecode and arbitrary-file loading.
    ("", "dofile"),
    ("", "loadfile"),
];

fn apply_sandbox(lua: &Lua) -> Result<(), PluginError> {
    let globals = lua.globals();
    for (path, key) in SANDBOX_REMOVALS {
        if path.is_empty() {
            globals.set(*key, mlua::Value::Nil)?;
            continue;
        }
        let table: mlua::Value = globals.get(*path)?;
        if let mlua::Value::Table(t) = table {
            t.set(*key, mlua::Value::Nil)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_strips_dangerous_globals() {
        let rt = PluginRuntime::new().unwrap();
        for (path, key) in SANDBOX_REMOVALS {
            let chunk = if path.is_empty() {
                format!("return {key} == nil")
            } else {
                format!("return {path} == nil or {path}.{key} == nil")
            };
            let v: bool = rt.lua().load(&chunk).eval().unwrap_or(false);
            assert!(v, "sandbox failed to remove {path}.{key}");
        }
    }

    #[test]
    fn benign_library_functions_still_work() {
        let rt = PluginRuntime::new().unwrap();
        let v: i64 = rt.lua().load("return string.len('hello')").eval().unwrap();
        assert_eq!(v, 5);
        let v: f64 = rt.lua().load("return math.sqrt(81)").eval().unwrap();
        assert!((v - 9.0).abs() < 1e-9);
    }

    #[test]
    fn os_execute_call_errors_after_sandboxing() {
        let rt = PluginRuntime::new().unwrap();
        let res: Result<mlua::Value, _> = rt.lua().load("return os.execute('echo hi')").eval();
        // The function pointer was nil'd, so calling it raises.
        assert!(res.is_err());
    }

    #[test]
    fn dofile_and_loadfile_are_unreachable() {
        let rt = PluginRuntime::new().unwrap();
        for chunk in ["dofile('/etc/passwd')", "loadfile('/etc/passwd')"] {
            let res: Result<mlua::Value, _> = rt.lua().load(chunk).eval();
            assert!(res.is_err(), "expected error from {chunk}");
        }
    }

    #[test]
    fn eval_returns_lua_values() {
        let rt = PluginRuntime::new().unwrap();
        let v: mlua::Value = rt.eval("return 21 * 2").unwrap();
        assert_eq!(v.as_integer(), Some(42));
    }
}
