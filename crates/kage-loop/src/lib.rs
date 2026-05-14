//! Pure event-driven agent loop.
//!
//! The loop drives a [`kage_core`]-based conversation forward by talking to
//! a provider and a tool registry. It is fully synchronous: no tokio, no
//! async traits, no `Pin<Box<dyn Stream>>`. Hosts subscribe to streaming
//! [`LoopEvent`](kage_core::LoopEvent)s through a single emit callback.

pub mod compact;
pub mod config;
pub mod context;
mod dispatch;
mod doom;
pub mod hooks;
pub mod run;
mod stream;
pub mod system_prompt;

pub use compact::{COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, force_compact};
pub use config::{LoopConfig, SteeringMode};
pub use context::{AgentContext, TokenBudget};
pub use hooks::{HookResult, Hooks, NoopHooks, TurnSummary};
pub use kage_provider::{StreamRequest, ThinkingLevel};
pub use run::run;
pub use system_prompt::{DEFAULT_ROLE, EnvContext, compose as compose_system_prompt, with_skills};
