//! Sandboxed Lua plugin runtime and extension API.
//!
//! Plugins extend kage at runtime in Lua. The host loads a [`PluginRuntime`]
//! per process, evaluates plugin scripts against it, and dispatches loop
//! events through the runtime so plugins can react.

pub mod error;
pub mod runtime;

pub use error::PluginError;
pub use runtime::{PluginRuntime, SANDBOX_REMOVALS};
