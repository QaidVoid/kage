//! Sandboxed Lua plugin runtime and extension API.
//!
//! Plugins extend kage at runtime in Lua. The host loads a [`PluginRuntime`]
//! per process, evaluates plugin scripts against it, and dispatches loop
//! events through the runtime so plugins can react.

pub mod api;
pub mod commands;
pub mod error;
pub mod events;
pub mod fs;
pub mod http;
pub mod loader;
pub mod providers;
pub mod runtime;
pub mod tools;
pub mod watcher;

pub use api::{HostLog, LogLevel, SharedHostLog, StderrHostLog, default_host_log};
pub use commands::{CommandOutput, LuaCommand, PluginArgSpec};
pub use error::PluginError;
pub use loader::{LoadReport, load_dir};
pub use providers::LuaProvider;
pub use runtime::{PluginRuntime, PluginRuntimeBuilder, SANDBOX_REMOVALS, SharedLua};
pub use tools::LuaTool;
pub use watcher::PluginWatcher;
