//! The `env` capability: `kage.env`.
//!
//! Attached only onto the `kage` proxy of a plugin that was granted
//! `env` (see [`crate::capabilities`]); an ungranted plugin never sees
//! it. Returns the value of a process environment variable or `nil`
//! when it is unset. The capability grant itself is the access
//! control: there is no allowlist of which variables can be read,
//! matching the unrestricted-command shape of `exec`. A granted plugin
//! can read every variable in the host process environment, including
//! secrets such as provider API keys, and the access is read-only
//! (there is no setter). Grant it only to trusted plugins.

use kage_core::sync::lock;

use mlua::{Lua, Table};

use crate::capabilities::{Capability, CapabilityRegistry};

/// Live lookup of the token the host holds for a provider id, as
/// saved by the login flow. Read on every call, so a credential saved
/// mid-session applies without a rebuild. Returns `None` when nothing
/// is stored.
pub type CredentialLookup = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Register the `env` installer into `registry`.
///
/// The installer runs (via `request_capabilities`) against a granted
/// plugin's `kage` proxy and sets `env` and `credential` on it. A
/// missing variable returns `nil`; an empty value returns the empty
/// string. A provider id with no stored credential answers `nil`.
pub(crate) fn register(registry: &CapabilityRegistry, lookup: CredentialLookup) {
    let mut reg = lock(registry);
    reg.entry(Capability::Env)
        .or_default()
        .push(Box::new(move |lua: &Lua, pkage: &Table| {
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
            let lookup = lookup.clone();
            pkage.set(
                "credential",
                lua.create_function(move |_, provider: String| Ok(lookup(provider.as_str())))?,
            )?;
            Ok(())
        }));
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

    fn rt_with_lookup() -> PluginRuntime {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("p".to_owned(), vec!["env".to_owned()]);
        let lookup: super::CredentialLookup = std::sync::Arc::new(|provider: &str| {
            (provider == "demo").then(|| "token-123".to_owned())
        });
        PluginRuntime::builder()
            .capabilities(caps)
            .credential_lookup(lookup)
            .build()
            .unwrap()
    }

    #[test]
    fn credential_returns_the_stored_token() {
        let rt = rt_with_lookup();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'env'}); return kage.credential('demo') == 'token-123'",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn credential_returns_nil_when_nothing_is_stored() {
        let rt = rt_with_lookup();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'env'}); return kage.credential('nope') == nil",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn ungranted_plugin_has_no_credential() {
        let rt = rt_with_lookup();
        let v = rt
            .eval_plugin("other", "return kage.credential == nil")
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }
}
