//! Append-only JSONL session storage with branching.
//!
//! Sessions are stored as line-delimited JSON: a single
//! [`entry::Header`](entry::Header) on the first line, followed by any number
//! of [`SessionEntry`] lines appended in order. The format is intentionally
//! plain text so it can be inspected with `cat`, searched with `rg`, and
//! transported by ordinary file copy.

pub mod entry;
pub mod error;
pub mod fork;
pub mod list;
pub mod reader;
pub mod resume;
pub mod search;
pub mod writer;

pub use entry::{
    Compaction, Custom, EntryId, FORMAT_VERSION, Header, Label, MessageEntry, ModelChange,
    SessionEntry, SessionId, ThinkingLevelChange,
};
pub use error::SessionError;
pub use fork::{fork, resolve_entry_prefix};
pub use list::{SessionSummary, list};
pub use reader::SessionReader;
pub use resume::{ReplayResult, find_by_prefix, find_last, replay};
pub use search::{SearchHit, search};
pub use writer::SessionWriter;
