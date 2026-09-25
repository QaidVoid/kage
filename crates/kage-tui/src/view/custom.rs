//! Widget for [`Block::Custom`] - plugin / host-injected blocks
//! whose `kind` the core does not interpret.
//!
//! Internal `kage:*` kinds get purpose-built chrome instead of a
//! raw `[kage:...]` label: informational kinds (help, notify, retry,
//! theme, image, plugin, mcp, log) render as quiet muted lines, usage
//! as a styled panel, errors as a U+2717 line in the error color, shell output under its
//! `$ command` header, and a truncated reply as a warning line.
//! Unknown kinds - plugin blocks without a registered renderer - keep
//! the `[kind]` header, which is the useful debugging view for plugin
//! authors.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::widget::{BlockWidget, RenderCtx};
use super::{
    Emphasis, custom_style, fold_indicator, header_line, mark_emphasis, plain_lines,
    tool_call_style,
};
use crate::buffer::Block;
use crate::theme::current;

/// The chrome a custom block gets, derived from its kind.
enum Chrome {
    /// Muted lines: pure informational text.
    Quiet,
    /// The message after a U+2717 glyph, in the error color.
    Error,
    /// A `$ command` header in the tool style above the output.
    Shell,
    /// The `/usage` panel: section labels, bright values, colored bar.
    Usage,
    /// One line in the warning color.
    Warning,
    /// The plugin-debug view: `[kind]` header, custom accent body.
    Raw,
}

fn chrome_for(kind: &str) -> Chrome {
    match kind {
        "kage:help" | "kage:notify" | "kage:retry" | "kage:theme" | "kage:image"
        | "kage:plugin" | "kage:mcp" | "kage:log" => Chrome::Quiet,
        "kage:error" => Chrome::Error,
        "kage:usage" => Chrome::Usage,
        "kage:shell" => Chrome::Shell,
        "kage:truncated" => Chrome::Warning,
        _ => Chrome::Raw,
    }
}

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

    /// The lines of the body, or only the first one when folded, so a
    /// folded notice stays findable.
    fn body(&self) -> &str {
        if self.folded {
            self.text.lines().next().unwrap_or_default()
        } else {
            &self.text
        }
    }

    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let fg = |c| Style::default().fg(c);
        let out = match chrome_for(&self.kind) {
            Chrome::Quiet => plain_lines(self.body(), fg(current().muted_fg)),
            Chrome::Warning => plain_lines(self.body(), fg(current().warning_fg)),
            Chrome::Error => {
                let style = fg(current().tool_error_fg);
                let mut out = plain_lines(self.body(), style);
                if let Some(first) = out.first_mut() {
                    first.spans.insert(
                        0,
                        Span::styled("\u{2717} ", style.add_modifier(Modifier::BOLD)),
                    );
                }
                out
            }
            Chrome::Usage => usage_lines(&self.text, self.folded),
            Chrome::Shell => {
                let mut lines = self.text.lines();
                let header = lines.next().unwrap_or_default();
                let mut out = vec![Line::from(Span::styled(
                    header.to_owned(),
                    tool_call_style().add_modifier(Modifier::BOLD),
                ))];
                if !self.folded {
                    let body: Vec<&str> = lines.collect();
                    let body = match body.split_last() {
                        Some((&"(exit code 0)", rest)) => rest,
                        _ => &body[..],
                    };
                    out.extend(plain_lines(&body.join("\n"), fg(current().assistant_fg)));
                }
                out
            }
            Chrome::Raw => {
                let mut out = vec![header_line(
                    fold_indicator(self.folded),
                    &self.kind,
                    None,
                    custom_style(),
                )];
                if !self.folded {
                    out.extend(plain_lines(&self.text, custom_style()));
                }
                out
            }
        };
        mark_emphasis(out, width, emphasis)
    }
}

/// Styled `/usage` panel. Parses the stable body the usage command
/// pushes (`Session usage` / model / counters lines, `Context window`
/// / bar line) into labeled spans: the first section doubles as the
/// header (fold marker included), later section labels muted, model
/// and values bright, the bar filled in the success color (warning
/// past 90%). Unknown lines pass through muted so a newer body still
/// reads. Folded keeps the header and the model row. Returns raw
/// lines: the caller applies the focus rule once.
fn usage_lines(body: &str, folded: bool) -> Vec<Line<'static>> {
    let muted = || Style::default().fg(current().muted_fg);
    let bright = || Style::default().fg(current().assistant_fg);
    let mut out = vec![Line::from(vec![
        Span::styled(
            format!("{} ", fold_indicator(folded)),
            custom_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled("Session usage", custom_style().add_modifier(Modifier::BOLD)),
    ])];
    for line in body.lines() {
        if line == "Session usage" {
            continue;
        }
        if folded && line == "Context window" {
            break;
        }
        if line == "Context window" {
            out.push(Line::from(Span::styled(line.to_owned(), muted())));
        } else if let Some(rest) = line.strip_prefix("    ") {
            out.push(usage_counters(rest));
        } else if let Some(rest) = line.strip_prefix("  [") {
            out.push(usage_bar_line(rest));
        } else if let Some(rest) = line.strip_prefix("  ") {
            if rest == "(unknown window)" {
                out.push(Line::from(Span::styled(format!("  {rest}"), muted())));
            } else {
                out.push(Line::from(Span::styled(
                    format!("  {rest}"),
                    bright().add_modifier(Modifier::BOLD),
                )));
            }
        } else if !line.is_empty() {
            out.push(Line::from(Span::styled(line.to_owned(), muted())));
        }
        if folded {
            break;
        }
    }
    out
}

/// `input 1.2M  output 3k ...` with labels muted and values bright; a
/// trailing parenthesized note (the cost) renders dim.
fn usage_counters(rest: &str) -> Line<'static> {
    let muted = || Style::default().fg(current().muted_fg);
    let bright = || Style::default().fg(current().assistant_fg);
    let mut spans = vec![Span::raw("    ".to_owned())];
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    let mut note: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].starts_with('(') {
            note.extend_from_slice(&tokens[i..]);
            break;
        }
        pairs.push((tokens[i], tokens.get(i + 1).copied().unwrap_or("")));
        i += 2;
    }
    for (n, (label, value)) in pairs.iter().enumerate() {
        if n > 0 {
            spans.push(Span::raw("  ".to_owned()));
        }
        spans.push(Span::styled(format!("{label} "), muted()));
        spans.push(Span::styled((*value).to_owned(), bright()));
    }
    if !note.is_empty() {
        spans.push(Span::styled(
            format!("  {}", note.join(" ")),
            muted().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// `[<bar>]  <pct>%  (<used> / <window>)` with the fill counted from
/// the block glyphs and colored by the parsed percent.
fn usage_bar_line(rest: &str) -> Line<'static> {
    let bright = || Style::default().fg(current().assistant_fg);
    let (bar, tail) = rest.split_once(']').unwrap_or((rest, ""));
    let filled = bar.chars().filter(|&c| c == '\u{2588}').count();
    let empty = bar.chars().filter(|&c| c == '\u{2591}').count();
    let pct: u64 = tail
        .split('%')
        .next()
        .unwrap_or("")
        .trim()
        .parse()
        .unwrap_or(0);
    let fill_style = Style::default().fg(if pct >= 90 {
        current().warning_fg
    } else {
        current().success_fg
    });
    Line::from(vec![
        Span::raw("  [".to_owned()),
        Span::styled(
            "\u{2588}".repeat(filled),
            fill_style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "\u{2591}".repeat(empty),
            Style::default()
                .fg(current().muted_fg)
                .add_modifier(Modifier::DIM),
        ),
        Span::styled(format!("]{tail}"), bright()),
    ])
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
    use ratatui::style::Color;

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
        // An unknown (plugin) kind: raw blocks keep the `[kind]`
        // header as the plugin-debug view.
        Block::Custom {
            kind: "myplugin:cards".into(),
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
        assert!(text.contains("myplugin:cards"), "got {text:?}");
        assert!(text.contains("log payload"), "got {text:?}");
    }

    #[test]
    fn informational_kinds_render_quiet_without_a_kind_label() {
        for kind in [
            "kage:help",
            "kage:notify",
            "kage:theme",
            "kage:image",
            "kage:plugin",
        ] {
            let block = Block::Custom {
                kind: kind.into(),
                text: "welcome to kage".into(),
                folded: false,
            };
            let w = CustomBlockWidget::from_block(&block).unwrap();
            let text = painted(&w.lines(60, &ctx(&Theme::default())));
            assert!(!text.contains(kind), "{kind} must not leak: {text:?}");
            assert!(text.contains("welcome to kage"), "{text:?}");
        }
    }

    fn rows(kind: &str, text: &str) -> Vec<String> {
        let block = Block::Custom {
            kind: kind.into(),
            text: text.into(),
            folded: false,
        };
        let w = CustomBlockWidget::from_block(&block).unwrap();
        w.lines(60, &ctx(&Theme::default()))
            .iter()
            .map(|l| {
                let row: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                row.strip_prefix("  ").unwrap_or(&row).to_owned()
            })
            .collect()
    }

    #[test]
    fn notices_carry_no_bracket_tags() {
        for kind in [
            "kage:error",
            "kage:shell",
            "kage:mcp",
            "kage:log",
            "kage:truncated",
            "kage:retry",
        ] {
            let text = rows(kind, "$ ls\nbody").join("\n");
            assert!(!text.contains('['), "{kind}: {text:?}");
            assert!(!text.contains(kind), "{kind}: {text:?}");
        }
    }

    #[test]
    fn errors_lead_with_a_cross() {
        assert_eq!(
            rows("kage:error", "model unavailable"),
            ["\u{2717} model unavailable"]
        );
    }

    #[test]
    fn shell_blocks_drop_a_zero_exit_code() {
        assert_eq!(
            rows("kage:shell", "$ ls\na.rs\n(exit code 0)"),
            ["$ ls", "a.rs"]
        );
        assert_eq!(
            rows("kage:shell", "$ false\n\n(exit code 1)"),
            ["$ false", "", "(exit code 1)"]
        );
    }

    #[test]
    fn folded_quiet_block_stays_visible() {
        let block = Block::Custom {
            kind: "kage:help".into(),
            text: "first line\nsecond line".into(),
            folded: true,
        };
        let w = CustomBlockWidget::from_block(&block).unwrap();
        let text = painted(&w.lines(60, &ctx(&Theme::default())));
        assert!(text.contains("first line"), "{text:?}");
        assert!(!text.contains("second line"), "{text:?}");
    }

    #[test]
    fn quiet_blocks_indent_like_assistant_text() {
        let block = Block::Custom {
            kind: "kage:help".into(),
            text: "welcome to kage".into(),
            folded: false,
        };
        let w = CustomBlockWidget::from_block(&block).unwrap();
        let lines = w.lines(60, &ctx(&Theme::default()));
        let text = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "  welcome to kage", "indented past the rule column");
    }

    fn usage_fixture() -> String {
        let bar = "\u{2588}".repeat(15) + &"\u{2591}".repeat(5);
        [
            "Session usage",
            "  p:m",
            "    input 650.2M  output 1.8M  cache read 12.4M  cache write 3.1M  total 652M  ($1.23)",
            "Context window",
            &format!("  [{bar}]  76%  (190k / 250k)"),
        ]
        .join("\n")
    }

    fn usage_block(text: String) -> Block {
        Block::Custom {
            kind: "kage:usage".into(),
            text,
            folded: false,
        }
    }

    #[test]
    fn usage_panel_drops_the_kind_tag_and_styles_sections() {
        let w = CustomBlockWidget::from_block(&usage_block(usage_fixture())).unwrap();
        let lines = w.lines(60, &ctx(&Theme::default()));
        let text = painted(&lines);
        assert!(!text.contains("kage:usage"), "no raw tag: {text:?}");
        let header = text.lines().next().unwrap_or_default();
        assert_eq!(header, "  v Session usage", "one rule column: {text:?}");
        assert!(text.contains("Session usage"), "{text:?}");
        assert!(text.contains("p:m"), "{text:?}");
        assert!(text.contains("650.2M"), "{text:?}");
        assert!(text.contains("76%"), "{text:?}");
        let theme = Theme::default();
        let styled: Vec<(String, Option<Color>)> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect();
        assert!(
            styled
                .iter()
                .any(|(t, c)| t.contains("650.2M") && *c == Some(theme.assistant_fg)),
            "values read bright: {styled:?}"
        );
        assert!(
            styled
                .iter()
                .any(|(t, c)| t.contains("input ") && *c == Some(theme.muted_fg)),
            "labels read muted: {styled:?}"
        );
        assert!(
            styled
                .iter()
                .any(|(t, c)| t.contains("\u{2588}") && *c == Some(theme.success_fg)),
            "bar fill uses the success color: {styled:?}"
        );
    }

    #[test]
    fn usage_panel_marks_a_hot_bar_warning() {
        let bar = "\u{2588}".repeat(19) + "\u{2591}";
        let text = [
            "Session usage",
            "  p:m",
            "    input 1k  output 1k  cache read 0  cache write 0  total 2k",
            "Context window",
            &format!("  [{bar}]  95%  (950k / 1M)"),
        ]
        .join("\n");
        let w = CustomBlockWidget::from_block(&usage_block(text)).unwrap();
        let theme = Theme::default();
        let styled: Vec<(String, Option<Color>)> = w
            .lines(60, &ctx(&theme))
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect();
        assert!(
            styled
                .iter()
                .any(|(t, c)| t.contains("\u{2588}") && *c == Some(theme.warning_fg)),
            "hot fill warns: {styled:?}"
        );
    }

    #[test]
    fn usage_panel_without_a_window_reads_muted() {
        let text = [
            "Usage",
            "Session usage",
            "  p:m",
            "    input 0  output 0  cache read 0  cache write 0  total 0",
            "Context window",
            "  (unknown window)",
        ]
        .join("\n");
        let w = CustomBlockWidget::from_block(&usage_block(text)).unwrap();
        let rendered = painted(&w.lines(60, &ctx(&Theme::default())));
        assert!(rendered.contains("(unknown window)"), "{rendered:?}");
    }

    #[test]
    fn folded_usage_keeps_header_and_model_only() {
        let w = CustomBlockWidget::from_block(&Block::Custom {
            kind: "kage:usage".into(),
            text: usage_fixture(),
            folded: true,
        })
        .unwrap();
        let rendered = painted(&w.lines(60, &ctx(&Theme::default())));
        assert!(rendered.contains("Session usage"), "{rendered:?}");
        assert!(rendered.contains("p:m"), "{rendered:?}");
        assert!(!rendered.contains("650.2M"), "{rendered:?}");
        assert!(!rendered.contains("76%"), "{rendered:?}");
    }
}
