//! Widget for an unpaired tool result (rare; usually a result is
//! consumed via [`super::ToolPairBlockWidget`] and skipped at the
//! per-block layer). Renders a header + truncated body in the
//! mark-emphasis style.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use super::widget::{BlockWidget, RenderCtx};
use super::{
    Emphasis, mark_emphasis, prefix_line, tool_error_style, tool_result_header_line,
    tool_result_style, truncated_body_lines,
};
use crate::buffer::Block;

/// Renders an orphan [`crate::buffer::Block::ToolResult`] (a tool
/// result whose matching [`crate::buffer::Block::ToolCall`] is
/// missing).
#[derive(Clone, Debug)]
pub struct ToolResultAloneBlockWidget {
    name: String,
    output: String,
    is_error: bool,
    folded: bool,
}

impl ToolResultAloneBlockWidget {
    /// Construct a widget for an orphan tool result.
    #[must_use]
    pub fn from_block(block: &Block) -> Option<Self> {
        match block {
            Block::ToolResult {
                name,
                output,
                is_error,
                folded,
                ..
            } => Some(Self {
                name: name.clone(),
                output: output.clone(),
                is_error: *is_error,
                folded: *folded,
            }),
            _ => None,
        }
    }

    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        out.push(tool_result_header_line(
            self.folded,
            &self.name,
            &self.output,
            self.is_error,
        ));
        if !self.folded {
            let body_style = if self.is_error {
                tool_error_style()
            } else {
                tool_result_style()
            };
            for body_line in truncated_body_lines(&self.output, body_style) {
                out.push(prefix_line("  ", body_line));
            }
        }
        mark_emphasis(out, width, emphasis, None)
    }
}

impl BlockWidget for ToolResultAloneBlockWidget {
    fn measure(&self, width: u16) -> u16 {
        u16::try_from(self.lines_for(width, Emphasis::None).len()).unwrap_or(u16::MAX)
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &RenderCtx<'_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        Paragraph::new(self.lines(area.width, ctx)).render(area, buf);
    }

    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        self.lines_for(width, ctx.emphasis)
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
            row_budget: None,
        }
    }

    fn orphan_result() -> Block {
        Block::ToolResult {
            call_id: "missing".into(),
            name: "find".into(),
            output: "result body".into(),
            is_error: false,
            folded: false,
            duration_ms: Some(5),
        }
    }

    #[test]
    fn from_block_rejects_non_result_variants() {
        let user = Block::User { text: "hi".into() };
        assert!(ToolResultAloneBlockWidget::from_block(&user).is_none());
    }

    #[test]
    fn render_paints_tool_name_and_body() {
        let w = ToolResultAloneBlockWidget::from_block(&orphan_result()).unwrap();
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
        assert!(painted.contains("find"));
        assert!(painted.contains("result body"));
    }
}
