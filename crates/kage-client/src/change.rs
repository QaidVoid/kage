//! What one incoming frame changed, for hosts redrawing from events.
//!
//! [`Client::handle`](crate::Client::handle) returns the scopes that
//! moved, in the order the frame applied them. A host redraws exactly
//! those and nothing else.

use serde_json::Value;

use crate::frame::RpcError;
use kage_acp_wire::{FsResult, ModelProvider, OptionEntry};

/// The scopes and one-shot answers a handled frame produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// The `initialize` handshake answered: protocol version, agent
    /// identity and capabilities are known.
    Connection,
    /// A session was created, opened, released, or its metadata,
    /// config options, usage, queue or run state changed.
    Session {
        /// The session that moved.
        id: String,
    },
    /// A session's transcript items changed.
    Transcript {
        /// The session whose transcript moved.
        id: String,
    },
    /// A session's permission asks changed: one arrived, was answered
    /// or was withdrawn.
    Permission {
        /// The session whose ask queue moved.
        id: String,
    },
    /// An open ask was withdrawn through `$/cancel_request`: another
    /// client answered it, or the agent took it back. The ask left
    /// this client's queue without a reply of ours and no decision
    /// was recorded. A host closes the card on this and may toast
    /// that the ask was answered elsewhere.
    AnsweredElsewhere {
        /// The session whose ask was withdrawn.
        id: String,
        /// The request id that was withdrawn.
        request_id: u64,
    },
    /// A session's agent tree changed.
    Agents {
        /// The session whose subagents moved.
        id: String,
    },
    /// The recorded-session directory moved: a `session/list` page
    /// landed.
    Directory,
    /// A `_kage/config/get` answer arrived. The read-only
    /// configuration is delivered here and not kept in state, because
    /// the wire carries it raw.
    Config {
        /// The id of the request answered: a `_kage/config/get`, or the
        /// `_kage/config/set` whose write this snapshot follows.
        request: u64,
        /// The configuration sections as the wire answered.
        config: Value,
    },
    /// A `_kage/fs` answer arrived for a session. The result is
    /// delivered here and not kept in state, because a listing or a
    /// file is a one-shot answer to one picker.
    Fs {
        /// The session the operation ran against.
        session_id: String,
        /// The listing or file read.
        result: FsResult,
    },
    /// A `_kage/session/fork` answer arrived: `to` is a new recorded
    /// session copied from `from`, ready to open.
    Forked {
        /// The session that was copied.
        from: String,
        /// The copy.
        to: String,
    },
    /// A `_kage/session/export` answer arrived. The Markdown is
    /// delivered here and not kept in state.
    Exported {
        /// The session that was rendered.
        session_id: String,
        /// The transcript as Markdown.
        markdown: String,
    },
    /// A `_kage/models/list` answer arrived: every model the engine can
    /// run now, by provider.
    Models {
        /// The providers with their models.
        providers: Vec<ModelProvider>,
    },
    /// A `_kage/options` answer arrived: every engine option a client
    /// can change, with its value in effect.
    Options {
        /// The options, in the engine's order.
        options: Vec<OptionEntry>,
    },
    /// A `_kage/config/test` answer arrived: whether the provider's
    /// model list answered, and what it listed.
    Tested {
        /// The id of the test request, so a form matches its answer.
        request: u64,
        /// What the engine found.
        result: kage_acp_wire::ConfigTestResult,
    },
    /// A `_kage/providers/directory` answer arrived.
    ProviderDirectory {
        /// The id of the request, so a picker matches its answer.
        request: u64,
        /// The providers the directory lists.
        providers: Vec<kage_acp_wire::DirectoryProvider>,
    },
    /// A plugin was installed or removed.
    Plugins {
        /// The id of the request answered.
        request: u64,
    },
    /// A `_kage/auth/set` answer arrived: the provider's key was saved
    /// or removed.
    KeySaved {
        /// The provider the key is for.
        provider: String,
    },
    /// A request the client sent was answered with an error. The
    /// client keeps no error log; a host surfaces this where it fits.
    Failed {
        /// The id of the request that failed.
        request: u64,
        /// The error the agent answered with.
        error: RpcError,
    },
}

impl Change {
    /// The session this change is about, when it is about one.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Session { id }
            | Self::Transcript { id }
            | Self::Permission { id }
            | Self::AnsweredElsewhere { id, .. }
            | Self::Agents { id } => Some(id),
            Self::Fs { session_id, .. } | Self::Exported { session_id, .. } => Some(session_id),
            Self::Forked { from, .. } => Some(from),
            Self::Connection
            | Self::Directory
            | Self::Config { .. }
            | Self::Models { .. }
            | Self::Options { .. }
            | Self::Tested { .. }
            | Self::ProviderDirectory { .. }
            | Self::Plugins { .. }
            | Self::KeySaved { .. }
            | Self::Failed { .. } => None,
        }
    }
}
