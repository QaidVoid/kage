//! Model Context Protocol client and server adapters.
//!
//! Layering: depends on `kage-core`, `kage-jsonrpc`, and `kage-tools`.

pub mod catalog;
pub mod expand;
mod http;
pub mod manager;
pub mod oauth;
pub mod serve;
pub mod server;
pub mod tools;

pub use catalog::{PromptMessage, ResourceContents};
pub use manager::McpManager;
pub use oauth::TokenSource;
pub use serve::{ServeGate, serve};
pub use server::{
    McpConnection, McpError, McpServerHandle, PROTOCOL_VERSION, ServerRequestHandler,
};
pub use tools::{McpTool, McpToolDef, tools_from_connection};
