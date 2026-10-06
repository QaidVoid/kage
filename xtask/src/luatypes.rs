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
    run_at(&stub_path(), check)
}

/// Render the stub to `path`, or in check mode verify the bytes already
/// there. The check comparison folds CRLF to LF so a Windows checkout
/// with `core.autocrlf=true` does not report drift and then rewrite
/// the file with churn.
///
/// # Errors
///
/// The file cannot be read or written, or check mode found drift.
pub fn run_at(path: &Path, check: bool) -> Result<PathBuf, String> {
    let rendered = spec::lua_stub();
    if check {
        let on_disk =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if on_disk.replace("\r\n", "\n") != rendered {
            return Err(format!(
                "{} is out of date; run `cargo xtask gen-lua-types`",
                path.display()
            ));
        }
        return Ok(path.to_path_buf());
    }
    kage_core::fsutil::atomic_write(path, rendered.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    #[test]
    fn committed_stub_passes_the_drift_check() {
        if let Err(report) = super::run(true) {
            panic!("{report}");
        }
    }

    #[test]
    fn check_tolerates_crlf_line_endings() {
        let rendered = kage_plugin::spec::lua_stub();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kage.lua");
        std::fs::write(&path, rendered.replace('\n', "\r\n")).unwrap();
        super::run_at(&path, true).expect("a CRLF twin of the stub passes the drift check");
    }

    #[test]
    fn check_reports_a_stale_stub() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kage.lua");
        std::fs::write(&path, "-- stale\n").unwrap();
        let err = super::run_at(&path, true).unwrap_err();
        assert!(err.contains("out of date"), "{err}");
    }

    #[test]
    fn write_replaces_the_file_and_leaves_no_temp_behind() {
        let rendered = kage_plugin::spec::lua_stub();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kage.lua");
        std::fs::write(&path, "old").unwrap();
        super::run_at(&path, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), rendered);
        let count = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, 1, "the write must be atomic, with no temp sibling");
    }
}
