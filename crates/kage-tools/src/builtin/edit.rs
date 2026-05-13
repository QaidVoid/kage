//! `edit` tool: search-and-replace inside an existing file with a unified
//! diff in the structured output.
//!
//! Refuses to apply when `old_str` matches more than once unless
//! `replace_all` is explicitly true, to keep edits tightly scoped.

use std::path::Path;

use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use similar::TextDiff;

use crate::atomic::atomic_write;
use crate::{Tool, ToolContext, ToolError, resolve, schema_for};

/// Input shape for the `edit` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct EditInput {
    /// Path to edit, relative to the workdir.
    path: String,
    /// Substring to replace. Must be present in the file.
    old_str: String,
    /// Replacement text.
    new_str: String,
    /// Replace every occurrence rather than requiring a unique match.
    #[serde(default)]
    replace_all: bool,
}

/// Search-and-replace inside an existing file.
#[derive(Debug, Default)]
pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Replace `old_str` with `new_str` in `path`. Errors if `old_str` is not \
         present, or appears more than once and `replace_all` is not set. Returns \
         a unified diff in the structured output."
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<EditInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: EditInput = serde_json::from_value(input)?;
        let path = resolve(cx.workdir(), Path::new(&input.path))?;
        let original = std::fs::read_to_string(&path)?;

        let count = original.matches(&input.old_str).count();
        if count == 0 {
            return Ok(ToolOutput {
                is_error: true,
                text: format!("`old_str` not found in {}", input.path),
                structured: None,
                terminate: false,
            });
        }
        if count > 1 && !input.replace_all {
            return Ok(ToolOutput {
                is_error: true,
                text: format!(
                    "`old_str` matches {count} times in {}; pass `replace_all: true` \
                     or supply more context to make the match unique",
                    input.path,
                ),
                structured: None,
                terminate: false,
            });
        }

        let new_content = if input.replace_all {
            original.replace(&input.old_str, &input.new_str)
        } else {
            original.replacen(&input.old_str, &input.new_str, 1)
        };

        atomic_write(&path, new_content.as_bytes())?;

        let diff = unified_diff(&original, &new_content, &input.path);
        let suffix = if count == 1 { "" } else { "s" };
        Ok(ToolOutput {
            is_error: false,
            text: format!("edited {} ({count} replacement{suffix})", input.path),
            structured: Some(serde_json::json!({
                "path": input.path,
                "replacements": count,
                "diff": diff,
            })),
            terminate: false,
        })
    }
}

fn unified_diff(original: &str, new: &str, path: &str) -> String {
    let diff = TextDiff::from_lines(original, new);
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        out.push_str(&hunk.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kage_core::CancelFlag;

    use super::*;

    fn run(workdir: &Path, input: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir, &cancel);
        EditTool.execute(input, &cx)
    }

    #[test]
    fn replaces_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "hello world\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({"path":"f.txt","old_str":"world","new_str":"kage"}),
        )
        .unwrap();
        assert!(!out.is_error, "got {out:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "hello kage\n"
        );
        let diff = out.structured.unwrap()["diff"].as_str().unwrap().to_owned();
        assert!(diff.contains("-hello world"));
        assert!(diff.contains("+hello kage"));
    }

    #[test]
    fn errors_when_old_str_missing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "abc").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({"path":"f.txt","old_str":"nope","new_str":"x"}),
        )
        .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("not found"));
        assert_eq!(fs::read_to_string(dir.path().join("f.txt")).unwrap(), "abc");
    }

    #[test]
    fn refuses_ambiguous_match_without_flag() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "x\nx\nx\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({"path":"f.txt","old_str":"x","new_str":"y"}),
        )
        .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("3 times"));
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "x\nx\nx\n"
        );
    }

    #[test]
    fn replace_all_replaces_every_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "x\nx\nx\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({"path":"f.txt","old_str":"x","new_str":"y","replace_all":true}),
        )
        .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "y\ny\ny\n"
        );
        assert_eq!(out.structured.unwrap()["replacements"], 3);
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            dir.path(),
            serde_json::json!({"path":"nope.txt","old_str":"a","new_str":"b"}),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Io(_)));
    }
}
