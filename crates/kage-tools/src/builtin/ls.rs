//! `ls` tool: list directory contents, optionally recursive.

use std::path::Path;

use ignore::WalkBuilder;
use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolError, resolve, schema_for};

/// Input shape for the `ls` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct LsInput {
    /// Optional subdirectory under workdir. Defaults to workdir.
    #[serde(default)]
    path: Option<String>,
    /// Recurse into subdirectories. Honors `.gitignore` and `.kageignore`.
    #[serde(default)]
    recursive: bool,
}

/// List files and directories.
#[derive(Debug, Default)]
pub struct LsTool;

impl Tool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn description(&self) -> &'static str {
        "List directory contents. With `recursive: true`, walks subdirectories \
         honoring `.gitignore` and `.kageignore`. Entries are prefixed with \
         `f` (file), `d` (directory), or `l` (symlink)."
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<LsInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: LsInput = serde_json::from_value(input)?;
        let target = match &input.path {
            Some(p) => resolve(cx.workdir(), Path::new(p))?,
            None => cx.workdir().to_path_buf(),
        };

        let mut entries: Vec<String> = Vec::new();
        if input.recursive {
            let walker = WalkBuilder::new(&target)
                .add_custom_ignore_filename(".kageignore")
                .build();
            for entry in walker {
                if cx.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                let Ok(entry) = entry else { continue };
                let rel = entry.path().strip_prefix(&target).unwrap_or(entry.path());
                if rel.as_os_str().is_empty() {
                    continue;
                }
                let prefix = entry_prefix(entry.file_type());
                entries.push(format!("{prefix} {}", rel.to_string_lossy()));
            }
        } else {
            for entry in std::fs::read_dir(&target)? {
                if cx.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                let Ok(entry) = entry else { continue };
                let file_type = entry.file_type().ok();
                let prefix = entry_prefix(file_type);
                let name = entry.file_name();
                entries.push(format!("{prefix} {}", name.to_string_lossy()));
            }
        }
        entries.sort();

        let text = if entries.is_empty() {
            "(empty)".to_owned()
        } else {
            entries.join("\n")
        };
        let count = entries.len();
        Ok(ToolOutput {
            is_error: false,
            text,
            structured: Some(serde_json::json!({"entries": count})),
        })
    }
}

fn entry_prefix(file_type: Option<std::fs::FileType>) -> char {
    match file_type {
        Some(t) if t.is_dir() => 'd',
        Some(t) if t.is_symlink() => 'l',
        Some(_) => 'f',
        None => '?',
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
        LsTool.execute(input, &cx)
    }

    #[test]
    fn lists_workdir_root_by_default() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let out = run(dir.path(), serde_json::json!({})).unwrap();
        assert!(out.text.contains("f a.txt"));
        assert!(out.text.contains("d sub"));
    }

    #[test]
    fn recursive_walks_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/inner.txt"), "x").unwrap();
        let out = run(dir.path(), serde_json::json!({"recursive":true})).unwrap();
        assert!(out.text.contains("sub"));
        assert!(out.text.contains("sub/inner.txt"));
    }

    #[test]
    fn empty_dir_returns_marker() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), serde_json::json!({})).unwrap();
        assert_eq!(out.text, "(empty)");
    }

    #[test]
    fn lists_named_subdir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/x.txt"), "x").unwrap();
        let out = run(dir.path(), serde_json::json!({"path":"sub"})).unwrap();
        assert!(out.text.contains("f x.txt"));
    }

    #[test]
    fn recursive_honors_kageignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        fs::write(dir.path().join("b.txt"), "x").unwrap();
        fs::write(dir.path().join(".kageignore"), "b.txt\n").unwrap();
        let out = run(dir.path(), serde_json::json!({"recursive":true})).unwrap();
        assert!(out.text.contains("a.txt"));
        assert!(!out.text.contains("b.txt"));
    }
}
