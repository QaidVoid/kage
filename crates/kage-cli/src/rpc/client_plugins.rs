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
//! nobody registered with. An owner that stops answering is treated the
//! same way after [`REQUEST_TIMEOUT`], with a warning in the host log;
//! a tool call is bounded by its cancellation instead of a deadline,
//! since a tool may legitimately run for long.

use std::sync::Arc;
use std::time::Duration;

use kage_acp::acp::{
    PluginCallRequest, PluginCallResult, PluginDrainQueue, PluginDrainRequest, PluginDrainResult,
    PluginEventRequest, PluginHookKind, PluginHookRequest, PluginPendingMessage, PluginPendingRole,
    PluginSessionOp, PluginToolSpec, PluginToolsRequest, PluginToolsResult,
};
use kage_acp::agent::AgentPeer;
use kage_core::environment::Environment;
use kage_core::sync::lock;
use kage_core::{Risk, SessionId, ToolOutput};
use kage_jsonrpc::RpcError;
use kage_plugin::messages::{PendingMessage, PendingRole};
use kage_plugin::session_plugins::SessionPlugins;
use kage_plugin::sessions::PendingSessionOp;
use kage_plugin::{LogLevel, PluginError, SharedHostLog, default_host_log};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde_json::Value;

pub(crate) const PLUGIN_TOOLS: &str = "_kage/plugin/tools";
pub(crate) const PLUGIN_CALL: &str = "_kage/plugin/call";
pub(crate) const PLUGIN_HOOK: &str = "_kage/plugin/hook";
pub(crate) const PLUGIN_EVENT: &str = "_kage/plugin/event";
pub(crate) const PLUGIN_DRAIN: &str = "_kage/plugin/drain";

/// How long the owner has to answer a snapshot, event, hook or drain
/// request before the host stops waiting.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The plugin surface of session `session`, answered by the client on
/// `peer`. The snapshot is the one answer to `_kage/plugin/tools`,
/// taken while the opening `session/new` is still in flight.
pub(super) struct ClientPlugins {
    session_id: String,
    peer: AgentPeer,
    timeout: Duration,
    snapshot: PluginToolsResult,
    tools: Vec<Arc<dyn Tool>>,
    overrides: Vec<Arc<dyn Tool>>,
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
    pub(super) fn open(session: SessionId, peer: AgentPeer) -> Result<Self, RpcError> {
        Self::open_with(session, peer, REQUEST_TIMEOUT)
    }

    /// [`Self::open`] with `timeout` as the deadline of every request
    /// to the owner.
    ///
    /// # Errors
    ///
    /// The owner's error, an internal error when it does not answer
    /// within `timeout` or sends a malformed snapshot, and `-32602`
    /// when a tool names a risk the host does not know.
    fn open_with(session: SessionId, peer: AgentPeer, timeout: Duration) -> Result<Self, RpcError> {
        let request = PluginToolsRequest {
            session_id: session.to_string(),
        };
        let params = serde_json::to_value(&request).unwrap_or_default();
        let reply = peer
            .request_timeout(PLUGIN_TOOLS, params, timeout)
            .map_err(|e| {
                if is_timeout(&e, PLUGIN_TOOLS) {
                    RpcError::internal(format!(
                        "the client's plugin host did not answer {PLUGIN_TOOLS} within {}s",
                        timeout.as_secs_f32()
                    ))
                } else {
                    e
                }
            })?;
        let snapshot = serde_json::from_value::<PluginToolsResult>(reply)
            .map_err(|e| RpcError::internal(format!("plugin tools: {e}")))?;
        let session_id = session.to_string();
        let tools = client_tools(&session_id, &peer, &snapshot.tools)?;
        let overrides = client_tools(&session_id, &peer, &snapshot.overrides)?;
        Ok(Self {
            session_id,
            peer,
            timeout,
            snapshot,
            tools,
            overrides,
            sink: default_host_log(),
        })
    }

    /// Ask the owner within the deadline. A silent owner is logged here
    /// and reads as an error the caller degrades on.
    fn ask(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let reply = self.peer.request_timeout(method, params, self.timeout);
        if let Err(e) = &reply
            && is_timeout(e, method)
        {
            lock(&self.sink).log(
                LogLevel::Warn,
                &format!(
                    "the client's plugin host did not answer {method} within {}s",
                    self.timeout.as_secs_f32()
                ),
            );
        }
        reply
    }

    /// Send one `_kage/plugin/event`. The owner being gone passes
    /// through like a handler-less event.
    fn fire_event(&self, request: &PluginEventRequest) -> Result<(), PluginError> {
        let params = serde_json::to_value(request).unwrap_or_default();
        match self.ask(PLUGIN_EVENT, params) {
            Ok(_) => Ok(()),
            Err(e) if e.is_connection_closed() || is_timeout(&e, PLUGIN_EVENT) => Ok(()),
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
        match self.ask(PLUGIN_DRAIN, params) {
            Ok(reply) => serde_json::from_value(reply).unwrap_or_else(|e| {
                lock(&self.sink).log(LogLevel::Warn, &format!("plugin drain: {e}"));
                PluginDrainResult::default()
            }),
            Err(e) => {
                if !e.is_connection_closed() && !is_timeout(&e, PLUGIN_DRAIN) {
                    lock(&self.sink).log(LogLevel::Warn, &format!("plugin drain: {e}"));
                }
                PluginDrainResult::default()
            }
        }
    }
}

impl SessionPlugins for ClientPlugins {
    fn registered_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }

    fn registered_tool_overrides(&self) -> Vec<Arc<dyn Tool>> {
        self.overrides.clone()
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
        match self.ask(PLUGIN_HOOK, params) {
            // A null reply is the handlers' "no change".
            Ok(reply) if reply.is_null() => Ok(request.payload),
            Ok(reply) => Ok(reply),
            Err(e) if e.is_connection_closed() || is_timeout(&e, PLUGIN_HOOK) => {
                Ok(request.payload)
            }
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
        match self.ask(PLUGIN_HOOK, params) {
            Ok(reply) => serde_json::from_value::<bool>(reply)
                .map_err(|e| PluginError::Host(format!("{} predicate: {e}", request.event))),
            Err(e) if e.is_connection_closed() || is_timeout(&e, PLUGIN_HOOK) => Ok(false),
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
        // A proxy serves one session, so "everything" is that session's
        // ops plus the owner-less ones. Draining across the connection
        // would take another client-hosted session's ops.
        self.drain(
            PluginDrainQueue::SessionOpsFor,
            Some(self.session_id.clone()),
        )
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
    peer: AgentPeer,
    spec: PluginToolSpec,
    risk: Risk,
}

impl ClientTool {
    /// Fails with `-32602` when the spec's risk is not one the host
    /// knows, so a tool never runs under a weaker class than it asked
    /// for.
    fn new(session_id: &str, peer: &AgentPeer, spec: &PluginToolSpec) -> Result<Self, RpcError> {
        let risk = match spec.risk.as_deref() {
            None => Risk::Read,
            Some(name) => serde_json::from_value::<Risk>(Value::from(name)).map_err(|_| {
                RpcError::new(
                    -32602,
                    format!("plugin tool `{}`: unknown risk `{name}`", spec.name),
                )
            })?,
        };
        Ok(Self {
            session_id: session_id.to_owned(),
            peer: peer.clone(),
            spec: spec.clone(),
            risk,
        })
    }
}

/// The specs as registry tools whose calls run on the client.
fn client_tools(
    session_id: &str,
    peer: &AgentPeer,
    specs: &[PluginToolSpec],
) -> Result<Vec<Arc<dyn Tool>>, RpcError> {
    specs
        .iter()
        .map(|spec| Ok(Arc::new(ClientTool::new(session_id, peer, spec)?) as Arc<dyn Tool>))
        .collect()
}

/// Whether `error` is `method` running past its deadline.
fn is_timeout(error: &RpcError, method: &str) -> bool {
    *error == RpcError::timed_out(method)
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
        self.risk
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
            .request_cancellable(PLUGIN_CALL, params, cx.cancel_flag())
            .map_err(|e| {
                if cx.is_cancelled() {
                    ToolError::Cancelled
                } else {
                    ToolError::Other(format!("plugin tool `{}`: {e}", self.spec.name))
                }
            })?;
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

#[cfg(test)]
#[path = "client_plugins_tests.rs"]
mod tests;
