//! `cargo xtask` - workspace housekeeping commands.
//!
//! Currently exposes one subcommand:
//!
//! * `refresh-models`: fetch `https://models.dev/api.json`, curate the
//!   subset kage needs, and rewrite
//!   `crates/kage-provider/src/catalog/generated.rs`. This is run by
//!   maintainers; `cargo build` itself is offline.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Deserialize;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Provider ids kage carries `Provider` impls for. The catalog is
/// pruned to just these so the committed `generated.rs` stays small;
/// adding a new provider impl means appending its id here and re-running
/// `cargo xtask refresh-models`.
const SUPPORTED_PROVIDERS: &[ProviderMap] = &[
    ProviderMap::same("anthropic"),
    ProviderMap::same("openai"),
    ProviderMap::same("zai"),
    ProviderMap::same("zai-coding-plan"),
    ProviderMap::same("deepseek"),
    ProviderMap::same("groq"),
    ProviderMap::same("mistral"),
    ProviderMap::same("cerebras"),
    ProviderMap::same("xai"),
    ProviderMap::same("openrouter"),
    ProviderMap::same("fireworks-ai"),
    ProviderMap::same("moonshotai"),
    ProviderMap::same("kimi-for-coding"),
    // models.dev calls Google's API "google" but kage's Provider impl
    // is registered under "gemini".
    ProviderMap {
        api_id: "google",
        kage_id: "gemini",
    },
];

#[derive(Clone, Copy)]
struct ProviderMap {
    api_id: &'static str,
    kage_id: &'static str,
}

impl ProviderMap {
    const fn same(id: &'static str) -> Self {
        Self {
            api_id: id,
            kage_id: id,
        }
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Re-fetch and regenerate the provider/model catalog.
    RefreshModels {
        /// Override the upstream URL (handy for offline / fixture testing).
        #[arg(long, default_value = MODELS_DEV_URL)]
        source: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::RefreshModels { source } => match refresh_models(&source) {
            Ok(path) => {
                eprintln!("xtask: wrote {}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("xtask: refresh-models failed: {e}");
                ExitCode::from(1)
            }
        },
    }
}

#[derive(Deserialize)]
struct ApiProvider {
    name: String,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    models: BTreeMap<String, ApiModel>,
}

#[derive(Deserialize)]
struct ApiModel {
    id: String,
    name: String,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    limit: Option<ApiLimit>,
    #[serde(default)]
    cost: Option<ApiCost>,
}

#[derive(Deserialize)]
struct ApiLimit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

/// Per-million-token pricing the upstream catalog reports. All four
/// fields are USD per million tokens; `cache_read` and `cache_write`
/// may be absent for providers that don't price the prompt cache
/// separately.
#[derive(Deserialize)]
struct ApiCost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

fn refresh_models(source: &str) -> Result<PathBuf, String> {
    let raw = fetch(source)?;
    let api: BTreeMap<String, ApiProvider> =
        serde_json::from_str(&raw).map_err(|e| format!("parse {source}: {e}"))?;

    let mut curated: Vec<CuratedProvider> = Vec::new();
    for map in SUPPORTED_PROVIDERS {
        let raw = api
            .get(map.api_id)
            .ok_or_else(|| format!("upstream missing provider '{}'", map.api_id))?;
        curated.push(CuratedProvider::from(raw, map.kage_id));
    }

    let dest = workspace_root().join("crates/kage-provider/src/catalog/generated.rs");
    let rendered = render(&curated);
    fs::write(&dest, rendered).map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(dest)
}

fn fetch(url: &str) -> Result<String, String> {
    if let Some(path) = url.strip_prefix("file://") {
        return fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"));
    }
    let agent = ureq::Agent::new_with_defaults();
    let response = agent
        .get(url)
        .header("user-agent", "kage-xtask/0.1")
        .call()
        .map_err(|e| format!("get {url}: {e}"))?;
    let mut body = String::new();
    response
        .into_body()
        .into_reader()
        .take(8 * 1024 * 1024)
        .read_to_string(&mut body)
        .map_err(|e| format!("read body: {e}"))?;
    Ok(body)
}

struct CuratedProvider {
    kage_id: String,
    name: String,
    api: Option<String>,
    env: Vec<String>,
    models: Vec<CuratedModel>,
}

impl CuratedProvider {
    fn from(api: &ApiProvider, kage_id: &str) -> Self {
        let mut models: Vec<CuratedModel> = api
            .models
            .values()
            .filter(|m| m.tool_call)
            .map(CuratedModel::from)
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Self {
            kage_id: kage_id.to_owned(),
            name: api.name.clone(),
            api: api.api.clone(),
            env: api.env.clone(),
            models,
        }
    }
}

struct CuratedModel {
    id: String,
    name: String,
    context: Option<u64>,
    output: Option<u64>,
    reasoning: bool,
    release_date: Option<String>,
    cost: Option<CuratedCost>,
}

/// Normalized per-million-token pricing emitted into `generated.rs`.
struct CuratedCost {
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

impl CuratedModel {
    fn from(m: &ApiModel) -> Self {
        let limit = m.limit.as_ref();
        let cost = m.cost.as_ref().and_then(|c| {
            // input and output are both required; if either is missing
            // there's no useful pricing to emit.
            match (c.input, c.output) {
                (Some(i), Some(o)) => Some(CuratedCost {
                    input: i,
                    output: o,
                    cache_read: c.cache_read,
                    cache_write: c.cache_write,
                }),
                _ => None,
            }
        });
        Self {
            id: m.id.clone(),
            name: m.name.clone(),
            context: limit.and_then(|l| l.context),
            output: limit.and_then(|l| l.output),
            reasoning: m.reasoning,
            release_date: m.release_date.clone(),
            cost,
        }
    }
}

fn render(providers: &[CuratedProvider]) -> String {
    let mut out = String::new();
    out.push_str(
        "//! @generated by `cargo xtask refresh-models`. Do not edit by hand.\n\
         //!\n\
         //! Source: <https://models.dev/api.json>, curated to kage's supported providers.\n\
         \n\
         #![allow(clippy::unreadable_literal)]\n\
         \n\
         #[allow(unused_imports)]\n\
         use super::{ModelCost, ModelInfo, ProviderInfo};\n\
         #[allow(unused_imports)]\n\
         use crate::ThinkingLevel;\n\
         \n",
    );
    let _ = writeln!(out, "/// Static provider/model catalog.");
    let _ = writeln!(out, "pub static PROVIDERS: &[ProviderInfo] = &[");
    for p in providers {
        emit_provider(&mut out, p);
    }
    out.push_str("];\n");
    out
}

fn emit_provider(out: &mut String, p: &CuratedProvider) {
    let _ = writeln!(out, "    ProviderInfo {{");
    let _ = writeln!(out, "        id: {},", quote(&p.kage_id));
    let _ = writeln!(out, "        name: {},", quote(&p.name));
    let _ = writeln!(out, "        api: {},", opt_quote(p.api.as_deref()));
    out.push_str("        env: &[");
    for (i, e) in p.env.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&quote(e));
    }
    out.push_str("],\n");
    out.push_str("        models: &[\n");
    for m in &p.models {
        emit_model(out, m);
    }
    out.push_str("        ],\n");
    out.push_str("    },\n");
}

fn emit_model(out: &mut String, m: &CuratedModel) {
    let _ = writeln!(out, "            ModelInfo {{");
    let _ = writeln!(out, "                id: {},", quote(&m.id));
    let _ = writeln!(out, "                name: {},", quote(&m.name));
    let _ = writeln!(out, "                context: {},", opt_int(m.context));
    let _ = writeln!(out, "                output: {},", opt_int(m.output));
    let _ = writeln!(out, "                reasoning: {},", m.reasoning);
    let _ = writeln!(
        out,
        "                release_date: {},",
        opt_quote(m.release_date.as_deref())
    );
    match &m.cost {
        Some(c) => {
            let _ = writeln!(out, "                cost: Some(ModelCost {{");
            let _ = writeln!(out, "                    input: {:.6},", c.input);
            let _ = writeln!(out, "                    output: {:.6},", c.output);
            let _ = writeln!(
                out,
                "                    cache_read: {},",
                opt_float(c.cache_read)
            );
            let _ = writeln!(
                out,
                "                    cache_write: {},",
                opt_float(c.cache_write)
            );
            out.push_str("                }),\n");
        }
        None => out.push_str("                cost: None,\n"),
    }
    out.push_str("                thinking_levels: None,\n");
    out.push_str("            },\n");
}

fn opt_float(v: Option<f64>) -> String {
    match v {
        Some(n) => format!("Some({n:.6})"),
        None => "None".to_owned(),
    }
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{{{:04x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn opt_quote(s: Option<&str>) -> String {
    match s {
        Some(s) => format!("Some({})", quote(s)),
        None => "None".to_owned(),
    }
}

fn opt_int(v: Option<u64>) -> String {
    match v {
        Some(n) => format!("Some({n})"),
        None => "None".to_owned(),
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .expect("xtask is a workspace member; parent exists")
}
