//! `UserBlockWidget`: per-block renderer for user prompts.
//!
//! The widget delegates row construction to the existing
//! [`super::user_block_lines`] helper, which produces the tinted
//! "chat bubble" rows (left-edge rule, top/bottom padding, inline
//! emphasis).

use ratatui::text::Line;

use super::user_block_lines;
use super::widget::{BlockWidget, RenderCtx};

/// Renders a [`crate::buffer::Block::User`] as the existing tinted
/// "chat bubble" with a left-edge rule, top/bottom padding rows, and
/// inline emphasis from [`RenderCtx::emphasis`].
///
/// Owns the prompt text so the widget is `'static` and can sit behind
/// `Box<dyn BlockWidget>` in the block registry.
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
    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        user_block_lines(&self.text, width, ctx.emphasis)
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

    #[test]
    fn lines_include_the_prompt_text() {
        let w = UserBlockWidget::new("hello");
        let theme = Theme::default();
        assert!(
            painted(&w.lines(40, &ctx(&theme))).contains("hello"),
            "expected 'hello' to appear somewhere in the bubble"
        );
    }

    #[test]
    fn lines_grow_with_more_input_lines() {
        let one = UserBlockWidget::new("one");
        let three = UserBlockWidget::new("one\ntwo\nthree");
        let theme = Theme::default();
        assert!(
            three.lines(80, &ctx(&theme)).len() > one.lines(80, &ctx(&theme)).len(),
            "more input lines should produce more rows"
        );
    }

    #[test]
    fn focused_lines_start_with_focus_emphasis_glyph() {
        let w = UserBlockWidget::new("focus me");
        let theme = Theme::default();
        let mut focused_ctx = ctx(&theme);
        focused_ctx.emphasis = Emphasis::Focused;
        let row0 = painted(&w.lines(40, &focused_ctx)[..1]);
        assert!(
            row0.starts_with(Emphasis::Focused.rule_glyph()),
            "expected focused rule glyph at row 0 start, got {row0:?}"
        );
    }
}
