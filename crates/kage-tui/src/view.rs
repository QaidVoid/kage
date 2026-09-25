//! Render the conversation buffer and input area into a ratatui [`Frame`].
//!
//! [`render`] is the single entry point. It walks the buffer's blocks,
//! turns each one into a styled [`Line`], lays them out in a scrollable
//! [`Paragraph`], and paints the chrome slots and the input around it.
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
pub mod tool_view;
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
pub(crate) use ratatui::widgets::{Block as RtBlock, Paragraph, Wrap};
pub(crate) use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) use crate::buffer::{Block, Buffer};
pub(crate) use crate::cmdline::CommandLine;
pub(crate) use crate::input::{InputState, Mode, Pane};
pub(crate) use crate::layout::Regions;
pub(crate) use crate::usage::SessionUsage;

/// Read-only snapshot of the live state the chrome needs to paint.
/// Built fresh each frame from whatever the host has wired in.
#[derive(Default)]
pub struct StatusCtx<'a> {
    /// Friendly label of the active model (the model picker's label,
    /// else the `provider:model` id), if known.
    pub model: Option<&'a str>,
    /// Short session id pill, if recording is active.
    pub session_id: Option<&'a str>,
    /// Active session title, for the `title` component.
    pub title: Option<&'a str>,
    /// Currently submitted search pattern, if any. Blocks whose
    /// content contains this pattern get a `Match` emphasis.
    pub search_pattern: Option<&'a str>,
    /// Cached block indices matching `search_pattern`, in buffer
    /// order. Avoids O(text) substring scan per visible block per
    /// frame.
    pub search_match_set: Option<&'a [usize]>,
    /// Open `/` search line, if the user is mid-typing one. Painted
    /// over the footer row.
    pub search_line: Option<&'a CommandLine>,
    /// `(current_1_indexed, total)` for the active search. `current`
    /// is `0` when the focus isn't on any match. Painted as
    /// `match X/Y` by the `search` component and on the search line.
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
    /// Slot specs for the header, activity row, input pill, footer and
    /// start screen. The default paints kage's built-in chrome.
    pub slots: kage_plugin::SlotSpecs,
    /// Footer hint for the `hint` component: the pending keys, or what
    /// the next key does.
    pub hint: Option<&'a str>,
    /// Working row text for the `activity` component, present while a
    /// run is in flight.
    pub activity: Option<&'a str>,
    /// Working directory, for the `cwd` component.
    pub cwd: Option<&'a str>,
    /// Id of the active model, shown next to its label on the start
    /// card.
    pub model_id: Option<&'a str>,
    /// What the start card lists, when the host provided it.
    pub start: Option<&'a StartInfo>,
    /// Keys for the start card's change hints.
    pub start_keys: StartKeys,
    /// Prompts sent during the run that were not delivered yet, listed
    /// above the input.
    pub pending: &'a [PendingPrompt],
    /// Live agents under the session on screen, pinned above the
    /// pending prompts.
    pub agents: &'a [AgentRow],
    /// Key that opens the agents overlay, named in the pinned list's
    /// `+N more` row.
    pub agents_key: Option<&'a str>,
    /// The agent on screen, for the `breadcrumb` component. `None` in
    /// the main view, where the `title` component and the start card
    /// paint instead.
    pub breadcrumb: Option<&'a Breadcrumb>,
    /// Placeholder of the empty draft in insert mode and the modeless
    /// editor, in place of the default one.
    pub placeholder: Option<&'a str>,
}

/// The agent on screen, as the `breadcrumb` component shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Breadcrumb {
    /// Agent names from the main session's own agent down to the one on
    /// screen.
    pub trail: Vec<String>,
    /// The task description the model wrote.
    pub description: String,
    /// Where the agent is: `queued`, `running`, `waiting`, `done`,
    /// `failed` or `stopped`.
    pub state: &'static str,
    /// How long the current run has taken, or the last run took.
    pub elapsed_ms: Option<u64>,
    /// Input and output tokens the agent used.
    pub tokens: u64,
    /// Tool calls the agent made.
    pub tool_calls: u32,
}

/// Start card data the chrome state does not carry. Set once by the
/// host, with the sessions listed again when the session changes.
#[derive(Clone, Debug, Default)]
pub struct StartInfo {
    /// Recent sessions in the working directory, newest first, as the
    /// session lister reports them.
    pub sessions: Vec<crate::picker::PickItem>,
    /// Startup notices: credential problems, an unavailable default
    /// model, the version update.
    pub notices: Vec<(kage_core::protocol::NoticeLevel, String)>,
    /// One line on how the configured permission rules gate tools.
    pub permissions: String,
}

/// Keys the start card's change hints name, from the live keymap.
#[derive(Clone, Debug, Default)]
pub struct StartKeys {
    /// Opens the model picker.
    pub model: Option<String>,
    /// Cycles the thinking level.
    pub thinking: Option<String>,
    /// Opens the session picker.
    pub sessions: Option<String>,
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
    let sources = slot::Sources::new(status, session_usage, input, frame.area().width);
    let toast_rows = toast::toast_rows(toasts.len(), regions.buffer);
    let mut regions = regions;
    regions.buffer.height -= toast_rows;
    let toast_area = Rect {
        y: regions.buffer.bottom(),
        height: toast_rows,
        ..regions.buffer
    };
    slot::render_header(frame, regions.header, &sources);
    render_buffer(
        frame,
        regions,
        buffer,
        status.search_pattern,
        status.search_match_set,
    );
    if let Some(area) = start_area(buffer, regions.buffer).filter(|_| status.breadcrumb.is_none()) {
        slot::render_start(frame, area, &sources);
    }
    slot::render_activity(frame, regions.activity, &sources);
    render_input(frame, regions, input, &sources);
    render_toasts(frame, toast_area, toasts, &theme);
    if let Some(cl) = cmdline {
        render_cmdline_line(frame, regions.footer, cl);
        render_cmdline_error(frame, regions, cl);
        render_cmdline_popup(frame, regions, cl);
        place_cmdline_cursor(frame, regions, cl);
    } else if let Some(sl) = status.search_line {
        render_search_line(frame, regions.footer, sl, status.search_match_count);
        place_cmdline_cursor(frame, regions, sl);
    } else {
        slot::render_footer(frame, regions.footer, &sources);
    }
    capture_and_overlay(frame, regions, buffer, screen_selection, captured_rows);
}

/// Where the start card may paint: the buffer rows below the last
/// painted block, while the conversation is empty. Notice blocks, such
/// as config errors, stay above the card. Shell output ends it like a
/// prompt does.
fn start_area(buffer: &Buffer, area: Rect) -> Option<Rect> {
    let blocks = buffer.blocks();
    let is_notice = |b: &Block| matches!(b, Block::Custom { kind, .. } if kind != "kage:shell");
    if !blocks.iter().all(is_notice) {
        return None;
    }
    let top = match blocks.len().checked_sub(1) {
        None => area.y,
        Some(last) => buffer.screen_rows_of(last)?.1.saturating_add(1),
    };
    let bottom = area.y.saturating_add(area.height);
    (top < bottom).then(|| Rect::new(area.x, top, area.width, bottom - top))
}

/// Row heights of the chrome for one frame: the header and activity
/// rows collapse while their slots paint nothing, and the input fits
/// its draft.
#[must_use]
pub fn chrome_heights(
    status: &StatusCtx<'_>,
    session_usage: Option<&SessionUsage>,
    input: &InputState,
    width: u16,
) -> crate::layout::Heights {
    let sources = slot::Sources::new(status, session_usage, input, width);
    crate::layout::Heights {
        header: u16::from(slot::row_has_content(
            kage_plugin::SlotName::Header,
            &sources,
        )),
        activity: u16::from(slot::row_has_content(
            kage_plugin::SlotName::Activity,
            &sources,
        )),
        input: input_height(input, status.agents.len(), status.pending.len(), width),
        footer: 1,
    }
}

/// Input region height for `input`'s draft at terminal `width`: the
/// rows of pinned `agents` and `pending` prompts, then the wrapped
/// content rows, clamped to the configured bounds, plus the two rules.
#[must_use]
pub fn input_height(input: &InputState, agents: usize, pending: usize, width: u16) -> u16 {
    let rows = input_visual_row_count(input.text(), input_body_width(width));
    crate::layout::input_height_for(rows)
        .saturating_add(agents_height(agents))
        .saturating_add(pending_height(pending))
}

/// Width of the input's text column at terminal `width`: everything
/// right of the prompt glyph.
#[must_use]
pub fn input_body_width(width: u16) -> u16 {
    width.saturating_sub(INPUT_GLYPH_WIDTH)
}

/// Clip `s` to at most `max` display columns, appending `suffix`
/// (counted against `max`) when anything was dropped. Measures cells,
/// not chars, so wide text (CJK, emoji) never overruns its budget.
pub(crate) fn truncate_to_width(s: &str, max: usize, suffix: &str) -> String {
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_owned();
    }
    let budget = max.saturating_sub(suffix.width());
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push_str(suffix);
    out
}

/// Right-pad `s` with spaces to exactly `width` display columns;
/// never truncates (pair with [`truncate_to_width`] when a budget
/// applies). Measures cells, not chars.
pub(crate) fn pad_to_width(s: &str, width: usize) -> String {
    let w = s.width();
    if w >= width {
        return s.to_owned();
    }
    let mut out = s.to_owned();
    out.push_str(&" ".repeat(width - w));
    out
}

/// First-item offset for a popup list of `total` entries with at most
/// `max_visible` rows, keeping `selected` in view. Anchors to the top
/// while the selection fits, then scrolls only once it passes the
/// bottom: the early rows of a list stay put while cycling through
/// them. Returns `(offset, window)` with `window <= max_visible`.
pub(crate) fn popup_scroll_window(
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

/// First-item offset keeping `selected` roughly centered in a window
/// of `rows` over `total`, clamped so no blank rows render past the
/// end. Centering keeps mid-list selections visually stable instead
/// of hugging the bottom edge.
pub(crate) fn scroll_offset_centered(selected: usize, total: usize, rows: usize) -> usize {
    if rows == 0 || total <= rows {
        return 0;
    }
    selected
        .saturating_sub(rows / 2)
        .min(total.saturating_sub(rows))
}

mod blocks;
mod bubble;
mod buffer;
mod cmdline;
mod input;
mod modeline;
mod slot;

// Render entry points the top-level `render` calls.
use buffer::{capture_and_overlay, render_buffer};
use cmdline::{
    place_cmdline_cursor, render_cmdline_error, render_cmdline_line, render_cmdline_popup,
    render_search_line,
};
use input::render_input;

// Helpers shared across the split submodules, re-routed through the
// parent so each submodule's `use super::*` keeps resolving them.
pub(crate) use cmdline::highlight_matches_in_lines;
pub(crate) use input::wrap_input_rows;
pub(crate) use modeline::{
    input_cursor_position, input_scroll_offset, mode_border_color, mode_pill_style,
};

// Internal helpers the test module exercises directly.
#[cfg(test)]
pub(crate) use bubble::split_line_into_rows;
#[cfg(test)]
pub(crate) use modeline::input_visual_cursor;

// Re-exports so sibling block widgets keep resolving `super::*` helpers
// and the host (`app`) keeps its `view::*` entry points after the split.
pub(crate) use blocks::{
    ToolRow, assistant_style, custom_style, fold_indicator, header_line, prefix_line,
    thinking_style, tool_call_style, tool_group_lines, tool_row_lines,
};
pub(crate) use bubble::{
    FOCUS_RULE_WIDTH, bubble_content_width, mark_emphasis, plain_lines, user_block_lines,
    wrap_in_bubble_focused,
};
pub use buffer::CapturedCell;
pub(crate) use buffer::build_block_lines;
pub(crate) use input::{
    AGENT_MAX_ROWS, INPUT_GLYPH_WIDTH, agents_height, pending_height, split_input,
};
pub use input::{AgentRow, AgentRowState, PendingPrompt};
pub use modeline::input_visual_row_count;
pub(crate) use modeline::{
    chrome_lines_to_ratatui, format_token_count, spinner_frame, spinner_frame_index,
};
pub(crate) use slot::START_SESSIONS;

#[cfg(test)]
mod tests;
