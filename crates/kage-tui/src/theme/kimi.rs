//! The Kimi palettes, kimi dark and kimi light, from Kimi Code's TUI
//! (`darkColors` and `lightColors`).
//!
//! Kimi's TUI paints on the terminal background, so the surfaces come
//! from Kimi's desktop app instead. The text and accent tokens map
//! onto kage's palette tokens and go through the same [`build`]:
//!
//! | kage | Kimi |
//! | --- | --- |
//! | `fg`, `fg_strong`, `fg_dark`, `comment` | `text`, `textStrong`, `textDim`, `textMuted` |
//! | `fg_gutter` | `border` |
//! | `lantern`, `blue` | `primary` |
//! | `violet` | `shellMode` |
//! | `mist` | `accent` |
//! | `sand` | `roleUser` |
//! | `green`, `yellow`, `red` | `success`, `warning`, `error` |
//!
//! Kimi's diff colors equal `success` and `error`, which the diff
//! groups already link to. `borderFocus`, `diffAddedStrong`,
//! `diffRemovedStrong`, `diffGutter` and `diffMeta` have no kage role.

use super::Theme;
use super::kage::{Palette, build};

const DARK: Palette = Palette {
    bg: (0x12, 0x12, 0x12),
    bg_dark: (0x0d, 0x0d, 0x0d),
    bg_highlight: (0x1f, 0x1f, 0x1f),
    bg_raised: (0x29, 0x29, 0x29),
    fg: (0xe0, 0xe0, 0xe0),
    fg_strong: (0xf5, 0xf5, 0xf5),
    fg_dark: (0x88, 0x88, 0x88),
    comment: (0x6b, 0x6b, 0x6b),
    fg_gutter: (0x5a, 0x5a, 0x5a),
    lantern: (0x4f, 0xa8, 0xff),
    violet: (0xbd, 0x93, 0xf9),
    green: (0x4e, 0xc8, 0x7e),
    yellow: (0xe8, 0xa8, 0x38),
    red: (0xe8, 0x54, 0x54),
    blue: (0x4f, 0xa8, 0xff),
    mist: (0x5b, 0xc0, 0xbe),
    sand: (0xff, 0xcb, 0x6b),
    visual: 22,
};

const LIGHT: Palette = Palette {
    bg: (0xff, 0xff, 0xff),
    bg_dark: (0xf9, 0xfb, 0xfc),
    bg_highlight: (0xf5, 0xf5, 0xf5),
    bg_raised: (0xf5, 0xf5, 0xf5),
    fg: (0x1a, 0x1a, 0x1a),
    fg_strong: (0x1a, 0x1a, 0x1a),
    fg_dark: (0x45, 0x45, 0x45),
    comment: (0x5f, 0x5f, 0x5f),
    fg_gutter: (0x73, 0x73, 0x73),
    lantern: (0x15, 0x65, 0xc0),
    violet: (0x7c, 0x3a, 0xed),
    green: (0x0e, 0x7a, 0x38),
    yellow: (0x92, 0x66, 0x0a),
    red: (0xb9, 0x1c, 0x1c),
    blue: (0x15, 0x65, 0xc0),
    mist: (0x00, 0x83, 0x8f),
    sand: (0x9a, 0x4a, 0x00),
    visual: 16,
};

impl Theme {
    /// kimi dark: Kimi Code's dark palette on neutral grey surfaces
    /// with a blue primary accent.
    #[must_use]
    pub fn kimi_dark() -> Self {
        build("kimi-dark", &DARK)
    }

    /// kimi light: Kimi Code's light palette on white, tuned for
    /// 4.5:1 text contrast.
    #[must_use]
    pub fn kimi_light() -> Self {
        build("kimi-light", &LIGHT)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

    #[test]
    fn kimi_tokens_land_on_their_roles() {
        let dark = Theme::kimi_dark();
        assert_eq!(dark.bg, Color::Rgb(0x12, 0x12, 0x12));
        assert_eq!(dark.assistant_fg, Color::Rgb(0xe0, 0xe0, 0xe0));
        assert_eq!(dark.user_rule, Color::Rgb(0x4f, 0xa8, 0xff));
        assert_eq!(dark.input_border_visual, Color::Rgb(0xbd, 0x93, 0xf9));
        assert_eq!(dark.success_fg, Color::Rgb(0x4e, 0xc8, 0x7e));
        assert_eq!(dark.tool_error_fg, Color::Rgb(0xe8, 0x54, 0x54));
        assert!(!dark.bg_is_light());

        let light = Theme::kimi_light();
        assert_eq!(light.bg, Color::Rgb(0xff, 0xff, 0xff));
        assert_eq!(light.assistant_fg, Color::Rgb(0x1a, 0x1a, 0x1a));
        assert_eq!(light.user_rule, Color::Rgb(0x15, 0x65, 0xc0));
        assert_eq!(light.warning_fg, Color::Rgb(0x92, 0x66, 0x0a));
        assert!(light.bg_is_light());
    }
}
