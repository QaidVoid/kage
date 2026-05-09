//! Four-region layout: top status bar, conversation buffer, input
//! card, bottom modeline.
//!
//! ratatui already implements differential rendering, so this crate's
//! "layout engine" is just a thin helper over [`ratatui::layout::Layout`]
//! that locks in the kage TUI's vertical structure:
//!
//! ```text
//! +-----------------------------+
//! | status bar (1 line)         |  status_top
//! +-----------------------------+
//! |                             |
//! | conversation buffer (rest)  |  buffer
//! |                             |
//! +-----------------------------+
//! | +-----+-----+-----+-----+   |
//! | | bordered input card   |   |  input  (3..=10 rows)
//! | +-----+-----+-----+-----+   |
//! +-----------------------------+
//! | modeline (1 line)           |  status_bottom
//! +-----------------------------+
//! ```
//!
//! The input area grows with content up to a small cap so a runaway
//! prompt never eats the entire screen. The bordered card claims two
//! extra rows over the raw text height for its top and bottom borders;
//! the [`INPUT_CHROME_LINES`] constant captures that overhead.
//!
//! The bottom modeline is opt-in plugin chrome: callers pass
//! `status_bottom_height = 0` to suppress the row entirely (the
//! default until a plugin or extension registers a widget there) and
//! a positive height once content is available. The mode label lives
//! on the input card's top border, not in the modeline; this row is
//! reserved exclusively for plugin status widgets.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Minimum number of *content* rows the input card holds.
pub const INPUT_CONTENT_MIN_LINES: u16 = 1;
/// Maximum number of *content* rows the input card holds before it
/// stops growing. Past this the user scrolls inside the card.
pub const INPUT_CONTENT_MAX_LINES: u16 = 8;
/// Rows the bordered chrome around the input content claims (top
/// border + bottom border).
pub const INPUT_CHROME_LINES: u16 = 2;
/// Total minimum input region height (content + chrome).
pub const INPUT_MIN_LINES: u16 = INPUT_CONTENT_MIN_LINES + INPUT_CHROME_LINES;
/// Total maximum input region height (content + chrome).
pub const INPUT_MAX_LINES: u16 = INPUT_CONTENT_MAX_LINES + INPUT_CHROME_LINES;
/// Rows the bottom modeline claims when it has anything to show.
/// Stage A treats this as opt-in: callers pass `0` to suppress the
/// row entirely (default behavior until plugin widgets register
/// themselves), and `STATUS_BOTTOM_LINES_DEFAULT` when content is
/// available.
pub const STATUS_BOTTOM_LINES_DEFAULT: u16 = 1;

/// Output of [`split`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Regions {
    /// Top single-line status bar (model, session pill, search count).
    pub status: Rect,
    /// Center conversation buffer; consumes whatever vertical space is
    /// left after status, input, and modeline.
    pub buffer: Rect,
    /// Bordered input card. Render with `Borders::ALL`; the renderer
    /// owns the inset math for the inner content area.
    pub input: Rect,
    /// Bottom single-line modeline (mode pill, plugin widgets).
    pub status_bottom: Rect,
}

/// Split `area` into the four kage regions. `input_height` is
/// clamped to `INPUT_MIN_LINES..=INPUT_MAX_LINES`. `status_bottom`
/// is opt-in: pass `0` when nothing wants to draw there (the row is
/// returned with `height = 0` so renderers naturally skip it), or a
/// positive height when plugin widgets / extension chrome have
/// registered themselves.
#[must_use]
pub fn split(area: Rect, input_height: u16, status_bottom_height: u16) -> Regions {
    let input = input_height.clamp(INPUT_MIN_LINES, INPUT_MAX_LINES);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(input),
            Constraint::Length(status_bottom_height),
        ])
        .split(area);
    Regions {
        status: chunks[0],
        buffer: chunks[1],
        input: chunks[2],
        status_bottom: chunks[3],
    }
}

/// Pick the input region height that fits `content_lines` of text,
/// adding chrome for the bordered card. Result is in
/// [`INPUT_MIN_LINES`]..=[`INPUT_MAX_LINES`].
#[must_use]
pub fn input_height_for(content_lines: u16) -> u16 {
    let clamped = content_lines.clamp(INPUT_CONTENT_MIN_LINES, INPUT_CONTENT_MAX_LINES);
    clamped.saturating_add(INPUT_CHROME_LINES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_carves_four_regions_top_to_bottom() {
        let area = Rect::new(0, 0, 80, 24);
        let regions = split(area, INPUT_MIN_LINES, STATUS_BOTTOM_LINES_DEFAULT);
        assert_eq!(regions.status, Rect::new(0, 0, 80, 1));
        assert_eq!(regions.input, Rect::new(0, 20, 80, INPUT_MIN_LINES));
        assert_eq!(regions.status_bottom, Rect::new(0, 23, 80, 1));
        assert_eq!(
            regions.buffer,
            Rect::new(0, 1, 80, 24 - 1 - 1 - INPUT_MIN_LINES)
        );
    }

    #[test]
    fn split_with_zero_status_bottom_collapses_modeline() {
        let area = Rect::new(0, 0, 80, 24);
        let regions = split(area, INPUT_MIN_LINES, 0);
        assert_eq!(regions.status_bottom.height, 0);
        // Buffer reclaims the row that the modeline would have used.
        assert_eq!(regions.buffer.height, 24 - 1 - INPUT_MIN_LINES);
    }

    #[test]
    fn split_grows_input_up_to_cap() {
        let area = Rect::new(0, 0, 80, 30);
        let regions = split(area, 30, STATUS_BOTTOM_LINES_DEFAULT);
        assert_eq!(regions.input.height, INPUT_MAX_LINES);
        assert_eq!(regions.status_bottom.height, STATUS_BOTTOM_LINES_DEFAULT);
        assert_eq!(
            regions.buffer.height,
            30 - 1 - INPUT_MAX_LINES - STATUS_BOTTOM_LINES_DEFAULT
        );
    }

    #[test]
    fn split_clamps_below_minimum() {
        let area = Rect::new(0, 0, 80, 12);
        let regions = split(area, 0, 0);
        assert_eq!(regions.input.height, INPUT_MIN_LINES);
    }

    #[test]
    fn input_height_for_clamps_both_directions() {
        assert_eq!(input_height_for(0), INPUT_MIN_LINES);
        assert_eq!(input_height_for(1), INPUT_MIN_LINES);
        assert_eq!(input_height_for(3), 3 + INPUT_CHROME_LINES);
        assert_eq!(input_height_for(8), INPUT_MAX_LINES);
        assert_eq!(input_height_for(20), INPUT_MAX_LINES);
    }
}
