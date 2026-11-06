//! Append-only JSONL session storage with branching.
//!
//! Layering: depends only on `kage-core`.
//!
//! Sessions are stored as line-delimited JSON: a single
//! [`entry::Header`] on the first line, followed by any number
//! of [`SessionEntry`] lines appended in order. An undo or rewind
//! appends a [`SessionEntry::Branch`] marker instead of rewriting: the
//! conversation continues from the entry the marker names, and the
//! entries it abandons stay in the file (see [`tree::SessionTree`]).
//! The format is intentionally
//! plain text so it can be inspected with `cat`, searched with `rg`, and
//! transported by ordinary file copy.

pub mod blobs;
pub mod entry;
pub mod error;
pub mod external_replay;
pub mod fork;
pub mod list;
pub mod reader;
pub mod resume;
pub mod search;
pub mod tree;
pub mod writer;

pub use blobs::BlobStore;
pub use entry::{
    BranchEntry, Compaction, Custom, EntryId, ExternalAcpSession, FORMAT_VERSION, Header, Label,
    MessageEntry, ModelChange, SessionBackend, SessionEntry, SessionId, SessionTitle,
    ThinkingLevelChange,
};
pub use error::SessionError;
pub use fork::{Snapshot, fork, fork_as, resolve_entry_prefix, snapshot};
pub use list::{SessionCache, SessionSummary, is_agent_session, list};
pub use reader::SessionReader;
pub use resume::{ReplayResult, ReplayUsage, find_by_prefix, find_last, replay};
pub use search::{SearchHit, search};
pub use tree::{SessionTree, TreeNode, TreeWalk};
pub use writer::{SessionWriter, delete_locked, is_locked};
