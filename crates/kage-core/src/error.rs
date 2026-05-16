//! Workspace-wide error type.
//!
//! Each crate may layer its own error type on top of these variants; this
//! module captures the failure modes shared across the codebase: filesystem
//! I/O, JSON serialization, configuration loading, path-safety violations,
//! and explicit cancellation.

use std::path::PathBuf;

/// Errors raised across the kage workspace.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem or other I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON encode or decode failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Configuration could not be loaded.
    #[error("config error: {0}")]
    Config(#[from] Box<figment::Error>),

    /// Configuration could not be serialized or written back to TOML.
    #[error("config write error: {0}")]
    ConfigWrite(String),

    /// A path failed safety validation (traversal, escape, or wrong root).
    #[error("invalid path {path:?}: {reason}")]
    InvalidPath {
        /// The offending path.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
    },

    /// The operation was cancelled by its caller.
    #[error("cancelled")]
    Cancelled,
}

impl From<figment::Error> for Error {
    fn from(e: figment::Error) -> Self {
        Self::Config(Box::new(e))
    }
}

/// Result alias used throughout the workspace.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_io_error() {
        let io = std::io::Error::other("disk on fire");
        let err: Error = io.into();
        assert!(matches!(err, Error::Io(_)));
        assert!(err.to_string().contains("disk on fire"));
    }

    #[test]
    fn from_serde_error() {
        let serde = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let err: Error = serde.into();
        assert!(matches!(err, Error::Json(_)));
    }

    #[test]
    fn invalid_path_message_contains_reason() {
        let err = Error::InvalidPath {
            path: PathBuf::from("/etc/shadow"),
            reason: "outside workdir".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("/etc/shadow"));
        assert!(msg.contains("outside workdir"));
    }

    #[test]
    fn cancelled_displays_cleanly() {
        let err = Error::Cancelled;
        assert_eq!(err.to_string(), "cancelled");
    }
}
