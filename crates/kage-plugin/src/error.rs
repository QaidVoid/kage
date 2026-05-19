//! Error type returned by the plugin runtime.

/// Anything that can go wrong loading or running a plugin.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// Lua VM raised an error during runtime construction or execution.
    #[error("lua error: {0}")]
    Lua(#[from] mlua::Error),
    /// Plugin script could not be read from disk.
    #[error("plugin io error at {path}: {source}")]
    Io {
        /// Path that failed.
        path: std::path::PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A bridged coroutine is already parked. Only one blocking plugin
    /// call may be in flight at a time; the caller must resolve the
    /// outstanding one before starting another.
    #[error("plugin bridge busy: a blocking call is already in progress")]
    BridgeBusy,
    /// `bridge_resume` or `bridge_cancel` was called with no coroutine
    /// parked.
    #[error("plugin bridge idle: no suspended coroutine to resume")]
    BridgeIdle,
    /// A bridged coroutine yielded a value that is not a kage suspend
    /// request, or settled in an unexpected thread state.
    #[error("plugin bridge protocol error: {0}")]
    BridgeProtocol(String),
    /// Plugin configuration was invalid - e.g. a capability grant
    /// named a capability that does not exist.
    #[error("plugin config error: {0}")]
    Config(String),
}
