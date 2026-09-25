//! `ToolPairBlockWidget`: per-block renderer for a paired tool call
//! and its result (the merged tool row the user sees in the buffer).

use std::sync::Arc;

use ratatui::text::Line;
use serde_json::Value;

use super::tool_view::{EditDiff, ToolPhase};
use super::widget::{BlockWidget, RenderCtx};
use super::{ToolRow, tool_row_lines};
use crate::buffer::Block;

/// Renders one [`Block::ToolCall`] paired with its matching
/// [`Block::ToolResult`] as a finished tool row: state bullet, verb,
/// target, duration and a body chosen by the tool's kind. An
/// interrupted call shows the last progress it streamed instead of the
/// cancellation text.
///
/// Unpaired tool calls (still running) and unpaired tool results stay
/// on the standalone widgets.
#[derive(Clone, Debug)]
pub struct ToolPairBlockWidget {
    name: String,
    input: Arc<Value>,
    phase: ToolPhase,
    folded: bool,
    output: String,
    duration_ms: Option<u64>,
    diff: Option<Arc<EditDiff>>,
}

impl ToolPairBlockWidget {
    /// Construct a widget from a paired call and result.
    ///
    /// Returns `None` when either block is the wrong variant; callers
    /// who already verified the pair from `Buffer` should `unwrap()`.
    #[must_use]
    pub fn from_pair(call: &Block, result: &Block) -> Option<Self> {
        let Block::ToolCall {
            name,
            input,
            phase,
            folded,
            progress,
            diff,
            ..
        } = call
        else {
            return None;
        };
        let Block::ToolResult {
            output,
            duration_ms,
            ..
        } = result
        else {
            return None;
        };
        Some(Self {
            name: name.clone(),
            input: Arc::clone(input),
            phase: *phase,
            folded: *folded,
            output: if *phase == ToolPhase::Interrupted {
                progress.clone()
            } else {
                output.clone()
            },
            duration_ms: *duration_ms,
            diff: diff.clone(),
        })
    }
}

impl BlockWidget for ToolPairBlockWidget {
    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let row = ToolRow {
            name: &self.name,
            input: &self.input,
            phase: self.phase,
            folded: self.folded,
            elapsed_ms: self.duration_ms,
            output: &self.output,
            diff: self.diff.as_deref(),
        };
        tool_row_lines(&row, width, ctx.emphasis, ctx.row_budget)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::Emphasis;
    use super::*;
    use crate::buffer::Buffer;
    use crate::theme::Theme;

    fn ctx(theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            theme,
            focused: false,
            emphasis: Emphasis::None,
            selection: None,
            search_pattern: None,
            row_budget: None,
        }
    }

    fn rows_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn widget(
        name: &str,
        input: Value,
        output: &str,
        is_error: bool,
        folded: bool,
    ) -> Vec<Line<'static>> {
        let mut buf = Buffer::new();
        buf.push_tool_call("c1", name, input);
        buf.push_tool_result_with_duration("c1", output, is_error, Some(4900));
        if !folded {
            buf.toggle_fold(0);
        }
        let w = ToolPairBlockWidget::from_pair(&buf.blocks()[0], &buf.blocks()[1]).unwrap();
        w.lines(80, &ctx(&Theme::default()))
    }

    fn rows(name: &str, input: Value, output: &str, is_error: bool) -> Vec<String> {
        rows_of(&widget(name, input, output, is_error, true))
    }

    #[test]
    fn from_pair_rejects_non_tool_blocks() {
        let user = Block::User { text: "hi".into() };
        assert!(ToolPairBlockWidget::from_pair(&user, &user).is_none());
    }

    #[test]
    fn read_row_is_one_verb_first_header_with_its_duration() {
        let rows = rows(
            "read",
            json!({"path": "README.md"}),
            "line one\nline two",
            false,
        );
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].contains("\u{2022} Read README.md"), "{rows:?}");
        assert!(rows[0].ends_with("4.9s"), "{rows:?}");
        assert!(!rows[0].contains("line one"), "{rows:?}");
    }

    #[test]
    fn bash_rows_never_show_the_model_labels() {
        let rows = rows(
            "bash",
            json!({"command": "echo hi"}),
            "stdout:\nhi\nthere\n\nexit: 0",
            false,
        );
        assert!(rows[0].contains("Ran echo hi"), "{rows:?}");
        assert!(rows.iter().any(|r| r.ends_with("there")), "{rows:?}");
        for row in &rows {
            assert!(
                !row.contains("stdout:") && !row.contains("exit: 0"),
                "{rows:?}"
            );
        }
    }

    #[test]
    fn folded_bash_shows_the_last_five_lines() {
        let out: Vec<String> = (1..=8).map(|i| format!("line {i}")).collect();
        let text = format!("stdout:\n{}\nexit: 0", out.join("\n"));
        let rows = rows("bash", json!({"command": "seq 8"}), &text, false);
        assert!(rows[1].ends_with("... 3 earlier lines"), "{rows:?}");
        assert!(
            rows[2].ends_with("line 4") && rows[6].ends_with("line 8"),
            "{rows:?}"
        );
    }

    #[test]
    fn failed_bash_shows_its_exit_code() {
        let rows = rows(
            "bash",
            json!({"command": "false"}),
            "stderr:\nboom\nexit: 1",
            true,
        );
        assert!(rows[0].contains("\u{2717} Ran false"), "{rows:?}");
        assert!(rows[0].ends_with("exit 1 \u{b7} 4.9s"), "{rows:?}");
        assert!(rows[1].ends_with("boom"), "{rows:?}");
    }

    #[test]
    fn edit_rows_show_counts_and_the_diff() {
        let rows = rows(
            "edit",
            json!({"path": "a.rs", "old_str": "x", "new_str": "y"}),
            "edited",
            false,
        );
        assert!(rows[0].contains("Edited a.rs (+1 -1)"), "{rows:?}");
        assert!(
            rows[1].ends_with("- x") && rows[2].ends_with("+ y"),
            "{rows:?}"
        );
    }

    #[test]
    fn failed_and_denied_edits_do_not_claim_the_edit() {
        let input = json!({"path": "a.rs", "old_str": "x", "new_str": "y"});
        let failed = rows("edit", input.clone(), "`old_str` not found", true);
        assert!(failed[0].contains("\u{2717} Edit a.rs"), "{failed:?}");
        assert!(!failed[0].contains("(+1 -1)"), "{failed:?}");
        assert!(failed[1].ends_with("`old_str` not found"), "{failed:?}");

        let mut buf = Buffer::new();
        buf.push_tool_call("c1", "edit", input);
        buf.set_tool_phase("c1", ToolPhase::Denied);
        buf.push_tool_result("c1", "denied by user", true);
        let w = ToolPairBlockWidget::from_pair(&buf.blocks()[0], &buf.blocks()[1]).unwrap();
        let denied = rows_of(&w.lines(80, &ctx(&Theme::default())));
        assert!(denied[0].contains("\u{2298} Edit a.rs"), "{denied:?}");
        assert!(denied[0].ends_with("denied"), "{denied:?}");
        assert!(!denied[0].contains("Edited"), "{denied:?}");
    }

    #[test]
    fn unknown_tools_name_themselves_once() {
        let rows = rows("my_tool", json!({"foo": "bar"}), "done", false);
        assert!(rows[0].contains("Called my_tool foo: bar"), "{rows:?}");
        assert_eq!(rows[0].matches("my_tool").count(), 1, "{rows:?}");
        assert!(rows[1].ends_with("done"), "{rows:?}");
    }

    #[test]
    fn a_read_error_is_not_syntax_highlighted() {
        let lines = widget(
            "read",
            json!({"path": "main.rs"}),
            "fn main() { let x = 1; }",
            true,
            false,
        );
        let error_fg = crate::theme::current().tool_error_fg;
        let body = &lines[1];
        for span in body.spans.iter().filter(|s| s.content.contains("fn")) {
            assert_eq!(span.style.fg, Some(error_fg), "{body:?}");
        }
    }

    #[test]
    fn unfolded_rows_never_have_fewer_lines_than_folded() {
        let input = json!({"path": "README.md"});
        let folded = widget("read", input.clone(), "a\nb\nc", false, true);
        let unfolded = widget("read", input, "a\nb\nc", false, false);
        assert!(unfolded.len() > folded.len());
    }
}
