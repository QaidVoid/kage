//! Core types, events, configuration, and errors shared across the workspace.

pub mod event;
pub mod message;

pub use event::{LoopError, LoopEvent, TokenUsage, ToolOutput};
pub use message::{Content, ImageSource, Message, MessageId, Role, ToolCallId};
