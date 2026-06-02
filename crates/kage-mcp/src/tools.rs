//! Discover an MCP server's tools and expose them as kage tools.
//!
//! [`McpConnection::list_tools`] walks the (possibly paginated)
//! `tools/list` result; [`tools_from_connection`] wraps each entry in
//! an [`McpTool`] that implements [`kage_tools::Tool`], so MCP tools
//! drop into the same `ToolRegistry` the model already invokes.
//!
//! Names are namespaced `<server>__<tool>` (the de-facto MCP
//! convention) so a server cannot shadow a built-in tool, and MCP
//! tools are classified [`Risk::Exec`] unconditionally: a server is
//! opaque, so the host must gate every call rather than guess it is
//! read-only.

use std::sync::Arc;

use kage_core::{Risk, ToolOutput};
use kage_tools::error::ToolError;
use kage_tools::tool::{Tool, ToolContext};

use crate::server::{McpConnection, McpError};

/// One tool advertised by `tools/list`.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDef {
    /// Server-side tool name (what `tools/call` expects).
    pub name: String,
    /// Model-readable description (empty when the server omits it).
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub input_schema: serde_json::Value,
}

impl McpConnection {
    /// List the server's tools, following `nextCursor` pagination to
    /// the end so the full set is returned in one call.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Rpc`] on a JSON-RPC error or dropped
    /// connection, and [`McpError::Protocol`] when the result is not
    /// the expected `{ tools: [...] }` shape.
    pub fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.take().map_or_else(
                || serde_json::json!({}),
                |c| serde_json::json!({ "cursor": c }),
            );
            let result = self.request("tools/list", params)?;
            let tools = result
                .get("tools")
                .and_then(|t| t.as_array())
                .ok_or_else(|| McpError::Protocol {
                    server: self.name().to_owned(),
                    detail: "tools/list result missing `tools` array".to_owned(),
                })?;
            for tool in tools {
                let Some(name) = tool.get("name").and_then(|n| n.as_str()) else {
                    return Err(McpError::Protocol {
                        server: self.name().to_owned(),
                        detail: "a tool entry had no `name`".to_owned(),
                    });
                };
                out.push(McpToolDef {
                    name: name.to_owned(),
                    description: tool
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_owned(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
                });
            }
            match result.get("nextCursor").and_then(|c| c.as_str()) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
                _ => break,
            }
        }
        Ok(out)
    }
}

/// A single MCP tool, adapted to the kage [`Tool`] trait.
pub struct McpTool {
    conn: Arc<McpConnection>,
    /// Server-side name passed back in `tools/call`.
    original_name: String,
    /// Namespaced name the model invokes (`<server>__<tool>`).
    exposed_name: String,
    description: String,
    schema: serde_json::Value,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("server", &self.conn.name())
            .field("name", &self.exposed_name)
            .finish_non_exhaustive()
    }
}

impl McpTool {
    /// Build an adapter for one discovered tool on `conn`.
    #[must_use]
    pub fn new(conn: Arc<McpConnection>, def: McpToolDef) -> Self {
        let exposed_name = format!("{}__{}", conn.name(), def.name);
        Self {
            conn,
            original_name: def.name,
            exposed_name,
            description: def.description,
            schema: def.input_schema,
        }
    }

    fn render_result(value: &serde_json::Value) -> ToolOutput {
        let is_error = value
            .get("isError")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let mut parts: Vec<String> = Vec::new();
        if let Some(items) = value.get("content").and_then(|c| c.as_array()) {
            for item in items {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                            parts.push(t.to_owned());
                        }
                    }
                    Some(other) => {
                        parts.push(format!("[mcp {other} content omitted]"));
                    }
                    None => parts.push("[mcp content of unknown type]".to_owned()),
                }
            }
        }
        ToolOutput {
            is_error,
            text: parts.join("\n"),
            structured: Some(value.clone()),
            terminate: false,
        }
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.exposed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn risk(&self) -> Risk {
        Risk::Exec
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let arguments = if input.is_null() {
            serde_json::json!({})
        } else {
            input
        };
        let params = serde_json::json!({
            "name": self.original_name,
            "arguments": arguments,
        });
        match self
            .conn
            .request_cancellable("tools/call", params, &|| cx.is_cancelled())
        {
            Ok(result) => Ok(Self::render_result(&result)),
            Err(McpError::Rpc { source, .. }) if source.code == -32800 => Err(ToolError::Cancelled),
            Err(e) => Ok(ToolOutput {
                is_error: true,
                text: e.to_string(),
                structured: None,
                terminate: false,
            }),
        }
    }
}

/// Discover every tool on `conn` and wrap each as a `dyn Tool` ready
/// to register. The connection is shared into each adapter so calls
/// outlive discovery without re-spawning the server.
///
/// # Errors
///
/// Propagates [`McpConnection::list_tools`] failures.
pub fn tools_from_connection(conn: &Arc<McpConnection>) -> Result<Vec<Arc<dyn Tool>>, McpError> {
    let defs = conn.list_tools()?;
    Ok(defs
        .into_iter()
        .map(|def| Arc::new(McpTool::new(Arc::clone(conn), def)) as Arc<dyn Tool>)
        .collect())
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::thread;

    use kage_core::CancelFlag;

    use super::*;
    use crate::server::PROTOCOL_VERSION;
    use kage_jsonrpc::{Inbound, Peer, connect};

    /// A scripted MCP server: answers `initialize`, serves a two-page
    /// `tools/list`, and echoes `tools/call` arguments back as text.
    fn scripted() -> (Arc<McpConnection>, thread::JoinHandle<()>) {
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_peer, cli_in, _c) = connect(BufReader::new(cli_r), cli_w);
        let (srv_peer, srv_in, _s) = connect(BufReader::new(srv_r), srv_w);
        let responder: Peer = srv_peer.clone();
        let handle = thread::spawn(move || {
            for msg in srv_in {
                let Inbound::Request { id, method, params } = msg else {
                    continue;
                };
                let outcome = match method.as_str() {
                    "initialize" => Ok(serde_json::json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "fs", "version": "0" },
                    })),
                    "tools/list" => {
                        if params.get("cursor").and_then(|c| c.as_str()) == Some("p2") {
                            Ok(serde_json::json!({
                                "tools": [{
                                    "name": "write_file",
                                    "description": "write a file",
                                    "inputSchema": { "type": "object" },
                                }]
                            }))
                        } else {
                            Ok(serde_json::json!({
                                "tools": [{
                                    "name": "read_file",
                                    "description": "read a file",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": { "path": { "type": "string" } },
                                    },
                                }],
                                "nextCursor": "p2",
                            }))
                        }
                    }
                    "tools/call" => {
                        let args = params.get("arguments").cloned().unwrap_or_default();
                        Ok(serde_json::json!({
                            "content": [{ "type": "text", "text": args.to_string() }],
                            "isError": false,
                        }))
                    }
                    other => Err(kage_jsonrpc::RpcError::method_not_found(other)),
                };
                let _ = responder.respond(&id, outcome);
            }
        });
        let conn = Arc::new(McpConnection::initialize("fs", cli_peer, cli_in, &[], None).unwrap());
        (conn, handle)
    }

    #[test]
    fn list_tools_follows_pagination() {
        let (conn, _h) = scripted();
        let defs = conn.list_tools().unwrap();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["read_file", "write_file"]);
    }

    #[test]
    fn adapter_namespaces_and_passes_schema() {
        let (conn, _h) = scripted();
        let tools = tools_from_connection(&conn).unwrap();
        let read = tools.iter().find(|t| t.name() == "fs__read_file").unwrap();
        assert_eq!(read.description(), "read a file");
        assert_eq!(read.schema()["properties"]["path"]["type"], "string");
        assert_eq!(read.risk(), Risk::Exec);
    }

    #[test]
    fn execute_round_trips_arguments_as_text() {
        let (conn, _h) = scripted();
        let tools = tools_from_connection(&conn).unwrap();
        let read = tools.iter().find(|t| t.name() == "fs__read_file").unwrap();
        let cancel = CancelFlag::default();
        let cx = ToolContext::new(std::path::Path::new("."), &cancel);
        let out = read
            .execute(serde_json::json!({ "path": "/tmp/x" }), &cx)
            .unwrap();
        assert!(!out.is_error);
        assert!(out.text.contains("/tmp/x"), "echoed args: {}", out.text);
        assert!(out.structured.is_some());
    }

    #[test]
    fn cancelled_call_reports_cancelled() {
        let (conn, _h) = scripted();
        let tool = McpTool::new(
            Arc::clone(&conn),
            McpToolDef {
                name: "slow".to_owned(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
        );
        let cancel = CancelFlag::default();
        cancel.cancel();
        let cx = ToolContext::new(std::path::Path::new("."), &cancel);
        let err = tool.execute(serde_json::Value::Null, &cx).unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }
}
