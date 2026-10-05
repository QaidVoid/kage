//! Error type returned by session reader and writer.

use std::path::PathBuf;

use crate::entry::EntryId;

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
    /// JSON decoding failed for a line.
    ///
    /// An unterminated trailing line is instead treated as a torn
    /// write and forgiven, so this error names a line the file really
    /// holds complete.
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
    /// Session file exists but holds no entries at all.
    #[error("session {path} is empty")]
    Empty {
        /// Path of the offending file.
        path: PathBuf,
    },
    /// First entry of the session file is not a header, so the file
    /// can neither be replayed nor forked from.
    #[error("session {path}: first entry is not a header")]
    MissingHeader {
        /// Path of the offending file.
        path: PathBuf,
    },
    /// The file holds a single line that never received its `\n`, so
    /// the session header never landed completely. There is no
    /// complete line to keep and appending would glue the next entry
    /// onto the fragment, so `open` refuses instead of repairing.
    #[error("session {path}: header line never completed, nothing to repair into")]
    TornHeader {
        /// Path of the offending file.
        path: PathBuf,
    },
    /// A header appeared again after the first entry. A session file
    /// carries exactly one header, at the top.
    #[error("session {path}: second header in file")]
    SecondHeader {
        /// Path of the offending file.
        path: PathBuf,
    },
    /// A fork or rewind named an entry id the source session does not
    /// have.
    #[error("session {path} has no entry {at}")]
    EntryNotFound {
        /// Path of the source file.
        path: PathBuf,
        /// The requested entry id.
        at: EntryId,
    },
}
