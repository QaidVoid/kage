//! Tool trait, registry, and built-in tools.

pub mod atomic;
pub mod builtin;
pub mod error;
pub mod path;
pub mod registry;
pub mod schema;
pub mod ssrf;
pub mod tool;

pub use builtin::builtin_registry;
pub use error::ToolError;
pub use path::{resolve, resolve_under};
pub use registry::ToolRegistry;
pub use schema::schema_for;
pub use tool::{Tool, ToolContext};
