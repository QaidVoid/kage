//! Sandboxed Lua plugin runtime and extension API.
//!
//! Plugins extend kage at runtime in Lua. The host loads a [`PluginRuntime`]
//! per process, evaluates plugin scripts against it, and dispatches loop
//! events through the runtime so plugins can react.

pub mod api;
pub mod commands;
pub mod error;
pub mod events;
pub mod runtime;
pub mod tools;

pub use api::{HostLog, LogLevel, SharedHostLog, StderrHostLog, default_host_log};
pub use commands::{CommandOutput, LuaCommand};
pub use error::PluginError;
pub use runtime::{PluginRuntime, PluginRuntimeBuilder, SANDBOX_REMOVALS, SharedLua};
pub use tools::LuaTool;
