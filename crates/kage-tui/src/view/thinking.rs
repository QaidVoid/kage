//! `ThinkingBlockWidget`: per-block renderer for hidden
//! chain-of-thought blocks.
//!
//! Renders a `thinking` header line plus, when not folded, one body
//! row per logical thinking line each prefixed with the themed left
//! rule glyph. The whole block flows through `mark_emphasis` so
//! focused / search-matching thinking blocks pick up the standard
//! left-edge accent.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::widget::{BlockWidget, RenderCtx};
use super::{
    DECORATION_MARKER, Emphasis, fold_indicator, header_line, mark_emphasis, plain_lines,
    thinking_style,
};

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
    /// `folded` collapses the body to just the header line.
    #[must_use]
    pub fn new(text: impl Into<String>, folded: bool, live: bool) -> Self {
        Self {
            text: text.into(),
            folded,
            live,
        }
    }

    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        out.push(header_line(
            fold_indicator(self.folded),
            "thinking",
            None,
            thinking_style(),
        ));
        if !self.folded {
            // Each body line gets a left-rule glyph in the thinking
            // fg color so the thinking section reads distinct from
            // assistant text even on terminals that swallow italic.
            // The glyph is decoration so cell-based selection skips
            // it on yank.
            let rule = Span::styled(
                "\u{258e} ",
                Style::default()
                    .fg(crate::theme::current().thinking_fg)
                    .add_modifier(DECORATION_MARKER),
            );
            for body_line in plain_lines(&self.text, thinking_style()) {
                let mut spans = Vec::with_capacity(body_line.spans.len() + 1);
                spans.push(rule.clone());
                spans.extend(body_line.spans);
                out.push(Line::from(spans));
            }
        }
        let _ = (Modifier::empty(), self.live);
        mark_emphasis(out, width, emphasis, None)
    }
}

impl BlockWidget for ThinkingBlockWidget {
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
        }
    }

    #[test]
    fn folded_widget_measures_header_plus_bottom_pad() {
        // PB.7: every non-bubble block gets a trailing pad row, so
        // a folded thinking block (1 header line) measures 2 rows.
        let w = ThinkingBlockWidget::new("a\nb\nc", true, false);
        assert_eq!(
            usize::from(w.measure(40)),
            1 + super::super::widget::BlockPadding::BOTTOM
        );
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
    fn unfocused_render_keeps_gutter_blank_but_reserved() {
        let w = ThinkingBlockWidget::new("body", false, false);
        let theme = Theme::default();
        let area = Rect::new(0, 0, 30, w.measure(30));
        let mut buf = Buffer::empty(area);
        w.render(area, &mut buf, &ctx(&theme));
        // PB.5: even when not focused, column 0 holds a blank cell
        // (no rule glyph) so toggling focus does not shift the body.
        for y in area.top()..area.bottom() {
            let cell = buf[(area.left(), y)].symbol();
            assert!(
                cell == " " || cell == "\u{258e}",
                "row {y} col 0 should be blank space or thinking rule, got {cell:?}"
            );
        }
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
