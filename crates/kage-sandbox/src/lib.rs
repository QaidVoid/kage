//! Sandbox abstraction for executing untrusted commands.
//!
//! Layering: leaf crate; it has no `kage-*` Cargo dependency yet (the
//! `kage_core` reference below is documentation, not a build dependency).
//!
//! Deferred placeholder (post-0.1): this crate holds the layer slot for
//! OS-level command isolation (for example bubblewrap on Linux or
//! `sandbox-exec` on macOS) selected by
//! [`kage_core::config::SandboxBackend`]. It ships empty in 0.1 so the
//! workspace layering and the `SandboxConfig` surface stay stable while
//! the backends land later. No tool consults a sandbox yet: the default
//! backend is `Local` (no isolation) and the host prints the
//! "running unsandboxed" warning.
