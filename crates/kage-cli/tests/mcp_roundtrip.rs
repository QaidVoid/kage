//! End-to-end MCP: drive the real `kage mcp serve` subprocess
//! through the client (spawn -> initialize -> tools/list -> call).

use std::collections::BTreeMap;

use kage_core::CancelFlag;
use kage_core::config::{McpConfig, McpServer};
use kage_mcp::{McpManager, McpServerHandle, tools_from_connection};
use kage_tools::ToolRegistry;
use kage_tools::tool::ToolContext;

/// `[mcp.servers.kage]` pointed at this test binary's `kage mcp serve`.
fn kage_serve_config() -> McpServer {
    McpServer {
        command: Some(env!("CARGO_BIN_EXE_kage").to_owned()),
        args: vec!["mcp".to_owned(), "serve".to_owned()],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        disabled: false,
    }
}

#[test]
fn spawn_kage_mcp_serve_and_round_trip_a_builtin_tool() {
    let cfg = kage_serve_config();

    let handle = McpServerHandle::spawn("kage", &cfg, &[]).expect("spawn kage mcp serve");
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

#[test]
fn manager_restart_respawns_and_keeps_tools_registered() {
    let mut cfg = McpConfig::default();
    cfg.servers.insert("kage".to_owned(), kage_serve_config());

    let (mut manager, errors) = McpManager::spawn_all(&cfg, vec![]);
    assert!(errors.is_empty(), "spawn errors: {errors:?}");
    assert_eq!(manager.len(), 1);

    let mut reg = ToolRegistry::new();
    let reg_errors = manager.register_into(&mut reg);
    assert!(reg_errors.is_empty(), "register errors: {reg_errors:?}");
    assert!(
        reg.get("kage__ls").is_some(),
        "ls registered before restart"
    );
    let before = reg.len();

    manager
        .restart("kage", &mut reg)
        .expect("restart spawns a fresh server");

    assert!(
        reg.get("kage__ls").is_some(),
        "ls re-registered after restart"
    );
    assert_eq!(reg.len(), before, "tool set is identical after restart");
}
