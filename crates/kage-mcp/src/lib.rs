//! Model Context Protocol client and server adapters.

pub mod transport;

pub use transport::{Inbound, Peer, RpcError, connect};
