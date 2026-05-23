//! The `env` capability: `kage.env`.
//!
//! Attached only onto the `kage` proxy of a plugin that was granted
//! `env` (see [`crate::capabilities`]); an ungranted plugin never sees
//! it. Returns the value of a process environment variable or `nil`
//! when it is unset. The capability grant itself is the access
//! control: there is no allowlist of which variables can be read,
//! matching the unrestricted-command shape of `exec`.

use mlua::{Lua, Table};

use crate::capabilities::{Capability, CapabilityRegistry};

/// Register the `env` installer into `registry`.
///
/// The installer runs (via `request_capabilities`) against a granted
/// plugin's `kage` proxy and sets `env` on it. A missing variable
/// returns `nil`; an empty value returns the empty string.
pub(crate) fn register(registry: &CapabilityRegistry) {
    let mut reg = registry.lock().expect("capability registry mutex poisoned");
    reg.insert(
        Capability::Env,
        Box::new(|lua: &Lua, pkage: &Table| {
            pkage.set(
                "env",
                lua.create_function(|_, name: String| match std::env::var(&name) {
                    Ok(value) => Ok(Some(value)),
                    Err(std::env::VarError::NotPresent) => Ok(None),
                    Err(std::env::VarError::NotUnicode(_)) => Err(mlua::Error::external(format!(
                        "kage.env: {name} is not valid UTF-8"
                    ))),
                })?,
            )?;
            Ok(())
        }),
    );
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    fn rt_with_env() -> PluginRuntime {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("p".to_owned(), vec!["env".to_owned()]);
        PluginRuntime::builder().capabilities(caps).build().unwrap()
    }

    #[test]
    fn env_returns_value_when_set() {
        if std::env::var_os("PATH").is_none() {
            return;
        }
        let rt = rt_with_env();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'env'}); \
                 local p = kage.env('PATH'); \
                 return type(p) == 'string' and #p > 0",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn env_returns_nil_when_unset() {
        let rt = rt_with_env();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'env'}); \
                 return kage.env('KAGE_DEFINITELY_UNSET_VAR_xyz123') == nil",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn ungranted_plugin_has_no_env() {
        let rt = rt_with_env();
        let v = rt.eval_plugin("other", "return kage.env == nil").unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }
}
