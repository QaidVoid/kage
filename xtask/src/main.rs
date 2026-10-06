//! `cargo xtask` - workspace housekeeping commands.
//!
//! Layering: build tooling outside the runtime crate layering; depends
//! on `kage-plugin` and `kage-provider`.
//!
//! Subcommands:
//!
//! * `refresh-models`: fetch `https://models.dev/api.json`, curate the
//!   subset kage needs with `kage_provider::catalog::source`, and
//!   rewrite `crates/kage-provider/src/catalog/generated.rs`. This is
//!   run by maintainers; `cargo build` itself is offline. `--check`
//!   re-renders and diffs without writing (the CI drift gate).
//! * `gen-lua-types`: regenerate `plugins/types/kage.lua`.
//! * `check-ascii`: the ASCII-only source gate.

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
/// check runs against, relative to the workspace root.
const CATALOG_FIXTURE: &str = "xtask/fixtures/models.json";

/// Hard cap on a downloaded catalog body.
const DOWNLOAD_LIMIT: usize = 32 * 1024 * 1024;

fn refresh_models(source_url: &str, check: bool) -> Result<PathBuf, String> {
    let raw = fetch(source_url)?;
    let providers = source::parse(&raw)?;
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
    let pruned = source::prune(&raw)?;
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
         //! Source: <https://models.dev/api.json>, curated to kage's supported providers.\n\
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

    /// The committed upstream snapshot must render byte-for-byte to the
    /// committed catalog, so the offline drift gate (and the CI
    /// `--source file://` run) is exact.
    #[test]
    fn committed_fixture_renders_to_the_committed_catalog() {
        let root = workspace_root();
        let fixture = fs::read_to_string(root.join(CATALOG_FIXTURE)).expect("fixture committed");
        let on_disk = fs::read_to_string(root.join(GENERATED_CATALOG)).expect("catalog committed");
        let providers = source::parse(&fixture).expect("fixture parses");
        for map in SUPPORTED_PROVIDERS {
            assert!(
                providers.iter().any(|p| p.id == map.kage_id),
                "fixture is missing provider '{}'",
                map.api_id
            );
        }
        let formatted = rustfmt_formatted(&render(&providers)).expect("renders and formats");
        assert_eq!(formatted, on_disk);
    }
}
