//! Expand MCP prompt commands and resource mentions in a user prompt.
//!
//! The engine calls [`expand`] on the run thread before a prompt enters
//! history, so the TUI, ACP and print mode share one implementation and
//! the recorded message is exactly what the model receives. Two rules
//! apply, both only to servers in the session's catalog:
//!
//! - A first text block that starts with `/<server>:<prompt>` naming a
//!   prompt of a live server is replaced by that prompt's messages, with
//!   the rest of the text bound to its arguments ([`bind_arguments`]).
//! - Every `@<server>:<uri>` token is read with `resources/read`, once
//!   per distinct URI, and appended to the message as a
//!   [`kage_core::resource_block`]. The typed text stays as it is.
//!
//! Resource text is capped at [`MAX_RESOURCE_TEXT`] per resource and
//! [`MAX_PROMPT_TEXT`] per prompt, with a line saying what was cut.

use std::sync::Arc;

use kage_core::protocol::{McpPrompt, McpServerInfo};
use kage_core::{Content, ImageSource, Role, resource_block};
use serde_json::{Map, Value};

use crate::catalog::{PromptMessage, ResourceContents};
use crate::server::{McpConnection, McpError};

/// Bytes of text kept from one resource.
pub const MAX_RESOURCE_TEXT: usize = 64 * 1024;

/// Bytes of resource text kept across one prompt.
pub const MAX_PROMPT_TEXT: usize = 256 * 1024;

/// Why a prompt could not be expanded. Nothing reaches the model.
#[derive(Debug, thiserror::Error)]
pub enum ExpandError {
    /// A required prompt argument was not given.
    #[error("mcp {server}:{prompt}: missing argument {argument}")]
    MissingArgument {
        /// Server name.
        server: String,
        /// Prompt name.
        prompt: String,
        /// The first missing argument.
        argument: String,
    },
    /// `prompts/get` failed.
    #[error("mcp {server}:{prompt}: {reason}")]
    Prompt {
        /// Server name.
        server: String,
        /// Prompt name.
        prompt: String,
        /// What the server or transport reported.
        reason: String,
    },
    /// A mention named a configured server that is not live.
    #[error("mcp {server}: not connected")]
    NotConnected {
        /// Server name.
        server: String,
    },
    /// `resources/read` failed.
    #[error("mcp {server}: read {uri}: {reason}")]
    Read {
        /// Server name.
        server: String,
        /// The mentioned URI.
        uri: String,
        /// What the server or transport reported.
        reason: String,
    },
}

/// Split `text` into `(server, prompt, rest)` when, after leading
/// whitespace, it starts with `/<server>:<prompt>`. `rest` is the
/// trimmed text after the command.
#[must_use]
pub fn parse_prompt_command(text: &str) -> Option<(&str, &str, &str)> {
    let command = text.trim_start().strip_prefix('/')?;
    let (token, rest) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    let (server, prompt) = token.split_once(':')?;
    (!server.is_empty() && !prompt.is_empty()).then_some((server, prompt, rest.trim()))
}

/// Every `@<server>:<uri>` token in `text` as `(server, uri)`, in order.
/// A token starts after whitespace or at the start of the text, the URI
/// runs to the next whitespace, and one trailing `.`, `,` or `;` is
/// dropped.
#[must_use]
pub fn find_mentions(text: &str) -> Vec<(&str, &str)> {
    text.split_whitespace()
        .filter_map(|word| {
            let (server, uri) = word.strip_prefix('@')?.split_once(':')?;
            let uri = uri.strip_suffix(['.', ',', ';']).unwrap_or(uri);
            (!server.is_empty() && !uri.is_empty()).then_some((server, uri))
        })
        .collect()
}

/// Bind `rest` to `prompt`'s arguments: whitespace separated words in
/// declared order, with the last declared argument taking the
/// remainder. Absent optional arguments are left out.
///
/// # Errors
///
/// Returns the name of the first required argument without a value.
pub fn bind_arguments(prompt: &McpPrompt, rest: &str) -> Result<Map<String, Value>, String> {
    let mut arguments = Map::new();
    let mut rest = rest.trim();
    let last = prompt.arguments.len().saturating_sub(1);
    for (i, argument) in prompt.arguments.iter().enumerate() {
        let value = if i == last {
            std::mem::take(&mut rest)
        } else {
            let (word, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            rest = tail.trim_start();
            word
        };
        if value.is_empty() {
            if argument.required {
                return Err(argument.name.clone());
            }
            continue;
        }
        arguments.insert(argument.name.clone(), Value::String(value.to_owned()));
    }
    Ok(arguments)
}

/// Expand the prompt command and the resource mentions in `content`,
/// using the live connections in `clients` and the prompts listed in
/// `catalog`. Content without either comes back unchanged.
///
/// # Errors
///
/// Fails when a prompt command misses a required argument, `prompts/get`
/// fails, a mention names a configured server that is not live, or a
/// mentioned resource cannot be read.
pub fn expand(
    content: Vec<Content>,
    clients: &[(String, Arc<McpConnection>)],
    catalog: &[McpServerInfo],
) -> Result<Vec<Content>, ExpandError> {
    let live = |server: &str| {
        clients
            .iter()
            .find(|(name, _)| name == server)
            .map(|(_, conn)| conn)
    };
    let mut budget = MAX_PROMPT_TEXT;
    let mut out = Vec::with_capacity(content.len());
    let mut mentions: Vec<(String, String)> = Vec::new();
    let mut first_text = true;
    for block in content {
        let Content::Text { text } = block else {
            out.push(block);
            continue;
        };
        if std::mem::take(&mut first_text)
            && let Some((server, name, rest)) = parse_prompt_command(&text)
            && let Some(conn) = live(server)
            && let Some(prompt) = find_prompt(catalog, server, name)
        {
            let arguments =
                bind_arguments(prompt, rest).map_err(|argument| ExpandError::MissingArgument {
                    server: server.to_owned(),
                    prompt: name.to_owned(),
                    argument,
                })?;
            let messages = conn
                .get_prompt(name, arguments)
                .map_err(|err| ExpandError::Prompt {
                    server: server.to_owned(),
                    prompt: name.to_owned(),
                    reason: reason(&err),
                })?;
            out.extend(
                messages
                    .iter()
                    .map(|message| prompt_content(message, server, &mut budget)),
            );
            continue;
        }
        for (server, uri) in find_mentions(&text) {
            let known = catalog.iter().any(|info| info.name == server);
            if known && !mentions.iter().any(|(s, u)| s == server && u == uri) {
                mentions.push((server.to_owned(), uri.to_owned()));
            }
        }
        out.push(Content::Text { text });
    }
    for (server, uri) in mentions {
        let conn = live(&server).ok_or_else(|| ExpandError::NotConnected {
            server: server.clone(),
        })?;
        let parts = conn.read_resource(&uri).map_err(|err| ExpandError::Read {
            server: server.clone(),
            uri: uri.clone(),
            reason: reason(&err),
        })?;
        out.extend(
            parts
                .into_iter()
                .map(|part| resource_content(&server, part, &mut budget)),
        );
    }
    Ok(out)
}

fn find_prompt<'a>(
    catalog: &'a [McpServerInfo],
    server: &str,
    name: &str,
) -> Option<&'a McpPrompt> {
    catalog
        .iter()
        .find(|info| info.name == server)?
        .prompts
        .iter()
        .find(|prompt| prompt.name == name)
}

/// The server's own message for an RPC error, else the whole error.
fn reason(err: &McpError) -> String {
    match err {
        McpError::Rpc { source, .. } => source.message.clone(),
        other => other.to_string(),
    }
}

/// One prompt message as content. Assistant text is labelled and kept in
/// the user message, because providers disagree on role sequences.
fn prompt_content(message: &PromptMessage, server: &str, budget: &mut usize) -> Content {
    let content = mcp_content(&message.content, server, budget);
    match (message.role, content) {
        (Role::Assistant, Content::Text { text }) => Content::Text {
            text: format!("Assistant: {text}"),
        },
        (_, content) => content,
    }
}

/// An MCP content block (`text`, `image`, `audio`, `resource` or
/// `resource_link`) as kage content.
fn mcp_content(value: &Value, server: &str, budget: &mut usize) -> Content {
    let field = |key: &str| value.get(key).and_then(Value::as_str);
    let text = |text: String| Content::Text { text };
    match field("type") {
        Some("text") => text(field("text").unwrap_or_default().to_owned()),
        Some("image") => match field("data") {
            Some(data) => image(data, field("mimeType").unwrap_or("image/png")),
            None => text("[image omitted]".to_owned()),
        },
        Some("audio") => text("[audio omitted]".to_owned()),
        Some("resource") => match embedded_resource(value.get("resource")) {
            Some(part) => resource_content(server, part, budget),
            None => text("[resource omitted]".to_owned()),
        },
        Some("resource_link") => {
            let uri = field("uri").unwrap_or_default();
            match field("name") {
                Some(name) => text(format!("Referenced resource: {uri} ({name})")),
                None => text(format!("Referenced resource: {uri}")),
            }
        }
        other => text(format!("[{} content omitted]", other.unwrap_or("unknown"))),
    }
}

fn embedded_resource(resource: Option<&Value>) -> Option<ResourceContents> {
    let resource = resource?;
    let field = |key: &str| resource.get(key).and_then(Value::as_str).map(str::to_owned);
    let uri = field("uri").unwrap_or_default();
    let mime_type = field("mimeType");
    if let Some(text) = field("text") {
        Some(ResourceContents::Text {
            uri,
            mime_type,
            text,
        })
    } else {
        field("blob").map(|data| ResourceContents::Blob {
            uri,
            mime_type,
            data,
        })
    }
}

/// One part of a resource as content: text in a resource block, an image
/// blob as an image, and any other blob as one line.
fn resource_content(server: &str, part: ResourceContents, budget: &mut usize) -> Content {
    match part {
        ResourceContents::Text {
            uri,
            mime_type,
            text,
        } => Content::Text {
            text: resource_block::render(
                &uri,
                Some(server),
                mime_type.as_deref(),
                &cap(&text, budget),
            ),
        },
        ResourceContents::Blob {
            mime_type: Some(mime),
            data,
            ..
        } if mime.starts_with("image/") => image(&data, &mime),
        ResourceContents::Blob {
            uri,
            mime_type,
            data,
        } => Content::Text {
            text: format!(
                "[binary resource {server}:{uri}: {}, {} bytes]",
                mime_type.as_deref().unwrap_or("application/octet-stream"),
                decoded_len(&data)
            ),
        },
    }
}

fn image(data: &str, mime: &str) -> Content {
    Content::Image {
        source: ImageSource::Base64 {
            data: data.to_owned(),
        },
        mime: mime.to_owned(),
    }
}

/// `text` cut to [`MAX_RESOURCE_TEXT`] and to what is left of `budget`,
/// with a line saying how much was kept.
pub(crate) fn cap(text: &str, budget: &mut usize) -> String {
    let limit = MAX_RESOURCE_TEXT.min(*budget);
    if text.len() <= limit {
        *budget -= text.len();
        return text.to_owned();
    }
    let mut kept = limit;
    while !text.is_char_boundary(kept) {
        kept -= 1;
    }
    *budget -= kept;
    format!(
        "{}\n[truncated: kept {kept} of {} bytes]",
        &text[..kept],
        text.len()
    )
}

/// Size of the bytes a base64 string encodes.
pub(crate) fn decoded_len(data: &str) -> usize {
    let data = data.trim_end();
    let padding = data.bytes().rev().take_while(|b| *b == b'=').count();
    (data.len() / 4 * 3).saturating_sub(padding)
}

#[cfg(test)]
mod tests {
    use kage_core::protocol::{McpPromptArgument, McpServerStatus};
    use kage_jsonrpc::RpcError;
    use serde_json::json;

    use super::*;
    use crate::catalog::tests::{Seen, scripted};

    fn text(text: &str) -> Content {
        Content::Text { text: text.into() }
    }

    fn argument(name: &str, required: bool) -> McpPromptArgument {
        McpPromptArgument {
            name: name.into(),
            description: None,
            required,
        }
    }

    fn prompt(name: &str, arguments: Vec<McpPromptArgument>) -> McpPrompt {
        McpPrompt {
            name: name.into(),
            description: None,
            arguments,
        }
    }

    fn info(name: &str, prompts: Vec<McpPrompt>) -> McpServerInfo {
        McpServerInfo {
            name: name.into(),
            status: McpServerStatus::Connected,
            tools: 0,
            resources: Vec::new(),
            templates: Vec::new(),
            prompts,
        }
    }

    type Clients = Vec<(String, Arc<McpConnection>)>;

    /// A live server `srv` with prompt `p(a, b, c)` whose `prompts/get`
    /// echoes its arguments, and resources whose text is their URI,
    /// except `test://missing` (an error), `test://big` (70 KiB) and
    /// `test://img` and `test://bin` (blobs).
    fn server() -> (Clients, Vec<McpServerInfo>, Seen) {
        let caps = json!({ "resources": {}, "prompts": {} });
        let (conn, _peer, seen) = scripted("srv", caps, |method, params| {
            let uri = params["uri"].as_str().unwrap_or_default();
            Ok(match (method, uri) {
                ("prompts/get", _) => json!({ "messages": [
                    { "role": "user", "content": { "type": "text", "text": params["arguments"].to_string() } },
                ] }),
                ("resources/read", "test://missing") => {
                    return Err(RpcError::new(-32002, "resource not found"));
                }
                ("resources/read", "test://big") => json!({ "contents": [
                    { "uri": uri, "text": "x".repeat(70 * 1024) },
                ] }),
                ("resources/read", "test://img") => json!({ "contents": [
                    { "uri": uri, "mimeType": "image/png", "blob": "aGk=" },
                ] }),
                ("resources/read", "test://bin") => json!({ "contents": [
                    { "uri": uri, "mimeType": "application/zip", "blob": "aGk=" },
                ] }),
                ("resources/read", _) => json!({ "contents": [
                    { "uri": uri, "mimeType": "text/plain", "text": uri },
                ] }),
                (other, _) => return Err(RpcError::method_not_found(other)),
            })
        });
        let catalog = vec![info(
            "srv",
            vec![prompt(
                "p",
                vec![
                    argument("a", true),
                    argument("b", false),
                    argument("c", false),
                ],
            )],
        )];
        (vec![("srv".into(), conn)], catalog, seen)
    }

    fn requests(seen: &Seen) -> Vec<(String, Value)> {
        seen.lock().unwrap().clone()
    }

    #[test]
    fn prompt_commands_parse_after_leading_whitespace() {
        assert_eq!(
            parse_prompt_command("  /srv:p one  two "),
            Some(("srv", "p", "one  two"))
        );
        assert_eq!(parse_prompt_command("/srv:p"), Some(("srv", "p", "")));
        assert_eq!(parse_prompt_command("/help"), None);
        assert_eq!(parse_prompt_command("/:p x"), None);
        assert_eq!(parse_prompt_command("say /srv:p"), None);
    }

    #[test]
    fn arguments_bind_in_order_and_the_last_takes_the_remainder() {
        let p = prompt(
            "p",
            vec![
                argument("a", true),
                argument("b", false),
                argument("c", false),
            ],
        );
        let bound = bind_arguments(&p, "one two three  four").unwrap();
        assert_eq!(
            Value::Object(bound),
            json!({ "a": "one", "b": "two", "c": "three  four" })
        );
        let bound = bind_arguments(&p, "one").unwrap();
        assert_eq!(Value::Object(bound), json!({ "a": "one" }));
        assert_eq!(bind_arguments(&p, "  "), Err("a".to_owned()));
        let bound = bind_arguments(&prompt("none", Vec::new()), "ignored").unwrap();
        assert!(bound.is_empty());
    }

    #[test]
    fn a_prompt_command_is_replaced_by_the_prompt_messages() {
        let (clients, catalog, seen) = server();
        let image = Content::Image {
            source: ImageSource::Base64 {
                data: "aGk=".into(),
            },
            mime: "image/png".into(),
        };
        let out = expand(
            vec![text(" /srv:p 0.7 terse and short"), image.clone()],
            &clients,
            &catalog,
        )
        .unwrap();
        assert_eq!(
            out,
            [text(r#"{"a":"0.7","b":"terse","c":"and short"}"#), image]
        );
        assert_eq!(
            requests(&seen),
            [(
                "prompts/get".to_owned(),
                json!({ "name": "p", "arguments": { "a": "0.7", "b": "terse", "c": "and short" } })
            )]
        );
    }

    #[test]
    fn a_missing_required_argument_fails_before_any_request() {
        let (clients, catalog, seen) = server();
        let err = expand(vec![text("/srv:p")], &clients, &catalog).unwrap_err();
        assert_eq!(err.to_string(), "mcp srv:p: missing argument a");
        assert!(requests(&seen).is_empty());
    }

    #[test]
    fn an_unknown_server_or_prompt_leaves_the_text_alone() {
        let (clients, catalog, seen) = server();
        for input in ["/nope:p x", "/srv:other x", "/help", "hi @nope:test://a"] {
            let out = expand(vec![text(input)], &clients, &catalog).unwrap();
            assert_eq!(out, [text(input)]);
        }
        assert!(requests(&seen).is_empty());
    }

    #[test]
    fn mentions_drop_trailing_punctuation_and_need_a_word_start() {
        assert_eq!(
            find_mentions("see @srv:test://a, @srv:test://b. and @srv:c;; me@srv:x @:y @srv:"),
            [("srv", "test://a"), ("srv", "test://b"), ("srv", "c;")]
        );
    }

    #[test]
    fn mentions_read_each_uri_once_and_append_blocks() {
        let (clients, catalog, seen) = server();
        let typed = "compare @srv:test://a with @srv:test://b; and @srv:test://a.";
        let out = expand(vec![text(typed)], &clients, &catalog).unwrap();
        assert_eq!(
            out,
            [
                text(typed),
                text(&resource_block::render(
                    "test://a",
                    Some("srv"),
                    Some("text/plain"),
                    "test://a"
                )),
                text(&resource_block::render(
                    "test://b",
                    Some("srv"),
                    Some("text/plain"),
                    "test://b"
                )),
            ]
        );
        let uris: Vec<Value> = requests(&seen)
            .into_iter()
            .map(|(_, p)| p["uri"].clone())
            .collect();
        assert_eq!(uris, [json!("test://a"), json!("test://b")]);
    }

    #[test]
    fn a_failing_read_or_a_dead_server_is_an_error() {
        let (clients, mut catalog, _seen) = server();
        let err = expand(vec![text("@srv:test://missing")], &clients, &catalog).unwrap_err();
        assert_eq!(
            err.to_string(),
            "mcp srv: read test://missing: resource not found"
        );
        catalog.push(McpServerInfo {
            status: McpServerStatus::Failed {
                error: "gone".into(),
            },
            ..info("down", Vec::new())
        });
        let err = expand(vec![text("@down:test://a")], &clients, &catalog).unwrap_err();
        assert_eq!(err.to_string(), "mcp down: not connected");
    }

    #[test]
    fn resource_text_is_capped_with_a_line() {
        let (clients, catalog, _seen) = server();
        let out = expand(vec![text("@srv:test://big")], &clients, &catalog).unwrap();
        let Content::Text { text: block } = &out[1] else {
            panic!("{out:?}");
        };
        assert!(block.contains("\n[truncated: kept 65536 of 71680 bytes]\n</resource>"));
        let body = block.split_once('\n').unwrap().1;
        assert_eq!(body.matches('x').count(), MAX_RESOURCE_TEXT);

        let mut budget = 10;
        assert_eq!(cap("abcdef", &mut budget), "abcdef");
        assert_eq!(
            cap("ghijkl", &mut budget),
            "ghij\n[truncated: kept 4 of 6 bytes]"
        );
        assert_eq!(cap("m", &mut budget), "\n[truncated: kept 0 of 1 bytes]");
    }

    #[test]
    fn blobs_become_images_or_one_line() {
        let (clients, catalog, _seen) = server();
        let out = expand(
            vec![text("@srv:test://img @srv:test://bin")],
            &clients,
            &catalog,
        )
        .unwrap();
        assert_eq!(
            out[1..],
            [
                Content::Image {
                    source: ImageSource::Base64 {
                        data: "aGk=".into()
                    },
                    mime: "image/png".into(),
                },
                text("[binary resource srv:test://bin: application/zip, 2 bytes]"),
            ]
        );
    }

    #[test]
    fn prompt_results_map_every_content_kind() {
        let mut budget = MAX_PROMPT_TEXT;
        let message = |role: Role, content: Value| PromptMessage { role, content };
        let contents: Vec<Content> = [
            message(Role::Assistant, json!({ "type": "text", "text": "sure" })),
            message(
                Role::User,
                json!({ "type": "image", "data": "aGk=", "mimeType": "image/jpeg" }),
            ),
            message(
                Role::User,
                json!({ "type": "resource", "resource": { "uri": "test://r", "text": "body" } }),
            ),
            message(
                Role::User,
                json!({ "type": "resource_link", "uri": "test://l", "name": "L" }),
            ),
            message(Role::User, json!({ "type": "audio", "data": "x" })),
        ]
        .iter()
        .map(|m| prompt_content(m, "srv", &mut budget))
        .collect();
        assert_eq!(
            contents,
            [
                text("Assistant: sure"),
                Content::Image {
                    source: ImageSource::Base64 {
                        data: "aGk=".into()
                    },
                    mime: "image/jpeg".into(),
                },
                text(&resource_block::render(
                    "test://r",
                    Some("srv"),
                    None,
                    "body"
                )),
                text("Referenced resource: test://l (L)"),
                text("[audio omitted]"),
            ]
        );
    }
}
