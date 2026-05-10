//! Widget for [`Block::Custom`] - plugin / host-injected blocks
//! whose `kind` the core does not interpret. Renders as a header
//! line plus an indented body (folded blocks show only the header).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, block_to_lines};
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

    fn synthetic(&self) -> Block {
        Block::Custom {
            kind: self.kind.clone(),
            text: self.text.clone(),
            folded: self.folded,
        }
    }
}

impl BlockWidget for CustomBlockWidget {
    fn measure(&self, width: u16) -> u16 {
        let lines = block_to_lines(&self.synthetic(), width, Emphasis::None);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &RenderCtx<'_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        Paragraph::new(self.lines(area.width, ctx)).render(area, buf);
    }

    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        block_to_lines(&self.synthetic(), width, ctx.emphasis)
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
    fn render_paints_kind_and_body() {
        let w = CustomBlockWidget::from_block(&custom_block()).unwrap();
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
        assert!(painted.contains("kage:log"));
        assert!(painted.contains("log payload"));
    }
}
