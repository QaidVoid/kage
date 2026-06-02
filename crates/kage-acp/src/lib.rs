//! Spec-conformant Agent Client Protocol over stdio.
//!
//! Layering: depends on `kage-core`, `kage-jsonrpc`, and `kage-provider`.
//!
//! Built bottom-up on the shared [`kage_jsonrpc`] bidirectional peer:
//! [`acp`] is the protocol's wire schema (protocol version 1);
//! [`agent`] serves kage as an ACP agent that editors drive; [`client`]
//! consumes another ACP agent as a [`kage_provider::Provider`] for
//! stacking.

pub mod acp;
pub mod agent;
pub mod client;
