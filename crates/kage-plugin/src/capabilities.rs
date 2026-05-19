//! Opt-in trusted-capability tier.
//!
//! A plugin gets only the sandboxed base `kage` surface by default.
//! Elevated APIs (filesystem-unscoped, process exec, session rewind)
//! are *capabilities*: the user grants them per-plugin in config
//! (`[plugins.capabilities] my-plugin = ["session_write"]`), and the
//! plugin must explicitly ask for them at load time via
//! `kage.request_capabilities`. Only when both sides agree is the
//! capability's API attached - and it is attached onto that one
//! plugin's environment (see [`crate::runtime`] per-plugin `_ENV`),
//! so an ungranted plugin cannot even see it, let alone call it.
//!
//! This module owns the capability vocabulary, the grant lookup, and
//! the `request_capabilities` binding. The capabilities themselves
//! (their actual APIs) register an installer into the
//! [`CapabilityRegistry`] at runtime-build time; this module stays
//! agnostic about what each one does.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use mlua::{Lua, RegistryKey, Table, Value};

use crate::error::PluginError;

/// One elevated capability a plugin may be granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Capability {
    /// Inspect session entries and request a reseating rewind.
    SessionWrite,
    /// Run a subprocess (no shell) rooted at the workdir.
    Exec,
}

impl Capability {
    /// Parse a wire name (as used in config and
    /// `kage.request_capabilities`). Unknown names are rejected loudly
    /// rather than silently dropped, so a config typo is visible.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "session_write" => Ok(Self::SessionWrite),
            "exec" => Ok(Self::Exec),
            other => Err(format!(
                "unknown capability {other:?} (known: session_write, exec)"
            )),
        }
    }
}

/// Resolved per-plugin grants: plugin file stem -> capability set.
pub(crate) type Grants = Arc<HashMap<String, HashSet<Capability>>>;

/// The name of the plugin currently being evaluated, if any. Set by
/// [`crate::runtime::PluginRuntime::eval_plugin`] for the duration of
/// that plugin's chunk so `request_capabilities` knows who is asking.
pub(crate) type CurrentPlugin = Arc<Mutex<Option<String>>>;

/// Attaches a granted capability's API onto a single plugin's `kage`
/// proxy table. Capabilities register one of these so this module
/// need not know their surface.
pub(crate) type CapabilityInstaller = Box<dyn Fn(&Lua, &Table) -> mlua::Result<()> + Send>;

/// Capability -> installer, populated at runtime-build time by each
/// capability's own wiring. Empty means no capability has an API yet.
pub(crate) type CapabilityRegistry = Arc<Mutex<HashMap<Capability, CapabilityInstaller>>>;

/// An empty capability registry for capabilities to register into.
#[must_use]
pub(crate) fn capability_registry() -> CapabilityRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Parse the raw config grant map into typed [`Capability`] sets.
///
/// # Errors
///
/// Returns [`PluginError::Config`] if any grant names a capability
/// that does not exist - a config typo must not silently disable a
/// capability the user believes they enabled.
pub(crate) fn parse_grants(
    raw: &BTreeMap<String, Vec<String>>,
) -> Result<HashMap<String, HashSet<Capability>>, PluginError> {
    let mut out: HashMap<String, HashSet<Capability>> = HashMap::new();
    for (plugin, caps) in raw {
        let mut set = HashSet::new();
        for cap in caps {
            let parsed = Capability::parse(cap).map_err(|e| {
                PluginError::Config(format!("[plugins.capabilities] {plugin}: {e}"))
            })?;
            set.insert(parsed);
        }
        out.insert(plugin.clone(), set);
    }
    Ok(out)
}

/// Install `kage.request_capabilities` on the base `kage` table.
///
/// The binding is part of the shared base surface (every plugin can
/// call it), but its effect is per-plugin: it consults `grants` for
/// the plugin currently being evaluated, returns a truthful
/// `{ name = bool }` table so a plugin can degrade when a capability
/// is missing, and for each granted-and-requested capability runs its
/// registered installer against that plugin's own `kage` proxy.
pub(crate) fn install_request_capabilities(
    lua: &Lua,
    current: CurrentPlugin,
    grants: Grants,
    envs: Arc<Mutex<HashMap<String, RegistryKey>>>,
    registry: CapabilityRegistry,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "request_capabilities",
        lua.create_function(move |lua, requested: Vec<String>| {
            let plugin = current
                .lock()
                .map_err(|_| mlua::Error::external("current-plugin mutex poisoned"))?
                .clone();
            let result = lua.create_table()?;
            // No plugin context (host-side eval): nothing is granted.
            let granted = plugin
                .as_deref()
                .and_then(|p| grants.get(p))
                .cloned()
                .unwrap_or_default();
            for name in &requested {
                let cap = Capability::parse(name).map_err(mlua::Error::external)?;
                let ok = granted.contains(&cap);
                result.set(name.as_str(), ok)?;
                if ok {
                    attach_capability(lua, cap, plugin.as_deref(), &envs, &registry)?;
                }
            }
            Ok(result)
        })?,
    )?;
    Ok(())
}

/// Run the registered installer for `cap` against the requesting
/// plugin's `kage` proxy table, so the elevated API is visible only
/// to that plugin. A capability with no registered installer (none
/// have one until their phase lands) is a silent no-op here.
fn attach_capability(
    lua: &Lua,
    cap: Capability,
    plugin: Option<&str>,
    envs: &Arc<Mutex<HashMap<String, RegistryKey>>>,
    registry: &CapabilityRegistry,
) -> mlua::Result<()> {
    let Some(plugin) = plugin else {
        return Ok(());
    };
    let reg = registry
        .lock()
        .map_err(|_| mlua::Error::external("capability registry mutex poisoned"))?;
    let Some(installer) = reg.get(&cap) else {
        return Ok(());
    };
    let env: Table = {
        let slots = envs
            .lock()
            .map_err(|_| mlua::Error::external("plugin env map poisoned"))?;
        let Some(key) = slots.get(plugin) else {
            return Ok(());
        };
        lua.registry_value::<Table>(key)?
    };
    let pkage: Value = env.get("kage")?;
    let Value::Table(pkage) = pkage else {
        return Err(mlua::Error::external("plugin kage proxy missing"));
    };
    installer(lua, &pkage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_parse_maps_known_and_rejects_unknown() {
        assert_eq!(
            Capability::parse("session_write"),
            Ok(Capability::SessionWrite)
        );
        assert_eq!(Capability::parse("exec"), Ok(Capability::Exec));
        assert!(Capability::parse("teleport").is_err());
    }

    #[test]
    fn parse_grants_rejects_unknown_capability() {
        let mut raw = BTreeMap::new();
        raw.insert("p".to_owned(), vec!["session_write".to_owned()]);
        assert!(parse_grants(&raw).is_ok());
        raw.insert("q".to_owned(), vec!["bogus".to_owned()]);
        let err = parse_grants(&raw).unwrap_err();
        assert!(matches!(err, PluginError::Config(_)));
    }

    #[test]
    fn builder_rejects_unknown_capability_in_config() {
        let mut caps = BTreeMap::new();
        caps.insert("p".to_owned(), vec!["nope".to_owned()]);
        let err = crate::PluginRuntime::builder()
            .capabilities(caps)
            .build()
            .unwrap_err();
        assert!(matches!(err, PluginError::Config(_)));
    }

    #[test]
    fn request_capabilities_is_truthful_and_per_plugin() {
        let mut caps = BTreeMap::new();
        caps.insert("trusted".to_owned(), vec!["session_write".to_owned()]);
        let rt = crate::PluginRuntime::builder()
            .capabilities(caps)
            .build()
            .unwrap();

        // Granted plugin: a granted capability is true, others false.
        let yes = rt
            .eval_plugin(
                "trusted",
                "return kage.request_capabilities({'session_write','exec'}).session_write",
            )
            .unwrap();
        assert_eq!(yes.as_boolean(), Some(true));
        let no = rt
            .eval_plugin("trusted", "return kage.request_capabilities({'exec'}).exec")
            .unwrap();
        assert_eq!(no.as_boolean(), Some(false));

        // A different plugin gets nothing, even for the same capability.
        let other = rt
            .eval_plugin(
                "other",
                "return kage.request_capabilities({'session_write'}).session_write",
            )
            .unwrap();
        assert_eq!(other.as_boolean(), Some(false));

        // Unknown names raise rather than silently resolving false.
        assert!(
            rt.eval_plugin("trusted", "return kage.request_capabilities({'teleport'})")
                .is_err()
        );
    }
}
