//! kage CLI binary.
//!
//! Print mode (`-p`) runs a single prompt through the agent loop and streams
//! the assistant's text to stdout, then exits. The `list` subcommand prints
//! a table of recorded sessions stored under
//! `$XDG_DATA_HOME/kage/sessions/` (default `~/.local/share/kage/sessions/`).

mod auth;
mod doctor;
mod history;
mod init;
mod oauth;
mod plugins;
mod runtime_env;
mod session;
mod state;
mod tui;
mod usage_hooks;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use clap::{Parser, Subcommand};
use kage_core::{CancelFlag, Content, LoopEvent, Message, Role};
use kage_loop::{AgentContext, Hooks, LoopConfig, NoopHooks, run};
use kage_provider::{ProviderRegistry, anthropic, compat, gemini, openai, openai_responses};
use kage_session::{EntryId, FORMAT_VERSION, Header, SessionId, SessionSummary, SessionWriter};
use kage_tools::builtin_registry;

use crate::plugins::{PluginEventHooks, setup_runtime};
use crate::session::SessionRecordingHooks;

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

/// Dispatch a parsed subcommand. Lives outside [`main`] so its body
/// doesn't push the entry point past clippy's `too_many_lines`
/// threshold; the print-mode path below still runs from [`main`]
/// directly because it needs every local in scope.
fn run_subcommand(command: Command) -> ExitCode {
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
    }
}

/// Print a shell completion script for `kage` to stdout. The script
/// is generated fresh from the clap definition every invocation, so
/// adding or renaming a subcommand requires no extra checked-in
/// artifacts.
fn run_completions(shell: clap_complete::Shell) -> ExitCode {
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
fn run_gen_manpage(out: &std::path::Path) -> ExitCode {
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
    // Plugins load against a placeholder prompt (no skills yet) so they
    // can contribute skill directories via `resources_discover`. The
    // real system prompt is rebuilt below once skills are loaded.
    let bare_prompt = runtime_env::build_system_prompt(&cli.system, &workdir, &model, &[]);
    let plugin_runtime = match plugins_dir() {
        Ok(dir) => match setup_runtime(&dir, &workdir, &model, &bare_prompt) {
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
    let skills = load_skills(&workdir, plugin_runtime.as_deref());
    let system_prompt = runtime_env::build_system_prompt(&cli.system, &workdir, &model, &skills);

    let mut tools = builtin_registry();
    if let Some(rt) = plugin_runtime.as_ref() {
        apply_plugin_tools(&mut tools, rt);
    }
    let mut cx = AgentContext::new(resolved.model.clone(), &system_prompt).with_workdir(&workdir);
    if let Some(window) = runtime_env::context_window_for(&model) {
        cx = cx.with_context_window(window);
    }
    if let Some(out) = runtime_env::max_output_tokens_for(&model) {
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
    json_mode: bool,
) -> ExitCode {
    let cfg = LoopConfig::default();
    let cancel = CancelFlag::new();
    let mut stdout = io::stdout().lock();
    let result = run_with_hooks(
        provider,
        tools,
        cx,
        cfg,
        &cancel,
        user_msg,
        writer,
        plugin_runtime,
        |event| {
            if json_mode {
                print_event_json(&mut stdout, &event);
            } else {
                print_event(&mut stdout, &event);
            }
        },
    );
    if !json_mode {
        let _ = writeln!(stdout);
    }
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
    cfg: LoopConfig,
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
            if let Some(rt) = plugin_runtime.as_ref() {
                hooks = hooks.with_plugin_runtime(Arc::clone(rt));
            }
            hooks.record_user_message(user_msg);
            Box::new(hooks)
        }
    };

    if let Some(runtime) = plugin_runtime {
        let mut hooks = PluginEventHooks::new(BoxedHooks(session_layer), runtime.clone());
        hooks.dispatch_before_agent_start(&cx.system_prompt, &first_user_text(user_msg));
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

/// Extract the first text block from a user message, joined with newlines
/// if there are multiple. Returns an empty string when the message carries
/// no text (image-only, tool-result-only, etc.).
fn first_user_text(msg: &Message) -> String {
    let mut out = String::new();
    for block in &msg.content {
        if let Content::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
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

    fn transform_context(&mut self, messages: &mut Vec<kage_core::Message>) -> Result<(), String> {
        self.0.transform_context(messages)
    }

    fn transform_provider_request(
        &mut self,
        req: &mut kage_loop::StreamRequest,
    ) -> Result<(), String> {
        self.0.transform_provider_request(req)
    }

    fn on_turn_start(&mut self, index: u32) {
        self.0.on_turn_start(index);
    }

    fn on_turn_end(&mut self, index: u32, had_tool_calls: bool) {
        self.0.on_turn_end(index, had_tool_calls);
    }

    fn should_stop_after_turn(&mut self, summary: &kage_loop::TurnSummary) -> bool {
        self.0.should_stop_after_turn(summary)
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
#[allow(clippy::too_many_lines)]
fn run_resume(
    id: Option<&str>,
    last: bool,
    print: Option<&str>,
    model_override: Option<&str>,
    json: bool,
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
        apply_plugin_tools(&mut tools, rt);
    }
    let mut cx = AgentContext::new(resolved.model.clone(), &replay.header.system_prompt)
        .with_workdir(&workdir);
    if let Some(window) = runtime_env::context_window_for(&model) {
        cx = cx.with_context_window(window);
    }
    if let Some(out) = runtime_env::max_output_tokens_for(&model) {
        cx = cx.with_max_output_tokens(out);
    }
    cx.history = replay.history;
    cx.budget.used_input = replay.usage_total.input;
    cx.budget.used_output = replay.usage_total.output;
    cx.budget.used_cache_read = replay.usage_total.cache_read;
    cx.budget.used_cache_write = replay.usage_total.cache_write;
    cx.budget.current_context = replay.usage_total.last_context;
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
        json,
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
pub(crate) fn sessions_dir() -> Result<PathBuf, String> {
    Ok(data_root()?.join("sessions"))
}

/// Apply a plugin runtime's registered + overridden tools to `tools`.
/// Plain registrations land first, then overrides; an override that
/// names a tool not present after the first pass logs a warning to
/// stderr (headless mode only - the TUI surfaces the same message
/// through its plugin error channel).
fn apply_plugin_tools(tools: &mut kage_tools::ToolRegistry, rt: &kage_plugin::PluginRuntime) {
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
fn plugins_dir() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?
        .join("kage")
        .join("plugins"))
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

fn build_session_path(dir: &std::path::Path, session: SessionId) -> PathBuf {
    dir.join(format!("{session}.jsonl"))
}

/// Build a registry holding every provider whose API key is reachable
/// through either an env var (priority) or the saved auth store.
fn build_provider_registry() -> ProviderRegistry {
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

/// Look up `provider`'s bearer credential, preferring the env var
/// declared by [`auth::env_var_for`] and falling back to the auth
/// store. Returns the API key string for [`auth::Credential::ApiKey`]
/// entries and the access token for [`auth::Credential::Oauth`]
/// entries; the refresh path in [`build_provider_registry`] runs
/// before this is called so the returned token is fresh.
fn lookup_key(provider: &str, store: &auth::AuthStore) -> Option<String> {
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
fn refresh_expiring_oauth(store: &mut auth::AuthStore, dirty: &mut bool) {
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

/// Emit `event` as one JSONL row on `out`. Skips the trailing newline
/// the text-mode path adds because each event already terminates with
/// `\n`, so consumers can split on `\n` and run `serde_json::from_str`
/// on each line. Serialization can only fail on cycle errors, which
/// our event types can't produce; we still flush so streaming
/// consumers see the row immediately.
fn print_event_json<W: Write>(out: &mut W, event: &LoopEvent) {
    match serde_json::to_string(event) {
        Ok(line) => {
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
        Err(err) => {
            let _ = writeln!(
                out,
                r#"{{"type":"error","kind":{{"kind":"other","message":"encode: {err}"}}}}"#
            );
            let _ = out.flush();
        }
    }
}

#[cfg(test)]
mod json_print_tests {
    use kage_core::{MessageId, TokenUsage};

    use super::*;

    #[test]
    fn text_delta_renders_as_single_jsonl_row() {
        let mut buf = Vec::new();
        print_event_json(
            &mut buf,
            &LoopEvent::TextDelta {
                id: MessageId::new(),
                delta: "hi".into(),
            },
        );
        let line = String::from_utf8(buf).unwrap();
        assert!(line.ends_with('\n'));
        let trimmed = line.trim_end();
        // Body is one JSON value per line.
        let parsed: serde_json::Value = serde_json::from_str(trimmed).unwrap();
        assert_eq!(parsed["type"], "text_delta");
        assert_eq!(parsed["delta"], "hi");
    }

    #[test]
    fn message_end_carries_usage_through_jsonl() {
        let mut buf = Vec::new();
        print_event_json(
            &mut buf,
            &LoopEvent::MessageEnd {
                id: MessageId::new(),
                usage: TokenUsage {
                    input: 12,
                    output: 7,
                    ..TokenUsage::default()
                },
            },
        );
        let line = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["type"], "message_end");
        assert_eq!(parsed["usage"]["input"], 12);
        assert_eq!(parsed["usage"]["output"], 7);
    }
}
