//! `kage init`: first-run setup wizard.
//!
//! Detects existing config and credentials, optionally writes a
//! starter `~/.config/kage/config.toml`, and offers to save a provider API
//! key so the user can run `kage` without exporting env vars.
//!
//! Idempotent: rerunning is safe. Without `--force` an existing config
//! is left untouched and the wizard only tops up missing pieces (auth
//! credentials, plugin / session directories).

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::ExitCode;

use kage_core::config::Config;

use crate::auth::{self, AuthStore, KNOWN_PROVIDERS};

/// The `lua-language-server` definition stub, embedded so `kage init`
/// can drop it next to a generated `.luarc.json` regardless of where
/// the binary runs from. Kept in sync with the repo copy under
/// `plugins/types/kage.lua`.
const TYPE_STUB: &str = include_str!("../../../plugins/types/kage.lua");

/// Default starter `config.toml` body. Written verbatim when the
/// wizard is asked to create a fresh config; mirrors the Rust
/// [`Config::default`] so the `[provider] default_model =
/// "anthropic:claude-sonnet-4-6"` line stays in sync as a documented
/// starting point even though `figment` would synthesize it from the
/// struct default.
const STARTER_CONFIG: &str = r#"# kage configuration. See `kage doctor` for diagnostics.

[provider]
# Default `provider:model` to start each session with. Override per
# session with `kage -m <provider:model> ...` or with the in-TUI
# model picker (Ctrl-P).
default_model = "anthropic:claude-sonnet-4-6"

[ui]
# Bundled themes: "default", "tokyo-night", "catppuccin-mocha", ...
theme = "default"
# Capture mouse events (drag-to-select, click-to-focus). Set to false
# if you prefer your terminal's native selection.
mouse = true

[plugins]
# Defaults: `~/.config/kage/plugins/`. Set `dir` to an absolute path
# to point elsewhere. `enabled = ["foo", "bar"]` restricts loading
# to a named subset; an empty list loads everything in the dir.
enabled = []

[sandbox]
# 0.1 ships only the "local" backend. "bubblewrap" and "sandbox-exec"
# are placeholders for post-0.1.
backend = "local"
suppress_warning = false

[keybindings]
# `bindings` maps chord -> command name. Chords use vim-style
# notation; commands match the slash-command and `:`-command palette.
bindings = {}
"#;

/// Run the `kage init` wizard. `force` overwrites any existing
/// config; `non_interactive` skips every prompt (writes the config if
/// missing, never asks for an API key).
pub fn run(force: bool, non_interactive: bool) -> ExitCode {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "kage init: first-run setup");
    let _ = writeln!(stdout);

    if let Err(err) = ensure_directories(&mut stdout) {
        let _ = writeln!(stdout, "FAILED: {err}");
        return ExitCode::from(1);
    }

    match handle_config(&mut stdout, force) {
        Ok(()) => {}
        Err(err) => {
            let _ = writeln!(stdout, "FAILED: {err}");
            return ExitCode::from(1);
        }
    }

    if let Err(err) = install_lua_lsp(&mut stdout) {
        let _ = writeln!(stdout, "  WARN: lua lsp setup skipped: {err}");
    }

    if non_interactive {
        let _ = writeln!(stdout, "skipping credential prompt (--non-interactive)");
    } else if let Err(err) = handle_auth(&mut stdout) {
        let _ = writeln!(stdout, "WARN: credential setup skipped: {err}");
    }

    let _ = writeln!(stdout);
    let _ = writeln!(stdout, "Done. Try `kage doctor` to verify the install.");
    ExitCode::SUCCESS
}

/// Create the per-user data and plugin directories so subsequent
/// commands don't trip over a missing parent. Skipped silently when
/// the home directory cannot be resolved (the wizard moves on; the
/// auth / config paths will surface their own errors later).
fn ensure_directories<W: Write>(out: &mut W) -> Result<(), String> {
    let mut created = Vec::new();
    if let Ok(dir) = crate::data_root() {
        if !dir.exists() {
            fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
            created.push(dir);
        }
    }
    if let Ok(dir) = crate::sessions_dir() {
        if !dir.exists() {
            fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
            created.push(dir);
        }
    }
    if let Some(config_path) = Config::default_path()
        && let Some(parent) = config_path.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        created.push(parent.to_path_buf());
    }
    for path in &created {
        let _ = writeln!(out, "  created {}", path.display());
    }
    Ok(())
}

/// Write the starter [`STARTER_CONFIG`] when no config exists, or
/// when `force` is set. Reports the action through `out`.
fn handle_config<W: Write>(out: &mut W, force: bool) -> Result<(), String> {
    let Some(path) = Config::default_path() else {
        return Err("cannot resolve home directory".into());
    };
    let exists = path.exists();
    if exists && !force {
        let _ = writeln!(out, "  config:  {} (kept)", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    write_config(&path)?;
    let label = if exists { "overwrote" } else { "wrote" };
    let _ = writeln!(out, "  config:  {label} {}", path.display());
    Ok(())
}

fn write_config(path: &Path) -> Result<(), String> {
    fs::write(path, STARTER_CONFIG).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Drop the type stub into the data dir and point
/// `lua-language-server` at it via a `.luarc.json` in the plugins
/// dir. Resolves the XDG paths, then delegates the file work to
/// [`write_lua_lsp`] so the merge logic stays unit-testable.
fn install_lua_lsp<W: Write>(out: &mut W) -> Result<(), String> {
    let types_dir = crate::data_root()?.join("types");
    let luarc = crate::plugins_dir()?.join(".luarc.json");
    write_lua_lsp(&types_dir, &luarc, out)
}

/// Write `kage.lua` into `types_dir` (always refreshed: it is a
/// generated artifact, not user-edited) and ensure `luarc` lists
/// `types_dir` in `workspace.library`. An existing `.luarc.json` is
/// merged, never clobbered; if it is not valid JSON it is left
/// untouched and the step is reported as skipped rather than
/// silently overwriting the user's file.
fn write_lua_lsp<W: Write>(types_dir: &Path, luarc: &Path, out: &mut W) -> Result<(), String> {
    fs::create_dir_all(types_dir).map_err(|e| format!("mkdir {}: {e}", types_dir.display()))?;
    let stub = types_dir.join("kage.lua");
    fs::write(&stub, TYPE_STUB).map_err(|e| format!("write {}: {e}", stub.display()))?;
    let _ = writeln!(out, "  types:   wrote {}", stub.display());

    let lib = types_dir.to_string_lossy().into_owned();
    let mut doc: serde_json::Value = if luarc.exists() {
        let body =
            fs::read_to_string(luarc).map_err(|e| format!("read {}: {e}", luarc.display()))?;
        match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(
                    out,
                    "  luarc:   {} is not valid JSON ({e}); left untouched",
                    luarc.display()
                );
                return Ok(());
            }
        }
    } else {
        serde_json::json!({})
    };

    let Some(root) = doc.as_object_mut() else {
        let _ = writeln!(
            out,
            "  luarc:   {} is not a JSON object; left untouched",
            luarc.display()
        );
        return Ok(());
    };
    let workspace = root
        .entry("workspace")
        .or_insert_with(|| serde_json::json!({}));
    let Some(workspace) = workspace.as_object_mut() else {
        let _ = writeln!(
            out,
            "  luarc:   workspace key is not an object; left untouched"
        );
        return Ok(());
    };
    let library = workspace
        .entry("library")
        .or_insert_with(|| serde_json::json!([]));
    let Some(library) = library.as_array_mut() else {
        let _ = writeln!(
            out,
            "  luarc:   workspace.library is not an array; left untouched"
        );
        return Ok(());
    };
    if library.iter().any(|v| v.as_str() == Some(lib.as_str())) {
        let _ = writeln!(out, "  luarc:   {} (kept)", luarc.display());
        return Ok(());
    }
    library.push(serde_json::Value::String(lib));
    if let Some(parent) = luarc.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let body =
        serde_json::to_string_pretty(&doc).map_err(|e| format!("serialize .luarc.json: {e}"))?;
    fs::write(luarc, format!("{body}\n")).map_err(|e| format!("write {}: {e}", luarc.display()))?;
    let _ = writeln!(out, "  luarc:   wrote {}", luarc.display());
    Ok(())
}

/// Show the current credential state. If no provider has a saved key
/// or env var, ask whether to add one now and run the same prompt
/// flow as `kage auth login`.
fn handle_auth<W: Write>(out: &mut W) -> Result<(), String> {
    let store = AuthStore::load()?;
    let env_count = KNOWN_PROVIDERS
        .iter()
        .filter(|p| {
            let env = auth::env_var_for(p);
            !env.is_empty() && std::env::var(env).is_ok_and(|v| !v.is_empty())
        })
        .count();
    let stored_count = store.providers.len();
    let _ = writeln!(out, "  auth:    {stored_count} stored, {env_count} via env");
    if stored_count > 0 || env_count > 0 {
        return Ok(());
    }

    let _ = write!(out, "  no provider credentials found. Add one now? [Y/n] ");
    let _ = out.flush();
    if !confirm_default_yes()? {
        return Ok(());
    }

    let Some(provider) = prompt_provider()? else {
        let _ = writeln!(out, "  no provider chosen, skipping");
        return Ok(());
    };
    let key = read_secret(&format!("  {provider} API key: "))?;
    if key.is_empty() {
        let _ = writeln!(out, "  empty key, skipping");
        return Ok(());
    }
    let mut store = AuthStore::load()?;
    store.set_api_key(&provider, key);
    store.save()?;
    let _ = writeln!(out, "  saved {provider} key to auth.json");
    Ok(())
}

/// Prompt the user to pick a provider id from [`KNOWN_PROVIDERS`].
fn prompt_provider() -> Result<Option<String>, String> {
    let items: Vec<kage_tui::PickItem> = KNOWN_PROVIDERS
        .iter()
        .map(|p| {
            let label = match kage_provider::catalog::provider(p) {
                Some(info) => format!("{} ({}, {} models)", info.name, p, info.models.len()),
                None => (*p).to_owned(),
            };
            kage_tui::PickItem::simple(*p).with_label(label)
        })
        .collect();
    kage_tui::pick("Select a provider", &items).map_err(|e| format!("picker failed: {e}"))
}

fn confirm_default_yes() -> Result<bool, String> {
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("read stdin: {e}"))?;
    let trimmed = line.trim().to_ascii_lowercase();
    Ok(matches!(trimmed.as_str(), "" | "y" | "yes"))
}

fn read_secret(prompt: &str) -> Result<String, String> {
    rpassword::prompt_password(prompt).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_config_round_trips_through_config_loader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path).unwrap();
        // The shipped starter file must round-trip through Config::load
        // so users editing it as a starting point get the documented
        // defaults rather than a parse error or unexpected drift.
        let cfg = Config::load(&path).expect("starter config parses");
        assert_eq!(cfg.provider.default_model, "anthropic:claude-sonnet-4-6");
        assert_eq!(cfg.ui.theme, "default");
        assert!(cfg.ui.mouse);
        assert!(cfg.plugins.enabled.is_empty());
        assert!(matches!(
            cfg.sandbox.backend,
            kage_core::config::SandboxBackend::Local
        ));
        assert!(!cfg.sandbox.suppress_warning);
        assert!(cfg.keybindings.bindings.is_empty());
    }

    #[test]
    fn write_config_creates_file_with_starter_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert_eq!(body, STARTER_CONFIG);
    }

    #[test]
    fn lua_lsp_writes_stub_and_fresh_luarc() {
        let dir = tempfile::tempdir().unwrap();
        let types = dir.path().join("types");
        let luarc = dir.path().join("plugins").join(".luarc.json");
        let mut out = Vec::new();
        write_lua_lsp(&types, &luarc, &mut out).unwrap();

        assert_eq!(
            fs::read_to_string(types.join("kage.lua")).unwrap(),
            TYPE_STUB
        );
        let doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&luarc).unwrap()).unwrap();
        let lib = doc["workspace"]["library"].as_array().unwrap();
        assert_eq!(lib.len(), 1);
        assert_eq!(lib[0].as_str(), Some(types.to_string_lossy().as_ref()));
    }

    #[test]
    fn lua_lsp_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let types = dir.path().join("types");
        let luarc = dir.path().join(".luarc.json");
        let mut out = Vec::new();
        write_lua_lsp(&types, &luarc, &mut out).unwrap();
        write_lua_lsp(&types, &luarc, &mut out).unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&luarc).unwrap()).unwrap();
        assert_eq!(doc["workspace"]["library"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn lua_lsp_merges_into_existing_luarc() {
        let dir = tempfile::tempdir().unwrap();
        let types = dir.path().join("types");
        let luarc = dir.path().join(".luarc.json");
        fs::write(
            &luarc,
            r#"{ "runtime": { "version": "Lua 5.4" },
                "workspace": { "library": ["/my/own/types"] } }"#,
        )
        .unwrap();
        let mut out = Vec::new();
        write_lua_lsp(&types, &luarc, &mut out).unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&luarc).unwrap()).unwrap();
        assert_eq!(doc["runtime"]["version"], serde_json::json!("Lua 5.4"));
        let lib = doc["workspace"]["library"].as_array().unwrap();
        assert!(lib.iter().any(|v| v == "/my/own/types"));
        assert!(
            lib.iter()
                .any(|v| v.as_str() == Some(types.to_string_lossy().as_ref()))
        );
    }

    #[test]
    fn lua_lsp_leaves_invalid_luarc_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let types = dir.path().join("types");
        let luarc = dir.path().join(".luarc.json");
        fs::write(&luarc, "{ not json at all").unwrap();
        let mut out = Vec::new();
        write_lua_lsp(&types, &luarc, &mut out).unwrap();
        // The stub still lands, but the broken file is preserved.
        assert!(types.join("kage.lua").exists());
        assert_eq!(fs::read_to_string(&luarc).unwrap(), "{ not json at all");
    }
}
