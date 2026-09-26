//! Markdown-to-ratatui renderer for assistant text.
//!
//! Walks a `pulldown_cmark::Parser` event stream and emits styled
//! [`ratatui::text::Line`]s the assistant block widget paints. Fenced
//! code blocks render as a dim language label over the indented code,
//! which is passed through to [`crate::syntax::highlight_with_lang`]
//! so syntect runs on languages we have grammars for; everything else
//! is plain styled text.
//!
//! The renderer is line-oriented: it buffers spans of the current line
//! and flushes a [`Line`] whenever the parser emits a paragraph break,
//! a heading, a list item, or an explicit `SoftBreak` / `HardBreak`.
//! Adjacent inline styles (bold, italic, inline code) stack via a
//! small style state. Block quotes keep a dim `>` gutter on every
//! line, and tables render as plain columns padded to line up, with a
//! bold header over a dim rule.
//!
//! Not supported by design (yet): images, footnotes, HTML
//! passthrough, task lists, autolinks. They render as the raw text the
//! parser yields so users still see content rather than a silent drop.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::syntax::{highlight_with_lang, plain_lines_styled};
use crate::view::UnicodeWidthStr as _;

/// Convert `text` into a vector of styled [`Line`]s by walking the
/// `CommonMark` event stream. `fallback` is the base text style for
/// paragraphs and list items; headings, code, and quotes layer their
/// own modifiers on top.
#[must_use]
pub fn render(text: &str, fallback: Style) -> Vec<Line<'static>> {
    render_with(text, fallback, true)
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

/// Indent of fenced code under its language label.
const CODE_INDENT: &str = "  ";

/// Gutter painted before every line of a block quote, once per level.
pub(crate) const QUOTE_GUTTER: &str = "> ";

/// Cells between two table columns.
const TABLE_GAP: &str = "  ";

fn render_with(text: &str, fallback: Style, highlight_code: bool) -> Vec<Line<'static>> {
    let mut state = RenderState::new(fallback);
    state.highlight_code = highlight_code;
    for event in Parser::new_ext(text, Options::ENABLE_TABLES) {
        state.handle(event);
    }
    state.finish()
}

struct RenderState {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    fallback: Style,
    style_stack: Vec<Style>,
    list_stack: Vec<ListFrame>,
    in_code_block: Option<String>,
    code_body: String,
    link_dest: Option<String>,
    link_text: String,
    pending_blank: bool,
    has_block_content: bool,
    highlight_code: bool,
    quote_depth: usize,
    table: Option<Table>,
}

/// A table being collected: every cell's spans, rendered once the
/// table ends and every column width is known.
#[derive(Default)]
struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
    cell: Vec<Span<'static>>,
    header_rows: usize,
}

struct ListFrame {
    ordered_index: Option<u64>,
    indent: usize,
}

impl RenderState {
    fn new(fallback: Style) -> Self {
        Self {
            lines: Vec::new(),
            current: Vec::new(),
            fallback,
            style_stack: vec![fallback],
            list_stack: Vec::new(),
            in_code_block: None,
            code_body: String::new(),
            link_dest: None,
            link_text: String::new(),
            pending_blank: false,
            has_block_content: false,
            highlight_code: true,
            quote_depth: 0,
            table: None,
        }
    }

    fn emit_line(&mut self, mut line: Line<'static>) {
        if self.quote_depth > 0 {
            line.spans.insert(
                0,
                Span::styled(QUOTE_GUTTER.repeat(self.quote_depth), dim_style()),
            );
        }
        self.lines.push(line);
    }

    fn current_style(&self) -> Style {
        *self.style_stack.last().unwrap_or(&self.fallback)
    }

    fn push_text(&mut self, text: String, style: Style) {
        if text.is_empty() {
            return;
        }
        match &mut self.table {
            Some(table) => table.cell.push(Span::styled(text, style)),
            None => self.current.push(Span::styled(text, style)),
        }
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
            self.emit_line(Line::from(""));
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

    fn handle(&mut self, event: Event<'_>) {
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
            Tag::Strong | Tag::TableHead => {
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
                self.flush_line();
                self.maybe_emit_pending_blank();
                self.quote_depth += 1;
                let s = self.fallback.add_modifier(Modifier::DIM | Modifier::ITALIC);
                self.style_stack.push(s);
            }
            Tag::Table(alignments) => {
                self.maybe_emit_pending_blank();
                self.table = Some(Table {
                    alignments,
                    ..Table::default()
                });
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
            Tag::Link { dest_url, .. } => {
                let s = self
                    .current_style()
                    .add_modifier(Modifier::UNDERLINED)
                    .fg(crate::theme::current().md_link_fg);
                self.style_stack.push(s);
                self.link_dest = Some(dest_url.into_string());
                self.link_text.clear();
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
            TagEnd::Heading(_) => {
                self.flush_line();
                self.style_stack.pop();
                self.has_block_content = true;
                self.emit_paragraph_break();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.style_stack.pop();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.has_block_content = true;
                self.emit_paragraph_break();
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    let cell = std::mem::take(&mut table.cell);
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if end == TagEnd::TableHead {
                    self.style_stack.pop();
                }
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                    if end == TagEnd::TableHead {
                        table.header_rows = table.rows.len();
                    }
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    for line in table_lines(table) {
                        self.emit_line(line);
                    }
                }
                self.has_block_content = true;
                self.emit_paragraph_break();
            }
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                self.style_stack.pop();
            }
            TagEnd::Link => {
                if let Some(dest) = self.link_dest.take()
                    && self.link_text.trim() != dest
                {
                    let shown = dest.strip_prefix("mailto:").unwrap_or(&dest);
                    self.push_text(format!(" ({shown})"), dim_style());
                }
                self.link_text.clear();
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
                    if !lang.is_empty() {
                        self.emit_line(Line::from(Span::styled(lang.clone(), dim_style())));
                    }
                    let body_lines = if self.highlight_code {
                        highlight_with_lang(&body, &lang, self.fallback)
                    } else {
                        plain_lines_styled(&body, dim_style())
                    };
                    for mut line in body_lines {
                        line.spans.insert(0, Span::raw(CODE_INDENT));
                        self.emit_line(line);
                    }
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
            if self.link_dest.is_some() {
                self.link_text.push_str(text);
            }
        }
    }

    fn handle_break(&mut self) {
        if self.in_code_block.is_some() {
            self.code_body.push('\n');
        } else if self.table.is_some() {
            self.push_text(" ".to_owned(), self.current_style());
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

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        if self.lines.is_empty() {
            return plain_lines_styled("", self.fallback);
        }
        self.lines
    }
}

/// Lay `table` out as plain lines: every column padded to its widest
/// cell and aligned as the delimiter row asks, the header rows over a
/// dim rule.
fn table_lines(table: Table) -> Vec<Line<'static>> {
    let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0; columns];
    for row in &table.rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(spans_width(cell));
        }
    }
    let mut lines = Vec::with_capacity(table.rows.len() + 1);
    for (i, row) in table.rows.into_iter().enumerate() {
        if i == table.header_rows && i > 0 {
            let rule: Vec<String> = widths.iter().map(|w| "\u{2500}".repeat(*w)).collect();
            lines.push(Line::from(Span::styled(rule.join(TABLE_GAP), dim_style())));
        }
        let mut spans = Vec::new();
        let mut cells = row.into_iter();
        for (col, width) in widths.iter().enumerate() {
            let cell = cells.next().unwrap_or_default();
            let room = width.saturating_sub(spans_width(&cell));
            let (left, right) = match table.alignments.get(col) {
                Some(Alignment::Right) => (room, 0),
                Some(Alignment::Center) => (room / 2, room - room / 2),
                _ => (0, room),
            };
            if col > 0 {
                spans.push(Span::raw(TABLE_GAP));
            }
            if left > 0 {
                spans.push(Span::raw(" ".repeat(left)));
            }
            spans.extend(cell);
            if right > 0 && col + 1 < columns {
                spans.push(Span::raw(" ".repeat(right)));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

fn heading_style(level: HeadingLevel, fallback: Style) -> Style {
    let base = fallback.add_modifier(Modifier::BOLD);
    let t = crate::theme::current();
    match level {
        HeadingLevel::H1 => base.fg(t.md_h1_fg),
        HeadingLevel::H2 => base.fg(t.md_h2_fg),
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
        .fg(crate::theme::current().muted_fg)
        .add_modifier(Modifier::DIM)
}

fn inline_code_style() -> Style {
    Style::default()
        .fg(crate::theme::current().md_code_fg)
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
    fn link_renders_text_followed_by_url() {
        let lines = render("[kage](https://example.com/kage)", Style::default());
        let text = spans_text(&lines[0]);
        assert_eq!(text, "kage (https://example.com/kage)");
        let url_dim = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains("https://example.com/kage") && s.style == dim_style());
        assert!(url_dim, "url suffix should be dimmed, got {text:?}");
    }

    #[test]
    fn autolink_does_not_duplicate_the_url() {
        let lines = render("see <https://example.com/docs>", Style::default());
        let text = spans_text(&lines[0]);
        assert_eq!(text, "see https://example.com/docs");
    }

    #[test]
    fn inline_code_uses_code_style() {
        let lines = render("call `foo()` here", Style::default());
        let has_code = lines[0].spans.iter().any(|s| {
            s.content == "foo()" && s.style.fg == Some(crate::theme::current().md_code_fg)
        });
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
    fn block_quote_keeps_a_gutter_on_every_line() {
        let lines = render("> one\n>\n> two\n\nafter", Style::default());
        let texts: Vec<String> = lines.iter().map(spans_text).collect();
        assert_eq!(texts, ["> one", "> ", "> two", "", "after"]);
        let nested = render("> outer\n>> inner", Style::default());
        assert_eq!(spans_text(&nested[0]), "> outer");
        assert!(
            nested.iter().any(|l| spans_text(l) == "> > inner"),
            "{nested:?}"
        );
    }

    #[test]
    fn table_renders_as_aligned_columns_under_a_rule() {
        let md = "| name | size |\n|:-----|-----:|\n| a.rs | 12 |\n| lib.rs | 3 |";
        let texts: Vec<String> = render(md, Style::default())
            .iter()
            .map(spans_text)
            .collect();
        assert_eq!(
            texts,
            [
                "name    size",
                "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}  \u{2500}\u{2500}\u{2500}\u{2500}",
                "a.rs      12",
                "lib.rs     3",
            ]
        );
        assert!(texts.iter().all(|t| !t.contains('|')));
    }

    #[test]
    fn table_header_is_bold_and_cells_keep_inline_styles() {
        let md = "| key | what |\n|---|---|\n| `x` | **bold** |";
        let lines = render(md, Style::default());
        assert!(
            lines[0]
                .spans
                .iter()
                .filter(|s| !s.content.trim().is_empty())
                .all(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(lines[2].spans.iter().any(|s| s.content == "x"
            && s.style.fg == Some(crate::theme::current().md_code_fg)));
    }

    #[test]
    fn table_after_a_paragraph_is_separated_by_a_blank_line() {
        let md = "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nafter";
        let texts: Vec<String> = render(md, Style::default())
            .iter()
            .map(spans_text)
            .collect();
        assert_eq!(texts[0], "intro");
        assert_eq!(texts[1], "");
        assert_eq!(texts[2], "a  b");
        assert_eq!(texts[5], "");
        assert_eq!(texts[6], "after");
    }

    #[test]
    fn fenced_code_renders_a_language_label_and_no_backticks() {
        let lines = render("```rust\nfn main() {}\n```", Style::default());
        assert_eq!(spans_text(&lines[0]), "rust");
        let has_body = lines.iter().any(|l| spans_text(l) == "  fn main() {}");
        assert!(has_body, "{lines:?}");
        assert!(lines.iter().all(|l| !spans_text(l).contains("```")));
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
            let code: Vec<_> = l
                .spans
                .iter()
                .filter(|s| !s.content.trim().is_empty())
                .collect();
            !code.is_empty()
                && code.iter().all(|s| {
                    s.style.add_modifier.contains(Modifier::DIM)
                        && s.style.fg == Some(crate::theme::current().muted_fg)
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
