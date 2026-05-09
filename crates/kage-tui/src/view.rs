//! Render the conversation buffer and input area into a ratatui [`Frame`].
//!
//! [`render`] is the single entry point. It walks the buffer's blocks,
//! turns each one into a styled [`Line`], lays them out in a scrollable
//! [`Paragraph`], and paints the status bar and input area on top.
//!
//! Block styling lives in [`block_to_lines`]: assistant text is plain,
//! thinking is dimmed, tool calls render as a header line plus an
//! optional indented body, and custom blocks are passed through with
//! their `kind` shown in the header.

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RtBlock, Borders, Paragraph, Wrap};

use crate::buffer::{Block, Buffer};
use crate::input::{InputState, Mode};
use crate::layout::Regions;

/// Paint the entire TUI for one frame.
pub fn render(frame: &mut Frame, regions: Regions, buffer: &Buffer, input: &InputState) {
    render_status(frame, regions, input);
    render_buffer(frame, regions, buffer);
    render_input(frame, regions, input);
}

fn render_status(frame: &mut Frame, regions: Regions, input: &InputState) {
    let mode = mode_label(input.mode());
    let line = Line::from(vec![
        Span::styled(format!(" {mode} "), mode_style(input.mode())),
        Span::raw(" kage"),
    ]);
    let paragraph = Paragraph::new(line)
        .alignment(Alignment::Left)
        .style(Style::default().bg(Color::DarkGray));
    frame.render_widget(paragraph, regions.status);
}

fn render_buffer(frame: &mut Frame, regions: Regions, buffer: &Buffer) {
    let mut lines: Vec<Line<'_>> = Vec::new();
    for block in buffer.blocks() {
        for line in block_to_lines(block) {
            lines.push(line);
        }
        // Blank separator between blocks.
        lines.push(Line::raw(""));
    }
    let scroll = u16::try_from(buffer.scroll()).unwrap_or(u16::MAX);
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, regions.buffer);
}

fn render_input(frame: &mut Frame, regions: Regions, input: &InputState) {
    let title = Line::from(format!(
        " prompt [{}] ",
        mode_label(input.mode()).to_lowercase()
    ));
    let body = Paragraph::new(input.text())
        .wrap(Wrap { trim: false })
        .block(RtBlock::default().title(title).borders(Borders::TOP));
    frame.render_widget(body, regions.input);
}

/// Convert one [`Block`] into its rendered [`Line`]s.
///
/// Folded blocks contribute one header line. Unfolded blocks contribute
/// the header plus the body. Assistant text has no header; it is the
/// content directly. Thinking text is rendered dimmed.
#[must_use]
pub fn block_to_lines(block: &Block) -> Vec<Line<'static>> {
    match block {
        Block::User { text } => prefixed_lines(">", text, user_style()),
        Block::Assistant { text, .. } => plain_lines(text, assistant_style()),
        Block::Thinking { text, folded, .. } => {
            let mut out = Vec::new();
            out.push(header_line(
                fold_indicator(*folded),
                "thinking",
                None,
                thinking_style(),
            ));
            if !*folded {
                for body_line in plain_lines(text, thinking_style()) {
                    out.push(prefix_line("  ", body_line));
                }
            }
            out
        }
        Block::ToolCall {
            name,
            input_summary,
            input_pretty,
            folded,
            ..
        } => {
            let mut out = Vec::new();
            let header_text = if input_summary.is_empty() {
                format!("{name}()")
            } else {
                format!("{name}({input_summary})")
            };
            out.push(header_line(
                fold_indicator(*folded),
                "tool",
                Some(header_text),
                tool_call_style(),
            ));
            if !*folded {
                for body_line in plain_lines(input_pretty, tool_call_style()) {
                    out.push(prefix_line("  ", body_line));
                }
            }
            out
        }
        Block::ToolResult {
            name,
            output,
            is_error,
            folded,
            ..
        } => {
            let style = if *is_error {
                tool_error_style()
            } else {
                tool_result_style()
            };
            let mut out = Vec::new();
            let header_text = if *is_error {
                format!("{name} (error)")
            } else {
                name.clone()
            };
            out.push(header_line(
                fold_indicator(*folded),
                "result",
                Some(header_text),
                style,
            ));
            if !*folded {
                for body_line in plain_lines(output, style) {
                    out.push(prefix_line("  ", body_line));
                }
            }
            out
        }
        Block::Custom {
            kind, text, folded, ..
        } => {
            let mut out = Vec::new();
            out.push(header_line(
                fold_indicator(*folded),
                kind,
                None,
                custom_style(),
            ));
            if !*folded {
                for body_line in plain_lines(text, custom_style()) {
                    out.push(prefix_line("  ", body_line));
                }
            }
            out
        }
    }
}

fn prefixed_lines(prefix: &str, text: &str, style: Style) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut first = true;
    for raw in text.split('\n') {
        let p = if first { prefix } else { " " };
        first = false;
        out.push(Line::from(vec![
            Span::styled(format!("{p} "), style.add_modifier(Modifier::BOLD)),
            Span::styled(raw.to_owned(), style),
        ]));
    }
    out
}

fn plain_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .map(|line| Line::from(Span::styled(line.to_owned(), style)))
        .collect()
}

fn header_line(indicator: char, tag: &str, detail: Option<String>, style: Style) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!("{indicator} "), style.add_modifier(Modifier::BOLD)),
        Span::styled(format!("[{tag}]"), style.add_modifier(Modifier::BOLD)),
    ];
    if let Some(d) = detail {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(d, style));
    }
    Line::from(spans)
}

fn prefix_line(prefix: &str, line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(prefix.to_owned()));
    spans.extend(line.spans);
    Line::from(spans)
}

fn fold_indicator(folded: bool) -> char {
    if folded { '>' } else { 'v' }
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "NOR",
        Mode::Insert => "INS",
        Mode::Visual => "VIS",
    }
}

fn mode_style(mode: Mode) -> Style {
    let bg = match mode {
        Mode::Normal => Color::Blue,
        Mode::Insert => Color::Green,
        Mode::Visual => Color::Magenta,
    };
    Style::default()
        .fg(Color::White)
        .bg(bg)
        .add_modifier(Modifier::BOLD)
}

fn user_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn assistant_style() -> Style {
    Style::default().fg(Color::White)
}

fn thinking_style() -> Style {
    Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::DIM | Modifier::ITALIC)
}

fn tool_call_style() -> Style {
    Style::default().fg(Color::Yellow)
}

fn tool_result_style() -> Style {
    Style::default().fg(Color::Gray)
}

fn tool_error_style() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

fn custom_style() -> Style {
    Style::default().fg(Color::Magenta)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    use super::*;
    use crate::buffer::Buffer;

    fn snapshot_lines(buffer: &Buffer, input: &InputState, area: Rect) -> Vec<String> {
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let regions = crate::layout::split(frame.area(), 1);
                render(frame, regions, buffer, input);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = Vec::new();
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            out.push(row.trim_end().to_owned());
        }
        out
    }

    #[test]
    fn folded_thinking_renders_one_line() {
        let mut buffer = Buffer::new();
        buffer.append_thinking_delta("step 1\nstep 2");
        buffer.finish_streaming();
        // Thinking starts unfolded; fold it so we can assert the body
        // doesn't make it to the screen.
        assert!(buffer.toggle_fold(0));
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 40, 6));
        assert!(lines.iter().any(|l| l.contains("[thinking]")));
        assert!(!lines.iter().any(|l| l.contains("step 1")));
    }

    #[test]
    fn unfolded_thinking_includes_body() {
        let mut buffer = Buffer::new();
        buffer.append_thinking_delta("step 1\nstep 2");
        buffer.finish_streaming();
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 40, 8));
        assert!(lines.iter().any(|l| l.contains("[thinking]")));
        assert!(lines.iter().any(|l| l.contains("step 1")));
        assert!(lines.iter().any(|l| l.contains("step 2")));
    }

    #[test]
    fn assistant_text_renders_without_header() {
        let mut buffer = Buffer::new();
        buffer.append_assistant_delta("hi there");
        buffer.finish_streaming();
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 40, 5));
        assert!(lines.iter().any(|l| l.contains("hi there")));
        // No `[assistant]` header tag.
        assert!(!lines.iter().any(|l| l.contains("[assistant]")));
    }

    #[test]
    fn user_block_has_prefix() {
        let mut buffer = Buffer::new();
        buffer.push_user("hello");
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 40, 5));
        assert!(lines.iter().any(|l| l.contains("> hello")));
    }

    #[test]
    fn status_bar_shows_mode_label() {
        let buffer = Buffer::new();
        let mut input = InputState::new();
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 20, 4));
        assert!(lines[0].contains("INS"));
    }
}
