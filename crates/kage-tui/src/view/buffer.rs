//! Conversation buffer rendering and cell capture.

#[allow(clippy::wildcard_imports)] // free-fn split: shares the parent view module scope
use super::*;

#[allow(clippy::too_many_lines)]
pub(super) fn render_buffer(
    frame: &mut Frame,
    regions: Regions,
    buffer: &mut Buffer,
    search_pattern: Option<&str>,
    search_match_set: Option<&std::collections::HashSet<usize>>,
) {
    let width = regions.buffer.width;
    let visible = usize::from(regions.buffer.height);
    if width == 0 || visible == 0 {
        return;
    }

    // The opaque base canvas is painted once for the whole frame in
    // `render`; blocks tint their own rows on top of it.

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

    let registry = registry::global()
        .read()
        .expect("block registry rwlock poisoned");
    let focus = buffer.effective_focus();

    let mut heights: Vec<usize> = Vec::with_capacity(n);
    let mut total_rows = 0usize;
    for idx in 0..n {
        if consumed_results.contains(&idx) {
            heights.push(0);
            continue;
        }
        let h = if let Some(cached) = buffer.cached_height(idx, width) {
            usize::from(cached)
        } else {
            let block_lines = build_block_lines(
                buffer,
                idx,
                width,
                &result_by_call,
                Emphasis::None,
                &registry,
                Some(0),
            );
            let measured = block_lines.len();
            let stored = u16::try_from(measured).unwrap_or(u16::MAX);
            buffer.set_cached_height(idx, width, stored);
            measured
        };
        heights.push(h);
        total_rows = total_rows.saturating_add(h).saturating_add(1);
    }
    total_rows = total_rows.saturating_sub(1);

    let max_scroll_back = total_rows.saturating_sub(visible);

    if focus != buffer.last_drawn_focus() {
        if let Some(old) = buffer.last_drawn_focus() {
            buffer.invalidate_height(old);
        }
        if let Some(new) = focus {
            buffer.invalidate_height(new);
        }
        if let Some(focus_idx) = focus {
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
            idx,
            focus,
            search_match_set,
            &consumed_results,
            &call_idx_for_result,
        );
        let cached_owner;
        let built_owner;
        let take_rows = row_budget.saturating_sub(emitted_rows);
        let block_lines: &[Line<'static>] =
            if let Some(cached) = buffer.cached_render_lines(idx, width) {
                cached_owner = cached;
                cached_owner.as_slice()
            } else {
                let budget = Some(intra_block_skip.saturating_add(take_rows));
                let built =
                    build_block_lines(buffer, idx, width, &result_by_call, emp, &registry, budget);
                let measured = built.len();
                let stored = u16::try_from(measured).unwrap_or(u16::MAX);
                buffer.set_cached_height(idx, width, stored);
                buffer.set_cached_render_lines(idx, width, std::sync::Arc::new(built.clone()));
                built_owner = built;
                built_owner.as_slice()
            };
        let (sliced, slice_offset) =
            slice_lines_for_window(block_lines, width, intra_block_skip, take_rows);
        // The first emitted block sets the paragraph-level scroll;
        // subsequent blocks always slice from row 0 so no further
        // adjustment is needed.
        if emitted_lines.is_empty() {
            paragraph_scroll = slice_offset;
        }
        let sliced_rows = sliced.len();
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
        .iter()
        .filter_map(|&(idx, vtop, vbot)| {
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
    // Unclamped virtual spans (same coordinate space as mouse
    // selection rows) so yank can map a selected row to its source
    // line even when the block is scrolled past its own top/bottom.
    buffer.set_last_block_virtual_rows(block_layout);
    buffer.set_last_area_geometry(area.x, area.y, area.width, area.height, visible_top);

    let paragraph = Paragraph::new(emitted_lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph.scroll((paragraph_scroll, 0)), regions.buffer);
    buffer.set_last_drawn_focus(focus);
}

/// Compute the emphasis for the displayed block at `idx`. Merged
/// tool pairs pick `max` across both halves so a focused result
/// lights up the call's bubble too.
fn emphasis_for(
    idx: usize,
    focus: Option<usize>,
    search_match_set: Option<&std::collections::HashSet<usize>>,
    consumed_results: &std::collections::HashSet<usize>,
    call_idx_for_result: &std::collections::HashMap<usize, usize>,
) -> Emphasis {
    let single = |i: usize| -> Emphasis {
        if focus == Some(i) {
            Emphasis::Focused
        } else if search_match_set.is_some_and(|s| s.contains(&i)) {
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
pub(crate) fn capture_and_overlay(
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
    let buf = frame.buffer_mut();
    let (start, end) = match selection {
        Some((a, c)) if a <= c => (Some(a), Some(c)),
        Some((a, c)) => (Some(c), Some(a)),
        None => (None, None),
    };

    let has_selection = start.is_some() && end.is_some();
    if has_selection {
        let theme = crate::theme::current();
        let highlight_bg = theme.selection_color;
        let on_select_fg = Color::Black;
        let last_col = area.x.saturating_add(area.width).saturating_sub(1);
        let (s, e) = (start.unwrap(), end.unwrap());
        for screen_row in area.y..area.y.saturating_add(area.height) {
            let vrow = virtual_top.saturating_add(usize::from(screen_row - area.y));
            let mut row_cells: Vec<CapturedCell> = Vec::with_capacity(usize::from(area.width));
            for col in area.x..area.x.saturating_add(area.width) {
                let cell = &mut buf[(col, screen_row)];
                let ch = cell.symbol().chars().next().unwrap_or(' ');
                let decoration = cell_is_decoration(cell.modifier);
                cell.modifier.remove(DECORATION_MARKER);
                row_cells.push(CapturedCell { ch, decoration });
            }
            if vrow >= s.0 && vrow <= e.0 {
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
    } else {
        for screen_row in area.y..area.y.saturating_add(area.height) {
            for col in area.x..area.x.saturating_add(area.width) {
                let cell = &mut buf[(col, screen_row)];
                cell.modifier.remove(DECORATION_MARKER);
            }
        }
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

/// Build the rendered lines for the block at `idx`, automatically
/// merging a `ToolCall` with its paired `ToolResult` when one exists.
/// Callers pass the `result_by_call` map (so lookups stay cheap inside
/// the render loop) and the emphasis state for this idx.
///
/// PB.9 routes this through the [`registry::BlockRenderer`] so block
/// rendering goes through the same widget dispatch plugins hook into
/// via `set_builtin` / `set_custom`.
/// Compute block height from raw text without building styled lines.
/// Returns `None` for block types that need the full render path.
/// The estimate uses char-based wrapping, consistent with
/// `wrap_rows`.
fn build_block_lines(
    buffer: &Buffer,
    idx: usize,
    width: u16,
    result_by_call: &std::collections::HashMap<String, usize>,
    emphasis: Emphasis,
    registry: &registry::BlockRenderer,
    row_budget: Option<usize>,
) -> Vec<Line<'static>> {
    let blocks = buffer.blocks();
    let cur = &blocks[idx];
    let theme = crate::theme::current();
    let ctx = widget::RenderCtx {
        theme: &theme,
        focused: emphasis == Emphasis::Focused,
        emphasis,
        selection: None,
        search_pattern: None,
        row_budget,
    };
    if let Block::ToolCall { call_id, .. } = cur
        && let Some(&result_idx) = result_by_call.get(call_id)
        && let Some(w) = registry.pair_widget_for(cur, &blocks[result_idx])
    {
        return w.lines(width, &ctx);
    }
    if let Some(w) = registry.widget_for(cur) {
        return w.lines(width, &ctx);
    }
    Vec::new()
}
