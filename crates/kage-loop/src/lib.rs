//! Pure event-driven agent loop.
//!
//! The loop drives a [`kage_core`]-based conversation forward by talking to
//! a provider and a tool registry. It is fully synchronous: no tokio, no
//! async traits, no `Pin<Box<dyn Stream>>`. Hosts subscribe to streaming
//! [`LoopEvent`](kage_core::LoopEvent)s through a single emit callback.

pub mod config;
pub mod context;
pub mod hooks;

pub use config::LoopConfig;
pub use context::{AgentContext, TokenBudget};
pub use hooks::{Hooks, NoopHooks};
