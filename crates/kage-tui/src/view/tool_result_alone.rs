//! Widget for an unpaired tool result (rare; usually a result is
//! consumed via [`super::ToolPairBlockWidget`] and skipped at the
//! per-block layer). The lines path renders this on the assistant /
//! mark-emphasis style with a header + body.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};

use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, block_to_lines};
use crate::buffer::Block;

/// Renders an orphan [`Block::ToolResult`] (a tool result whose
/// matching [`Block::ToolCall`] is missing).
#[derive(Clone, Debug)]
pub struct ToolResultAloneBlockWidget {
    call_id: String,
    name: String,
    output: String,
    is_error: bool,
    folded: bool,
    duration_ms: Option<u64>,
}

impl ToolResultAloneBlockWidget {
    /// Construct a widget for an orphan tool result.
    #[must_use]
    pub fn from_block(block: &Block) -> Option<Self> {
        match block {
            Block::ToolResult {
                call_id,
                name,
                output,
                is_error,
                folded,
                duration_ms,
            } => Some(Self {
                call_id: call_id.clone(),
                name: name.clone(),
                output: output.clone(),
                is_error: *is_error,
                folded: *folded,
                duration_ms: *duration_ms,
            }),
            _ => None,
        }
    }

    fn synthetic(&self) -> Block {
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

impl BlockWidget for ToolResultAloneBlockWidget {
    fn measure(&self, width: u16) -> u16 {
        let lines = block_to_lines(&self.synthetic(), width, Emphasis::None);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &RenderCtx<'_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let lines = block_to_lines(&self.synthetic(), area.width, ctx.emphasis);
        Paragraph::new(lines).render(area, buf);
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
