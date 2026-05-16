//! Agent Client Protocol stdio JSON-RPC server and client.
//!
//! The crate is built bottom-up: [`framing`] is the LSP-style
//! `Content-Length` transport, [`schema`] is the JSON-RPC 2.0 request
//! and notification shape, and later modules layer the server that
//! drives the agent loop on top of them.

pub mod acp;
pub mod agent;
pub mod client;
pub mod framing;
pub mod jsonrpc;
pub mod schema;
pub mod server;
