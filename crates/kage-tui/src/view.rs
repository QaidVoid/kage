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

pub mod assistant;
pub mod thinking;
pub mod tool_pair;
pub mod user;
pub mod widget;

pub use assistant::AssistantBlockWidget;
pub use thinking::ThinkingBlockWidget;
pub use tool_pair::ToolPairBlockWidget;
pub use user::UserBlockWidget;
pub use widget::{BlockWidget, EmptyBlockWidget, RenderCtx, SelectionState};

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RtBlock, Borders, Paragraph, Wrap};

use crate::buffer::{Block, Buffer};
use crate::cmdline::CommandLine;
use crate::input::{InputState, Mode, Pane};
use crate::layout::Regions;
use crate::usage::SessionUsage;

/// Read-only snapshot of the live state the status bar needs to
/// paint. Built fresh each frame from whatever the host has wired in.
#[derive(Default)]
pub struct StatusCtx<'a> {
    /// Active `provider:model` id, if known.
    pub model: Option<&'a str>,
    /// Short session id pill, if recording is active.
    pub session_id: Option<&'a str>,
    /// Currently submitted search pattern, if any. Blocks whose
    /// content contains this pattern get a `Match` emphasis.
    pub search_pattern: Option<&'a str>,
    /// Open `/` search line, if the user is mid-typing one.
    pub search_line: Option<&'a CommandLine>,
    /// `(current_1_indexed, total)` for the active search. `current`
    /// is `0` when the focus isn't on any match. Painted as
    /// `match X/Y` on the right side of the status bar.
    pub search_match_count: Option<(usize, usize)>,
}

/// `Modifier` bit reserved as the per-cell "decoration" tag - the
/// renderer's bubble/rule/padding code OR's this onto every span it
/// paints purely for chrome, and the cell-based selection path
/// queries it to skip non-selectable cells. Plays the same role as
/// `selectable={false}` in `OpenTUI`'s virtual DOM, but lives on the
/// already-rendered cell grid so we don't need a parallel scene
/// graph. `SLOW_BLINK` is unused by everything else in this crate
/// and most terminal emulators ignore it visually, so it's a safe
/// hijack.
pub(crate) const DECORATION_MARKER: Modifier = Modifier::SLOW_BLINK;

/// True when a cell's modifier carries the decoration marker. Used
/// by [`capture_and_overlay`] to skip overlay painting on chrome
/// cells and by the host's yank path to filter them out of clipboard
/// text.
fn cell_is_decoration(modifier: Modifier) -> bool {
    modifier.contains(DECORATION_MARKER)
}

/// What kind of attention a block should draw on this frame: the
/// navigation head (white rule), a search match (yellow rule), or
/// neither. `Ord` is implemented so merged tool pairs pick `max`
/// across both halves; Focused beats Match beats None.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Emphasis {
    /// No special highlight.
    None,
    /// Block contains a hit for the active search pattern.
    Match,
    /// Block is the navigation head.
    Focused,
}

impl Emphasis {
    pub(super) fn rule_glyph(self) -> &'static str {
        match self {
            Self::None => "\u{258e}",
            Self::Match | Self::Focused => "\u{258c}",
        }
    }

    pub(super) fn rule_color(self, base: Color) -> Color {
        let t = crate::theme::current();
        match self {
            Self::None => base,
            Self::Focused => t.focus_color,
            Self::Match => t.match_color,
        }
    }
}

/// Paint the entire TUI for one frame.
///
/// Takes `buffer` mutably so the renderer can write back the clamped
/// scroll position. Without this, when `Buffer::scroll` inflates past
/// the actual max (because the user kept pressing `k`), pressing `j`
/// has no visible effect until the inflated count drains down to the
/// renderer-clamped value. Persisting the clamp here keeps user input
/// in sync with what's on screen.
#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    regions: Regions,
    buffer: &mut Buffer,
    input: &InputState,
    cmdline: Option<&CommandLine>,
    status: &StatusCtx<'_>,
    screen_selection: Option<((usize, u16), (usize, u16))>,
    captured_rows: &mut std::collections::BTreeMap<usize, Vec<CapturedCell>>,
    session_usage: Option<&SessionUsage>,
) {
    render_status(frame, regions, input, cmdline, status);
    render_buffer(frame, regions, buffer, status.search_pattern);
    render_input(frame, regions, input);
    render_modeline(frame, regions, session_usage);
    if let Some(cl) = cmdline {
        place_cmdline_cursor(frame, regions, cl);
    } else if let Some(sl) = status.search_line {
        place_search_cursor(frame, regions, sl);
    }
    capture_and_overlay(frame, regions, buffer, screen_selection, captured_rows);
}

fn render_status(
    frame: &mut Frame,
    regions: Regions,
    _input: &InputState,
    cmdline: Option<&CommandLine>,
    status: &StatusCtx<'_>,
) {
    let theme = crate::theme::current();
    if let Some(cl) = cmdline {
        let line = Line::from(vec![
            Span::styled(":", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(cl.text().to_owned()),
        ]);
        let paragraph = Paragraph::new(line)
            .alignment(Alignment::Left)
            .style(Style::default().bg(theme.status_bg));
        frame.render_widget(paragraph, regions.status);
        return;
    }
    if let Some(sl) = status.search_line {
        let line = Line::from(vec![
            Span::styled(
                "/",
                Style::default()
                    .fg(theme.match_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(sl.text().to_owned()),
        ]);
        let paragraph = Paragraph::new(line)
            .alignment(Alignment::Left)
            .style(Style::default().bg(theme.status_bg));
        frame.render_widget(paragraph, regions.status);
        return;
    }

    let bg_style = Style::default().bg(theme.status_bg);
    let dim = Style::default()
        .fg(theme.status_dim_fg)
        .bg(theme.status_bg)
        .add_modifier(Modifier::DIM);
    let mut left_spans = vec![Span::styled(
        " kage".to_owned(),
        bg_style.add_modifier(Modifier::BOLD),
    )];
    if let Some(model) = status.model
        && !model.is_empty()
    {
        left_spans.push(Span::styled("  ".to_owned(), bg_style));
        left_spans.push(Span::styled(model.to_owned(), dim));
    }
    let mut right_spans: Vec<Span<'static>> = Vec::new();
    if let Some((current, total)) = status.search_match_count {
        let label = if total == 0 {
            "no match".to_owned()
        } else if current == 0 {
            format!("match -/{total}")
        } else {
            format!("match {current}/{total}")
        };
        right_spans.push(Span::styled(
            format!("{label}  "),
            Style::default()
                .fg(theme.match_color)
                .bg(theme.status_bg)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(sid) = status.session_id
        && !sid.is_empty()
    {
        right_spans.push(Span::styled(format!("session {sid} "), dim));
    }
    let total = usize::from(regions.status.width);
    let left_width: usize = left_spans.iter().map(|s| s.content.chars().count()).sum();
    let right_width: usize = right_spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = total.saturating_sub(left_width + right_width);
    let mut spans = left_spans;
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), bg_style));
    }
    spans.extend(right_spans);
    let paragraph = Paragraph::new(Line::from(spans))
        .alignment(Alignment::Left)
        .style(bg_style);
    frame.render_widget(paragraph, regions.status);
}

/// Same as [`place_cmdline_cursor`] but for the `/` search line.
fn place_search_cursor(frame: &mut Frame, regions: Regions, line: &CommandLine) {
    place_cmdline_cursor(frame, regions, line);
}

/// Walk every `Line` in `lines` and split spans whose text contains
/// `pattern` (ASCII case-insensitive) into pre-match / match /
/// post-match chunks, applying a high-contrast yellow highlight to
/// the matches. Allocates nothing per call beyond the rebuilt span
/// vector; matters because this runs per frame across every block.
fn highlight_matches_in_lines(lines: &mut [Line<'static>], pattern: &str) {
    let needle = pattern.trim();
    if needle.is_empty() {
        return;
    }
    for line in lines {
        let original = std::mem::take(&mut line.spans);
        let mut rebuilt: Vec<Span<'static>> = Vec::with_capacity(original.len());
        for span in original {
            for piece in split_span_for_match(span, needle) {
                rebuilt.push(piece);
            }
        }
        line.spans = rebuilt;
    }
}

/// Find the byte position of `needle` inside `haystack` ignoring
/// ASCII case, starting at `from`. No allocation. Returns absolute
/// byte position into `haystack`.
fn ascii_ifind(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || from >= h.len() || h.len() - from < n.len() {
        return None;
    }
    let limit = h.len() - n.len();
    'outer: for i in from..=limit {
        for j in 0..n.len() {
            if !h[i + j].eq_ignore_ascii_case(&n[j]) {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

fn split_span_for_match(span: Span<'static>, needle: &str) -> Vec<Span<'static>> {
    if ascii_ifind(&span.content, needle, 0).is_none() {
        return vec![span];
    }
    let hit = span.style.patch(
        Style::default()
            .bg(crate::theme::current().match_color)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::REVERSED),
    );
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0;
    while let Some(abs) = ascii_ifind(&span.content, needle, cursor) {
        if abs > cursor {
            out.push(Span::styled(
                span.content[cursor..abs].to_owned(),
                span.style,
            ));
        }
        let end = abs + needle.len();
        out.push(Span::styled(span.content[abs..end].to_owned(), hit));
        if end <= cursor {
            // Defensive: needle was zero-width or boundary fudged;
            // bail to avoid an infinite loop.
            break;
        }
        cursor = end;
    }
    if cursor < span.content.len() {
        out.push(Span::styled(span.content[cursor..].to_owned(), span.style));
    }
    if out.is_empty() {
        return vec![span];
    }
    out
}

/// Position the terminal cursor on the status row at the cmdline's
/// editing position when the `:` command line is open. Without this
/// the user has no visual cue where typing will land.
fn place_cmdline_cursor(frame: &mut Frame, regions: Regions, cmdline: &CommandLine) {
    let row = regions.status;
    if row.width == 0 {
        return;
    }
    let prefix_width = 1u16;
    let col = u16::try_from(cmdline.text()[..cmdline.cursor()].chars().count()).unwrap_or(u16::MAX);
    let cx = row
        .x
        .saturating_add(prefix_width)
        .saturating_add(col)
        .min(row.x + row.width - 1);
    frame.set_cursor_position((cx, row.y));
}

#[allow(clippy::too_many_lines)]
fn render_buffer(
    frame: &mut Frame,
    regions: Regions,
    buffer: &mut Buffer,
    search_pattern: Option<&str>,
) {
    let width = regions.buffer.width;
    let visible = usize::from(regions.buffer.height);
    if width == 0 || visible == 0 {
        return;
    }

    // Owned-key snapshots of the call/result topology. Owning the
    // call_id strings here means we don't keep an immutable borrow of
    // `buffer.blocks()` alive across the height-cache writes below.
    let n = buffer.blocks().len();
    let mut result_by_call: std::collections::HashMap<String, usize> =
        std::collections::HashMap::with_capacity(n);
    for (i, b) in buffer.blocks().iter().enumerate() {
        if let Block::ToolResult { call_id, .. } = b {
            result_by_call.entry(call_id.clone()).or_insert(i);
        }
    }
    let mut consumed_results: std::collections::HashSet<usize> =
        std::collections::HashSet::with_capacity(n);
    let mut call_idx_for_result: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::with_capacity(n);
    for (i, b) in buffer.blocks().iter().enumerate() {
        if let Block::ToolCall { call_id, .. } = b
            && let Some(&rid) = result_by_call.get(call_id)
        {
            consumed_results.insert(rid);
            call_idx_for_result.insert(rid, i);
        }
    }

    let focus = buffer.effective_focus();

    // Pass 1: gather per-block heights. Cached entries return
    // immediately; misses build the block's lines once, measure with
    // `line_count`, and store the result. The cache survives across
    // frames so steady-state cost is O(blocks) for the lookup plus
    // O(visible) for actual line construction in pass 2.
    let mut heights: Vec<usize> = Vec::with_capacity(n);
    let mut total_rows = 0usize;
    for idx in 0..n {
        if consumed_results.contains(&idx) {
            heights.push(0);
            continue;
        }
        // Live (streaming) blocks change every frame, so caching is
        // pointless and a precise measure runs syntect/wrap over a
        // text that's about to be invalidated. Approximate them.
        // Stable blocks measure-and-cache once; subsequent frames
        // hit the cache. This is the contract scroll math depends
        // on - approximations and real measurements can't share the
        // same coordinate space, so anything that's emitted in pass
        // 2 needs an exact height here.
        let h = if let Some(cached) = buffer.cached_height(idx, width) {
            usize::from(cached)
        } else if buffer.is_live(idx) {
            approximate_block_height(buffer, idx, width)
        } else {
            let block_lines =
                build_block_lines(buffer, idx, width, &result_by_call, Emphasis::None);
            let measured = Paragraph::new(block_lines.clone())
                .wrap(Wrap { trim: false })
                .line_count(width);
            let stored = u16::try_from(measured).unwrap_or(u16::MAX);
            buffer.set_cached_height(idx, width, stored);
            buffer.set_cached_render_lines(idx, width, std::sync::Arc::new(block_lines));
            measured
        };
        heights.push(h);
        // +1 for the blank separator row that always follows a
        // non-consumed block.
        total_rows = total_rows.saturating_add(h).saturating_add(1);
    }
    // Drop the trailing separator that has no successor block.
    total_rows = total_rows.saturating_sub(1);

    let max_scroll_back = total_rows.saturating_sub(visible);

    if focus != buffer.last_drawn_focus()
        && let Some(focus_idx) = focus
    {
        let display_idx = if consumed_results.contains(&focus_idx) {
            call_idx_for_result.get(&focus_idx).copied()
        } else {
            Some(focus_idx)
        };
        if let Some(di) = display_idx {
            let mut rendered_start = 0usize;
            for (i, h) in heights.iter().enumerate().take(di) {
                if !consumed_results.contains(&i) {
                    rendered_start = rendered_start.saturating_add(*h).saturating_add(1);
                }
            }
            let rendered_height = heights[di];
            let rendered_end = rendered_start.saturating_add(rendered_height);
            let scroll_to_top = total_rows.saturating_sub(rendered_start + visible);
            let scroll_to_bottom = total_rows.saturating_sub(rendered_end);
            let current = buffer.scroll().min(max_scroll_back);
            let in_view = current >= scroll_to_top && current <= scroll_to_bottom;
            if !in_view {
                let target = if rendered_height > visible || current < scroll_to_top {
                    scroll_to_top
                } else {
                    scroll_to_bottom
                };
                buffer.set_scroll(target.min(max_scroll_back));
            }
        }
    }

    let scroll_back = buffer.scroll().min(max_scroll_back);
    if scroll_back != buffer.scroll() {
        buffer.set_scroll(scroll_back);
    }
    let visible_top = total_rows.saturating_sub(visible.saturating_add(scroll_back));
    let visible_bot = visible_top.saturating_add(visible);

    let mut emitted_lines: Vec<Line<'static>> = Vec::new();
    let mut acc = 0usize;
    let mut paragraph_scroll = 0u16;
    let mut emitted_rows = 0usize;
    let mut emitted_any = false;
    // Stop once we've covered the viewport plus a small margin for
    // wrap surprises. Skipping ahead saves the per-line clone cost
    // for huge unfolded blocks during fast scrolling.
    let row_budget = visible.saturating_add(32);
    // (block_idx, virtual_top, virtual_bottom) collected during this
    // pass; converted to absolute terminal rows below so mouse
    // handlers can translate a click row into a block.
    let mut block_layout: Vec<(usize, usize, usize)> = Vec::new();
    for (idx, h) in heights.iter().copied().enumerate() {
        if consumed_results.contains(&idx) {
            continue;
        }
        let block_top = acc;
        let block_bot = acc.saturating_add(h);
        let block_advance = h.saturating_add(1);

        if block_bot <= visible_top {
            acc = acc.saturating_add(block_advance);
            continue;
        }
        if block_top >= visible_bot {
            break;
        }
        if emitted_rows >= row_budget {
            break;
        }
        let intra_block_skip = if emitted_any {
            0
        } else {
            emitted_any = true;
            visible_top.saturating_sub(block_top)
        };
        let emp = emphasis_for(
            buffer,
            idx,
            focus,
            search_pattern,
            &consumed_results,
            &call_idx_for_result,
        );
        // Reuse the cached render only when there's no extra
        // emphasis to bake in - cached lines were built with
        // `Emphasis::None`, so a focused/match block has to rebuild
        // to pick up the rule glyph and accent color. The common
        // case (most blocks unfocused on screen) falls into the
        // cheap branch.
        let block_lines: Vec<Line<'static>> = if emp == Emphasis::None
            && let Some(cached) = buffer.cached_render_lines(idx, width)
        {
            cached.as_ref().clone()
        } else {
            let built = build_block_lines(buffer, idx, width, &result_by_call, emp);
            if emp == Emphasis::None {
                let measured = Paragraph::new(built.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(width);
                let stored = u16::try_from(measured).unwrap_or(u16::MAX);
                buffer.set_cached_height(idx, width, stored);
                buffer.set_cached_render_lines(idx, width, std::sync::Arc::new(built.clone()));
            }
            built
        };
        let take_rows = row_budget.saturating_sub(emitted_rows);
        let (sliced, slice_offset) =
            slice_lines_for_window(&block_lines, width, intra_block_skip, take_rows);
        // The first emitted block sets the paragraph-level scroll;
        // subsequent blocks always slice from row 0 so no further
        // adjustment is needed.
        if emitted_lines.is_empty() {
            paragraph_scroll = slice_offset;
        }
        let sliced_rows: usize = sliced
            .iter()
            .map(|l| wrap_rows(l, usize::from(width).max(1)))
            .sum();
        emitted_lines.extend(sliced);
        emitted_rows = emitted_rows.saturating_add(sliced_rows);
        emitted_lines.push(Line::raw(""));
        emitted_rows = emitted_rows.saturating_add(1);
        block_layout.push((idx, block_top, block_bot));
        acc = acc.saturating_add(block_advance);
    }

    if let Some(pattern) = search_pattern {
        highlight_matches_in_lines(&mut emitted_lines, pattern);
    }

    // Translate the virtual layout into absolute terminal rows, clamped
    // to the buffer area. Skip blocks fully outside the area (defensive;
    // pass 2 already filtered them, but the screen clamp is what
    // matters for mouse hit-testing).
    let area = regions.buffer;
    let area_y = area.y;
    let area_bottom = area.y.saturating_add(area.height);
    let screen_rows: Vec<(usize, u16, u16)> = block_layout
        .into_iter()
        .filter_map(|(idx, vtop, vbot)| {
            let virt_view_top = vtop.saturating_sub(visible_top);
            let virt_view_bot = vbot.saturating_sub(visible_top);
            let top = area_y.saturating_add(u16::try_from(virt_view_top).unwrap_or(u16::MAX));
            let bot = area_y.saturating_add(u16::try_from(virt_view_bot).unwrap_or(u16::MAX));
            let top = top.min(area_bottom);
            let bot = bot.min(area_bottom);
            (top < bot).then_some((idx, top, bot))
        })
        .collect();
    buffer.set_last_block_screen_rows(screen_rows);
    buffer.set_last_area_geometry(area.x, area.y, area.width, area.height, visible_top);

    let paragraph = Paragraph::new(emitted_lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph.scroll((paragraph_scroll, 0)), regions.buffer);
    buffer.set_last_drawn_focus(focus);
}

/// Compute the emphasis for the displayed block at `idx`. Merged
/// tool pairs pick `max` across both halves so a focused result
/// lights up the call's bubble too.
fn emphasis_for(
    buffer: &Buffer,
    idx: usize,
    focus: Option<usize>,
    search_pattern: Option<&str>,
    consumed_results: &std::collections::HashSet<usize>,
    call_idx_for_result: &std::collections::HashMap<usize, usize>,
) -> Emphasis {
    let single = |i: usize| -> Emphasis {
        if focus == Some(i) {
            Emphasis::Focused
        } else if search_pattern.is_some_and(|p| buffer.block_contains(i, p)) {
            Emphasis::Match
        } else {
            Emphasis::None
        }
    };
    let mut e = single(idx);
    // Walk the result side of any merged pair this idx represents,
    // picking up emphasis from the consumed half.
    for (rid, cid) in call_idx_for_result {
        if *cid == idx && consumed_results.contains(rid) {
            e = e.max(single(*rid));
        }
    }
    e
}

/// One captured cell from the rendered frame: the char that was
/// painted plus whether the renderer marked it as decoration. The
/// host's yank path filters out non-selectable cells using this
/// flag so clipboard text matches what the user logically owns.
#[derive(Clone, Copy, Debug)]
pub struct CapturedCell {
    /// First `char` of the cell's symbol. Multi-`char` graphemes
    /// collapse to their first; OK for clipboard-style yank where
    /// exact display width doesn't matter.
    pub ch: char,
    /// `true` if the renderer tagged this cell as chrome via
    /// [`DECORATION_MARKER`].
    pub decoration: bool,
}

/// Walk the buffer area's cell rows once: store each visible row's
/// captured cells into `captured_rows` keyed by virtual row, and
/// apply the selection bg overlay for rows in the selection range.
/// Skips cells the renderer flagged with [`DECORATION_MARKER`] so
/// the overlay doesn't bleed onto borders/padding and yank doesn't
/// pull chrome into the clipboard. Stays in virtual-row space so
/// selection survives subsequent scrolls.
fn capture_and_overlay(
    frame: &mut Frame,
    regions: Regions,
    buffer: &Buffer,
    selection: Option<((usize, u16), (usize, u16))>,
    captured_rows: &mut std::collections::BTreeMap<usize, Vec<CapturedCell>>,
) {
    let area = regions.buffer;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let virtual_top = buffer.last_virtual_top();
    let theme = crate::theme::current();
    let highlight_bg = theme.selection_color;
    let on_select_fg = Color::Black;
    let buf = frame.buffer_mut();
    let last_col = area.x.saturating_add(area.width).saturating_sub(1);
    let (start, end) = match selection {
        Some((a, c)) if a <= c => (Some(a), Some(c)),
        Some((a, c)) => (Some(c), Some(a)),
        None => (None, None),
    };
    for screen_row in area.y..area.y.saturating_add(area.height) {
        let vrow = virtual_top.saturating_add(usize::from(screen_row - area.y));
        let mut row_cells: Vec<CapturedCell> = Vec::with_capacity(usize::from(area.width));
        for col in area.x..area.x.saturating_add(area.width) {
            let cell = &mut buf[(col, screen_row)];
            let ch = cell.symbol().chars().next().unwrap_or(' ');
            let decoration = cell_is_decoration(cell.modifier);
            // The renderer hijacks `Modifier::SLOW_BLINK` as the
            // chrome-marker bit; capture the flag here, then strip
            // it so the terminal never paints an actual blink.
            // Most emulators ignore the attribute; some (kitty, a
            // few VTE forks, Windows Terminal in some modes) do not,
            // and the user reported a steady visible blink.
            cell.modifier.remove(DECORATION_MARKER);
            row_cells.push(CapturedCell { ch, decoration });
        }
        if let (Some(s), Some(e)) = (start, end)
            && vrow >= s.0
            && vrow <= e.0
        {
            // Cap each row's overlay at the last cell that's neither
            // chrome nor a trailing pad space: whitespace beyond the
            // last real char looks like a long trailing highlight
            // strip otherwise. `rposition` walks from the right, so
            // mid-line spaces (code indentation, prose between
            // words) still get painted - only the trailing run is
            // trimmed.
            let last_real_local = row_cells
                .iter()
                .rposition(|cell| !cell.decoration && cell.ch != ' ');
            if let Some(last_real_idx) = last_real_local {
                let last_real_col = area
                    .x
                    .saturating_add(u16::try_from(last_real_idx).unwrap_or(u16::MAX));
                let from_col = if vrow == s.0 { s.1 } else { area.x };
                let to_col = if vrow == e.0 { e.1 } else { last_col };
                let lo = from_col.max(area.x);
                let hi = to_col.min(last_col).min(last_real_col);
                if hi >= lo {
                    for col in lo..=hi {
                        let local_idx = usize::from(col - area.x);
                        if let Some(cell_meta) = row_cells.get(local_idx)
                            && cell_meta.decoration
                        {
                            continue;
                        }
                        let cell = &mut buf[(col, screen_row)];
                        cell.set_bg(highlight_bg);
                        cell.set_fg(on_select_fg);
                    }
                }
            }
        }
        captured_rows.insert(vrow, row_cells);
    }
}

/// Approximate wrapped-row count for one [`Line`] at `width`, used
/// by [`slice_lines_for_window`] to walk a cached vector of lines and
/// land on the slice that intersects the visible window. Counts
/// `char` instances rather than display width, so wide-char content
/// (CJK, emoji) under-counts; the rendered viewport just shows
/// slightly fewer rows than expected, no scroll-drift bug.
fn wrap_rows(line: &Line<'_>, width: usize) -> usize {
    let chars: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    if chars == 0 {
        1
    } else {
        chars.div_ceil(width).max(1)
    }
}

/// Walk `lines` accumulating wrap rows; produce the smallest
/// contiguous prefix that covers `[skip_rows, skip_rows + take_rows]`
/// and a Paragraph-level scroll offset to land the viewport on the
/// right row of the first emitted line. Used in pass 2 to avoid
/// cloning every line of a giant unfolded block when only the
/// viewport is actually visible.
fn slice_lines_for_window(
    lines: &[Line<'static>],
    width: u16,
    skip_rows: usize,
    take_rows: usize,
) -> (Vec<Line<'static>>, u16) {
    let usable = usize::from(width).max(1);
    let mut rows_seen = 0usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut paragraph_offset = 0u16;
    let mut started = false;
    for line in lines {
        let lh = wrap_rows(line, usable);
        if started {
            out.push(line.clone());
        } else if rows_seen.saturating_add(lh) > skip_rows {
            started = true;
            paragraph_offset =
                u16::try_from(skip_rows.saturating_sub(rows_seen)).unwrap_or(u16::MAX);
            out.push(line.clone());
        }
        rows_seen = rows_seen.saturating_add(lh);
        if rows_seen >= skip_rows.saturating_add(take_rows).saturating_add(16) {
            break;
        }
    }
    (out, paragraph_offset)
}

/// Cheap height estimate for a streaming block. Counts logical
/// newlines and divides each line's char count by the available
/// width. Off vs. the real wrap-aware count when content has long
/// lines that `WordWrapper` would break on word boundaries; that
/// inaccuracy only shifts auto-follow scroll math by a few rows on
/// an in-flight block, so it's an acceptable cost for skipping the
/// per-frame wrap pass on a block whose text changes 30 times a
/// second.
fn approximate_block_height(buffer: &Buffer, idx: usize, width: u16) -> usize {
    let blocks = buffer.blocks();
    let Some(block) = blocks.get(idx) else {
        return 0;
    };
    let usable = usize::from(width).max(1);
    let text: &str = match block {
        Block::User { text }
        | Block::Assistant { text, .. }
        | Block::Thinking { text, .. }
        | Block::Custom { text, .. } => text,
        Block::ToolCall { input_pretty, .. } => input_pretty,
        Block::ToolResult { output, .. } => output,
    };
    let mut rows = 0usize;
    for logical in text.split('\n') {
        let chars = logical.chars().count();
        rows = rows.saturating_add(chars.div_ceil(usable).max(1));
    }
    // Most block kinds add at least a header line beyond the body.
    rows.saturating_add(1)
}

/// Build the rendered lines for the block at `idx`, automatically
/// merging a `ToolCall` with its paired `ToolResult` when one exists.
/// Callers pass the `result_by_call` map (so lookups stay cheap inside
/// the render loop) and the emphasis state for this idx.
fn build_block_lines(
    buffer: &Buffer,
    idx: usize,
    width: u16,
    result_by_call: &std::collections::HashMap<String, usize>,
    emphasis: Emphasis,
) -> Vec<Line<'static>> {
    let blocks = buffer.blocks();
    let cur = &blocks[idx];
    if let Block::ToolCall { call_id, .. } = cur
        && let Some(&result_idx) = result_by_call.get(call_id)
    {
        return tool_pair_to_lines(cur, &blocks[result_idx], width, emphasis);
    }
    block_to_lines(cur, width, emphasis)
}

/// Width in cells of the leading prompt glyph plus its trailing
/// space. Painted on the first content row only; subsequent rows
/// (multi-line draft, soft-wrapped continuation) align under the
/// glyph slot but stay blank.
pub(crate) const INPUT_GLYPH_WIDTH: u16 = 2;

/// Single-character prompt glyph painted at the start of the input
/// content. Plain ASCII so it renders the same in every terminal and
/// doesn't trigger our "no fancy chars" lint when grep'd.
const INPUT_GLYPH: &str = ">";

/// Default placeholder text shown when the input is empty.
const INPUT_PLACEHOLDER_INSERT: &str = "ask kage anything...";
const INPUT_PLACEHOLDER_NORMAL: &str = "press i to insert  ::  : ex command  ::  / search";

fn render_input(frame: &mut Frame, regions: Regions, input: &InputState) {
    let area = regions.input;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = crate::theme::current();
    let mode = input.mode();
    let pane_focused = input.focused_pane() == Pane::Input;
    let border_color = if pane_focused {
        mode_border_color(&theme, mode)
    } else {
        // Buffer pane has focus: dim the input chrome so the eye
        // tracks the buffer block focus instead.
        theme.status_dim_fg
    };
    let mut pill_style = mode_pill_style(&theme, mode);
    if !pane_focused {
        pill_style = pill_style.add_modifier(Modifier::DIM);
    }

    let mut top_line: Vec<Span<'static>> =
        vec![Span::styled(format!(" {} ", mode_label(mode)), pill_style)];
    let hint = mode_hint_text(mode);
    if !hint.is_empty() {
        top_line.push(Span::raw(" "));
        let mut hint_style = Style::default().fg(theme.input_hint_fg);
        if !pane_focused {
            hint_style = hint_style.add_modifier(Modifier::DIM);
        }
        top_line.push(Span::styled(hint, hint_style));
    }

    let block = RtBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(top_line));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let glyph_width = INPUT_GLYPH_WIDTH.min(inner.width);
    let body_width = inner.width.saturating_sub(glyph_width);
    let body_area = ratatui::layout::Rect::new(
        inner.x.saturating_add(glyph_width),
        inner.y,
        body_width,
        inner.height,
    );
    let scroll_off = input_scroll_offset(input, body_area);

    let glyph_area = ratatui::layout::Rect::new(inner.x, inner.y, glyph_width, 1);
    if glyph_width >= INPUT_GLYPH_WIDTH {
        let glyph = Paragraph::new(Line::from(Span::styled(
            format!("{INPUT_GLYPH} "),
            Style::default().fg(theme.input_glyph_fg),
        )));
        frame.render_widget(glyph, glyph_area);
    }

    if input.text().is_empty() {
        if let Some(text) = placeholder_for(mode) {
            let placeholder = Paragraph::new(Line::from(Span::styled(
                text,
                Style::default()
                    .fg(theme.input_placeholder_fg)
                    .add_modifier(Modifier::ITALIC),
            )));
            frame.render_widget(placeholder, body_area);
        }
    } else if body_width > 0 {
        let visual_range = if mode == Mode::Visual {
            input.input_visual_range()
        } else {
            None
        };
        // Lines are pre-wrapped at body_width chars to match
        // input_visual_cursor exactly; no Paragraph::wrap needed.
        let lines = build_input_body_lines(input.text(), visual_range, &theme, body_width);
        let body = Paragraph::new(lines).scroll((scroll_off, 0));
        frame.render_widget(body, body_area);
    }

    // Cursor visibility: present in the input card whenever the
    // input pane has window focus AND the user is in a mode where
    // we want a hardware cursor on the input. That includes Normal
    // (vim cursor), Insert (editing), and an active input-pane
    // visual selection. Buffer-cell visual leaves the input cursor
    // hidden because the user's attention is on the buffer overlay.
    let input_visual_active = mode == Mode::Visual && input.input_visual_range().is_some();
    let show_cursor = input.focused_pane() == Pane::Input
        && (matches!(mode, Mode::Normal | Mode::Insert) || input_visual_active);
    if show_cursor && let Some(pos) = input_cursor_position(input, body_area, scroll_off) {
        frame.set_cursor_position(pos);
    }
}

/// Build the [`Line`]s for the input body using explicit char-based
/// hard-wrap at `body_width`. Pre-wrapping (rather than letting
/// `Paragraph::wrap` do it) keeps the visual layout perfectly in
/// sync with [`input_visual_cursor`]: both walk chars in fixed-width
/// chunks, so the cursor lands exactly under the char it indexes.
/// Word-aware wrap would put breaks at spaces, leaving the cursor
/// off-by-N relative to the painted text.
///
/// When `visual_range` is `Some`, the chunk for each visual row is
/// further split into pre-selection / selected / post-selection
/// spans so the highlight paints across wrap boundaries cleanly.
fn build_input_body_lines(
    text: &str,
    visual_range: Option<(usize, usize)>,
    theme: &crate::theme::Theme,
    body_width: u16,
) -> Vec<Line<'static>> {
    let highlight = Style::default().bg(theme.selection_color);
    let bw = usize::from(body_width.max(1));
    let mut out = Vec::new();
    let mut byte_offset = 0usize;
    for line in text.split('\n') {
        let line_start_abs = byte_offset;
        let line_bytes = line.len();
        // Walk chars in chunks of `bw`. Track the absolute byte
        // offset into `text` so the visual_range projection lines up.
        let mut chunk_start_abs = line_start_abs;
        let mut chunk_chars = 0usize;
        for (idx, _) in line.char_indices() {
            let abs = line_start_abs + idx;
            if chunk_chars == bw {
                push_input_row(
                    &mut out,
                    text,
                    chunk_start_abs,
                    abs,
                    visual_range,
                    highlight,
                );
                chunk_start_abs = abs;
                chunk_chars = 0;
            }
            chunk_chars += 1;
        }
        // Final chunk for this logical line, including the empty
        // trailing chunk so an empty logical line still produces one
        // visual row.
        push_input_row(
            &mut out,
            text,
            chunk_start_abs,
            line_start_abs + line_bytes,
            visual_range,
            highlight,
        );
        byte_offset = line_start_abs + line_bytes + 1;
    }
    out
}

/// Append one wrapped visual row spanning `text[start..end]` to
/// `out`, splitting into selection-aware spans when `visual_range`
/// overlaps the slice.
fn push_input_row(
    out: &mut Vec<Line<'static>>,
    text: &str,
    start: usize,
    end: usize,
    visual_range: Option<(usize, usize)>,
    highlight: Style,
) {
    let chunk = &text[start..end];
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some((vs, ve)) = visual_range {
        let sel_start = if vs <= start {
            0
        } else if vs >= end {
            chunk.len()
        } else {
            vs - start
        };
        let sel_end = if ve <= start {
            0
        } else if ve >= end {
            chunk.len()
        } else {
            ve - start
        };
        if sel_start > 0 {
            spans.push(Span::raw(chunk[..sel_start].to_owned()));
        }
        if sel_end > sel_start {
            spans.push(Span::styled(
                chunk[sel_start..sel_end].to_owned(),
                highlight,
            ));
        }
        if sel_end < chunk.len() {
            spans.push(Span::raw(chunk[sel_end..].to_owned()));
        }
    } else {
        spans.push(Span::raw(chunk.to_owned()));
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    out.push(Line::from(spans));
}

/// Paint the bottom modeline. When the host has registered a
/// [`SessionUsage`] handle, the row shows the active model, the
/// running token totals (input / output) and the context-window
/// fill. Otherwise the row is filled with the modeline background
/// so the chrome reads as a coherent strip rather than an unstyled
/// terminal row. Mode is intentionally absent here - the colored
/// pill on the input border is the canonical mode display.
fn render_modeline(frame: &mut Frame, regions: Regions, usage: Option<&SessionUsage>) {
    let area = regions.status_bottom;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = crate::theme::current();
    let bg = Style::default().bg(theme.modeline_bg);
    let fg = Style::default().fg(theme.modeline_fg).bg(theme.modeline_bg);
    let dim = fg.add_modifier(Modifier::DIM);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(u) = usage
        && (!u.model.is_empty() || u.total_tokens() > 0 || u.current_context > 0 || u.working)
    {
        spans.push(Span::styled(" ", bg));
        // Working spinner: a 10-frame braille ticker keyed off
        // wall-clock time so it animates without a frame counter
        // on the App. When idle, paint a single dim dot so the
        // strip width stays stable across transitions.
        if u.working {
            let frame = spinner_frame();
            spans.push(Span::styled(
                format!("{frame} "),
                fg.add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled("  ", bg));
        }
        if !u.model.is_empty() {
            spans.push(Span::styled(
                u.model.clone(),
                fg.add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled("  ::  ", dim));
        }
        if u.context_window > 0 {
            #[allow(clippy::cast_precision_loss)]
            let pct =
                (u.current_context as f64 / u.context_window as f64 * 100.0).clamp(0.0, 999.9);
            spans.push(Span::styled(
                format!(
                    "ctx {} / {} ({:.0}%)",
                    format_token_count(u.current_context),
                    format_token_count(u.context_window),
                    pct
                ),
                fg,
            ));
            spans.push(Span::styled("  ::  ", dim));
        } else if u.current_context > 0 {
            spans.push(Span::styled(
                format!("ctx {}", format_token_count(u.current_context)),
                fg,
            ));
            spans.push(Span::styled("  ::  ", dim));
        }
        // Cumulative session totals: what the user has been charged
        // for since the session started. Distinct from `ctx` above,
        // which is just the current turn's prompt fill against the
        // window.
        spans.push(Span::styled(
            format!(
                "{} in / {} out",
                format_token_count(u.input_tokens),
                format_token_count(u.output_tokens)
            ),
            fg,
        ));
    }
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = usize::from(area.width).saturating_sub(used);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), bg));
    }
    let line = Paragraph::new(Line::from(spans))
        .alignment(Alignment::Left)
        .style(bg);
    frame.render_widget(line, area);
}

/// Format a token count compactly so the modeline doesn't blow past
/// 80 columns: under 1k as raw digits, otherwise `<n>.<n>k` (no
/// `M` suffix - million-token windows still read fine as `1024k`).
fn format_token_count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    #[allow(clippy::cast_precision_loss)]
    let value = n as f64 / 1_000.0;
    if value >= 100.0 {
        format!("{value:.0}k")
    } else if value >= 10.0 {
        format!("{value:.1}k")
    } else {
        format!("{value:.2}k")
    }
}

/// Pick a braille spinner glyph keyed off wall-clock time so the
/// modeline ticks while the agent is working without us having to
/// thread a frame counter through `App::draw`. Cycle period ~= 1
/// second (10 frames at 100 ms each).
fn spinner_frame() -> &'static str {
    const FRAMES: &[&str] = &[
        "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}",
        "\u{2827}", "\u{2807}", "\u{280F}",
    ];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    #[allow(clippy::cast_possible_truncation)]
    let idx = ((now / 100) as usize) % FRAMES.len();
    FRAMES[idx]
}

fn mode_border_color(theme: &crate::theme::Theme, mode: Mode) -> Color {
    match mode {
        Mode::Normal => theme.input_border_normal,
        Mode::Insert => theme.input_border_insert,
        Mode::Visual => theme.input_border_visual,
    }
}

fn mode_pill_style(theme: &crate::theme::Theme, mode: Mode) -> Style {
    let (bg, fg) = match mode {
        Mode::Normal => (theme.input_pill_normal_bg, theme.input_pill_normal_fg),
        Mode::Insert => (theme.input_pill_insert_bg, theme.input_pill_insert_fg),
        Mode::Visual => (theme.input_pill_visual_bg, theme.input_pill_visual_fg),
    };
    Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD)
}

fn mode_hint_text(mode: Mode) -> &'static str {
    match mode {
        Mode::Insert => "Enter send  ::  Shift+Enter newline  ::  Esc normal",
        Mode::Normal => "i insert  ::  :ex  ::  /search  ::  j/k scroll",
        Mode::Visual => "y yank  ::  Esc cancel",
    }
}

fn placeholder_for(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Insert => Some(INPUT_PLACEHOLDER_INSERT),
        Mode::Normal => Some(INPUT_PLACEHOLDER_NORMAL),
        Mode::Visual => None,
    }
}

/// How many rows to scroll the input Paragraph so that the cursor row
/// always stays inside the visible content area. Once the prompt has
/// more rows than the input area can fit ([`INPUT_CONTENT_MAX_LINES`]
/// from `layout.rs`), scrolling is the only way to keep typing
/// visible.
/// Total visual rows the input text occupies inside `body_width`,
/// counting wrapped continuation rows. Empty logical lines still
/// count for one row each (so a trailing newline grows the input).
#[must_use]
pub fn input_visual_row_count(text: &str, body_width: u16) -> u16 {
    let bw = usize::from(body_width.max(1));
    let mut total: usize = 0;
    for line in text.split('\n') {
        let chars = line.chars().count();
        total += if chars == 0 { 1 } else { chars.div_ceil(bw) };
    }
    u16::try_from(total).unwrap_or(u16::MAX)
}

/// Visual `(row, col)` of the cursor in the wrapped layout. Walks
/// every prior logical line, accumulating wrapped row counts, then
/// adds the wrap-rows / column for the current logical line up to
/// the cursor.
fn input_visual_cursor(text: &str, cursor: usize, body_width: u16) -> (u16, u16) {
    let bw = usize::from(body_width.max(1));
    let prefix = text.get(..cursor).unwrap_or("");
    let mut row: usize = 0;
    let mut last_break = 0;
    for (i, _) in prefix.match_indices('\n') {
        let chars = prefix[last_break..i].chars().count();
        row += if chars == 0 { 1 } else { chars.div_ceil(bw) };
        last_break = i + 1;
    }
    let chars_in_current = prefix[last_break..].chars().count();
    row += chars_in_current / bw;
    let col = chars_in_current % bw;
    (
        u16::try_from(row).unwrap_or(u16::MAX),
        u16::try_from(col).unwrap_or(u16::MAX),
    )
}

/// How many rows to scroll the input Paragraph so the cursor's
/// visual row stays inside `body_area`. Wrap-aware: a long single
/// logical line that wraps to many visual rows scrolls correctly.
fn input_scroll_offset(input: &InputState, body_area: ratatui::layout::Rect) -> u16 {
    if body_area.height == 0 || body_area.width == 0 {
        return 0;
    }
    let (cursor_row, _) = input_visual_cursor(input.text(), input.cursor(), body_area.width);
    let max_visible_row = body_area.height.saturating_sub(1);
    cursor_row.saturating_sub(max_visible_row)
}

/// Compute the screen position of the prompt cursor inside the input
/// body area. Returns `None` if `body_area` is empty. Wrap-aware:
/// the visual `(row, col)` mirrors what `Paragraph::wrap` paints,
/// so a long line that wraps places the cursor on the right wrapped
/// row instead of clamping to the right edge of row 0.
fn input_cursor_position(
    input: &InputState,
    body_area: ratatui::layout::Rect,
    scroll_off: u16,
) -> Option<(u16, u16)> {
    if body_area.height == 0 || body_area.width == 0 {
        return None;
    }
    let max_x = body_area.x + body_area.width - 1;
    let max_y = body_area.y + body_area.height - 1;
    let (row, col) = input_visual_cursor(input.text(), input.cursor(), body_area.width);
    let row_offset = row.saturating_sub(scroll_off);
    let cx = body_area.x.saturating_add(col).min(max_x);
    let cy = body_area.y.saturating_add(row_offset).min(max_y);
    Some((cx, cy))
}

/// Convert one [`Block`] into its rendered [`Line`]s.
///
/// `width` is the rendering area's column count, used by blocks that
/// pad to a full-width visual block (`User` bubble). Other blocks
/// ignore it.
///
/// Folded blocks contribute one header line. Unfolded blocks contribute
/// the header plus the body. Assistant text has no header; it is the
/// content directly. Thinking text is rendered dimmed.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn block_to_lines(block: &Block, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
    match block {
        Block::User { text } => user_block_lines(text, width, emphasis),
        Block::Assistant { text, live } => {
            // Skip syntect while the block is still streaming. Each
            // delta changes the text, so the syntect cache would miss
            // on every frame and we'd re-highlight the whole growing
            // body 30 times a second. Once the stream finishes the
            // text is stable, the cache hits, and we syntect-highlight
            // for free.
            let lines = if *live {
                plain_lines(text, assistant_style())
            } else {
                crate::syntax::highlight_fenced(text, assistant_style())
            };
            mark_emphasis(lines, width, emphasis)
        }
        Block::Thinking { text, folded, .. } => {
            let mut out = Vec::new();
            out.push(header_line(
                fold_indicator(*folded),
                "thinking",
                None,
                thinking_style(),
            ));
            if !*folded {
                // Each body line gets a left-rule glyph in the
                // thinking fg color so the thinking section is
                // visibly distinct from assistant text even on
                // terminals that don't render italic. The glyph
                // itself is decoration so cell-based selection
                // skips it on yank.
                let rule = Span::styled(
                    "\u{258e} ",
                    Style::default()
                        .fg(crate::theme::current().thinking_fg)
                        .add_modifier(DECORATION_MARKER),
                );
                for body_line in plain_lines(text, thinking_style()) {
                    let mut spans = Vec::with_capacity(body_line.spans.len() + 1);
                    spans.push(rule.clone());
                    spans.extend(body_line.spans);
                    out.push(Line::from(spans));
                }
            }
            mark_emphasis(out, width, emphasis)
        }
        Block::ToolCall {
            name,
            input_summary,
            input_pretty,
            folded,
            ..
        } => {
            // No matching result yet: render as a pending bubble so
            // the user sees in-flight calls visually distinct from
            // completed ones.
            let dim = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM);
            let mut content: Vec<Line<'static>> = Vec::new();
            let style = tool_call_style();
            let mut header_spans = vec![
                Span::styled(
                    format!("{} ", fold_indicator(*folded)),
                    style.add_modifier(Modifier::BOLD),
                ),
                Span::styled(name.to_owned(), style.add_modifier(Modifier::BOLD)),
            ];
            if !input_summary.is_empty() {
                header_spans.push(Span::raw(" "));
                header_spans.push(Span::styled(input_summary.to_owned(), style));
            }
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled("running...".to_owned(), dim));
            content.push(Line::from(header_spans));
            if !*folded {
                content.push(Line::raw(""));
                for body_line in plain_lines(input_pretty, style) {
                    content.push(body_line);
                }
            }
            let theme = crate::theme::current();
            wrap_in_bubble_focused(
                content,
                theme.tool_rule,
                theme.tool_pending_bg,
                width,
                emphasis,
            )
        }
        Block::ToolResult {
            name,
            output,
            is_error,
            folded,
            ..
        } => {
            let mut out = Vec::new();
            out.push(tool_result_header_line(*folded, name, output, *is_error));
            if !*folded {
                let body_style = if *is_error {
                    tool_error_style()
                } else {
                    tool_result_style()
                };
                for body_line in truncated_body_lines(output, body_style) {
                    out.push(prefix_line("  ", body_line));
                }
            }
            mark_emphasis(out, width, emphasis)
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
            mark_emphasis(out, width, emphasis)
        }
    }
}

/// Render a user prompt as a tinted full-width "chat bubble" with a
/// thin themed left-edge rule and one row of padding above and below
/// the text.
fn user_block_lines(text: &str, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
    let theme = crate::theme::current();
    let mut content: Vec<Line<'static>> = Vec::new();
    for raw in text.split('\n') {
        content.push(Line::from(Span::styled(
            raw.to_owned(),
            Style::default()
                .fg(theme.focus_color)
                .add_modifier(Modifier::BOLD),
        )));
    }
    wrap_in_bubble_focused(content, theme.user_rule, theme.user_bg, width, emphasis)
}

/// Width in cells of the focus-rule chrome that PB.5 reserves on
/// every non-bubble block (assistant text, thinking, custom,
/// standalone tool result). One cell for the rule glyph or its
/// blank stand-in, one cell of padding before the body.
pub(super) const FOCUS_RULE_WIDTH: usize = 2;

/// Prepend a left-edge focus rule to every visual row of an
/// already-built non-bubble block's render.
///
/// PB.5 reserves the column unconditionally so toggling focus does
/// not shift the body horizontally; PB.6 additionally pre-wraps
/// each logical line to `width - FOCUS_RULE_WIDTH` chars so the
/// rule prefix lands on **every** visual row, including wrapped
/// continuations. Without the pre-wrap, ratatui's `Paragraph::wrap`
/// would only see one logical line with the prefix and fold the
/// rest of the text below the rule.
fn mark_emphasis(lines: Vec<Line<'static>>, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
    let prefix: Span<'static> = if emphasis == Emphasis::None {
        Span::styled(
            " ".repeat(FOCUS_RULE_WIDTH),
            Style::default().add_modifier(DECORATION_MARKER),
        )
    } else {
        let style = Style::default()
            .fg(emphasis.rule_color(Color::White))
            .add_modifier(Modifier::BOLD)
            .add_modifier(DECORATION_MARKER);
        Span::styled(format!("{} ", emphasis.rule_glyph()), style)
    };
    let body_width = usize::from(width).saturating_sub(FOCUS_RULE_WIDTH).max(1);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    for line in lines {
        for row_spans in split_line_into_rows(line, body_width) {
            let mut spans = Vec::with_capacity(row_spans.len() + 1);
            spans.push(prefix.clone());
            spans.extend(row_spans);
            out.push(Line::from(spans));
        }
    }
    out
}

/// Wrap a vector of content lines in a full-width "bubble": each row
/// starts with a colored left-edge rule, every cell is given the
/// background color, and a one-row pad sits above and below.
///
/// Each input line is truncated to fit on exactly one visual row; if
/// the content would have overflowed the buffer width and wrapped,
/// the wrap would break the bubble's visual cohesion (the wrapped
/// continuation has no leading rule and no trailing pad). Trade off:
/// the user can expand the block to read the full content.
///
/// Spans inside `content` are reused as-is except their background is
/// overridden with `bg` so the bubble reads as a uniform block.
fn wrap_in_bubble_focused(
    content: Vec<Line<'static>>,
    rule_color: Color,
    bg: Color,
    width: u16,
    emphasis: Emphasis,
) -> Vec<Line<'static>> {
    const RULE_WIDTH: usize = 1;
    const LEFT_PAD: usize = 1;
    const RIGHT_PAD: usize = 1;
    let total = usize::from(width);
    let interior = total
        .saturating_sub(RULE_WIDTH)
        .max(LEFT_PAD + RIGHT_PAD + 1);
    let max_content = interior.saturating_sub(LEFT_PAD + RIGHT_PAD);
    let rule_style = Style::default()
        .fg(emphasis.rule_color(rule_color))
        .bg(bg)
        .add_modifier(Modifier::BOLD)
        .add_modifier(DECORATION_MARKER);
    let rule_glyph = emphasis.rule_glyph();
    // Padding cells (top/bottom rows, leading space after rule,
    // trailing fill spaces) are pure chrome - tag them with the
    // decoration marker so cell-based selection skips them.
    let bg_only = Style::default().bg(bg).add_modifier(DECORATION_MARKER);
    let pad_row = || -> Line<'static> {
        Line::from(vec![
            Span::styled(rule_glyph.to_owned(), rule_style),
            Span::styled(" ".repeat(interior), bg_only),
        ])
    };

    let mut out: Vec<Line<'static>> = Vec::with_capacity(content.len() + 2);
    out.push(pad_row());
    for line in content {
        for visual_spans in split_line_into_rows(line, max_content) {
            let used_chars: usize = visual_spans.iter().map(|s| s.content.chars().count()).sum();
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(visual_spans.len() + 3);
            spans.push(Span::styled(rule_glyph.to_owned(), rule_style));
            spans.push(Span::styled(" ".repeat(LEFT_PAD), bg_only));
            for s in visual_spans {
                // Content spans keep their original modifiers - any
                // bg the bubble paints around them stays selectable
                // since it sits under user-visible text.
                spans.push(Span::styled(s.content, s.style.bg(bg)));
            }
            let used = LEFT_PAD + used_chars;
            if used < interior {
                spans.push(Span::styled(" ".repeat(interior - used), bg_only));
            }
            out.push(Line::from(spans));
        }
    }
    out.push(pad_row());
    out
}

/// Split one logical line into one or more visual rows, each holding
/// at most `max` characters across its spans. Style is preserved per
/// span; long spans are chunked. Empty input yields one empty row.
///
/// This is character-wise, not word-wise: it never breaks mid-word at
/// a fancy boundary, just at exactly `max` chars. Trade off: simple
/// math, OK for code/path content; English prose can mid-word break.
fn split_line_into_rows(line: Line<'static>, max: usize) -> Vec<Vec<Span<'static>>> {
    if max == 0 || line.spans.is_empty() {
        return vec![Vec::new()];
    }
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut row_used = 0usize;
    for span in line.spans {
        let style = span.style;
        let chars: Vec<char> = span.content.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if row_used >= max {
                rows.push(Vec::new());
                row_used = 0;
            }
            let avail = max - row_used;
            let take = avail.min(chars.len() - i);
            let piece: String = chars[i..i + take].iter().collect();
            rows.last_mut().unwrap().push(Span::styled(piece, style));
            i += take;
            row_used += take;
        }
    }
    rows
}

fn plain_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .map(|line| Line::from(Span::styled(line.to_owned(), style)))
        .collect()
}

/// Render a paired `ToolCall` + `ToolResult` as one composite block.
///
/// Layout (folded → just the header):
/// ```text
/// > read README.md                    <- header
///                                     <- blank
///   ... (12 earlier lines)            <- truncation hint
///   matching first visible line       <- body (tail-truncated)
///   matching second visible line
///                                     <- blank
///   Took 23ms · 1.2 KB                <- dim footer
/// ```
pub(super) fn tool_pair_to_lines(
    call: &Block,
    result: &Block,
    width: u16,
    emphasis: Emphasis,
) -> Vec<Line<'static>> {
    let (name, input_summary, input_pretty, folded) = match call {
        Block::ToolCall {
            name,
            input_summary,
            input_pretty,
            folded,
            ..
        } => (name, input_summary, input_pretty, *folded),
        _ => return Vec::new(),
    };
    let (output, is_error, duration_ms) = match result {
        Block::ToolResult {
            output,
            is_error,
            duration_ms,
            ..
        } => (output, *is_error, *duration_ms),
        _ => return Vec::new(),
    };

    let style = tool_call_style();
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let mut content: Vec<Line<'static>> = Vec::new();

    // Header: `<fold> <name> <summary>  <size>  Took <ms>` packs the
    // most-useful at-a-glance info on a single row regardless of fold
    // state. The body preview grows below it.
    let mut header_spans = vec![
        Span::styled(
            format!("{} ", fold_indicator(folded)),
            style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(name.to_owned(), style.add_modifier(Modifier::BOLD)),
    ];
    if !input_summary.is_empty() {
        header_spans.push(Span::raw(" "));
        header_spans.push(Span::styled(input_summary.to_owned(), style));
    }
    header_spans.push(Span::raw("  "));
    if is_error {
        header_spans.push(Span::styled(
            "ERROR".to_owned(),
            tool_error_style().add_modifier(Modifier::BOLD),
        ));
    } else {
        header_spans.push(Span::styled(human_size(output.len()), dim));
    }
    if let Some(footer) = duration_footer(duration_ms) {
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(footer, dim));
    }
    content.push(Line::from(header_spans));

    // Body: tail-truncated. Folded gets a small preview window;
    // unfolded shows much more. Unfolded with hundreds of huge tool
    // outputs hurts frame time so the cap is intentional in both.
    // Folded: tight preview cap. Unfolded: no cap. The user
    // intentionally expanded this block; honor the request. Huge
    // outputs may cost frame time but that's the trade they chose.
    let (cap_lines, cap_bytes) = if folded {
        (FOLDED_PREVIEW_LINES, FOLDED_PREVIEW_BYTES)
    } else {
        (usize::MAX, usize::MAX)
    };
    let body_style = if is_error {
        tool_error_style()
    } else {
        tool_result_style()
    };
    let body = truncated_body(
        output,
        body_style,
        cap_lines,
        cap_bytes,
        body_trim_for(name),
    );
    if !body.is_empty() {
        content.push(Line::raw(""));
        // For `read`/`view` results, syntect-highlight the body if we
        // can infer a syntax from the path's extension. Other tools
        // (find, grep, bash) keep the plain truncated body.
        let highlighted = highlight_read_body_if_applicable(name, input_summary, &body, body_style);
        for line in highlighted {
            content.push(line);
        }
    }
    if !folded && input_recap_worth_showing(name, input_summary, input_pretty) {
        content.push(Line::raw(""));
        content.push(Line::from(Span::styled("input:".to_owned(), dim)));
        for body_line in plain_lines(input_pretty, style) {
            content.push(body_line);
        }
    }
    let theme = crate::theme::current();
    let bg = if is_error {
        theme.tool_error_bg
    } else {
        theme.tool_bg
    };
    wrap_in_bubble_focused(content, theme.tool_rule, bg, width, emphasis)
}

/// Lines and bytes shown in a folded tool block's preview. Trades
/// completeness for screen real estate; the user expands with `zo` to
/// see more.
const FOLDED_PREVIEW_LINES: usize = 6;
/// Byte cap that complements [`FOLDED_PREVIEW_LINES`].
const FOLDED_PREVIEW_BYTES: usize = 2 * 1024;

/// Heuristic: should we show the pretty-printed input above the output
/// body? Skip it when the header summary already conveys the call (the
/// common case for `read README.md`, `find *.rs`, etc.) and only show
/// it when the user might genuinely want to inspect arguments
/// (multi-line bash, complex JSON inputs).
fn input_recap_worth_showing(name: &str, summary: &str, pretty: &str) -> bool {
    // Bash commands often span multiple lines via embedded newlines;
    // showing the full pretty version is useful there.
    if matches!(name, "bash" | "shell") && summary.contains('\n') {
        return true;
    }
    // For any other tool, skip the recap when the pretty form is just
    // the same JSON we already summarized to. The summary covers it.
    let pretty_compact = pretty.replace([' ', '\n'], "");
    pretty_compact.len() > 80 && !pretty_compact.contains(summary.replace(' ', "").as_str())
}

/// `Took 12ms` style timing string, or `None` when timing is unknown.
#[allow(clippy::cast_precision_loss)]
fn duration_footer(ms: Option<u64>) -> Option<String> {
    let ms = ms?;
    if ms < 1000 {
        Some(format!("Took {ms}ms"))
    } else {
        let secs = ms as f64 / 1000.0;
        Some(format!("Took {secs:.1}s"))
    }
}

/// Render `output` showing its **last** N lines (with a `... ({n}
/// earlier lines)` marker on top). Tools like `find`, `grep`, and
/// `bash` typically have the most relevant content near the tail; we
/// follow pi's convention of preserving that.
/// Whether a tool's body preview should keep the head (start) or
/// tail (end) of the output when truncated. Reading a file means the
/// top is most useful; running `find`/`grep`/`bash` means the most
/// recent / final lines carry the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyTrim {
    Head,
    Tail,
}

fn body_trim_for(tool: &str) -> BodyTrim {
    match tool {
        "read" | "view" => BodyTrim::Head,
        _ => BodyTrim::Tail,
    }
}

/// For `read`/`view` tool blocks, run the (already-truncated) body
/// through syntect using the syntax inferred from the file path's
/// extension. Other tools pass through unchanged.
///
/// Operates on already-rendered lines so it preserves the tail/head
/// truncation marker added by `truncated_body`. The marker line is
/// the only one whose first span style is the dim `DarkGray`; we
/// detect that and skip highlighting it.
fn highlight_read_body_if_applicable(
    tool_name: &str,
    input_summary: &str,
    body: &[Line<'static>],
    fallback: Style,
) -> Vec<Line<'static>> {
    if !matches!(tool_name, "read" | "view") {
        return body.to_vec();
    }
    let path = input_summary.split_whitespace().next().unwrap_or("");
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext.is_empty() {
        return body.to_vec();
    }
    let mut out: Vec<Line<'static>> = Vec::with_capacity(body.len());
    for line in body {
        let original_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        // Skip the truncation marker line (it starts with "...").
        if original_text.trim_start().starts_with("...") {
            out.push(line.clone());
            continue;
        }
        let highlighted = crate::syntax::highlight_extension(&original_text, ext, fallback);
        for hl in highlighted {
            out.push(hl);
        }
    }
    out
}

fn truncated_body(
    output: &str,
    style: Style,
    cap_lines: usize,
    cap_bytes: usize,
    trim: BodyTrim,
) -> Vec<Line<'static>> {
    if output.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = output.split('\n').collect();
    let total = lines.len();
    let mut bytes = 0usize;
    let take_iter: Box<dyn Iterator<Item = &&str>> = match trim {
        BodyTrim::Head => Box::new(lines.iter()),
        BodyTrim::Tail => Box::new(lines.iter().rev()),
    };
    let mut taken: Vec<&str> = Vec::new();
    for line in take_iter {
        if taken.len() >= cap_lines || bytes >= cap_bytes {
            break;
        }
        bytes += line.len() + 1;
        taken.push(*line);
    }
    if matches!(trim, BodyTrim::Tail) {
        taken.reverse();
    }
    let elided = total - taken.len();
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let elision = match trim {
        BodyTrim::Head => format!("... ({elided} more lines, zo to expand)"),
        BodyTrim::Tail => format!("... ({elided} earlier lines, zo to expand)"),
    };
    let mut out = Vec::new();
    if matches!(trim, BodyTrim::Tail) && elided > 0 {
        out.push(Line::from(Span::styled(elision.clone(), dim)));
    }
    for line in taken {
        out.push(Line::from(Span::styled(line.to_owned(), style)));
    }
    if matches!(trim, BodyTrim::Head) && elided > 0 {
        out.push(Line::from(Span::styled(elision, dim)));
    }
    out
}

/// Header for a tool-call block: `{indicator} {name} {summary}` with no
/// bracketed tag. The summary is bold so the tool name and the salient
/// argument both pop, but the surrounding line stays compact.
/// Header for a tool-result block. Folded results inline a size pill
/// (or `ERROR` glyph) and a one-line preview of the output so the user
/// sees the gist without expanding. Unfolded results keep just the
/// name + size and rely on the body for detail.
fn tool_result_header_line(
    folded: bool,
    name: &str,
    output: &str,
    is_error: bool,
) -> Line<'static> {
    let indicator = if folded { '<' } else { 'v' };
    let style = if is_error {
        tool_error_style()
    } else {
        tool_result_style()
    };
    let mut spans = vec![
        Span::styled(format!("{indicator} "), style.add_modifier(Modifier::BOLD)),
        Span::styled(name.to_owned(), style.add_modifier(Modifier::BOLD)),
    ];
    spans.push(Span::raw("  "));
    if is_error {
        spans.push(Span::styled(
            "ERROR".to_owned(),
            tool_error_style().add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(
            human_size(output.len()),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    if folded && let Some(preview) = first_line_preview(output, 60) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("· {preview}"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// Render a byte count as a short human-readable string. Used for the
/// `(1.2 KB)` style annotation in tool result headers.
#[must_use]
#[allow(clippy::cast_precision_loss)]
fn human_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

/// Cap on the rendered body of a tool result. Beyond either limit the
/// body is truncated and a one-line marker tells the user how many
/// rows were elided. Tool outputs from `find`, `grep`, or large file
/// reads otherwise dominate the screen and slow each frame down.
const MAX_BODY_LINES: usize = 200;
/// Byte cap that complements [`MAX_BODY_LINES`] for outputs with very
/// long lines (e.g., a single-line JSON dump).
const MAX_BODY_BYTES: usize = 16 * 1024;

/// Render `output` as a list of styled lines, capping at
/// [`MAX_BODY_LINES`] / [`MAX_BODY_BYTES`] and appending a
/// `... (N more lines)` marker when content was elided. The full text
/// stays in the buffer's `Block` so a future "expand fully" gesture
/// can show the rest without rerunning the tool.
fn truncated_body_lines(output: &str, style: Style) -> Vec<Line<'static>> {
    if output.is_empty() {
        return Vec::new();
    }
    let total_lines = output.split('\n').count();
    let mut bytes = 0usize;
    let mut shown = 0usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in output.split('\n') {
        if shown >= MAX_BODY_LINES || bytes >= MAX_BODY_BYTES {
            break;
        }
        bytes += line.len() + 1;
        out.push(Line::from(Span::styled(line.to_owned(), style)));
        shown += 1;
    }
    if shown < total_lines {
        let remaining = total_lines - shown;
        out.push(Line::from(Span::styled(
            format!("... ({remaining} more lines)"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )));
    }
    out
}

/// First non-empty line of `text`, trimmed and truncated to `max`
/// characters. Returns `None` when there is no non-empty content.
fn first_line_preview(text: &str, max: usize) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    let trimmed = line.trim();
    if trimmed.chars().count() <= max {
        return Some(trimmed.to_owned());
    }
    let cut: String = trimmed.chars().take(max.saturating_sub(3)).collect();
    Some(format!("{cut}..."))
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

fn assistant_style() -> Style {
    Style::default().fg(crate::theme::current().assistant_fg)
}

fn thinking_style() -> Style {
    Style::default()
        .fg(crate::theme::current().thinking_fg)
        .add_modifier(Modifier::DIM | Modifier::ITALIC)
}

fn tool_call_style() -> Style {
    Style::default().fg(crate::theme::current().tool_rule)
}

fn tool_result_style() -> Style {
    Style::default().fg(crate::theme::current().tool_result_fg)
}

fn tool_error_style() -> Style {
    Style::default()
        .fg(crate::theme::current().tool_error_fg)
        .add_modifier(Modifier::BOLD)
}

fn custom_style() -> Style {
    Style::default().fg(crate::theme::current().custom_fg)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    use super::*;
    use crate::buffer::Buffer;

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
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
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
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
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
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        input.paste("a\nb\nc");
        assert_eq!(super::input_scroll_offset(&input, body), 0);
    }

    #[test]
    fn input_card_shows_mode_pill() {
        // Mode display lives on the input card's top border now (not
        // on the top status bar). Frame is wide enough so the pill
        // fits inside the card border.
        let mut buffer = Buffer::new();
        let mut input = InputState::new();
        input.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('i'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        let lines = snapshot_lines(&mut buffer, &input, Rect::new(0, 0, 60, 8));
        assert!(
            lines.iter().any(|l| l.contains("INS")),
            "expected mode pill INS somewhere on screen, got: {lines:#?}"
        );
        // Top status bar no longer carries the mode pill; just kage label.
        assert!(!lines[0].contains("INS"));
    }
}
