//! Binary-local bridge between the `acp` provider and the plugin
//! runtime.
//!
//! The provider registry is built before the plugin runtime exists in
//! every entrypoint, but the ACP client needs the runtime to consult
//! `kage.on_acp_permission` and `kage.acp.add_agent`. This module
//! keeps the current process runtime, which the entrypoint replaces after
//! loading or reloading plugins. The provider's permission resolver and
//! agent source read it lazily, so removed policies and registrations no
//! longer apply. Without a runtime, upstream permission requests are denied.

use std::sync::{Arc, Mutex};

use kage_acp::agent::PermissionDecision;
use kage_acp::client::{AgentSource, PermissionResolver};
use kage_core::sync::lock;
use kage_plugin::PluginRuntime;

static RUNTIME: Mutex<Option<Arc<PluginRuntime>>> = Mutex::new(None);

/// Replace the process runtime used by the ACP client, or clear its registrations.
pub(crate) fn set_runtime(runtime: Option<&Arc<PluginRuntime>>) {
    *lock(&RUNTIME) = runtime.cloned();
}

fn runtime() -> Option<Arc<PluginRuntime>> {
    lock(&RUNTIME).clone()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_runtime(allow: bool) -> Arc<PluginRuntime> {
        let runtime = PluginRuntime::builder()
            .capabilities(std::collections::BTreeMap::from([(
                "fixture".to_owned(),
                vec!["exec".to_owned()],
            )]))
            .build()
            .unwrap();
        runtime
            .eval_plugin(
                "fixture",
                &format!(
                    "kage.request_capabilities({{'exec'}}) \
                     kage.on_acp_permission(function() return {allow} end) {}",
                    if allow {
                        "kage.acp.add_agent({name='fixture', command='fixture'})"
                    } else {
                        ""
                    }
                ),
            )
            .unwrap();
        Arc::new(runtime)
    }

    #[test]
    #[expect(
        clippy::result_large_err,
        reason = "figment requires its callback error type"
    )]
    fn existing_resolvers_observe_replaced_permission_and_agent_registrations() {
        figment::Jail::expect_with(|_| {
            let resolver = permission_resolver();
            let agents = agent_source();
            let request = serde_json::from_value(serde_json::json!({
                "sessionId": "session", "toolCall": {"toolCallId": "tool"}, "options": []
            }))
            .unwrap();
            let allowed = configured_runtime(true);
            set_runtime(Some(&allowed));
            assert_eq!(resolver(&request), PermissionDecision::Allow);
            assert_eq!(agents().len(), 1);
            let denied = configured_runtime(false);
            set_runtime(Some(&denied));
            assert!(matches!(resolver(&request), PermissionDecision::Deny(_)));
            assert!(agents().is_empty());
            set_runtime(Some(&allowed));
            set_runtime(None);
            assert!(matches!(resolver(&request), PermissionDecision::Deny(_)));
            assert!(agents().is_empty());
            Ok(())
        });
    }
}
