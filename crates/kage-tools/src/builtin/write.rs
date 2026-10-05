//! `write` tool: create or overwrite a file with atomic semantics.
//!
//! The implementation writes to a sibling temp file in the same directory and
//! renames it onto the target, so partial failures never leave half-written
//! content visible. The destination must already have an existing parent
//! directory; the tool does not create directories.

use std::path::Path;

use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::atomic::atomic_write;
use crate::path_lock::with_path_lock;
use crate::{Tool, ToolContext, ToolError, schema_for};

/// Input shape for the `write` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct WriteInput {
    /// Path to write, relative to the workdir.
    #[serde(alias = "filePath")]
    path: String,
    /// File contents (UTF-8).
    content: String,
    /// Allow overwriting an existing file. Defaults to `false`.
    #[serde(default)]
    overwrite: bool,
}

/// Atomically write a file.
#[derive(Debug, Default)]
pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Atomically write `content` to `path`. Refuses to overwrite an \
         existing file unless `overwrite: true` is set. The parent \
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
        let target = cx.resolve_path(Path::new(&input.path))?;
        // The exists check and the rename are one sequence: without the
        // lock, two concurrent non-overwriting writes could both pass
        // the check and both land, silently losing one.
        with_path_lock(&target, || self.write_locked(&input, &target))
    }
}

impl WriteTool {
    fn write_locked(&self, input: &WriteInput, target: &Path) -> Result<ToolOutput, ToolError> {
        if target.exists() && !input.overwrite {
            return Ok(ToolOutput {
                is_error: true,
                text: format!(
                    "{} already exists; pass `overwrite: true` to replace it",
                    input.path
                ),
                structured: None,
                terminate: false,
            });
        }

        let parent = target.parent().ok_or_else(|| ToolError::Path {
            path: target.to_path_buf(),
            reason: "target has no parent directory".into(),
        })?;
        if !parent.exists() {
            return Err(ToolError::Path {
                path: parent.to_path_buf(),
                reason: "parent directory does not exist".into(),
            });
        }

        atomic_write(&target, input.content.as_bytes())
            .map_err(ToolError::io_at("write", &target))?;

        let bytes = input.content.len();
        Ok(ToolOutput {
            is_error: false,
            text: format!("wrote {bytes} bytes to {}", input.path),
            structured: Some(serde_json::json!({"path": input.path, "bytes": bytes})),
            terminate: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

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

    #[test]
    fn confined_write_rejects_escape() {
        let (root, work) = nested_workdir();
        let outside = root.path().join("outside-write-confined.txt");
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(&work, &cancel).with_confine();
        let err = WriteTool
            .execute(
                serde_json::json!({"path":"../outside-write-confined.txt","content":"x"}),
                &cx,
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }), "got {err:?}");
        assert!(!outside.exists());
    }

    #[test]
    fn unconfined_write_accepts_escape() {
        let (root, work) = nested_workdir();
        let out = run(
            &WriteTool,
            &work,
            serde_json::json!({"path":"../outside-write-unconfined.txt","content":"x"}),
        )
        .unwrap();
        assert!(!out.is_error);
        let outside = root.path().join("outside-write-unconfined.txt");
        assert_eq!(fs::read_to_string(outside).unwrap(), "x");
    }

    /// A workdir inside its own temp root, so `..` stays private to
    /// the test.
    fn nested_workdir() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        fs::create_dir(&work).unwrap();
        (root, work)
    }

    #[test]
    fn filepath_is_accepted_as_an_alias_for_path() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            &WriteTool,
            dir.path(),
            serde_json::json!({"filePath":"out.txt","content":"hi"}),
        )
        .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "hi"
        );
        // The canonical key still works: the alias is additive.
        run(
            &WriteTool,
            dir.path(),
            serde_json::json!({"path":"out2.txt","content":"hi"}),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("out2.txt")).unwrap(),
            "hi"
        );
    }

    /// Two non-overwriting writes racing onto one fresh path: the
    /// per-path lock must let exactly one through and keep the other
    /// from silently replacing the winner.
    #[test]
    fn concurrent_writes_without_overwrite_admit_exactly_one() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path();
        let results: Vec<ToolOutput> = std::thread::scope(|scope| {
            ["aaa", "bbbbb"]
                .map(|content| {
                    scope.spawn(move || {
                        let cancel = CancelFlag::new();
                        let cx = ToolContext::new(workdir, &cancel);
                        WriteTool
                            .execute(
                                serde_json::json!({"path":"race.txt","content":content}),
                                &cx,
                            )
                            .unwrap()
                    })
                })
                .map(|handle| handle.join().unwrap())
                .into()
        });
        assert_eq!(
            results.iter().filter(|out| !out.is_error).count(),
            1,
            "exactly one write must land: {results:?}"
        );
        let winner = results.iter().find(|out| !out.is_error).unwrap();
        let bytes = winner.structured.clone().unwrap()["bytes"]
            .as_u64()
            .unwrap();
        let content = fs::read_to_string(dir.path().join("race.txt")).unwrap();
        assert_eq!(content.len() as u64, bytes);
        assert!(content == "aaa" || content == "bbbbb", "{content:?}");
    }
}
