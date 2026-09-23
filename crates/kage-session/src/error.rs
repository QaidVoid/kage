//! Error type returned by session reader and writer.

use std::path::PathBuf;

/// Anything that can go wrong reading or writing a session file.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// I/O failure on a session file. The path is included for context.
    #[error("session io failed at {path}: {source}")]
    Io {
        /// Path of the offending file.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// JSON encoding failed while serializing an entry.
    #[error("session encode failed at {path}: {source}")]
    Encode {
        /// Path the entry was destined for.
        path: PathBuf,
        /// Underlying `serde_json` error.
        #[source]
        source: serde_json::Error,
    },
    /// JSON decoding failed for a non-trailing line.
    ///
    /// Callers reading a session may choose to surface this as a hard error
    /// or to log and continue; the reader does both depending on whether the
    /// failed line was the final line of the file (treated as a torn write).
    #[error("session decode failed at {path} line {line}: {source}")]
    Decode {
        /// Path of the file being read.
        path: PathBuf,
        /// 1-based line number of the offending line.
        line: usize,
        /// Underlying `serde_json` error.
        #[source]
        source: serde_json::Error,
    },
    /// Session file was written with a schema version this build cannot
    /// interpret. Refusing up front beats opaque decode errors partway
    /// through replaying a newer format.
    #[error("session {path} uses format version {found}; this build supports version {supported}")]
    UnsupportedVersion {
        /// Path of the offending file.
        path: PathBuf,
        /// Version recorded in the file's header.
        found: u32,
        /// Highest version this build understands.
        supported: u32,
    },
    /// Another process holds the advisory lock on this session file.
    /// Appending from two processes would interleave two JSONL streams
    /// into one file.
    #[error("session {path} is locked by another kage process")]
    Locked {
        /// Path of the locked file.
        path: PathBuf,
    },
}
