//! `AssistantBlockWidget`: per-block renderer for assistant text.
//!
//! Live blocks render markdown structure as deltas arrive
//! ([`crate::markdown::render_streaming`]: headings, lists, quotes,
//! inline bold/italic/code) but show fenced code as plain dim text,
//! since running syntect over a half-written body 30 times a second
//! is wasted work. Finished blocks run the full
//! [`crate::markdown::render`], which adds syntect highlighting to
//! code fences. Emphasis adds the left-edge marker via
//! `mark_emphasis`.

use ratatui::text::Line;

use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, assistant_style, mark_emphasis};

/// Renders a [`Block::Assistant`] as live markdown (structure now,
/// code highlighted once the turn finishes), with the usual
/// `mark_emphasis` left rule when focused or matching a search.
#[derive(Clone, Debug)]
pub struct AssistantBlockWidget {
    text: String,
    live: bool,
}

impl AssistantBlockWidget {
    /// Construct a widget for an assistant text block.
    ///
    /// `live` mirrors [`crate::buffer::Block::Assistant::live`]: when
    /// `true` the renderer formats markdown but leaves code fences
    /// plain (syntect would re-run every delta); once the turn ends,
    /// set it to `false` so code is highlighted and the cache hits.
    #[must_use]
    pub fn new(text: impl Into<String>, live: bool) -> Self {
        Self {
            text: text.into(),
            live,
        }
    }

    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let body = if self.live {
            crate::markdown::render_streaming(&self.text, assistant_style())
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
    fn lines_yield_at_least_one_row_for_one_line_of_text() {
        let w = AssistantBlockWidget::new("hello", false);
        let theme = Theme::default();
        assert!(!w.lines(40, &ctx(&theme)).is_empty());
    }

    #[test]
    fn lines_include_assistant_text() {
        let w = AssistantBlockWidget::new("hello world", false);
        let theme = Theme::default();
        assert!(
            painted(&w.lines(40, &ctx(&theme))).contains("hello world"),
            "expected assistant text in rendered lines"
        );
    }

    #[test]
    fn focused_lines_start_with_emphasis_marker() {
        let w = AssistantBlockWidget::new("hi", false);
        let theme = Theme::default();
        let mut focused = ctx(&theme);
        focused.emphasis = Emphasis::Focused;
        let rows = w.lines(40, &focused);
        let first = painted(&rows[..1]);
        assert!(
            first.starts_with(Emphasis::Focused.rule_glyph()),
            "expected focus rule prefix, got {first:?}"
        );
    }

    #[test]
    fn focused_lines_paint_rule_on_every_body_row() {
        let long = "a".repeat(120);
        let w = AssistantBlockWidget::new(&long, false);
        let theme = Theme::default();
        let mut focused = ctx(&theme);
        focused.emphasis = Emphasis::Focused;
        let rows = w.lines(20, &focused);
        assert!(rows.len() >= 6, "expected the long line to wrap");
        // Every visual row of body content (everything before the
        // trailing pad row) carries the focus rule glyph.
        let body_rows = rows.len().saturating_sub(1);
        for (y, row) in rows.iter().take(body_rows).enumerate() {
            let text = painted(std::slice::from_ref(row));
            assert!(
                text.starts_with(Emphasis::Focused.rule_glyph()),
                "expected focus rule on body row {y}, got {text:?}"
            );
        }
    }

    #[test]
    fn live_and_settled_lines_are_identical() {
        let live = AssistantBlockWidget::new("plain text", true);
        let settled = AssistantBlockWidget::new("plain text", false);
        let theme = Theme::default();
        assert_eq!(
            painted(&live.lines(40, &ctx(&theme))),
            painted(&settled.lines(40, &ctx(&theme)))
        );
    }
}
