//! Core types, events, configuration, and errors shared across the workspace.

pub mod cancel;
pub mod config;
pub mod error;
pub mod event;
pub mod message;
pub mod risk;
pub mod skills;
pub mod templates;
pub mod tool_spec;

pub use cancel::CancelFlag;
pub use config::{
    Config, KeybindingsConfig, PluginsConfig, ProviderConfig, SandboxBackend, SandboxConfig,
    UiConfig,
};
pub use error::{Error, Result};
pub use event::{LoopError, LoopEvent, TokenUsage, ToolOutput, ToolUpdate};
pub use message::{Content, ImageSource, Message, MessageId, Role, ToolCallId};
pub use risk::{Risk, classify};
pub use skills::{Skill, SkillError, load_skill_file, load_skills_dir};
pub use templates::{
    Template, TemplateError, load_template_file, load_templates_dir, render_template,
};
pub use tool_spec::ToolSpec;
