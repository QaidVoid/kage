//! Custom widget for `kage:compaction` blocks.
//!
//! Compaction events from `kage_loop::compact` arrive as a custom
//! block whose payload is `"[compacted: kept N, summarized M]\n<body>"`.
//! The default custom widget renders that as a kind-tagged card with
//! plain body text - readable but visually identical to other custom
//! blocks. Compactions are load-bearing for long sessions, so this
//! widget gives them a dedicated treatment: a small dim header chip
//! with the kept/summarized counts, then the summary body rendered
//! through the same markdown renderer assistant text uses.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, mark_emphasis, prefix_line};
use crate::buffer::Block;

/// Renders a `Block::Custom { kind: "kage:compaction", .. }` as a
/// styled summary card.
#[derive(Clone, Debug)]
pub struct CompactionBlockWidget {
    text: String,
    folded: bool,
}

impl CompactionBlockWidget {
    /// Build from a `Block::Custom`. Returns `None` for any other
    /// block kind (the registry only dispatches `kage:compaction`
    /// blocks here, but a defensive check keeps unrelated callers
    /// safe).
    #[must_use]
    pub fn from_block(block: &Block) -> Option<Self> {
        match block {
            Block::Custom { kind, text, folded } if kind == "kage:compaction" => Some(Self {
                text: text.clone(),
                folded: *folded,
            }),
            _ => None,
        }
    }

    fn parse_counts(first_line: &str) -> Option<(u64, u64)> {
        let inside = first_line.trim().strip_prefix('[')?.strip_suffix(']')?;
        let kept = extract_count(inside, "kept ");
        let summarized = extract_count(inside, "summarized ");
        Some((kept?, summarized?))
    }

    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let (first, body) = match self.text.split_once('\n') {
            Some((head, tail)) => (head, tail),
            None => (self.text.as_str(), ""),
        };

        let header = header_line(first, self.folded);
        let mut out: Vec<Line<'static>> = vec![header];

        if !self.folded && !body.is_empty() {
            let body_style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM);
            for line in crate::markdown::render(body, body_style) {
                out.push(prefix_line("  ", line));
            }
        }

        mark_emphasis(out, width, emphasis)
    }
}

impl BlockWidget for CompactionBlockWidget {
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

fn extract_count(text: &str, marker: &str) -> Option<u64> {
    let idx = text.find(marker)?;
    let after = &text[idx + marker.len()..];
    after
        .split(|c: char| c == ',' || c.is_whitespace())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse::<u64>().ok())
}

fn header_line(first: &str, folded: bool) -> Line<'static> {
    let chip_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM | Modifier::ITALIC);
    let fold = if folded { "+" } else { "-" };
    let counts = CompactionBlockWidget::parse_counts(first);
    let mut spans = vec![
        Span::styled(format!("{fold} "), dim),
        Span::styled(" summary ".to_owned(), chip_style),
    ];
    if let Some((kept, summarized)) = counts {
        spans.push(Span::styled(
            format!("  kept {kept}, summarized {summarized}"),
            dim,
        ));
    } else {
        spans.push(Span::styled(format!("  {first}"), dim));
    }
    Line::from(spans)
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

    fn compaction_block(text: &str) -> Block {
        Block::Custom {
            kind: "kage:compaction".into(),
            text: text.into(),
            folded: false,
        }
    }

    #[test]
    fn from_block_rejects_non_compaction_custom() {
        let other = Block::Custom {
            kind: "kage:log".into(),
            text: "x".into(),
            folded: false,
        };
        assert!(CompactionBlockWidget::from_block(&other).is_none());
    }

    #[test]
    fn parse_counts_extracts_kept_and_summarized() {
        let (k, s) = CompactionBlockWidget::parse_counts("[compacted: kept 4, summarized 12]")
            .expect("should parse");
        assert_eq!((k, s), (4, 12));
    }

    #[test]
    fn parse_counts_handles_no_brackets() {
        assert!(CompactionBlockWidget::parse_counts("nothing useful").is_none());
    }

    #[test]
    fn header_includes_summary_chip_and_counts() {
        let block = compaction_block("[compacted: kept 3, summarized 7]\n# summary\nbody");
        let w = CompactionBlockWidget::from_block(&block).unwrap();
        let lines = w.lines_for(80, Emphasis::None);
        let header_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains("summary"), "got {header_text:?}");
        assert!(header_text.contains("kept 3"), "got {header_text:?}");
        assert!(header_text.contains("summarized 7"), "got {header_text:?}");
    }

    #[test]
    fn folded_block_drops_body_rows() {
        let mut block = compaction_block("[compacted: kept 1, summarized 2]\nbody");
        if let Block::Custom { folded, .. } = &mut block {
            *folded = true;
        }
        let w = CompactionBlockWidget::from_block(&block).unwrap();
        let unfolded = CompactionBlockWidget::from_block(&compaction_block(
            "[compacted: kept 1, summarized 2]\nbody",
        ))
        .unwrap();
        assert!(w.measure(60) < unfolded.measure(60));
    }

    #[test]
    fn render_paints_summary_chip_into_buffer() {
        let block = compaction_block("[compacted: kept 1, summarized 2]\nthe summary");
        let w = CompactionBlockWidget::from_block(&block).unwrap();
        let theme = Theme::default();
        let area = Rect::new(0, 0, 80, w.measure(80));
        let mut buf = Buffer::empty(area);
        w.render(area, &mut buf, &ctx(&theme));
        let mut painted = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                painted.push_str(buf[(x, y)].symbol());
            }
            painted.push('\n');
        }
        assert!(painted.contains("summary"));
        assert!(painted.contains("the summary"));
    }
}
