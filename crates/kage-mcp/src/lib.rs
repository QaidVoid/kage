//! Model Context Protocol client and server adapters.

pub mod server;
pub mod tools;
pub mod transport;

pub use server::{McpConnection, McpError, McpServerHandle, PROTOCOL_VERSION};
pub use tools::{McpTool, McpToolDef, tools_from_connection};
pub use transport::{Inbound, Peer, RpcError, connect};
