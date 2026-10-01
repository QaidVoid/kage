//! The sans-IO client side of the Agent Client Protocol.
//!
//! [`Client`] mirrors what an agent tells it into a plain [`State`]
//! any view can render, and turns commands into [`Frame`] messages to
//! send. It owns the JSON-RPC framing and the request ids, keeps the
//! pending requests so each answer lands on the session that asked,
//! and does no IO of its own: a host drains
//! [`Client::take_outgoing`] and feeds [`Client::handle`], one frame
//! at a time, over any transport and any threading model, wasm
//! included.
//!
//! The state is the honest sum of what frames delivered: open
//! [`Session`]s with their grouped transcript items, usage, config
//! options, permission asks, queued prompts and the subagent tree,
//! plus the directory a `session/list` answered with. Nothing is
//! assumed that no frame carried, and [`Change`] reports exactly what
//! a frame moved so a host redraws only that.

#![forbid(unsafe_code)]

mod change;
mod client;
mod frame;
mod state;

pub use change::Change;
pub use client::{Client, PermissionDecision, PromptOutcome, SteerError};
pub use frame::{Frame, RpcError};
pub use state::{
    PermissionAsk, QueuedPrompt, SUBJECT_KEYS, Session, State, Subagent, ToolCallItem,
    TranscriptItem, Usage,
};

pub use kage_acp_wire as wire;
