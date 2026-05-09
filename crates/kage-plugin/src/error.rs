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
}
