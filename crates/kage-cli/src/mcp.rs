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
    let handler = sampling_handler(&cfg);
    let (mut manager, mut errors) =
        McpManager::spawn_all(&cfg, vec![workdir.to_path_buf()], handler);
    errors.extend(manager.register_into(tools));
    (manager, errors)
}

/// Build the server-request handler when `[mcp] allow_sampling` is set,
/// backed by the host's default model. Returns `None` when sampling is
/// off, so servers never see a `sampling` capability advertised.
fn sampling_handler(cfg: &McpConfig) -> Option<std::sync::Arc<dyn kage_mcp::ServerRequestHandler>> {
    if !cfg.allow_sampling {
        return None;
    }
    let registry = crate::build_provider_registry();
    let model = crate::default_model(&registry);
    Some(std::sync::Arc::new(SamplingHandler { registry, model }))
}

/// Answers `sampling/createMessage` by running the request through the
/// host's default model. Other server requests are declined (`None`).
struct SamplingHandler {
    registry: kage_provider::ProviderRegistry,
    model: String,
}

impl kage_mcp::ServerRequestHandler for SamplingHandler {
    fn capabilities(&self) -> serde_json::Value {
        serde_json::json!({ "sampling": {} })
    }

    fn handle(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, kage_jsonrpc::RpcError>> {
        if method != "sampling/createMessage" {
            return None;
        }
        Some(self.create_message(params))
    }
}

impl SamplingHandler {
    fn create_message(
        &self,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, kage_jsonrpc::RpcError> {
        use kage_core::{CancelFlag, Content, Message, Role};
        use kage_provider::{ProviderEvent, StreamRequest};

        let mut history = Vec::new();
        if let Some(messages) = params.get("messages").and_then(|m| m.as_array()) {
            for msg in messages {
                let role = match msg.get("role").and_then(serde_json::Value::as_str) {
                    Some("assistant") => Role::Assistant,
                    _ => Role::User,
                };
                let text = msg
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                history.push(Message::new(role, vec![Content::Text { text }], None));
            }
        }

        let resolved = self
            .registry
            .resolve(&self.model)
            .map_err(|e| kage_jsonrpc::RpcError::internal(format!("sampling: {e}")))?;
        let mut req = StreamRequest::new(resolved.model.clone(), history);
        req.system = params
            .get("systemPrompt")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        req.max_output_tokens = params
            .get("maxTokens")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok());

        let cancel = CancelFlag::new();
        let stream = resolved
            .provider
            .stream(req, &cancel)
            .map_err(|e| kage_jsonrpc::RpcError::internal(format!("sampling stream: {e}")))?;
        let mut text = String::new();
        for event in stream {
            match event {
                Ok(ProviderEvent::TextDelta { delta }) => text.push_str(&delta),
                Ok(ProviderEvent::MessageEnd { .. }) => break,
                Ok(_) => {}
                Err(e) => {
                    return Err(kage_jsonrpc::RpcError::internal(format!("sampling: {e}")));
                }
            }
        }

        Ok(serde_json::json!({
            "role": "assistant",
            "content": { "type": "text", "text": text },
            "model": self.model,
            "stopReason": "endTurn",
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kage_mcp::ServerRequestHandler;
    use kage_provider::testing::MockProvider;
    use kage_provider::{ProviderEvent, ProviderRegistry, StopReason};

    use super::SamplingHandler;

    #[test]
    fn sampling_runs_the_prompt_through_the_model() {
        let mock = MockProvider::replaying(vec![
            Ok(ProviderEvent::TextDelta {
                delta: "pong".into(),
            }),
            Ok(ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                usage: kage_core::TokenUsage::default(),
            }),
        ]);
        let registry = ProviderRegistry::new().with(Arc::new(mock));
        let handler = SamplingHandler {
            registry,
            model: "mock:m".to_owned(),
        };

        let params = serde_json::json!({
            "messages": [
                { "role": "user", "content": { "type": "text", "text": "ping" } }
            ],
            "systemPrompt": "be brief",
            "maxTokens": 100,
        });
        let result = handler
            .handle("sampling/createMessage", &params)
            .expect("sampling is handled")
            .expect("sampling succeeds");
        assert_eq!(result["role"], "assistant");
        assert_eq!(result["content"]["text"], "pong");
        assert_eq!(result["model"], "mock:m");
    }

    #[test]
    fn non_sampling_request_is_declined() {
        let registry = ProviderRegistry::new();
        let handler = SamplingHandler {
            registry,
            model: "mock:m".to_owned(),
        };
        assert!(
            handler
                .handle("elicitation/create", &serde_json::Value::Null)
                .is_none()
        );
        assert!(handler.capabilities()["sampling"].is_object());
    }
}
