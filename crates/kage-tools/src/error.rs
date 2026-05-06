//! Errors raised by [`Tool::execute`](crate::Tool::execute).

use std::path::PathBuf;

/// Failure modes shared by all tools.
///
/// Tools surface user-visible failures by returning a [`ToolOutput`](kage_core::ToolOutput)
/// with `is_error = true` rather than raising one of these. `ToolError` is
/// reserved for internal failures (bad input, IO errors, cancellation) that
/// the loop needs to react to differently than "the model's plan didn't pan out".
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The model passed input the tool could not deserialize or validate.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// The tool was given a path that resolved outside the allowed root.
    #[error("invalid path {path:?}: {reason}")]
    Path {
        /// Offending path.
        path: PathBuf,
        /// Why the path was rejected.
        reason: String,
    },

    /// Filesystem or process I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON encode or decode failure when deserializing input.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Operation exceeded its timeout budget.
    #[error("tool {name} timed out after {millis}ms")]
    Timeout {
        /// Tool name.
        name: String,
        /// Timeout that was exceeded, in milliseconds.
        millis: u64,
    },

    /// Cancelled by the caller before completion.
    #[error("cancelled")]
    Cancelled,

    /// Catch-all for anything else.
    #[error("{0}")]
    Other(String),
}
