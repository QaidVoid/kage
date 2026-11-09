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
//! Each grant is a single coarse token; there is no per-resource
//! scoping. Granting `exec` permits running any command, `env` permits
//! reading any environment variable, and `net` permits any outbound
//! request that passes the host safety check. A grant is the whole
//! capability or nothing.
//!
//! The trusted user environment (`init.lua`) is the exception: it gets
//! every capability attached up front (see [`install_trusted`]).
//!
//! This module owns the capability vocabulary, the grant lookup, and
//! the `request_capabilities` binding. The capabilities themselves
//! (their actual APIs) register an installer into the
//! [`CapabilityRegistry`] at runtime-build time; this module stays
//! agnostic about what each one does.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use kage_core::sync::lock;
use mlua::{Lua, RegistryKey, Table, Value};

use crate::error::PluginError;

/// One elevated capability a plugin may be granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Capability {
    /// Inspect session entries and request a reseating rewind.
    SessionWrite,
    /// Run any subprocess (no shell, any binary on the `PATH`, any
    /// args) rooted at the workdir, and declare subprocess-spawning
    /// config: `kage.exec`, plus plugin-declared MCP servers and ACP
    /// agents (the host spawns what was declared). Coarse: no command
    /// allowlist.
    Exec,
    /// Read any process environment variable. Coarse: no per-variable
    /// allowlist, and read-only (no setter).
    Env,
    /// Make outbound HTTP requests via `kage.http`.
    Net,
    /// Call synchronous cryptographic primitives via `kage.crypto`.
    /// Stateless pure functions over byte strings.
    Crypto,
    /// See conversation and prompt text, and rewrite what the model
    /// sees: the `transform_context`, `before_provider_request`,
    /// `compact_prepare`, and text-bearing notification events
    /// (including the `tool_call`, `tool_update` and `tool_result`
    /// payloads), plus `kage.config().system_prompt`. Coarse:
    /// granting it hands over the whole transcript.
    Context,
    /// Register an LLM provider via `kage.register_provider`, whose
    /// handler sees the full outgoing request and fabricates the
    /// response stream. Reaching the network or reading credentials
    /// still needs `net`/`env` on top.
    Provider,
    /// Create or overwrite files under the workdir via `kage.fs.write`.
    /// Reading (`kage.fs.read`) stays on the base surface.
    FsWrite,
}

impl Capability {
    /// Every capability, in the order docs and the generated stub
    /// list them. Exhaustive: adding a variant without adding it here
    /// fails the capability-alias test in `spec::tests`.
    pub(crate) const ALL: &[Capability] = &[
        Self::SessionWrite,
        Self::Exec,
        Self::Env,
        Self::Net,
        Self::Crypto,
        Self::Context,
        Self::Provider,
        Self::FsWrite,
    ];

    /// Parse a wire name (as used in config and
    /// `kage.request_capabilities`). Unknown names are rejected loudly
    /// rather than silently dropped, so a config typo is visible.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "session_write" => Ok(Self::SessionWrite),
            "exec" => Ok(Self::Exec),
            "env" => Ok(Self::Env),
            "net" => Ok(Self::Net),
            "crypto" => Ok(Self::Crypto),
            "context" => Ok(Self::Context),
            "provider" => Ok(Self::Provider),
            "fs_write" => Ok(Self::FsWrite),
            other => Err(format!(
                "unknown capability {other:?} (known: {})",
                Self::ALL
                    .iter()
                    .map(|c| c.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// The wire name, for messages that tell a user or plugin author
    /// which capability to grant.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::SessionWrite => "session_write",
            Self::Exec => "exec",
            Self::Env => "env",
            Self::Net => "net",
            Self::Crypto => "crypto",
            Self::Context => "context",
            Self::Provider => "provider",
            Self::FsWrite => "fs_write",
        }
    }

    /// What holding the capability lets a plugin do, in a few words, for
    /// a user deciding whether to grant it.
    pub(crate) fn summary(self) -> &'static str {
        match self {
            Self::SessionWrite => "read the session's entries, fork, rewind and inject messages",
            Self::Exec => "run any program",
            Self::Env => "read environment variables",
            Self::Net => "make outbound HTTP requests",
            Self::Crypto => "use cryptographic primitives",
            Self::Context => "read the conversation and rewrite what the model sees",
            Self::Provider => "register an LLM provider",
            Self::FsWrite => "create and overwrite files in the workdir",
        }
    }
}

/// Resolved per-plugin grants: plugin file stem -> capability set.
pub(crate) type Grants = Arc<HashMap<String, HashSet<Capability>>>;

/// What each package's `plugin.toml` lets it hold, by plugin name. A
/// plugin with no entry, a script, has no ceiling beyond its grants.
pub(crate) type Ceilings = Arc<Mutex<HashMap<String, HashSet<Capability>>>>;

/// Whether `plugin` may hold `cap` by its manifest: true for a plugin
/// with no ceiling.
pub(crate) fn declared(ceilings: &Ceilings, plugin: &str, cap: Capability) -> bool {
    lock(ceilings)
        .get(plugin)
        .is_none_or(|ceiling| ceiling.contains(&cap))
}

/// The name of the plugin currently being evaluated or running a
/// callback, if any. Set by [`crate::runtime::EvalState::eval_in`] for
/// the duration of that plugin's chunk, and by [`scoped`] for the
/// duration of every callback the host runs on a plugin's behalf, so
/// capability-gated surfaces (`request_capabilities`, event
/// registration) know who is asking. `None` means trusted host eval.
pub(crate) type CurrentPlugin = Arc<Mutex<Option<String>>>;

/// Whether `owner` is a trusted host context rather than a plugin: no
/// owner (host eval) or a reserved `@`-prefixed environment (`@user`,
/// `@defaults`). The loader refuses plugin stems starting with `@`, so
/// the prefix cannot be forged.
pub(crate) fn is_trusted(owner: Option<&str>) -> bool {
    owner.is_none_or(|name| name.starts_with('@'))
}

/// The owner currently in the slot, cloned out.
pub(crate) fn current_owner(slot: &CurrentPlugin) -> Option<String> {
    lock(slot).clone()
}

/// Run `f` with `owner` set as the current plugin, restoring the
/// previous value afterwards (also when `f` panics).
pub(crate) fn scoped<R>(slot: &CurrentPlugin, owner: Option<&str>, f: impl FnOnce() -> R) -> R {
    let value = owner.map(str::to_owned);
    let prev = {
        let mut guard = lock(slot);
        std::mem::replace(&mut *guard, value)
    };
    let _guard = crate::guard::RestoreOnDrop::arm(move || {
        lock(slot).clone_from(&prev);
    });
    f()
}

/// An owner token and the slot it scopes, grabbed at registration and
/// carried to every invocation site.
#[derive(Clone, Default)]
pub(crate) struct OwnerScope {
    /// The plugin that registered the callback, or `None` for the
    /// trusted user config and host eval.
    pub(crate) owner: Option<String>,
    pub(crate) current: CurrentPlugin,
}

impl OwnerScope {
    /// Snapshot the slot's current owner together with the slot.
    pub(crate) fn grab(slot: &CurrentPlugin) -> Self {
        Self {
            owner: current_owner(slot),
            current: Arc::clone(slot),
        }
    }

    /// Run `f` with the recorded owner set on the slot.
    pub(crate) fn run<R>(&self, f: impl FnOnce() -> R) -> R {
        scoped(&self.current, self.owner.as_deref(), f)
    }
}

/// A future that sets the owner slot around every poll of `inner`, so a
/// coroutine the owner thread drives between jobs keeps its plugin's
/// identity across yields. Held-across-await state would leak the owner
/// into unrelated jobs, so the slot is set and restored per poll.
pub(crate) struct OwnerScoped {
    slot: CurrentPlugin,
    owner: Option<String>,
    inner: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl OwnerScoped {
    pub(crate) fn new(
        slot: CurrentPlugin,
        owner: Option<String>,
        inner: Pin<Box<dyn Future<Output = ()> + Send>>,
    ) -> Self {
        Self { slot, owner, inner }
    }
}

impl Future for OwnerScoped {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        scoped(&this.slot, this.owner.as_deref(), || {
            this.inner.as_mut().poll(cx)
        })
    }
}

/// Attaches a granted capability's API onto a single plugin's `kage`
/// proxy table. Capabilities register one of these so this module
/// need not know their surface. Several installers may attach to one
/// capability (`exec` covers `kage.exec` and the declare-a-subprocess
/// config surfaces); they run in registration order.
pub(crate) type CapabilityInstaller = Box<dyn Fn(&Lua, &Table) -> mlua::Result<()> + Send>;

/// Capability -> installers, populated at runtime-build time by each
/// capability's own wiring. Empty means no capability has an API yet.
pub(crate) type CapabilityRegistry = Arc<Mutex<HashMap<Capability, Vec<CapabilityInstaller>>>>;

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
/// registered installer against that plugin's own `kage` proxy. A
/// granted name whose capability has no registered installer raises
/// instead of reporting true.
pub(crate) fn install_request_capabilities(
    lua: &Lua,
    current: CurrentPlugin,
    grants: Grants,
    ceilings: Ceilings,
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
                if let Some(plugin) = plugin.as_deref()
                    && !declared(&ceilings, plugin, cap)
                {
                    return Err(mlua::Error::external(format!(
                        "{plugin} requested the {} capability, which its plugin.toml \
                         does not declare",
                        cap.name()
                    )));
                }
                let ok = granted.contains(&cap);
                if ok {
                    let installed = registry
                        .lock()
                        .map_err(|_| mlua::Error::external("capability registry mutex poisoned"))?
                        .contains_key(&cap);
                    if !installed {
                        return Err(mlua::Error::external(format!(
                            "capability {} has no installer",
                            cap.name()
                        )));
                    }
                }
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

/// Attach every registered capability onto `pkage`, and replace its
/// `request_capabilities` with one that reports each known capability
/// as granted. Used for the trusted user environment, which holds every
/// capability without the request handshake.
pub(crate) fn install_trusted(
    lua: &Lua,
    registry: &CapabilityRegistry,
    pkage: &Table,
) -> mlua::Result<()> {
    let reg = registry
        .lock()
        .map_err(|_| mlua::Error::external("capability registry mutex poisoned"))?;
    for installer in reg.values().flatten() {
        installer(lua, pkage)?;
    }
    pkage.set(
        "request_capabilities",
        lua.create_function(|lua, requested: Vec<String>| {
            let result = lua.create_table()?;
            for name in &requested {
                Capability::parse(name).map_err(mlua::Error::external)?;
                result.set(name.as_str(), true)?;
            }
            Ok(result)
        })?,
    )
}

/// Run the registered installer for `cap` against the requesting
/// plugin's `kage` proxy table, so the elevated API is visible only
/// to that plugin. `request_capabilities` has already refused a
/// granted name with no installer, so every capability reaching here
/// has one.
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
    let Some(installers) = reg.get(&cap) else {
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
    for installer in installers {
        installer(lua, &pkage)?;
    }
    Ok(())
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
        assert_eq!(Capability::parse("env"), Ok(Capability::Env));
        assert_eq!(Capability::parse("net"), Ok(Capability::Net));
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

    #[test]
    fn every_capability_granted_and_requested_reports_true() {
        let mut caps = BTreeMap::new();
        caps.insert(
            "trusted".to_owned(),
            Capability::ALL
                .iter()
                .map(|cap| cap.name().to_owned())
                .collect(),
        );
        let rt = crate::PluginRuntime::builder()
            .capabilities(caps)
            .build()
            .unwrap();
        let names = Capability::ALL
            .iter()
            .map(|cap| format!("'{}'", cap.name()))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!("return kage.request_capabilities({{{names}}})");
        let Value::Table(result) = rt.eval_plugin("trusted", &source).unwrap() else {
            panic!("expected a result table");
        };
        for cap in Capability::ALL {
            assert!(
                result.get::<bool>(cap.name()).unwrap(),
                "{} was granted and has an installer",
                cap.name()
            );
        }
    }

    #[test]
    fn a_granted_capability_without_an_installer_errors() {
        let lua = Lua::new();
        lua.globals()
            .set("kage", lua.create_table().unwrap())
            .unwrap();
        let mut grants = HashMap::new();
        let mut granted = HashSet::new();
        granted.insert(Capability::Net);
        grants.insert("p".to_owned(), granted);
        install_request_capabilities(
            &lua,
            Arc::new(Mutex::new(Some("p".to_owned()))),
            Arc::new(grants),
            Ceilings::default(),
            Arc::new(Mutex::new(HashMap::new())),
            capability_registry(),
        )
        .unwrap();
        let err = lua
            .load("return kage.request_capabilities({'net'})")
            .eval::<Value>()
            .unwrap_err();
        assert!(
            err.to_string().contains("capability net has no installer"),
            "the error names the capability: {err}"
        );
    }

    #[test]
    fn trusted_owners_are_host_eval_and_reserved_envs() {
        assert!(is_trusted(None));
        assert!(is_trusted(Some("@user")));
        assert!(is_trusted(Some("@defaults")));
        assert!(!is_trusted(Some("p")));
    }

    #[test]
    fn scoped_sets_the_owner_and_restores_the_previous_one() {
        let slot: CurrentPlugin = Arc::default();
        scoped(&slot, Some("outer"), || {
            assert_eq!(lock(&slot).as_deref(), Some("outer"));
            scoped(&slot, Some("inner"), || {
                assert_eq!(lock(&slot).as_deref(), Some("inner"));
            });
            assert_eq!(lock(&slot).as_deref(), Some("outer"));
            scoped(&slot, None, || assert!(lock(&slot).is_none()));
            assert_eq!(lock(&slot).as_deref(), Some("outer"));
        });
        assert!(lock(&slot).is_none());
    }

    #[test]
    fn scoped_restores_the_owner_when_the_body_panics() {
        let slot: CurrentPlugin = Arc::default();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scoped(&slot, Some("p"), || panic!("boom"));
        }));
        assert!(outcome.is_err());
        assert!(lock(&slot).is_none());
    }

    #[test]
    fn owner_scoped_sets_the_slot_on_every_poll() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll, Waker};

        let slot: CurrentPlugin = Arc::default();
        let inner_polls = Arc::new(AtomicUsize::new(0));
        let slot_for_inner = Arc::clone(&slot);
        let polls_for_inner = Arc::clone(&inner_polls);
        let inner = {
            let mut first = true;
            std::future::poll_fn(move |_| {
                polls_for_inner.fetch_add(1, Ordering::SeqCst);
                let owner = lock(&slot_for_inner).clone();
                if first {
                    first = false;
                    assert_eq!(owner.as_deref(), Some("p"), "first poll sees the owner");
                    return Poll::Pending;
                }
                assert_eq!(owner.as_deref(), Some("p"), "resumed poll sees the owner");
                Poll::Ready(())
            })
        };
        let task = OwnerScoped::new(Arc::clone(&slot), Some("p".to_owned()), Box::pin(inner));
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut pinned = std::pin::pin!(task);
        assert_eq!(pinned.as_mut().poll(&mut cx), Poll::Pending);
        assert!(lock(&slot).is_none());
        assert_eq!(pinned.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(inner_polls.load(Ordering::SeqCst), 2);
        assert!(lock(&slot).is_none());
    }
}
