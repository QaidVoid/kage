//! Spec-conformant Agent Client Protocol over stdio.
//!
//! Built bottom-up: [`jsonrpc`] is the bidirectional newline-delimited
//! JSON-RPC 2.0 peer; [`acp`] is the protocol's wire schema (protocol
//! version 1); [`agent`] serves kage as an ACP agent that editors
//! drive; [`client`] consumes another ACP agent as a
//! [`kage_provider::Provider`] for stacking.

pub mod acp;
pub mod agent;
pub mod client;
pub mod jsonrpc;
