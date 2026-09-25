//! Terminal color depth and downsampling.
//!
//! Themes are written in 24-bit color. A terminal that does not
//! advertise truecolor gets every painted color mapped to the nearest
//! entry of the 256-color palette, or of the 16 ANSI colors.

use ratatui::buffer::Buffer;
use ratatui::style::Color;

use super::ansi_rgb;

/// Channel levels of the 6x6x6 color cube in the 256-color palette.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// How many colors the terminal can show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit color: colors pass through unchanged.
    #[default]
    TrueColor,
    /// The xterm 256-color palette.
    Ansi256,
    /// The 16 ANSI colors.
    Ansi16,
}

impl ColorDepth {
    /// Read the depth from `COLORTERM` and `TERM`.
    #[must_use]
    pub fn detect() -> Self {
        let colorterm = std::env::var("COLORTERM").ok();
        let term = std::env::var("TERM").ok();
        Self::from_env(colorterm.as_deref(), term.as_deref())
    }

    /// The depth `COLORTERM` and `TERM` values describe: truecolor when
    /// `COLORTERM` says `truecolor` or `24bit` or `TERM` ends in
    /// `-direct`, 256 colors when `TERM` names `256color`, else 16.
    #[must_use]
    pub fn from_env(colorterm: Option<&str>, term: Option<&str>) -> Self {
        let colorterm = colorterm.unwrap_or_default().to_ascii_lowercase();
        let term = term.unwrap_or_default().to_ascii_lowercase();
        if matches!(colorterm.as_str(), "truecolor" | "24bit") || term.ends_with("-direct") {
            Self::TrueColor
        } else if term.contains("256color") {
            Self::Ansi256
        } else {
            Self::Ansi16
        }
    }

    /// `color` as this depth can show it.
    #[must_use]
    pub fn map(self, color: Color) -> Color {
        match (self, color) {
            (Self::Ansi256, Color::Rgb(r, g, b)) => Color::Indexed(nearest_256(r, g, b)),
            (Self::Ansi16, Color::Rgb(r, g, b)) => nearest_16((r, g, b)),
            (Self::Ansi16, Color::Indexed(i)) => nearest_16(indexed_rgb(i)),
            _ => color,
        }
    }

    /// Map every cell color in `buf` to this depth.
    pub fn apply(self, buf: &mut Buffer) {
        if self == Self::TrueColor {
            return;
        }
        for cell in &mut buf.content {
            cell.fg = self.map(cell.fg);
            cell.bg = self.map(cell.bg);
        }
    }
}

/// Index of the 256-color palette entry closest to `(r, g, b)`: the
/// nearer of the 6x6x6 cube and the grayscale ramp.
fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    let level = |v: u8| {
        (0..CUBE_LEVELS.len())
            .min_by_key(|&i| CUBE_LEVELS[i].abs_diff(v))
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = (CUBE_LEVELS[ri], CUBE_LEVELS[gi], CUBE_LEVELS[bi]);
    let average = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let gray_step = (average.saturating_sub(3) / 10).min(23);
    let gray_value = u8::try_from(8 + gray_step * 10).unwrap_or(u8::MAX);
    let gray = (gray_value, gray_value, gray_value);
    let target = (r, g, b);
    if distance(gray, target) < distance(cube, target) {
        u8::try_from(232 + gray_step).unwrap_or(u8::MAX)
    } else {
        u8::try_from(16 + 36 * ri + 6 * gi + bi).unwrap_or(u8::MAX)
    }
}

/// RGB of a 256-color palette entry under the xterm defaults.
fn indexed_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0..16 => {
            let (r, g, b) = ansi_rgb(i);
            let byte = |v: u32| u8::try_from(v).unwrap_or(u8::MAX);
            (byte(r), byte(g), byte(b))
        }
        16..232 => {
            let i = usize::from(i - 16);
            (
                CUBE_LEVELS[i / 36],
                CUBE_LEVELS[(i % 36) / 6],
                CUBE_LEVELS[i % 6],
            )
        }
        _ => {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
    }
}

/// The ANSI color closest to `rgb` under the xterm defaults.
fn nearest_16(rgb: (u8, u8, u8)) -> Color {
    const NAMED: [Color; 16] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    let nearest = (0u8..16)
        .min_by_key(|&i| distance(indexed_rgb(i), rgb))
        .unwrap_or(0);
    NAMED[usize::from(nearest)]
}

/// Squared distance between two colors, weighted toward green the way
/// the eye is.
fn distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let d = |x: u8, y: u8| u32::from(x.abs_diff(y)).pow(2);
    2 * d(a.0, b.0) + 4 * d(a.1, b.1) + 3 * d(a.2, b.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorterm_and_term_pick_the_depth() {
        let depth = ColorDepth::from_env;
        assert_eq!(
            depth(Some("truecolor"), Some("xterm")),
            ColorDepth::TrueColor
        );
        assert_eq!(depth(Some("24bit"), None), ColorDepth::TrueColor);
        assert_eq!(depth(None, Some("xterm-direct")), ColorDepth::TrueColor);
        assert_eq!(depth(None, Some("tmux-256color")), ColorDepth::Ansi256);
        assert_eq!(depth(Some(""), Some("xterm-256color")), ColorDepth::Ansi256);
        assert_eq!(depth(None, Some("xterm")), ColorDepth::Ansi16);
        assert_eq!(depth(None, None), ColorDepth::Ansi16);
    }

    #[test]
    fn rgb_maps_to_the_nearest_palette_entry() {
        let to256 = |r, g, b| ColorDepth::Ansi256.map(Color::Rgb(r, g, b));
        assert_eq!(to256(255, 0, 0), Color::Indexed(196));
        assert_eq!(to256(0, 0, 0), Color::Indexed(16));
        assert_eq!(to256(18, 20, 28), Color::Indexed(233));
        assert_eq!(to256(128, 128, 128), Color::Indexed(244));
        let to16 = |r, g, b| ColorDepth::Ansi16.map(Color::Rgb(r, g, b));
        assert_eq!(to16(18, 20, 28), Color::Black);
        assert_eq!(to16(228, 108, 108), Color::LightRed);
        assert_eq!(to16(226, 230, 238), Color::Gray);
        assert_eq!(ColorDepth::Ansi16.map(Color::Indexed(196)), Color::Red);
    }

    #[test]
    fn named_colors_and_truecolor_pass_through() {
        for depth in [ColorDepth::Ansi256, ColorDepth::Ansi16] {
            assert_eq!(depth.map(Color::Reset), Color::Reset);
            assert_eq!(depth.map(Color::Cyan), Color::Cyan);
        }
        let rgb = Color::Rgb(1, 2, 3);
        assert_eq!(ColorDepth::TrueColor.map(rgb), rgb);
        assert_eq!(
            ColorDepth::Ansi256.map(Color::Indexed(33)),
            Color::Indexed(33)
        );
    }
}
