//! `edit` tool: apply one or more substring or line-range changes to an
//! existing file and report the result as a unified diff.
//!
//! Two input forms are accepted:
//! 1. Shorthand single-substring form: `{path, old_str, new_str, replace_all?}`.
//! 2. Multi-change form: `{path, changes: [...]}` where each change is
//!    either a substring shorthand `{old_str, new_str, replace_all?}` or a
//!    precise line range `{range: {start, end}, text}` (1-based, inclusive).
//!
//! The tool validates that no two resulting byte ranges overlap before
//! writing anything, so a partial apply never lands on disk.

use std::path::Path;

use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use similar::TextDiff;

use crate::atomic::atomic_write;
use crate::{Tool, ToolContext, ToolError, resolve, schema_for};

/// Input shape for the `edit` tool. Accepts either the shorthand
/// single-substring form or the multi-change form.
#[derive(Debug, Deserialize, JsonSchema)]
struct EditInput {
    /// Path to edit, relative to the workdir.
    path: String,
    /// Substring to replace (shorthand single-change form). Required when
    /// `changes` is absent.
    #[serde(default)]
    old_str: Option<String>,
    /// Replacement text (shorthand single-change form).
    #[serde(default)]
    new_str: Option<String>,
    /// In shorthand form, replace every occurrence rather than requiring a
    /// unique match.
    #[serde(default)]
    replace_all: bool,
    /// Multi-change form: a list of changes to apply in one shot.
    #[serde(default)]
    changes: Option<Vec<EditChange>>,
}

/// One change in the multi-change form. Either a substring shorthand or a
/// precise line-range replacement.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
enum EditChange {
    /// Substring-based replacement, same semantics as the shorthand form.
    Substring {
        /// Substring to replace.
        old_str: String,
        /// Replacement text.
        new_str: String,
        /// Replace every occurrence in this change. Defaults to false.
        #[serde(default)]
        replace_all: bool,
    },
    /// Line-range replacement: replace lines `[start..=end]` with `text`.
    /// Line numbers are 1-based and inclusive on both ends.
    LineRange {
        /// 1-based inclusive line range to replace.
        range: LineRange,
        /// Text to splice in. Must end with a newline unless `end` is the
        /// last line of the file and the file does not end with a newline.
        text: String,
    },
}

/// Inclusive 1-based line range.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
struct LineRange {
    /// First line to replace, 1-based.
    start: u32,
    /// Last line to replace, 1-based.
    end: u32,
}

/// Search-and-replace inside an existing file.
#[derive(Debug, Default)]
pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Apply one or more changes to an existing file. Pass either a single \
         substring shorthand (`old_str`/`new_str`/`replace_all?`) or a list of \
         changes in `changes` mixing substring shorthands with line-range \
         replacements (`{range: {start, end}, text}`, 1-based inclusive). \
         Returns a unified diff in the structured output."
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

        let changes = match collect_changes(&input) {
            Ok(c) => c,
            Err(msg) => return Ok(error(&input.path, msg)),
        };
        if changes.is_empty() {
            return Ok(error(
                &input.path,
                "edit requires either old_str/new_str or a non-empty changes list".into(),
            ));
        }

        let mut splices = Vec::new();
        for change in &changes {
            match resolve_change(change, &original, &input.path) {
                Ok(mut spans) => splices.append(&mut spans),
                Err(msg) => return Ok(error(&input.path, msg)),
            }
        }

        splices.sort_by_key(|s| s.start);
        if let Some(overlap) = first_overlap(&splices) {
            return Ok(error(
                &input.path,
                format!("overlapping edits: byte ranges {overlap}"),
            ));
        }

        let new_content = apply_splices(&original, &splices);
        atomic_write(&path, new_content.as_bytes())?;

        let diff = unified_diff(&original, &new_content, &input.path);
        let count = splices.len();
        let suffix = if count == 1 { "" } else { "s" };
        Ok(ToolOutput {
            is_error: false,
            text: format!("edited {} ({count} change{suffix})", input.path),
            structured: Some(serde_json::json!({
                "path": input.path,
                "replacements": count,
                "diff": diff,
            })),
            terminate: false,
        })
    }
}

/// Build a normalized list of [`EditChange`]s out of the input's two
/// accepted shapes. The shorthand `old_str`/`new_str` fields lift into a
/// single `Substring` change; otherwise the `changes` array is returned
/// verbatim.
fn collect_changes(input: &EditInput) -> Result<Vec<EditChange>, String> {
    let has_shorthand = input.old_str.is_some() || input.new_str.is_some();
    let has_changes = input.changes.is_some();
    if has_shorthand && has_changes {
        return Err("provide either old_str/new_str OR changes, not both".into());
    }
    if let Some(list) = &input.changes {
        return Ok(list
            .iter()
            .map(|c| match c {
                EditChange::Substring {
                    old_str,
                    new_str,
                    replace_all,
                } => EditChange::Substring {
                    old_str: old_str.clone(),
                    new_str: new_str.clone(),
                    replace_all: *replace_all,
                },
                EditChange::LineRange { range, text } => EditChange::LineRange {
                    range: *range,
                    text: text.clone(),
                },
            })
            .collect());
    }
    match (&input.old_str, &input.new_str) {
        (Some(old_str), Some(new_str)) => Ok(vec![EditChange::Substring {
            old_str: old_str.clone(),
            new_str: new_str.clone(),
            replace_all: input.replace_all,
        }]),
        (Some(_), None) | (None, Some(_)) => {
            Err("shorthand form requires both old_str and new_str".into())
        }
        (None, None) => Ok(Vec::new()),
    }
}

/// One concrete byte-range replacement, after resolving substrings and
/// line ranges against the file contents.
#[derive(Clone, Debug)]
struct Splice {
    start: usize,
    end: usize,
    replacement: String,
}

/// Convert one [`EditChange`] into one or more concrete [`Splice`]s
/// against the original file contents.
fn resolve_change(change: &EditChange, original: &str, path: &str) -> Result<Vec<Splice>, String> {
    match change {
        EditChange::Substring {
            old_str,
            new_str,
            replace_all,
        } => resolve_substring(old_str, new_str, *replace_all, original, path),
        EditChange::LineRange { range, text } => {
            resolve_line_range(*range, text, original, path).map(|s| vec![s])
        }
    }
}

fn resolve_substring(
    old_str: &str,
    new_str: &str,
    replace_all: bool,
    original: &str,
    path: &str,
) -> Result<Vec<Splice>, String> {
    if old_str.is_empty() {
        return Err("old_str must not be empty".into());
    }
    let positions: Vec<usize> = original.match_indices(old_str).map(|(i, _)| i).collect();
    if positions.is_empty() {
        return Err(format!("`old_str` not found in {path}"));
    }
    if positions.len() > 1 && !replace_all {
        return Err(format!(
            "`old_str` matches {n} times in {path}; pass `replace_all: true` \
             or supply more context to make the match unique",
            n = positions.len(),
        ));
    }
    Ok(positions
        .into_iter()
        .map(|start| Splice {
            start,
            end: start + old_str.len(),
            replacement: new_str.to_owned(),
        })
        .collect())
}

fn resolve_line_range(
    range: LineRange,
    text: &str,
    original: &str,
    path: &str,
) -> Result<Splice, String> {
    if range.start == 0 || range.end == 0 {
        return Err(format!(
            "line numbers are 1-based; got start={} end={} in {path}",
            range.start, range.end,
        ));
    }
    if range.start > range.end {
        return Err(format!(
            "range.start ({}) must be <= range.end ({}) in {path}",
            range.start, range.end,
        ));
    }
    let mut line_starts: Vec<usize> = vec![0];
    for (i, ch) in original.char_indices() {
        if ch == '\n' {
            line_starts.push(i + ch.len_utf8());
        }
    }
    let total_lines = if line_starts.last().copied() == Some(original.len()) {
        line_starts.len().saturating_sub(1)
    } else {
        line_starts.len()
    };
    let start_idx = range.start as usize - 1;
    let end_idx = range.end as usize - 1;
    if start_idx >= total_lines || end_idx >= total_lines {
        return Err(format!(
            "line range {}..{} out of bounds for {path} ({total_lines} lines)",
            range.start, range.end,
        ));
    }
    let start_byte = line_starts[start_idx];
    let end_byte = line_starts
        .get(end_idx + 1)
        .copied()
        .unwrap_or(original.len());
    Ok(Splice {
        start: start_byte,
        end: end_byte,
        replacement: text.to_owned(),
    })
}

/// Returns a debug string describing the first overlap found between two
/// already-sorted splices, or `None` when none overlap.
fn first_overlap(splices: &[Splice]) -> Option<String> {
    for pair in splices.windows(2) {
        if pair[0].end > pair[1].start {
            return Some(format!(
                "[{}, {}) and [{}, {})",
                pair[0].start, pair[0].end, pair[1].start, pair[1].end,
            ));
        }
    }
    None
}

/// Apply a sorted, non-overlapping splice list to `original` and return
/// the rewritten string.
fn apply_splices(original: &str, splices: &[Splice]) -> String {
    let mut out = String::with_capacity(original.len());
    let mut cursor = 0;
    for sp in splices {
        out.push_str(&original[cursor..sp.start]);
        out.push_str(&sp.replacement);
        cursor = sp.end;
    }
    out.push_str(&original[cursor..]);
    out
}

fn error(path: &str, message: String) -> ToolOutput {
    let _ = path;
    ToolOutput {
        is_error: true,
        text: message,
        structured: None,
        terminate: false,
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
    fn shorthand_replaces_unique_match() {
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
    fn shorthand_errors_when_old_str_missing() {
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
    fn shorthand_refuses_ambiguous_match_without_flag() {
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
    fn shorthand_replace_all_replaces_every_occurrence() {
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

    #[test]
    fn multi_change_substring_list_applies_in_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "alpha beta gamma\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({
                "path": "f.txt",
                "changes": [
                    {"old_str": "alpha", "new_str": "A"},
                    {"old_str": "gamma", "new_str": "G"},
                ],
            }),
        )
        .unwrap();
        assert!(!out.is_error, "got {out:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "A beta G\n"
        );
        assert_eq!(out.structured.unwrap()["replacements"], 2);
    }

    #[test]
    fn multi_change_line_range_replaces_block() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({
                "path": "f.txt",
                "changes": [
                    {"range": {"start": 2, "end": 3}, "text": "2.5\n"},
                ],
            }),
        )
        .unwrap();
        assert!(!out.is_error, "got {out:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "one\n2.5\nfour\n"
        );
    }

    #[test]
    fn multi_change_mixed_substring_and_range() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({
                "path": "f.txt",
                "changes": [
                    {"old_str": "alpha", "new_str": "A"},
                    {"range": {"start": 3, "end": 3}, "text": "G\n"},
                ],
            }),
        )
        .unwrap();
        assert!(!out.is_error, "got {out:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "A\nbeta\nG\n"
        );
    }

    #[test]
    fn overlap_between_two_substring_changes_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "abcdef\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({
                "path": "f.txt",
                "changes": [
                    {"old_str": "abc", "new_str": "x"},
                    {"old_str": "bcd", "new_str": "y"},
                ],
            }),
        )
        .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("overlapping"), "got {out:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "abcdef\n",
        );
    }

    #[test]
    fn overlap_between_range_and_substring_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "aaa\nbbb\nccc\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({
                "path": "f.txt",
                "changes": [
                    {"range": {"start": 1, "end": 2}, "text": "X\n"},
                    {"old_str": "aaa", "new_str": "Y"},
                ],
            }),
        )
        .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("overlapping"));
    }

    #[test]
    fn shorthand_and_changes_together_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "abc").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({
                "path": "f.txt",
                "old_str": "a",
                "new_str": "z",
                "changes": [{"old_str": "b", "new_str": "y"}],
            }),
        )
        .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("either old_str/new_str OR changes"));
    }

    #[test]
    fn empty_input_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "abc").unwrap();
        let out = run(dir.path(), serde_json::json!({"path": "f.txt"})).unwrap();
        assert!(out.is_error);
        assert!(
            out.text
                .contains("either old_str/new_str or a non-empty changes")
        );
    }

    #[test]
    fn line_range_out_of_bounds_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "one\ntwo\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({
                "path": "f.txt",
                "changes": [{"range": {"start": 1, "end": 9}, "text": "x\n"}],
            }),
        )
        .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("out of bounds"));
    }

    #[test]
    fn line_range_zero_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "one\ntwo\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({
                "path": "f.txt",
                "changes": [{"range": {"start": 0, "end": 1}, "text": "x\n"}],
            }),
        )
        .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("1-based"));
    }

    #[test]
    fn line_range_inverted_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({
                "path": "f.txt",
                "changes": [{"range": {"start": 3, "end": 1}, "text": "x\n"}],
            }),
        )
        .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("<= range.end"));
    }

    #[test]
    fn multi_change_diff_covers_every_edit() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({
                "path": "f.txt",
                "changes": [
                    {"old_str": "one", "new_str": "ONE"},
                    {"old_str": "three", "new_str": "THREE"},
                ],
            }),
        )
        .unwrap();
        assert!(!out.is_error);
        let diff = out.structured.unwrap()["diff"].as_str().unwrap().to_owned();
        assert!(diff.contains("-one"));
        assert!(diff.contains("+ONE"));
        assert!(diff.contains("-three"));
        assert!(diff.contains("+THREE"));
    }
}
