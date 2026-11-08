//! The plugin surface the agent engine drives while a session runs.
//!
//! [`SessionPlugins`] is what the engine asks for: the tools a session
//! registers, the events and hooks its turns fire, the drains its
//! queues serve, and the session environment lifecycle. The real
//! implementation is the in-process [`PluginRuntime`]; the trait is the
//! seam that lets another implementation stand in for it, so a host
//! whose plugins live in the client can serve the same surface over
//! the wire.
//!
//! The drains mirror the engine's discipline today:
//! [`crate::Invocation::current`] names the session the run thread
//! drives,
//! `take_mcp_restarts` and `take_pending_messages` serve that session,
//! `take_pending_session_ops_for` serves the session it names plus the
//! owner-less ops, and `take_pending_session_ops` drains everything,
//! which the engine only calls when no session is in scope. An
//! implementation must keep that split, except that one serving a
//! single session over a shared connection (the client-hosted proxy)
//! drains that session's ops and the owner-less ones for it, so two
//! sessions on one connection never take each other's ops.

use std::sync::Arc;

use kage_core::SessionId;
use kage_core::environment::Environment;
use serde_json::Value;

use crate::api::SharedHostLog;
use crate::error::PluginError;
use crate::messages::PendingMessage;
use crate::runtime::PluginRuntime;
use crate::sessions::PendingSessionOp;

/// What the engine needs from a session's plugins.
pub trait SessionPlugins: Send + Sync {
    /// The tools plugins register, snapshotted for the session's tool
    /// registry.
    fn registered_tools(&self) -> Vec<Arc<dyn kage_tools::Tool>>;

    /// The tools plugins register as overrides of engine tools.
    fn registered_tool_overrides(&self) -> Vec<Arc<dyn kage_tools::Tool>>;

    /// The in-place renames plugins apply to engine tools.
    fn registered_renames(&self) -> Vec<(String, String)>;

    /// How many handlers a plugin registered for `event`, without
    /// touching the Lua state, so quiet events stay cheap.
    fn handler_count(&self, event: &str) -> usize;

    /// Fire `event` at every handler registered for it.
    fn dispatch_event(&self, event: &str, payload: &Value) -> Result<(), PluginError>;

    /// Queue `event` for handlers, coalescing repeats per invocation.
    fn notify_event(&self, event: &str, payload: &Value) -> Result<(), PluginError>;

    /// Run `event`'s transform handler, if any, and return what it
    /// produced.
    fn dispatch_transform(&self, event: &str, payload: Value) -> Result<Value, PluginError>;

    /// Run `event`'s predicate handler, if any, and return what it
    /// decided.
    fn dispatch_predicate(&self, event: &str, payload: &Value) -> Result<bool, PluginError>;

    /// Drain the MCP restarts plugins asked for in this invocation.
    fn take_mcp_restarts(&self) -> Vec<String>;

    /// Drain the steering and follow-up messages plugins queued in
    /// this invocation.
    fn take_pending_messages(&self) -> Vec<PendingMessage>;

    /// Drain every queued session operation, whatever session it
    /// names, and the owner-less ones. The engine only calls this when
    /// no session is in scope. An implementation bound to one session
    /// drains that session's ops and the owner-less ones.
    fn take_pending_session_ops(&self) -> Vec<PendingSessionOp>;

    /// Drain the session operations queued for `session`, plus the
    /// owner-less ones.
    fn take_pending_session_ops_for(&self, session: SessionId) -> Vec<PendingSessionOp>;

    /// Register the environment a session's plugin work runs in.
    fn set_session_env(&self, session: SessionId, env: Environment);

    /// Forget a session's environment, when the session is gone.
    fn forget_session(&self, session: SessionId);

    /// The host log plugin errors and logs flow to.
    fn sink(&self) -> SharedHostLog;
}

impl SessionPlugins for PluginRuntime {
    fn registered_tools(&self) -> Vec<Arc<dyn kage_tools::Tool>> {
        PluginRuntime::registered_tools(self)
    }

    fn registered_tool_overrides(&self) -> Vec<Arc<dyn kage_tools::Tool>> {
        PluginRuntime::registered_tool_overrides(self)
    }

    fn registered_renames(&self) -> Vec<(String, String)> {
        PluginRuntime::registered_renames(self)
    }

    fn handler_count(&self, event: &str) -> usize {
        PluginRuntime::handler_count(self, event)
    }

    fn dispatch_event(&self, event: &str, payload: &Value) -> Result<(), PluginError> {
        PluginRuntime::dispatch_event(self, event, payload)
    }

    fn notify_event(&self, event: &str, payload: &Value) -> Result<(), PluginError> {
        PluginRuntime::notify_event(self, event, payload)
    }

    fn dispatch_transform(&self, event: &str, payload: Value) -> Result<Value, PluginError> {
        PluginRuntime::dispatch_transform(self, event, payload)
    }

    fn dispatch_predicate(&self, event: &str, payload: &Value) -> Result<bool, PluginError> {
        PluginRuntime::dispatch_predicate(self, event, payload)
    }

    fn take_mcp_restarts(&self) -> Vec<String> {
        PluginRuntime::take_mcp_restarts(self)
    }

    fn take_pending_messages(&self) -> Vec<PendingMessage> {
        PluginRuntime::take_pending_messages(self)
    }

    fn take_pending_session_ops(&self) -> Vec<PendingSessionOp> {
        PluginRuntime::take_pending_session_ops(self)
    }

    fn take_pending_session_ops_for(&self, session: SessionId) -> Vec<PendingSessionOp> {
        PluginRuntime::take_pending_session_ops_for(self, session)
    }

    fn set_session_env(&self, session: SessionId, env: Environment) {
        PluginRuntime::set_session_env(self, session, env);
    }

    fn forget_session(&self, session: SessionId) {
        PluginRuntime::forget_session(self, session);
    }

    fn sink(&self) -> SharedHostLog {
        PluginRuntime::sink(self)
    }
}
