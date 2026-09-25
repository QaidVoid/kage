//! Custom widget for `kage:compaction` blocks.
//!
//! Compaction events from the agent loop arrive as a custom block whose
//! payload is `"Compacted history (M messages summarized, N kept)\n<body>"`; a
//! replayed session carries only the framed body. The widget shows one
//! `Compacted history` line and, when unfolded, the summary rendered
//! through the markdown renderer assistant text uses.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, mark_emphasis, prefix_line};
use crate::buffer::Block;

/// First line of a live compaction payload.
const HEADER: &str = "Compacted history";

/// Renders a `Block::Custom { kind: "kage:compaction", .. }` as a
/// header line over a foldable summary.
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

    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let (header, framed) = match self.text.split_once('\n') {
            Some((first, rest)) if first.starts_with(HEADER) => (first, rest),
            _ if self.text.starts_with(HEADER) => (self.text.as_str(), ""),
            _ => (HEADER, self.text.as_str()),
        };
        let t = crate::theme::current();
        let mut out = vec![Line::from(Span::styled(
            header.to_owned(),
            Style::default().fg(t.muted_fg).add_modifier(Modifier::BOLD),
        ))];
        if !self.folded {
            let body_style = Style::default().fg(t.assistant_fg);
            for line in crate::markdown::render(&strip_summary_framing(framed), body_style) {
                out.push(prefix_line("  ", line));
            }
        }
        mark_emphasis(out, width, emphasis)
    }
}

impl BlockWidget for CompactionBlockWidget {
    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        self.lines_for(width, ctx.emphasis)
    }
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

    fn rows(text: &str, folded: bool) -> Vec<String> {
        let block = Block::Custom {
            kind: "kage:compaction".into(),
            text: text.into(),
            folded,
        };
        let theme = Theme::default();
        let ctx = RenderCtx {
            theme: &theme,
            focused: false,
            emphasis: Emphasis::None,
            selection: None,
            search_pattern: None,
            row_budget: None,
        };
        CompactionBlockWidget::from_block(&block)
            .unwrap()
            .lines(80, &ctx)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim()
                    .to_owned()
            })
            .collect()
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
    fn folded_compaction_is_one_header_line() {
        let text = "Compacted history (7 messages summarized, 3 kept)\n<summary>\nbody\n</summary>";
        assert_eq!(
            rows(text, true),
            ["Compacted history (7 messages summarized, 3 kept)"]
        );
    }

    #[test]
    fn unfolded_compaction_shows_the_summary_without_framing() {
        let text =
            "Compacted history (2 messages summarized, 1 kept)\n<summary>\nthe summary\n</summary>";
        let rows = rows(text, false);
        assert!(rows.iter().any(|r| r == "the summary"), "{rows:?}");
        assert!(rows.iter().all(|r| !r.contains("<summary>")), "{rows:?}");
    }

    #[test]
    fn replayed_compaction_gets_the_plain_header() {
        let rows = rows("prefix\n\n<summary>\n# Title\n</summary>", true);
        assert_eq!(rows, ["Compacted history"]);
    }

    #[test]
    fn strip_summary_framing_removes_wrapper_and_prefix() {
        let raw = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n# Title\n- item\n</summary>";
        let stripped = strip_summary_framing(raw);
        assert!(stripped.starts_with("# Title"), "got {stripped:?}");
        assert!(!stripped.contains("</summary>"), "got {stripped:?}");
    }

    #[test]
    fn strip_summary_framing_no_op_when_markers_missing() {
        let raw = "plain text with no wrapper";
        assert_eq!(strip_summary_framing(raw), raw);
    }
}
