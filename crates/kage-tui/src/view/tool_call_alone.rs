//! Widget for an unpaired in-flight tool call (rendered as the
//! "running..." pending bubble before its [`crate::buffer::Block::ToolResult`]
//! arrives).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, fold_indicator, plain_lines, tool_call_style, wrap_in_bubble_focused};
use crate::buffer::Block;

/// Renders the [`crate::buffer::Block::ToolCall`] standalone bubble:
/// the agent has invoked a tool but we have not yet seen the
/// matching result, so the body shows the pretty-printed input and
/// a `running...` marker.
#[derive(Clone, Debug)]
pub struct ToolCallAloneBlockWidget {
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
                name,
                input_summary,
                input_pretty,
                folded,
                ..
            } => Some(Self {
                name: name.clone(),
                input_summary: input_summary.clone(),
                input_pretty: input_pretty.clone(),
                folded: *folded,
            }),
            _ => None,
        }
    }

    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        let style = tool_call_style();
        let mut content: Vec<Line<'static>> = Vec::new();
        let mut header_spans = vec![
            Span::styled(
                format!("{} ", fold_indicator(self.folded)),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(self.name.clone(), style.add_modifier(Modifier::BOLD)),
        ];
        if !self.input_summary.is_empty() {
            header_spans.push(Span::raw(" "));
            header_spans.push(Span::styled(self.input_summary.clone(), style));
        }
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled("running...".to_owned(), dim));
        content.push(Line::from(header_spans));
        if !self.folded {
            content.push(Line::raw(""));
            for body_line in plain_lines(&self.input_pretty, style) {
                content.push(body_line);
            }
        }
        let theme = crate::theme::current();
        wrap_in_bubble_focused(
            content,
            theme.tool_pending_rule,
            theme.tool_pending_bg,
            width,
            emphasis,
            None,
        )
    }
}

impl BlockWidget for ToolCallAloneBlockWidget {
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
    use std::time::Instant;

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
