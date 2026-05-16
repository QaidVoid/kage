//! `kage rpc`: Agent Client Protocol server over stdio.
//!
//! Wires the protocol mechanics in [`kage_acp`] to the same agent
//! machinery print mode uses. The protocol crate owns framing,
//! schema, and threading; this module is the [`AcpBackend`]: it
//! resolves the model, builds the tool registry and plugin runtime,
//! and drives the loop, forwarding each [`kage_core::LoopEvent`] as an
//! `event` notification (the same alphabet as `kage -p --json`).

use std::io::BufReader;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use kage_acp::schema::PromptParams;
use kage_acp::server::{AcpBackend, Cancel, Permission, PermissionOutcome};
use kage_core::{CancelFlag, Content, Message, Role, ToolOutput};
use kage_loop::{AgentContext, Hooks, LoopConfig};
use kage_plugin::PluginRuntime;
use kage_provider::ProviderRegistry;
use kage_session::SessionWriter;
use kage_tools::{ToolRegistry, builtin_registry};

use crate::runtime_env;

/// Entry point for the `Rpc` subcommand.
pub(crate) fn run(model_override: Option<&str>, system_role: &str) -> ExitCode {
    let backend = match CliAcpBackend::new(model_override, system_role) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("kage: rpc: {e}");
            return ExitCode::from(1);
        }
    };
    let reader = BufReader::new(std::io::stdin());
    match kage_acp::server::serve(reader, std::io::stdout(), backend) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: rpc: {e}");
            ExitCode::from(1)
        }
    }
}

/// The innermost loop hook for `kage rpc`: every tool call is gated
/// by the editor. It is the base layer (the session and plugin hooks
/// wrap it and forward `before_tool_call` down), so it mirrors
/// `NoopHooks` for every other callback and overrides only the gate.
struct GateHooks {
    gate: Permission,
}

impl Hooks for GateHooks {
    fn before_tool_call(&mut self, name: &str, input: &serde_json::Value) -> Option<ToolOutput> {
        match self.gate.request(name, input) {
            PermissionOutcome::Allow => None,
            PermissionOutcome::Deny { reason } => {
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

/// The agent backend `kage rpc` exposes over ACP.
struct CliAcpBackend {
    registry: ProviderRegistry,
    /// Connection-default qualified model (`provider:model`).
    model: String,
    system_prompt: String,
    workdir: PathBuf,
    tools: ToolRegistry,
    plugin_runtime: Option<Arc<PluginRuntime>>,
    cx: AgentContext,
    /// Set once the first prompt (or a `session/load`) opens a file;
    /// later prompts append to it.
    session_path: Option<PathBuf>,
}

impl CliAcpBackend {
    fn new(model_override: Option<&str>, system_role: &str) -> Result<Self, String> {
        let registry = crate::build_provider_registry();
        if registry.ids().count() == 0 {
            return Err(
                "no provider credentials found; run `kage auth login` or set an API-key env var"
                    .to_owned(),
            );
        }
        let model = model_override.map_or_else(|| crate::default_model(&registry), str::to_owned);
        let resolved_model = registry
            .resolve(&model)
            .map_err(|e| format!("cannot resolve model {model}: {e}"))?
            .model
            .clone();

        let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let bare_prompt = runtime_env::build_system_prompt(system_role, &workdir, &model, &[]);
        let plugin_runtime = match crate::plugins_dir() {
            Ok(dir) => crate::plugins::setup_runtime(&dir, &workdir, &model, &bare_prompt)
                .unwrap_or_else(|e| {
                    eprintln!("kage: {e}");
                    None
                }),
            Err(e) => {
                eprintln!("kage: {e}");
                None
            }
        };
        let skills = crate::load_skills(&workdir, plugin_runtime.as_deref());
        let system_prompt =
            runtime_env::build_system_prompt(system_role, &workdir, &model, &skills);

        let mut tools = builtin_registry();
        if let Some(rt) = plugin_runtime.as_ref() {
            crate::apply_plugin_tools(&mut tools, rt);
        }

        let mut cx = AgentContext::new(resolved_model, &system_prompt).with_workdir(&workdir);
        if let Some(w) = runtime_env::context_window_for(&model) {
            cx = cx.with_context_window(w);
        }
        if let Some(o) = runtime_env::max_output_tokens_for(&model) {
            cx = cx.with_max_output_tokens(o);
        }

        Ok(Self {
            registry,
            model,
            system_prompt,
            workdir,
            tools,
            plugin_runtime,
            cx,
            session_path: None,
        })
    }

    fn loop_config(&self) -> LoopConfig {
        // Tools run strictly sequentially so the editor sees one
        // permission prompt at a time (a single outstanding gate).
        match kage_core::config::Config::load_layered(&self.workdir) {
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

    /// A session writer for this turn: append to the connection's
    /// file, opening or creating it on first use.
    fn turn_writer(&mut self) -> Option<SessionWriter> {
        if let Some(path) = self.session_path.clone() {
            return SessionWriter::open(&path)
                .map_err(|e| eprintln!("kage: rpc: session open: {e}"))
                .ok();
        }
        match crate::open_session(&self.model, &self.system_prompt) {
            Ok(w) => {
                self.session_path = Some(w.path().to_path_buf());
                Some(w)
            }
            Err(e) => {
                eprintln!("kage: rpc: session: {e}");
                None
            }
        }
    }
}

impl AcpBackend for CliAcpBackend {
    fn server_info(&self) -> serde_json::Value {
        serde_json::json!({
            "name": "kage",
            "version": env!("CARGO_PKG_VERSION"),
            "protocol": "acp",
            "model": self.model,
            "methods": [
                "initialize", "prompt", "cancel",
                "permission/respond", "session/load", "session/list",
            ],
        })
    }

    fn prompt(
        &mut self,
        params: &PromptParams,
        cancel: &Cancel,
        permission: &Permission,
        emit: &mut dyn FnMut(serde_json::Value),
    ) -> Result<serde_json::Value, String> {
        let qualified = params.model.clone().unwrap_or_else(|| self.model.clone());
        let (provider, bare_model) = {
            let resolved = self
                .registry
                .resolve(&qualified)
                .map_err(|e| format!("cannot resolve model {qualified}: {e}"))?;
            (Arc::clone(resolved.provider), resolved.model.clone())
        };

        self.cx.model = bare_model;
        if let Some(w) = runtime_env::context_window_for(&qualified) {
            self.cx.context_window = w;
        }
        self.cx.max_output_tokens = runtime_env::max_output_tokens_for(&qualified);

        let parent = self.cx.history.last().map(|m| m.id);
        let user_msg = Message::new(
            Role::User,
            vec![Content::Text {
                text: params.prompt.clone(),
            }],
            parent,
        );
        self.cx.history.push(user_msg.clone());

        let writer = self.turn_writer();
        let cfg = self.loop_config();
        let cancel_flag = CancelFlag::new();
        let acp_cancel = cancel.clone();
        let emit_cancel = cancel_flag.clone();

        let res = crate::run_with_hooks(
            provider.as_ref(),
            &self.tools,
            &mut self.cx,
            cfg,
            &cancel_flag,
            GateHooks {
                gate: permission.clone(),
            },
            &user_msg,
            writer,
            self.plugin_runtime.clone(),
            |event| {
                if acp_cancel.is_cancelled() {
                    emit_cancel.cancel();
                }
                match serde_json::to_value(&event) {
                    Ok(v) => emit(v),
                    Err(e) => emit(serde_json::json!({
                        "type": "error",
                        "kind": { "kind": "other", "message": format!("encode: {e}") },
                    })),
                }
            },
        );

        match res {
            Ok(()) => Ok(serde_json::json!({ "status": "completed" })),
            Err(e) => Err(e.to_string()),
        }
    }

    fn list_sessions(&mut self) -> Result<serde_json::Value, String> {
        let dir = crate::sessions_dir()?;
        let summaries = kage_session::list(&dir).map_err(|e| e.to_string())?;
        let rows: Vec<serde_json::Value> = summaries
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id.to_string(),
                    "model": s.model,
                    "updated_at": s.updated_at.to_rfc3339(),
                    "entries": s.entry_count,
                    "last_prompt": s.last_user_prompt,
                })
            })
            .collect();
        Ok(serde_json::Value::Array(rows))
    }

    fn load_session(&mut self, id: &str) -> Result<serde_json::Value, String> {
        let dir = crate::sessions_dir()?;
        let path = kage_session::find_by_prefix(&dir, id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no session matching `{id}`"))?;
        let replay = kage_session::replay(&path).map_err(|e| e.to_string())?;
        let messages = replay.history.len();
        let loaded = replay.header.session.to_string();
        self.cx.history = replay.history;
        if let Ok(r) = self.registry.resolve(&replay.model) {
            self.cx.model.clone_from(&r.model);
            self.model.clone_from(&replay.model);
        }
        self.session_path = Some(path);
        Ok(serde_json::json!({ "loaded": loaded, "messages": messages }))
    }
}
