//! Stream a hello-world response from any configured provider.
//!
//! ```sh
//! # ZAI standard plan
//! ZAI_API_KEY=... KAGE_PROVIDER=zai \
//!   nix develop --command cargo run -p kage-provider --example hello
//!
//! # ZAI coding plan
//! ZAI_CODING_API_KEY=... KAGE_PROVIDER=zai-coding-plan \
//!   nix develop --command cargo run -p kage-provider --example hello
//!
//! # Anthropic
//! ANTHROPIC_API_KEY=... KAGE_PROVIDER=anthropic \
//!   nix develop --command cargo run -p kage-provider --example hello
//!
//! # OpenAI
//! OPENAI_API_KEY=... KAGE_PROVIDER=openai \
//!   nix develop --command cargo run -p kage-provider --example hello
//!
//! # Gemini
//! GEMINI_API_KEY=... KAGE_PROVIDER=gemini \
//!   nix develop --command cargo run -p kage-provider --example hello
//! ```
//!
//! Optional overrides:
//! - `KAGE_MODEL` to pick a specific model id (defaults are cheap).
//! - `KAGE_PROMPT` to send a custom prompt.

use std::io::Write;

use kage_core::{CancelFlag, Content, Message, Role};
use kage_provider::{
    Provider, ProviderEvent, StreamRequest, anthropic::AnthropicProvider, gemini::GeminiProvider,
    openai::OpenAiProvider,
};

fn main() {
    let provider_id = std::env::var("KAGE_PROVIDER").unwrap_or_else(|_| "anthropic".into());
    let model = std::env::var("KAGE_MODEL").unwrap_or_else(|_| default_model(&provider_id));
    let prompt =
        std::env::var("KAGE_PROMPT").unwrap_or_else(|_| "Reply with exactly the word: pong".into());

    let provider: Box<dyn Provider> = match provider_id.as_str() {
        "anthropic" => Box::new(AnthropicProvider::new(must_env("ANTHROPIC_API_KEY"))),
        "openai" => Box::new(OpenAiProvider::new(must_env("OPENAI_API_KEY"))),
        "gemini" => Box::new(GeminiProvider::new(must_env("GEMINI_API_KEY"))),
        "zai" => Box::new(kage_provider::compat::zai(must_env("ZAI_API_KEY"))),
        "zai-coding-plan" => {
            let key =
                std::env::var("ZAI_CODING_API_KEY").unwrap_or_else(|_| must_env("ZAI_API_KEY"));
            Box::new(kage_provider::compat::zai_coding_plan(key))
        }
        other => {
            eprintln!(
                "unknown KAGE_PROVIDER `{other}`; expected one of: anthropic, openai, gemini, zai, zai-coding-plan"
            );
            std::process::exit(2);
        }
    };

    eprintln!(
        "[provider={} model={} prompt={prompt:?}]",
        provider.metadata().id,
        model,
    );

    let req = StreamRequest::new(
        &model,
        vec![Message::new(
            Role::User,
            vec![Content::Text { text: prompt }],
            None,
        )],
    );

    let cancel = CancelFlag::new();
    let stream = match provider.stream(req, &cancel) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("stream open failed: {e}");
            std::process::exit(1);
        }
    };

    let mut stdout = std::io::stdout().lock();
    let mut text_started = false;
    for ev in stream {
        match ev {
            Ok(ProviderEvent::MessageStart) => {}
            Ok(ProviderEvent::TextDelta { delta }) => {
                let _ = stdout.write_all(delta.as_bytes());
                let _ = stdout.flush();
                text_started = true;
            }
            Ok(ProviderEvent::ThinkingDelta { delta }) => {
                eprint!("\x1b[2m{delta}\x1b[0m");
            }
            Ok(ProviderEvent::ToolCallStart { name, .. }) => {
                eprintln!("\n[tool_call_start] {name}(");
            }
            Ok(ProviderEvent::ToolCallArgsDelta { partial, .. }) => {
                eprint!("{partial}");
            }
            Ok(ProviderEvent::ToolCallEnd { input, .. }) => {
                eprintln!("\n) {input}");
            }
            Ok(ProviderEvent::MessageEnd { stop_reason, usage }) => {
                if text_started {
                    let _ = stdout.write_all(b"\n");
                }
                eprintln!(
                    "[end stop={stop_reason:?} in={} out={} cache_read={} cache_write={}]",
                    usage.input, usage.output, usage.cache_read, usage.cache_write,
                );
            }
            Err(e) => {
                eprintln!("\n[error] {e}");
                std::process::exit(1);
            }
        }
    }
}

fn default_model(provider: &str) -> String {
    match provider {
        "anthropic" => "claude-haiku-4-5-20251001".into(),
        "openai" => "gpt-4o-mini".into(),
        "gemini" => "gemini-2.0-flash".into(),
        "zai" => "glm-4.5-air".into(),
        "zai-coding-plan" => "glm-4.6".into(),
        _ => "model".into(),
    }
}

fn must_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("required environment variable `{name}` is not set");
        std::process::exit(2);
    })
}
