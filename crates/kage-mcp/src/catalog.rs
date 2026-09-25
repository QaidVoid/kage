//! List and read an MCP server's resources, resource templates and
//! prompts.
//!
//! The list calls follow `nextCursor` pagination and stop at a per-server
//! cap (500 resources, 100 templates, 200 prompts), so one server cannot
//! grow the catalog without bound. A list call on a server that did not
//! advertise the matching capability returns an empty list without
//! sending a request.

use kage_core::Role;
use kage_core::protocol::{McpPrompt, McpPromptArgument, McpResource, McpResourceTemplate};
use serde_json::Value;

use crate::server::{McpConnection, McpError};

/// Upper bound on the pages followed by one list call, as for
/// `tools/list`.
const MAX_PAGES: usize = 100;

/// Resources kept per server.
const MAX_RESOURCES: usize = 500;

/// Resource templates kept per server.
const MAX_RESOURCE_TEMPLATES: usize = 100;

/// Prompts kept per server.
const MAX_PROMPTS: usize = 200;

/// One entry of a `resources/read` result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceContents {
    /// Text contents.
    Text {
        /// URI of this part of the resource.
        uri: String,
        /// MIME type, when the server sent one.
        mime_type: Option<String>,
        /// The text.
        text: String,
    },
    /// Binary contents.
    Blob {
        /// URI of this part of the resource.
        uri: String,
        /// MIME type, when the server sent one.
        mime_type: Option<String>,
        /// The bytes, base64 encoded as the server sent them.
        data: String,
    },
}

/// One message of a `prompts/get` result.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptMessage {
    /// [`Role::User`] or [`Role::Assistant`].
    pub role: Role,
    /// The MCP content block (`text`, `image`, `audio`, `resource` or
    /// `resource_link`), as the server sent it.
    pub content: Value,
}

impl McpConnection {
    /// List the server's resources, up to 500.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Rpc`] on a JSON-RPC error or dropped
    /// connection, and [`McpError::Protocol`] for a malformed result or
    /// runaway pagination.
    pub fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        if !self.has("resources") {
            return Ok(Vec::new());
        }
        let method = "resources/list";
        self.paginate(method, "resources", MAX_RESOURCES)?
            .iter()
            .map(|entry| {
                Ok(McpResource {
                    uri: self.required(method, entry, "uri")?,
                    name: self.required(method, entry, "name")?,
                    description: optional(entry, "description"),
                    mime_type: optional(entry, "mimeType"),
                })
            })
            .collect()
    }

    /// List the server's resource templates, up to 100.
    ///
    /// # Errors
    ///
    /// As [`Self::list_resources`].
    pub fn list_resource_templates(&self) -> Result<Vec<McpResourceTemplate>, McpError> {
        if !self.has("resources") {
            return Ok(Vec::new());
        }
        let method = "resources/templates/list";
        self.paginate(method, "resourceTemplates", MAX_RESOURCE_TEMPLATES)?
            .iter()
            .map(|entry| {
                Ok(McpResourceTemplate {
                    uri_template: self.required(method, entry, "uriTemplate")?,
                    name: self.required(method, entry, "name")?,
                    description: optional(entry, "description"),
                })
            })
            .collect()
    }

    /// List the server's prompts, up to 200.
    ///
    /// # Errors
    ///
    /// As [`Self::list_resources`].
    pub fn list_prompts(&self) -> Result<Vec<McpPrompt>, McpError> {
        if !self.has("prompts") {
            return Ok(Vec::new());
        }
        let method = "prompts/list";
        self.paginate(method, "prompts", MAX_PROMPTS)?
            .iter()
            .map(|entry| {
                let arguments = entry
                    .get("arguments")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .map(|arg| {
                        Ok(McpPromptArgument {
                            name: self.required(method, arg, "name")?,
                            description: optional(arg, "description"),
                            required: arg
                                .get("required")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        })
                    })
                    .collect::<Result<_, McpError>>()?;
                Ok(McpPrompt {
                    name: self.required(method, entry, "name")?,
                    description: optional(entry, "description"),
                    arguments,
                })
            })
            .collect()
    }

    /// Read one resource with `resources/read`. A resource may come back
    /// in several parts.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Rpc`] on a JSON-RPC error (such as an unknown
    /// URI) or dropped connection, and [`McpError::Protocol`] when the
    /// result has no `contents` array or a part has neither `text` nor
    /// `blob`.
    pub fn read_resource(&self, uri: &str) -> Result<Vec<ResourceContents>, McpError> {
        let method = "resources/read";
        let result = self.request(method, serde_json::json!({ "uri": uri }))?;
        self.array(method, &result, "contents")?
            .iter()
            .map(|part| {
                let part_uri = optional(part, "uri").unwrap_or_else(|| uri.to_owned());
                let mime_type = optional(part, "mimeType");
                if let Some(text) = optional(part, "text") {
                    Ok(ResourceContents::Text {
                        uri: part_uri,
                        mime_type,
                        text,
                    })
                } else if let Some(data) = optional(part, "blob") {
                    Ok(ResourceContents::Blob {
                        uri: part_uri,
                        mime_type,
                        data,
                    })
                } else {
                    Err(self.protocol(format!("{method} part had neither `text` nor `blob`")))
                }
            })
            .collect()
    }

    /// Fetch prompt `name` with `prompts/get`, passing `arguments`.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Rpc`] on a JSON-RPC error (such as a missing
    /// argument) or dropped connection, and [`McpError::Protocol`] when
    /// the result has no `messages` array or a message has an unknown
    /// role.
    pub fn get_prompt(
        &self,
        name: &str,
        arguments: serde_json::Map<String, Value>,
    ) -> Result<Vec<PromptMessage>, McpError> {
        let method = "prompts/get";
        let mut params = serde_json::json!({ "name": name });
        params["arguments"] = Value::Object(arguments);
        let result = self.request(method, params)?;
        self.array(method, &result, "messages")?
            .iter()
            .map(|message| {
                let role = match message.get("role").and_then(Value::as_str) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    other => {
                        return Err(self.protocol(format!(
                            "{method} message had role {}",
                            other.unwrap_or("(none)")
                        )));
                    }
                };
                Ok(PromptMessage {
                    role,
                    content: message.get("content").cloned().unwrap_or(Value::Null),
                })
            })
            .collect()
    }

    /// Collect the `key` array of `method` across pages, stopping at
    /// `cap` entries or after [`MAX_PAGES`] pages.
    fn paginate(&self, method: &str, key: &str, cap: usize) -> Result<Vec<Value>, McpError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = cursor.take().map_or_else(
                || serde_json::json!({}),
                |c| serde_json::json!({ "cursor": c }),
            );
            let result = self.request(method, params)?;
            out.extend(self.array(method, &result, key)?.iter().cloned());
            if out.len() >= cap {
                out.truncate(cap);
                return Ok(out);
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
                _ => return Ok(out),
            }
        }
        Err(self.protocol(format!("{method} pagination exceeded {MAX_PAGES} pages")))
    }

    fn array<'a>(
        &self,
        method: &str,
        result: &'a Value,
        key: &str,
    ) -> Result<&'a Vec<Value>, McpError> {
        result
            .get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| self.protocol(format!("{method} result missing `{key}` array")))
    }

    fn required(&self, method: &str, entry: &Value, key: &str) -> Result<String, McpError> {
        optional(entry, key)
            .ok_or_else(|| self.protocol(format!("a {method} entry had no `{key}`")))
    }

    fn protocol(&self, detail: String) -> McpError {
        McpError::Protocol {
            server: self.name().to_owned(),
            detail,
        }
    }
}

fn optional(entry: &Value, key: &str) -> Option<String> {
    entry.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::BufReader;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use kage_jsonrpc::{Inbound, Peer, RpcError, connect};
    use serde_json::json;

    use super::*;
    use crate::server::PROTOCOL_VERSION;

    /// Requests a scripted server received, as `(method, params)`.
    pub(crate) type Seen = Arc<Mutex<Vec<(String, Value)>>>;

    /// An in-process server named `name` that advertises `capabilities`
    /// and answers every other request with `answer`. Returns the
    /// client connection, the server's peer (for notifications) and the
    /// requests it saw after `initialize`.
    pub(crate) fn scripted(
        name: &str,
        capabilities: Value,
        answer: impl Fn(&str, &Value) -> Result<Value, RpcError> + Send + 'static,
    ) -> (Arc<McpConnection>, Peer, Seen) {
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_peer, cli_in, _c) = connect(BufReader::new(cli_r), cli_w);
        let (srv_peer, srv_in, _s) = connect(BufReader::new(srv_r), srv_w);
        let responder = srv_peer.clone();
        let seen = Seen::default();
        let log = Arc::clone(&seen);
        thread::spawn(move || {
            for msg in srv_in {
                let Inbound::Request { id, method, params } = msg else {
                    continue;
                };
                let outcome = if method == "initialize" {
                    Ok(json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": capabilities,
                    }))
                } else {
                    log.lock().unwrap().push((method.clone(), params.clone()));
                    answer(&method, &params)
                };
                let _ = responder.respond(&id, outcome);
            }
        });
        let conn = McpConnection::initialize(name, cli_peer, cli_in, &[], None).unwrap();
        (Arc::new(conn), srv_peer, seen)
    }

    /// Answer a list call with page `cursor` (absent means 0) holding
    /// `per_page` entries built by `entry`, and a `nextCursor` until
    /// page `pages - 1`.
    fn paged(
        params: &Value,
        key: &str,
        pages: usize,
        per_page: usize,
        entry: impl Fn(usize) -> Value,
    ) -> Value {
        let page: usize = params
            .get("cursor")
            .and_then(Value::as_str)
            .map_or(0, |c| c.parse().unwrap());
        let items: Vec<Value> = (0..per_page).map(|i| entry(page * per_page + i)).collect();
        let mut result = json!({ key: items });
        if page + 1 < pages {
            result["nextCursor"] = json!((page + 1).to_string());
        }
        result
    }

    fn methods(seen: &Seen) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|(m, _)| m.clone())
            .collect()
    }

    fn everything(method: &str, params: &Value) -> Result<Value, RpcError> {
        Ok(match method {
            "resources/list" => paged(
                params,
                "resources",
                2,
                2,
                |i| json!({ "uri": format!("test://r/{i}"), "name": format!("R{i}"), "mimeType": "text/plain" }),
            ),
            "resources/templates/list" => paged(
                params,
                "resourceTemplates",
                2,
                1,
                |i| json!({ "uriTemplate": format!("test://t/{i}/{{id}}"), "name": format!("T{i}") }),
            ),
            "prompts/list" => paged(params, "prompts", 2, 1, |i| {
                json!({
                    "name": format!("p{i}"),
                    "description": "a prompt",
                    "arguments": [
                        { "name": "a", "required": true },
                        { "name": "b", "description": "optional" },
                    ],
                })
            }),
            other => return Err(RpcError::method_not_found(other)),
        })
    }

    #[test]
    fn lists_follow_pagination() {
        let caps = json!({ "resources": {}, "prompts": {} });
        let (conn, _srv, seen) = scripted("everything", caps, everything);

        let resources = conn.list_resources().unwrap();
        assert_eq!(resources.len(), 4);
        assert_eq!(
            resources[3],
            McpResource {
                uri: "test://r/3".into(),
                name: "R3".into(),
                description: None,
                mime_type: Some("text/plain".into()),
            }
        );

        let templates = conn.list_resource_templates().unwrap();
        let uris: Vec<_> = templates.iter().map(|t| t.uri_template.as_str()).collect();
        assert_eq!(uris, ["test://t/0/{id}", "test://t/1/{id}"]);

        let prompts = conn.list_prompts().unwrap();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[1].name, "p1");
        assert_eq!(prompts[1].description.as_deref(), Some("a prompt"));
        assert_eq!(
            prompts[1].arguments,
            [
                McpPromptArgument {
                    name: "a".into(),
                    description: None,
                    required: true,
                },
                McpPromptArgument {
                    name: "b".into(),
                    description: Some("optional".into()),
                    required: false,
                },
            ]
        );
        assert_eq!(
            methods(&seen),
            [
                "resources/list",
                "resources/list",
                "resources/templates/list",
                "resources/templates/list",
                "prompts/list",
                "prompts/list",
            ]
        );
    }

    #[test]
    fn caps_stop_a_server_that_lists_more() {
        let caps = json!({ "resources": {}, "prompts": {} });
        let (conn, _srv, seen) = scripted("big", caps, |method, params| {
            let entry = |i: usize| json!({ "uri": format!("u{i}"), "uriTemplate": format!("u{i}"), "name": format!("n{i}") });
            Ok(match method {
                "resources/list" => paged(params, "resources", 50, 300, entry),
                "resources/templates/list" => paged(params, "resourceTemplates", 50, 60, entry),
                "prompts/list" => paged(params, "prompts", 50, 150, entry),
                other => return Err(RpcError::method_not_found(other)),
            })
        });
        let resources = conn.list_resources().unwrap();
        assert_eq!(resources.len(), MAX_RESOURCES);
        assert_eq!(resources.last().unwrap().uri, "u499");
        assert_eq!(
            conn.list_resource_templates().unwrap().len(),
            MAX_RESOURCE_TEMPLATES
        );
        assert_eq!(conn.list_prompts().unwrap().len(), MAX_PROMPTS);
        assert_eq!(seen.lock().unwrap().len(), 6, "two pages each, then stop");
    }

    #[test]
    fn no_list_call_without_the_capability() {
        let (conn, _srv, seen) = scripted("tools_only", json!({ "tools": {} }), everything);
        assert!(conn.list_resources().unwrap().is_empty());
        assert!(conn.list_resource_templates().unwrap().is_empty());
        assert!(conn.list_prompts().unwrap().is_empty());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_malformed_list_is_a_protocol_error() {
        let (conn, _srv, _seen) = scripted("bad", json!({ "resources": {} }), |method, _| {
            Ok(match method {
                "resources/list" => json!({ "resources": [{ "name": "no uri" }] }),
                _ => json!({}),
            })
        });
        let err = conn.list_resources().unwrap_err();
        assert!(err.to_string().contains("`uri`"), "{err}");
        let err = conn.list_resource_templates().unwrap_err();
        assert!(err.to_string().contains("resourceTemplates"), "{err}");
    }

    #[test]
    fn read_resource_returns_text_and_blob_parts() {
        let (conn, _srv, seen) = scripted("r", json!({ "resources": {} }), |method, params| {
            assert_eq!(method, "resources/read");
            if params["uri"] == "test://missing" {
                return Err(RpcError::new(-32002, "resource not found"));
            }
            Ok(json!({ "contents": [
                { "uri": params["uri"], "mimeType": "text/plain", "text": "hello" },
                { "uri": "test://img", "mimeType": "image/png", "blob": "aGk=" },
                { "text": "no uri" },
            ] }))
        });
        let parts = conn.read_resource("test://doc").unwrap();
        assert_eq!(
            parts,
            [
                ResourceContents::Text {
                    uri: "test://doc".into(),
                    mime_type: Some("text/plain".into()),
                    text: "hello".into(),
                },
                ResourceContents::Blob {
                    uri: "test://img".into(),
                    mime_type: Some("image/png".into()),
                    data: "aGk=".into(),
                },
                ResourceContents::Text {
                    uri: "test://doc".into(),
                    mime_type: None,
                    text: "no uri".into(),
                },
            ]
        );
        assert_eq!(seen.lock().unwrap()[0].1, json!({ "uri": "test://doc" }));
        let err = conn.read_resource("test://missing").unwrap_err();
        assert!(err.to_string().contains("resource not found"), "{err}");
    }

    #[test]
    fn get_prompt_passes_arguments_and_returns_messages() {
        let (conn, _srv, seen) = scripted("p", json!({ "prompts": {} }), |method, params| {
            assert_eq!(method, "prompts/get");
            Ok(json!({
                "description": "d",
                "messages": [
                    { "role": "user", "content": { "type": "text", "text": format!("t={}", params["arguments"]["temperature"].as_str().unwrap()) } },
                    { "role": "assistant", "content": { "type": "text", "text": "ok" } },
                ],
            }))
        });
        let mut arguments = serde_json::Map::new();
        arguments.insert("temperature".into(), json!("0.7"));
        let messages = conn.get_prompt("complex_prompt", arguments).unwrap();
        assert_eq!(
            messages,
            [
                PromptMessage {
                    role: Role::User,
                    content: json!({ "type": "text", "text": "t=0.7" }),
                },
                PromptMessage {
                    role: Role::Assistant,
                    content: json!({ "type": "text", "text": "ok" }),
                },
            ]
        );
        assert_eq!(
            seen.lock().unwrap()[0].1,
            json!({ "name": "complex_prompt", "arguments": { "temperature": "0.7" } })
        );
    }

    #[test]
    fn get_prompt_rejects_an_unknown_role() {
        let (conn, _srv, _seen) = scripted("p", json!({ "prompts": {} }), |_, _| {
            Ok(json!({ "messages": [{ "role": "system", "content": {} }] }))
        });
        let err = conn.get_prompt("x", serde_json::Map::new()).unwrap_err();
        assert!(err.to_string().contains("role system"), "{err}");
    }
}
