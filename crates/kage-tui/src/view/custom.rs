//! Widget for [`Block::Custom`] - plugin / host-injected blocks
//! whose `kind` the core does not interpret.
//!
//! Internal `kage:*` kinds get purpose-built chrome instead of a
//! raw `[kage:...]` label: informational kinds (help, notify, retry,
//! theme, image, plugin, mcp, log) render as quiet muted lines, errors
//! as a U+2717 line in the error color, shell output under its
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
}
