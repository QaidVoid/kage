//! `gen-lua-types`: render `plugins/types/kage.lua` from the
//! single-source spec that lives in `kage-plugin`.
//!
//! The stub is [`kage_plugin::spec::lua_stub`], rendered from the same
//! description the crate uses to reason about its own Lua surface, and
//! which a `kage-plugin` test pins to the actually-installed bindings.
//! This command writes it; `--check` re-renders and diffs without
//! writing (the CI drift gate). Editing the plugin API means editing the spec
//! in `crates/kage-plugin/src/spec.rs` and rerunning, so the shipped
//! `.lua` cannot drift from the Rust bindings.

use std::path::{Path, PathBuf};

use kage_plugin::spec;

/// Repo path of the generated stub, relative to the workspace root.
fn stub_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join("plugins")
        .join("types")
        .join("kage.lua")
}

/// `cargo xtask gen-lua-types [--check]`. Writes the stub, or in
/// check mode returns an error when the committed file is stale.
pub fn run(check: bool) -> Result<PathBuf, String> {
    let path = stub_path();
    let rendered = spec::lua_stub();
    if check {
        let on_disk =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if on_disk != rendered {
            return Err(format!(
                "{} is out of date; run `cargo xtask gen-lua-types`",
                path.display()
            ));
        }
        return Ok(path);
    }
    std::fs::write(&path, &rendered).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}
