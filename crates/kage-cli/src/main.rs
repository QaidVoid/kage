//! kage CLI binary.
//!
//! Print mode (`-p`) runs a single prompt through the agent loop and streams
//! the assistant's text to stdout, then exits. The `list` subcommand prints
//! a table of recorded sessions stored under
//! `$XDG_DATA_HOME/kage/sessions/` (default `~/.local/share/kage/sessions/`).

mod auth;
mod plugins;
mod session;
mod state;
mod tui;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use clap::{Parser, Subcommand};
use kage_core::{CancelFlag, Content, LoopEvent, Message, Role};
use kage_loop::{AgentContext, Hooks, LoopConfig, NoopHooks, run};
use kage_provider::{ProviderRegistry, anthropic, compat, gemini, openai};
use kage_session::{EntryId, FORMAT_VERSION, Header, SessionId, SessionSummary, SessionWriter};
use kage_tools::builtin_registry;

use crate::plugins::{PluginEventHooks, setup_runtime};
use crate::session::SessionRecordingHooks;

/// kage: a minimal, extensible coding agent.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Subcommand. Omitting one requires `-p/--print` and runs print mode.
    #[command(subcommand)]
    command: Option<Command>,

    /// Run a single prompt through the agent loop and stream the response
    /// to stdout. Required in Phase 4 (the interactive TUI lands later).
    #[arg(short = 'p', long = "print")]
    print: Option<String>,

    /// Provider-qualified model id (`provider:model`). Defaults to
    /// `zai:glm-4.6` when `ZAI_API_KEY` is set, otherwise the first
    /// provider with an API key in the environment.
    #[arg(short = 'm', long = "model")]
    model: Option<String>,

    /// System prompt to prepend.
    #[arg(
        long = "system",
        default_value = "You are kage, a helpful coding agent."
    )]
    system: String,

    /// Disable session recording. By default every run writes a JSONL
    /// session file under `$XDG_DATA_HOME/kage/sessions/<session-id>.jsonl`.
    #[arg(long = "no-session")]
    no_session: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List recorded sessions in `$XDG_DATA_HOME/kage/sessions/`.
    List,
    /// Resume a recorded session, appending new entries to the same file.
    Resume {
        /// Session id or unique prefix. Mutually exclusive with --last.
        id: Option<String>,
        /// Resume the most recently created session.
        #[arg(long = "last", conflicts_with = "id")]
        last: bool,
        /// New user prompt to append. Required: print mode is the only
        /// runtime in this build.
        #[arg(short = 'p', long = "print")]
        print: Option<String>,
        /// Override the recorded model. Defaults to the model the session
        /// was last using.
        #[arg(short = 'm', long = "model")]
        model: Option<String>,
    },
    /// Fork a recorded session at a specific entry into a new session file.
    Fork {
        /// Source session id or unique prefix.
        id: String,
        /// Entry id (or unique prefix) to fork at; everything up through
        /// this entry is copied into the new session.
        #[arg(long = "at")]
        at: String,
    },
    /// Regex-search recorded sessions.
    Search {
        /// Regex query, ripgrep-style.
        query: String,
    },
    /// Manage saved provider API credentials.
    Auth {
        /// Auth subcommand.
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand, Debug)]
enum AuthAction {
    /// Save an API key for a provider, prompting for it without echo.
    Login {
        /// Provider id. Omit to pick from a list.
        provider: Option<String>,
    },
    /// Remove a saved API key.
    Logout {
        /// Provider id.
        provider: String,
    },
    /// Show which providers have saved credentials available.
    List,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::List) => return run_list(),
        Some(Command::Resume {
            id,
            last,
            print,
            model,
        }) => return run_resume(id.as_deref(), last, print.as_deref(), model.as_deref()),
        Some(Command::Fork { id, at }) => return run_fork(&id, &at),
        Some(Command::Search { query }) => return run_search(&query),
        Some(Command::Auth { action }) => {
            return match action {
                AuthAction::Login { provider } => auth::run_login(provider.as_deref()),
                AuthAction::Logout { provider } => auth::run_logout(&provider),
                AuthAction::List => auth::run_list(),
            };
        }
        None => {}
    }

    let Some(prompt) = cli.print else {
        // No subcommand and no `-p`: drop into the interactive TUI.
        let registry_for_default = build_provider_registry();
        let model = cli
            .model
            .unwrap_or_else(|| default_model(&registry_for_default));
        return tui::run_tui(&model, &cli.system);
    };

    let registry = build_provider_registry();
    if registry.ids().count() == 0 {
        eprintln!(
            "kage: no provider credentials found. Run `kage auth login` to save \
             one, or export an env var (ANTHROPIC_API_KEY, OPENAI_API_KEY, \
             GEMINI_API_KEY, ZAI_API_KEY, ZAI_CODING_API_KEY)."
        );
        return ExitCode::from(1);
    }

    let model = cli.model.unwrap_or_else(|| default_model(&registry));
    let resolved = match registry.resolve(&model) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kage: cannot resolve model {model}: {e}");
            return ExitCode::from(1);
        }
    };

    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let plugin_runtime = match plugins_dir() {
        Ok(dir) => match setup_runtime(&dir, &workdir, &model, &cli.system) {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("kage: {e}");
                None
            }
        },
        Err(e) => {
            eprintln!("kage: {e}");
            None
        }
    };

    let mut tools = builtin_registry();
    if let Some(rt) = plugin_runtime.as_ref() {
        for tool in rt.registered_tools() {
            tools.register(tool);
        }
    }
    let mut cx = AgentContext::new(resolved.model.clone(), &cli.system);
    let user_msg = Message::new(Role::User, vec![Content::Text { text: prompt }], None);
    cx.history.push(user_msg.clone());

    let writer = if cli.no_session {
        None
    } else {
        match open_session(&model, &cli.system) {
            Ok(w) => {
                eprintln!("kage: recording session to {}", w.path().display());
                Some(w)
            }
            Err(e) => {
                eprintln!("kage: failed to open session file: {e}");
                return ExitCode::from(1);
            }
        }
    };

    let exit = execute_print_run(
        resolved.provider.as_ref(),
        &tools,
        &mut cx,
        &user_msg,
        writer,
        plugin_runtime,
    );
    if let Err(err) = state::record_last_model(&model) {
        eprintln!("kage: {err}");
    }
    exit
}

/// Drive one print-mode run. Streams loop events to stdout and, when a
/// writer is supplied, records the conversation. When a plugin runtime is
/// supplied, plugin event handlers fire at turn boundaries. Returns the
/// appropriate process exit code.
fn execute_print_run(
    provider: &dyn kage_provider::Provider,
    tools: &kage_tools::ToolRegistry,
    cx: &mut AgentContext,
    user_msg: &Message,
    writer: Option<SessionWriter>,
    plugin_runtime: Option<std::sync::Arc<kage_plugin::PluginRuntime>>,
) -> ExitCode {
    let cfg = LoopConfig::default();
    let cancel = CancelFlag::new();
    let mut stdout = io::stdout().lock();
    let result = run_with_hooks(
        provider,
        tools,
        cx,
        &cfg,
        &cancel,
        user_msg,
        writer,
        plugin_runtime,
        |event| print_event(&mut stdout, &event),
    );
    let _ = writeln!(stdout);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_with_hooks<F>(
    provider: &dyn kage_provider::Provider,
    tools: &kage_tools::ToolRegistry,
    cx: &mut AgentContext,
    cfg: &LoopConfig,
    cancel: &CancelFlag,
    user_msg: &Message,
    writer: Option<SessionWriter>,
    plugin_runtime: Option<std::sync::Arc<kage_plugin::PluginRuntime>>,
    mut emit: F,
) -> Result<(), kage_core::LoopError>
where
    F: FnMut(LoopEvent),
{
    let mut session_layer: Box<dyn Hooks> = match writer {
        None => Box::new(NoopHooks),
        Some(w) => {
            let mut hooks = SessionRecordingHooks::new(NoopHooks, w);
            hooks.record_user_message(user_msg);
            Box::new(hooks)
        }
    };

    if let Some(runtime) = plugin_runtime {
        let mut hooks = PluginEventHooks::new(BoxedHooks(session_layer), runtime.clone());
        hooks.dispatch_agent_start();
        let res = run(provider, tools, cx, cfg, &mut hooks, cancel, &mut emit);
        hooks.dispatch_agent_end(res.is_ok());
        res
    } else {
        run(
            provider,
            tools,
            cx,
            cfg,
            session_layer.as_mut(),
            cancel,
            &mut emit,
        )
    }
}

/// Adapter so a `Box<dyn Hooks>` satisfies the static-dispatch `Hooks`
/// bound on [`PluginEventHooks`].
struct BoxedHooks(Box<dyn Hooks>);

impl Hooks for BoxedHooks {
    fn before_tool_call(
        &mut self,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<kage_core::ToolOutput> {
        self.0.before_tool_call(name, input)
    }

    fn after_tool_call(
        &mut self,
        name: &str,
        output: kage_core::ToolOutput,
    ) -> kage_core::ToolOutput {
        self.0.after_tool_call(name, output)
    }

    fn on_event(&mut self, event: &LoopEvent) {
        self.0.on_event(event);
    }

    fn get_steering(&mut self) -> Option<String> {
        self.0.get_steering()
    }

    fn get_followup(&mut self) -> Option<String> {
        self.0.get_followup()
    }
}

/// Implement `kage resume`: replay an existing session and append a new
/// user prompt before re-running the loop.
fn run_resume(
    id: Option<&str>,
    last: bool,
    print: Option<&str>,
    model_override: Option<&str>,
) -> ExitCode {
    let Some(prompt) = print else {
        eprintln!("kage: resume requires -p/--print in this build");
        return ExitCode::from(2);
    };
    let dir = match sessions_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let path = match resolve_resume_target(&dir, id, last) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };

    let replay = match kage_session::replay(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kage: failed to replay session {}: {e}", path.display());
            return ExitCode::from(1);
        }
    };

    let registry = build_provider_registry();
    if registry.ids().count() == 0 {
        eprintln!(
            "kage: no provider credentials found. Run `kage auth login` to save \
             one, or export an env var (ANTHROPIC_API_KEY, OPENAI_API_KEY, \
             GEMINI_API_KEY, ZAI_API_KEY, ZAI_CODING_API_KEY)."
        );
        return ExitCode::from(1);
    }
    let model = model_override.unwrap_or(&replay.model).to_owned();
    let resolved = match registry.resolve(&model) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kage: cannot resolve model {model}: {e}");
            return ExitCode::from(1);
        }
    };

    let writer = match SessionWriter::open(&path) {
        Ok(w) => {
            eprintln!("kage: appending to session {}", w.path().display());
            w
        }
        Err(e) => {
            eprintln!("kage: failed to reopen session file: {e}");
            return ExitCode::from(1);
        }
    };

    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let plugin_runtime = match plugins_dir() {
        Ok(dir) => match setup_runtime(&dir, &workdir, &model, &replay.header.system_prompt) {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("kage: {e}");
                None
            }
        },
        Err(e) => {
            eprintln!("kage: {e}");
            None
        }
    };

    let mut tools = builtin_registry();
    if let Some(rt) = plugin_runtime.as_ref() {
        for tool in rt.registered_tools() {
            tools.register(tool);
        }
    }
    let mut cx = AgentContext::new(resolved.model.clone(), &replay.header.system_prompt);
    cx.history = replay.history;
    let user_msg = Message::new(
        Role::User,
        vec![Content::Text {
            text: prompt.to_owned(),
        }],
        cx.history.last().map(|m| m.id),
    );
    cx.history.push(user_msg.clone());

    let exit = execute_print_run(
        resolved.provider.as_ref(),
        &tools,
        &mut cx,
        &user_msg,
        Some(writer),
        plugin_runtime,
    );
    if let Err(err) = state::record_last_model(&model) {
        eprintln!("kage: {err}");
    }
    exit
}

/// Implement `kage search <query>`: regex-grep across the sessions dir and
/// render each hit as `<file>:<line>: <text>`.
fn run_search(query: &str) -> ExitCode {
    let dir = match sessions_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let hits = match kage_session::search(&dir, query) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("kage: search failed: {e}");
            return ExitCode::from(1);
        }
    };
    if hits.is_empty() {
        eprintln!("kage: no matches in {}", dir.display());
        return ExitCode::SUCCESS;
    }
    let mut stdout = io::stdout().lock();
    for hit in &hits {
        let name = hit
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| hit.path.to_str().unwrap_or("?"));
        let preview = render_hit_preview(hit);
        let _ = writeln!(stdout, "{name}:{}: {preview}", hit.line_no);
    }
    ExitCode::SUCCESS
}

/// One-line preview text for a search hit. Decodes the matched JSONL into
/// a `SessionEntry` when possible and surfaces only the human-readable
/// fragments; raw lines fall through unchanged.
fn render_hit_preview(hit: &kage_session::SearchHit) -> String {
    let Some(entry) = hit.entry() else {
        return hit.line.clone();
    };
    match entry {
        kage_session::SessionEntry::Header(h) => format!("[header] model={}", h.model),
        kage_session::SessionEntry::Message(m) => {
            let role = match m.message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::ToolResult => "tool",
                Role::System => "system",
            };
            let text = first_text_block(&m.message).unwrap_or_else(|| "(no text)".to_owned());
            format!("[{role}] {}", truncate_one_line(&text, 120))
        }
        kage_session::SessionEntry::Compaction(c) => {
            format!("[compaction] kept={} summarized={}", c.kept, c.summarized)
        }
        kage_session::SessionEntry::Label(l) => format!("[label] {}", l.text),
        kage_session::SessionEntry::ModelChange(m) => format!("[model_change] {}", m.model),
        kage_session::SessionEntry::ThinkingLevelChange(t) => {
            format!("[thinking_level] {}", t.level)
        }
        kage_session::SessionEntry::Custom(c) => format!("[custom:{}]", c.kind),
    }
}

fn first_text_block(message: &Message) -> Option<String> {
    for block in &message.content {
        if let Content::Text { text } = block {
            return Some(text.clone());
        }
        if let Content::ToolResultBlock { output, .. } = block {
            return Some(output.clone());
        }
    }
    None
}

/// Implement `kage fork`: copy a session up through a chosen entry into a
/// fresh file with a new session id, linked back to the source via the
/// header's `parent_session` and `parent_entry` fields.
fn run_fork(src_id_prefix: &str, at_prefix: &str) -> ExitCode {
    let dir = match sessions_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let src_path = match kage_session::find_by_prefix(&dir, src_id_prefix) {
        Ok(Some(p)) => p,
        Ok(None) => {
            eprintln!("kage: no session matches prefix '{src_id_prefix}'");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("kage: failed to resolve session id: {e}");
            return ExitCode::from(1);
        }
    };
    let at = match kage_session::resolve_entry_prefix(&src_path, at_prefix) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("kage: failed to resolve entry id: {e}");
            return ExitCode::from(1);
        }
    };
    let new_session = SessionId::new();
    let dst = dir.join(format!("{new_session}.jsonl"));
    if let Err(e) = kage_session::fork(&src_path, &dst, new_session, at) {
        eprintln!("kage: fork failed: {e}");
        return ExitCode::from(1);
    }
    println!("{new_session}");
    eprintln!("kage: forked {} into {}", src_path.display(), dst.display());
    ExitCode::SUCCESS
}

/// Resolve which session file to resume based on cli flags.
fn resolve_resume_target(
    dir: &std::path::Path,
    id: Option<&str>,
    last: bool,
) -> Result<PathBuf, String> {
    if last {
        return kage_session::find_last(dir)
            .map_err(|e| format!("failed to scan sessions: {e}"))?
            .ok_or_else(|| format!("no sessions in {}", dir.display()));
    }
    let prefix = id.ok_or_else(|| "resume requires either --last or a session id".to_owned())?;
    kage_session::find_by_prefix(dir, prefix)
        .map_err(|e| format!("failed to resolve session id: {e}"))?
        .ok_or_else(|| format!("no session matches prefix '{prefix}'"))
}

/// Resolve `$XDG_DATA_HOME/kage` (default `~/.local/share/kage`).
pub(crate) fn data_root() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_DATA_HOME", ".local/share")?.join("kage"))
}

/// Resolve `$XDG_STATE_HOME/kage` (default `~/.local/state/kage`).
pub(crate) fn state_root() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_STATE_HOME", ".local/state")?.join("kage"))
}

/// Resolve the XDG-style directory holding session files:
/// `$XDG_DATA_HOME/kage/sessions` (default `~/.local/share/kage/sessions`).
fn sessions_dir() -> Result<PathBuf, String> {
    Ok(data_root()?.join("sessions"))
}

/// Resolve the XDG-style plugin directory:
/// `$XDG_CONFIG_HOME/kage/plugins` (default `~/.config/kage/plugins`).
fn plugins_dir() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?
        .join("kage")
        .join("plugins"))
}

/// Resolve an XDG base directory: prefers `$ENV_VAR` if set and non-empty,
/// otherwise falls back to `$HOME/<fallback_subpath>`.
fn xdg_dir(env_var: &str, fallback_subpath: &str) -> Result<PathBuf, String> {
    if let Ok(v) = std::env::var(env_var)
        && !v.is_empty()
    {
        return Ok(PathBuf::from(v));
    }
    let home = dirs::home_dir().ok_or_else(|| "no home directory".to_owned())?;
    Ok(home.join(fallback_subpath))
}

/// Implement `kage list`: print one row per recorded session.
fn run_list() -> ExitCode {
    let dir = match sessions_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let summaries = match kage_session::list(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kage: failed to list sessions: {e}");
            return ExitCode::from(1);
        }
    };
    if summaries.is_empty() {
        eprintln!("kage: no sessions found in {}", dir.display());
        return ExitCode::SUCCESS;
    }
    print_session_table(&mut io::stdout().lock(), &summaries);
    ExitCode::SUCCESS
}

fn print_session_table<W: Write>(out: &mut W, summaries: &[SessionSummary]) {
    let id_h = "ID";
    let created_h = "CREATED";
    let model_h = "MODEL";
    let prompt_h = "PROMPT";
    let _ = writeln!(
        out,
        "{id_h:<10}  {created_h:<19}  {model_h:<32}  {prompt_h}"
    );
    for s in summaries {
        let id = s.id.to_string();
        let id_short: String = id.chars().take(10).collect();
        let created = s.created_at.format("%Y-%m-%d %H:%M:%S").to_string();
        let model = &s.model;
        let prompt = match &s.last_user_prompt {
            Some(text) => truncate_one_line(text, 60),
            None => "(no user prompt)".to_owned(),
        };
        let _ = writeln!(out, "{id_short:<10}  {created:<19}  {model:<32}  {prompt}");
    }
}

fn truncate_one_line(text: &str, max: usize) -> String {
    let single_line = text.lines().next().unwrap_or("").trim();
    if single_line.chars().count() <= max {
        return single_line.to_owned();
    }
    let head: String = single_line.chars().take(max - 3).collect();
    format!("{head}...")
}

/// Create a fresh session file under [`sessions_dir`].
fn open_session(model: &str, system_prompt: &str) -> Result<SessionWriter, String> {
    let dir = sessions_dir()?;
    let session = SessionId::new();
    let path = build_session_path(&dir, session);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let header = Header {
        version: FORMAT_VERSION,
        session,
        id: EntryId::new(),
        ts: Utc::now(),
        cwd,
        model: model.to_owned(),
        system_prompt: system_prompt.to_owned(),
        parent_session: None,
        parent_entry: None,
    };
    SessionWriter::create(path, header).map_err(|e| e.to_string())
}

fn build_session_path(dir: &std::path::Path, session: SessionId) -> PathBuf {
    dir.join(format!("{session}.jsonl"))
}

/// Build a registry holding every provider whose API key is reachable
/// through either an env var (priority) or the saved auth store.
fn build_provider_registry() -> ProviderRegistry {
    let store = auth::AuthStore::load().unwrap_or_else(|_| auth::AuthStore::empty());
    let mut registry = ProviderRegistry::new();
    if let Some(key) = lookup_key("anthropic", &store) {
        registry.register(Arc::new(anthropic::AnthropicProvider::new(key)));
    }
    if let Some(key) = lookup_key("openai", &store) {
        registry.register(Arc::new(openai::OpenAiProvider::new(key)));
    }
    if let Some(key) = lookup_key("gemini", &store) {
        registry.register(Arc::new(gemini::GeminiProvider::new(key)));
    }
    if let Some(key) = lookup_key("zai", &store) {
        registry.register(Arc::new(compat::zai(key)));
    }
    if let Some(key) = lookup_key("zai-coding-plan", &store) {
        registry.register(Arc::new(compat::zai_coding_plan(key)));
    }
    if let Some(key) = lookup_key("deepseek", &store) {
        registry.register(Arc::new(compat::deepseek(key)));
    }
    if let Some(key) = lookup_key("groq", &store) {
        registry.register(Arc::new(compat::groq(key)));
    }
    if let Some(key) = lookup_key("mistral", &store) {
        registry.register(Arc::new(compat::mistral(key)));
    }
    if let Some(key) = lookup_key("cerebras", &store) {
        registry.register(Arc::new(compat::cerebras(key)));
    }
    if let Some(key) = lookup_key("xai", &store) {
        registry.register(Arc::new(compat::xai(key)));
    }
    if let Some(key) = lookup_key("openrouter", &store) {
        registry.register(Arc::new(compat::openrouter(key)));
    }
    if let Some(key) = lookup_key("fireworks-ai", &store) {
        registry.register(Arc::new(compat::fireworks_ai(key)));
    }
    if let Some(key) = lookup_key("moonshotai", &store) {
        registry.register(Arc::new(compat::moonshotai(key)));
    }
    if let Some(key) = lookup_key("kimi-for-coding", &store) {
        registry.register(Arc::new(compat::kimi_for_coding(key)));
    }
    registry
}

/// Look up `provider`'s API key, preferring the env var declared by
/// [`auth::env_var_for`] and falling back to the auth store.
fn lookup_key(provider: &str, store: &auth::AuthStore) -> Option<String> {
    let env = auth::env_var_for(provider);
    if !env.is_empty() {
        if let Ok(v) = std::env::var(env) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    store.get(provider).map(str::to_owned)
}

/// Order in which `default_model` falls back when there is no saved
/// last-used model: most-popular providers first.
const DEFAULT_MODEL_PRIORITY: &[&str] = &[
    "anthropic",
    "openai",
    "zai-coding-plan",
    "zai",
    "gemini",
    "deepseek",
    "groq",
    "mistral",
    "cerebras",
    "xai",
    "openrouter",
    "fireworks-ai",
    "moonshotai",
    "kimi-for-coding",
];

/// Pick a sensible default model. Prefers the last model the user
/// successfully ran (when it still resolves), then walks
/// [`DEFAULT_MODEL_PRIORITY`], asking the catalog for each registered
/// provider's preferred model. Returns an empty string when nothing
/// is wired up; callers are expected to handle that as "no credentials".
fn default_model(registry: &ProviderRegistry) -> String {
    if let Some(model) = state::State::load().last_model
        && registry.resolve(&model).is_ok()
    {
        return model;
    }
    for candidate in DEFAULT_MODEL_PRIORITY {
        if registry.get(candidate).is_none() {
            continue;
        }
        if let Some(model) = kage_provider::catalog::preferred_model(candidate) {
            return format!("{candidate}:{}", model.id);
        }
    }
    String::new()
}

/// Render one streaming event to stdout. Only text-bearing events produce
/// visible output; tool calls render a single bracketed status line.
fn print_event<W: Write>(out: &mut W, event: &LoopEvent) {
    match event {
        LoopEvent::TextDelta { delta, .. } => {
            let _ = out.write_all(delta.as_bytes());
            let _ = out.flush();
        }
        LoopEvent::ToolCallStart { name, .. } => {
            let _ = writeln!(out, "\n[tool: {name}]");
            let _ = out.flush();
        }
        LoopEvent::ToolCallEnd { output, .. } => {
            if output.is_error {
                let _ = writeln!(out, "[tool error] {}", output.text);
            }
            let _ = out.flush();
        }
        LoopEvent::Compaction {
            kept, summarized, ..
        } => {
            let _ = writeln!(out, "\n[compacted: kept {kept}, summarized {summarized}]");
            let _ = out.flush();
        }
        LoopEvent::Error { kind } => {
            let _ = writeln!(out, "\n[error] {kind}");
            let _ = out.flush();
        }
        _ => {}
    }
}
