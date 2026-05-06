//! Tool trait, registry, and built-in tools.

pub mod error;
pub mod registry;
pub mod schema;
pub mod tool;

pub use error::ToolError;
pub use registry::ToolRegistry;
pub use schema::schema_for;
pub use tool::{Tool, ToolContext};
