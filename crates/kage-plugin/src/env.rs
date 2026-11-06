//! The `env` capability: `kage.env`.
//!
//! Attached only onto the `kage` proxy of a plugin that was granted
//! `env` (see [`crate::capabilities`]); an ungranted plugin never sees
//! it. Returns the value of a variable in the session's environment, the
//! one `kage.exec` starts processes with, or `nil` when it is unset. The capability grant itself is the access
//! control: there is no allowlist of which variables can be read,
//! matching the unrestricted-command shape of `exec`. A granted plugin
//! can read every variable in that environment, including
//! secrets such as provider API keys, and the access is read-only
//! (there is no setter). Grant it only to trusted plugins.

use std::ffi::OsString;

use crate::invocation::Environments;
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
/// Variables are read from the environment of the session the call runs
/// for, the one `kage.exec` starts processes with.
pub(crate) fn register(
    registry: &CapabilityRegistry,
    lookup: CredentialLookup,
    envs: Environments,
) {
    let mut reg = lock(registry);
    reg.entry(Capability::Env)
        .or_default()
        .push(Box::new(move |lua: &Lua, pkage: &Table| {
            pkage.set(
                "env",
                lua.create_function({
                    let envs = envs.clone();
                    move |_, name: String| match envs
                        .current()
                        .var(&name)
                        .map(OsString::into_string)
                    {
                        Some(Ok(value)) => Ok(Some(value)),
                        None => Ok(None),
                        Some(Err(_)) => Err(mlua::Error::external(format!(
                            "kage.env: {name} is not valid UTF-8"
                        ))),
                    }
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
    fn env_reads_the_session_environment() {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("p".to_owned(), vec!["env".to_owned()]);
        let rt = PluginRuntime::builder()
            .capabilities(caps)
            .env(kage_core::environment::Environment::captured(vec![(
                "CLIENT".into(),
                "editor".into(),
            )]))
            .build()
            .unwrap();
        let v = rt
            .eval_plugin(
                "p",
                "kage.request_capabilities({'env'}); \
                 return kage.env('CLIENT') == 'editor' and kage.env('PATH') == nil",
            )
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    /// Work for a session reads that session's environment, and so does a
    /// timer set while doing it; other work reads the runtime's own.
    #[test]
    fn each_session_reads_its_own_environment() {
        use crate::Invocation;
        use kage_core::SessionId;
        use kage_core::environment::Environment;

        let mut caps = std::collections::BTreeMap::new();
        caps.insert("p".to_owned(), vec!["env".to_owned()]);
        let rt = PluginRuntime::builder()
            .capabilities(caps)
            .env(Environment::captured(vec![(
                "CLIENT".into(),
                "host".into(),
            )]))
            .build()
            .unwrap();
        let (a, b) = (SessionId::new(), SessionId::new());
        rt.set_session_env(
            a,
            Environment::captured(vec![("CLIENT".into(), "a".into())]),
        );
        rt.set_session_env(
            b,
            Environment::captured(vec![("CLIENT".into(), "b".into())]),
        );
        let read = |invocation: Invocation| {
            invocation.scope(|| {
                rt.eval_plugin(
                    "p",
                    "kage.request_capabilities({'env'}); return kage.env('CLIENT')",
                )
                .unwrap()
                .as_string()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
            })
        };
        assert_eq!(read(Invocation::session(a)), "a");
        assert_eq!(read(Invocation::session(b)), "b");
        assert_eq!(read(Invocation::default()), "host");

        Invocation::session(b).scope(|| {
            rt.eval_plugin(
                "p",
                "kage.request_capabilities({'env'}); \
                 kage.defer(function() seen = kage.env('CLIENT') end, 0)",
            )
            .unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let seen = rt.eval_plugin("p", "return seen").unwrap();
            if let Some(seen) = seen.as_string() {
                assert_eq!(seen.to_str().unwrap(), "b");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the timer never ran");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        rt.forget_session(b);
        assert_eq!(read(Invocation::session(b)), "host");
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
