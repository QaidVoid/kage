//! kage CLI binary.
//!
//! Print mode (`-p`) runs a single prompt through the agent loop and streams
//! the assistant's text to stdout, then exits. The `list` subcommand prints
//! a table of recorded sessions stored under
//! `$XDG_DATA_HOME/kage/sessions/` (default `~/.local/share/kage/sessions/`).

mod acp_glue;
mod auth;
mod doctor;
mod history;
mod init;
mod mcp;
mod oauth;
mod plugins;
mod rpc;
mod runtime_env;
mod session;
mod state;
mod title;
mod tui;
mod usage_hooks;

pub(crate) use std::io::{self, Write};
pub(crate) use std::path::PathBuf;
pub(crate) use std::process::ExitCode;
pub(crate) use std::sync::Arc;

pub(crate) use chrono::Utc;
pub(crate) use clap::{Parser, Subcommand};
pub(crate) use kage_core::{CancelFlag, Content, LoopEvent, Message, Role};
pub(crate) use kage_loop::{AgentContext, Hooks, LoopConfig, NoopHooks, run};
pub(crate) use kage_provider::{
    ProviderRegistry, anthropic, compat, gemini, openai, openai_responses,
};
pub(crate) use kage_session::{
    EntryId, FORMAT_VERSION, Header, SessionId, SessionSummary, SessionWriter,
};
pub(crate) use kage_tools::builtin_registry;

pub(crate) use crate::plugins::{PluginEventHooks, setup_runtime};
pub(crate) use crate::session::SessionRecordingHooks;

/// kage: a minimal, extensible coding agent.
#[derive(Parser, Debug)]
#[command(name = "kage", version, about, long_about = None)]
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

    /// Emit one JSON object per loop event on stdout instead of plain
    /// text. Only meaningful with `-p/--print`; the same event alphabet
    /// the `rpc` transport uses (`message_start`, `text_delta`,
    /// `tool_call_start`, `tool_call_end`, `message_end`, `compaction`,
    /// `error`). Lets external tools parse the agent's output without
    /// screen-scraping.
    #[arg(long = "json", requires = "print")]
    json: bool,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
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
        /// Emit one JSON object per loop event on stdout instead of
        /// plain text. Same alphabet as the top-level `--json` flag.
        #[arg(long = "json", requires = "print")]
        json: bool,
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
    /// First-run setup wizard. Creates `~/.config/kage/config.toml`,
    /// scaffolds the data directories, and offers to save a provider
    /// API key. Idempotent: rerunning without `--force` keeps any
    /// existing config in place.
    Init {
        /// Overwrite an existing `~/.config/kage/config.toml` instead of
        /// keeping it.
        #[arg(long = "force")]
        force: bool,
        /// Skip every prompt. Useful for scripted bootstrap; will
        /// still write a fresh config when one is missing but never
        /// asks for an API key.
        #[arg(long = "non-interactive")]
        non_interactive: bool,
    },
    /// Diagnose the kage install: parses config, lists available
    /// providers, validates plugins, reports the active sandbox.
    /// Exit code is `0` when no check fails, `1` otherwise.
    Doctor,
    /// Render the `kage(1)` manpage from the clap CLI definition and
    /// write it to `--out` (default `man/kage.1`). Hidden because it
    /// is a developer / packager command: end users read the
    /// committed file at `man/kage.1`.
    #[command(hide = true)]
    GenManpage {
        /// Destination path for the generated manpage.
        #[arg(long = "out", default_value = "man/kage.1")]
        out: PathBuf,
    },
    /// Print a shell completion script for `kage` to stdout. Pipe to
    /// `source` (bash / zsh) or redirect into your shell's completion
    /// directory (fish / elvish). For example:
    ///
    ///   `source <(kage completions bash)`
    ///
    ///   `kage completions fish > ~/.config/fish/completions/kage.fish`
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Agent Client Protocol server: speak JSON-RPC over stdio so an
    /// editor (Zed, Neovim, ...) can drive kage. LSP-style
    /// `Content-Length` framing; each request is answered and loop
    /// progress is streamed back as `event` notifications.
    Rpc {
        /// Provider-qualified model id (`provider:model`). Defaults
        /// to the first authed provider's default model.
        #[arg(short = 'm', long = "model")]
        model: Option<String>,
        /// System-prompt role override forwarded to the agent loop.
        #[arg(long = "system", default_value = "")]
        system: String,
    },
    /// Model Context Protocol server: expose kage's built-in tools to
    /// another agent over stdio (newline-delimited JSON-RPC). Point an
    /// MCP client's server command at `kage mcp serve`.
    Mcp {
        /// MCP sub-action.
        #[command(subcommand)]
        action: McpAction,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum McpAction {
    /// Serve kage's built-in tools as an MCP server over stdio.
    Serve,
}

#[derive(Subcommand, Debug)]
pub(crate) enum AuthAction {
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

/// Dispatch a parsed subcommand. Lives outside [`main`] so its body
/// doesn't push the entry point past clippy's `too_many_lines`
/// threshold; the print-mode path below still runs from [`main`]
/// directly because it needs every local in scope.
pub(crate) fn run_subcommand(command: Command) -> ExitCode {
    match command {
        Command::List => run_list(),
        Command::Resume {
            id,
            last,
            print,
            model,
            json,
        } => run_resume(
            id.as_deref(),
            last,
            print.as_deref(),
            model.as_deref(),
            json,
        ),
        Command::Fork { id, at } => run_fork(&id, &at),
        Command::Search { query } => run_search(&query),
        Command::Auth { action } => match action {
            AuthAction::Login { provider } => auth::run_login(provider.as_deref()),
            AuthAction::Logout { provider } => auth::run_logout(&provider),
            AuthAction::List => auth::run_list(),
        },
        Command::Init {
            force,
            non_interactive,
        } => init::run(force, non_interactive),
        Command::Doctor => doctor::run(),
        Command::GenManpage { out } => run_gen_manpage(&out),
        Command::Completions { shell } => run_completions(shell),
        Command::Rpc { model, system } => rpc::run(model.as_deref(), &system),
        Command::Mcp { action } => match action {
            McpAction::Serve => mcp::run_serve(),
        },
    }
}

/// Print a shell completion script for `kage` to stdout. The script
/// is generated fresh from the clap definition every invocation, so
/// adding or renaming a subcommand requires no extra checked-in
/// artifacts.
pub(crate) fn run_completions(shell: clap_complete::Shell) -> ExitCode {
    use clap::CommandFactory as _;
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_owned();
    let mut stdout = io::stdout().lock();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut stdout);
    ExitCode::SUCCESS
}

/// Render the manpage via `clap_mangen` and write it to `out`.
/// Creates the parent directory when missing so a fresh checkout can
/// run `kage gen-manpage --out man/kage.1` without a prior `mkdir`.
pub(crate) fn run_gen_manpage(out: &std::path::Path) -> ExitCode {
    use clap::CommandFactory as _;
    let cmd = Cli::command();
    let mut buffer: Vec<u8> = Vec::new();
    if let Err(err) = clap_mangen::Man::new(cmd).render(&mut buffer) {
        eprintln!("kage: render manpage: {err}");
        return ExitCode::from(1);
    }
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!("kage: mkdir {}: {err}", parent.display());
        return ExitCode::from(1);
    }
    if let Err(err) = std::fs::write(out, &buffer) {
        eprintln!("kage: write {}: {err}", out.display());
        return ExitCode::from(1);
    }
    eprintln!("kage: wrote {}", out.display());
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(command) = cli.command {
        return run_subcommand(command);
    }

    let Some(prompt) = cli.print else {
        // No subcommand and no `-p`: drop into the interactive TUI.
        return tui::run_tui(cli.model.as_deref(), &cli.system);
    };

    let mut registry = build_provider_registry();

    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let provisional_model = cli
        .model
        .clone()
        .unwrap_or_else(|| default_model(&registry));
    let bare_prompt =
        runtime_env::build_system_prompt(&cli.system, &workdir, &provisional_model, &[]);
    let plugin_runtime = match plugins_dir() {
        Ok(dir) => match setup_runtime(&dir, &workdir, &provisional_model, &bare_prompt) {
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
    if let Some(rt) = plugin_runtime.as_ref() {
        plugins::merge_plugin_providers(rt, &mut registry);
        acp_glue::set_runtime(rt);
    }

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
    let skills = load_skills(&workdir, plugin_runtime.as_deref());
    let system_prompt = runtime_env::build_system_prompt(&cli.system, &workdir, &model, &skills);

    let mut tools = builtin_registry();
    if let Some(rt) = plugin_runtime.as_ref() {
        apply_plugin_tools(&mut tools, rt);
    }
    let (_mcp_manager, mcp_errors) =
        mcp::spawn_and_register(&mut tools, &workdir, plugin_runtime.as_deref());
    for (server, err) in mcp_errors {
        eprintln!("kage: mcp `{server}`: {err}");
    }
    let mut cx = AgentContext::new(resolved.model.clone(), &system_prompt).with_workdir(&workdir);
    if let Some(window) = runtime_env::context_window_for(&registry, &model) {
        cx = cx.with_context_window(window);
    }
    if let Some(out) = runtime_env::max_output_tokens_for(&registry, &model) {
        cx = cx.with_max_output_tokens(out);
    }
    let user_msg = Message::new(Role::User, vec![Content::Text { text: prompt }], None);
    cx.history.push(user_msg.clone());

    let writer = if cli.no_session {
        None
    } else {
        match open_session(&model, &system_prompt) {
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
        cli.json,
    );
    if let Err(err) = state::record_last_model(&model) {
        eprintln!("kage: {err}");
    }
    exit
}

mod cli_loop_run;
mod cli_printing;
mod cli_query;

pub(crate) use cli_loop_run::{execute_print_run, run_with_hooks};
pub(crate) use cli_printing::{print_event, print_event_json};
pub(crate) use cli_query::{run_fork, run_resume, run_search};

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
pub(crate) fn sessions_dir() -> Result<PathBuf, String> {
    Ok(data_root()?.join("sessions"))
}

/// Apply a plugin runtime's registered + overridden tools to `tools`.
/// Plain registrations land first, then overrides; an override that
/// names a tool not present after the first pass logs a warning to
/// stderr (headless mode only - the TUI surfaces the same message
/// through its plugin error channel).
pub(crate) fn apply_plugin_tools(
    tools: &mut kage_tools::ToolRegistry,
    rt: &kage_plugin::PluginRuntime,
) {
    for tool in rt.registered_tools() {
        tools.register(tool);
    }
    for tool in rt.registered_tool_overrides() {
        if tools.get(tool.name()).is_none() {
            eprintln!(
                "kage: override_tool: no tool named `{}` to override; treating as new registration",
                tool.name()
            );
        }
        tools.register(tool);
    }
}

/// Resolve the XDG-style plugin directory:
/// `$XDG_CONFIG_HOME/kage/plugins` (default `~/.config/kage/plugins`).
pub(crate) fn plugins_dir() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?
        .join("kage")
        .join("plugins"))
}

/// Resolve the XDG-style user theme directory:
/// `$XDG_CONFIG_HOME/kage/themes` (default `~/.config/kage/themes`).
pub(crate) fn themes_dir() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?
        .join("kage")
        .join("themes"))
}

/// Discover and load every SKILL.md under the user config dir
/// (`$XDG_CONFIG_HOME/kage/skills/<name>/`), the project-local
/// `./.kage/skills/<name>/`, and any directory contributed by a plugin's
/// `resources_discover` handler. Later entries shadow earlier ones with
/// the same skill name. Failing skills are logged to stderr and skipped.
pub(crate) fn load_skills(
    workdir: &std::path::Path,
    plugin_runtime: Option<&kage_plugin::PluginRuntime>,
) -> Vec<kage_core::Skill> {
    let mut out: std::collections::BTreeMap<String, kage_core::Skill> =
        std::collections::BTreeMap::new();
    let mut search: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(p) = xdg_dir("XDG_CONFIG_HOME", ".config") {
        search.push(p.join("kage").join("skills"));
    }
    search.push(workdir.join(".kage").join("skills"));
    if let Some(rt) = plugin_runtime {
        match rt.discover_resources() {
            Ok(entries) => search.extend(entries.skills),
            Err(err) => eprintln!("kage: resources_discover dispatch failed: {err}"),
        }
    }
    for dir in &search {
        for result in kage_core::load_skills_dir(dir) {
            match result {
                Ok(skill) => {
                    out.insert(skill.name.clone(), skill);
                }
                Err(err) => eprintln!("kage: skill load error: {err}"),
            }
        }
    }
    out.into_values().collect()
}

/// Resolve an XDG base directory: prefers `$ENV_VAR` if set and non-empty,
/// otherwise falls back to `$HOME/<fallback_subpath>`.
pub(crate) fn xdg_dir(env_var: &str, fallback_subpath: &str) -> Result<PathBuf, String> {
    if let Ok(v) = std::env::var(env_var)
        && !v.is_empty()
    {
        return Ok(PathBuf::from(v));
    }
    let home = dirs::home_dir().ok_or_else(|| "no home directory".to_owned())?;
    Ok(home.join(fallback_subpath))
}

/// Implement `kage list`: print one row per recorded session.
pub(crate) fn run_list() -> ExitCode {
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

pub(crate) fn print_session_table<W: Write>(out: &mut W, summaries: &[SessionSummary]) {
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

pub(crate) fn truncate_one_line(text: &str, max: usize) -> String {
    let single_line = text.lines().next().unwrap_or("").trim();
    if single_line.chars().count() <= max {
        return single_line.to_owned();
    }
    let head: String = single_line.chars().take(max - 3).collect();
    format!("{head}...")
}

/// Create a fresh session file under [`sessions_dir`].
pub(crate) fn open_session(model: &str, system_prompt: &str) -> Result<SessionWriter, String> {
    let (path, header) = plan_session(model, system_prompt)?;
    SessionWriter::create(path, header).map_err(|e| e.to_string())
}

/// Plan a fresh session: build the path and header without touching
/// the filesystem. The TUI uses this to defer file creation until the
/// first real prompt actually lands, so launching the TUI and
/// quitting (or resuming a different session) doesn't litter the
/// sessions directory with empty header-only stubs.
pub(crate) fn plan_session(model: &str, system_prompt: &str) -> Result<(PathBuf, Header), String> {
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
    Ok((path, header))
}

pub(crate) fn build_session_path(dir: &std::path::Path, session: SessionId) -> PathBuf {
    dir.join(format!("{session}.jsonl"))
}

/// Build a registry holding every provider whose API key is reachable
/// through either an env var (priority) or the saved auth store.
pub(crate) fn build_provider_registry() -> ProviderRegistry {
    let mut store = auth::AuthStore::load().unwrap_or_else(|_| auth::AuthStore::empty());
    let mut store_dirty = false;
    refresh_expiring_oauth(&mut store, &mut store_dirty);
    if store_dirty && let Err(err) = store.save() {
        eprintln!("kage: persist refreshed credentials: {err}");
    }
    let mut registry = ProviderRegistry::new();
    if let Some(key) = lookup_key("anthropic", &store) {
        registry.register(Arc::new(anthropic::AnthropicProvider::new(key)));
    }
    if let Some(key) = lookup_key("openai", &store) {
        registry.register(Arc::new(openai::OpenAiProvider::new(&key)));
        // The Responses API shares OpenAI auth: any user with an
        // OpenAI key automatically gets `openai-responses:` model
        // addressing. The credential store keeps a single entry.
        registry.register(Arc::new(openai_responses::OpenAiResponsesProvider::new(
            key,
        )));
    }
    if let Some(key) = lookup_key("gemini", &store) {
        registry.register(Arc::new(gemini::GeminiProvider::new(key)));
    }
    // Every OpenAI-compatible provider is described once in
    // `compat::COMPAT_PROVIDERS`; register each one the user has a key
    // for. Adding a provider is a single table entry there.
    for entry in compat::COMPAT_PROVIDERS {
        if let Some(key) = lookup_key(entry.id, &store) {
            registry.register(Arc::new(entry.build(key)));
        }
    }
    // The `acp` provider: `kage -m acp:<name>` drives an external ACP
    // agent declared in `[acp.agents.*]` or via `kage.acp.add_agent`.
    // Always registered (plugin-declared agents are resolved lazily);
    // its permission resolver defers to `kage.on_acp_permission` and
    // denies otherwise.
    let acp_cfg = kage_core::config::Config::load_default()
        .map(|c| c.acp)
        .unwrap_or_default();
    registry.register(Arc::new(
        kage_acp::client::AcpProvider::from_config(&acp_cfg)
            .with_permission(acp_glue::permission_resolver())
            .with_agent_source(acp_glue::agent_source()),
    ));
    registry
}

/// Look up `provider`'s bearer credential, preferring the env var
/// declared by [`auth::env_var_for`] and falling back to the auth
/// store. Returns the API key string for [`auth::Credential::ApiKey`]
/// entries and the access token for [`auth::Credential::Oauth`]
/// entries; the refresh path in [`build_provider_registry`] runs
/// before this is called so the returned token is fresh.
pub(crate) fn lookup_key(provider: &str, store: &auth::AuthStore) -> Option<String> {
    let env = auth::env_var_for(provider);
    if !env.is_empty() {
        if let Ok(v) = std::env::var(env) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    store.access_token(provider).map(str::to_owned)
}

/// Refresh every OAuth credential in `store` whose access token is
/// expired or due to expire inside the configured slack window. Sets
/// `*dirty` to `true` when at least one credential was rewritten so
/// the caller can persist before any provider request goes out.
pub(crate) fn refresh_expiring_oauth(store: &mut auth::AuthStore, dirty: &mut bool) {
    let now = Utc::now();
    let candidates: Vec<(String, auth::OAuthCredential)> = store
        .providers
        .iter()
        .filter_map(|(id, cred)| match cred {
            auth::Credential::Oauth(o) if o.expires_within(oauth::REFRESH_SLACK, now) => {
                Some((id.clone(), o.clone()))
            }
            _ => None,
        })
        .collect();
    for (id, prior) in candidates {
        match oauth::refresh(&id, &prior) {
            Ok(fresh) => {
                store.set_oauth(&id, fresh);
                *dirty = true;
            }
            Err(err) => {
                eprintln!("kage: refresh {id} credentials: {err}");
            }
        }
    }
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
pub(crate) fn default_model(registry: &ProviderRegistry) -> String {
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
