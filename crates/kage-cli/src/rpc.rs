//! `kage rpc`: a spec-conformant Agent Client Protocol agent.
//!
//! Speaks real ACP (newline-delimited JSON-RPC 2.0 over stdio,
//! protocol version 1) so editors that speak ACP - Zed, the bundled
//! Neovim client, anything built on the spec - can drive kage.
//! [`kage_acp::agent`] owns the protocol; this module is the
//! [`Agent`]: it bootstraps a session per `cwd`, runs the loop on
//! `session/prompt`, maps each [`kage_core::LoopEvent`] to a
//! `session/update`, and gates every tool call through the client via
//! `session/request_permission`.

use std::collections::HashMap;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use kage_acp::acp::{
    AgentCapabilities, ContentBlock, Implementation, InitializeRequest, InitializeResponse,
    LoadSessionRequest, MessageChunk, NewSessionRequest, NewSessionResponse, PROTOCOL_VERSION,
    PromptCapabilities, PromptRequest, PromptResponse, SessionUpdate, StopReason, ToolCall,
    ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use kage_acp::agent::{AcpPermission, Agent, PermissionDecision, PromptContext, serve_agent};
use kage_acp::jsonrpc::RpcError;
use kage_core::{CancelFlag, Content, LoopEvent, Message, Role, ToolOutput};
use kage_loop::{AgentContext, Hooks, LoopConfig};
use kage_plugin::PluginRuntime;
use kage_provider::ProviderRegistry;
use kage_session::{SessionId, SessionWriter};
use kage_tools::{ToolRegistry, builtin_registry};

use crate::runtime_env;

/// Entry point for the `Rpc` subcommand.
pub(crate) fn run(model_override: Option<&str>, system_role: &str) -> ExitCode {
    let agent = match CliAcpAgent::new(model_override, system_role) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("kage: rpc: {e}");
            return ExitCode::from(1);
        }
    };
    let reader = BufReader::new(std::io::stdin());
    match serve_agent(reader, std::io::stdout(), agent) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: rpc: {e}");
            ExitCode::from(1)
        }
    }
}

/// The innermost loop hook: every tool call is gated by the client
/// via `session/request_permission`. It is the base layer (session
/// and plugin hooks wrap it and forward `before_tool_call` down), so
/// it mirrors `NoopHooks` for every other callback.
struct GateHooks {
    gate: AcpPermission,
}

impl Hooks for GateHooks {
    fn before_tool_call(&mut self, name: &str, input: &serde_json::Value) -> Option<ToolOutput> {
        match self.gate.request(name, input.clone()) {
            PermissionDecision::Allow => None,
            PermissionDecision::Deny(reason) => {
                let detail = reason.map_or_else(String::new, |r| format!(": {r}"));
                Some(ToolOutput {
                    is_error: true,
                    text: format!("permission denied by client{detail}"),
                    structured: None,
                    terminate: false,
                })
            }
        }
    }
}

/// Per-session runtime state, built on `session/new`.
struct Session {
    model: String,
    system_prompt: String,
    tools: ToolRegistry,
    plugin_runtime: Option<Arc<PluginRuntime>>,
    cx: AgentContext,
    workdir: PathBuf,
    record_path: Option<PathBuf>,
    /// Owns the session's MCP child processes; kept alive so their
    /// tools stay valid for the session's lifetime.
    _mcp_manager: kage_mcp::McpManager,
}

/// The ACP agent `kage rpc` exposes.
struct CliAcpAgent {
    registry: ProviderRegistry,
    default_model: String,
    system_role: String,
    sessions: HashMap<String, Session>,
}

impl CliAcpAgent {
    fn new(model_override: Option<&str>, system_role: &str) -> Result<Self, String> {
        let registry = crate::build_provider_registry();
        if registry.ids().count() == 0 {
            return Err(
                "no provider credentials found; run `kage auth login` or set an API-key env var"
                    .to_owned(),
            );
        }
        let default_model =
            model_override.map_or_else(|| crate::default_model(&registry), str::to_owned);
        registry
            .resolve(&default_model)
            .map_err(|e| format!("cannot resolve model {default_model}: {e}"))?;
        Ok(Self {
            registry,
            default_model,
            system_role: system_role.to_owned(),
            sessions: HashMap::new(),
        })
    }

    fn build_session(&self, cwd: &str) -> Result<Session, RpcError> {
        let workdir = if cwd.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        } else {
            PathBuf::from(cwd)
        };
        let model = self.default_model.clone();
        let resolved = self
            .registry
            .resolve(&model)
            .map_err(|e| RpcError::internal(format!("resolve {model}: {e}")))?
            .model
            .clone();
        let bare = runtime_env::build_system_prompt(&self.system_role, &workdir, &model, &[]);
        let plugin_runtime = match crate::plugins_dir() {
            Ok(dir) => {
                crate::plugins::setup_runtime(&dir, &workdir, &model, &bare).unwrap_or_else(|e| {
                    eprintln!("kage: {e}");
                    None
                })
            }
            Err(e) => {
                eprintln!("kage: {e}");
                None
            }
        };
        let skills = crate::load_skills(&workdir, plugin_runtime.as_deref());
        let system_prompt =
            runtime_env::build_system_prompt(&self.system_role, &workdir, &model, &skills);
        let mut tools = builtin_registry();
        if let Some(rt) = plugin_runtime.as_ref() {
            crate::apply_plugin_tools(&mut tools, rt);
        }
        let (mcp_manager, mcp_errors) =
            crate::mcp::spawn_and_register(&mut tools, &workdir, plugin_runtime.as_deref());
        for (server, err) in mcp_errors {
            eprintln!("kage: mcp `{server}`: {err}");
        }
        let mut cx = AgentContext::new(resolved, &system_prompt).with_workdir(&workdir);
        if let Some(w) = runtime_env::context_window_for(&model) {
            cx = cx.with_context_window(w);
        }
        if let Some(o) = runtime_env::max_output_tokens_for(&model) {
            cx = cx.with_max_output_tokens(o);
        }
        Ok(Session {
            model,
            system_prompt,
            tools,
            plugin_runtime,
            cx,
            workdir,
            record_path: None,
            _mcp_manager: mcp_manager,
        })
    }
}

fn loop_config(workdir: &std::path::Path) -> LoopConfig {
    // Tools run strictly sequentially so the client sees one
    // permission prompt at a time.
    match kage_core::config::Config::load_layered(workdir) {
        Ok(c) => LoopConfig {
            compaction_threshold: c.loop_settings.compaction_threshold,
            parallel_tools: false,
            ..LoopConfig::default()
        },
        Err(e) => {
            eprintln!("kage: rpc: config error: {e}; using defaults");
            LoopConfig {
                parallel_tools: false,
                ..LoopConfig::default()
            }
        }
    }
}

fn turn_writer(
    model: &str,
    system_prompt: &str,
    path: &mut Option<PathBuf>,
) -> Option<SessionWriter> {
    if let Some(existing) = path.clone() {
        return SessionWriter::open(&existing)
            .map_err(|e| eprintln!("kage: rpc: session open: {e}"))
            .ok();
    }
    match crate::open_session(model, system_prompt) {
        Ok(w) => {
            *path = Some(w.path().to_path_buf());
            Some(w)
        }
        Err(e) => {
            eprintln!("kage: rpc: session: {e}");
            None
        }
    }
}

/// Translate a loop event into the matching ACP `session/update`.
/// `None` for events ACP has no streaming slot for (message
/// boundaries, compaction, errors handled via the prompt result).
fn to_update(event: &LoopEvent) -> Option<SessionUpdate> {
    match event {
        LoopEvent::TextDelta { delta, .. } => {
            Some(SessionUpdate::AgentMessageChunk(MessageChunk {
                content: ContentBlock::text(delta.clone()),
            }))
        }
        LoopEvent::ThinkingDelta { delta, .. } => {
            Some(SessionUpdate::AgentThoughtChunk(MessageChunk {
                content: ContentBlock::text(delta.clone()),
            }))
        }
        LoopEvent::ToolCallStart {
            id,
            name,
            input_partial,
        }
        | LoopEvent::ToolCallArgsDelta {
            id,
            name,
            input_partial,
        } => Some(SessionUpdate::ToolCall(ToolCall {
            tool_call_id: id.to_string(),
            title: name.clone(),
            kind: ToolKind::Other,
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            raw_input: Some(input_partial.clone()),
        })),
        LoopEvent::ToolCallEnd { id, output } => {
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: id.to_string(),
                status: Some(if output.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                }),
                content: vec![ToolCallContent::Content(MessageChunk {
                    content: ContentBlock::text(output.text.clone()),
                })],
                raw_output: output.structured.clone(),
            }))
        }
        _ => None,
    }
}

impl Agent for CliAcpAgent {
    fn initialize(&mut self, _req: InitializeRequest) -> InitializeResponse {
        InitializeResponse {
            protocol_version: PROTOCOL_VERSION,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                prompt_capabilities: PromptCapabilities::default(),
            },
            agent_info: Some(Implementation {
                name: "kage".to_owned(),
                title: None,
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            auth_methods: vec![],
        }
    }

    fn new_session(&mut self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
        let session = self.build_session(&req.cwd)?;
        let id = SessionId::new().to_string();
        self.sessions.insert(id.clone(), session);
        Ok(NewSessionResponse { session_id: id })
    }

    fn load_session(
        &mut self,
        req: LoadSessionRequest,
        ctx: &PromptContext,
    ) -> Result<(), RpcError> {
        let dir = crate::sessions_dir().map_err(RpcError::internal)?;
        let path = kage_session::find_by_prefix(&dir, &req.session_id)
            .map_err(|e| RpcError::internal(e.to_string()))?
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {}", req.session_id)))?;
        let replay = kage_session::replay(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        let mut session = self.build_session(&req.cwd)?;
        for message in &replay.history {
            let text = message
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                continue;
            }
            let chunk = MessageChunk {
                content: ContentBlock::text(text),
            };
            match message.role {
                Role::User => ctx.update(SessionUpdate::UserMessageChunk(chunk)),
                Role::Assistant => ctx.update(SessionUpdate::AgentMessageChunk(chunk)),
                _ => {}
            }
        }
        session.cx.history = replay.history;
        session.record_path = Some(path);
        self.sessions.insert(req.session_id, session);
        Ok(())
    }

    fn prompt(
        &mut self,
        req: PromptRequest,
        ctx: &PromptContext,
    ) -> Result<PromptResponse, RpcError> {
        let session = self
            .sessions
            .get_mut(&req.session_id)
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {}", req.session_id)))?;

        let text = req
            .prompt
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("\n");

        let (provider, bare_model) = {
            let resolved = self
                .registry
                .resolve(&session.model)
                .map_err(|e| RpcError::internal(format!("resolve {}: {e}", session.model)))?;
            (Arc::clone(resolved.provider), resolved.model.clone())
        };
        session.cx.model = bare_model;

        let parent = session.cx.history.last().map(|m| m.id);
        let user_msg = Message::new(Role::User, vec![Content::Text { text }], parent);
        session.cx.history.push(user_msg.clone());

        let writer = turn_writer(
            &session.model,
            &session.system_prompt,
            &mut session.record_path,
        );
        let cfg = loop_config(&session.workdir);
        let cancel_flag = CancelFlag::new();
        let emit_cancel = cancel_flag.clone();

        let res = crate::run_with_hooks(
            provider.as_ref(),
            &session.tools,
            &mut session.cx,
            cfg,
            &cancel_flag,
            GateHooks {
                gate: ctx.permission(),
            },
            &user_msg,
            writer,
            session.plugin_runtime.clone(),
            |event| {
                if ctx.is_cancelled() {
                    emit_cancel.cancel();
                }
                if let Some(update) = to_update(&event) {
                    ctx.update(update);
                }
            },
        );

        match res {
            Ok(()) => Ok(PromptResponse {
                stop_reason: if ctx.is_cancelled() {
                    StopReason::Cancelled
                } else {
                    StopReason::EndTurn
                },
            }),
            Err(e) => Err(RpcError::internal(e.to_string())),
        }
    }
}
