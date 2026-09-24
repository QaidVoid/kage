//! `ThinkingBlockWidget`: per-block renderer for hidden
//! chain-of-thought blocks.
//!
//! A live block reads `Thinking (Ns)` with the last lines of the
//! stream below it. Once finished and folded it collapses to
//! `Thought for Ns`, or plain `Thought` when replayed from history
//! without timing. Unfolded, the text renders as markdown with the
//! assistant renderer (`render_streaming` while live, `render` once
//! it settles). The block flows through `mark_emphasis` for per-row
//! wrapping and the reserved left column, so a focused or
//! search-matching block picks up the standard accent.

use std::time::Instant;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::tool_view::format_elapsed;
use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, FOCUS_RULE_WIDTH, mark_emphasis, thinking_style, truncate_to_width};
use crate::buffer::Block;

/// Lines of a live folded thinking block shown under its header.
const LIVE_TAIL_LINES: usize = 3;

/// Renders a [`Block::Thinking`] as a timed header and, depending on
/// its state, the tail or the whole of its text.
#[derive(Clone, Debug)]
pub struct ThinkingBlockWidget {
    text: String,
    folded: bool,
    live: bool,
    started_at: Instant,
    duration_ms: Option<u64>,
}

impl ThinkingBlockWidget {
    /// Construct a widget from a [`Block::Thinking`]. `None` for any
    /// other variant.
    #[must_use]
    pub fn from_block(block: &Block) -> Option<Self> {
        match block {
            Block::Thinking {
                text,
                folded,
                live,
                started_at,
                duration_ms,
                ..
            } => Some(Self {
                text: text.clone(),
                folded: *folded,
                live: *live,
                started_at: *started_at,
                duration_ms: *duration_ms,
            }),
            _ => None,
        }
    }

    fn header(&self) -> String {
        if self.live {
            let ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            format!("Thinking ({})", seconds(ms))
        } else {
            match self.duration_ms {
                Some(ms) => format!("Thought for {}", seconds(ms)),
                None => "Thought".to_owned(),
            }
        }
    }

    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let mut out = vec![Line::from(Span::styled(
            self.header(),
            thinking_style().add_modifier(Modifier::BOLD),
        ))];
        if !self.folded {
            out.extend(if self.live {
                crate::markdown::render_streaming(&self.text, thinking_style())
            } else {
                crate::markdown::render(&self.text, thinking_style())
            });
        } else if self.live {
            let room = usize::from(width).saturating_sub(FOCUS_RULE_WIDTH);
            let tail: Vec<&str> = self
                .text
                .lines()
                .rev()
                .filter(|l| !l.trim().is_empty())
                .take(LIVE_TAIL_LINES)
                .collect();
            out.extend(tail.into_iter().rev().map(|l| {
                Line::from(Span::styled(
                    truncate_to_width(l.trim(), room, "..."),
                    thinking_style(),
                ))
            }));
        }
        mark_emphasis(out, width, emphasis)
    }
}

/// Whole seconds, at least one, then minutes past a minute.
fn seconds(ms: u64) -> String {
    if ms < 60_000 {
        format!("{}s", (ms.saturating_add(500) / 1000).max(1))
    } else {
        format_elapsed(ms)
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
    use crate::buffer::Buffer;
    use crate::theme::Theme;

    fn rows(buf: &Buffer) -> Vec<String> {
        let theme = Theme::default();
        let ctx = RenderCtx {
            theme: &theme,
            focused: false,
            emphasis: Emphasis::None,
            selection: None,
            search_pattern: None,
            row_budget: None,
        };
        ThinkingBlockWidget::from_block(&buf.blocks()[0])
            .unwrap()
            .lines(40, &ctx)
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

    fn live(text: &str) -> Buffer {
        let mut buf = Buffer::new();
        buf.append_thinking_delta(text);
        buf
    }

    #[test]
    fn live_thinking_shows_a_timer_and_its_last_three_lines() {
        let buf = live("one\ntwo\n\nthree\nfour");
        assert_eq!(rows(&buf), ["Thinking (1s)", "two", "three", "four"]);
    }

    #[test]
    fn finished_thinking_folds_to_thought_for() {
        let mut buf = live("one\ntwo");
        buf.finish_streaming();
        assert!(matches!(
            buf.blocks()[0],
            Block::Thinking { folded: true, .. }
        ));
        assert_eq!(rows(&buf), ["Thought for 1s"]);
    }

    #[test]
    fn thinking_pinned_while_live_stays_open() {
        let mut buf = live("hidden reason");
        buf.toggle_fold(0);
        buf.finish_streaming();
        let rows = rows(&buf);
        assert!(rows[0].starts_with("Thought for"), "{rows:?}");
        assert!(rows.iter().any(|r| r == "hidden reason"), "{rows:?}");
    }

    #[test]
    fn replayed_thinking_reads_thought() {
        let mut buf = Buffer::new();
        buf.push_thinking("from history");
        assert_eq!(rows(&buf), ["Thought"]);
        buf.toggle_fold(0);
        assert!(rows(&buf).iter().any(|r| r == "from history"));
    }

    #[test]
    fn unfocused_lines_keep_gutter_blank_but_reserved() {
        let mut buf = Buffer::new();
        buf.push_thinking("body");
        buf.toggle_fold(0);
        let theme = Theme::default();
        let ctx = RenderCtx {
            theme: &theme,
            focused: false,
            emphasis: Emphasis::None,
            selection: None,
            search_pattern: None,
            row_budget: None,
        };
        let w = ThinkingBlockWidget::from_block(&buf.blocks()[0]).unwrap();
        for row in w.lines(30, &ctx) {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.starts_with("  "), "reserved gutter, got {text:?}");
        }
    }

    #[test]
    fn seconds_round_and_switch_to_minutes() {
        assert_eq!(seconds(200), "1s");
        assert_eq!(seconds(3_400), "3s");
        assert_eq!(seconds(65_000), "1m 05s");
    }
}
