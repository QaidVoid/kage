//! Live policy for server-initiated model requests.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kage_jsonrpc::RpcError;
use serde_json::Value;

use crate::ServerRequestHandler;

/// Shared sampling permission, including connections held by active runs.
#[derive(Clone, Default)]
pub struct SamplingPolicy(Arc<AtomicBool>);

impl SamplingPolicy {
    /// Allow or revoke acceptance of new sampling requests immediately.
    pub fn set_allowed(&self, allowed: bool) {
        self.0.store(allowed, Ordering::SeqCst);
    }

    /// Whether new sampling requests are currently permitted.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub(crate) fn wrap(
        &self,
        handler: Option<Arc<dyn ServerRequestHandler>>,
        advertise_sampling: bool,
    ) -> Option<Arc<dyn ServerRequestHandler>> {
        handler.map(|handler| {
            Arc::new(GatedHandler {
                policy: self.clone(),
                handler,
                advertise_sampling,
            }) as Arc<dyn ServerRequestHandler>
        })
    }
}

struct GatedHandler {
    policy: SamplingPolicy,
    handler: Arc<dyn ServerRequestHandler>,
    advertise_sampling: bool,
}

impl ServerRequestHandler for GatedHandler {
    fn handle(&self, method: &str, params: &Value) -> Option<Result<Value, RpcError>> {
        if method.starts_with("sampling/") && !self.policy.is_allowed() {
            return None;
        }
        self.handler.handle(method, params)
    }

    fn capabilities(&self) -> Value {
        let mut capabilities = self.handler.capabilities();
        if !self.advertise_sampling
            && let Some(capabilities) = capabilities.as_object_mut()
        {
            capabilities.remove("sampling");
        }
        capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Handler;

    impl ServerRequestHandler for Handler {
        fn handle(&self, _: &str, _: &Value) -> Option<Result<Value, RpcError>> {
            Some(Ok(json!({})))
        }

        fn capabilities(&self) -> Value {
            json!({"sampling": {}, "other": {}})
        }
    }

    #[test]
    fn existing_handler_copies_observe_live_sampling_policy() {
        let policy = SamplingPolicy::default();
        let handler = policy.wrap(Some(Arc::new(Handler)), true).unwrap();
        let existing = Arc::clone(&handler);
        for allowed in [false, true, false, true] {
            policy.set_allowed(allowed);
            assert_eq!(
                existing
                    .handle("sampling/createMessage", &json!({}))
                    .is_some(),
                allowed
            );
            let next = policy.wrap(Some(Arc::new(Handler)), allowed).unwrap();
            assert_eq!(next.capabilities().get("sampling").is_some(), allowed);
            assert!(existing.capabilities().get("sampling").is_some());
            assert!(existing.handle("other/request", &json!({})).is_some());
            assert!(existing.capabilities().get("other").is_some());
        }
    }
}
