//! Widget for [`Block::Custom`] - plugin / host-injected blocks
//! whose `kind` the core does not interpret.
//!
//! Internal `kage:*` kinds get purpose-built chrome instead of a
//! raw `[kage:...]` label: informational kinds (help, notify, theme,
//! image) render as quiet muted text with no header at all, errors
//! get a red `error` tag, and operational kinds (shell, mcp, log,
//! compaction) get a small muted tag. Unknown kinds - plugin blocks
//! without a registered renderer - keep the `[kind]` header, which
//! is the useful debugging view for plugin authors.

use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use super::widget::{BlockWidget, RenderCtx};
use super::{
    Emphasis, custom_style, fold_indicator, header_line, mark_emphasis, plain_lines, prefix_line,
};
use crate::buffer::Block;
use crate::theme::current;

/// The chrome a custom block gets, derived from its kind.
enum Chrome {
    /// No header, muted body: pure informational text.
    Quiet,
    /// A small muted tag above the body (`shell`, `error`, ...).
    /// The bool marks the tag as an alarm (error styling).
    Tag(&'static str, bool),
    /// The plugin-debug view: `[kind]` header, custom accent body.
    Raw,
}

fn chrome_for(kind: &str) -> Chrome {
    match kind {
        "kage:help" | "kage:notify" | "kage:theme" | "kage:image" => Chrome::Quiet,
        "kage:error" => Chrome::Tag("error", true),
        "kage:shell" => Chrome::Tag("shell", false),
        "kage:mcp" => Chrome::Tag("mcp", false),
        "kage:compaction" => Chrome::Tag("compaction", false),
        "kage:log" => Chrome::Tag("log", false),
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

    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        match chrome_for(&self.kind) {
            Chrome::Quiet => {
                if self.folded {
                    // A folded quiet block still needs one visible
                    // row: the fold indicator plus its first line.
                    let first = self.text.lines().next().unwrap_or_default().to_owned();
                    out.push(prefix_line("  ", Line::from(first).style(muted_style())));
                } else {
                    for body_line in plain_lines(&self.text, muted_style()) {
                        out.push(prefix_line("  ", body_line));
                    }
                }
            }
            Chrome::Tag(tag, alarm) => {
                let tag_style = if alarm {
                    Style::default()
                        .fg(current().tool_error_fg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    muted_style().add_modifier(Modifier::BOLD)
                };
                out.push(header_line(
                    fold_indicator(self.folded),
                    tag,
                    None,
                    tag_style,
                ));
                if !self.folded {
                    let body_style = if alarm {
                        Style::default().fg(current().tool_error_fg)
                    } else {
                        Style::default().fg(current().assistant_fg)
                    };
                    for body_line in plain_lines(&self.text, body_style) {
                        out.push(prefix_line("  ", body_line));
                    }
                }
            }
            Chrome::Raw => {
                out.push(header_line(
                    fold_indicator(self.folded),
                    &self.kind,
                    None,
                    custom_style(),
                ));
                if !self.folded {
                    for body_line in plain_lines(&self.text, custom_style()) {
                        out.push(prefix_line("  ", body_line));
                    }
                }
            }
        }
        mark_emphasis(out, width, emphasis, None)
    }
}

fn muted_style() -> Style {
    Style::default().fg(current().muted_fg)
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
        for kind in ["kage:help", "kage:notify", "kage:theme", "kage:image"] {
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

    #[test]
    fn error_kind_gets_an_error_tag_not_the_raw_kind() {
        let block = Block::Custom {
            kind: "kage:error".into(),
            text: "model unavailable".into(),
            folded: false,
        };
        let w = CustomBlockWidget::from_block(&block).unwrap();
        let text = painted(&w.lines(60, &ctx(&Theme::default())));
        assert!(text.contains("error"), "{text:?}");
        assert!(!text.contains("kage:error"), "{text:?}");
        assert!(text.contains("model unavailable"), "{text:?}");
    }

    #[test]
    fn operational_kinds_get_friendly_tags() {
        for (kind, tag) in [
            ("kage:shell", "shell"),
            ("kage:mcp", "mcp"),
            ("kage:compaction", "compaction"),
        ] {
            let block = Block::Custom {
                kind: kind.into(),
                text: "body".into(),
                folded: false,
            };
            let w = CustomBlockWidget::from_block(&block).unwrap();
            let text = painted(&w.lines(60, &ctx(&Theme::default())));
            assert!(text.contains(tag), "{kind}: {text:?}");
            assert!(!text.contains(kind), "{kind}: {text:?}");
        }
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
}
