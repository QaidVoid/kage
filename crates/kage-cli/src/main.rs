//! kage CLI binary.
//!
//! Layering: top of the workspace; depends on every library crate it uses.
//!
//! Print mode (`-p`) runs a single prompt through the agent loop and streams
//! the assistant's text to stdout, then exits. The `list` subcommand prints
//! a table of recorded sessions stored under
//! `$XDG_DATA_HOME/kage/sessions/` (default `~/.local/share/kage/sessions/`).

mod acp_glue;
mod auth;
mod doctor;
mod engine;
mod history;
mod init;
mod mcp;
mod oauth;
mod permissions;
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
pub(crate) use kage_core::{Content, LoopEvent, Message, Role};
pub(crate) use kage_loop::{AgentContext, LoopConfig};
pub(crate) use kage_provider::{
    ProviderRegistry, anthropic, compat, gemini, openai, openai_responses,
};
pub(crate) use kage_session::{
    EntryId, FORMAT_VERSION, Header, SessionId, SessionSummary, SessionWriter,
};
pub(crate) use kage_tools::builtin_registry;

pub(crate) use crate::plugins::setup_runtime;

/// kage: a minimal, extensible coding agent.
#[derive(Parser, Debug)]
#[command(name = "kage", version, about, long_about = None)]
struct Cli {
    /// Subcommand. With no subcommand, `-p/--print` runs print mode;
    /// with neither, the interactive TUI opens.
    #[command(subcommand)]
    command: Option<Command>,

    /// Run a single prompt through the agent loop and stream the response
    /// to stdout.
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

    /// Emit one JSON object per event on stdout instead of plain text.
    /// Only meaningful with `-p/--print`. Each line carries the event's
    /// `type` (such as `text_delta`, `tool_call_start`, `message_appended`,
    /// or `run_ended`) plus the `session` it belongs to and a per-session
    /// `seq` number, so external tools can parse the agent's output
    /// without screen-scraping.
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
        /// Emit one JSON object per event on stdout instead of plain
        /// text. Same format as the top-level `--json` flag.
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
    /// editor (Zed, Neovim, ...) can drive kage. Messages are
    /// newline-delimited JSON. Each request is answered, and loop
    /// progress is streamed back as ACP `session/update` notifications.
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
            AuthAction::Login { provider } => {
                let config = match kage_core::config::Config::load_default() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("kage: config: {e}; custom providers unavailable for login");
                        kage_core::config::Config::default()
                    }
                };
                auth::run_login(provider.as_deref(), &config)
            }
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

/// `-p/--print` belongs to print mode, which only runs without a
/// subcommand; a `-p` before a subcommand is always a usage error.
fn subcommand_print_conflict(cli: &Cli) -> Option<ExitCode> {
    (cli.command.is_some() && cli.print.is_some()).then(|| {
        eprintln!(
            "kage: -p/--print cannot be combined with a subcommand; \
             pass it after the subcommand (e.g. `kage resume -p ...`) or drop it"
        );
        ExitCode::from(2)
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(code) = subcommand_print_conflict(&cli) {
        return code;
    }

    if let Some(command) = cli.command {
        return run_subcommand(command);
    }

    if cli.print.is_some() {
        return run_print_mode(cli);
    }

    // No subcommand and no `-p`: drop into the interactive TUI.
    tui::run_tui(cli.model.as_deref(), &cli.system)
}

/// One `-p` print-mode run: provider and tool setup, permission gate,
/// session recording, and the exit code.
fn run_print_mode(cli: Cli) -> ExitCode {
    let Some(prompt) = cli.print else {
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

    let model = cli.model.unwrap_or_else(|| default_model(&registry));
    if !has_usable_provider(&registry) && registry.resolve(&model).is_err() {
        eprintln!("{NO_CREDENTIALS_MESSAGE}");
        return ExitCode::from(1);
    }
    if model.is_empty() {
        eprintln!("{NO_MODEL_MESSAGE}");
        return ExitCode::from(1);
    }
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
    let (mcp_manager, mcp_errors) =
        mcp::spawn_and_register(&mut tools, &workdir, plugin_runtime.as_deref());
    for (server, err) in mcp_errors {
        eprintln!("kage: mcp `{server}`: {err}");
    }
    // Layered config for the path-confinement flag; the permission
    // gate itself (and its validation) is built inside
    // `execute_print_run` from the same layered load.
    let app_config = kage_core::config::Config::load_layered(&workdir).unwrap_or_else(|e| {
        eprintln!("kage: {e}; using defaults");
        kage_core::config::Config::default()
    });
    let mut cx = AgentContext::new(resolved.model.clone(), &system_prompt).with_workdir(&workdir);
    if app_config.permissions.confine_paths {
        cx = cx.with_confine_paths();
    }
    if let Some(window) = runtime_env::context_window_for(&registry, &model) {
        cx = cx.with_context_window(window);
    }
    if let Some(out) = runtime_env::max_output_tokens_for(&registry, &model) {
        cx = cx.with_max_output_tokens(out);
    }
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
        Arc::new(registry),
        &model,
        tools,
        cx,
        prompt,
        writer,
        plugin_runtime,
        Some(mcp_manager),
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
mod sigint;

pub(crate) use cli_loop_run::execute_print_run;
pub(crate) use cli_printing::{print_envelope_json, print_event};
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

/// Resolve the plugin directory: `[plugins] dir` from the user config
/// when set (path semantics in [`resolve_plugin_dir`]), else the XDG
/// default `$XDG_CONFIG_HOME/kage/plugins` (default `~/.config/kage/plugins`).
pub(crate) fn plugins_dir() -> Result<PathBuf, String> {
    if let Ok(cfg) = kage_core::config::Config::load_default()
        && let Some(dir) = cfg.plugins.dir
    {
        return Ok(resolve_plugin_dir(dir));
    }
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?
        .join("kage")
        .join("plugins"))
}

/// Apply `[plugins] dir` path semantics: absolute paths as-is, `~`
/// expanded to the home directory, relative paths resolved against the
/// kage config directory (`~/.config/kage`). Pure so tests need no
/// environment isolation.
fn resolve_plugin_dir(dir: PathBuf) -> PathBuf {
    if dir.is_absolute() {
        return dir;
    }
    if let Ok(rest) = dir.strip_prefix("~")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    match xdg_dir("XDG_CONFIG_HOME", ".config") {
        Ok(base) => base.join("kage").join(dir),
        Err(_) => dir,
    }
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

/// Builtin provider ids `kage -m <id>:<model>` can address directly.
/// `acp` is listed because it is a valid `-m` prefix, but it is not
/// overridable: its configuration lives under `[acp.*]`.
pub(crate) const BUILTIN_PROVIDER_IDS: &[&str] =
    &["acp", "anthropic", "gemini", "openai", "openai-responses"];

/// Provider ids whose `[providers.<id>]` override kage honours: every
/// builtin except `acp`, plus each OpenAI-compatible catalog entry.
fn overridable_provider_ids() -> Vec<&'static str> {
    let mut ids: Vec<&'static str> = BUILTIN_PROVIDER_IDS
        .iter()
        .copied()
        .filter(|id| *id != "acp")
        .collect();
    ids.extend(compat::COMPAT_PROVIDERS.iter().map(|entry| entry.id));
    ids
}

/// Build a registry holding every configured provider: builtins and
/// catalog entries whose API key is reachable through either an env var
/// (priority) or the saved auth store, with `[providers.<id>]`
/// overrides for base URL and extra headers, plus every custom
/// provider declared under `[providers.custom.*]`.
///
/// A config that fails `[providers]` validation is a hard error: the
/// message prints and the process exits with status 1 rather than
/// silently running against a subset of the declared providers.
pub(crate) fn build_provider_registry() -> ProviderRegistry {
    let config = match kage_core::config::Config::load_default() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kage: config: {e}; using defaults");
            kage_core::config::Config::default()
        }
    };
    if let Err(e) = config
        .providers
        .validate(BUILTIN_PROVIDER_IDS, &overridable_provider_ids())
    {
        eprintln!("kage: {e}");
        std::process::exit(1);
    }
    let mut store = auth::AuthStore::load().unwrap_or_else(|_| auth::AuthStore::empty());
    let mut store_dirty = false;
    refresh_expiring_oauth(&mut store, &mut store_dirty);
    if store_dirty && let Err(err) = store.save() {
        eprintln!("kage: persist refreshed credentials: {err}");
    }
    let mut registry = ProviderRegistry::new();
    register_openai_family(&config, &store, &mut registry);
    register_compat_providers(&config, &store, &mut registry);
    register_custom_providers(&config, &store, &mut registry);
    let ov = config.providers.overrides.get("anthropic");
    let env = ov
        .and_then(|o| o.api_key_env.as_deref())
        .unwrap_or_else(|| auth::env_var_for("anthropic"));
    if let Some(key) = lookup_key_with_env("anthropic", env, &store) {
        let mut provider = match ov.and_then(|o| o.base_url.clone()) {
            Some(base) => anthropic::AnthropicProvider::with_base_url(key, base),
            None => anthropic::AnthropicProvider::new(key),
        };
        if let Some(o) = ov
            && !o.headers.is_empty()
        {
            provider = provider.with_extra_headers(o.headers.clone());
        }
        registry.register(Arc::new(provider));
    }
    let ov = config.providers.overrides.get("gemini");
    let env = ov
        .and_then(|o| o.api_key_env.as_deref())
        .unwrap_or_else(|| auth::env_var_for("gemini"));
    if let Some(key) = lookup_key_with_env("gemini", env, &store) {
        let mut provider = match ov.and_then(|o| o.base_url.clone()) {
            Some(base) => gemini::GeminiProvider::with_base_url(key, base),
            None => gemini::GeminiProvider::new(key),
        };
        if let Some(o) = ov
            && !o.headers.is_empty()
        {
            provider = provider.with_extra_headers(o.headers.clone());
        }
        registry.register(Arc::new(provider));
    }
    // The `acp` provider: `kage -m acp:<name>` drives an external ACP
    // agent declared in `[acp.agents.*]` or via `kage.acp.add_agent`.
    // Always registered (plugin-declared agents are resolved lazily);
    // its permission resolver defers to `kage.on_acp_permission` and
    // denies otherwise.
    registry.register(Arc::new(
        kage_acp::client::AcpProvider::from_config(&config.acp)
            .with_permission(acp_glue::permission_resolver())
            .with_agent_source(acp_glue::agent_source()),
    ));
    registry
}

/// Register the `openai` and `openai-responses` providers, which
/// share one credential: `kage auth login openai` stores a single
/// entry both use.
fn register_openai_family(
    config: &kage_core::config::Config,
    store: &auth::AuthStore,
    registry: &mut ProviderRegistry,
) {
    let ov = config.providers.overrides.get("openai");
    let env = ov
        .and_then(|o| o.api_key_env.as_deref())
        .unwrap_or_else(|| auth::env_var_for("openai"));
    let Some(key) = lookup_key_with_env("openai", env, store) else {
        return;
    };
    let mut provider = match ov.and_then(|o| o.base_url.clone()) {
        Some(base) => openai::OpenAiProvider::with_base_url(&key, base),
        None => openai::OpenAiProvider::new(&key),
    };
    if let Some(o) = ov
        && !o.headers.is_empty()
    {
        provider = provider.with_extra_headers(o.headers.clone());
    }
    registry.register(Arc::new(provider));
    // The Responses API shares OpenAI auth: any user with an OpenAI
    // key automatically gets `openai-responses:` model addressing. An
    // `api_key_env` on the `openai-responses` override redirects just
    // this provider's env lookup; otherwise the OpenAI key is reused.
    let rov = config.providers.overrides.get("openai-responses");
    let response_key = rov
        .and_then(|o| o.api_key_env.as_deref())
        .filter(|env| !env.is_empty())
        .and_then(|env| lookup_key_with_env("openai-responses", env, store))
        .unwrap_or_else(|| key.clone());
    let mut responses = match rov.and_then(|o| o.base_url.clone()) {
        Some(base) => openai_responses::OpenAiResponsesProvider::with_base_url(response_key, base),
        None => openai_responses::OpenAiResponsesProvider::new(response_key),
    };
    if let Some(o) = rov
        && !o.headers.is_empty()
    {
        responses = responses.with_extra_headers(o.headers.clone());
    }
    registry.register(Arc::new(responses));
}

/// Register each OpenAI-compatible catalog entry the user has a key
/// for. Every entry is described once in `compat::COMPAT_PROVIDERS`;
/// adding a provider is a single table entry there.
fn register_compat_providers(
    config: &kage_core::config::Config,
    store: &auth::AuthStore,
    registry: &mut ProviderRegistry,
) {
    for entry in compat::COMPAT_PROVIDERS {
        let ov = config.providers.overrides.get(entry.id);
        let env = ov
            .and_then(|o| o.api_key_env.as_deref())
            .unwrap_or_else(|| auth::env_var_for(entry.id));
        if let Some(key) = lookup_key_with_env(entry.id, env, store) {
            let mut provider = match ov.and_then(|o| o.base_url.clone()) {
                Some(base) => entry.build_with_base_url(key, base),
                None => entry.build(key),
            };
            if let Some(o) = ov
                && !o.headers.is_empty()
            {
                provider = provider.with_extra_headers(o.headers.clone());
            }
            registry.register(Arc::new(provider));
        }
    }
}

/// Register every custom provider declared under
/// `[providers.custom.<id>]`. A provider with an explicitly empty
/// `api_key_env` needs no key at all (local gateways); any other
/// missing key skips registration.
fn register_custom_providers(
    config: &kage_core::config::Config,
    store: &auth::AuthStore,
    registry: &mut ProviderRegistry,
) {
    for (id, cfg) in &config.providers.custom {
        let env = cfg
            .api_key_env
            .clone()
            .unwrap_or_else(|| format!("{}_API_KEY", id.to_uppercase()));
        let key = if env.is_empty() {
            String::new()
        } else {
            match lookup_key_with_env(id, &env, store) {
                Some(key) => key,
                None => continue,
            }
        };
        let metadata = kage_provider::ProviderMetadata {
            id: id.clone(),
            display_name: cfg.display_name.clone().unwrap_or_else(|| id.clone()),
            supports_caching: cfg.caching,
            supports_thinking: cfg.thinking,
            supports_tool_use: cfg.tool_use,
        };
        let models: Vec<kage_provider::ProviderModel> = cfg
            .models
            .iter()
            .map(|m| kage_provider::ProviderModel {
                id: m.id.clone(),
                name: m.name.clone(),
                context: m.context,
                max_output: m.max_output,
            })
            .collect();
        let provider: Arc<dyn kage_provider::Provider> = match cfg.kind {
            kage_core::config::CustomProviderKind::OpenAi => Arc::new(
                openai::OpenAiProvider::compatible(key, cfg.base_url.clone(), metadata)
                    .with_extra_headers(cfg.headers.clone())
                    .with_models(models),
            ),
            kage_core::config::CustomProviderKind::Anthropic => Arc::new(
                anthropic::AnthropicProvider::with_base_url(key, cfg.base_url.clone())
                    .with_extra_headers(cfg.headers.clone())
                    .with_models(models),
            ),
            kage_core::config::CustomProviderKind::Gemini => Arc::new(
                gemini::GeminiProvider::with_base_url(key, cfg.base_url.clone())
                    .with_extra_headers(cfg.headers.clone())
                    .with_models(models),
            ),
        };
        registry.register(provider);
    }
}

/// Look up `provider`'s bearer credential from `env_var` (when
/// non-empty and set), falling back to the auth store. Returns the API
/// key string for [`auth::Credential::ApiKey`] entries and the access
/// token for [`auth::Credential::Oauth`] entries; the refresh path in
/// [`build_provider_registry`] runs before this is called so the
/// returned token is fresh. `env_var` defaults to
/// [`auth::env_var_for`]'s name for the provider; `[providers.<id>]`
/// `api_key_env` overrides can redirect the lookup.
pub(crate) fn lookup_key_with_env(
    provider: &str,
    env_var: &str,
    store: &auth::AuthStore,
) -> Option<String> {
    if !env_var.is_empty() {
        if let Ok(v) = std::env::var(env_var) {
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

/// Printed when no provider other than `acp` is registered and the
/// requested model does not resolve.
pub(crate) const NO_CREDENTIALS_MESSAGE: &str = "kage: no provider credentials found. \
    Run `kage auth login` to save one, or export an env var (ANTHROPIC_API_KEY, \
    OPENAI_API_KEY, GEMINI_API_KEY, ZAI_API_KEY, ZAI_CODING_API_KEY).";

/// Printed when no model was requested and none could be picked.
pub(crate) const NO_MODEL_MESSAGE: &str =
    "kage: no model configured. Set [provider] default_model or pass -m provider:model";

/// Whether any provider other than the always-registered `acp` provider
/// is available, meaning some credential or custom provider is wired up.
pub(crate) fn has_usable_provider(registry: &ProviderRegistry) -> bool {
    registry.ids().any(|id| id != "acp")
}

/// Pick a sensible default model. A configured `[provider] default_model`
/// that still resolves (its provider has credentials) wins; otherwise the
/// last model the user successfully ran (when it still resolves), then
/// [`fallback_model`]. Returns an empty string when nothing is wired up.
pub(crate) fn default_model(registry: &ProviderRegistry) -> String {
    if let Ok(cfg) = kage_core::config::Config::load_default()
        && registry.resolve(&cfg.provider.default_model).is_ok()
    {
        return cfg.provider.default_model;
    }
    if let Some(model) = state::State::load().last_model
        && registry.resolve(&model).is_ok()
    {
        return model;
    }
    fallback_model(registry)
}

/// Walk [`DEFAULT_MODEL_PRIORITY`], asking the catalog for each registered
/// provider's preferred model, then take the first declared model of the
/// first non-`acp` provider (by id) that declares any. Returns an empty
/// string when neither yields a model.
fn fallback_model(registry: &ProviderRegistry) -> String {
    for candidate in DEFAULT_MODEL_PRIORITY {
        if registry.get(candidate).is_none() {
            continue;
        }
        if let Some(model) = kage_provider::catalog::preferred_model(candidate) {
            return format!("{candidate}:{}", model.id);
        }
    }
    let mut ids: Vec<&str> = registry.ids().filter(|id| *id != "acp").collect();
    ids.sort_unstable();
    for id in ids {
        if let Some(model) = registry.get(id).and_then(|p| p.models().into_iter().next()) {
            return format!("{id}:{}", model.id);
        }
    }
    String::new()
}

/// Notice for a `[provider] default_model` the user set explicitly that
/// does not resolve, naming `using` as the model picked instead. `None`
/// when the key was not set by the user or it resolves.
pub(crate) fn default_model_notice(registry: &ProviderRegistry, using: &str) -> Option<String> {
    let explicit = std::env::var_os("KAGE_PROVIDER__DEFAULT_MODEL").is_some()
        || kage_core::config::Config::default_path()
            .is_some_and(|path| config_sets_default_model(&path));
    if !explicit {
        return None;
    }
    let configured = kage_core::config::Config::load_default()
        .ok()?
        .provider
        .default_model;
    if registry.resolve(&configured).is_ok() {
        return None;
    }
    let provider = configured
        .split_once(':')
        .map_or(configured.as_str(), |(p, _)| p);
    Some(format!(
        "default_model `{configured}` is unavailable (no credentials for `{provider}`). \
         Using `{using}`. Run /login {provider} to connect it."
    ))
}

/// Whether the TOML file at `path` sets `[provider] default_model`.
fn config_sets_default_model(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .is_some_and(|table| {
            table
                .get("provider")
                .and_then(toml::Value::as_table)
                .is_some_and(|provider| provider.contains_key("default_model"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_dir_override_absolute_asis() {
        let p = resolve_plugin_dir(PathBuf::from("/opt/kage-plugins"));
        assert_eq!(p, PathBuf::from("/opt/kage-plugins"));
    }

    #[test]
    fn plugin_dir_override_tilde_expands_home() {
        let p = resolve_plugin_dir(PathBuf::from("~/my-plugins"));
        let home = dirs::home_dir().expect("test needs a home directory");
        assert_eq!(p, home.join("my-plugins"));
    }

    #[test]
    fn plugin_dir_override_relative_resolves_against_config_dir() {
        let p = resolve_plugin_dir(PathBuf::from("extra-plugins"));
        let base = xdg_dir("XDG_CONFIG_HOME", ".config").expect("test needs a home directory");
        assert_eq!(p, base.join("kage").join("extra-plugins"));
    }

    #[derive(Debug)]
    struct StubProvider {
        meta: kage_provider::ProviderMetadata,
        models: Vec<kage_provider::ProviderModel>,
    }

    impl kage_provider::Provider for StubProvider {
        fn metadata(&self) -> &kage_provider::ProviderMetadata {
            &self.meta
        }

        fn stream(
            &self,
            _req: kage_provider::StreamRequest,
            _cancel: &kage_core::CancelFlag,
        ) -> Result<kage_provider::EventStream, kage_provider::ProviderError> {
            Ok(Box::new(std::iter::empty()))
        }

        fn models(&self) -> Vec<kage_provider::ProviderModel> {
            self.models.clone()
        }
    }

    fn stub(id: &str, models: &[&str]) -> Arc<dyn kage_provider::Provider> {
        Arc::new(StubProvider {
            meta: kage_provider::ProviderMetadata {
                id: id.to_owned(),
                display_name: id.to_owned(),
                supports_caching: false,
                supports_thinking: false,
                supports_tool_use: true,
            },
            models: models
                .iter()
                .map(|m| kage_provider::ProviderModel {
                    id: (*m).to_owned(),
                    name: (*m).to_owned(),
                    context: None,
                    max_output: None,
                })
                .collect(),
        })
    }

    #[test]
    fn acp_only_registry_has_no_usable_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(stub("acp", &[]));
        assert!(!has_usable_provider(&registry));
        registry.register(stub("custom", &[]));
        assert!(has_usable_provider(&registry));
    }

    #[test]
    fn fallback_model_uses_first_declared_custom_model() {
        let registry = ProviderRegistry::new()
            .with(stub("acp", &["agent"]))
            .with(stub("zeta", &["z-1"]))
            .with(stub("empty", &[]))
            .with(stub("local", &["llama-3", "qwen"]));
        assert_eq!(fallback_model(&registry), "local:llama-3");
    }

    #[test]
    fn fallback_model_is_empty_without_models() {
        let registry = ProviderRegistry::new().with(stub("acp", &["agent"]));
        assert_eq!(fallback_model(&registry), "");
    }

    #[test]
    fn detects_explicit_default_model_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[provider]\ndefault_model = \"openai:gpt-4o\"\n").unwrap();
        assert!(config_sets_default_model(&path));
        std::fs::write(&path, "[ui]\ntheme = \"dark\"\n[provider]\n").unwrap();
        assert!(!config_sets_default_model(&path));
        assert!(!config_sets_default_model(&dir.path().join("missing.toml")));
    }
}
