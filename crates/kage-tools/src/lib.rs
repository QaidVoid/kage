//! Tool trait, registry, and built-in tools.

pub mod error;
pub mod registry;
pub mod tool;

pub use error::ToolError;
pub use registry::ToolRegistry;
pub use tool::{Tool, ToolContext};
