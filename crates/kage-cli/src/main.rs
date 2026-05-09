//! kage CLI binary.
//!
//! Print mode (`-p`) runs a single prompt through the agent loop and streams
//! the assistant's text to stdout, then exits. The `list` subcommand prints
//! a table of recorded sessions stored under `~/.kage/sessions/`.

mod session;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use clap::{Parser, Subcommand};
use kage_core::{CancelFlag, Content, LoopEvent, Message, Role};
use kage_loop::{AgentContext, LoopConfig, NoopHooks, run};
use kage_provider::{ProviderRegistry, anthropic, gemini, openai, zai};
use kage_session::{EntryId, FORMAT_VERSION, Header, SessionId, SessionSummary, SessionWriter};
use kage_tools::builtin_registry;

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
    /// session file under `~/.kage/sessions/<session-id>.jsonl`.
    #[arg(long = "no-session")]
    no_session: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List recorded sessions in `~/.kage/sessions/`.
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
        None => {}
    }

    let Some(prompt) = cli.print else {
        eprintln!("kage: -p/--print is required in this build");
        return ExitCode::from(2);
    };

    let registry = build_provider_registry();
    if registry.ids().count() == 0 {
        eprintln!(
            "kage: no provider API keys found in environment. Set one of \
             ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY, ZAI_API_KEY."
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

    let tools = builtin_registry();
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

    execute_print_run(
        resolved.provider.as_ref(),
        &tools,
        &mut cx,
        &user_msg,
        writer,
    )
}

/// Drive one print-mode run. Streams loop events to stdout and, when a
/// writer is supplied, records the conversation. Returns the appropriate
/// process exit code.
fn execute_print_run(
    provider: &dyn kage_provider::Provider,
    tools: &kage_tools::ToolRegistry,
    cx: &mut AgentContext,
    user_msg: &Message,
    writer: Option<SessionWriter>,
) -> ExitCode {
    let cfg = LoopConfig::default();
    let cancel = CancelFlag::new();
    let mut stdout = io::stdout().lock();
    let result = match writer {
        None => {
            let mut hooks = NoopHooks;
            run(provider, tools, cx, &cfg, &mut hooks, &cancel, |event| {
                print_event(&mut stdout, &event);
            })
        }
        Some(w) => {
            let mut hooks = SessionRecordingHooks::new(NoopHooks, w);
            hooks.record_user_message(user_msg);
            run(provider, tools, cx, &cfg, &mut hooks, &cancel, |event| {
                print_event(&mut stdout, &event);
            })
        }
    };
    let _ = writeln!(stdout);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
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
            "kage: no provider API keys found in environment. Set one of \
             ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY, ZAI_API_KEY."
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

    let tools = builtin_registry();
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

    execute_print_run(
        resolved.provider.as_ref(),
        &tools,
        &mut cx,
        &user_msg,
        Some(writer),
    )
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

/// Resolve the directory holding session files: `~/.kage/sessions/`.
fn sessions_dir() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "no home directory".to_owned())?;
    Ok(home.join(".kage").join("sessions"))
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

/// Create a fresh session file under `~/.kage/sessions/`.
fn open_session(model: &str, system_prompt: &str) -> Result<SessionWriter, String> {
    let home = dirs::home_dir().ok_or_else(|| "no home directory".to_owned())?;
    let dir = home.join(".kage").join("sessions");
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

/// Build a registry holding every provider whose API key env var is set.
fn build_provider_registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        registry.register(Arc::new(anthropic::AnthropicProvider::new(key)));
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        registry.register(Arc::new(openai::OpenAiProvider::new(key)));
    }
    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        registry.register(Arc::new(gemini::GeminiProvider::new(key)));
    }
    if let Ok(key) = std::env::var("ZAI_API_KEY") {
        registry.register(Arc::new(zai::provider(key.clone())));
        registry.register(Arc::new(zai::coding_plan(key)));
    }
    registry
}

/// Pick a sensible default model based on what providers are registered.
fn default_model(registry: &ProviderRegistry) -> String {
    if registry.get("zai").is_some() {
        return "zai:glm-4.6".to_owned();
    }
    if registry.get("anthropic").is_some() {
        return "anthropic:claude-sonnet-4-6".to_owned();
    }
    if registry.get("openai").is_some() {
        return "openai:gpt-4o-mini".to_owned();
    }
    if registry.get("gemini").is_some() {
        return "gemini:gemini-2.0-flash".to_owned();
    }
    "zai:glm-4.6".to_owned()
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
