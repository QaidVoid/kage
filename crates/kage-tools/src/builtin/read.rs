//! `read` tool: read a file from the workspace, optionally limited to a line range.

use std::fmt::Write;
use std::path::Path;

use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolError, resolve, schema_for};

/// Files larger than this are truncated; the model is told via a footer.
const MAX_BYTES: usize = 2_000_000;

/// Input shape for the `read` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct ReadInput {
    /// Path to read, relative to the workdir.
    path: String,
    /// Optional 1-indexed start line (inclusive).
    #[serde(default)]
    start_line: Option<u64>,
    /// Optional 1-indexed end line (inclusive).
    #[serde(default)]
    end_line: Option<u64>,
}

/// Read a file from disk.
#[derive(Debug, Default)]
pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read a file from the workspace. Optional `start_line` and `end_line` limit \
         the returned range (1-indexed, inclusive). Files larger than 2MB are truncated."
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<ReadInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: ReadInput = serde_json::from_value(input)?;
        let path = resolve(cx.workdir(), Path::new(&input.path))?;

        let bytes = std::fs::read(&path)?;
        let total_bytes = bytes.len();
        let truncated = total_bytes > MAX_BYTES;
        let head = &bytes[..total_bytes.min(MAX_BYTES)];
        let text = String::from_utf8_lossy(head).into_owned();

        let sliced = slice_lines(&text, input.start_line, input.end_line);

        let mut output = sliced.into_owned();
        if truncated {
            let _ = write!(
                output,
                "\n\n[... truncated: file is {total_bytes} bytes, showed first {MAX_BYTES} ...]"
            );
        }

        Ok(ToolOutput {
            is_error: false,
            text: output,
            structured: None,
            terminate: false,
        })
    }
}

fn slice_lines(text: &str, start: Option<u64>, end: Option<u64>) -> std::borrow::Cow<'_, str> {
    if start.is_none() && end.is_none() {
        return std::borrow::Cow::Borrowed(text);
    }
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start_idx = start
        .map_or(0, |n| {
            usize::try_from(n.saturating_sub(1)).unwrap_or(usize::MAX)
        })
        .min(total);
    let end_idx = end
        .map_or(total, |n| usize::try_from(n).unwrap_or(usize::MAX))
        .min(total);
    let end_idx = end_idx.max(start_idx);
    std::borrow::Cow::Owned(lines[start_idx..end_idx].join("\n"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kage_core::CancelFlag;

    use super::*;

    fn run(
        tool: &ReadTool,
        workdir: &Path,
        input: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let cancel = CancelFlag::new();
        let cx = ToolContext::new(workdir, &cancel);
        tool.execute(input, &cx)
    }

    #[test]
    fn reads_full_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hello.txt"), "line1\nline2\nline3").unwrap();
        let out = run(
            &ReadTool,
            dir.path(),
            serde_json::json!({"path":"hello.txt"}),
        )
        .unwrap();
        assert_eq!(out.text, "line1\nline2\nline3");
        assert!(!out.is_error);
    }

    #[test]
    fn slices_by_line_range() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\ne").unwrap();
        let out = run(
            &ReadTool,
            dir.path(),
            serde_json::json!({"path":"f.txt","start_line":2,"end_line":4}),
        )
        .unwrap();
        assert_eq!(out.text, "b\nc\nd");
    }

    #[test]
    fn truncates_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(MAX_BYTES + 100);
        fs::write(dir.path().join("big.txt"), &big).unwrap();
        let out = run(&ReadTool, dir.path(), serde_json::json!({"path":"big.txt"})).unwrap();
        assert!(out.text.contains("truncated"));
        assert!(out.text.len() > MAX_BYTES);
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            &ReadTool,
            dir.path(),
            serde_json::json!({"path":"missing.txt"}),
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Io(_)), "got {err:?}");
    }

    #[test]
    fn invalid_input_returns_json_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(&ReadTool, dir.path(), serde_json::json!({})).unwrap_err();
        assert!(matches!(err, ToolError::Json(_)), "got {err:?}");
    }

    #[test]
    fn end_line_beyond_eof_clamps() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("short.txt"), "a\nb").unwrap();
        let out = run(
            &ReadTool,
            dir.path(),
            serde_json::json!({"path":"short.txt","start_line":1,"end_line":1000}),
        )
        .unwrap();
        assert_eq!(out.text, "a\nb");
    }
}
