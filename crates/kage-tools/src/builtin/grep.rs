//! `grep` tool: regex search through workspace files honoring `.gitignore`
//! and `.kageignore`.
//!
//! Uses the `grep` and `ignore` crates from the ripgrep family. Does not
//! shell out to `rg`.

use std::path::Path;

use grep::regex::RegexMatcherBuilder;
use grep::searcher::{Searcher, Sink, SinkMatch};
use ignore::WalkBuilder;
use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolError, resolve_under, schema_for};

const DEFAULT_MAX_MATCHES: u64 = 1_000;

/// Input shape for the `grep` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct GrepInput {
    /// Regex pattern (Rust regex syntax).
    pattern: String,
    /// Optional subdirectory under workdir to search. Defaults to workdir.
    #[serde(default)]
    path: Option<String>,
    /// Match case-insensitively.
    #[serde(default)]
    ignore_case: bool,
    /// Cap on total matches across all files. Defaults to 1000.
    #[serde(default)]
    max_matches: Option<u64>,
}

/// Recursive regex search.
#[derive(Debug, Default)]
pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Recursively search workspace files for a regex pattern. Honors \
         `.gitignore` and `.kageignore`. Returns lines as `path:line:text`. \
         Capped at 1000 matches by default."
    }

    fn schema(&self) -> serde_json::Value {
        schema_for::<GrepInput>()
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: GrepInput = serde_json::from_value(input)?;
        let root = match &input.path {
            Some(p) => resolve_under(cx.workdir(), Path::new(p))?,
            None => cx.workdir().to_path_buf(),
        };
        let max = input.max_matches.unwrap_or(DEFAULT_MAX_MATCHES);

        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(input.ignore_case)
            .build(&input.pattern)
            .map_err(|e| ToolError::InvalidInput(format!("invalid regex: {e}")))?;

        let mut hits: Vec<Hit> = Vec::new();
        let mut total = 0u64;
        let mut hit_cap = false;

        let walker = WalkBuilder::new(&root)
            .add_custom_ignore_filename(".kageignore")
            .hidden(true)
            .build();

        'walk: for entry in walker {
            if cx.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path().to_path_buf();
            let mut searcher = Searcher::new();
            let mut sink = MatchSink {
                hits: &mut hits,
                total: &mut total,
                max,
                path: &path,
                root: &root,
                hit_cap: false,
            };
            let _ = searcher.search_path(&matcher, &path, &mut sink);
            if sink.hit_cap {
                hit_cap = true;
                break 'walk;
            }
        }

        let text = format_hits(&hits, hit_cap, max, total);
        Ok(ToolOutput {
            is_error: false,
            text,
            structured: Some(serde_json::json!({
                "pattern": input.pattern,
                "matches": total,
                "truncated": hit_cap,
            })),
        })
    }
}

#[derive(Debug)]
struct Hit {
    rel_path: String,
    line: u64,
    text: String,
}

struct MatchSink<'a> {
    hits: &'a mut Vec<Hit>,
    total: &'a mut u64,
    max: u64,
    path: &'a Path,
    root: &'a Path,
    hit_cap: bool,
}

impl Sink for MatchSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if *self.total >= self.max {
            self.hit_cap = true;
            return Ok(false);
        }
        let line = mat.line_number().unwrap_or(0);
        let text = String::from_utf8_lossy(mat.bytes()).trim_end().to_owned();
        let rel = self
            .path
            .strip_prefix(self.root)
            .unwrap_or(self.path)
            .to_string_lossy()
            .into_owned();
        self.hits.push(Hit {
            rel_path: rel,
            line,
            text,
        });
        *self.total += 1;
        Ok(true)
    }
}

fn format_hits(hits: &[Hit], truncated: bool, max: u64, total: u64) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if hits.is_empty() {
        return "(no matches)".into();
    }
    for h in hits {
        let _ = writeln!(out, "{}:{}:{}", h.rel_path, h.line, h.text);
    }
    if truncated {
        let _ = writeln!(out, "[... truncated at {max} matches; total {total}+ ...]");
    } else {
        let _ = writeln!(out, "({total} matches)");
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
        GrepTool.execute(input, &cx)
    }

    fn populate(dir: &Path) {
        fs::write(dir.join("a.txt"), "alpha\nbravo\nCharlie\n").unwrap();
        fs::write(dir.join("b.txt"), "kage\nshadow\n").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/c.txt"), "alphabet").unwrap();
    }

    #[test]
    fn finds_matches_recursively() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"alpha"})).unwrap();
        assert!(out.text.contains("a.txt:1:alpha"));
        assert!(out.text.contains("sub/c.txt:1:alphabet"));
    }

    #[test]
    fn case_insensitive_match() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(
            dir.path(),
            serde_json::json!({"pattern":"charlie","ignore_case":true}),
        )
        .unwrap();
        assert!(out.text.contains("a.txt:3:Charlie"));
    }

    #[test]
    fn no_matches_returns_empty_marker() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let out = run(dir.path(), serde_json::json!({"pattern":"zzz"})).unwrap();
        assert_eq!(out.text, "(no matches)");
    }

    #[test]
    fn invalid_regex_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), serde_json::json!({"pattern":"["})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn path_outside_workdir_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), serde_json::json!({"pattern":"x","path":"/etc"})).unwrap_err();
        assert!(matches!(err, ToolError::Path { .. }));
    }

    #[test]
    fn honors_kageignore() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        fs::write(dir.path().join(".kageignore"), "b.txt\n").unwrap();
        let out = run(dir.path(), serde_json::json!({"pattern":"shadow"})).unwrap();
        assert_eq!(out.text, "(no matches)");
    }

    #[test]
    fn max_matches_cap_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let many: String = (0..50).map(|_| "x\n").collect();
        fs::write(dir.path().join("f.txt"), &many).unwrap();
        let out = run(
            dir.path(),
            serde_json::json!({"pattern":"x","max_matches":5}),
        )
        .unwrap();
        assert!(out.text.contains("truncated"));
        assert_eq!(out.structured.unwrap()["truncated"], true);
    }
}
