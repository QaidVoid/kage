//! kage CLI binary.
//!
//! Layering: top of the workspace; depends on every library crate it uses.
//!
//! Without a subcommand kage opens the interactive TUI. Print mode (`-p`)
//! runs a single prompt through the agent loop, streams the reply to
//! stdout, and exits. Subcommands:
//!
//! - `list`, `resume`, `fork` and `search` work on the recorded sessions
//!   under `$XDG_DATA_HOME/kage/sessions/`.
//! - `auth` manages saved provider credentials, and `init` is the
//!   first-run setup wizard.
//! - `doctor` diagnoses the install, and `trust` trusts a project config.
//! - `models` manages the model catalog.
//! - `rpc` serves the Agent Client Protocol over stdio. `mcp` serves the
//!   built-in tools over the Model Context Protocol and logs in to MCP
//!   servers.
//! - `completions` and the hidden `gen-manpage` generate shell and man
//!   page files.

mod acp_glue;
mod agents;
mod auth;
mod doctor;
mod engine;
mod history;
mod init;
mod mcp;
mod mcp_auth;
mod paths;
mod permissions;
mod plugins;
mod providers;
mod rpc;
mod runtime_env;
mod state;
mod title;
mod trust;
mod tui;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use clap::{Parser, Subcommand};
use kage_loop::AgentContext;
use kage_session::{EntryId, FORMAT_VERSION, Header, SessionId, SessionSummary, SessionWriter};
use kage_tools::builtin_registry;

pub(crate) use paths::{
    cache_root, config_dir, data_root, models_cache_path, plugins_dir, sessions_dir, state_root,
    themes_dir,
};
pub(crate) use providers::{
    NO_CREDENTIALS_MESSAGE, NO_MODEL_MESSAGE, build_provider_registry, configured_default_model,
    default_model, has_usable_provider,
};

use crate::plugins::setup_runtime;

/// The system prompt role when `--system` is not given.
pub(crate) const DEFAULT_SYSTEM: &str = "You are kage, a helpful coding agent.";

/// kage: a minimal, extensible coding agent.
#[derive(Parser, Debug)]
#[command(
    name = "kage",
    version,
    about = "A minimal, extensible coding agent in your terminal",
    long_about = None
)]
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
    /// `[provider] default_model` when its provider has credentials,
    /// then the last model you used, then the first available
    /// provider's preferred model.
    #[arg(short = 'm', long = "model")]
    model: Option<String>,

    /// System prompt to prepend.
    #[arg(long = "system", default_value = DEFAULT_SYSTEM)]
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
        /// New user prompt to append in print mode. Without it the
        /// session opens in the interactive TUI.
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
    /// Print a shell completion script for `kage` to stdout.
    ///
    /// Pipe it to `source` (bash, zsh) or redirect it into your
    /// shell's completion directory (fish, elvish), as in these
    /// examples:
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
        /// the same way as the top-level `-m`.
        #[arg(short = 'm', long = "model")]
        model: Option<String>,
        /// System-prompt role override forwarded to the agent loop.
        #[arg(long = "system", default_value = "")]
        system: String,
    },
    /// Trust the current directory's `.kage/config.toml`. Until a
    /// project is trusted, its `mcp`, `permissions` and
    /// `plugins.capabilities` settings are ignored. Trust covers the
    /// values as they are now, so editing any of them asks again.
    Trust {
        /// Forget the trust recorded for this directory.
        #[arg(long)]
        revoke: bool,
    },
    /// Manage the model catalog. kage ships a snapshot of models.dev;
    /// `kage models refresh` fetches a newer one into the model cache,
    /// which later runs use in place of the snapshot's model entries.
    Models {
        /// Models sub-action.
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Model Context Protocol: expose kage's built-in tools to another
    /// agent over stdio (newline-delimited JSON-RPC) with `kage mcp
    /// serve`, or log in to remote MCP servers that need OAuth.
    Mcp {
        /// MCP sub-action.
        #[command(subcommand)]
        action: McpAction,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ModelsAction {
    /// Download the models.dev catalog into
    /// `$XDG_CACHE_HOME/kage/models.json`. It can add models and update
    /// their metadata, never provider endpoints or credentials. Only
    /// runs when asked; kage never refreshes on its own.
    Refresh,
}

#[derive(Subcommand, Debug)]
pub(crate) enum McpAction {
    /// Serve kage's built-in tools as an MCP server over stdio. Calls
    /// are checked against `[permissions]`, and a tool whose verdict is
    /// `ask` is refused because there is no one to ask.
    Serve {
        /// Comma-separated built-in tools to expose. Unknown names are
        /// an error.
        #[arg(long, value_delimiter = ',', default_value = "read,grep,find,ls")]
        tools: Vec<String>,
    },
    /// Log in to a remote MCP server with OAuth. Prints the
    /// authorization URL, opens it in a browser when one is available,
    /// and waits for the browser to return, or for the redirected URL to
    /// be pasted on a machine without one. The token is stored in
    /// `$XDG_DATA_HOME/kage/mcp-auth.json` (mode 0600).
    Login {
        /// Server name from `[mcp.servers.<name>]`.
        server: String,
    },
    /// Forget the stored OAuth token of a remote MCP server.
    Logout {
        /// Server name from `[mcp.servers.<name>]`.
        server: String,
    },
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
    /// Show which known and custom providers have credentials available.
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
        } => config_error().unwrap_or_else(|| {
            run_resume(
                id.as_deref(),
                last,
                print.as_deref(),
                model.as_deref(),
                json,
            )
        }),
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
        Command::Rpc { model, system } => {
            config_error().unwrap_or_else(|| rpc::run(model.as_deref(), &system))
        }
        Command::Trust { revoke } => trust::run(revoke),
        Command::Models {
            action: ModelsAction::Refresh,
        } => run_models_refresh(),
        Command::Mcp { action } => match action {
            McpAction::Serve { tools } => mcp::run_serve(&tools),
            McpAction::Login { server } => mcp_auth::run_login(&server),
            McpAction::Logout { server } => mcp_auth::run_logout(&server),
        },
    }
}

/// Implement `kage models refresh`: download the catalog into the model
/// cache.
fn run_models_refresh() -> ExitCode {
    let dest = match models_cache_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let url = kage_provider::catalog::source::MODELS_DEV_URL;
    match kage_provider::catalog::refresh(url, &dest) {
        Ok(count) => {
            println!("wrote {count} models to {}", dest.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kage: models refresh: {e}");
            ExitCode::from(1)
        }
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
    let buffer = match render_manpage(&Cli::command()) {
        Ok(buffer) => buffer,
        Err(err) => {
            eprintln!("kage: render manpage: {err}");
            return ExitCode::from(1);
        }
    };
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

/// Paths listed in the manpage's FILES section, with what each holds.
const MANPAGE_FILES: &[(&str, &str)] = &[
    (
        "~/.config/kage/config.toml",
        "User config, written by kage init.",
    ),
    (
        "~/.config/kage/init.lua",
        "Lua config the TUI runs after config.toml.",
    ),
    (
        "<workdir>/.kage/config.toml",
        "Project config, merged over the user config. Its mcp, permissions and plugins.capabilities tables apply only after kage trust.",
    ),
    (
        "~/.config/kage/{plugins,themes,agents,skills,templates}/",
        "Lua plugins, user themes, agent definitions, skills and prompt templates.",
    ),
    (
        "~/.local/share/kage/sessions/",
        "Recorded sessions, one JSONL file each.",
    ),
    (
        "~/.local/share/kage/auth.json",
        "Saved provider credentials (mode 0600).",
    ),
    (
        "~/.local/state/kage/",
        "Session state, input history and trusted projects (trust.json).",
    ),
    (
        "~/.cache/kage/models.json",
        "Model catalog written by kage models refresh.",
    ),
];

/// Render the manpage for `cmd`. The NAME, SYNOPSIS, OPTIONS and
/// VERSION sections come from `clap_mangen`; DESCRIPTION, COMMANDS and
/// FILES are written here so the page describes the TUI, lists the
/// subcommands as `kage <command>` rather than as separate pages that
/// are not installed, and names the files kage reads and writes.
fn render_manpage(cmd: &clap::Command) -> io::Result<String> {
    use clap_mangen::Man;
    use clap_mangen::roff::{Roff, bold, italic, roman};

    type Section = fn(&Man, &mut dyn Write) -> io::Result<()>;
    let man = Man::new(cmd.clone());
    let preamble = Roff::new().render();
    let mut page = preamble.clone();
    let mut push = |section: String| {
        page.push_str(section.strip_prefix(&preamble).unwrap_or(&section));
    };
    let render = |section: Section| -> io::Result<String> {
        let mut buffer = Vec::new();
        section(&man, &mut buffer)?;
        Ok(String::from_utf8_lossy(&buffer).into_owned())
    };
    push(render(Man::render_title)?);
    push(render(Man::render_name_section)?);
    push(render(Man::render_synopsis_section)?);

    let mut roff = Roff::new();
    roff.control("SH", ["DESCRIPTION"]);
    roff.text([
        roman("With no command and no "),
        bold("-p"),
        roman(", kage opens its interactive TUI in the current directory. Type a prompt and press enter to send it. On an empty prompt, "),
        bold("?"),
        roman(" shows the keys and "),
        bold("/"),
        roman(" opens the command palette. "),
        bold("ctrl+q"),
        roman(" quits."),
    ]);
    roff.control("PP", []);
    roff.text([
        roman("With "),
        bold("-p"),
        roman(
            ", kage runs one prompt through the agent loop, streams the reply to stdout and exits.",
        ),
    ]);
    push(roff.render());
    push(render(Man::render_options_section)?);

    let mut roff = Roff::new();
    roff.control("SH", ["COMMANDS"]);
    let listed = |c: &&clap::Command| !c.is_hide_set() && c.get_name() != "help";
    for sub in cmd.get_subcommands().filter(listed) {
        let nested = sub
            .get_subcommands()
            .filter(listed)
            .map(|n| (format!("{} {}", sub.get_name(), n.get_name()), n));
        for (name, page) in std::iter::once((sub.get_name().to_owned(), sub)).chain(nested) {
            roff.control("TP", []);
            roff.text([bold(format!("kage {name}"))]);
            if let Some(about) = page.get_about() {
                roff.text([roman(about.to_string())]);
            }
        }
    }
    roff.control("PP", []);
    roff.text([
        roman("Run "),
        bold("kage "),
        italic("command"),
        bold(" --help"),
        roman(" for the options of a command."),
    ]);

    roff.control("SH", ["FILES"]);
    for (path, what) in MANPAGE_FILES {
        roff.control("TP", []);
        roff.text([italic(*path)]);
        roff.text([roman(*what)]);
    }
    roff.control("PP", []);
    roff.text([roman(
        "XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME and XDG_CACHE_HOME replace the ~/.config, ~/.local/share, ~/.local/state and ~/.cache roots.",
    )]);
    push(roff.render());
    push(render(Man::render_version_section)?);
    Ok(page)
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
    if let Ok(path) = models_cache_path() {
        kage_provider::catalog::use_cache(&path);
    }

    if let Some(command) = cli.command {
        return run_subcommand(command);
    }
    if let Some(code) = config_error() {
        return code;
    }

    if cli.print.is_some() {
        return run_print_mode(cli);
    }

    // No subcommand and no `-p`: drop into the interactive TUI.
    tui::run_tui(cli.model.as_deref(), &cli.system, None)
}

/// Load the layered config for the current directory before a run
/// starts. A config that does not load stops kage with the error and
/// the returned exit status, because running on defaults would drop
/// custom providers and could start the first-run wizard.
fn config_error() -> Option<ExitCode> {
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let err = kage_core::config::Config::load_layered(&workdir).err()?;
    eprintln!("kage: {err}");
    Some(ExitCode::from(1))
}

/// One `-p` print-mode run: provider and tool setup, permission gate,
/// session recording, and the exit code.
fn run_print_mode(cli: Cli) -> ExitCode {
    let Some(prompt) = cli.print else {
        return tui::run_tui(cli.model.as_deref(), &cli.system, None);
    };
    let mut registry = match build_provider_registry() {
        Ok(registry) => registry,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };

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
        for id in plugins::merge_plugin_providers(rt, &mut registry) {
            eprintln!("kage: plugin provider `{id}` shadows the built-in registration");
        }
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
    let (mcp_manager, mcp_errors) =
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
pub(crate) use cli_query::{run_fork, run_resume, run_search};

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
    if let Ok(p) = config_dir() {
        search.push(p.join("skills"));
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

/// Implement `kage list`: print one row per recorded session.
pub(crate) fn run_list() -> ExitCode {
    let dir = match sessions_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let mut summaries = match kage_session::list(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kage: failed to list sessions: {e}");
            return ExitCode::from(1);
        }
    };
    summaries.retain(|s| s.agent.is_none());
    if summaries.is_empty() {
        eprintln!("kage: no sessions found in {}", dir.display());
        return ExitCode::SUCCESS;
    }
    print_session_table(&mut io::stdout().lock(), &summaries);
    ExitCode::SUCCESS
}

/// Print one row per session: short id, local creation time, model and
/// the title, or the last prompt for a session without one.
pub(crate) fn print_session_table<W: Write>(out: &mut W, summaries: &[SessionSummary]) {
    let id_h = "ID";
    let created_h = "CREATED";
    let model_h = "MODEL";
    let title_h = "TITLE";
    let _ = writeln!(out, "{id_h:<10}  {created_h:<16}  {model_h:<32}  {title_h}");
    for s in summaries {
        let id = s.id.to_string();
        let id_short: String = id.chars().take(10).collect();
        let created = s
            .created_at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let model = &s.model;
        let title = s
            .title
            .as_deref()
            .or(s.last_user_prompt.as_deref())
            .map_or_else(
                || "(untitled session)".to_owned(),
                |text| truncate_one_line(text, 60),
            );
        let _ = writeln!(out, "{id_short:<10}  {created:<16}  {model:<32}  {title}");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(title: Option<&str>, prompt: Option<&str>) -> SessionSummary {
        let created_at = Utc::now();
        SessionSummary {
            id: SessionId::new(),
            path: PathBuf::from("/s.jsonl"),
            created_at,
            updated_at: created_at,
            cwd: PathBuf::from("/p"),
            model: "mock:m".to_owned(),
            parent_session: None,
            last_user_prompt: prompt.map(str::to_owned),
            title: title.map(str::to_owned),
            entry_count: 1,
            agent: None,
        }
    }

    #[test]
    fn session_table_shows_local_time_and_prefers_the_title() {
        let rows = [
            summary(Some("Fix the parser"), Some("last prompt")),
            summary(None, Some("only a prompt\nsecond line")),
            summary(None, None),
        ];
        let mut out = Vec::new();
        print_session_table(&mut out, &rows);
        let text = String::from_utf8(out).expect("utf-8");
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].ends_with("TITLE"), "{text}");
        let local = rows[0]
            .created_at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        assert!(lines[1].contains(&local), "{text}");
        assert!(lines[1].ends_with("Fix the parser"), "{text}");
        assert!(lines[2].ends_with("only a prompt"), "{text}");
        assert!(lines[3].ends_with("(untitled session)"), "{text}");
    }

    #[test]
    fn manpage_lists_commands_inline_and_has_files() {
        use clap::CommandFactory as _;
        let page = render_manpage(&Cli::command()).unwrap();
        assert_eq!(page.matches(".ds Aq").count(), 2, "one preamble");
        assert!(!page.contains("kage \\- kage"), "NAME repeats the name");
        assert!(!page.contains("(1)"), "no references to missing pages");
        assert!(!page.contains("For example:"));
        assert!(page.contains("\\fBkage auth list\\fR"));
        assert!(page.contains(".SH FILES"));
    }
}
