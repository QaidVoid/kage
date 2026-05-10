//! `ThinkingBlockWidget`: per-block renderer for hidden
//! chain-of-thought blocks.
//!
//! Same shim approach as PB.2 / PB.3 assistant: build a synthetic
//! `Block::Thinking` and route through `block_to_lines` so behavior is
//! pixel-identical with the existing renderer. PB.6 replaces the lines
//! path with direct buffer painting.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};

use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, block_to_lines};
use crate::buffer::Block;

/// Renders a [`Block::Thinking`] with its `thinking` header line, the
/// fold indicator, and (when unfolded) one body row per thinking line
/// each prefixed with the themed left rule glyph.
#[derive(Clone, Debug)]
pub struct ThinkingBlockWidget {
    text: String,
    folded: bool,
    live: bool,
}

impl ThinkingBlockWidget {
    /// Construct a widget for a thinking block.
    ///
    /// `folded` collapses the body to just the header line; `live`
    /// mirrors [`Block::Thinking::live`].
    #[must_use]
    pub fn new(text: impl Into<String>, folded: bool, live: bool) -> Self {
        Self {
            text: text.into(),
            folded,
            live,
        }
    }

    fn synthetic_block(&self) -> Block {
        Block::Thinking {
            text: self.text.clone(),
            folded: self.folded,
            live: self.live,
        }
    }
}

impl BlockWidget for ThinkingBlockWidget {
    fn measure(&self, width: u16) -> u16 {
        let lines = block_to_lines(&self.synthetic_block(), width, Emphasis::None);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &RenderCtx<'_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let lines = block_to_lines(&self.synthetic_block(), area.width, ctx.emphasis);
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

    #[test]
    fn folded_widget_measures_one_row() {
        let w = ThinkingBlockWidget::new("a\nb\nc", true, false);
        assert_eq!(w.measure(40), 1);
    }

    #[test]
    fn unfolded_widget_measures_more_than_one_row() {
        let w = ThinkingBlockWidget::new("body line", false, false);
        assert!(w.measure(40) >= 2);
    }

    #[test]
    fn unfolded_render_paints_body_text() {
        let w = ThinkingBlockWidget::new("hidden reason", false, false);
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, w.measure(40));
        let mut buf = Buffer::empty(area);
        w.render(area, &mut buf, &ctx(&theme));
        let mut found = false;
        for y in area.top()..area.bottom() {
            let mut row = String::new();
            for x in area.left()..area.right() {
                row.push_str(buf[(x, y)].symbol());
            }
            if row.contains("hidden reason") {
                found = true;
                break;
            }
        }
        assert!(found, "expected thinking body text in painted buffer");
    }

    #[test]
    fn folded_render_omits_body() {
        let w = ThinkingBlockWidget::new("hidden reason", true, false);
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, w.measure(40));
        let mut buf = Buffer::empty(area);
        w.render(area, &mut buf, &ctx(&theme));
        for y in area.top()..area.bottom() {
            let mut row = String::new();
            for x in area.left()..area.right() {
                row.push_str(buf[(x, y)].symbol());
            }
            assert!(
                !row.contains("hidden reason"),
                "folded thinking should hide body, found {row:?}"
            );
        }
    }
}
