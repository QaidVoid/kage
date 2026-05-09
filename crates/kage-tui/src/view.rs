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
    let total_lines = lines.len();
    let visible = usize::from(regions.buffer.height);
    // [`Buffer::scroll`] is rows from the bottom; the Paragraph wants
    // rows from the top. Translate, clamping so the viewport never
    // drops past the last line of content.
    let max_scroll_back = total_lines.saturating_sub(visible);
    let scroll_back = buffer.scroll().min(max_scroll_back);
    let top_offset = max_scroll_back - scroll_back;
    let scroll = u16::try_from(top_offset).unwrap_or(u16::MAX);
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
    let scroll_off = input_scroll_offset(input, regions.input);
    let body = Paragraph::new(input.text())
        .wrap(Wrap { trim: false })
        .scroll((scroll_off, 0))
        .block(RtBlock::default().title(title).borders(Borders::TOP));
    frame.render_widget(body, regions.input);
    if input.mode() == Mode::Insert {
        if let Some(pos) = input_cursor_position(input, regions.input, scroll_off) {
            frame.set_cursor_position(pos);
        }
    }
}

/// How many rows to scroll the input Paragraph so that the cursor row
/// always stays inside the visible content area. Once the prompt has
/// more rows than the input area can fit (`INPUT_MAX_LINES = 8`),
/// scrolling is the only way to keep typing visible.
fn input_scroll_offset(input: &InputState, area: ratatui::layout::Rect) -> u16 {
    if area.height < 2 {
        return 0;
    }
    let content_height = usize::from(area.height - 1);
    if content_height == 0 {
        return 0;
    }
    let prefix = input.text().get(..input.cursor()).unwrap_or("");
    let cursor_row = prefix.matches('\n').count();
    let max_visible_row = content_height - 1;
    let off = cursor_row.saturating_sub(max_visible_row);
    u16::try_from(off).unwrap_or(u16::MAX)
}

/// Compute the screen position of the prompt cursor inside the input
/// region. Returns `None` if the input has no inner area (a one-row
/// region collapses to the title border alone).
fn input_cursor_position(
    input: &InputState,
    area: ratatui::layout::Rect,
    scroll_off: u16,
) -> Option<(u16, u16)> {
    if area.height < 2 || area.width == 0 {
        return None;
    }
    let inner_x = area.x;
    let inner_y = area.y + 1;
    let max_x = area.x + area.width - 1;
    let max_y = area.y + area.height - 1;
    let prefix = input.text().get(..input.cursor()).unwrap_or("");
    let row_offset = u16::try_from(prefix.matches('\n').count())
        .unwrap_or(u16::MAX)
        .saturating_sub(scroll_off);
    let last_line = prefix.rsplit('\n').next().unwrap_or("");
    let col_offset = u16::try_from(last_line.chars().count()).unwrap_or(u16::MAX);
    let cx = inner_x.saturating_add(col_offset).min(max_x);
    let cy = inner_y.saturating_add(row_offset).min(max_y);
    Some((cx, cy))
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
    fn folded_tool_call_renders_one_header_line() {
        let mut buffer = Buffer::new();
        buffer.push_tool_call("c1", "bash", "ls -la", "{\n  \"cmd\": \"ls -la\"\n}");
        // Tool calls start folded.
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 60, 6));
        let header_line = lines
            .iter()
            .find(|l| l.contains("[tool]"))
            .expect("tool header present");
        assert!(header_line.contains("bash(ls -la)"));
        assert!(!lines.iter().any(|l| l.contains("\"cmd\"")));
    }

    #[test]
    fn unfolded_tool_call_shows_full_input_body() {
        let mut buffer = Buffer::new();
        buffer.push_tool_call("c1", "bash", "ls -la", "{\n  \"cmd\": \"ls -la\"\n}");
        assert!(buffer.toggle_fold(0));
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 60, 12));
        assert!(lines.iter().any(|l| l.contains("[tool]")));
        assert!(lines.iter().any(|l| l.contains("\"cmd\"")));
    }

    #[test]
    fn tool_result_error_renders_distinct_header() {
        let mut buffer = Buffer::new();
        buffer.push_tool_call("c1", "bash", "false", "{}");
        buffer.push_tool_result("c1", "exit 1", true);
        let input = InputState::new();
        let lines = snapshot_lines(&buffer, &input, Rect::new(0, 0, 60, 12));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("[result]") && l.contains("error"))
        );
    }

    #[test]
    fn cursor_position_advances_with_typed_text() {
        let area = Rect::new(0, 4, 40, 4);
        let mut input = InputState::new();
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        for c in "hello".chars() {
            input.handle_key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char(c),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        }
        let pos = super::input_cursor_position(&input, area, 0).unwrap();
        // Inner row = area.y + 1 = 5; cursor column = 5 chars into row.
        assert_eq!(pos, (5, 5));
    }

    #[test]
    fn cursor_position_walks_to_next_row_on_newline() {
        let area = Rect::new(0, 0, 20, 5);
        let mut input = InputState::new();
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        // Paste pre-builds multi-line content cheaply.
        input.paste("ab\ncd");
        let pos = super::input_cursor_position(&input, area, 0).unwrap();
        // Two rows below the title border -> y = 0 + 1 + 1 = 2; col = 2.
        assert_eq!(pos, (2, 2));
    }

    #[test]
    fn input_scrolls_when_cursor_row_exceeds_visible_height() {
        // Area height = 4 rows -> 1 border + 3 content rows.
        let area = Rect::new(0, 0, 40, 4);
        let mut input = InputState::new();
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        // Five rows of content; cursor lands on row 4 (last line).
        input.paste("a\nb\nc\nd\ne");
        let off = super::input_scroll_offset(&input, area);
        // cursor_row=4, max_visible_row=2 -> scroll by 2.
        assert_eq!(off, 2);
        // Cursor renders on the last visible row of the area (y = 3).
        let pos = super::input_cursor_position(&input, area, off).unwrap();
        assert_eq!(pos.1, 3);
    }

    #[test]
    fn input_does_not_scroll_when_text_fits() {
        let area = Rect::new(0, 0, 40, 6);
        let mut input = InputState::new();
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        input.paste("a\nb\nc");
        assert_eq!(super::input_scroll_offset(&input, area), 0);
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
