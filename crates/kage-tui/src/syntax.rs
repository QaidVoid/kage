//! Syntect-backed syntax highlighting for fenced code blocks.
//!
//! Two entry points: [`highlight_fenced`] walks a piece of assistant
//! text looking for ```` ```lang ... ``` ```` fences and yields
//! styled lines mixing plain text with syntect-highlighted code, and
//! [`highlight_extension`] renders an entire blob (typically a `read`
//! tool result) using a syntax inferred from the file extension.
//!
//! Both share a single global [`SyntaxSet`] / [`ThemeSet`] loaded once
//! via [`std::sync::OnceLock`] - syntect's default loaders take ~10ms
//! and bring in ~150 syntaxes, so we deliberately avoid re-init per
//! call.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    let ts = THEME_SET.get_or_init(ThemeSet::load_defaults);
    ts.themes
        .get("base16-ocean.dark")
        .unwrap_or_else(|| ts.themes.values().next().expect("syntect ships themes"))
}

/// Render `code` using the syntax for the given file extension (no
/// leading dot). Falls back to plain text styled with `fallback` when
/// the extension is unknown. Each input line becomes one [`Line`].
#[must_use]
pub fn highlight_extension(code: &str, extension: &str, fallback: Style) -> Vec<Line<'static>> {
    let ss = syntax_set();
    let Some(syntax) = ss.find_syntax_by_extension(extension) else {
        return plain_lines(code, fallback);
    };
    highlight_with_syntax(code, syntax, fallback)
}

/// Walk `text` looking for fenced code blocks (```` ```lang ... ``` ````).
/// Inside each fence the body is highlighted; outside, the text is
/// rendered with `fallback` style. Lines are split on `\n`; the fence
/// markers themselves render as dim borders.
#[must_use]
pub fn highlight_fenced(text: &str, fallback: Style) -> Vec<Line<'static>> {
    let ss = syntax_set();
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut iter = text.split('\n').peekable();
    while let Some(line) = iter.next() {
        if let Some(rest) = line.strip_prefix("```") {
            let lang = rest.trim();
            // Capture fence content until closing ``` (or end of input).
            let mut body = String::new();
            let mut closed = false;
            for inner in iter.by_ref() {
                if inner.trim_start().starts_with("```") {
                    closed = true;
                    break;
                }
                body.push_str(inner);
                body.push('\n');
            }
            // Emit opening fence as a dim marker line.
            out.push(Line::from(Span::styled(
                if lang.is_empty() {
                    "```".to_owned()
                } else {
                    format!("```{lang}")
                },
                dim,
            )));
            let syntax_ref = ss
                .find_syntax_by_token(lang)
                .or_else(|| ss.find_syntax_by_name(lang));
            let body_lines = match syntax_ref {
                Some(syntax) => highlight_with_syntax(&body, syntax, fallback),
                None => plain_lines(&body, fallback),
            };
            for body_line in body_lines {
                out.push(body_line);
            }
            if closed {
                out.push(Line::from(Span::styled("```".to_owned(), dim)));
            }
            continue;
        }
        out.push(Line::from(Span::styled(line.to_owned(), fallback)));
    }
    out
}

fn highlight_with_syntax(
    code: &str,
    syntax: &SyntaxReference,
    fallback: Style,
) -> Vec<Line<'static>> {
    let theme = theme();
    let mut h = HighlightLines::new(syntax, theme);
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in LinesWithEndings::from(code) {
        let Ok(regions) = h.highlight_line(line, syntax_set()) else {
            return plain_lines(code, fallback);
        };
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(regions.len());
        for (style, piece) in regions {
            let trimmed = piece.trim_end_matches('\n');
            spans.push(Span::styled(trimmed.to_owned(), to_ratatui_style(style)));
        }
        out.push(Line::from(spans));
    }
    out
}

fn plain_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    text.split('\n')
        .map(|line| Line::from(Span::styled(line.to_owned(), style)))
        .collect()
}

fn to_ratatui_style(s: SynStyle) -> Style {
    let mut out = Style::default().fg(Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b));
    if s.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_text_yields_marker_then_highlighted_body() {
        let text = "before\n```rust\nfn main() {}\n```\nafter";
        let lines = highlight_fenced(text, Style::default());
        // before, ```rust, fn main() {} (highlighted), ```, after
        assert!(lines.len() >= 5);
        let strs: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(strs.iter().any(|s| s == "before"));
        assert!(strs.iter().any(|s| s == "```rust"));
        assert!(strs.iter().any(|s| s.contains("fn main()")));
    }

    #[test]
    fn highlight_extension_falls_back_for_unknown_ext() {
        let lines = highlight_extension("hello", "xyzz", Style::default());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn highlight_extension_uses_known_syntax() {
        let lines = highlight_extension("fn main() {}", "rs", Style::default());
        assert_eq!(lines.len(), 1);
        // Highlighted spans should split into multiple pieces (kw, name, etc).
        assert!(lines[0].spans.len() > 1, "expected multiple spans");
    }
}
