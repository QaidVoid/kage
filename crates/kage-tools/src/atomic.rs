//! Atomic filesystem writes used by `write` and `edit` tools.
//!
//! Re-exported from [`kage_core::fsutil`] so every crate shares one
//! implementation.

pub use kage_core::fsutil::{atomic_write, atomic_write_private};
