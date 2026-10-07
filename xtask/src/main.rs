//! `cargo xtask` - workspace housekeeping commands.
//!
//! Layering: build tooling outside the runtime crate layering; depends
//! on `kage-plugin` and `kage-provider`.
//!
//! Subcommands:
//!
//! * `refresh-models`: fetch `https://models.dev/api.json`, overlay
//!   the hand-maintained `xtask/fixtures/manual.json` for providers
//!   models.dev lacks, curate the subset kage needs with
//!   `kage_provider::catalog::source`, and rewrite
//!   `crates/kage-provider/src/catalog/generated.rs`. The manual
//!   fixture is drift-checked against Command Code's live model list
//!   and synced to the official CLI catalog's thinking levels. This
//!   is run by maintainers; `cargo build` itself is offline. `--check`
//!   re-renders and diffs without writing (the CI drift gate).
//! * `gen-lua-types`: regenerate `plugins/types/kage.lua`.
//! * `check-ascii`: the ASCII-only source gate.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use kage_core::{Inputs, Reasoning};
use kage_provider::catalog::source::{
    self, MODELS_DEV_URL, SUPPORTED_PROVIDERS, SourceModel, SourceProvider,
};
use serde_json::Value;

mod ascii;
mod luatypes;

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
        /// Do not write; fail if the committed catalog is out of date
        /// (the CI drift gate).
        #[arg(long)]
        check: bool,
    },
    /// Regenerate `plugins/types/kage.lua` from the in-tree spec.
    GenLuaTypes {
        /// Do not write; fail if the committed file is out of date
        /// (the CI drift gate).
        #[arg(long)]
        check: bool,
    },
    /// Fail if any Rust source carries raw non-ASCII bytes (the CI
    /// ASCII-only gate). Unicode glyphs must use `\u{..}` escapes.
    CheckAscii,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::RefreshModels { source, check } => match refresh_models(&source, check) {
            Ok(path) => {
                let verb = if check { "up to date" } else { "wrote" };
                eprintln!("xtask: {verb} {}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("xtask: refresh-models failed: {e}");
                ExitCode::from(1)
            }
        },
        Command::GenLuaTypes { check } => match luatypes::run(check) {
            Ok(path) => {
                let verb = if check { "up to date" } else { "wrote" };
                eprintln!("xtask: {verb} {}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("xtask: gen-lua-types failed: {e}");
                ExitCode::from(1)
            }
        },
        Command::CheckAscii => match ascii::run() {
            Ok(()) => {
                eprintln!("xtask: source is ASCII-only");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("xtask: check-ascii failed: {e}");
                ExitCode::from(1)
            }
        },
    }
}

/// Repo path of the generated catalog, relative to the workspace root.
const GENERATED_CATALOG: &str = "crates/kage-provider/src/catalog/generated.rs";

/// Repo path of the committed upstream snapshot the offline drift
/// check runs against, relative to the workspace root. Written from
/// the pure upstream body, before the manual overlay joins.
const CATALOG_FIXTURE: &str = "xtask/fixtures/models.json";

/// Repo path of the hand-maintained catalog overlay for providers
/// models.dev does not carry, relative to the workspace root. Each
/// top-level key replaces the same-named upstream provider wholesale.
const CATALOG_MANUAL: &str = "xtask/fixtures/manual.json";

/// Command Code's public model list, the authority the manual fixture
/// is drift-checked against. It carries every model on both wire
/// formats, with name, context window and served endpoints.
const COMMAND_CODE_MODELS_URL: &str = "https://api.commandcode.ai/provider/v1/models";

/// The official `command-code` CLI's generated model reference: exact
/// ids, names and effort levels per model, served straight from the
/// npm package. The authority the fixture's reasoning shapes sync to;
/// the endpoint above publishes no effort metadata.
const COMMAND_CODE_CLI_CATALOG_URL: &str = "https://cdn.jsdelivr.net/npm/command-code/dist/bundled/command-code-knowledge/reference/models.md";

/// Hard cap on a downloaded catalog body.
const DOWNLOAD_LIMIT: usize = 32 * 1024 * 1024;

fn refresh_models(source_url: &str, check: bool) -> Result<PathBuf, String> {
    let raw = fetch(source_url)?;
    let manual_path = workspace_root().join(CATALOG_MANUAL);
    let mut manual = fs::read_to_string(&manual_path)
        .map_err(|e| format!("read {}: {e}", manual_path.display()))?;
    // The manual fixture must track Command Code: the endpoint is the
    // authority for ids, names, context windows and wire placement, the
    // official CLI catalog for the reasoning shapes. Both checks are
    // skipped when `--source` overrides upstream for offline testing.
    if source_url == MODELS_DEV_URL {
        let live = fetch(COMMAND_CODE_MODELS_URL)?;
        check_command_code_drift(&manual, &live)?;
        let cli = fetch(COMMAND_CODE_CLI_CATALOG_URL)?;
        let synced = sync_commandcode_reasoning(&manual, &cli)?;
        if synced != manual {
            if check {
                return Err(format!(
                    "{} is out of date with the Command Code CLI catalog; run `cargo xtask refresh-models`",
                    CATALOG_MANUAL
                ));
            }
            kage_core::fsutil::atomic_write(&manual_path, synced.as_bytes())
                .map_err(|e| format!("write {}: {e}", manual_path.display()))?;
            manual = synced;
        }
    }
    // Keep the committed upstream snapshot a pure upstream mirror:
    // prune before the manual overlay joins, so `models.json` never
    // carries hand-maintained providers.
    let pruned = source::prune(&raw)?;
    let providers = source::parse(&merge_catalogs(&raw, &manual)?)?;
    for map in SUPPORTED_PROVIDERS {
        if !providers.iter().any(|p| p.id == map.kage_id) {
            return Err(format!("upstream missing provider '{}'", map.api_id));
        }
    }
    let dest = workspace_root().join(GENERATED_CATALOG);
    let formatted = rustfmt_formatted(&render(&providers))?;
    if check {
        let on_disk =
            fs::read_to_string(&dest).map_err(|e| format!("read {}: {e}", dest.display()))?;
        if on_disk != formatted {
            return Err(format!(
                "{} is out of date; run `cargo xtask refresh-models`",
                dest.display()
            ));
        }
        return Ok(dest);
    }
    // Keep the committed upstream snapshot in step with generated.rs
    // so the offline drift gate and its unit test stay exact. Pruned
    // to the same subset the runtime cache keeps: parse drops nothing
    // the full body would have, so both render identically.
    let fixture = workspace_root().join(CATALOG_FIXTURE);
    if let Some(dir) = fixture.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    kage_core::fsutil::atomic_write(&fixture, pruned.as_bytes())
        .map_err(|e| format!("write {}: {e}", fixture.display()))?;
    kage_core::fsutil::atomic_write(&dest, formatted.as_bytes())
        .map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(dest)
}

/// `upstream` with every top-level entry of `manual` inserted over it:
/// a manual provider replaces the same-named upstream provider
/// wholesale, everything else passes through untouched.
///
/// # Errors
///
/// Either body does not parse as a JSON object.
fn merge_catalogs(upstream: &str, manual: &str) -> Result<String, String> {
    let mut root: serde_json::Map<String, Value> =
        serde_json::from_str(upstream).map_err(|e| format!("parse catalog: {e}"))?;
    let overlay: serde_json::Map<String, Value> =
        serde_json::from_str(manual).map_err(|e| format!("parse manual catalog: {e}"))?;
    for (id, provider) in overlay {
        root.insert(id, provider);
    }
    serde_json::to_string(&root).map_err(|e| e.to_string())
}

/// Compare the manual fixture against Command Code's live model list:
/// every live id must be in the fixture and vice versa, filed under
/// the provider its served endpoints dictate, with matching name and
/// context window. The endpoint is the authority; it enforces what it
/// publishes.
///
/// # Errors
///
/// Either body does not parse, or drift was found.
fn check_command_code_drift(manual: &str, live: &str) -> Result<(), String> {
    let manual: serde_json::Map<String, Value> =
        serde_json::from_str(manual).map_err(|e| format!("parse manual catalog: {e}"))?;
    let live: Value = serde_json::from_str(live).map_err(|e| format!("parse live models: {e}"))?;
    let live_items = live
        .get("data")
        .and_then(Value::as_array)
        .ok_or("live model list has no `data` array")?;

    // fixture id -> (provider id, name, context window).
    let mut fixture: BTreeMap<&str, (&str, &str, Option<u64>)> = BTreeMap::new();
    for (provider_id, provider) in &manual {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            return Err(format!("manual provider '{provider_id}' has no models"));
        };
        for (model_id, model) in models {
            let name = model
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("manual model '{model_id}' has no name"))?;
            let context = model
                .get("limit")
                .and_then(|limit| limit.get("context"))
                .and_then(Value::as_u64);
            fixture.insert(model_id.as_str(), (provider_id.as_str(), name, context));
        }
    }

    // live id -> (name, context window, served on chat/completions).
    let mut live: BTreeMap<&str, (&str, Option<u64>, bool)> = BTreeMap::new();
    for item in live_items {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .ok_or("live model without an `id`")?;
        let name = item.get("name").and_then(Value::as_str).unwrap_or(id);
        let context = item.get("context_length").and_then(Value::as_u64);
        let on_chat = item
            .get("supported_endpoints")
            .and_then(Value::as_array)
            .map(|endpoints| {
                endpoints
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|endpoint| endpoint == "/chat/completions")
            })
            .unwrap_or(false);
        live.insert(id, (name, context, on_chat));
    }

    let mut problems: Vec<String> = Vec::new();
    for (id, (provider_id, name, context)) in &fixture {
        let Some((live_name, live_context, on_chat)) = live.get(id) else {
            problems.push(format!("retired upstream: {id}"));
            continue;
        };
        let expected = if *on_chat {
            "commandcode"
        } else {
            "commandcode-claude"
        };
        if provider_id != &expected {
            problems.push(format!(
                "{id}: under {provider_id}, endpoints say {expected}"
            ));
        }
        if name != live_name {
            problems.push(format!("{id}: named {name:?} upstream {live_name:?}"));
        }
        if context != live_context {
            problems.push(format!(
                "{id}: context {context:?} upstream {live_context:?}"
            ));
        }
    }
    for id in live.keys() {
        if !fixture.contains_key(id) {
            problems.push(format!("new upstream: {id}"));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    const CAP: usize = 20;
    let mut message = problems[..problems.len().min(CAP)].join("; ");
    if problems.len() > CAP {
        message = format!("{message}; and {} more", problems.len() - CAP);
    }
    Err(format!(
        "Command Code model drift, update {}: {message}",
        CATALOG_MANUAL
    ))
}

/// The effort settings one CLI catalog row publishes.
struct CliEfforts {
    /// Effort values the model accepts, with `off` removed.
    values: Vec<String>,
    /// The row listed `off`, so thinking can be switched off.
    toggle: bool,
}

/// One row of the CLI catalog's model tables.
struct CliRow {
    /// The Anthropic section lists the models served on the Messages
    /// wire; every other section is Chat Completions.
    anthropic: bool,
    /// The row's Efforts column. `None` when it reads an em dash: the
    /// CLI sends no thinking setting and the model decides its own depth.
    efforts: Option<CliEfforts>,
}

/// Parse the `command-code` CLI's generated models reference: markdown
/// tables under vendor sections, one row per model.
///
/// # Errors
///
/// A row is malformed, or an effort token is unknown to kage.
fn parse_cli_catalog(md: &str) -> Result<BTreeMap<String, CliRow>, String> {
    const EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];
    let mut rows = BTreeMap::new();
    let mut anthropic = false;
    for line in md.lines() {
        if let Some(section) = line.strip_prefix("## ") {
            anthropic = section.trim() == "Anthropic";
            continue;
        }
        let Some(row) = line.strip_prefix("| `") else {
            continue;
        };
        let mut cells = row.split('|');
        let id = cells
            .next()
            .unwrap_or_default()
            .trim()
            .trim_end_matches('`');
        let _name = cells.next();
        let _context = cells.next();
        let efforts_cell = cells.next().map(str::trim).unwrap_or_default();
        let efforts = if efforts_cell == "\u{2014}" || efforts_cell.is_empty() {
            None
        } else {
            let mut toggle = false;
            let mut values = Vec::new();
            for token in efforts_cell.split(',').map(str::trim) {
                if token == "off" {
                    toggle = true;
                } else if EFFORTS.contains(&token) {
                    values.push(token.to_owned());
                } else {
                    return Err(format!(
                        "CLI catalog row '{id}' lists unknown effort '{token}'"
                    ));
                }
            }
            if values.is_empty() {
                return Err(format!("CLI catalog row '{id}' lists no efforts"));
            }
            Some(CliEfforts { values, toggle })
        };
        rows.insert(id.to_owned(), CliRow { anthropic, efforts });
    }
    Ok(rows)
}

/// Rewrite the manual fixture's `reasoning` fields for the commandcode
/// providers from the CLI catalog, the same source Command Code's own
/// agent sends from. Ids, names and context stay under the endpoint
/// check's authority; a model on one side only is an error, so the
/// two cannot drift apart here.
///
/// # Errors
///
/// Either body does not parse, an id exists on one side only, or a
/// model sits under the wrong provider for its CLI section.
fn sync_commandcode_reasoning(manual: &str, cli_md: &str) -> Result<String, String> {
    let cli = parse_cli_catalog(cli_md)?;
    let mut root: serde_json::Map<String, Value> =
        serde_json::from_str(manual).map_err(|e| format!("parse manual catalog: {e}"))?;
    for provider_id in ["commandcode", "commandcode-claude"] {
        let models = root
            .get_mut(provider_id)
            .and_then(|p| p.get_mut("models"))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| format!("manual provider '{provider_id}' has no models"))?;
        for (model_id, model) in models.iter_mut() {
            let Some(row) = cli.get(model_id.as_str()) else {
                return Err(format!("CLI catalog has no row for '{model_id}'"));
            };
            let expected = if row.anthropic {
                "commandcode-claude"
            } else {
                "commandcode"
            };
            if provider_id != expected {
                return Err(format!(
                    "{model_id}: under {provider_id}, CLI catalog says {expected}"
                ));
            }
            let object = model
                .as_object_mut()
                .ok_or_else(|| format!("manual model '{model_id}' is not an object"))?;
            match &row.efforts {
                None => {
                    object.insert("reasoning".into(), Value::Bool(false));
                    object.remove("reasoning_options");
                }
                Some(efforts) => {
                    object.insert("reasoning".into(), Value::Bool(true));
                    let mut options = Vec::new();
                    if efforts.toggle {
                        options.push(serde_json::json!({"type": "toggle"}));
                    }
                    options.push(serde_json::json!({"type": "effort", "values": efforts.values}));
                    object.insert("reasoning_options".into(), Value::Array(options));
                }
            }
        }
    }
    for id in cli.keys() {
        let known = ["commandcode", "commandcode-claude"].iter().any(|p| {
            root.get(*p)
                .and_then(|p| p.get("models"))
                .and_then(Value::as_object)
                .is_some_and(|m| m.contains_key(id.as_str()))
        });
        if !known {
            return Err(format!(
                "new Command Code model '{id}'; add it to {CATALOG_MANUAL}"
            ));
        }
    }
    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

/// `source` run through `rustfmt` so the rendered catalog matches what
/// the old write-then-format pass produced. Formatted output is read
/// from the child's stdout, so the destination file is only ever
/// touched by the final atomic write.
fn rustfmt_formatted(source: &str) -> Result<String, String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout"])
        .current_dir(workspace_root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("run rustfmt: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "run rustfmt: no stdin".to_owned())?
        .write_all(source.as_bytes())
        .map_err(|e| format!("feed rustfmt: {e}"))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("run rustfmt: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "rustfmt failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("rustfmt output is not utf-8: {e}"))
}

/// Download `url` with the same agent settings, user agent and body cap
/// the runtime catalog refresh uses. `file://` reads a local fixture
/// instead, keeping the drift gate testable offline.
fn fetch(url: &str) -> Result<String, String> {
    if let Some(path) = url.strip_prefix("file://") {
        return fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .into();
    let response = agent
        .get(url)
        .header(
            "user-agent",
            concat!("kage/", env!("CARGO_PKG_VERSION"), " xtask"),
        )
        .call()
        .map_err(|e| format!("get {url}: {e}"))?;
    read_limited(response.into_body().into_reader(), DOWNLOAD_LIMIT)
}

/// `reader` decoded as UTF-8, failing once it exceeds `limit` bytes
/// instead of silently truncating mid-frame.
fn read_limited(reader: impl std::io::Read, limit: usize) -> Result<String, String> {
    let mut reader = reader;
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read body: {e}"))?;
    if bytes.len() > limit {
        return Err(format!("catalog exceeds {} MiB", limit / (1024 * 1024)));
    }
    String::from_utf8(bytes).map_err(|e| format!("catalog is not utf-8: {e}"))
}

fn render(providers: &[SourceProvider]) -> String {
    let mut out = String::new();
    out.push_str(
        "//! @generated by `cargo xtask refresh-models`. Do not edit by hand.\n\
         //!\n\
         //! Source: <https://models.dev/api.json> plus the hand-maintained\n\
         //! `xtask/fixtures/manual.json`, curated to kage's supported providers.\n\
         \n\
         #![allow(clippy::unreadable_literal)]\n\
         \n\
         #[allow(unused_imports)]\n\
         use super::{ModelCost, ModelInfo, ProviderInfo};\n\
         #[allow(unused_imports)]\n\
         use kage_core::{Effort, Efforts, Input, Inputs, Reasoning, ReasoningField};\n\
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

fn emit_provider(out: &mut String, p: &SourceProvider) {
    let _ = writeln!(out, "    ProviderInfo {{");
    let _ = writeln!(out, "        id: {},", quote(&p.id));
    let _ = writeln!(out, "        name: {},", quote(&p.name));
    let _ = writeln!(out, "        api: {},", opt_quote(p.api.as_deref()));
    out.push_str("        models: &[\n");
    for m in &p.models {
        emit_model(out, m);
    }
    out.push_str("        ],\n");
    out.push_str("    },\n");
}

fn emit_model(out: &mut String, m: &SourceModel) {
    let _ = writeln!(out, "ModelInfo {{");
    let _ = writeln!(out, "id: {},", quote(&m.id));
    let _ = writeln!(out, "name: {},", quote(&m.name));
    let _ = writeln!(out, "context: {},", opt_int(m.context));
    let _ = writeln!(out, "input_limit: {},", opt_int(m.input_limit));
    let _ = writeln!(out, "output: {},", opt_int(m.output));
    let _ = writeln!(out, "reasoning: {},", reasoning_expr(m.reasoning));
    let _ = writeln!(out, "input: {},", inputs_expr(m.input));
    match m.interleaved {
        Some(field) => {
            let _ = writeln!(out, "interleaved: Some(ReasoningField::{field:?}),");
        }
        None => out.push_str("interleaved: None,\n"),
    }
    let _ = writeln!(
        out,
        "release_date: {},",
        opt_quote(m.release_date.as_deref())
    );
    match &m.cost {
        Some(c) => {
            let _ = writeln!(out, "cost: Some(ModelCost {{");
            let _ = writeln!(out, "input: {:.6},", c.input);
            let _ = writeln!(out, "output: {:.6},", c.output);
            let _ = writeln!(out, "cache_read: {},", opt_float(c.cache_read));
            let _ = writeln!(out, "cache_write: {},", opt_float(c.cache_write));
            out.push_str("}),\n");
        }
        None => out.push_str("cost: None,\n"),
    }
    out.push_str("},\n");
}

fn reasoning_expr(r: Reasoning) -> String {
    match r {
        Reasoning::Unknown => "Reasoning::Unknown".to_owned(),
        Reasoning::None => "Reasoning::None".to_owned(),
        Reasoning::Fixed => "Reasoning::Fixed".to_owned(),
        Reasoning::Toggle => "Reasoning::Toggle".to_owned(),
        Reasoning::Effort { efforts, toggle } => {
            let list: Vec<String> = efforts.iter().map(|e| format!("Effort::{e:?}")).collect();
            format!(
                "Reasoning::Effort {{ efforts: Efforts::of(&[{}]), toggle: {toggle} }}",
                list.join(", ")
            )
        }
        Reasoning::Budget { min, max, toggle } => format!(
            "Reasoning::Budget {{ min: {min}, max: {}, toggle: {toggle} }}",
            opt_int(max.map(u64::from))
        ),
    }
}

fn inputs_expr(inputs: Inputs) -> String {
    let list: Vec<String> = inputs.iter().map(|i| format!("Input::{i:?}")).collect();
    format!("Inputs::of(&[{}])", list.join(", "))
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
            c if (c as u32) < 0x20 || !c.is_ascii() => {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Upstream model names carry whatever Unicode models.dev publishes;
    /// the rendered catalog must stay ASCII or `check-ascii` fails on a
    /// file whose header says not to edit it by hand.
    #[test]
    fn quote_escapes_non_ascii() {
        let quoted = quote("a\u{2011}b");
        assert_eq!(quoted, "\"a\\u{2011}b\"");
        assert!(quoted.is_ascii(), "{quoted}");
        assert!(quote("\u{1f600}").is_ascii());
    }

    #[test]
    fn quote_keeps_ascii_control_and_printable_escaping() {
        assert_eq!(quote("a\"b\\c\nd\te"), "\"a\\\"b\\\\c\\nd\\te\"");
        assert_eq!(quote("\u{1}"), "\"\\u{0001}\"");
        assert_eq!(quote("plain"), "\"plain\"");
    }

    /// A small models.dev-shaped fixture through the whole parse,
    /// render and format pipeline, pinning the generator's shape.
    #[test]
    fn fixture_renders_through_rustfmt() {
        const FIXTURE: &str = r#"{
            "anthropic": {
                "name": "Anthropic",
                "api": "https://api.anthropic.com",
                "models": {
                    "claude-test": {
                        "id": "claude-test",
                        "name": "Claude Test",
                        "tool_call": true,
                        "reasoning": true,
                        "reasoning_options": [{ "type": "toggle" }],
                        "modalities": { "input": ["text", "image"], "output": ["text"] },
                        "limit": { "context": 200000, "output": 64000 },
                        "release_date": "2026-01-01",
                        "cost": { "input": 3, "output": 15,
                                  "cache_read": 0.3, "cache_write": 3.75 }
                    }
                }
            }
        }"#;
        let providers = source::parse(FIXTURE).expect("fixture parses");
        let formatted = rustfmt_formatted(&render(&providers)).expect("renders and formats");
        assert!(formatted.contains("id: \"anthropic\""), "{formatted}");
        assert!(formatted.contains("id: \"claude-test\""), "{formatted}");
        assert!(formatted.contains("Reasoning::Toggle"), "{formatted}");
        assert!(formatted.is_ascii(), "output stays ASCII");
    }

    #[test]
    fn read_limited_rejects_oversized_bodies_instead_of_truncating() {
        let body = vec![b'a'; 16];
        let err = read_limited(std::io::Cursor::new(&body), 8).unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
        let ok = read_limited(std::io::Cursor::new(&body), 16).unwrap();
        assert_eq!(ok.len(), 16);
        assert!(read_limited(std::io::Cursor::new(&body), 15).is_err());
    }

    /// The committed upstream snapshot stays a pure upstream mirror:
    /// it parses, covers every upstream-supported provider, and never
    /// carries the manual commandcode providers.
    #[test]
    fn committed_fixture_stays_a_pure_upstream_snapshot() {
        let root = workspace_root();
        let fixture = fs::read_to_string(root.join(CATALOG_FIXTURE)).expect("fixture committed");
        let upstream_only = source::parse(&fixture).expect("fixture parses");
        assert!(
            upstream_only
                .iter()
                .all(|p| p.id != "commandcode" && p.id != "commandcode-claude"),
            "{} must stay a pure upstream snapshot",
            CATALOG_FIXTURE
        );
        for map in SUPPORTED_PROVIDERS {
            assert!(
                upstream_only.iter().any(|p| p.id == map.kage_id)
                    || map.api_id.starts_with("commandcode"),
                "fixture is missing provider '{}'",
                map.api_id
            );
        }
    }

    /// The committed catalog must equal the merged render, so the
    /// offline drift gate covers the hand-maintained providers too.
    #[test]
    fn committed_manual_fixture_renders_into_the_committed_catalog() {
        let root = workspace_root();
        let fixture = fs::read_to_string(root.join(CATALOG_FIXTURE)).expect("fixture committed");
        let manual = fs::read_to_string(root.join(CATALOG_MANUAL)).expect("manual committed");
        let on_disk = fs::read_to_string(root.join(GENERATED_CATALOG)).expect("catalog committed");
        let merged = merge_catalogs(&fixture, &manual).expect("fixture merges");
        let providers = source::parse(&merged).expect("merged parses");
        for id in ["commandcode", "commandcode-claude"] {
            let provider = providers
                .iter()
                .find(|p| p.id == id)
                .unwrap_or_else(|| panic!("manual fixture supplies '{id}'"));
            assert!(!provider.models.is_empty(), "{id} carries models");
        }
        // Parse drops models it cannot read, so a typo in the fixture
        // would silently shrink the catalog; pin every entry through.
        let manual_value: Value = serde_json::from_str(&manual).expect("manual parses");
        for provider in providers.iter().filter(|p| p.id.starts_with("commandcode")) {
            let expected = manual_value[&provider.id]["models"]
                .as_object()
                .expect("manual provider carries models")
                .len();
            assert_eq!(
                provider.models.len(),
                expected,
                "{}: every manual model must parse",
                provider.id
            );
        }
        let formatted = rustfmt_formatted(&render(&providers)).expect("renders and formats");
        assert_eq!(formatted, on_disk);
    }

    #[test]
    fn manual_overlay_replaces_providers_and_keeps_the_rest() {
        let upstream = r#"{
            "anthropic": {"name": "Anthropic", "api": "https://x", "models": {}},
            "extra": {"name": "Extra"}
        }"#;
        let manual = r#"{
            "commandcode": {"name": "Command Code", "models": {}},
            "anthropic": {"name": "Replaced", "models": {}}
        }"#;
        let merged: Value =
            serde_json::from_str(&merge_catalogs(upstream, manual).expect("merges")).unwrap();
        assert_eq!(merged["commandcode"]["name"], "Command Code");
        assert_eq!(merged["anthropic"]["name"], "Replaced");
        assert_eq!(merged["anthropic"]["api"], serde_json::json!(null));
        assert_eq!(merged["extra"]["name"], "Extra");
    }

    #[test]
    fn merge_catalogs_rejects_non_object_bodies() {
        assert!(merge_catalogs("[]", "{}").is_err());
        assert!(merge_catalogs("{}", "3").is_err());
    }

    /// A minimal fixture and matching live body for the drift check.
    fn drift_inputs() -> (String, String) {
        let manual = r#"{
            "commandcode": {"models": {
                "m-a": {"id": "m-a", "name": "A", "limit": {"context": 1000}},
                "m-b": {"id": "m-b", "name": "B", "limit": {"context": 2000}}
            }},
            "commandcode-claude": {"models": {
                "claude-x": {"id": "claude-x", "name": "X", "limit": {"context": 3000}}
            }}
        }"#;
        let live = r#"{"data": [
            {"id": "m-a", "name": "A", "context_length": 1000,
             "supported_endpoints": ["/chat/completions"]},
            {"id": "m-b", "name": "B", "context_length": 2000,
             "supported_endpoints": ["/chat/completions"]},
            {"id": "claude-x", "name": "X", "context_length": 3000,
             "supported_endpoints": ["/messages"]}
        ]}"#;
        (manual.to_owned(), live.to_owned())
    }

    #[test]
    fn drift_check_passes_on_agreement() {
        let (manual, live) = drift_inputs();
        assert!(check_command_code_drift(&manual, &live).is_ok());
    }

    #[test]
    fn drift_check_reports_every_kind_of_drift() {
        let (manual, _) = drift_inputs();
        let live = r#"{"data": [
            {"id": "m-a", "name": "A2", "context_length": 1500,
             "supported_endpoints": ["/messages"]},
            {"id": "m-new", "name": "New", "context_length": 5}
        ]}"#;
        let err = check_command_code_drift(&manual, live).unwrap_err();
        assert!(err.contains("m-b"), "{err}");
        assert!(err.contains("retired upstream"), "{err}");
        assert!(err.contains("m-new"), "{err}");
        assert!(err.contains("new upstream"), "{err}");
        assert!(err.contains("named"), "{err}");
        assert!(err.contains("context"), "{err}");
        assert!(err.contains("claude-x"), "{err}");
        assert!(err.contains("commandcode-claude"), "{err}");
    }

    #[test]
    fn drift_check_rejects_malformed_bodies() {
        let (manual, live) = drift_inputs();
        assert!(check_command_code_drift("[]", &live).is_err());
        assert!(check_command_code_drift(&manual, "{}").is_err());
        assert!(check_command_code_drift(&manual, "not json").is_err());
    }

    /// A trimmed-down CLI catalog reference with every cell shape.
    /// The Efforts "none" cells read `__DASH__`, replaced with the
    /// real glyph the CLI ships: raw strings cannot escape it and
    /// sources stay ASCII.
    const CLI_SAMPLE: &str = r#"
# Command Code Models

## Anthropic

| Id (use EXACTLY this) | Name | Context | Efforts | Rates | Plan | Best for |
|---|---|---|---|---|---|---|
| `claude-x` | Claude X | 1M | low, medium, high, xhigh, max | $2/$10 | GOAT | desc |
| `claude-h` | Claude H | 200K | __DASH__ | $1/$5 | Pro | desc |

## Open Source

| Id (use EXACTLY this) | Name | Context | Efforts | Rates | Plan | Best for |
|---|---|---|---|---|---|---|
| `m-a` | A | 1M | off, high, max | $1/$2 | Go | desc |
| `m-b` | B | 256K | __DASH__ | $0/$0 | Go | desc |
"#;

    fn cli_sample() -> String {
        CLI_SAMPLE.replace("__DASH__", "\u{2014}")
    }

    #[test]
    fn cli_catalog_parses_sections_efforts_and_toggle() {
        let rows = parse_cli_catalog(&cli_sample()).expect("parses");
        assert_eq!(rows.len(), 4);
        let x = &rows["claude-x"];
        assert!(x.anthropic);
        let efforts = x.efforts.as_ref().expect("efforts");
        assert_eq!(efforts.values, ["low", "medium", "high", "xhigh", "max"]);
        assert!(!efforts.toggle);
        assert!(rows["claude-h"].efforts.is_none());
        let a = &rows["m-a"];
        assert!(!a.anthropic);
        let efforts = a.efforts.as_ref().expect("efforts");
        assert_eq!(efforts.values, ["high", "max"]);
        assert!(efforts.toggle);
        assert!(rows["m-b"].efforts.is_none());
        assert!(
            parse_cli_catalog("## X\n| `m` | M | 1M | wild, guess | $1/$2 | Go | d |").is_err()
        );
    }

    /// The manual fixture matching [`CLI_SAMPLE`], with reasoning
    /// fields deliberately stale.
    fn cli_sync_fixture() -> String {
        r#"{
          "commandcode": {"models": {
            "m-a": {"id": "m-a", "name": "A", "reasoning": true,
                    "reasoning_options": [{"type": "effort", "values": ["low"]}],
                    "interleaved": {"field": "reasoning_content"}},
            "m-b": {"id": "m-b", "name": "B", "reasoning": true,
                    "reasoning_options": [{"type": "toggle"}]}
          }},
          "commandcode-claude": {"models": {
            "claude-x": {"id": "claude-x", "name": "Claude X", "reasoning": true,
                         "reasoning_options": [{"type": "toggle"},
                           {"type": "budget_tokens", "min": 1024}]},
            "claude-h": {"id": "claude-h", "name": "Claude H", "reasoning": false}
          }}
        }"#
        .to_owned()
    }

    #[test]
    fn cli_sync_rewrites_reasoning_from_the_catalog() {
        let synced: Value = serde_json::from_str(
            &sync_commandcode_reasoning(&cli_sync_fixture(), &cli_sample()).expect("syncs"),
        )
        .unwrap();
        let a = &synced["commandcode"]["models"]["m-a"];
        assert_eq!(a["reasoning"], true);
        assert_eq!(
            a["reasoning_options"],
            serde_json::json!([
                {"type": "toggle"},
                {"type": "effort", "values": ["high", "max"]}
            ])
        );
        // Response-side metadata the CLI says nothing about survives.
        assert_eq!(a["interleaved"]["field"], "reasoning_content");
        let b = &synced["commandcode"]["models"]["m-b"];
        assert_eq!(b["reasoning"], false);
        assert!(b.get("reasoning_options").is_none());
        let x = &synced["commandcode-claude"]["models"]["claude-x"];
        assert_eq!(
            x["reasoning_options"],
            serde_json::json!([{"type": "effort", "values": ["low", "medium", "high", "xhigh", "max"]}])
        );
        let h = &synced["commandcode-claude"]["models"]["claude-h"];
        assert_eq!(h["reasoning"], false);
        // A sync that changes nothing is a fixed point.
        let once = sync_commandcode_reasoning(&cli_sync_fixture(), &cli_sample()).unwrap();
        assert_eq!(
            sync_commandcode_reasoning(&once, &cli_sample()).unwrap(),
            once
        );
    }

    #[test]
    fn cli_sync_rejects_drift_between_fixture_and_catalog() {
        let fixture = cli_sync_fixture();
        let sample = cli_sample();
        let with_new = format!("{sample}\n| `m-c` | C | 1M | \u{2014} | $0/$0 | Go | d |");
        assert!(
            sync_commandcode_reasoning(&fixture, &with_new)
                .unwrap_err()
                .contains("new Command Code model"),
        );
        assert!(
            sync_commandcode_reasoning(
                &fixture,
                "## Open Source\n| `m-x` | X | 1M | low | $1/$2 | Go | d |"
            )
            .unwrap_err()
            .contains("no row for 'm-a'"),
        );
        let misplaced = "## Anthropic\n| `m-a` | A | 1M | off, high, max | $1/$2 | Go | d |\n";
        assert!(
            sync_commandcode_reasoning(&fixture, misplaced)
                .unwrap_err()
                .contains("CLI catalog says commandcode-claude"),
        );
    }
}
