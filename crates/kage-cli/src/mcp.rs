//! `kage mcp serve`: run kage as an MCP server over stdio.
//!
//! Builds the built-in tool registry and hands stdin/stdout to
//! [`kage_mcp::serve`]. Diagnostics go to stderr so they do not
//! corrupt the JSON-RPC stream on stdout.

use std::io::BufReader;
use std::path::PathBuf;
use std::process::ExitCode;

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
