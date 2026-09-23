//! Widget for [`Block::Custom`] - plugin / host-injected blocks
//! whose `kind` the core does not interpret. Renders as a header
//! line plus an indented body (folded blocks show only the header).

use ratatui::text::Line;

use super::widget::{BlockWidget, RenderCtx};
use super::{
    Emphasis, custom_style, fold_indicator, header_line, mark_emphasis, plain_lines, prefix_line,
};
use crate::buffer::Block;

/// Renders a [`Block::Custom`] using the default header+body layout.
/// Plugins that want a different look register their own
/// [`super::BlockFactory`] under the same `kind` via
/// [`super::BlockRenderer::set_custom`].
#[derive(Clone, Debug)]
pub struct CustomBlockWidget {
    kind: String,
    text: String,
    folded: bool,
}

impl CustomBlockWidget {
    /// Construct a widget from a [`Block::Custom`].
    #[must_use]
    pub fn from_block(block: &Block) -> Option<Self> {
        match block {
            Block::Custom { kind, text, folded } => Some(Self {
                kind: kind.clone(),
                text: text.clone(),
                folded: *folded,
            }),
            _ => None,
        }
    }

    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        out.push(header_line(
            fold_indicator(self.folded),
            &self.kind,
            None,
            custom_style(),
        ));
        if !self.folded {
            for body_line in plain_lines(&self.text, custom_style()) {
                out.push(prefix_line("  ", body_line));
            }
        }
        mark_emphasis(out, width, emphasis, None)
    }
}

impl BlockWidget for CustomBlockWidget {
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

    fn custom_block() -> Block {
        Block::Custom {
            kind: "kage:log".into(),
            text: "log payload".into(),
            folded: false,
        }
    }

    #[test]
    fn from_block_rejects_non_custom_variants() {
        let user = Block::User { text: "hi".into() };
        assert!(CustomBlockWidget::from_block(&user).is_none());
    }

    #[test]
    fn lines_paint_kind_and_body() {
        let w = CustomBlockWidget::from_block(&custom_block()).unwrap();
        let theme = Theme::default();
        let text = painted(&w.lines(60, &ctx(&theme)));
        assert!(text.contains("kage:log"), "got {text:?}");
        assert!(text.contains("log payload"), "got {text:?}");
    }
}
