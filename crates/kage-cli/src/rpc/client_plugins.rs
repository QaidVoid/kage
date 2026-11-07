//! The session-plugin surface of a client-hosted session.
//!
//! A session opened with `_meta.kage.plugins = "client"` runs its
//! plugins inside the ACP client that owns it. [`ClientPlugins`] stands
//! in for the in-process [`PluginRuntime`]: it fetches what the
//! client's plugins register once at open, then answers the engine's
//! [`SessionPlugins`] calls with `_kage/plugin/{tools,call,hook,event,drain}`
//! requests to that client. Hooks fire only for events the client
//! reported handlers for, and once the owner connection is gone the
//! tools fail while the hooks and drains pass through, like a runtime
//! nobody registered with.

use std::sync::Arc;

use kage_acp::acp::{
    PluginCallRequest, PluginCallResult, PluginDrainQueue, PluginDrainRequest, PluginDrainResult,
    PluginEventRequest, PluginHookKind, PluginHookRequest, PluginPendingMessage, PluginPendingRole,
    PluginSessionOp, PluginToolSpec, PluginToolsRequest, PluginToolsResult,
};
use kage_core::environment::Environment;
use kage_core::sync::lock;
use kage_core::{Risk, SessionId, ToolOutput};
use kage_jsonrpc::{Peer, RpcError};
use kage_plugin::messages::{PendingMessage, PendingRole};
use kage_plugin::session_plugins::SessionPlugins;
use kage_plugin::sessions::PendingSessionOp;
use kage_plugin::{LogLevel, PluginError, SharedHostLog, default_host_log};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde_json::Value;

const PLUGIN_TOOLS: &str = "_kage/plugin/tools";
const PLUGIN_CALL: &str = "_kage/plugin/call";
const PLUGIN_HOOK: &str = "_kage/plugin/hook";
const PLUGIN_EVENT: &str = "_kage/plugin/event";
const PLUGIN_DRAIN: &str = "_kage/plugin/drain";

/// The plugin surface of session `session`, answered by the client on
/// `peer`. The snapshot is the one answer to `_kage/plugin/tools`,
/// taken while the opening `session/new` is still in flight.
pub(super) struct ClientPlugins {
    session_id: String,
    peer: Peer,
    snapshot: PluginToolsResult,
    sink: SharedHostLog,
}

impl std::fmt::Debug for ClientPlugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientPlugins")
            .field("session_id", &self.session_id)
            .field("tools", &self.snapshot.tools.len())
            .field("overrides", &self.snapshot.overrides.len())
            .field("renames", &self.snapshot.renames.len())
            .finish_non_exhaustive()
    }
}

impl ClientPlugins {
    /// Ask the client that owns `session` what its plugins register.
    /// Fails the session open when the client does not answer, since it
    /// asked to host the plugins in the first place.
    pub(super) fn open(session: SessionId, peer: Peer) -> Result<Self, RpcError> {
        let request = PluginToolsRequest {
            session_id: session.to_string(),
        };
        let params = serde_json::to_value(&request).unwrap_or_default();
        let reply = peer.request(PLUGIN_TOOLS, params)?;
        let snapshot = serde_json::from_value::<PluginToolsResult>(reply)
            .map_err(|e| RpcError::internal(format!("plugin tools: {e}")))?;
        Ok(Self {
            session_id: session.to_string(),
            peer,
            snapshot,
            sink: default_host_log(),
        })
    }

    /// The specs as registry tools whose calls run on the client.
    fn tools(&self, specs: &[PluginToolSpec]) -> Vec<Arc<dyn Tool>> {
        specs
            .iter()
            .map(|spec| {
                Arc::new(ClientTool {
                    session_id: self.session_id.clone(),
                    peer: self.peer.clone(),
                    spec: spec.clone(),
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    /// Send one `_kage/plugin/event`. The owner being gone passes
    /// through like a handler-less event.
    fn fire_event(&self, request: &PluginEventRequest) -> Result<(), PluginError> {
        let params = serde_json::to_value(request).unwrap_or_default();
        match self.peer.request(PLUGIN_EVENT, params) {
            Ok(_) => Ok(()),
            Err(e) if e.is_connection_closed() => Ok(()),
            Err(e) => Err(PluginError::Host(format!("{}: {e}", request.event))),
        }
    }

    /// Drain one queue from the client. `session` names the session
    /// when the queue serves one. A gone owner yields an empty queue;
    /// a live one that fails the ask logs and yields an empty one.
    fn drain(&self, queue: PluginDrainQueue, session: Option<String>) -> PluginDrainResult {
        let request = PluginDrainRequest {
            session_id: session,
            queue,
        };
        let params = serde_json::to_value(&request).unwrap_or_default();
        match self.peer.request(PLUGIN_DRAIN, params) {
            Ok(reply) => serde_json::from_value(reply).unwrap_or_else(|e| {
                lock(&self.sink).log(LogLevel::Warn, &format!("plugin drain: {e}"));
                PluginDrainResult::default()
            }),
            Err(e) => {
                if !e.is_connection_closed() {
                    lock(&self.sink).log(LogLevel::Warn, &format!("plugin drain: {e}"));
                }
                PluginDrainResult::default()
            }
        }
    }
}

impl SessionPlugins for ClientPlugins {
    fn registered_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools(&self.snapshot.tools)
    }

    fn registered_tool_overrides(&self) -> Vec<Arc<dyn Tool>> {
        self.tools(&self.snapshot.overrides)
    }

    fn registered_renames(&self) -> Vec<(String, String)> {
        self.snapshot
            .renames
            .iter()
            .map(|rename| (rename.from.clone(), rename.to.clone()))
            .collect()
    }

    fn handler_count(&self, event: &str) -> usize {
        self.snapshot.handlers.get(event).copied().unwrap_or(0) as usize
    }

    fn dispatch_event(&self, event: &str, payload: &Value) -> Result<(), PluginError> {
        if self.handler_count(event) == 0 {
            return Ok(());
        }
        self.fire_event(&PluginEventRequest {
            session_id: self.session_id.clone(),
            event: event.to_owned(),
            payload: payload.clone(),
            notify: false,
        })
    }

    fn notify_event(&self, event: &str, payload: &Value) -> Result<(), PluginError> {
        if self.handler_count(event) == 0 {
            return Ok(());
        }
        let request = PluginEventRequest {
            session_id: self.session_id.clone(),
            event: event.to_owned(),
            payload: payload.clone(),
            notify: true,
        };
        // The concrete runtime queues and logs handler trouble instead
        // of failing the fire-and-forget caller; the proxy does too.
        if let Err(err) = self.fire_event(&request) {
            lock(&self.sink).log(
                LogLevel::Error,
                &format!("{} dispatch: {err}", request.event),
            );
        }
        Ok(())
    }

    fn dispatch_transform(&self, event: &str, payload: Value) -> Result<Value, PluginError> {
        if self.handler_count(event) == 0 {
            return Ok(payload);
        }
        let request = PluginHookRequest {
            session_id: self.session_id.clone(),
            event: event.to_owned(),
            kind: PluginHookKind::Transform,
            payload,
        };
        let params = serde_json::to_value(&request).unwrap_or_default();
        match self.peer.request(PLUGIN_HOOK, params) {
            // A null reply is the handlers' "no change".
            Ok(reply) if reply.is_null() => Ok(request.payload),
            Ok(reply) => Ok(reply),
            Err(e) if e.is_connection_closed() => Ok(request.payload),
            Err(e) => Err(PluginError::Host(format!(
                "{} transform: {e}",
                request.event
            ))),
        }
    }

    fn dispatch_predicate(&self, event: &str, payload: &Value) -> Result<bool, PluginError> {
        if self.handler_count(event) == 0 {
            return Ok(false);
        }
        let request = PluginHookRequest {
            session_id: self.session_id.clone(),
            event: event.to_owned(),
            kind: PluginHookKind::Predicate,
            payload: payload.clone(),
        };
        let params = serde_json::to_value(&request).unwrap_or_default();
        match self.peer.request(PLUGIN_HOOK, params) {
            Ok(reply) => serde_json::from_value::<bool>(reply)
                .map_err(|e| PluginError::Host(format!("{} predicate: {e}", request.event))),
            Err(e) if e.is_connection_closed() => Ok(false),
            Err(e) => Err(PluginError::Host(format!(
                "{} predicate: {e}",
                request.event
            ))),
        }
    }

    fn take_mcp_restarts(&self) -> Vec<String> {
        self.drain(PluginDrainQueue::McpRestarts, Some(self.session_id.clone()))
            .restarts
    }

    fn take_pending_messages(&self) -> Vec<PendingMessage> {
        self.drain(PluginDrainQueue::Messages, Some(self.session_id.clone()))
            .messages
            .into_iter()
            .map(pending_message)
            .collect()
    }

    fn take_pending_session_ops(&self) -> Vec<PendingSessionOp> {
        self.drain(PluginDrainQueue::SessionOps, None)
            .ops
            .into_iter()
            .map(pending_session_op)
            .collect()
    }

    fn take_pending_session_ops_for(&self, session: SessionId) -> Vec<PendingSessionOp> {
        self.drain(PluginDrainQueue::SessionOpsFor, Some(session.to_string()))
            .ops
            .into_iter()
            .map(pending_session_op)
            .collect()
    }

    /// The client owns its plugins' environments; the host keeps none.
    fn set_session_env(&self, _session: SessionId, _env: Environment) {}

    fn forget_session(&self, _session: SessionId) {}

    fn sink(&self) -> SharedHostLog {
        Arc::clone(&self.sink)
    }
}

/// One tool a client-hosted plugin registered; the call runs on the
/// client over `_kage/plugin/call`.
struct ClientTool {
    session_id: String,
    peer: Peer,
    spec: PluginToolSpec,
}

impl std::fmt::Debug for ClientTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientTool")
            .field("name", &self.spec.name)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl Tool for ClientTool {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn schema(&self) -> serde_json::Value {
        self.spec.schema.clone()
    }

    fn risk(&self) -> Risk {
        tool_risk(self.spec.risk.as_deref())
    }

    fn execution_mode(&self) -> Option<ExecMode> {
        self.spec.parallel.map(|parallel| {
            if parallel {
                ExecMode::Parallel
            } else {
                ExecMode::Sequential
            }
        })
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        if cx.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let request = PluginCallRequest {
            session_id: self.session_id.clone(),
            tool: self.spec.name.clone(),
            input,
        };
        let params = serde_json::to_value(&request).unwrap_or_default();
        let reply = self
            .peer
            .request(PLUGIN_CALL, params)
            .map_err(|e| ToolError::Other(format!("plugin tool `{}`: {e}", self.spec.name)))?;
        let result = serde_json::from_value::<PluginCallResult>(reply)
            .map_err(|e| ToolError::Other(format!("plugin tool `{}`: {e}", self.spec.name)))?;
        Ok(ToolOutput {
            is_error: result.is_error,
            text: result.text,
            structured: result.structured,
            // Plugin tools speak the Lua tool API, which carries no
            // termination signal or media.
            terminate: false,
            media: Vec::new(),
        })
    }
}

/// The spec's risk classification; an unset or unknown one reads.
fn tool_risk(risk: Option<&str>) -> Risk {
    risk.and_then(|risk| serde_json::from_value::<Risk>(Value::from(risk)).ok())
        .unwrap_or(Risk::Read)
}

/// The plugin form of one queued message.
fn pending_message(message: PluginPendingMessage) -> PendingMessage {
    PendingMessage {
        text: message.text,
        trigger_turn: message.trigger_turn,
        deliver_as: match message.deliver_as {
            PluginPendingRole::User => PendingRole::User,
            PluginPendingRole::Assistant => PendingRole::Assistant,
            PluginPendingRole::System => PendingRole::System,
        },
    }
}

/// The plugin form of one queued session write.
fn pending_session_op(op: PluginSessionOp) -> PendingSessionOp {
    match op {
        PluginSessionOp::AppendCustom { kind, data } => {
            PendingSessionOp::AppendCustom { kind, data }
        }
        PluginSessionOp::SetLabel { anchor, text } => PendingSessionOp::SetLabel { anchor, text },
    }
}
