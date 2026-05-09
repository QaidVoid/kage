//! Three-region layout: status bar, conversation buffer, input area.
//!
//! ratatui already implements differential rendering, so this crate's
//! "layout engine" is just a thin helper over [`ratatui::layout::Layout`]
//! that locks in the kage TUI's vertical structure:
//!
//! ```text
//! +-----------------------------+
//! | status bar (1 line)         |
//! +-----------------------------+
//! |                             |
//! | conversation buffer (rest)  |
//! |                             |
//! +-----------------------------+
//! | input area (1..=8 lines)    |
//! +-----------------------------+
//! ```
//!
//! The input area grows with content up to a small cap so a runaway
//! prompt never eats the entire screen.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Min and max lines reserved for the input area.
pub const INPUT_MIN_LINES: u16 = 1;
/// Hard cap on the input area's height in lines.
pub const INPUT_MAX_LINES: u16 = 8;

/// Output of [`split`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Regions {
    /// Top single-line status bar.
    pub status: Rect,
    /// Center conversation buffer; consumes whatever vertical space is
    /// left after status and input.
    pub buffer: Rect,
    /// Bottom input area.
    pub input: Rect,
}

/// Split `area` into the three kage regions. `input_height` is clamped
/// to `INPUT_MIN_LINES..=INPUT_MAX_LINES`. If the available height does
/// not even fit the status bar plus a one-line input, the buffer is
/// allowed to shrink to zero rows so the regions still tile cleanly.
#[must_use]
pub fn split(area: Rect, input_height: u16) -> Regions {
    let input = input_height.clamp(INPUT_MIN_LINES, INPUT_MAX_LINES);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(input),
        ])
        .split(area);
    Regions {
        status: chunks[0],
        buffer: chunks[1],
        input: chunks[2],
    }
}

/// Pick the input height that fits `requested_lines`, never below
/// [`INPUT_MIN_LINES`] and never above [`INPUT_MAX_LINES`].
#[must_use]
pub fn input_height_for(requested_lines: u16) -> u16 {
    requested_lines.clamp(INPUT_MIN_LINES, INPUT_MAX_LINES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_carves_three_regions_top_to_bottom() {
        let area = Rect::new(0, 0, 80, 24);
        let regions = split(area, 1);
        assert_eq!(regions.status, Rect::new(0, 0, 80, 1));
        assert_eq!(regions.input, Rect::new(0, 23, 80, 1));
        assert_eq!(regions.buffer, Rect::new(0, 1, 80, 22));
    }

    #[test]
    fn split_grows_input_up_to_cap() {
        let area = Rect::new(0, 0, 80, 30);
        let regions = split(area, 12);
        assert_eq!(regions.input.height, INPUT_MAX_LINES);
        assert_eq!(regions.buffer.height, 30 - 1 - INPUT_MAX_LINES);
    }

    #[test]
    fn split_clamps_below_minimum() {
        let area = Rect::new(0, 0, 80, 10);
        let regions = split(area, 0);
        assert_eq!(regions.input.height, INPUT_MIN_LINES);
    }

    #[test]
    fn split_handles_skinny_height() {
        // Only enough rows for status + input.
        let area = Rect::new(0, 0, 80, 2);
        let regions = split(area, 1);
        assert_eq!(regions.status.height, 1);
        assert_eq!(regions.input.height, 1);
        assert_eq!(regions.buffer.height, 0);
    }

    #[test]
    fn input_height_for_clamps_both_directions() {
        assert_eq!(input_height_for(0), INPUT_MIN_LINES);
        assert_eq!(input_height_for(3), 3);
        assert_eq!(input_height_for(20), INPUT_MAX_LINES);
    }
}
