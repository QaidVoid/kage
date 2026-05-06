//! Tool trait, registry, and built-in tools.

pub mod atomic;
pub mod builtin;
pub mod error;
pub mod path;
pub mod registry;
pub mod schema;
pub mod tool;

pub use error::ToolError;
pub use path::resolve_under;
pub use registry::ToolRegistry;
pub use schema::schema_for;
pub use tool::{Tool, ToolContext};
