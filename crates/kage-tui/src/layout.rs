//! Five-region layout: header, conversation buffer, activity row,
//! input, footer.
//!
//! ratatui already implements differential rendering, so this crate's
//! "layout engine" is a thin helper over [`ratatui::layout::Layout`]
//! that locks in the kage TUI's vertical structure:
//!
//! ```text
//! +-----------------------------+
//! | header (0 or 1 row)         |  collapses when empty
//! +-----------------------------+
//! | conversation buffer (rest)  |
//! +-----------------------------+
//! | activity (0 or 1 row)       |  the working row
//! +-----------------------------+
//! | top rule                    |
//! | input content (1..max rows) |  input
//! | bottom rule                 |
//! +-----------------------------+
//! | footer (1 row)              |  `:` and `/` lines paint here
//! +-----------------------------+
//! ```
//!
//! The caller decides every height except the buffer's per frame and
//! passes them as [`Heights`]. The input grows with its content up to
//! a small cap ([`input_height_for`]) so a runaway prompt never eats
//! the entire screen; [`INPUT_CHROME_LINES`] is the two rules around
//! the content.

use std::sync::RwLock;

use kage_core::sync::{read, write};
use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Default minimum number of *content* rows the input card holds.
/// The live value is configurable via [`set_input_bounds`].
pub const INPUT_CONTENT_MIN_LINES: u16 = 1;
/// Default maximum number of *content* rows the input card holds
/// before it stops growing. Past this the user scrolls inside the
/// card. The live value is configurable via [`set_input_bounds`].
pub const INPUT_CONTENT_MAX_LINES: u16 = 8;

/// Hard ceiling on the configurable input content max, so a bad
/// config value cannot let the input eat the whole screen.
pub const INPUT_CONTENT_MAX_CEILING: u16 = 64;

/// Live, host-configurable `(content_min, content_max)` bounds. The
/// host sets this once at startup from `[ui]` config; the renderer
/// reads it. Defaults match the `*_LINES` constants so behavior is
/// unchanged until a user opts in. Mirrors the `theme` global.
static INPUT_BOUNDS: RwLock<(u16, u16)> =
    RwLock::new((INPUT_CONTENT_MIN_LINES, INPUT_CONTENT_MAX_LINES));

/// Read the live `(content_min, content_max)` bounds.
#[must_use]
fn input_bounds() -> (u16, u16) {
    *read(&INPUT_BOUNDS)
}

/// Set the input card's content-row bounds from config. `min` is
/// floored at 1, `max` is clamped to `min..=INPUT_CONTENT_MAX_CEILING`
/// so an out-of-range or inverted config can never wedge the layout.
pub fn set_input_bounds(min: u16, max: u16) {
    let min = min.max(1);
    let max = max.clamp(min, INPUT_CONTENT_MAX_CEILING);
    *write(&INPUT_BOUNDS) = (min, max);
}
/// Rows the rules above and below the input content claim.
pub const INPUT_CHROME_LINES: u16 = 2;
/// Total minimum input region height (content + chrome).
pub const INPUT_MIN_LINES: u16 = INPUT_CONTENT_MIN_LINES + INPUT_CHROME_LINES;
/// Total maximum input region height (content + chrome).
pub const INPUT_MAX_LINES: u16 = INPUT_CONTENT_MAX_LINES + INPUT_CHROME_LINES;

/// Row heights of the fixed regions for one frame. The buffer takes
/// whatever is left.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Heights {
    /// Header rows: 1 when the header slot paints anything, else 0.
    pub header: u16,
    /// Activity rows: 1 when the activity slot paints anything, else 0.
    pub activity: u16,
    /// Input rows, rules included.
    pub input: u16,
    /// Footer rows.
    pub footer: u16,
}

/// Output of [`split`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Regions {
    /// Top row (session title, widgets, search count). Zero height
    /// while the header slot paints nothing.
    pub header: Rect,
    /// Center conversation buffer; consumes whatever vertical space is
    /// left after the other regions.
    pub buffer: Rect,
    /// The working row. Zero height while the activity slot paints
    /// nothing.
    pub activity: Rect,
    /// The input: a top rule, the content rows and a bottom rule.
    pub input: Rect,
    /// Bottom row: the footer slot, or the open `:` or `/` line.
    pub footer: Rect,
}

/// Split `area` into the kage regions, top to bottom, with the given
/// fixed heights. The buffer takes the rest.
#[must_use]
pub fn split(area: Rect, heights: Heights) -> Regions {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(heights.header),
            Constraint::Min(0),
            Constraint::Length(heights.activity),
            Constraint::Length(heights.input),
            Constraint::Length(heights.footer),
        ])
        .split(area);
    Regions {
        header: chunks[0],
        buffer: chunks[1],
        activity: chunks[2],
        input: chunks[3],
        footer: chunks[4],
    }
}

/// Pick the input region height that fits `content_lines` of text,
/// adding the two rules. Result is in
/// [`INPUT_MIN_LINES`]..=[`INPUT_MAX_LINES`] under the default bounds.
#[must_use]
pub fn input_height_for(content_lines: u16) -> u16 {
    let (cmin, cmax) = input_bounds();
    content_lines
        .clamp(cmin, cmax)
        .saturating_add(INPUT_CHROME_LINES)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Mutex, MutexGuard};

    /// Serializes tests against the process-global `INPUT_BOUNDS`.
    /// `configured_bounds_resize_the_input_then_restore` mutates it,
    /// and the `split`/`input_height` tests assert the default bounds;
    /// run in parallel, the readers observe the mutated values
    /// (observed as flaky `left: 4, right: 3` failures).
    static PROCESS_GLOBALS: Mutex<()> = Mutex::new(());

    fn process_globals() -> MutexGuard<'static, ()> {
        PROCESS_GLOBALS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn heights(header: u16, activity: u16, input: u16) -> Heights {
        Heights {
            header,
            activity,
            input,
            footer: 1,
        }
    }

    #[test]
    fn split_stacks_every_height_combination_top_to_bottom() {
        let area = Rect::new(0, 0, 80, 24);
        for header in [0, 1] {
            for activity in [0, 1] {
                for input in [INPUT_MIN_LINES, INPUT_MAX_LINES] {
                    let r = split(area, heights(header, activity, input));
                    let label = format!("header {header}, activity {activity}, input {input}");
                    assert_eq!(r.header, Rect::new(0, 0, 80, header), "{label}");
                    assert_eq!(r.buffer.y, header, "{label}");
                    assert_eq!(
                        r.buffer.height,
                        24 - header - activity - input - 1,
                        "{label}"
                    );
                    assert_eq!(r.activity.y, r.buffer.bottom(), "{label}");
                    assert_eq!(r.activity.height, activity, "{label}");
                    assert_eq!(
                        r.input,
                        Rect::new(0, r.activity.bottom(), 80, input),
                        "{label}"
                    );
                    assert_eq!(r.footer, Rect::new(0, 23, 80, 1), "{label}");
                }
            }
        }
    }

    #[test]
    fn input_height_for_clamps_both_directions() {
        let _globals = process_globals();
        assert_eq!(input_height_for(0), INPUT_MIN_LINES);
        assert_eq!(input_height_for(1), INPUT_MIN_LINES);
        assert_eq!(input_height_for(3), 3 + INPUT_CHROME_LINES);
        assert_eq!(input_height_for(8), INPUT_MAX_LINES);
        assert_eq!(input_height_for(20), INPUT_MAX_LINES);
    }

    #[test]
    fn configured_bounds_resize_the_input_then_restore() {
        let _globals = process_globals();
        // Enlarge the cap, verify the input grows past the old max,
        // then restore the default so other tests are unaffected.
        set_input_bounds(2, 20);
        assert_eq!(input_height_for(15), 15 + INPUT_CHROME_LINES);
        assert_eq!(input_height_for(0), 2 + INPUT_CHROME_LINES, "min floor");
        assert_eq!(input_height_for(99), 20 + INPUT_CHROME_LINES, "max cap");
        let r = split(Rect::new(0, 0, 80, 40), heights(0, 0, input_height_for(99)));
        assert_eq!(r.input.height, 20 + INPUT_CHROME_LINES);
        // Out-of-range config is clamped, not honored verbatim.
        set_input_bounds(0, 9999);
        assert_eq!(
            input_height_for(0),
            1 + INPUT_CHROME_LINES,
            "min floored to 1"
        );
        assert_eq!(
            input_height_for(9999),
            INPUT_CONTENT_MAX_CEILING + INPUT_CHROME_LINES,
            "max clamped to ceiling"
        );
        set_input_bounds(INPUT_CONTENT_MIN_LINES, INPUT_CONTENT_MAX_LINES);
        assert_eq!(input_height_for(20), INPUT_MAX_LINES, "restored default");
    }
}
