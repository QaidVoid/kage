//! Model Context Protocol client and server adapters.

mod http;
pub mod manager;
pub mod serve;
pub mod server;
pub mod tools;

pub use manager::McpManager;
pub use serve::serve;
pub use server::{
    McpConnection, McpError, McpServerHandle, PROTOCOL_VERSION, ServerRequestHandler,
};
pub use tools::{McpTool, McpToolDef, tools_from_connection};
