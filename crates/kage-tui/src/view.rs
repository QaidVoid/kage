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

pub(crate) use ratatui::Frame;
pub(crate) use ratatui::layout::{Alignment, Rect};
pub(crate) use ratatui::style::{Color, Modifier, Style};
pub(crate) use ratatui::text::{Line, Span};
pub(crate) use ratatui::widgets::{Block as RtBlock, Borders, Paragraph, Wrap};
pub(crate) use unicode_width::UnicodeWidthChar;

pub(crate) use crate::buffer::{Block, Buffer};
pub(crate) use crate::cmdline::CommandLine;
pub(crate) use crate::input::{InputState, Mode, Pane};
pub(crate) use crate::layout::Regions;
pub(crate) use crate::usage::SessionUsage;

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

mod blocks;
mod bubble;
mod buffer;
mod cmdline;
mod input;
mod modeline;

// Render entry points the top-level `render` / `render_status` call.
use buffer::{capture_and_overlay, render_buffer};
use cmdline::{
    place_cmdline_cursor, place_search_cursor, render_cmdline_error, render_cmdline_popup,
};
use input::render_input;
use modeline::render_modeline;

// Helpers shared across the split submodules, re-routed through the
// parent so each submodule's `use super::*` keeps resolving them.
pub(crate) use blocks::mode_label;
pub(crate) use cmdline::highlight_matches_in_lines;
pub(crate) use input::{INPUT_PLACEHOLDER_INSERT, INPUT_PLACEHOLDER_NORMAL, wrap_input_rows};
pub(crate) use modeline::{
    input_cursor_position, input_scroll_offset, mode_border_color, mode_pill_style, placeholder_for,
};

// Internal helpers the test module exercises directly.
#[cfg(test)]
pub(crate) use blocks::{first_line_preview, human_size};
#[cfg(test)]
pub(crate) use bubble::split_line_into_rows;
#[cfg(test)]
pub(crate) use modeline::{format_token_count, input_visual_cursor};

// Re-exports so sibling block widgets keep resolving `super::*` helpers
// and the host (`app`) keeps its `view::*` entry points after the split.
pub(crate) use blocks::{
    assistant_style, custom_style, fold_indicator, header_line, prefix_line, thinking_style,
    tool_call_style, tool_error_style, tool_pair_to_lines, tool_result_header_line,
    tool_result_style, truncated_body_lines,
};
pub(crate) use bubble::{mark_emphasis, plain_lines, user_block_lines, wrap_in_bubble_focused};
pub use buffer::CapturedCell;
pub(crate) use input::INPUT_GLYPH_WIDTH;
pub use modeline::input_visual_row_count;
pub(crate) use modeline::{chrome_lines_to_ratatui, spinner_frame_index};

#[cfg(test)]
mod tests;
