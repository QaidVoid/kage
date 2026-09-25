//! What a client knows about the session's MCP servers.
//!
//! The engine publishes one [`McpServerInfo`] per configured server so a
//! client can complete resource mentions, list prompt commands and show
//! server status without touching the network.

use serde::{Deserialize, Serialize};

/// One configured MCP server and the catalog it advertises.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct McpServerInfo {
    /// Configured server name, the `<name>` of `[mcp.servers.<name>]`.
    pub name: String,
    /// Whether the server is usable.
    #[serde(flatten)]
    pub status: McpServerStatus,
    /// Number of tools the server contributes.
    pub tools: u32,
    /// Resources the server lists, capped per server.
    #[serde(default)]
    pub resources: Vec<McpResource>,
    /// Resource templates the server lists, capped per server.
    #[serde(default)]
    pub templates: Vec<McpResourceTemplate>,
    /// Prompts the server lists, capped per server.
    #[serde(default)]
    pub prompts: Vec<McpPrompt>,
}

/// Whether an MCP server is usable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpServerStatus {
    /// The server is live.
    Connected,
    /// The server failed to start or its transport died.
    Failed {
        /// The spawn, handshake or crash error.
        error: String,
    },
    /// The server needs an OAuth login before it can connect.
    NeedsAuth,
}

/// One entry of `resources/list`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct McpResource {
    /// Resource URI, as passed to `resources/read`.
    pub uri: String,
    /// Short name the server gave the resource.
    pub name: String,
    /// What the resource holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MIME type of the contents, when the server knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// One entry of `resources/templates/list`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct McpResourceTemplate {
    /// RFC 6570 URI template, such as `test://static/resource/{id}`.
    pub uri_template: String,
    /// Short name the server gave the template.
    pub name: String,
    /// What the template's resources hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One entry of `prompts/list`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct McpPrompt {
    /// Prompt name, as passed to `prompts/get`.
    pub name: String,
    /// What the prompt does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Declared arguments, in the order the server lists them.
    #[serde(default)]
    pub arguments: Vec<McpPromptArgument>,
}

/// One declared argument of an [`McpPrompt`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct McpPromptArgument {
    /// Argument name.
    pub name: String,
    /// What the argument means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether `prompts/get` fails without it.
    #[serde(default)]
    pub required: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(info: &McpServerInfo) -> serde_json::Value {
        let value = serde_json::to_value(info).unwrap();
        let back: McpServerInfo = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(&back, info);
        value
    }

    #[test]
    fn server_info_roundtrips_with_a_flat_status() {
        let info = McpServerInfo {
            name: "everything".into(),
            status: McpServerStatus::Connected,
            tools: 11,
            resources: vec![McpResource {
                uri: "test://static/resource/1".into(),
                name: "Resource 1".into(),
                description: None,
                mime_type: Some("text/plain".into()),
            }],
            templates: vec![McpResourceTemplate {
                uri_template: "test://static/resource/{id}".into(),
                name: "Static resource".into(),
                description: Some("by id".into()),
            }],
            prompts: vec![McpPrompt {
                name: "complex_prompt".into(),
                description: Some("A prompt with arguments".into()),
                arguments: vec![
                    McpPromptArgument {
                        name: "temperature".into(),
                        description: None,
                        required: true,
                    },
                    McpPromptArgument {
                        name: "style".into(),
                        description: None,
                        required: false,
                    },
                ],
            }],
        };
        let value = roundtrip(&info);
        assert_eq!(value["status"], "connected");
        assert_eq!(value["resources"][0]["mime_type"], "text/plain");
        assert!(value["resources"][0].get("description").is_none());
        assert_eq!(
            value["templates"][0]["uri_template"],
            "test://static/resource/{id}"
        );
        assert_eq!(value["prompts"][0]["arguments"][0]["required"], true);
    }

    #[test]
    fn every_status_roundtrips() {
        for (status, tag) in [
            (McpServerStatus::Connected, "connected"),
            (
                McpServerStatus::Failed {
                    error: "spawn `nope`".into(),
                },
                "failed",
            ),
            (McpServerStatus::NeedsAuth, "needs_auth"),
        ] {
            let info = McpServerInfo {
                name: "s".into(),
                status,
                tools: 0,
                resources: Vec::new(),
                templates: Vec::new(),
                prompts: Vec::new(),
            };
            let value = roundtrip(&info);
            assert_eq!(value["status"], tag);
        }
        let failed = serde_json::json!({"name": "b", "status": "failed", "error": "x", "tools": 0});
        let info: McpServerInfo = serde_json::from_value(failed).unwrap();
        assert_eq!(info.status, McpServerStatus::Failed { error: "x".into() });
    }
}
