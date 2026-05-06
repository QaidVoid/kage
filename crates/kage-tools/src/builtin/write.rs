//! `write` tool: create or overwrite a file with atomic semantics.
//!
//! The implementation writes to a sibling temp file in the same directory and
//! renames it onto the target, so partial failures never leave half-written
//! content visible. The destination must already have an existing parent
//! directory; the tool does not create directories.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolError, resolve_under, schema_for};

/// Input shape for the `write` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct WriteInput {
    /// Path to write, relative to the workdir.
    path: String,
    /// File contents (UTF-8).
    content: String,
    /// Allow overwriting an existing file. Defaults to `false`.
    #[serde(default)]
    overwrite: bool,
}

/// Atomically write a file inside the workspace.
#[derive(Debug, Default)]
pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Atomically write `content` to `path` inside the workspace. Refuses to \
         overwrite an existing file unless `overwrite: true` is set. The parent \
         directory must exist."
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<WriteInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: WriteInput = serde_json::from_value(input)?;
        let target = resolve_under(cx.workdir(), Path::new(&input.path))?;

        if target.exists() && !input.overwrite {
            return Ok(ToolOutput {
                is_error: true,
                text: format!(
                    "{} already exists; pass `overwrite: true` to replace it",
                    input.path
                ),
                structured: None,
            });
        }

        let parent = target.parent().ok_or_else(|| ToolError::Path {
            path: target.clone(),
            reason: "target has no parent directory".into(),
        })?;
        if !parent.exists() {
            return Err(ToolError::Path {
                path: parent.to_path_buf(),
                reason: "parent directory does not exist".into(),
            });
        }

        let temp = temp_sibling(&target);
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(input.content.as_bytes())?;
            file.sync_all()?;
        }
        if let Err(e) = fs::rename(&temp, &target) {
            let _ = fs::remove_file(&temp);
            return Err(ToolError::Io(e));
        }

        let bytes = input.content.len();
        Ok(ToolOutput {
            is_error: false,
            text: format!("wrote {bytes} bytes to {}", input.path),
            structured: Some(serde_json::json!({"path": input.path, "bytes": bytes})),
        })
    }
}

fn temp_sibling(target: &Path) -> std::path::PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let suffix = ulid::Ulid::new().to_string();
    let name = match target.file_name() {
        Some(n) => format!(".{}.{suffix}.tmp", n.to_string_lossy()),
        None => format!(".kage-{suffix}.tmp"),
    };
    parent.join(name)
}

#[cfg(test)]
mod tests {
    use kage_core::CancelFlag;

    use super::*;

    fn run(
        tool: &WriteTool,
        workdir: &Path,
        input: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir, &cancel);
        tool.execute(input, &cx)
    }

    #[test]
    fn writes_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            &WriteTool,
            dir.path(),
            serde_json::json!({"path":"hello.txt","content":"hi"}),
        )
        .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "hi"
        );
    }

    #[test]
    fn refuses_to_overwrite_without_flag() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x.txt"), "old").unwrap();
        let out = run(
            &WriteTool,
            dir.path(),
            serde_json::json!({"path":"x.txt","content":"new"}),
        )
        .unwrap();
        assert!(out.is_error);
        assert_eq!(fs::read_to_string(dir.path().join("x.txt")).unwrap(), "old");
    }

    #[test]
    fn overwrites_when_flag_is_set() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x.txt"), "old").unwrap();
        let out = run(
            &WriteTool,
            dir.path(),
            serde_json::json!({"path":"x.txt","content":"new","overwrite":true}),
        )
        .unwrap();
        assert!(!out.is_error);
        assert_eq!(fs::read_to_string(dir.path().join("x.txt")).unwrap(), "new");
    }

    #[test]
    fn rejects_path_outside_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            &WriteTool,
            dir.path(),
            serde_json::json!({"path":"/tmp/outside.txt","content":"x"}),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }

    #[test]
    fn rejects_when_parent_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            &WriteTool,
            dir.path(),
            serde_json::json!({"path":"missing/x.txt","content":"y"}),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }

    #[test]
    fn writes_into_existing_subdir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let out = run(
            &WriteTool,
            dir.path(),
            serde_json::json!({"path":"sub/x.txt","content":"y"}),
        )
        .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(dir.path().join("sub/x.txt")).unwrap(),
            "y"
        );
    }

    #[test]
    fn structured_output_carries_byte_count() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            &WriteTool,
            dir.path(),
            serde_json::json!({"path":"a.txt","content":"hello"}),
        )
        .unwrap();
        let s = out.structured.unwrap();
        assert_eq!(s["bytes"], 5);
        assert_eq!(s["path"], "a.txt");
    }
}
