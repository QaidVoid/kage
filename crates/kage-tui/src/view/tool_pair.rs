//! `ToolPairBlockWidget`: per-block renderer for a paired tool call
//! and its result (the merged "tool block" the user sees in the buffer).
//!
//! Calls into `tool_pair_to_lines` and its private helper cluster
//! (`truncated_body`, `body_trim_for`, `highlight_read_body_if_applicable`,
//! `input_recap_worth_showing`, `duration_footer`) which live in
//! `view.rs` because they are shared with the bubble layout path.

use std::time::Instant;

use ratatui::text::Line;

use super::tool_pair_to_lines;
use super::widget::{BlockWidget, RenderCtx};
use crate::buffer::Block;

/// Renders one [`Block::ToolCall`] paired with its matching
/// [`Block::ToolResult`] as the merged tool block (header + body
/// preview + duration footer + tinted bubble).
///
/// The widget owns enough data from both sides of the pair to
/// reconstruct the synthetic blocks the existing `tool_pair_to_lines`
/// helper expects. Unpaired tool calls (still running) and unpaired
/// tool results stay on the standalone `block_to_lines` path.
#[derive(Clone, Debug)]
pub struct ToolPairBlockWidget {
    call_id: String,
    name: String,
    input_summary: String,
    input_pretty: String,
    folded: bool,
    output: String,
    is_error: bool,
    duration_ms: Option<u64>,
}

impl ToolPairBlockWidget {
    /// Construct a widget from a paired call and result.
    ///
    /// Returns `None` when either block is the wrong variant; callers
    /// who already verified the pair from `Buffer` should `unwrap()`.
    #[must_use]
    pub fn from_pair(call: &Block, result: &Block) -> Option<Self> {
        let (call_id, name, input_summary, input_pretty, folded) = match call {
            Block::ToolCall {
                call_id,
                name,
                input_summary,
                input_pretty,
                folded,
                ..
            } => (
                call_id.clone(),
                name.clone(),
                input_summary.clone(),
                input_pretty.clone(),
                *folded,
            ),
            _ => return None,
        };
        let (output, is_error, duration_ms) = match result {
            Block::ToolResult {
                output,
                is_error,
                duration_ms,
                ..
            } => (output.clone(), *is_error, *duration_ms),
            _ => return None,
        };
        Some(Self {
            call_id,
            name,
            input_summary,
            input_pretty,
            folded,
            output,
            is_error,
            duration_ms,
        })
    }

    fn synthetic_call(&self) -> Block {
        Block::ToolCall {
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            input_summary: self.input_summary.clone(),
            input_pretty: self.input_pretty.clone(),
            folded: self.folded,
            started_at: Instant::now(),
        }
    }

    fn synthetic_result(&self) -> Block {
        Block::ToolResult {
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            output: self.output.clone(),
            is_error: self.is_error,
            folded: self.folded,
            duration_ms: self.duration_ms,
        }
    }
}

impl BlockWidget for ToolPairBlockWidget {
    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        tool_pair_to_lines(
            &self.synthetic_call(),
            &self.synthetic_result(),
            width,
            ctx.emphasis,
            ctx.row_budget,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::super::Emphasis;
    use super::*;
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

    fn painted(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn pair(folded: bool, is_error: bool) -> (Block, Block) {
        let call = Block::ToolCall {
            call_id: "c1".into(),
            name: "read".into(),
            input_summary: "README.md".into(),
            input_pretty: "{\"path\":\"README.md\"}".into(),
            folded,
            started_at: Instant::now(),
        };
        let result = Block::ToolResult {
            call_id: "c1".into(),
            name: "read".into(),
            output: "line one\nline two\nline three".into(),
            is_error,
            folded,
            duration_ms: Some(42),
        };
        (call, result)
    }

    #[test]
    fn from_pair_rejects_non_tool_blocks() {
        let user = Block::User { text: "hi".into() };
        let result = Block::ToolResult {
            call_id: "c".into(),
            name: "x".into(),
            output: "y".into(),
            is_error: false,
            folded: true,
            duration_ms: None,
        };
        assert!(ToolPairBlockWidget::from_pair(&user, &result).is_none());
    }

    #[test]
    fn lines_paired_block_returns_at_least_header_row() {
        let (call, result) = pair(true, false);
        let w = ToolPairBlockWidget::from_pair(&call, &result).unwrap();
        let theme = Theme::default();
        assert!(!w.lines(60, &ctx(&theme)).is_empty());
    }

    #[test]
    fn folded_pair_lines_never_exceed_unfolded() {
        let (cf, rf) = pair(true, false);
        let (cu, ru) = pair(false, false);
        let folded = ToolPairBlockWidget::from_pair(&cf, &rf).unwrap();
        let unfolded = ToolPairBlockWidget::from_pair(&cu, &ru).unwrap();
        let theme = Theme::default();
        assert!(
            unfolded.lines(60, &ctx(&theme)).len() >= folded.lines(60, &ctx(&theme)).len(),
            "folding should not add rows"
        );
    }

    #[test]
    fn lines_unfolded_pair_include_body_lines() {
        let (call, result) = pair(false, false);
        let w = ToolPairBlockWidget::from_pair(&call, &result).unwrap();
        let theme = Theme::default();
        let text = painted(&w.lines(60, &ctx(&theme)));
        assert!(
            text.contains("read"),
            "expected tool name in rendered lines: {text:?}"
        );
        assert!(
            text.contains("line one"),
            "expected body text in rendered lines: {text:?}"
        );
    }

    #[test]
    fn lines_folded_pair_keep_tool_name() {
        let (call, result) = pair(true, false);
        let w = ToolPairBlockWidget::from_pair(&call, &result).unwrap();
        let theme = Theme::default();
        let text = painted(&w.lines(60, &ctx(&theme)));
        assert!(text.contains("read"), "expected tool name even when folded");
    }

    #[test]
    fn lines_error_pair_include_error_marker() {
        let (call, result) = pair(false, true);
        let w = ToolPairBlockWidget::from_pair(&call, &result).unwrap();
        let theme = Theme::default();
        let text = painted(&w.lines(60, &ctx(&theme)));
        assert!(
            text.contains("ERROR"),
            "error pair should display ERROR marker, got: {text:?}"
        );
    }
}
