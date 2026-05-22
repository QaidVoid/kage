//! Render the conversation buffer and input area into a ratatui [`Frame`].
//!
//! [`render`] is the single entry point. It walks the buffer's blocks,
//! turns each one into a styled [`Line`], lays them out in a scrollable
//! [`Paragraph`], and paints the status bar and input area on top.
//!
//! Block styling lives in the per-kind widget modules (`view::user`,
//! `view::assistant`, etc.); `render_buffer` dispatches via
//! [`registry::BlockRenderer`] and concatenates each widget's
//! [`widget::BlockWidget::lines`] into one Paragraph.

pub mod assistant;
pub mod compaction;
pub mod custom;
pub mod plugin_block;
pub mod registry;
pub mod thinking;
pub mod toast;
pub mod tool_call_alone;
pub mod tool_pair;
pub mod tool_result_alone;
pub mod user;
pub mod widget;

pub use assistant::AssistantBlockWidget;
pub use compaction::CompactionBlockWidget;
pub use custom::CustomBlockWidget;
pub use registry::{BlockFactory, BlockRenderer, BuiltinKind};
pub use thinking::ThinkingBlockWidget;
pub use toast::render_toasts;
pub use tool_call_alone::ToolCallAloneBlockWidget;
pub use tool_pair::ToolPairBlockWidget;
pub use tool_result_alone::ToolResultAloneBlockWidget;
pub use user::UserBlockWidget;
pub use widget::{BlockWidget, EmptyBlockWidget, RenderCtx, SelectionState};

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RtBlock, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthChar;

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
    /// Cached set of block indices matching `search_pattern`.
    /// Avoids O(text) substring scan per visible block per frame.
    pub search_match_set: Option<&'a std::collections::HashSet<usize>>,
    /// Open `/` search line, if the user is mid-typing one.
    pub search_line: Option<&'a CommandLine>,
    /// `(current_1_indexed, total)` for the active search. `current`
    /// is `0` when the focus isn't on any match. Painted as
    /// `match X/Y` on the right side of the status bar.
    pub search_match_count: Option<(usize, usize)>,
    /// Pre-rendered output of any plugin-registered status-bar widgets,
    /// in registration order. The host pre-renders each entry by
    /// calling `LuaWidget::render(width)`; non-empty texts are painted
    /// on the right edge before built-in pills.
    pub plugin_widgets: &'a [String],
    /// Transient `(key, text)` entries set by `kage.set_status`.
    /// Painted alongside widgets on the right edge in key-sorted
    /// order. Empty when no plugins push status.
    pub plugin_status: &'a [(String, String)],
    /// Pre-rendered styled lines from a plugin `kage.ui.set_header`
    /// renderer. When non-empty the host paints these in place of the
    /// built-in status bar; the `:` command line and `/` search line
    /// still take priority.
    pub plugin_header: &'a [kage_plugin::ChromeLine],
    /// Pre-rendered styled lines from a plugin `kage.ui.set_footer`
    /// renderer. When non-empty they replace the built-in modeline.
    pub plugin_footer: &'a [kage_plugin::ChromeLine],
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
    toasts: &[crate::toast::Toast],
) {
    // Opaque base for the entire frame: header, conversation, input,
    // modeline, every gap and overlay paint over this, so nothing
    // bleeds the terminal background through as a patchwork. A theme
    // that opts into `transparent` skips this so the terminal
    // background (wallpaper, blur) shows through the whole UI.
    let theme = crate::theme::current();
    if !theme.transparent {
        let full = frame.area();
        frame.render_widget(
            RtBlock::default().style(Style::default().bg(theme.bg)),
            full,
        );
    }
    render_status(frame, regions, input, cmdline, status);
    render_buffer(
        frame,
        regions,
        buffer,
        status.search_pattern,
        status.search_match_set,
    );
    render_input(frame, regions, input);
    render_modeline(frame, regions, session_usage, status.plugin_footer);
    if !toasts.is_empty() {
        let theme = crate::theme::current();
        render_toasts(frame, regions.buffer, toasts, &theme);
    }
    if let Some(cl) = cmdline {
        render_cmdline_error(frame, regions, cl);
        render_cmdline_popup(frame, regions, cl);
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

    if !status.plugin_header.is_empty() {
        let base = Style::default().fg(theme.status_dim_fg).bg(theme.status_bg);
        let lines = chrome_lines_to_ratatui(status.plugin_header, base);
        let paragraph = Paragraph::new(lines).alignment(Alignment::Left).style(base);
        frame.render_widget(paragraph, regions.status);
        return;
    }

    let bg_style = Style::default().bg(theme.status_bg);
    // The bar blends into the canvas now (no band), so `DIM` grey on
    // dark would be unreadable. Use the readable muted tier instead.
    let muted = Style::default().fg(theme.muted_fg).bg(theme.status_bg);
    // Quiet brand label: a recessive marker, not a headline. The
    // model rides right next to it so the bar reads "kage <model>"
    // as one tight unit instead of a spaced-out toolbar.
    let mut left_spans = vec![Span::styled(" kage".to_owned(), muted)];
    if let Some(model) = status.model
        && !model.is_empty()
    {
        left_spans.push(Span::styled(" ".to_owned(), bg_style));
        left_spans.push(Span::styled(model.to_owned(), muted));
    }
    let mut right_spans: Vec<Span<'static>> = Vec::new();
    for text in status.plugin_widgets {
        if text.is_empty() {
            continue;
        }
        right_spans.push(Span::styled(format!("{text}  "), muted));
    }
    for (_key, text) in status.plugin_status {
        if text.is_empty() {
            continue;
        }
        right_spans.push(Span::styled(format!("{text}  "), muted));
    }
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
        right_spans.push(Span::styled(format!("#{sid} "), muted));
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
        // Alloc-free pre-check: most on-screen lines during a search
        // contain no match, so leave them completely untouched rather
        // than take + rebuild + reallocate their span vec every frame.
        if !line
            .spans
            .iter()
            .any(|s| ascii_ifind(&s.content, needle, 0).is_some())
        {
            continue;
        }
        let original = std::mem::take(&mut line.spans);
        let mut rebuilt: Vec<Span<'static>> = Vec::with_capacity(original.len() + 2);
        for span in original {
            if ascii_ifind(&span.content, needle, 0).is_some() {
                rebuilt.extend(split_span_for_match(span, needle));
            } else {
                // No match in this span: move it through untouched
                // (no per-span `vec![span]` allocation).
                rebuilt.push(span);
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

/// Maximum number of completion rows painted in the popup. Anything
/// beyond this is summarized as "+ N more" on the last row.
const POPUP_MAX_VISIBLE: usize = 8;

/// Reuses the slash palette's blue accent so the popup visually
/// reads as the same surface; selected rows use white-on-blue.
/// Background is [`Theme::modeline_bg`] (dark navy) so the popup is
/// distinct from the status row's [`Theme::status_bg`], rather than
/// merging into a single dark-gray block.
fn popup_styles() -> (Style, Style, Style) {
    let theme = crate::theme::current();
    let bg = theme.modeline_bg;
    let row = Style::default().fg(Color::White).bg(bg);
    let sel = Style::default()
        .fg(Color::White)
        .bg(Color::Blue)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.status_dim_fg).bg(bg);
    (row, sel, dim)
}

/// Paint an inline validation error below the cmdline status row.
/// Shown when the host set [`CommandLine::set_error`] after a failed
/// submit attempt. The error is rendered in the tool-error foreground
/// colour so it is visually distinct from the completion popup. The
/// popup is suppressed while an error is visible so the user focuses
/// on fixing the input.
fn render_cmdline_error(frame: &mut Frame, regions: Regions, cmdline: &CommandLine) {
    let Some(err) = cmdline.error() else {
        return;
    };
    let theme = crate::theme::current();
    let bg = theme.modeline_bg;
    let fg = theme.tool_error_fg;
    let style = Style::default().fg(fg).bg(bg);

    let y = regions.status.y.saturating_add(1);
    if y >= regions.buffer.y.saturating_add(regions.buffer.height) {
        return;
    }
    let width = regions.status.width.max(regions.buffer.width);
    let area = Rect {
        x: regions.status.x,
        y,
        width,
        height: 1,
    };

    let marker = "! ";
    let marker_chars = marker.len();
    let inner = usize::from(area.width).saturating_sub(marker_chars);
    let text = truncate_to_width(err, inner);
    let total_chars = marker_chars + text.chars().count();
    let pad = usize::from(area.width).saturating_sub(total_chars);
    let line = Line::from(vec![
        Span::styled(marker.to_owned(), style.add_modifier(Modifier::BOLD)),
        Span::styled(format!("{text}{}", " ".repeat(pad)), style),
    ]);
    frame.render_widget(crate::opaque::OpaqueClear, area);
    frame.render_widget(Paragraph::new(line), area);
}

/// Paint the completion popup over the conversation buffer when the
/// cmdline has candidate completions. Each row shows the value plus an
/// optional dimmed description; the [`CommandLine::selected`] row is
/// highlighted. When there are more items than fit, a sliding window
/// follows the selection and `... N more above` / `... N more below`
/// indicator rows show how many candidates are off-screen. Suppressed
/// when an inline error is active.
fn render_cmdline_popup(frame: &mut Frame, regions: Regions, cmdline: &CommandLine) {
    // Suppress the popup when an error is shown so the user can
    // focus on fixing the input.
    if cmdline.error().is_some() {
        return;
    }
    let completions = cmdline.completions();
    if completions.items.is_empty() {
        return;
    }
    let total = completions.items.len();
    let max_visible = POPUP_MAX_VISIBLE.min(total);
    let (offset, window) = popup_scroll_window(cmdline.selected(), total, max_visible);
    let above = offset;
    let below = total.saturating_sub(offset + window);
    let rows_above = usize::from(above > 0);
    let rows_below = usize::from(below > 0);
    let total_rows = window + rows_above + rows_below;

    let buf_h = usize::from(regions.buffer.height);
    let total_rows = total_rows.min(buf_h);
    if total_rows == 0 {
        return;
    }
    let height = u16::try_from(total_rows).unwrap_or(u16::MAX);

    let width = popup_width(regions, completions);
    if width == 0 {
        return;
    }
    let anchor_x = regions.status.x.saturating_add(1);
    let area = Rect {
        x: anchor_x,
        y: regions.status.y.saturating_add(1),
        width,
        height,
    };
    if area.y >= regions.buffer.y.saturating_add(regions.buffer.height) {
        return;
    }

    let (row_style, sel_style, dim_style) = popup_styles();
    let max_value_chars = completions
        .items
        .iter()
        .skip(offset)
        .take(window)
        .map(|c| c.value.chars().count())
        .max()
        .unwrap_or(0);
    let inner_width = usize::from(area.width);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(total_rows);

    if rows_above > 0 {
        lines.push(Line::from(Span::styled(
            pad_to_width(&format!("  ... {above} more above"), inner_width),
            dim_style,
        )));
    }

    for (i, item) in completions
        .items
        .iter()
        .enumerate()
        .skip(offset)
        .take(window)
    {
        let selected = cmdline.selected() == Some(i);
        let value_style = if selected { sel_style } else { row_style };
        let desc_style = if selected { sel_style } else { dim_style };
        lines.push(popup_row(
            item.value.as_str(),
            item.description.as_deref(),
            max_value_chars,
            inner_width,
            value_style,
            desc_style,
        ));
    }

    if rows_below > 0 {
        lines.push(Line::from(Span::styled(
            pad_to_width(&format!("  ... {below} more below"), inner_width),
            dim_style,
        )));
    }

    frame.render_widget(crate::opaque::OpaqueClear, area);
    frame.render_widget(Paragraph::new(lines).style(row_style), area);
}

/// Compute the visible-items window for the completion popup so the
/// selected row stays in view as the user cycles past the bottom or
/// scrolls back above the top. Returns `(offset, window_len)` where
/// `offset` is the index of the first item to render and `window_len`
/// is how many to render (clamped to `max_visible`).
fn popup_scroll_window(
    selected: Option<usize>,
    total: usize,
    max_visible: usize,
) -> (usize, usize) {
    if total <= max_visible {
        return (0, total);
    }
    let sel = selected.unwrap_or(0);
    let offset = if sel < max_visible {
        0
    } else {
        (sel + 1)
            .saturating_sub(max_visible)
            .min(total - max_visible)
    };
    (offset, max_visible)
}

fn popup_width(regions: Regions, completions: &crate::cmdparse::Completions) -> u16 {
    let max_value = completions
        .items
        .iter()
        .map(|c| c.value.chars().count())
        .max()
        .unwrap_or(0);
    let max_desc = completions
        .items
        .iter()
        .filter_map(|c| c.description.as_deref().map(|d| d.chars().count()))
        .max()
        .unwrap_or(0);
    let separator = if max_desc > 0 { 2 } else { 0 };
    let leading = 2;
    let mut desired = leading + max_value + separator + max_desc;
    // When the popup will scroll, reserve enough room to paint the
    // "... N more above/below" indicator. ~22 cells fits up to four-
    // digit counts without truncation.
    if completions.items.len() > POPUP_MAX_VISIBLE {
        desired = desired.max(22);
    }
    let viewport = usize::from(regions.buffer.width.max(regions.status.width));
    let cap = viewport.saturating_sub(2).min(80);
    let width = desired.min(cap).max(max_value + leading);
    u16::try_from(width.min(viewport)).unwrap_or(u16::MAX)
}

fn popup_row(
    value: &str,
    description: Option<&str>,
    value_col_chars: usize,
    inner_width: usize,
    value_style: Style,
    desc_style: Style,
) -> Line<'static> {
    let leading = "  ";
    let leading_chars = leading.chars().count();
    let value_chars = value.chars().count();
    let value_pad = value_col_chars.saturating_sub(value_chars);
    let after_value = leading_chars + value_chars + value_pad;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    spans.push(Span::styled(leading.to_owned(), value_style));
    spans.push(Span::styled(value.to_owned(), value_style));
    if value_pad > 0 {
        spans.push(Span::styled(" ".repeat(value_pad), value_style));
    }
    if let Some(desc) = description {
        let remaining = inner_width.saturating_sub(after_value).saturating_sub(2);
        if remaining > 0 {
            let truncated = truncate_to_width(desc, remaining);
            spans.push(Span::styled("  ".to_owned(), desc_style));
            spans.push(Span::styled(truncated, desc_style));
        }
    }
    let painted: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if painted < inner_width {
        spans.push(Span::styled(" ".repeat(inner_width - painted), value_style));
    }
    Line::from(spans)
}

fn truncate_to_width(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = chars[..max_chars.saturating_sub(1)].iter().collect();
    out.push('\u{2026}');
    out
}

fn pad_to_width(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_owned();
    }
    let mut out = s.to_owned();
    out.push_str(&" ".repeat(width - n));
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

/// Width in cells of the leading prompt glyph plus its trailing
/// space. Painted on the first content row only; subsequent rows
/// (multi-line draft, soft-wrapped continuation) align under the
/// glyph slot but stay blank.
pub(crate) const INPUT_GLYPH_WIDTH: u16 = 1;

/// Single-character prompt glyph painted at the start of the input
/// content. Plain ASCII so it renders the same in every terminal and
/// doesn't trigger our "no fancy chars" lint when grep'd.
const INPUT_GLYPH: &str = "|";

/// Default placeholder text shown when the input is empty.
const INPUT_PLACEHOLDER_INSERT: &str = "Send a message...";
const INPUT_PLACEHOLDER_NORMAL: &str = "press i to type, Esc to scroll";

fn render_input(frame: &mut Frame, regions: Regions, input: &InputState) {
    let area = regions.input;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = crate::theme::current();
    let mode = input.mode();
    let pane_focused = input.focused_pane() == Pane::Input;
    // Buffer pane focused: recede the input chrome to the muted tier
    // so the eye tracks the focused buffer block, but stay visible
    // (the bars no longer sit on a band, so `DIM` would vanish).
    let border_color = if pane_focused {
        mode_border_color(&theme, mode)
    } else {
        theme.muted_fg
    };
    let pill_style = if pane_focused {
        mode_pill_style(&theme, mode)
    } else {
        Style::default().fg(theme.muted_fg)
    };

    // Attachments show inline in the prompt as editable
    // `[image #N ...]` markers, so the top border stays just the
    // mode pill.
    let top_line: Vec<Span<'static>> =
        vec![Span::styled(format!(" {} ", mode_label(mode)), pill_style)];

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
            INPUT_GLYPH.to_string(),
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
        // Visual mode paints the selection; otherwise, when the
        // cursor is parked at the tail of an `[image #N ...]` chip,
        // paint that whole chip so the user sees the block one
        // Backspace will delete as a unit.
        let (range, highlight) = if mode == Mode::Visual {
            (
                input.input_visual_range(),
                Style::default().bg(theme.selection_color),
            )
        } else if let Some(r) = input.armed_image_range() {
            (
                Some(r),
                Style::default()
                    .bg(theme.selection_color)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (None, Style::default())
        };
        // Lines are pre-wrapped at body_width chars to match
        // input_visual_cursor exactly; no Paragraph::wrap needed.
        let lines = build_input_body_lines(input.text(), range, highlight, body_width);
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

/// Build the [`Line`]s for the input body using word-aware wrap at
/// `body_width`. Pre-wrapping (rather than letting `Paragraph::wrap`
/// do it) keeps the visual layout perfectly in sync with
/// [`input_visual_cursor`]: both consume the same row plan from
/// [`wrap_input_rows`], so the cursor lands exactly under the char it
/// indexes regardless of where the wrap broke.
///
/// When `highlight_range` is `Some`, each row range is further split
/// into pre / highlighted / post spans (styled with `highlight`) so
/// the band paints across wrap boundaries cleanly. Used for the
/// Visual selection and for the armed image-chip block.
fn build_input_body_lines(
    text: &str,
    highlight_range: Option<(usize, usize)>,
    highlight: Style,
    body_width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (start, end) in wrap_input_rows(text, body_width) {
        push_input_row(&mut out, text, start, end, highlight_range, highlight);
    }
    out
}

/// Word-aware wrap plan for the input area.
///
/// Returns one `(byte_start, byte_end)` per visual row. The ranges
/// project directly into the source `text` so callers that need a
/// cursor's `(row, col)` (or selection spans) can index the same
/// rows that get painted.
///
/// Wrap rules:
/// - Logical lines (split on `\n`) are wrapped independently. An
///   empty logical line still produces one zero-length row so a
///   trailing newline grows the input.
/// - Within a logical line, a row is filled greedily by characters.
///   When the next character would overflow `body_width`, the row is
///   cut at the most recent ASCII space (the space is consumed and
///   not painted on either side); if no break point exists in the
///   row, the cut is mid-character.
/// - A "word" longer than `body_width` is split at `body_width`
///   character boundaries until it fits.
fn wrap_input_rows(text: &str, body_width: u16) -> Vec<(usize, usize)> {
    let bw = usize::from(body_width.max(1));
    let mut rows = Vec::new();
    let mut byte_offset = 0usize;
    for line in text.split('\n') {
        let line_start = byte_offset;
        let line_bytes = line.len();
        wrap_one_logical_line(line, line_start, bw, &mut rows);
        byte_offset = line_start + line_bytes + 1;
    }
    rows
}

fn wrap_one_logical_line(
    line: &str,
    line_start_abs: usize,
    bw: usize,
    rows: &mut Vec<(usize, usize)>,
) {
    let line_end_abs = line_start_abs + line.len();
    if line.is_empty() {
        rows.push((line_start_abs, line_end_abs));
        return;
    }
    let mut row_start_abs = line_start_abs;
    let mut row_chars = 0usize;
    let mut last_space_abs: Option<usize> = None;
    let mut byte_pos = line_start_abs;
    for c in line.chars() {
        let c_len = c.len_utf8();
        if row_chars >= bw {
            if let Some(sb) = last_space_abs.filter(|&s| s > row_start_abs) {
                rows.push((row_start_abs, sb));
                row_start_abs = sb + 1;
            } else {
                rows.push((row_start_abs, byte_pos));
                row_start_abs = byte_pos;
            }
            row_chars = line[(row_start_abs - line_start_abs)..(byte_pos - line_start_abs)]
                .chars()
                .count();
            last_space_abs = None;
        }
        if c == ' ' {
            last_space_abs = Some(byte_pos);
        }
        byte_pos += c_len;
        row_chars += 1;
    }
    rows.push((row_start_abs, line_end_abs));
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
/// Map plugin-supplied [`kage_plugin::ChromeLine`]s onto ratatui
/// lines. `base` carries the row's default fg/bg; a span's `fg` / `bg`
/// overrides it when the string parses, and the attribute bits map to
/// terminal modifiers. An unparseable color is dropped so the span
/// inherits `base` rather than failing the whole row.
pub(crate) fn chrome_lines_to_ratatui(
    lines: &[kage_plugin::ChromeLine],
    base: Style,
) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|cl| {
            let spans: Vec<Span<'static>> = cl
                .spans
                .iter()
                .map(|sp| {
                    let mut style = base;
                    if let Some(c) = sp.fg.as_deref().and_then(parse_chrome_color) {
                        style = style.fg(c);
                    }
                    if let Some(c) = sp.bg.as_deref().and_then(parse_chrome_color) {
                        style = style.bg(c);
                    }
                    let a = sp.attrs;
                    if a.bold() {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if a.dim() {
                        style = style.add_modifier(Modifier::DIM);
                    }
                    if a.italic() {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if a.underline() {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    Span::styled(sp.text.clone(), style)
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

/// Resolve a plugin color string via ratatui's color grammar (named
/// colors such as `red`, `#rrggbb` hex, or an indexed number).
/// Unparseable input yields `None`.
fn parse_chrome_color(name: &str) -> Option<Color> {
    name.parse::<Color>().ok()
}

fn render_modeline(
    frame: &mut Frame,
    regions: Regions,
    usage: Option<&SessionUsage>,
    plugin_footer: &[kage_plugin::ChromeLine],
) {
    let area = regions.status_bottom;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = crate::theme::current();
    let bg = Style::default().bg(theme.modeline_bg);
    let fg = Style::default().fg(theme.modeline_fg).bg(theme.modeline_bg);
    if !plugin_footer.is_empty() {
        let lines = chrome_lines_to_ratatui(plugin_footer, fg);
        let paragraph = Paragraph::new(lines).alignment(Alignment::Left).style(bg);
        frame.render_widget(paragraph, area);
        return;
    }
    // Blended into the canvas (no band): a `DIM` separator would
    // vanish, so use the readable muted tier.
    let dim = Style::default().fg(theme.muted_fg).bg(theme.modeline_bg);
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
        // Logical groups separated by a muted dot: model, context
        // fill, cumulative io (+ cost), thinking level. Each is
        // labelled so a field reads on its own; the dot only ever
        // appears between groups, never trailing.
        let mut prior_group = false;
        let sep = |spans: &mut Vec<Span<'static>>, prior: &mut bool| {
            if *prior {
                spans.push(Span::styled(" . ", dim));
            }
            *prior = true;
        };
        if !u.model.is_empty() {
            sep(&mut spans, &mut prior_group);
            spans.push(Span::styled(
                u.model.clone(),
                fg.add_modifier(Modifier::BOLD),
            ));
        }
        if u.context_window > 0 {
            sep(&mut spans, &mut prior_group);
            #[allow(clippy::cast_precision_loss)]
            let pct =
                (u.current_context as f64 / u.context_window as f64 * 100.0).clamp(0.0, 999.9);
            spans.push(Span::styled(
                format!(
                    "ctx {}/{} ({:.0}%)",
                    format_token_count(u.current_context),
                    format_token_count(u.context_window),
                    pct
                ),
                fg,
            ));
        } else if u.current_context > 0 {
            sep(&mut spans, &mut prior_group);
            spans.push(Span::styled(
                format!("ctx {}", format_token_count(u.current_context)),
                fg,
            ));
        }
        // Cumulative session totals (what the user has been charged
        // for since the session started), distinct from `ctx` above.
        // Cost rides in the same group as the io it paid for.
        sep(&mut spans, &mut prior_group);
        spans.push(Span::styled(
            format!(
                "io {}+{}",
                format_token_count(u.input_tokens),
                format_token_count(u.output_tokens)
            ),
            fg,
        ));
        if u.total_cost > 0.0 {
            spans.push(Span::styled(format!(" ${:.4}", u.total_cost), fg));
        }
        if let Some(level) = u.thinking_level
            && !level.is_off()
        {
            sep(&mut spans, &mut prior_group);
            spans.push(Span::styled(
                format!("think:{}", level.label()),
                fg.add_modifier(Modifier::BOLD),
            ));
        }
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

/// Format a token count compactly so the modeline stays narrow:
/// under 1k as raw digits, then `k` / `M` / `B` with adaptive
/// precision and trailing zeros trimmed (`21M`, not `21000k` or
/// `21.0M`; `78.7k`; `1.16k`; `1.5M`).
fn format_token_count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    #[allow(clippy::cast_precision_loss)]
    let (value, suffix) = if n < 1_000_000 {
        (n as f64 / 1_000.0, 'k')
    } else if n < 1_000_000_000 {
        (n as f64 / 1_000_000.0, 'M')
    } else {
        (n as f64 / 1_000_000_000.0, 'B')
    };
    let decimals = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    let mut s = format!("{value:.decimals$}");
    if s.contains('.') {
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        s.truncate(trimmed.len());
    }
    s.push(suffix);
    s
}

/// Pick a braille spinner glyph keyed off wall-clock time so the
/// modeline ticks while the agent is working without us having to
/// thread a frame counter through `App::draw`. Cycle period ~= 1
/// second (10 frames at 100 ms each).
const SPINNER_FRAMES: &[&str] = &[
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280F}",
];

/// Index into the spinner frame table for the current wall-clock
/// instant. The frame advances on a 100ms cadence. The event loop
/// reads this to repaint only when the glyph actually moves instead of
/// once per wake, so a static buffer during a long tool call does not
/// cost a full redraw every poll interval.
pub(crate) fn spinner_frame_index() -> usize {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    #[allow(clippy::cast_possible_truncation)]
    {
        ((now / 100) as usize) % SPINNER_FRAMES.len()
    }
}

fn spinner_frame() -> &'static str {
    SPINNER_FRAMES[spinner_frame_index()]
}

fn mode_border_color(theme: &crate::theme::Theme, mode: Mode) -> Color {
    match mode {
        Mode::Normal => theme.input_border_normal,
        Mode::Insert => theme.input_border_insert,
        Mode::Visual => theme.input_border_visual,
    }
}

fn mode_pill_style(theme: &crate::theme::Theme, mode: Mode) -> Style {
    let fg = match mode {
        Mode::Normal => theme.input_pill_normal_fg,
        Mode::Insert => theme.input_pill_insert_fg,
        Mode::Visual => theme.input_pill_visual_fg,
    };
    Style::default().fg(fg).add_modifier(Modifier::BOLD)
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
    u16::try_from(wrap_input_rows(text, body_width).len()).unwrap_or(u16::MAX)
}

/// Visual `(row, col)` of the cursor in the wrapped layout. Walks
/// the same wrap plan [`build_input_body_lines`] paints so the
/// cursor lands on the row and column that match what's on screen,
/// regardless of whether the row break was a soft (word) or hard
/// (mid-character) cut.
fn input_visual_cursor(text: &str, cursor: usize, body_width: u16) -> (u16, u16) {
    let rows = wrap_input_rows(text, body_width);
    if rows.is_empty() {
        return (0, 0);
    }
    let cursor = cursor.min(text.len());
    for (idx, (start, end)) in rows.iter().enumerate() {
        if cursor <= *end {
            let row_text = text.get(*start..cursor).unwrap_or("");
            let col = row_text.chars().count();
            return (
                u16::try_from(idx).unwrap_or(u16::MAX),
                u16::try_from(col).unwrap_or(u16::MAX),
            );
        }
    }
    let (last_start, last_end) = rows[rows.len() - 1];
    let last_chars = text[last_start..last_end].chars().count();
    (
        u16::try_from(rows.len() - 1).unwrap_or(u16::MAX),
        u16::try_from(last_chars).unwrap_or(u16::MAX),
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

/// Render a user prompt as a tinted full-width "chat bubble" with a
/// thin themed left-edge rule and one row of padding above and below
/// the text.
pub(super) fn user_block_lines(text: &str, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
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
    wrap_in_bubble_focused(content, theme.user_rule, theme.user_bg, width, emphasis, None)
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
/// each logical line to `width - FOCUS_RULE_WIDTH` display columns
/// so the rule prefix lands on **every** visual row, including
/// wrapped continuations. Without the pre-wrap, ratatui's
/// `Paragraph::wrap` would only see one logical line with the
/// prefix and fold the rest of the text below the rule. The
/// pre-wrap must measure in the same display-width metric ratatui
/// uses, or a row of wide glyphs overflows and ratatui re-folds it
/// onto a prefix-less continuation.
pub(super) fn mark_emphasis(
    lines: Vec<Line<'static>>,
    width: u16,
    emphasis: Emphasis,
    persistent_rule: Option<Color>,
) -> Vec<Line<'static>> {
    let prefix: Span<'static> = if emphasis == Emphasis::None {
        match persistent_rule {
            // A recessive always-on spine so the turn is anchored
            // even when it is not the focus/search target.
            Some(c) => Span::styled(
                format!("{} ", emphasis.rule_glyph()),
                Style::default().fg(c).add_modifier(DECORATION_MARKER),
            ),
            None => Span::styled(
                " ".repeat(FOCUS_RULE_WIDTH),
                Style::default().add_modifier(DECORATION_MARKER),
            ),
        }
    } else {
        let style = Style::default()
            .fg(emphasis.rule_color(Color::White))
            .add_modifier(Modifier::BOLD)
            .add_modifier(DECORATION_MARKER);
        Span::styled(format!("{} ", emphasis.rule_glyph()), style)
    };
    let body_width = usize::from(width).saturating_sub(FOCUS_RULE_WIDTH).max(1);
    let mut out: Vec<Line<'static>> =
        Vec::with_capacity(lines.len() + widget::BlockPadding::BOTTOM);
    for line in lines {
        for row_spans in split_line_into_rows(line, body_width) {
            let mut spans = Vec::with_capacity(row_spans.len() + 1);
            spans.push(prefix.clone());
            spans.extend(row_spans);
            out.push(Line::from(spans));
        }
    }
    // PB.7: trailing pad row(s) so non-bubble blocks have the same
    // visual separation bubbles already get from their bottom pad.
    // Carries the gutter so the rule reads as continuous.
    for _ in 0..widget::BlockPadding::BOTTOM {
        out.push(Line::from(vec![prefix.clone()]));
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
pub(super) fn wrap_in_bubble_focused(
    content: Vec<Line<'static>>,
    rule_color: Color,
    bg: Color,
    width: u16,
    emphasis: Emphasis,
    content_window: Option<(usize, usize)>,
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
    let bg_only = Style::default().bg(bg).add_modifier(DECORATION_MARKER);
    let pad_row = || -> Line<'static> {
        Line::from(vec![
            Span::styled(rule_glyph.to_owned(), rule_style),
            Span::styled(" ".repeat(interior), bg_only),
        ])
    };
    let make_row = |visual_spans: Vec<Span<'static>>| -> Line<'static> {
        let used_chars: usize = visual_spans.iter().map(|s| s.content.chars().count()).sum();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(visual_spans.len() + 3);
        spans.push(Span::styled(rule_glyph.to_owned(), rule_style));
        spans.push(Span::styled(" ".repeat(LEFT_PAD), bg_only));
        for s in visual_spans {
            spans.push(Span::styled(s.content, s.style.bg(bg)));
        }
        let used = LEFT_PAD + used_chars;
        if used < interior {
            spans.push(Span::styled(" ".repeat(interior - used), bg_only));
        }
        Line::from(spans)
    };

    if let Some((skip_rows, take_rows)) = content_window {
        let mut out: Vec<Line<'static>> = Vec::with_capacity(take_rows + 2);
        out.push(pad_row());
        let mut rows_produced = 0usize;
        let mut rows_skipped = 0usize;
        let target = skip_rows.saturating_add(take_rows);
        for line in content {
            if rows_produced >= take_rows {
                break;
            }
            for visual_spans in split_line_into_rows(line, max_content) {
                if rows_skipped < skip_rows {
                    rows_skipped += 1;
                    continue;
                }
                out.push(make_row(visual_spans));
                rows_produced += 1;
                if rows_produced >= take_rows {
                    break;
                }
            }
            if rows_skipped + rows_produced >= target {
                break;
            }
        }
        out.push(pad_row());
        out
    } else {
        let mut out: Vec<Line<'static>> = Vec::with_capacity(content.len() + 2);
        out.push(pad_row());
        for line in content {
            for visual_spans in split_line_into_rows(line, max_content) {
                out.push(make_row(visual_spans));
            }
        }
        out.push(pad_row());
        out
    }
}

/// Split one logical line into one or more visual rows, each holding
/// at most `max` characters across its spans. Style is preserved per
/// span; long spans are chunked. Empty input yields one empty row.
///
/// This is character-wise, not word-wise: it never breaks mid-word at
/// a fancy boundary, just at exactly `max` chars. Trade off: simple
/// math, OK for code/path content; English prose can mid-word break.
/// Word-aware row split for a styled line.
///
/// Walks the line's spans as a flat `(char, style)` stream, packs as
/// many chars as fit into `max` columns, and breaks at the most recent
/// ASCII space when the next char would overflow. The space is
/// consumed (not painted on either row) so the result reads cleanly
/// across the wrap. Words longer than `max` fall back to a
/// mid-character break.
///
/// Style boundaries are preserved: each output row is rebuilt as a
/// minimal sequence of `Span`s, coalescing consecutive chars that
/// share a style.
fn split_line_into_rows(line: Line<'static>, max: usize) -> Vec<Vec<Span<'static>>> {
    if max == 0 || line.spans.is_empty() {
        return vec![Vec::new()];
    }
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in line.spans {
        let style = span.style;
        for c in span.content.chars() {
            chars.push((c, style));
        }
    }
    if chars.is_empty() {
        return vec![Vec::new()];
    }

    // Accumulate display width, the unicode-width metric ratatui's
    // `Paragraph` wrap uses. Counting `char`s instead lets a row of
    // wide glyphs (CJK, emoji) overflow `max` cells; the outer
    // `Paragraph::wrap` then folds the overflow onto a continuation
    // row that never received the gutter prefix, so the left rule
    // appears to skip wrapped text.
    let cw = |c: char| UnicodeWidthChar::width(c).unwrap_or(0);
    let row_width = |chars: &[(char, Style)]| -> usize { chars.iter().map(|&(c, _)| cw(c)).sum() };

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut row_start = 0usize;
    let mut row_used = 0usize;
    let mut last_space: Option<usize> = None;
    let mut i = 0;
    while i < chars.len() {
        if row_used >= max {
            if let Some(sp) = last_space.filter(|&s| s > row_start) {
                ranges.push((row_start, sp));
                row_start = sp + 1;
                row_used = row_width(&chars[row_start..i]);
                last_space = None;
                continue;
            }
            ranges.push((row_start, i));
            row_start = i;
            row_used = 0;
            last_space = None;
        }
        if chars[i].0 == ' ' {
            last_space = Some(i);
        }
        row_used += cw(chars[i].0);
        i += 1;
    }
    ranges.push((row_start, chars.len()));

    let mut rows: Vec<Vec<Span<'static>>> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        rows.push(spans_for_range(&chars[start..end]));
    }
    rows
}

fn spans_for_range(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut current_style: Option<Style> = None;
    let mut current_content = String::new();
    for &(c, st) in chars {
        if Some(st) != current_style {
            if !current_content.is_empty() {
                out.push(Span::styled(
                    std::mem::take(&mut current_content),
                    current_style.unwrap_or_default(),
                ));
            }
            current_style = Some(st);
        }
        current_content.push(c);
    }
    if !current_content.is_empty() {
        out.push(Span::styled(
            current_content,
            current_style.unwrap_or_default(),
        ));
    }
    out
}

pub(super) fn plain_lines(text: &str, style: Style) -> Vec<Line<'static>> {
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
    row_budget: Option<usize>,
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
    let dim = Style::default().fg(crate::theme::current().muted_fg);
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
    let (cap_lines, cap_bytes) = if folded {
        (FOLDED_PREVIEW_LINES, FOLDED_PREVIEW_BYTES)
    } else {
        (UNFOLDED_MAX_LINES, UNFOLDED_MAX_BYTES)
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
        let highlight_limit = row_budget.map(|b| b.saturating_sub(3));
        let highlighted = highlight_read_body_if_applicable(
            name,
            input_summary,
            &body,
            body_style,
            highlight_limit,
        );
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
    let rule = if is_error {
        theme.tool_error_rule
    } else {
        theme.tool_rule
    };
    wrap_in_bubble_focused(content, rule, bg, width, emphasis, None)
}

/// Lines and bytes shown in a folded tool block's preview. Trades
/// completeness for screen real estate; the user expands with `zo` to
/// see more.
const FOLDED_PREVIEW_LINES: usize = 6;
/// Byte cap that complements [`FOLDED_PREVIEW_LINES`].
const FOLDED_PREVIEW_BYTES: usize = 2 * 1024;
/// Max body lines shown for an unfolded tool block. Bounds the
/// worst-case line construction cost without affecting typical
/// outputs (most are well under this). The height estimator uses the
/// same cap so scroll geometry stays consistent.
const UNFOLDED_MAX_LINES: usize = 500;
/// Byte cap for unfolded tool output body.
const UNFOLDED_MAX_BYTES: usize = 256 * 1024;

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
    highlight_limit: Option<usize>,
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
    let mut highlighted_count = 0usize;
    for line in body {
        let original_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if original_text.trim_start().starts_with("...") {
            out.push(line.clone());
            continue;
        }
        let over_budget = highlight_limit.is_some_and(|limit| highlighted_count >= limit);
        if over_budget {
            out.push(line.clone());
        } else {
            let highlighted = crate::syntax::highlight_extension(&original_text, ext, fallback);
            highlighted_count += highlighted.len();
            for hl in highlighted {
                out.push(hl);
            }
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
    let dim = Style::default().fg(crate::theme::current().muted_fg);
    let elision = match trim {
        BodyTrim::Head => format!("... ({elided} more lines)"),
        BodyTrim::Tail => format!("... ({elided} earlier lines)"),
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
pub(super) fn tool_result_header_line(
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
            Style::default().fg(crate::theme::current().muted_fg),
        ));
    }
    if folded && let Some(preview) = first_line_preview(output, 60) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("· {preview}"),
            Style::default().fg(crate::theme::current().muted_fg),
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
    const GB: usize = MB * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / GB as f64)
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
pub(super) fn truncated_body_lines(output: &str, style: Style) -> Vec<Line<'static>> {
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

pub(super) fn header_line(
    indicator: char,
    tag: &str,
    detail: Option<String>,
    style: Style,
) -> Line<'static> {
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

pub(super) fn prefix_line(prefix: &str, line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(prefix.to_owned()));
    spans.extend(line.spans);
    Line::from(spans)
}

pub(super) fn fold_indicator(folded: bool) -> char {
    if folded { '>' } else { 'v' }
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "-",
        Mode::Insert => "*",
        Mode::Visual => "%",
    }
}

pub(super) fn assistant_style() -> Style {
    Style::default().fg(crate::theme::current().assistant_fg)
}

pub(super) fn thinking_style() -> Style {
    Style::default()
        .fg(crate::theme::current().thinking_fg)
        .add_modifier(Modifier::DIM | Modifier::ITALIC)
}

pub(super) fn tool_call_style() -> Style {
    Style::default().fg(crate::theme::current().tool_rule)
}

pub(super) fn tool_result_style() -> Style {
    Style::default().fg(crate::theme::current().tool_result_fg)
}

pub(super) fn tool_error_style() -> Style {
    Style::default()
        .fg(crate::theme::current().tool_error_fg)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn custom_style() -> Style {
    Style::default().fg(crate::theme::current().custom_fg)
}

#[cfg(test)]
mod tests {
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
}
