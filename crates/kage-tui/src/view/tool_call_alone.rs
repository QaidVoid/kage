//! Widget for an unpaired in-flight tool call (rendered as the
//! "running..." pending bubble before its [`Block::ToolResult`]
//! arrives).
//!
//! Same shim approach as the other widgets: delegate to the existing
//! `block_to_lines` matcher with a synthetic [`Block::ToolCall`].

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};

use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, block_to_lines};
use crate::buffer::Block;

/// Renders the [`Block::ToolCall`] standalone bubble (the agent has
/// invoked a tool but we have not yet seen the matching result).
#[derive(Clone, Debug)]
pub struct ToolCallAloneBlockWidget {
    call_id: String,
    name: String,
    input_summary: String,
    input_pretty: String,
    folded: bool,
}

impl ToolCallAloneBlockWidget {
    /// Construct a widget for a not-yet-paired tool call.
    #[must_use]
    pub fn from_block(block: &Block) -> Option<Self> {
        match block {
            Block::ToolCall {
                call_id,
                name,
                input_summary,
                input_pretty,
                folded,
                ..
            } => Some(Self {
                call_id: call_id.clone(),
                name: name.clone(),
                input_summary: input_summary.clone(),
                input_pretty: input_pretty.clone(),
                folded: *folded,
            }),
            _ => None,
        }
    }

    fn synthetic(&self) -> Block {
        Block::ToolCall {
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            input_summary: self.input_summary.clone(),
            input_pretty: self.input_pretty.clone(),
            folded: self.folded,
            started_at: Instant::now(),
        }
    }
}

impl BlockWidget for ToolCallAloneBlockWidget {
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

    fn pending_block() -> Block {
        Block::ToolCall {
            call_id: "c1".into(),
            name: "bash".into(),
            input_summary: "ls -la".into(),
            input_pretty: "{\"cmd\":\"ls -la\"}".into(),
            folded: false,
            started_at: Instant::now(),
        }
    }

    #[test]
    fn from_block_rejects_non_tool_call_variants() {
        let user = Block::User { text: "hi".into() };
        assert!(ToolCallAloneBlockWidget::from_block(&user).is_none());
    }

    #[test]
    fn render_paints_running_marker_and_tool_name() {
        let w = ToolCallAloneBlockWidget::from_block(&pending_block()).unwrap();
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
        assert!(painted.contains("bash"));
        assert!(painted.contains("running"));
    }
}
