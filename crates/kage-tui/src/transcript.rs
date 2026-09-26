//! The plain-text transcript printed after the TUI exits.

use kage_core::sync::read;
use ratatui::text::Line;

use crate::buffer::{Block, Buffer, ToolTopology, gap_between};
use crate::view::{DECORATION_MARKER, Emphasis, build_block_lines, registry};

/// How much of the conversation [`render`] prints, per the
/// `transcript_on_exit` option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptScope {
    /// Every block.
    Full,
    /// From the last user prompt on.
    Last,
    /// Nothing.
    None,
}

impl TranscriptScope {
    /// Parse a `transcript_on_exit` value: `full`, `last` or `none`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Self::Full),
            "last" => Some(Self::Last),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// Render `buffer` as plain text `width` columns wide, as the
/// conversation pane paints it without focus: folds and tool rows
/// read as they did on screen, while bands, rules and padding are
/// dropped and trailing spaces trimmed. User prompts start with `> `
/// and blocks are spaced as on screen. `Last` with no user
/// prompt, and `None`, render nothing.
#[must_use]
pub fn render(buffer: &Buffer, width: u16, scope: TranscriptScope) -> String {
    let blocks = buffer.blocks();
    let start = match scope {
        TranscriptScope::Full => 0,
        TranscriptScope::Last => {
            match blocks.iter().rposition(|b| matches!(b, Block::User { .. })) {
                Some(idx) => idx,
                None => return String::new(),
            }
        }
        TranscriptScope::None => return String::new(),
    };
    let topology = ToolTopology::build(blocks);
    let registry = read(registry::global());
    let mut out: Vec<String> = Vec::new();
    let mut above: Option<&Block> = None;
    for (idx, block) in blocks.iter().enumerate().skip(start) {
        if topology.is_hidden(idx) {
            continue;
        }
        let lines = build_block_lines(
            buffer,
            idx,
            width,
            &topology,
            Emphasis::None,
            &registry,
            None,
        );
        let mut rows: Vec<String> = lines.iter().map(plain_text).collect();
        let first = rows.iter().position(|row| !row.is_empty());
        let last = rows.iter().rposition(|row| !row.is_empty());
        let (Some(first), Some(last)) = (first, last) else {
            continue;
        };
        rows.truncate(last + 1);
        rows.drain(..first);
        if matches!(block, Block::User { .. }) {
            for (i, row) in rows.iter_mut().enumerate() {
                if !row.is_empty() {
                    row.insert_str(0, if i == 0 { "> " } else { "  " });
                }
            }
        }
        if let Some(above) = above {
            out.extend(std::iter::repeat_n(
                String::new(),
                gap_between(above, block),
            ));
        }
        above = Some(block);
        out.extend(rows);
    }
    out.join("\n")
}

/// The text of `line` without its decoration spans or trailing
/// spaces.
fn plain_text(line: &Line<'_>) -> String {
    let mut text: String = line
        .spans
        .iter()
        .filter(|span| {
            !line
                .style
                .patch(span.style)
                .add_modifier
                .contains(DECORATION_MARKER)
        })
        .map(|span| span.content.as_ref())
        .collect();
    text.truncate(text.trim_end().len());
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Buffer {
        let mut buf = Buffer::new();
        buf.push_custom("kage:error", "config: bad key", false);
        buf.push_user("first question");
        buf.append_assistant_delta("First answer.");
        buf.finish_streaming();
        buf.push_tool_call("c1", "shell", serde_json::json!({ "command": "echo hi" }));
        buf.push_tool_result_with_duration("c1", "hi", false, Some(1200));
        buf.push_user("second question\nwith two lines");
        buf.append_assistant_delta("Second answer.");
        buf.finish_streaming();
        buf
    }

    #[test]
    fn full_prints_every_block_without_decoration() {
        let text = render(&fixture(), 60, TranscriptScope::Full);
        assert_eq!(
            text,
            "\u{2717} config: bad key\n\
             \n\
             > first question\n\
             \n\
             First answer.\n\
             \n\
             \u{2022} Ran echo hi                                        1.2s\n    hi\n\
             \n\
             > second question\n  with two lines\n\
             \n\
             Second answer."
        );
        assert!(!text.contains(['\u{258e}', '\u{258c}']), "{text:?}");
        assert!(text.lines().all(|l| l.trim_end() == l), "{text:?}");
    }

    #[test]
    fn last_starts_at_the_last_user_block() {
        let text = render(&fixture(), 60, TranscriptScope::Last);
        assert_eq!(
            text,
            "> second question\n  with two lines\n\nSecond answer."
        );
    }

    #[test]
    fn last_without_a_prompt_and_none_print_nothing() {
        let mut buf = Buffer::new();
        buf.push_custom("kage:error", "config: bad key", false);
        assert_eq!(render(&buf, 60, TranscriptScope::Last), "");
        assert_eq!(render(&fixture(), 60, TranscriptScope::None), "");
    }

    #[test]
    fn consecutive_tool_rows_sit_flush() {
        let mut buf = Buffer::new();
        buf.push_tool_call("c1", "shell", serde_json::json!({ "command": "true" }));
        buf.push_tool_result_with_duration("c1", "", false, Some(1000));
        buf.push_tool_call("c2", "shell", serde_json::json!({ "command": "false" }));
        buf.push_tool_result_with_duration("c2", "", false, Some(1000));
        buf.append_assistant_delta("Done.");
        buf.finish_streaming();
        let text = render(&buf, 40, TranscriptScope::Full);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text:?}");
        assert!(lines[0].contains("Ran true"), "{text:?}");
        assert!(lines[1].contains("Ran false"), "{text:?}");
        assert_eq!(&lines[2..], ["", "Done."], "{text:?}");
    }

    #[test]
    fn long_lines_wrap_at_the_width() {
        let mut buf = Buffer::new();
        buf.append_assistant_delta("one two three four five six seven eight");
        buf.finish_streaming();
        let text = render(&buf, 20, TranscriptScope::Full);
        assert!(text.lines().count() > 1, "{text:?}");
        assert!(text.lines().all(|l| l.len() <= 20), "{text:?}");
    }

    #[test]
    fn scope_parses_the_option_values() {
        assert_eq!(TranscriptScope::parse("full"), Some(TranscriptScope::Full));
        assert_eq!(TranscriptScope::parse("last"), Some(TranscriptScope::Last));
        assert_eq!(TranscriptScope::parse("none"), Some(TranscriptScope::None));
        assert_eq!(TranscriptScope::parse("all"), None);
    }
}
