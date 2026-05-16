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
}

impl CompactionBlockWidget {
    /// Build from a `Block::Custom`. Returns `None` for any other
    /// block kind (the registry only dispatches `kage:compaction`
    /// blocks here, but a defensive check keeps unrelated callers
    /// safe). The block's `folded` flag is intentionally ignored:
    /// compaction summaries are always rendered fully expanded.
    #[must_use]
    pub fn from_block(block: &Block) -> Option<Self> {
        match block {
            Block::Custom { kind, text, .. } if kind == "kage:compaction" => {
                Some(Self { text: text.clone() })
            }
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
        // Two on-the-wire shapes reach this widget:
        //   live event: `[compacted: kept N, summarized M]\n<framed body>`
        //   replayed:   `<framed body>` (no counts header)
        // Strip the counts header when present, then unwrap the
        // `<summary>...</summary>` framing.
        let (counts_line, framed) = match self.text.lines().next() {
            Some(first) if first.trim().starts_with('[') && first.contains("compacted") => {
                let tail = self
                    .text
                    .get(first.len()..)
                    .unwrap_or("")
                    .trim_start_matches('\n');
                (Some(first), tail)
            }
            _ => (None, self.text.as_str()),
        };

        let mut out: Vec<Line<'static>> = vec![header_line(counts_line)];
        let unwrapped = strip_summary_framing(framed);
        if !unwrapped.is_empty() {
            let body_style = Style::default().fg(Color::White);
            for line in crate::markdown::render(&unwrapped, body_style) {
                out.push(prefix_line("  ", line));
            }
        }

        mark_emphasis(out, width, emphasis, None)
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

fn header_line(counts_source: Option<&str>) -> Line<'static> {
    let label_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM | Modifier::ITALIC);
    let mut spans = vec![
        Span::styled("\u{2261} ".to_owned(), label_style),
        Span::styled("summary".to_owned(), label_style),
    ];
    if let Some(first) = counts_source
        && let Some((kept, summarized)) = CompactionBlockWidget::parse_counts(first)
    {
        spans.push(Span::styled(
            format!("  kept {kept}, summarized {summarized}"),
            dim,
        ));
    }
    Line::from(spans)
}

/// Drop the `<summary>...</summary>` framing and the prefix sentence
/// the loop inserts before persisting the synthetic message. The
/// resulting text is the model's actual summary content.
fn strip_summary_framing(text: &str) -> String {
    let start_marker = "<summary>";
    let end_marker = "</summary>";
    let after_open = match text.find(start_marker) {
        Some(i) => &text[i + start_marker.len()..],
        None => text,
    };
    let body = match after_open.find(end_marker) {
        Some(i) => &after_open[..i],
        None => after_open,
    };
    body.trim_matches('\n').to_owned()
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
    fn folded_flag_is_ignored_always_renders_body() {
        let mut block = compaction_block("[compacted: kept 1, summarized 2]\nbody");
        if let Block::Custom { folded, .. } = &mut block {
            *folded = true;
        }
        let folded_w = CompactionBlockWidget::from_block(&block).unwrap();
        let unfolded_w = CompactionBlockWidget::from_block(&compaction_block(
            "[compacted: kept 1, summarized 2]\nbody",
        ))
        .unwrap();
        assert_eq!(
            folded_w.measure(60),
            unfolded_w.measure(60),
            "compaction summary should not honour the folded flag"
        );
    }

    #[test]
    fn strip_summary_framing_removes_wrapper_and_prefix() {
        let raw = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n# Title\n- item\n</summary>";
        let stripped = strip_summary_framing(raw);
        assert!(stripped.starts_with("# Title"), "got {stripped:?}");
        assert!(!stripped.contains("<summary>"), "got {stripped:?}");
        assert!(!stripped.contains("</summary>"), "got {stripped:?}");
    }

    #[test]
    fn strip_summary_framing_no_op_when_markers_missing() {
        let raw = "plain text with no wrapper";
        assert_eq!(strip_summary_framing(raw), raw);
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
