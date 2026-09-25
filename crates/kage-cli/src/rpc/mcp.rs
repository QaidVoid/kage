//! The MCP servers a client passes and the prompt commands it sees.

use std::collections::BTreeMap;

use kage_acp::acp::McpServer;
use kage_core::config::McpServer as McpSpec;
use kage_core::protocol::{McpServerInfo, McpServerStatus};
use kage_jsonrpc::RpcError;
use kage_mcp::McpError;

/// The MCP servers a client passed, as specs by name.
///
/// # Errors
///
/// Invalid params for an `sse` server, since kage has no SSE transport.
pub(super) fn editor_servers(servers: &[McpServer]) -> Result<BTreeMap<String, McpSpec>, RpcError> {
    servers
        .iter()
        .map(|server| match server {
            McpServer::Stdio(stdio) => Ok((
                stdio.name.clone(),
                McpSpec {
                    command: Some(stdio.command.clone()),
                    args: stdio.args.clone(),
                    env: stdio
                        .env
                        .iter()
                        .map(|v| (v.name.clone(), v.value.clone()))
                        .collect(),
                    url: None,
                    headers: BTreeMap::new(),
                    disabled: false,
                    oauth: None,
                },
            )),
            McpServer::Http(http) => Ok((
                http.name.clone(),
                McpSpec {
                    command: None,
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    url: Some(http.url.clone()),
                    headers: http
                        .headers
                        .iter()
                        .map(|h| (h.name.clone(), h.value.clone()))
                        .collect(),
                    disabled: false,
                    oauth: None,
                },
            )),
            McpServer::Sse(sse) => Err(RpcError::new(
                -32602,
                format!(
                    "MCP server `{}` uses the sse transport, which kage does not \
                     support. Use http instead.",
                    sse.name
                ),
            )),
        })
        .collect()
}

/// `err` without the `kage mcp login` hint when it names one of the
/// `editor`'s servers, which are not configured, so a login cannot help.
pub(super) fn without_login(err: McpError, editor: &[String]) -> McpError {
    match err {
        McpError::Unauthorized { server, .. } if editor.contains(&server) => {
            McpError::Unauthorized {
                server,
                login: false,
            }
        }
        other => other,
    }
}

/// One `available_commands_update` entry per prompt of a live server in
/// `servers`, named `<server>:<prompt>`. The input hint lists required
/// arguments as `<name>` and optional ones as `[name]`, and a prompt
/// without arguments takes no input.
pub(super) fn prompt_commands(servers: &[McpServerInfo]) -> Vec<serde_json::Value> {
    let live = servers
        .iter()
        .filter(|server| server.status == McpServerStatus::Connected);
    live.flat_map(|server| {
        server.prompts.iter().map(move |prompt| {
            let hint = prompt
                .arguments
                .iter()
                .map(|arg| {
                    if arg.required {
                        format!("<{}>", arg.name)
                    } else {
                        format!("[{}]", arg.name)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let mut command = serde_json::json!({
                "name": format!("{}:{}", server.name, prompt.name),
                "description": prompt.description.clone().unwrap_or_default(),
            });
            if !hint.is_empty() {
                command["input"] = serde_json::json!({ "hint": hint });
            }
            command
        })
    })
    .collect()
}
