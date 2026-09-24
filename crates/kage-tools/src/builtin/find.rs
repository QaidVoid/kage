//! `find` tool: locate files by glob, honoring ignore files.

use std::fmt::Write as _;
use std::path::Path;

use globset::Glob;
use ignore::WalkBuilder;
use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolError, resolve, schema_for};

/// Upper bound on returned paths; keeps a huge tree from flooding the
/// model's context.
const MAX_ENTRIES: usize = 5_000;

/// Input shape for the `find` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct FindInput {
    /// Glob pattern, e.g. `**/*.rs` or `src/**/*.toml`.
    pattern: String,
    /// Optional subdirectory under workdir. Defaults to workdir.
    #[serde(default)]
    path: Option<String>,
    /// Filter by entry kind: `"f"` for files, `"d"` for directories. None = both.
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

/// Locate files by glob pattern.
#[derive(Debug, Default)]
pub struct FindTool;

impl Tool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }

    fn description(&self) -> &'static str {
        "Find paths matching a glob pattern. Honors `.gitignore` \
         and `.kageignore`. Optional `type` filters by `f` (files) or `d` \
         (directories). Output is capped at 5000 entries."
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<FindInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: FindInput = serde_json::from_value(input)?;
        let root = match &input.path {
            Some(p) => resolve(cx.workdir(), Path::new(p))?,
            None => cx.workdir().to_path_buf(),
        };
        let glob = Glob::new(&input.pattern)
            .map_err(|e| ToolError::InvalidInput(format!("invalid glob: {e}")))?
            .compile_matcher();

        let want_files = input
            .kind
            .as_deref()
            .map(|k| matches!(k, "f" | "file" | "files"));
        let want_dirs = input
            .kind
            .as_deref()
            .map(|k| matches!(k, "d" | "dir" | "directory" | "directories"));

        let walker = WalkBuilder::new(&root)
            .add_custom_ignore_filename(".kageignore")
            .build();

        let mut matches: Vec<String> = Vec::new();
        let mut truncated = false;
        for entry in walker {
            if cx.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let Ok(entry) = entry else { continue };
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let is_file = entry.file_type().is_some_and(|t| t.is_file());

            if let Some(true) = want_files
                && !is_file
            {
                continue;
            }
            if let Some(true) = want_dirs
                && !is_dir
            {
                continue;
            }
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if rel.as_os_str().is_empty() {
                continue;
            }
            if glob.is_match(rel) {
                matches.push(rel.to_string_lossy().into_owned());
                if matches.len() > MAX_ENTRIES {
                    truncated = true;
                    break;
                }
            }
        }
        if truncated {
            matches.truncate(MAX_ENTRIES);
        }
        matches.sort();

        let text = if matches.is_empty() {
            "(no matches)".to_owned()
        } else {
            let mut text = matches.join("\n");
            if truncated {
                let _ = write!(text, "\n[... truncated at {MAX_ENTRIES} entries ...]");
            }
            text
        };
        let count = matches.len();
        Ok(ToolOutput {
            is_error: false,
            text,
            structured: Some(serde_json::json!({
                "pattern": input.pattern,
                "matches": count,
                "truncated": truncated,
                "paths": matches,
            })),
            terminate: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kage_core::CancelFlag;

    use super::*;

    fn run(workdir: &Path, input: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir, &cancel);
        FindTool.execute(input, &cx)
    }

    fn populate(dir: &Path) {
        fs::create_dir(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(dir.join("src/lib.rs"), "// lib").unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        fs::write(dir.join("README.md"), "# kage").unwrap();
    }

    #[test]
    fn finds_rust_files() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"**/*.rs"})).unwrap();
        assert!(out.text.contains("src/main.rs"));
        assert!(out.text.contains("src/lib.rs"));
        assert!(!out.text.contains("Cargo.toml"));
    }

    #[test]
    fn type_dir_filters_to_directories() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"*","type":"d"})).unwrap();
        assert_eq!(out.text, "src");
    }

    #[test]
    fn invalid_glob_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), serde_json::json!({"pattern":"["})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn no_matches_returns_marker() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"**/*.zzz"})).unwrap();
        assert_eq!(out.text, "(no matches)");
    }

    #[test]
    fn output_caps_at_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_ENTRIES + 10) {
            fs::write(dir.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let out = run(dir.path(), serde_json::json!({"pattern":"*.txt"})).unwrap();
        assert!(out.text.contains("[... truncated at 5000 entries ...]"));
        let structured = out.structured.unwrap();
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["matches"], MAX_ENTRIES);
        assert_eq!(structured["paths"].as_array().unwrap().len(), MAX_ENTRIES);
    }
}
