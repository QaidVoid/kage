//! The `mcp_resource` tool, which lets the model list and read the
//! resources of live MCP servers.
//!
//! [`crate::McpManager`] registers it while at least one live server
//! advertises resources and unregisters it otherwise. Listing answers
//! from the manager's cached catalog without a request. Reading goes
//! through `resources/read` and caps text like a mention does
//! ([`crate::expand::MAX_RESOURCE_TEXT`] per part and
//! [`crate::expand::MAX_PROMPT_TEXT`] per call). Errors read like a
//! mention's, and a URI with a template placeholder is refused.
//!
//! The tool is not named `<server>__<tool>`, so permission gates treat
//! it like a built-in read tool rather than an MCP tool.

use std::fmt::Write;
use std::sync::Arc;

use kage_core::protocol::{McpResource, McpResourceTemplate};
use kage_core::{Risk, ToolOutput, resource_block};
use kage_tools::error::ToolError;
use kage_tools::tool::{Tool, ToolContext};

use crate::catalog::ResourceContents;
use crate::expand::{ExpandError, MAX_PROMPT_TEXT, cap, decoded_len, find_placeholder, read_error};
use crate::server::McpConnection;

/// Name the model invokes.
pub const RESOURCE_TOOL: &str = "mcp_resource";

/// One live server that advertises resources, with its cached lists.
pub(crate) struct ResourceServer {
    pub(crate) name: String,
    pub(crate) conn: Arc<McpConnection>,
    pub(crate) resources: Vec<McpResource>,
    pub(crate) templates: Vec<McpResourceTemplate>,
}

/// Lists a server's cached resources and templates, or reads one
/// resource by URI.
pub struct McpResourceTool {
    servers: Vec<ResourceServer>,
    description: String,
}

impl std::fmt::Debug for McpResourceTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.servers.iter().map(|s| s.name.as_str()).collect();
        f.debug_struct("McpResourceTool")
            .field("servers", &names)
            .finish_non_exhaustive()
    }
}

impl McpResourceTool {
    /// Build the tool over `servers`. The description names them sorted,
    /// so it changes only when the set of servers does.
    pub(crate) fn new(mut servers: Vec<ResourceServer>) -> Self {
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        let description = format!(
            "List or read the resources of MCP servers. Servers with resources: {}. \
             Pass only `server` to list its resources and resource templates. \
             Pass `server` and `uri` to read one resource.",
            names.join(", ")
        );
        Self {
            servers,
            description,
        }
    }

    fn server(&self, name: &str) -> Option<&ResourceServer> {
        self.servers.iter().find(|s| s.name == name)
    }
}

impl Tool for McpResourceTool {
    fn name(&self) -> &str {
        RESOURCE_TOOL
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Name of the MCP server.",
                },
                "uri": {
                    "type": "string",
                    "description": "URI of the resource to read. Omit it to list the server's resources.",
                },
            },
            "required": ["server"],
            "additionalProperties": false,
        })
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execute(
        &self,
        input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let field = |key: &str| input.get(key).and_then(serde_json::Value::as_str);
        let name = field("server")
            .ok_or_else(|| ToolError::InvalidInput("`server` must be a string".to_owned()))?;
        let Some(server) = self.server(name) else {
            let known: Vec<&str> = self.servers.iter().map(|s| s.name.as_str()).collect();
            return Ok(output(
                true,
                format!(
                    "no MCP server `{name}` with resources. Known: {}",
                    known.join(", ")
                ),
            ));
        };
        let Some(uri) = field("uri") else {
            return Ok(output(false, listing(server)));
        };
        if let Some(placeholder) = find_placeholder(uri) {
            let refused = ExpandError::Placeholder {
                server: server.name.clone(),
                uri: uri.to_owned(),
                placeholder: placeholder.to_owned(),
            };
            return Ok(output(true, refused.to_string()));
        }
        match server.conn.read_resource(uri) {
            Ok(parts) if parts.is_empty() => Ok(output(false, format!("{uri} has no contents"))),
            Ok(parts) => Ok(output(false, contents(&server.name, parts))),
            Err(err) => {
                let failed = read_error(&server.name, uri, &err);
                Ok(output(true, failed.to_string()))
            }
        }
    }
}

fn output(is_error: bool, text: String) -> ToolOutput {
    ToolOutput {
        is_error,
        text,
        structured: None,
        terminate: false,
    }
}

/// The cached resources and templates of `server`, one per line.
fn listing(server: &ResourceServer) -> String {
    let name = &server.name;
    if server.resources.is_empty() && server.templates.is_empty() {
        return format!("`{name}` lists no resources.");
    }
    let mut out = String::new();
    if !server.resources.is_empty() {
        let _ = write!(out, "Resources of `{name}`:");
        for resource in &server.resources {
            let _ = write!(out, "\n- {} ({})", resource.uri, resource.name);
            if let Some(mime) = &resource.mime_type {
                let _ = write!(out, " [{mime}]");
            }
            if let Some(description) = &resource.description {
                let _ = write!(out, ": {description}");
            }
        }
    }
    if !server.templates.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let _ = write!(
            out,
            "Resource templates of `{name}` (fill in the braces to build a URI):"
        );
        for template in &server.templates {
            let _ = write!(out, "\n- {} ({})", template.uri_template, template.name);
            if let Some(description) = &template.description {
                let _ = write!(out, ": {description}");
            }
        }
    }
    out
}

/// The parts of a read resource: text in resource blocks, capped like a
/// mention, and blobs as one line each.
fn contents(server: &str, parts: Vec<ResourceContents>) -> String {
    let mut budget = MAX_PROMPT_TEXT;
    parts
        .into_iter()
        .map(|part| match part {
            ResourceContents::Text {
                uri,
                mime_type,
                text,
            } => resource_block::render(
                &uri,
                Some(server),
                mime_type.as_deref(),
                &cap(&text, &mut budget),
            ),
            ResourceContents::Blob {
                uri,
                mime_type,
                data,
            } => format!(
                "[binary resource {server}:{uri}: {}, {} bytes]",
                mime_type.as_deref().unwrap_or("application/octet-stream"),
                decoded_len(&data)
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use kage_core::CancelFlag;
    use kage_jsonrpc::RpcError;
    use serde_json::json;

    use super::*;
    use crate::test_support::{Seen, scripted};

    fn resource(uri: &str, name: &str) -> McpResource {
        McpResource {
            uri: uri.into(),
            name: name.into(),
            description: None,
            mime_type: None,
        }
    }

    /// A server `name` whose `resources/read` returns the URI as text,
    /// except `test://big` (70 KiB), `test://bin` (a blob) and
    /// `test://missing` (an error).
    fn server(name: &str, resources: Vec<McpResource>) -> (ResourceServer, Seen) {
        let (conn, _peer, seen) = scripted(name, json!({ "resources": {} }), |method, params| {
            let uri = params["uri"].as_str().unwrap_or_default();
            Ok(match (method, uri) {
                ("resources/read", "test://big") => json!({ "contents": [
                    { "uri": uri, "mimeType": "text/plain", "text": "x".repeat(70 * 1024) },
                ] }),
                ("resources/read", "test://bin") => json!({ "contents": [
                    { "uri": uri, "mimeType": "application/zip", "blob": "aGk=" },
                ] }),
                ("resources/read", "test://missing") => {
                    return Err(RpcError::new(-32002, "resource not found"));
                }
                ("resources/read", _) => json!({ "contents": [
                    { "uri": uri, "mimeType": "text/plain", "text": uri },
                ] }),
                (other, _) => return Err(RpcError::method_not_found(other)),
            })
        });
        let server = ResourceServer {
            name: name.into(),
            conn,
            resources,
            templates: Vec::new(),
        };
        (server, seen)
    }

    fn run(tool: &McpResourceTool, input: serde_json::Value) -> ToolOutput {
        let cancel = CancelFlag::default();
        let cx = ToolContext::new(std::path::Path::new("."), &cancel);
        tool.execute(input, &cx).unwrap()
    }

    #[test]
    fn listing_answers_from_the_cache() {
        let (mut srv, seen) = server("srv", Vec::new());
        srv.resources = vec![McpResource {
            description: Some("the readme".into()),
            mime_type: Some("text/markdown".into()),
            ..resource("test://readme", "README")
        }];
        srv.templates = vec![McpResourceTemplate {
            uri_template: "test://r/{id}".into(),
            name: "R".into(),
            description: None,
        }];
        let (empty, _) = server("empty", Vec::new());
        let tool = McpResourceTool::new(vec![srv, empty]);

        let out = run(&tool, json!({ "server": "srv" }));
        assert!(!out.is_error);
        assert_eq!(
            out.text,
            "Resources of `srv`:\n\
             - test://readme (README) [text/markdown]: the readme\n\n\
             Resource templates of `srv` (fill in the braces to build a URI):\n\
             - test://r/{id} (R)"
        );
        assert_eq!(
            run(&tool, json!({ "server": "empty" })).text,
            "`empty` lists no resources."
        );
        assert!(seen.lock().unwrap().is_empty(), "listing sent a request");
    }

    #[test]
    fn reading_returns_capped_text_and_blob_lines() {
        let (srv, seen) = server("srv", Vec::new());
        let tool = McpResourceTool::new(vec![srv]);

        let out = run(&tool, json!({ "server": "srv", "uri": "test://a" }));
        assert!(!out.is_error);
        assert_eq!(
            out.text,
            resource_block::render("test://a", Some("srv"), Some("text/plain"), "test://a")
        );

        let out = run(&tool, json!({ "server": "srv", "uri": "test://big" }));
        assert!(
            out.text
                .ends_with("\n[truncated: kept 65536 of 71680 bytes]\n</resource>"),
            "{}",
            &out.text[out.text.len() - 80..]
        );

        let out = run(&tool, json!({ "server": "srv", "uri": "test://bin" }));
        assert_eq!(
            out.text,
            "[binary resource srv:test://bin: application/zip, 2 bytes]"
        );

        let out = run(&tool, json!({ "server": "srv", "uri": "test://missing" }));
        assert!(out.is_error);
        assert_eq!(out.text, "mcp srv: read test://missing: resource not found");

        let out = run(&tool, json!({ "server": "srv", "uri": "test://r/{id}" }));
        assert!(out.is_error);
        assert_eq!(out.text, "mcp srv: test://r/{id}: fill in {id} first");
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[test]
    fn an_unknown_server_is_an_error_result() {
        let (srv, seen) = server("srv", Vec::new());
        let tool = McpResourceTool::new(vec![srv]);
        let out = run(&tool, json!({ "server": "nope", "uri": "test://a" }));
        assert!(out.is_error);
        assert_eq!(out.text, "no MCP server `nope` with resources. Known: srv");
        assert!(seen.lock().unwrap().is_empty());

        let cancel = CancelFlag::default();
        let cx = ToolContext::new(std::path::Path::new("."), &cancel);
        let err = tool.execute(json!({ "uri": "test://a" }), &cx).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err}");
    }

    #[test]
    fn the_description_names_the_sorted_servers_only() {
        let (b, _) = server("b", vec![resource("test://1", "one")]);
        let (a, _) = server("a", Vec::new());
        let first = McpResourceTool::new(vec![b, a]);
        assert!(
            first
                .description()
                .contains("Servers with resources: a, b."),
            "{}",
            first.description()
        );
        assert_eq!(first.risk(), Risk::Read);
        assert_eq!(first.name(), "mcp_resource");

        let (b, _) = server("b", vec![resource("test://2", "two")]);
        let (a, _) = server("a", vec![resource("test://3", "three")]);
        let same_set = McpResourceTool::new(vec![a, b]);
        assert_eq!(same_set.description(), first.description());

        let (a, _) = server("a", Vec::new());
        let fewer = McpResourceTool::new(vec![a]);
        assert_ne!(fewer.description(), first.description());
    }
}
