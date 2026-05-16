//! `AssistantBlockWidget`: per-block renderer for assistant text.
//!
//! Live blocks skip the markdown parser (the cache would miss on
//! every streamed delta and re-parse a growing body 30 times a
//! second). Finished blocks run through
//! [`crate::markdown::render`], which handles headings, lists,
//! quotes, inline bold/italic/code, and fenced code blocks (syntect-
//! highlighted). Emphasis adds the left-edge marker via
//! `mark_emphasis`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, assistant_style, mark_emphasis, plain_lines};

/// Renders a [`Block::Assistant`] as plain text (while streaming) or
/// fenced-syntax-highlighted text (once the turn finishes), with the
/// usual `mark_emphasis` left rule when focused or matching a search.
#[derive(Clone, Debug)]
pub struct AssistantBlockWidget {
    text: String,
    live: bool,
}

impl AssistantBlockWidget {
    /// Construct a widget for an assistant text block.
    ///
    /// `live` mirrors [`crate::buffer::Block::Assistant::live`]: when
    /// `true` the renderer skips syntect since each delta would
    /// invalidate the cache; once the turn ends, set it to `false`
    /// so the cache hits.
    #[must_use]
    pub fn new(text: impl Into<String>, live: bool) -> Self {
        Self {
            text: text.into(),
            live,
        }
    }

    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let body = if self.live {
            plain_lines(&self.text, assistant_style())
        } else {
            crate::markdown::render(&self.text, assistant_style())
        };
        mark_emphasis(
            body,
            width,
            emphasis,
            Some(crate::theme::current().assistant_rule),
        )
    }
}

impl BlockWidget for AssistantBlockWidget {
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
    fn measure_counts_at_least_one_row_for_one_line_of_text() {
        let w = AssistantBlockWidget::new("hello", false);
        assert!(w.measure(40) >= 1);
    }

    #[test]
    fn render_paints_assistant_text_into_buffer() {
        let w = AssistantBlockWidget::new("hello world", false);
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
            if row.contains("hello world") {
                found = true;
                break;
            }
        }
        assert!(found, "expected assistant text in painted buffer");
    }

    #[test]
    fn focused_render_includes_emphasis_marker() {
        let w = AssistantBlockWidget::new("hi", false);
        let theme = Theme::default();
        let mut focused = ctx(&theme);
        focused.emphasis = Emphasis::Focused;
        let area = Rect::new(0, 0, 40, w.measure(40));
        let mut buf = Buffer::empty(area);
        w.render(area, &mut buf, &focused);
        let mut row = String::new();
        for x in area.left()..area.right() {
            row.push_str(buf[(x, area.top())].symbol());
        }
        assert!(
            row.starts_with(Emphasis::Focused.rule_glyph()),
            "expected focus rule prefix, got {row:?}"
        );
    }

    #[test]
    fn focused_render_paints_rule_on_every_wrapped_row() {
        let long = "a".repeat(120);
        let w = AssistantBlockWidget::new(&long, false);
        let theme = Theme::default();
        let mut focused = ctx(&theme);
        focused.emphasis = Emphasis::Focused;
        let area = Rect::new(0, 0, 20, w.measure(20));
        assert!(area.height >= 6, "expected the long line to wrap");
        let mut buf = Buffer::empty(area);
        w.render(area, &mut buf, &focused);
        // PB.6: every visual row of body content (everything before
        // the trailing pad row) carries the focus rule glyph.
        let body_rows = area.height.saturating_sub(1);
        for y in area.top()..(area.top() + body_rows) {
            assert_eq!(
                buf[(area.left(), y)].symbol(),
                Emphasis::Focused.rule_glyph(),
                "expected focus rule at row {y} col 0"
            );
        }
    }

    #[test]
    fn live_and_settled_widgets_paint_the_same_visible_text() {
        let live = AssistantBlockWidget::new("plain text", true);
        let settled = AssistantBlockWidget::new("plain text", false);
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, live.measure(40).max(settled.measure(40)));
        let mut a = Buffer::empty(area);
        let mut b = Buffer::empty(area);
        live.render(area, &mut a, &ctx(&theme));
        settled.render(area, &mut b, &ctx(&theme));
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                assert_eq!(a[(x, y)].symbol(), b[(x, y)].symbol());
            }
        }
    }
}
