//! Agent Client Protocol stdio JSON-RPC server and client.
//!
//! The crate is built bottom-up: [`framing`] is the LSP-style
//! `Content-Length` transport; later modules layer the request schema
//! and the server that drives the agent loop on top of it.

pub mod framing;
