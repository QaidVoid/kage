//! Markdown-to-ratatui renderer for assistant text.
//!
//! Walks a `pulldown_cmark::Parser` event stream and emits styled
//! [`ratatui::text::Line`]s the assistant block widget paints. Fenced
//! code blocks are passed through to [`crate::syntax::highlight_with_lang`]
//! so syntect runs on languages we have grammars for; everything else
//! is plain styled text.
//!
//! The renderer is line-oriented: it buffers spans of the current line
//! and flushes a [`Line`] whenever the parser emits a paragraph break,
//! a heading, a list item, or an explicit `SoftBreak` / `HardBreak`.
//! Adjacent inline styles (bold, italic, inline code) stack via a
//! small style state.
//!
//! Not supported by design (yet): images, tables, footnotes, HTML
//! passthrough, task lists, autolinks. They render as the raw text the
//! parser yields so users still see content rather than a silent drop.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::syntax::{highlight_with_lang, plain_lines_styled};

/// Source range stand-in for a rendered line that maps to no source
/// (a blank separator, the empty-input fallback). Callers treat a
/// `start == usize::MAX` range as "no raw text for this row".
const NO_SRC: Range<usize> = usize::MAX..usize::MAX;

/// Convert `text` into a vector of styled [`Line`]s by walking the
/// `CommonMark` event stream. `fallback` is the base text style for
/// paragraphs and list items; headings, code, and quotes layer their
/// own modifiers on top.
#[must_use]
pub fn render(text: &str, fallback: Style) -> Vec<Line<'static>> {
    render_with(text, fallback, true)
}

/// Render like [`render`], but also return, per emitted line, the
/// byte [`Range`] of `text` it came from. A line that maps to no
/// source (blank separators) carries [`NO_SRC`]. Used by the yank
/// path to turn a rendered-row selection back into the exact raw
/// markdown substring, so copying part of a response gives verbatim
/// source, never the reflowed render.
#[must_use]
pub fn render_with_src(text: &str, fallback: Style) -> Vec<(Line<'static>, Range<usize>)> {
    let (lines, src) = render_parts(text, fallback, true);
    lines.into_iter().zip(src).collect()
}

/// Like [`render`], but fenced code is shown as plain dim text
/// instead of syntect-highlighted. Used for the still-streaming
/// assistant block: markdown structure (headings, lists, quotes,
/// emphasis) renders live as deltas arrive, while running syntect
/// over a half-written code body ~30x/sec is deferred until the
/// turn settles and [`render`] takes over.
#[must_use]
pub fn render_streaming(text: &str, fallback: Style) -> Vec<Line<'static>> {
    render_with(text, fallback, false)
}

fn render_with(text: &str, fallback: Style, highlight_code: bool) -> Vec<Line<'static>> {
    render_parts(text, fallback, highlight_code).0
}

fn render_parts(
    text: &str,
    fallback: Style,
    highlight_code: bool,
) -> (Vec<Line<'static>>, Vec<Range<usize>>) {
    let mut state = RenderState::new(fallback);
    state.highlight_code = highlight_code;
    for (event, range) in Parser::new(text).into_offset_iter() {
        state.handle(event, range);
    }
    state.finish()
}

struct RenderState {
    lines: Vec<Line<'static>>,
    /// Source byte range per entry in `lines`, kept in lockstep.
    src: Vec<Range<usize>>,
    current: Vec<Span<'static>>,
    fallback: Style,
    style_stack: Vec<Style>,
    list_stack: Vec<ListFrame>,
    in_code_block: Option<String>,
    code_body: String,
    pending_blank: bool,
    has_block_content: bool,
    highlight_code: bool,
    /// Union of every event source range seen since the last emitted
    /// line; becomes that line's range when it flushes, then resets.
    acc: Option<Range<usize>>,
}

struct ListFrame {
    ordered_index: Option<u64>,
    indent: usize,
}

impl RenderState {
    fn new(fallback: Style) -> Self {
        Self {
            lines: Vec::new(),
            src: Vec::new(),
            current: Vec::new(),
            fallback,
            style_stack: vec![fallback],
            list_stack: Vec::new(),
            in_code_block: None,
            code_body: String::new(),
            pending_blank: false,
            has_block_content: false,
            highlight_code: true,
            acc: None,
        }
    }

    /// Fold an event's source range into the line accumulator.
    fn note(&mut self, range: Range<usize>) {
        if range.start == usize::MAX {
            return;
        }
        self.acc = Some(match self.acc.take() {
            Some(a) => a.start.min(range.start)..a.end.max(range.end),
            None => range,
        });
    }

    /// Push `line` with the accumulated source range, then reset the
    /// accumulator so the next line starts fresh.
    fn emit_line(&mut self, line: Line<'static>) {
        let range = self.acc.take().unwrap_or(NO_SRC);
        self.lines.push(line);
        self.src.push(range);
    }

    /// Push `line` with an explicit source range, leaving the
    /// accumulator untouched (for multi-line structural emits like a
    /// fenced block, where every row shares the block's range).
    fn emit_line_src(&mut self, line: Line<'static>, range: Range<usize>) {
        self.lines.push(line);
        self.src.push(range);
    }

    fn current_style(&self) -> Style {
        *self.style_stack.last().unwrap_or(&self.fallback)
    }

    fn push_text(&mut self, text: String, style: Style) {
        if text.is_empty() {
            return;
        }
        self.current.push(Span::styled(text, style));
    }

    fn flush_line(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let line = std::mem::take(&mut self.current);
        self.emit_line(Line::from(line));
    }

    /// Emit one blank line between adjacent block elements unless we
    /// are at the very top of the output. Coalesced so back-to-back
    /// `End(Paragraph)` / `Start(Heading)` doesn't double-space.
    fn emit_paragraph_break(&mut self) {
        if !self.has_block_content {
            return;
        }
        self.pending_blank = true;
    }

    fn maybe_emit_pending_blank(&mut self) {
        if self.pending_blank {
            self.emit_line_src(Line::from(""), NO_SRC);
            self.pending_blank = false;
        }
    }

    fn list_prefix(&self) -> (String, usize) {
        if let Some(frame) = self.list_stack.last() {
            let indent = "  ".repeat(frame.indent);
            let marker = match frame.ordered_index {
                Some(i) => format!("{i}. "),
                None => "\u{2022} ".to_owned(),
            };
            (
                format!("{indent}{marker}"),
                indent.chars().count() + marker.chars().count(),
            )
        } else {
            (String::new(), 0)
        }
    }

    fn handle(&mut self, event: Event<'_>, range: Range<usize>) {
        self.note(range);
        match event {
            Event::Start(tag) => self.handle_start(tag),
            Event::End(end) => self.handle_end(end),
            Event::Text(text) => self.handle_text(&text),
            Event::Code(text) => {
                self.push_text(text.into_string(), inline_code_style());
            }
            Event::SoftBreak | Event::HardBreak => self.handle_break(),
            Event::Rule => self.handle_rule(),
            Event::Html(text) | Event::InlineHtml(text) => {
                let s = self.current_style();
                self.push_text(text.into_string(), s);
            }
            _ => {}
        }
    }

    fn handle_start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.maybe_emit_pending_blank(),
            Tag::Heading { level, .. } => {
                self.maybe_emit_pending_blank();
                self.style_stack.push(heading_style(level, self.fallback));
                self.push_text(heading_prefix(level), dim_style());
            }
            Tag::Strong => {
                let s = self.current_style().add_modifier(Modifier::BOLD);
                self.style_stack.push(s);
            }
            Tag::Emphasis => {
                let s = self.current_style().add_modifier(Modifier::ITALIC);
                self.style_stack.push(s);
            }
            Tag::Strikethrough => {
                let s = self.current_style().add_modifier(Modifier::CROSSED_OUT);
                self.style_stack.push(s);
            }
            Tag::BlockQuote(_) => {
                self.maybe_emit_pending_blank();
                let s = self.fallback.add_modifier(Modifier::DIM | Modifier::ITALIC);
                self.style_stack.push(s);
            }
            Tag::List(start) => {
                self.maybe_emit_pending_blank();
                let indent = self.list_stack.len();
                self.list_stack.push(ListFrame {
                    ordered_index: start,
                    indent,
                });
            }
            Tag::Item => {
                self.flush_line();
                let (prefix, _) = self.list_prefix();
                self.push_text(prefix, self.fallback);
                if let Some(frame) = self.list_stack.last_mut()
                    && let Some(idx) = frame.ordered_index.as_mut()
                {
                    *idx += 1;
                }
            }
            Tag::CodeBlock(kind) => {
                self.maybe_emit_pending_blank();
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.into_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.in_code_block = Some(lang);
                self.code_body.clear();
            }
            Tag::Link { title, .. } => {
                let s = self
                    .current_style()
                    .add_modifier(Modifier::UNDERLINED)
                    .fg(Color::Cyan);
                self.style_stack.push(s);
                if !title.is_empty() {
                    self.push_text(format!("{title} ("), self.current_style());
                }
            }
            _ => {}
        }
    }

    fn handle_end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph => {
                self.flush_line();
                self.has_block_content = true;
                self.emit_paragraph_break();
            }
            TagEnd::Heading(_) | TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.style_stack.pop();
                self.has_block_content = true;
                self.emit_paragraph_break();
            }
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link => {
                self.style_stack.pop();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.has_block_content = true;
                    self.emit_paragraph_break();
                }
            }
            TagEnd::Item => {
                self.flush_line();
            }
            TagEnd::CodeBlock => {
                if let Some(lang) = self.in_code_block.take() {
                    let body = std::mem::take(&mut self.code_body);
                    let fence_text = if lang.is_empty() {
                        "```".to_owned()
                    } else {
                        format!("```{lang}")
                    };
                    // Every row of the fence shares the block's
                    // source span, so selecting any code row yanks
                    // the whole verbatim fenced block.
                    let block_src = self.acc.take().unwrap_or(NO_SRC);
                    self.emit_line_src(
                        Line::from(Span::styled(fence_text, dim_style())),
                        block_src.clone(),
                    );
                    let body_lines = if self.highlight_code {
                        highlight_with_lang(&body, &lang, self.fallback)
                    } else {
                        plain_lines_styled(&body, dim_style())
                    };
                    for line in body_lines {
                        self.emit_line_src(line, block_src.clone());
                    }
                    self.emit_line_src(
                        Line::from(Span::styled("```".to_owned(), dim_style())),
                        block_src,
                    );
                }
                self.has_block_content = true;
                self.emit_paragraph_break();
            }
            _ => {}
        }
    }

    fn handle_text(&mut self, text: &str) {
        if self.in_code_block.is_some() {
            self.code_body.push_str(text);
        } else {
            let s = self.current_style();
            self.push_text(text.to_owned(), s);
        }
    }

    fn handle_break(&mut self) {
        if self.in_code_block.is_some() {
            self.code_body.push('\n');
        } else {
            self.flush_line();
        }
    }

    fn handle_rule(&mut self) {
        self.maybe_emit_pending_blank();
        self.emit_line(Line::from(Span::styled("\u{2500}".repeat(40), dim_style())));
        self.has_block_content = true;
        self.emit_paragraph_break();
    }

    fn finish(mut self) -> (Vec<Line<'static>>, Vec<Range<usize>>) {
        self.flush_line();
        if self.lines.is_empty() {
            let lines = plain_lines_styled("", self.fallback);
            let src = vec![NO_SRC; lines.len()];
            return (lines, src);
        }
        debug_assert_eq!(self.lines.len(), self.src.len());
        (self.lines, self.src)
    }
}

fn heading_style(level: HeadingLevel, fallback: Style) -> Style {
    let base = fallback.add_modifier(Modifier::BOLD);
    match level {
        HeadingLevel::H1 => base.fg(Color::Magenta),
        HeadingLevel::H2 => base.fg(Color::Cyan),
        _ => base,
    }
}

fn heading_prefix(level: HeadingLevel) -> String {
    let n = match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    };
    format!("{} ", "#".repeat(n))
}

fn dim_style() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM)
}

fn inline_code_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn plain_paragraph_renders_as_text() {
        let lines = render("hello world", Style::default());
        assert_eq!(spans_text(&lines[0]), "hello world");
    }

    #[test]
    fn heading_keeps_prefix_and_bolds_the_text() {
        let lines = render("## Title", Style::default());
        let text = spans_text(&lines[0]);
        assert!(text.contains("## "), "got {text:?}");
        assert!(text.contains("Title"));
        let has_bold = lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold);
    }

    #[test]
    fn bold_inline_becomes_bold_span() {
        let lines = render("some **bold** text", Style::default());
        let has_bold = lines[0]
            .spans
            .iter()
            .any(|s| s.content == "bold" && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold, "bold marker should mark the inner span bold");
    }

    #[test]
    fn italic_inline_becomes_italic_span() {
        let lines = render("some *fancy* text", Style::default());
        let has_italic = lines[0]
            .spans
            .iter()
            .any(|s| s.content == "fancy" && s.style.add_modifier.contains(Modifier::ITALIC));
        assert!(has_italic);
    }

    #[test]
    fn inline_code_uses_code_style() {
        let lines = render("call `foo()` here", Style::default());
        let has_code = lines[0]
            .spans
            .iter()
            .any(|s| s.content == "foo()" && s.style.fg == Some(Color::Yellow));
        assert!(has_code);
    }

    #[test]
    fn bullet_list_renders_bullet_glyph() {
        let lines = render("- one\n- two", Style::default());
        assert!(spans_text(&lines[0]).starts_with('\u{2022}'));
        assert!(spans_text(&lines[1]).starts_with('\u{2022}'));
        assert!(spans_text(&lines[0]).contains("one"));
        assert!(spans_text(&lines[1]).contains("two"));
    }

    #[test]
    fn ordered_list_numbers_items() {
        let lines = render("1. first\n2. second", Style::default());
        assert!(spans_text(&lines[0]).starts_with("1. "));
        assert!(spans_text(&lines[1]).starts_with("2. "));
    }

    #[test]
    fn block_quote_dims_text() {
        let lines = render("> quoted", Style::default());
        let dim = lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::DIM));
        assert!(dim);
    }

    #[test]
    fn fenced_code_block_includes_fence_markers_and_body() {
        let lines = render("```rust\nfn main() {}\n```", Style::default());
        assert!(spans_text(&lines[0]).contains("```rust"));
        let has_body = lines.iter().any(|l| spans_text(l).contains("fn main()"));
        assert!(has_body);
        assert!(spans_text(lines.last().unwrap()).contains("```"));
    }

    #[test]
    fn paragraphs_get_blank_line_between_them() {
        let lines = render("para one\n\npara two", Style::default());
        let texts: Vec<String> = lines.iter().map(spans_text).collect();
        let blank_idx = texts.iter().position(String::is_empty);
        assert!(blank_idx.is_some(), "expected a blank separator line");
    }

    #[test]
    fn render_streaming_keeps_structure_but_leaves_code_plain() {
        let md = "# Title\n\n```rust\nfn main() {}\n```";
        let live = render_streaming(md, Style::default());

        let head = spans_text(&live[0]);
        assert!(
            head.contains("# ") && head.contains("Title"),
            "got {head:?}"
        );
        assert!(
            live[0]
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "heading is still styled while streaming"
        );

        let is_plain_dim = |l: &Line<'_>| {
            !l.spans.is_empty()
                && l.spans.iter().all(|s| {
                    s.style.add_modifier.contains(Modifier::DIM)
                        && s.style.fg == Some(Color::DarkGray)
                })
        };
        let body = live
            .iter()
            .find(|l| spans_text(l).contains("fn main()"))
            .expect("code body present");
        assert!(
            is_plain_dim(body),
            "streaming code stays plain dim, not syntect: {:?}",
            body.spans
        );

        let settled = render(md, Style::default());
        let sbody = settled
            .iter()
            .find(|l| spans_text(l).contains("fn main()"))
            .expect("code body present");
        assert!(
            !is_plain_dim(sbody),
            "settled code is syntect-highlighted, not the streaming dim"
        );
    }

    #[test]
    fn empty_input_produces_empty_line() {
        let lines = render("", Style::default());
        assert_eq!(lines.len(), 1);
        assert_eq!(spans_text(&lines[0]), "");
    }
}
