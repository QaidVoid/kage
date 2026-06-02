//! Tests for view rendering helpers.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use super::*;
use crate::buffer::Buffer;

// --- Word-wrap helpers ---

#[test]
fn wrap_breaks_at_word_boundary_when_word_fits() {
    // Width 10 fits "hello" (5) + space + "world" (5) = 11 chars,
    // so "world" must wrap to a new row instead of splitting.
    let rows = wrap_input_rows("hello world", 10);
    let rendered: Vec<&str> = rows.iter().map(|(s, e)| &"hello world"[*s..*e]).collect();
    assert_eq!(rendered, vec!["hello", "world"]);
}

#[test]
fn wrap_falls_back_to_char_break_for_oversize_word() {
    // 15-char word in width 10 should char-break at 10 chars.
    let rows = wrap_input_rows("aaaaaaaaaaaaaaa", 10);
    let rendered: Vec<&str> = rows
        .iter()
        .map(|(s, e)| &"aaaaaaaaaaaaaaa"[*s..*e])
        .collect();
    assert_eq!(rendered, vec!["aaaaaaaaaa", "aaaaa"]);
}

#[test]
fn wrap_preserves_logical_newlines_as_row_breaks() {
    let rows = wrap_input_rows("ab\ncd", 10);
    assert_eq!(rows, vec![(0, 2), (3, 5)]);
}

#[test]
fn wrap_empty_logical_line_emits_one_zero_length_row() {
    let rows = wrap_input_rows("\n", 10);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1 - rows[0].0, 0);
}

#[test]
fn visual_cursor_word_wrapped_matches_paint() {
    // "hello world" at width 10 wraps to ["hello", "world"].
    // Cursor at byte 6 (start of "world") should be at (row 1, col 0).
    let (row, col) = input_visual_cursor("hello world", 6, 10);
    assert_eq!((row, col), (1, 0));
}

#[test]
fn visual_cursor_at_end_of_first_row_after_word_break() {
    // Cursor at byte 5 (the space) should be at end of row 0.
    let (row, col) = input_visual_cursor("hello world", 5, 10);
    assert_eq!((row, col), (0, 5));
}

#[test]
fn row_count_matches_actual_painted_rows() {
    // Three short words separated by spaces should be one row,
    // since they total 9 + 2 spaces = 11 > 10? No: "a b c" = 5
    // chars in width 10 = 1 row.
    assert_eq!(input_visual_row_count("a b c", 10), 1);
    // "alpha beta" = 10 chars, alpha+space+beta = 4+1+4 = 9 fits.
    assert_eq!(input_visual_row_count("alpha beta", 10), 1);
    // "alpha beta gamma" = 16 chars, wraps to 2 rows.
    assert_eq!(input_visual_row_count("alpha beta gamma", 10), 2);
}

// --- split_line_into_rows (block widgets) ---

fn row_text(row: &[Span<'_>]) -> String {
    row.iter().map(|s| s.content.as_ref()).collect()
}

#[test]
fn block_wrap_breaks_at_word_boundary_when_word_fits() {
    let line = Line::from(Span::raw("hello world"));
    let rows = split_line_into_rows(line, 10);
    let texts: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
    assert_eq!(texts, vec!["hello", "world"]);
}

#[test]
fn block_wrap_falls_back_to_char_break_for_oversize_word() {
    let line = Line::from(Span::raw("aaaaaaaaaaaaaaa"));
    let rows = split_line_into_rows(line, 10);
    let texts: Vec<String> = rows.iter().map(|r| row_text(r)).collect();
    assert_eq!(texts, vec!["aaaaaaaaaa", "aaaaa"]);
}

#[test]
fn block_wrap_preserves_span_styles_across_break() {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let line = Line::from(vec![Span::styled("hello", bold), Span::raw(" world tail")]);
    let rows = split_line_into_rows(line, 11);
    // Row 0 fits "hello world" (5+1+5=11); the bold style on
    // "hello" must survive.
    assert!(rows.len() >= 2, "expected at least 2 rows, got {rows:?}");
    let first_bold = rows[0]
        .iter()
        .any(|s| s.content == "hello" && s.style.add_modifier.contains(Modifier::BOLD));
    assert!(first_bold, "bold style should survive the wrap");
}

#[test]
fn block_wrap_uses_display_width_not_char_count() {
    // Each CJK ideograph is two display columns. With a 6-col
    // budget a row holds at most three; counting `char`s would
    // pack six (12 cols) and the outer `Paragraph::wrap` would
    // then fold the overflow onto a gutter-less continuation,
    // which is the "rule skips wrapped text" symptom.
    let line = Line::from(Span::raw(
        "\u{4e00}\u{4e8c}\u{4e09}\u{56db}\u{4e94}\u{516d}",
    ));
    let rows = split_line_into_rows(line, 6);
    for r in &rows {
        let cells: usize = row_text(r)
            .chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        assert!(
            cells <= 6,
            "row {:?} is {cells} cols, over the 6-col budget",
            row_text(r)
        );
    }
    assert_eq!(rows.len(), 2, "6 wide glyphs at 6 cols is 2 rows of 3");
}

#[test]
fn block_wrap_empty_line_yields_single_empty_row() {
    let line = Line::from(Span::raw(""));
    let rows = split_line_into_rows(line, 10);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].is_empty() || row_text(&rows[0]).is_empty());
}

fn snapshot_lines(buffer: &mut Buffer, input: &InputState, area: Rect) -> Vec<String> {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut captured: std::collections::BTreeMap<usize, Vec<CapturedCell>> =
        std::collections::BTreeMap::new();
    terminal
        .draw(|frame| {
            let regions = crate::layout::split(frame.area(), 1, 0);
            render(
                frame,
                regions,
                buffer,
                input,
                None,
                &StatusCtx::default(),
                None,
                &mut captured,
                None,
                &[],
            );
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
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 6));
    assert!(lines.iter().any(|l| l.contains("[thinking]")));
    assert!(!lines.iter().any(|l| l.contains("step 1")));
}

#[test]
fn unfolded_thinking_includes_body() {
    let mut buffer = Buffer::new();
    buffer.append_thinking_delta("step 1\nstep 2");
    buffer.finish_streaming();
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 8));
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
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 8));
    assert!(lines.iter().any(|l| l.contains("hi there")));
    // No `[assistant]` header tag.
    assert!(!lines.iter().any(|l| l.contains("[assistant]")));
}

#[test]
fn user_block_renders_with_padded_bubble() {
    let mut buffer = Buffer::new();
    buffer.push_user("hello");
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 40, 8));
    // Bubble keeps the prompt text intact; trailing whitespace is
    // trimmed by the test's snapshot helper.
    assert!(lines.iter().any(|l| l.contains("hello")));
    assert!(!lines.iter().any(|l| l.contains("> hello")));
}

#[test]
fn folded_tool_call_renders_name_then_summary_without_brackets() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "bash", "ls -la", "{\n  \"cmd\": \"ls -la\"\n}");
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 8));
    let header = lines
        .iter()
        .find(|l| l.contains("bash"))
        .expect("tool header present");
    assert!(header.contains("bash ls -la"));
    assert!(!header.contains("[tool]"));
    assert!(!header.contains('('));
    assert!(!lines.iter().any(|l| l.contains("\"cmd\"")));
}

#[test]
fn unfolded_tool_call_shows_full_input_body() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "bash", "ls -la", "{\n  \"cmd\": \"ls -la\"\n}");
    assert!(buffer.toggle_fold(0));
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 12));
    assert!(lines.iter().any(|l| l.contains("bash")));
    assert!(lines.iter().any(|l| l.contains("\"cmd\"")));
}

#[test]
fn folded_merged_pair_inlines_status_and_preview() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "bash", "false", "{}");
    buffer.push_tool_result("c1", "exit 1", true);
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 80, 12));
    let header = lines
        .iter()
        .find(|l| l.contains("> bash"))
        .expect("merged tool header");
    assert!(header.contains("ERROR"));
    // Old standalone-result tag should be gone.
    assert!(!lines.iter().any(|l| l.contains("[result]")));
}

#[test]
fn folded_merged_pair_shows_size_pill_and_body_preview() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "read", "README.md", "{}");
    buffer.push_tool_result("c1", "first line of file\nsecond line\nthird line", false);
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 90, 12));
    let header = lines
        .iter()
        .find(|l| l.contains("> read"))
        .expect("merged folded read header");
    assert!(header.contains(" B"), "expected size pill, got: {header}");
    assert!(
        lines.iter().any(|l| l.contains("first line of file")),
        "expected body preview line"
    );
    assert!(
        lines.iter().any(|l| l.contains("third line")),
        "expected body preview line"
    );
}

#[test]
fn unfolded_merged_pair_shows_body_and_inline_status() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "ls", ".", "{}");
    buffer.push_tool_result("c1", "a.rs\nb.rs\nc.rs", false);
    // Toggling either half flips both, so unfolding via the call
    // (idx 0) leaves the merged renderer with full body visible.
    assert!(buffer.toggle_fold(0));
    let input = InputState::new();
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 16));
    // Unfolded fold indicator is `v`.
    let header = lines
        .iter()
        .find(|l| l.contains("v ls"))
        .expect("unfolded ls header");
    // Header carries the size + Took inline.
    assert!(header.contains(" B"), "expected size pill, got: {header}");
    assert!(header.contains("Took"), "expected Took, got: {header}");
    assert!(lines.iter().any(|l| l.contains("a.rs")));
    assert!(lines.iter().any(|l| l.contains("c.rs")));
}

#[test]
fn toggling_either_half_of_a_pair_flips_both() {
    let mut buffer = Buffer::new();
    buffer.push_tool_call("c1", "ls", ".", "{}");
    buffer.push_tool_result("c1", "a", false);
    assert!(matches!(
        buffer.blocks()[0],
        Block::ToolCall { folded: true, .. }
    ));
    assert!(matches!(
        buffer.blocks()[1],
        Block::ToolResult { folded: true, .. }
    ));
    // Toggle the result; the call should flip too.
    assert!(buffer.toggle_fold(1));
    assert!(matches!(
        buffer.blocks()[0],
        Block::ToolCall { folded: false, .. }
    ));
    assert!(matches!(
        buffer.blocks()[1],
        Block::ToolResult { folded: false, .. }
    ));
}

#[test]
fn small_tool_output_is_not_truncated() {
    let style = Style::default();
    let lines = super::truncated_body_lines("a\nb\nc", style);
    assert_eq!(lines.len(), 3);
}

#[test]
fn over_200_line_output_is_capped_with_marker() {
    let style = Style::default();
    let raw: String = (0..250)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let lines = super::truncated_body_lines(&raw, style);
    // 200 capped lines + 1 marker.
    assert_eq!(lines.len(), 201);
    let last = format!("{}", lines.last().unwrap().spans[0].content);
    assert!(last.contains("more lines"), "got: {last}");
    assert!(last.contains("50"));
}

#[test]
fn many_short_lines_past_byte_budget_are_capped() {
    let style = Style::default();
    // 5000 lines of 8 chars each = ~45 KB, exceeds the 16 KB cap.
    let raw: String = (0..5000)
        .map(|i| format!("line{i:04}"))
        .collect::<Vec<_>>()
        .join("\n");
    let lines = super::truncated_body_lines(&raw, style);
    // We hit MAX_BODY_BYTES well before MAX_BODY_LINES; the body
    // ends with a "... (N more lines)" marker.
    let last = format!("{}", lines.last().unwrap().spans[0].content);
    assert!(last.contains("more lines"), "got: {last}");
}

#[test]
fn human_size_formats_units() {
    assert_eq!(super::human_size(512), "512 B");
    assert_eq!(super::human_size(2048), "2.0 KB");
    assert_eq!(super::human_size(1_500_000), "1.4 MB");
    assert_eq!(super::human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
}

#[test]
fn token_counts_scale_to_k_m_b_trimmed() {
    assert_eq!(super::format_token_count(999), "999");
    assert_eq!(super::format_token_count(1_000), "1k");
    assert_eq!(super::format_token_count(1_160), "1.16k");
    assert_eq!(super::format_token_count(78_700), "78.7k");
    assert_eq!(super::format_token_count(200_000), "200k");
    assert_eq!(super::format_token_count(1_500_000), "1.5M");
    assert_eq!(super::format_token_count(21_000_000), "21M");
    assert_eq!(super::format_token_count(200_000_000), "200M");
    assert_eq!(super::format_token_count(2_000_000_000), "2B");
}

#[test]
fn first_line_preview_skips_empty_leading_lines_and_truncates() {
    assert_eq!(
        super::first_line_preview("\n\nhello world", 20).as_deref(),
        Some("hello world")
    );
    assert_eq!(
        super::first_line_preview(&"a".repeat(80), 20).as_deref(),
        Some(&*format!("{}...", "a".repeat(17)))
    );
    assert_eq!(super::first_line_preview("\n\n  \n", 10), None);
}

/// Mirror what [`super::render_input`] does to derive the inner
/// content rect (`body_area`) from a full input region rect: inset
/// by one cell on every side for the bordered card, then by
/// [`super::INPUT_GLYPH_WIDTH`] columns on the left for the prompt
/// glyph.
fn body_area_for(region: Rect) -> Rect {
    Rect::new(
        region.x + 1 + super::INPUT_GLYPH_WIDTH,
        region.y + 1,
        region.width.saturating_sub(2 + super::INPUT_GLYPH_WIDTH),
        region.height.saturating_sub(2),
    )
}

#[test]
fn cursor_position_advances_with_typed_text() {
    let region = Rect::new(0, 4, 40, 4);
    let body = body_area_for(region);
    let mut input = InputState::new();
    // Default mode is Insert; no need to press 'i'.
    for c in "hello".chars() {
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char(c),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
    }
    let pos = super::input_cursor_position(&input, body, 0).unwrap();
    // body.x = 3 (border + glyph), body.y = 5 (skip top border);
    // 5 chars typed -> col 8, row 5.
    assert_eq!(pos, (body.x + 5, body.y));
}

#[test]
fn cursor_position_walks_to_next_row_on_newline() {
    let region = Rect::new(0, 0, 20, 5);
    let body = body_area_for(region);
    let mut input = InputState::new();
    // Default mode is Insert; no need to press 'i'.
    // Paste pre-builds multi-line content cheaply.
    input.paste("ab\ncd");
    let pos = super::input_cursor_position(&input, body, 0).unwrap();
    // Second logical row, 2 chars in -> col body.x + 2, row body.y + 1.
    assert_eq!(pos, (body.x + 2, body.y + 1));
}

#[test]
fn input_scrolls_when_cursor_row_exceeds_visible_height() {
    // Region height = 5 rows -> 2 chrome + 3 content rows.
    let region = Rect::new(0, 0, 40, 5);
    let body = body_area_for(region);
    assert_eq!(body.height, 3);
    let mut input = InputState::new();
    // Default mode is Insert; no need to press 'i'.
    // Five rows of content; cursor lands on row 4 (last line).
    input.paste("a\nb\nc\nd\ne");
    let off = super::input_scroll_offset(&input, body);
    // cursor_row=4, max_visible_row=2 -> scroll by 2.
    assert_eq!(off, 2);
    // Cursor renders on the last visible row of the body area.
    let pos = super::input_cursor_position(&input, body, off).unwrap();
    assert_eq!(pos.1, body.y + body.height - 1);
}

#[test]
fn input_does_not_scroll_when_text_fits() {
    let region = Rect::new(0, 0, 40, 6);
    let body = body_area_for(region);
    let mut input = InputState::new();
    // Default mode is Insert; no need to press 'i'.
    input.paste("a\nb\nc");
    assert_eq!(super::input_scroll_offset(&input, body), 0);
}

#[test]
fn input_card_shows_mode_pill() {
    // Mode display lives on the input card's top border now (not
    // on the top status bar). Frame is wide enough so the pill
    // fits inside the card border.
    let mut buffer = Buffer::new();
    let input = InputState::new();
    // Default mode is Insert; the pill should show * without
    // pressing 'i'.
    let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 8));
    assert!(
        lines.iter().any(|l| l.contains('*')),
        "expected mode pill * somewhere on screen, got: {lines:#?}"
    );
    // Top status bar no longer carries the mode pill.
    assert!(!lines[0].contains('*'));
}

fn snapshot_with_cmdline(cmdline: &CommandLine, area: Rect) -> Vec<String> {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut buffer = Buffer::new();
    let input = InputState::new();
    let mut captured: std::collections::BTreeMap<usize, Vec<CapturedCell>> =
        std::collections::BTreeMap::new();
    terminal
        .draw(|frame| {
            let regions = crate::layout::split(frame.area(), 1, 0);
            render(
                frame,
                regions,
                &mut buffer,
                &input,
                Some(cmdline),
                &StatusCtx::default(),
                None,
                &mut captured,
                None,
                &[],
            );
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

fn cell_bg_at(cmdline: &CommandLine, area: Rect, x: u16, y: u16) -> Color {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut buffer = Buffer::new();
    let input = InputState::new();
    let mut captured: std::collections::BTreeMap<usize, Vec<CapturedCell>> =
        std::collections::BTreeMap::new();
    terminal
        .draw(|frame| {
            let regions = crate::layout::split(frame.area(), 1, 0);
            render(
                frame,
                regions,
                &mut buffer,
                &input,
                Some(cmdline),
                &StatusCtx::default(),
                None,
                &mut captured,
                None,
                &[],
            );
        })
        .unwrap();
    terminal.backend().buffer()[(x, y)].bg
}

fn completion(value: &str, description: Option<&str>) -> crate::cmdparse::Completion {
    crate::cmdparse::Completion {
        value: value.to_owned(),
        description: description.map(str::to_owned),
        replace_range: 0..0,
    }
}

#[test]
fn popup_paints_nothing_with_zero_completions() {
    let empty = crate::cmdparse::Completions::default();
    let cl = CommandLine::for_test("", empty, true, None);
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 40, 12));
    // Row 1 is the first row below the status; it should be blank
    // (or at least not contain any completion text we did not give).
    assert!(
        lines[1].chars().all(|c| c == ' '),
        "row 1 should be blank, got {:?}",
        lines[1]
    );
}

#[test]
fn popup_paints_single_item_text() {
    let completions = crate::cmdparse::Completions {
        items: vec![completion("model", Some("switch model"))],
        anchor: 0,
    };
    let cl = CommandLine::for_test("m", completions, true, None);
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 40, 12));
    let popup_row = lines
        .iter()
        .skip(1)
        .find(|l| l.contains("model"))
        .expect("popup row containing 'model'");
    assert!(popup_row.contains("switch model"), "got {popup_row:?}");
}

#[test]
fn popup_paints_many_items_and_highlights_selected() {
    let completions = crate::cmdparse::Completions {
        items: vec![
            completion("model", Some("switch model")),
            completion("mouse", Some("toggle mouse")),
        ],
        anchor: 0,
    };
    let cl = CommandLine::for_test("mo", completions, true, Some(1));
    let area = Rect::new(0, 0, 50, 12);
    let lines = snapshot_with_cmdline(&cl, area);
    assert!(lines.iter().any(|l| l.contains("model")), "{lines:#?}");
    assert!(lines.iter().any(|l| l.contains("mouse")), "{lines:#?}");
    // The selected row (index 1, painted at y=2) should have the
    // blue selection bg; the unselected row (y=1) should not.
    let sel_bg = cell_bg_at(&cl, area, 3, 2);
    let unsel_bg = cell_bg_at(&cl, area, 3, 1);
    assert_eq!(sel_bg, Color::Blue, "selected row bg should be blue");
    assert_ne!(
        unsel_bg,
        Color::Blue,
        "unselected row bg should not be blue"
    );
}

#[test]
fn popup_scrolls_to_keep_selected_in_view() {
    let items: Vec<crate::cmdparse::Completion> = (0..12)
        .map(|i| completion(&format!("cmd{i:02}"), None))
        .collect();
    let completions = crate::cmdparse::Completions { items, anchor: 0 };

    // selected near top: window starts at 0, no "above" indicator,
    // "below" indicator shows the off-screen tail.
    let cl_top = CommandLine::for_test("c", completions.clone(), true, Some(2));
    let lines = snapshot_with_cmdline(&cl_top, Rect::new(0, 0, 40, 20));
    assert!(
        lines.iter().any(|l| l.contains("cmd00")),
        "cmd00 should be visible near the top, got {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("more below")),
        "expected 'more below' indicator, got {lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("more above")),
        "no 'more above' indicator near top, got {lines:#?}"
    );

    // selected past the bottom: window slides so selected is the last
    // visible row, both indicators present.
    let cl_bottom = CommandLine::for_test("c", completions.clone(), true, Some(10));
    let lines = snapshot_with_cmdline(&cl_bottom, Rect::new(0, 0, 40, 20));
    assert!(
        lines.iter().any(|l| l.contains("cmd10")),
        "selected cmd10 must be visible, got {lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("cmd00")),
        "cmd00 should have scrolled out of view, got {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("more above")),
        "expected 'more above' indicator, got {lines:#?}"
    );
}

#[test]
fn popup_truncates_description_in_narrow_viewport() {
    let completions = crate::cmdparse::Completions {
        items: vec![completion(
            "model",
            Some("switch to a provider:model identifier from the catalog"),
        )],
        anchor: 0,
    };
    let cl = CommandLine::for_test("m", completions, true, None);
    // 28 cells total: leading "  " + "model" (5) + "  " + ~19 desc chars + ellipsis.
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 28, 8));
    let popup_row = lines
        .iter()
        .skip(1)
        .find(|l| l.contains("model"))
        .expect("popup row");
    assert!(
        popup_row.contains('\u{2026}'),
        "expected ellipsis in narrow row, got {popup_row:?}",
    );
    assert!(
        !popup_row.contains("catalog"),
        "narrow viewport should drop the tail of the description, got {popup_row:?}",
    );
}

// --- Inline error rendering tests (PN.9) ---

#[test]
fn error_line_shows_marker_and_message() {
    let cl = CommandLine::for_test_with_error(
        "mouse mayb",
        "argument `state` must be one of on|off|toggle",
    );
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 60, 12));
    // Row 0 is the status row with ":mouse mayb".
    // Row 1 should contain the error marker and message.
    assert!(
        lines[0].contains("mouse mayb"),
        "status row should show typed text, got {:?}",
        lines[0]
    );
    assert!(
        lines[1].contains('!'),
        "error row should contain the error marker, got {:?}",
        lines[1]
    );
    assert!(
        lines[1].contains("must be one of"),
        "error row should contain the error message, got {:?}",
        lines[1]
    );
}

#[test]
fn error_line_suppresses_popup() {
    let completions = crate::cmdparse::Completions {
        items: vec![
            completion("model", Some("switch model")),
            completion("mouse", Some("toggle mouse")),
        ],
        anchor: 0,
    };
    // Error is set even though completions are populated.
    let mut cl = CommandLine::for_test("mo", completions, true, None);
    cl.set_error("fix your input");
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 50, 12));
    // The popup should be suppressed; only the error row appears.
    assert!(
        !lines.iter().any(|l| l.contains("switch model")),
        "popup should be suppressed when error is active, got {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("fix your input")),
        "error message should be visible, got {lines:#?}"
    );
}

#[test]
fn error_line_truncates_in_narrow_viewport() {
    let long_msg = "this is a very long error message that should definitely be truncated when the viewport is narrow";
    let cl = CommandLine::for_test_with_error("x", long_msg);
    let lines = snapshot_with_cmdline(&cl, Rect::new(0, 0, 30, 8));
    let error_row = &lines[1];
    assert!(
        error_row.contains('\u{2026}'),
        "long error should be truncated with ellipsis, got {error_row:?}"
    );
}
