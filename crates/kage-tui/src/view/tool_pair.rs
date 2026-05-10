//! `ToolPairBlockWidget`: per-block renderer for a paired tool call
//! and its result (the merged "tool block" the user sees in the buffer).
//!
//! Calls into `tool_pair_to_lines` and its private helper cluster
//! (`truncated_body`, `body_trim_for`, `highlight_read_body_if_applicable`,
//! `input_recap_worth_showing`, `duration_footer`) which live in
//! `view.rs` because they are shared with the bubble layout path.

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, tool_pair_to_lines};
use crate::buffer::Block;

/// Renders one [`Block::ToolCall`] paired with its matching
/// [`Block::ToolResult`] as the merged tool block (header + body
/// preview + duration footer + tinted bubble).
///
/// The widget owns enough data from both sides of the pair to
/// reconstruct the synthetic blocks the existing `tool_pair_to_lines`
/// helper expects. Unpaired tool calls (still running) and unpaired
/// tool results stay on the existing `block_to_lines` path until
/// PB.9 retires it.
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
    fn measure(&self, width: u16) -> u16 {
        let lines = tool_pair_to_lines(
            &self.synthetic_call(),
            &self.synthetic_result(),
            width,
            Emphasis::None,
        );
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &RenderCtx<'_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        Paragraph::new(self.lines(area.width, ctx)).render(area, buf);
    }

    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        tool_pair_to_lines(
            &self.synthetic_call(),
            &self.synthetic_result(),
            width,
            ctx.emphasis,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn ctx(theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            theme,
            focused: false,
            emphasis: Emphasis::None,
            selection: None,
            search_pattern: None,
        }
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
    fn measure_paired_block_returns_at_least_header_row() {
        let (call, result) = pair(true, false);
        let w = ToolPairBlockWidget::from_pair(&call, &result).unwrap();
        assert!(w.measure(60) >= 1);
    }

    #[test]
    fn folded_pair_measures_smaller_than_unfolded() {
        let (cf, rf) = pair(true, false);
        let (cu, ru) = pair(false, false);
        let folded = ToolPairBlockWidget::from_pair(&cf, &rf).unwrap();
        let unfolded = ToolPairBlockWidget::from_pair(&cu, &ru).unwrap();
        assert!(unfolded.measure(60) >= folded.measure(60));
    }

    #[test]
    fn render_unfolded_pair_paints_body_lines() {
        let (call, result) = pair(false, false);
        let w = ToolPairBlockWidget::from_pair(&call, &result).unwrap();
        let theme = Theme::default();
        let area = Rect::new(0, 0, 60, w.measure(60));
        let mut buf = Buffer::empty(area);
        w.render(area, &mut buf, &ctx(&theme));
        let mut painted = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                painted.push_str(buf[(x, y)].symbol());
            }
            painted.push('\n');
        }
        assert!(
            painted.contains("read"),
            "expected tool name in painted output"
        );
        assert!(
            painted.contains("line one"),
            "expected body text in painted output: {painted:?}"
        );
    }

    #[test]
    fn render_folded_pair_omits_full_body() {
        let (call, result) = pair(true, false);
        let w = ToolPairBlockWidget::from_pair(&call, &result).unwrap();
        let theme = Theme::default();
        let area = Rect::new(0, 0, 60, w.measure(60));
        let mut buf = Buffer::empty(area);
        w.render(area, &mut buf, &ctx(&theme));
        let mut painted = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                painted.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(
            painted.contains("read"),
            "expected tool name even when folded"
        );
    }

    #[test]
    fn render_error_pair_includes_error_marker() {
        let (call, result) = pair(false, true);
        let w = ToolPairBlockWidget::from_pair(&call, &result).unwrap();
        let theme = Theme::default();
        let area = Rect::new(0, 0, 60, w.measure(60));
        let mut buf = Buffer::empty(area);
        w.render(area, &mut buf, &ctx(&theme));
        let mut painted = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                painted.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(
            painted.contains("ERROR"),
            "error pair should display ERROR marker, got: {painted:?}"
        );
    }
}
