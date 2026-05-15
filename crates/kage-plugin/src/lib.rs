//! Sandboxed Lua plugin runtime and extension API.
//!
//! Plugins extend kage at runtime in Lua. The host loads a [`PluginRuntime`]
//! per process, evaluates plugin scripts against it, and dispatches loop
//! events through the runtime so plugins can react.

pub mod api;
pub mod bridge;
pub mod commands;
pub mod error;
pub mod events;
pub mod fs;
pub mod http;
pub mod lifecycle;
pub mod loader;
pub mod messages;
pub mod providers;
pub mod runtime;
pub mod sessions;
pub mod status;
pub mod tools;
pub mod ui;
pub mod watcher;
pub mod widgets;

pub use api::{HostLog, LogLevel, SharedHostLog, StderrHostLog, default_host_log};
pub use bridge::{BridgeStep, SharedBridge, SuspendRequest};
pub use commands::{BridgeArgs, BridgePrep, CommandOutput, LuaCommand, PluginArgSpec};
pub use error::PluginError;
pub use events::{DiscoveryEntries, SessionOpDecision};
pub use lifecycle::{SharedCompactRequest, SharedUsage};
pub use loader::{LoadReport, load_dir};
pub use messages::{PendingMessage, PendingRole, SharedPendingMessages};
pub use providers::LuaProvider;
pub use runtime::{PluginRuntime, PluginRuntimeBuilder, SANDBOX_REMOVALS, SharedLua};
pub use sessions::{PendingSessionOp, SharedForkRequest, SharedSessionList, SharedSessionOps};
pub use status::SharedStatus;
pub use tools::LuaTool;
pub use ui::{SelectItem, SelectRequest};
pub use watcher::PluginWatcher;
pub use widgets::LuaWidget;
