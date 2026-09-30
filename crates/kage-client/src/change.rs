//! What one incoming frame changed, for hosts redrawing from events.
//!
//! [`Client::handle`](crate::Client::handle) returns the scopes that
//! moved, in the order the frame applied them. A host redraws exactly
//! those and nothing else.

use serde_json::Value;

use crate::frame::RpcError;
use kage_acp_wire::FsResult;

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
            Self::Fs { session_id, .. } => Some(session_id),
            Self::Connection | Self::Directory | Self::Config { .. } | Self::Failed { .. } => None,
        }
    }
}
