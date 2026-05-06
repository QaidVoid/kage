//! Core types, events, configuration, and errors shared across the workspace.

pub mod config;
pub mod error;
pub mod event;
pub mod message;
pub mod risk;

pub use config::{
    Config, KeybindingsConfig, PluginsConfig, ProviderConfig, SandboxBackend, SandboxConfig,
    UiConfig,
};
pub use error::{Error, Result};
pub use event::{LoopError, LoopEvent, TokenUsage, ToolOutput};
pub use message::{Content, ImageSource, Message, MessageId, Role, ToolCallId};
pub use risk::{Risk, classify};
