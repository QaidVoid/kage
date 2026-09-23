//! `ThinkingBlockWidget`: per-block renderer for hidden
//! chain-of-thought blocks.
//!
//! Renders a plain `[thinking]` header plus, when not folded, the
//! thinking text as markdown using the same renderer as assistant
//! replies (`render_streaming` while the turn is live, `render`
//! once it settles so code fences get highlighted), just with no
//! left rule glyph and no fold chevron. The block still flows
//! through `mark_emphasis` for per-row wrapping, the reserved
//! (blank) left column, and the trailing pad, so its height matches
//! the renderer's scroll math and a focused / search-matching block
//! still picks up the standard accent like every other block.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, mark_emphasis, thinking_style};

/// Renders a [`Block::Thinking`] as a plain `[thinking]` header and,
/// when unfolded, its text as markdown via the same renderer as
/// assistant replies (no left rule glyph, no fold chevron).
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
        out.push(Line::from(Span::styled(
            "[thinking]",
            thinking_style().add_modifier(Modifier::BOLD),
        )));
        if !self.folded {
            let body = if self.live {
                crate::markdown::render_streaming(&self.text, thinking_style())
            } else {
                crate::markdown::render(&self.text, thinking_style())
            };
            out.extend(body);
        }
        mark_emphasis(out, width, emphasis, None)
    }
}

impl BlockWidget for ThinkingBlockWidget {
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
    fn folded_widget_measures_header_plus_bottom_pad() {
        // PB.7: every non-bubble block gets a trailing pad row, so
        // a folded thinking block (1 header line) is 2 rows.
        let w = ThinkingBlockWidget::new("a\nb\nc", true, false);
        let theme = Theme::default();
        assert_eq!(
            w.lines(40, &ctx(&theme)).len(),
            1 + super::super::widget::BlockPadding::BOTTOM
        );
    }

    #[test]
    fn unfolded_widget_measures_more_than_one_row() {
        let w = ThinkingBlockWidget::new("body line", false, false);
        let theme = Theme::default();
        assert!(w.lines(40, &ctx(&theme)).len() >= 2);
    }

    #[test]
    fn unfolded_lines_paint_body_text() {
        let w = ThinkingBlockWidget::new("hidden reason", false, false);
        let theme = Theme::default();
        let text = painted(&w.lines(40, &ctx(&theme)));
        assert!(text.contains("hidden reason"), "got {text:?}");
    }

    #[test]
    fn unfocused_lines_keep_gutter_blank_but_reserved() {
        let w = ThinkingBlockWidget::new("body", false, false);
        let theme = Theme::default();
        // PB.5: column 0 is the reserved gutter so toggling focus
        // does not shift the body. Thinking has no visible rule
        // glyph, so unfocused it is always a plain space.
        for row in w.lines(30, &ctx(&theme)) {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.starts_with(' '),
                "row should start with the blank reserved gutter, got {text:?}"
            );
        }
    }

    #[test]
    fn folded_lines_omit_body() {
        let w = ThinkingBlockWidget::new("hidden reason", true, false);
        let theme = Theme::default();
        let text = painted(&w.lines(40, &ctx(&theme)));
        assert!(!text.contains("hidden reason"), "got {text:?}");
    }
}
