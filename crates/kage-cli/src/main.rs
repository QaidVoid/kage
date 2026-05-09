//! kage CLI binary.
//!
//! Phase 4 ships only print mode (`-p`). Run a single prompt through the
//! agent loop and stream the assistant's text to stdout, then exit.

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use kage_core::{CancelFlag, Content, LoopEvent, Message, Role};
use kage_loop::{AgentContext, LoopConfig, NoopHooks, run};
use kage_provider::{ProviderRegistry, anthropic, gemini, openai, zai};
use kage_tools::builtin_registry;

/// kage: a minimal, extensible coding agent.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
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
    cx.history.push(Message::new(
        Role::User,
        vec![Content::Text { text: prompt }],
        None,
    ));

    let cfg = LoopConfig::default();
    let mut hooks = NoopHooks;
    let cancel = CancelFlag::new();

    let mut stdout = io::stdout().lock();
    let result = run(
        resolved.provider.as_ref(),
        &tools,
        &mut cx,
        &cfg,
        &mut hooks,
        &cancel,
        |event| print_event(&mut stdout, &event),
    );
    let _ = writeln!(stdout);

    // The loop emits errors as terminal `LoopEvent::Error`s, which `print_event`
    // already renders. Don't double-print on the `Err` return; just exit non-zero.
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
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
        LoopEvent::Compaction { kept, summarized } => {
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
