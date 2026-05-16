//! Binary-local bridge between the `acp` provider and the plugin
//! runtime.
//!
//! The provider registry is built before the plugin runtime exists in
//! every entrypoint, but the ACP client needs the runtime to consult
//! `kage.on_acp_permission` and `kage.acp.add_agent`. This module
//! holds a write-once handle to the process's plugin runtime that the
//! entrypoint records once it is built; the provider's permission
//! resolver and agent source read it lazily at turn time. `kage rpc`
//! (per-session runtimes) never records one, so it falls back to the
//! safe default: deny.

use std::sync::{Arc, OnceLock};

use kage_acp::agent::PermissionDecision;
use kage_acp::client::{AgentSource, PermissionResolver};
use kage_plugin::PluginRuntime;

static RUNTIME: OnceLock<Arc<PluginRuntime>> = OnceLock::new();

/// Record the process's plugin runtime for the ACP client. Write-once
/// (the first runtime built wins); later calls are ignored.
pub(crate) fn set_runtime(rt: &Arc<PluginRuntime>) {
    let _ = RUNTIME.set(Arc::clone(rt));
}

fn runtime() -> Option<Arc<PluginRuntime>> {
    RUNTIME.get().cloned()
}

/// Resolver backed by `kage.on_acp_permission`. No registered policy,
/// or a deny verdict, blocks the upstream agent's tool: kage never
/// auto-approves another agent's tools.
pub(crate) fn permission_resolver() -> PermissionResolver {
    Arc::new(|req| {
        let payload = serde_json::to_value(req).unwrap_or(serde_json::Value::Null);
        match runtime().and_then(|rt| rt.acp_permission(&payload)) {
            Some(true) => PermissionDecision::Allow,
            Some(false) => {
                PermissionDecision::Deny(Some("denied by kage.on_acp_permission".to_owned()))
            }
            None => PermissionDecision::Deny(Some(
                "no kage.on_acp_permission policy; upstream tool denied".to_owned(),
            )),
        }
    })
}

/// Agent source exposing `kage.acp.add_agent` declarations.
pub(crate) fn agent_source() -> AgentSource {
    Arc::new(|| {
        runtime()
            .map(|rt| rt.registered_acp_agents())
            .unwrap_or_default()
    })
}
