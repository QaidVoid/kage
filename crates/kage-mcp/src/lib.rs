//! Model Context Protocol client and server adapters.

pub mod server;
pub mod transport;

pub use server::{McpConnection, McpError, McpServerHandle, PROTOCOL_VERSION};
pub use transport::{Inbound, Peer, RpcError, connect};
