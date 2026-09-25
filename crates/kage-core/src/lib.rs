//! Core types, events, configuration, and errors shared across the workspace.
//!
//! Layering: workspace leaf; depends on no other `kage-*` crate.

pub mod agents;
pub mod cancel;
pub mod config;
pub mod error;
pub mod event;
pub mod highlight;
pub mod keymap;
pub mod message;
pub mod options;
pub mod permissions;
pub mod protocol;
pub mod risk;
pub mod skills;
pub mod sync;
pub mod templates;
pub mod thinking;
pub mod tool_spec;
pub mod trust;

pub use agents::{AgentDef, AgentDefs, AgentError, AgentSource, load_agent_file, load_agents_dir};
pub use cancel::CancelFlag;
pub use config::{
    AgentsConfig, Config, KeybindingsConfig, PluginsConfig, ProviderConfig, SandboxBackend,
    SandboxConfig, UiConfig,
};
pub use error::{Error, Result};
pub use event::{LoopError, LoopEvent, StopReason, TokenCost, TokenUsage, ToolOutput, ToolUpdate};
pub use message::{Content, ImageSource, Message, MessageId, Role, ToolCallId};
pub use protocol::SessionId;
pub use risk::{Risk, classify};
pub use skills::{Skill, SkillError, load_skill_file, load_skills_dir};
pub use templates::{
    Template, TemplateError, load_template_file, load_templates_dir, render_template,
};
pub use thinking::ThinkingLevel;
pub use tool_spec::ToolSpec;

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    /// Serializes tests that touch figment's process-global state.
    /// `Jail` mutates the process cwd, and `jail.set_env` mutates the
    /// process environment, which every config load observes.
    static PROCESS_GLOBALS: Mutex<()> = Mutex::new(());

    pub(crate) fn process_globals() -> MutexGuard<'static, ()> {
        PROCESS_GLOBALS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
