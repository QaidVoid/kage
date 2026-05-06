//! Core types, events, configuration, and errors shared across the workspace.

pub mod message;

pub use message::{Content, ImageSource, Message, MessageId, Role, ToolCallId};
