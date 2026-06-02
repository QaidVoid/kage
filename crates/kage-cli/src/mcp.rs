//! MCP wiring for the binary.
//!
//! - [`run_serve`] is the `kage mcp serve` server: builds the built-in
//!   registry and hands stdin/stdout to [`kage_mcp::serve`].
//! - [`spawn_and_register`] is the client side every run path calls:
//!   it spawns the configured `[mcp.servers.*]` (merged with any a
//!   plugin declared via `kage.mcp.add_server`) and registers their
//!   tools into the loop's [`ToolRegistry`], keeping the returned
//!   [`McpManager`] alive for the session.
//!
//! Diagnostics for `serve` go to stderr so they do not corrupt the
//! JSON-RPC stream on stdout.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kage_core::config::{Config, McpConfig};
use kage_mcp::{McpError, McpManager};
use kage_plugin::PluginRuntime;
use kage_tools::ToolRegistry;

/// Serve built-in tools as an MCP server until stdin closes.
pub(crate) fn run_serve() -> ExitCode {
    let registry = kage_tools::builtin_registry();
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match kage_mcp::serve(
        &registry,
        &workdir,
        BufReader::new(std::io::stdin()),
        std::io::stdout(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: mcp serve: {e}");
            ExitCode::from(1)
        }
    }
}

/// The MCP servers to spawn: `[mcp.servers.*]` from layered config,
/// then any a plugin declared via `kage.mcp.add_server` (a plugin
/// entry overrides a config entry of the same name, matching the
/// "plugins configure, core spawns" model used for ACP agents). A
/// malformed config degrades to just the plugin-declared set rather
/// than failing the run.
fn merged_config(workdir: &Path, runtime: Option<&PluginRuntime>) -> McpConfig {
    let mut merged = Config::load_layered(workdir)
        .map(|c| c.mcp)
        .unwrap_or_default();
    if let Some(rt) = runtime {
        for (name, server) in rt.registered_mcp_servers() {
            merged.servers.insert(name, server);
        }
    }
    merged
}

/// Spawn every enabled MCP server and register its tools into
/// `tools`. The caller must keep the returned [`McpManager`] alive
/// for the session: dropping it kills the child processes. Spawn and
/// discovery failures are returned as `(server, error)` for the
/// caller to surface (never swallowed).
pub(crate) fn spawn_and_register(
    tools: &mut ToolRegistry,
    workdir: &Path,
    runtime: Option<&PluginRuntime>,
) -> (McpManager, Vec<(String, McpError)>) {
    let cfg = merged_config(workdir, runtime);
    let (mut manager, mut errors) = McpManager::spawn_all(&cfg, vec![workdir.to_path_buf()]);
    errors.extend(manager.register_into(tools));
    (manager, errors)
}
