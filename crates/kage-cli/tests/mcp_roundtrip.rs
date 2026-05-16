//! End-to-end MCP: drive the real `kage mcp serve` subprocess
//! through the client (spawn -> initialize -> tools/list -> call).

use std::collections::BTreeMap;

use kage_core::CancelFlag;
use kage_core::config::McpServer;
use kage_mcp::{McpServerHandle, tools_from_connection};
use kage_tools::tool::ToolContext;

#[test]
fn spawn_kage_mcp_serve_and_round_trip_a_builtin_tool() {
    let cfg = McpServer {
        command: env!("CARGO_BIN_EXE_kage").to_owned(),
        args: vec!["mcp".to_owned(), "serve".to_owned()],
        env: BTreeMap::new(),
        disabled: false,
    };

    let handle = McpServerHandle::spawn("kage", &cfg).expect("spawn kage mcp serve");
    let conn = handle.connection();

    let defs = conn.list_tools().expect("tools/list");
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"ls"), "builtin `ls` exposed: {names:?}");
    assert!(names.contains(&"read"), "builtin `read` exposed: {names:?}");

    let tools = tools_from_connection(conn).expect("adapt tools");
    let ls = tools
        .iter()
        .find(|t| t.name() == "kage__ls")
        .expect("namespaced ls adapter");

    let cancel = CancelFlag::new();
    let cx = ToolContext::new(std::path::Path::new("."), &cancel);
    let out = ls
        .execute(serde_json::json!({}), &cx)
        .expect("ls executes over MCP");
    assert!(!out.is_error, "ls reported success: {}", out.text);
    assert!(out.structured.is_some(), "raw MCP result is preserved");
}
