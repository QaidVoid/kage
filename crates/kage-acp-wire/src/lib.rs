//! Agent Client Protocol wire schema (protocol version 1).
//!
//! Serde-only and `wasm32-unknown-unknown` clean: no platform, I/O or
//! kage internals, so every client, native or wasm, shares the exact
//! wire types `kage rpc` speaks. Field names are `camelCase` and
//! update/content tags are `snake_case`; unknown update kinds and
//! content blocks decode to their `Unknown` variants instead of
//! failing the frame that carried them.
//!
//! The types live in [`schema`] and are re-exported at the crate root.

pub mod schema;

pub use schema::*;
