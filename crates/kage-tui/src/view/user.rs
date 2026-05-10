//! `UserBlockWidget`: per-block renderer for user prompts.
//!
//! PB.2 ports the smallest block kind first to validate the
//! [`BlockWidget`] surface before the bigger ones (assistant, thinking,
//! tool pairs). The widget delegates row construction to the existing
//! [`super::user_block_lines`] helper and paints those rows via
//! ratatui's `Paragraph` widget. PB.6 will replace the lines path with
//! a direct buffer-painting implementation that pre-wraps to exact
//! width and reports an exact `measure()`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};

use super::user_block_lines;
use super::widget::{BlockWidget, RenderCtx};

/// Renders a [`crate::buffer::Block::User`] as the existing tinted
/// "chat bubble" with a left-edge rule, top/bottom padding rows, and
/// inline emphasis from [`RenderCtx::emphasis`].
///
/// Owns the prompt text so the widget is `'static` and can sit behind
/// `Box<dyn BlockWidget>` for the upcoming PB.10 registry.
#[derive(Clone, Debug)]
pub struct UserBlockWidget {
    text: String,
}

impl UserBlockWidget {
    /// Construct a widget for a user prompt with the given text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl BlockWidget for UserBlockWidget {
    fn measure(&self, width: u16) -> u16 {
        let lines = user_block_lines(&self.text, width, super::Emphasis::None);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &RenderCtx<'_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let lines = user_block_lines(&self.text, area.width, ctx.emphasis);
        Paragraph::new(lines).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
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
        }
    }

    #[test]
    fn measure_matches_lines_path_height() {
        let w = UserBlockWidget::new("hello");
        let expected = user_block_lines("hello", 40, Emphasis::None).len();
        assert_eq!(usize::from(w.measure(40)), expected);
    }

    #[test]
    fn measure_grows_with_more_input_lines() {
        let one = UserBlockWidget::new("one");
        let three = UserBlockWidget::new("one\ntwo\nthree");
        assert!(three.measure(80) > one.measure(80));
    }

    #[test]
    fn render_paints_text_inside_the_bubble_interior() {
        let w = UserBlockWidget::new("hello");
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
            if row.contains("hello") {
                found = true;
                break;
            }
        }
        assert!(found, "expected 'hello' to appear somewhere in the bubble");
    }

    #[test]
    fn render_into_zero_height_area_is_a_noop() {
        let w = UserBlockWidget::new("anything");
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 0);
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 1));
        w.render(area, &mut buf, &ctx(&theme));
    }

    #[test]
    fn render_focused_paints_focus_emphasis_glyph() {
        let w = UserBlockWidget::new("focus me");
        let theme = Theme::default();
        let mut focused_ctx = ctx(&theme);
        focused_ctx.emphasis = Emphasis::Focused;
        let area = Rect::new(0, 0, 40, w.measure(40));
        let mut buf = Buffer::empty(area);
        w.render(area, &mut buf, &focused_ctx);
        let mut row0 = String::new();
        for x in area.left()..area.right() {
            row0.push_str(buf[(x, area.top())].symbol());
        }
        assert!(
            row0.starts_with(Emphasis::Focused.rule_glyph()),
            "expected focused rule glyph at row 0 start, got {row0:?}"
        );
    }
}
