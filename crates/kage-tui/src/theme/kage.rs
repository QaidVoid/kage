//! The kage palettes, kage shadow (dark) and kage dawn (light), and
//! how their tokens map onto the renderer's roles.
//!
//! The token values match kage-theme's `lua/kage/palette.lua`, which
//! also drives the Neovim colorscheme and the terminal configs, so
//! the TUI, the editor and the terminal share one palette.

use kage_core::highlight::Highlights;
use ratatui::style::Color;

use super::Theme;

/// The tokens of one kage variant. Other bundled palettes fill the
/// same tokens to reuse [`build`].
pub(super) struct Palette {
    pub(super) bg: (u8, u8, u8),
    pub(super) bg_dark: (u8, u8, u8),
    pub(super) bg_highlight: (u8, u8, u8),
    pub(super) bg_raised: (u8, u8, u8),
    pub(super) fg: (u8, u8, u8),
    pub(super) fg_strong: (u8, u8, u8),
    pub(super) fg_dark: (u8, u8, u8),
    pub(super) comment: (u8, u8, u8),
    pub(super) fg_gutter: (u8, u8, u8),
    pub(super) lantern: (u8, u8, u8),
    pub(super) violet: (u8, u8, u8),
    pub(super) green: (u8, u8, u8),
    pub(super) yellow: (u8, u8, u8),
    pub(super) red: (u8, u8, u8),
    pub(super) blue: (u8, u8, u8),
    pub(super) mist: (u8, u8, u8),
    pub(super) sand: (u8, u8, u8),
    /// Percent of violet over `bg` for selections.
    pub(super) visual: u16,
}

const SHADOW: Palette = Palette {
    bg: (0x0f, 0x0e, 0x13),
    bg_dark: (0x0b, 0x0a, 0x0e),
    bg_highlight: (0x17, 0x15, 0x1d),
    bg_raised: (0x22, 0x1f, 0x2a),
    fg: (0xcd, 0xc9, 0xd6),
    fg_strong: (0xf4, 0xf1, 0xfa),
    fg_dark: (0x98, 0x95, 0xa0),
    comment: (0x6f, 0x6a, 0x7e),
    fg_gutter: (0x46, 0x44, 0x4c),
    lantern: (0xf2, 0xa6, 0x5a),
    violet: (0xa9, 0x8b, 0xfa),
    green: (0x8b, 0xd4, 0x9c),
    yellow: (0xe8, 0xc9, 0x6a),
    red: (0xf2, 0x72, 0x7f),
    blue: (0x9a, 0xb4, 0xff),
    mist: (0xb8, 0xb0, 0xd4),
    sand: (0xe9, 0xc2, 0x9a),
    visual: 22,
};

const DAWN: Palette = Palette {
    bg: (0xf7, 0xf5, 0xf9),
    bg_dark: (0xee, 0xeb, 0xf2),
    bg_highlight: (0xef, 0xec, 0xf3),
    bg_raised: (0xe4, 0xdf, 0xeb),
    fg: (0x2b, 0x27, 0x35),
    fg_strong: (0x15, 0x12, 0x1c),
    fg_dark: (0x56, 0x50, 0x6a),
    comment: (0x8a, 0x84, 0x99),
    fg_gutter: (0xb4, 0xae, 0xc0),
    lantern: (0xa8, 0x58, 0x0f),
    violet: (0x6a, 0x47, 0xcc),
    green: (0x2b, 0x74, 0x43),
    yellow: (0x83, 0x64, 0x09),
    red: (0xbf, 0x36, 0x49),
    blue: (0x34, 0x56, 0xc2),
    mist: (0x5a, 0x52, 0x82),
    sand: (0x93, 0x56, 0x28),
    visual: 16,
};

fn rgb(c: (u8, u8, u8)) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// `fg` over `bg` at `percent` (0 gives `bg`, 100 gives `fg`), rounded
/// half up like kage-theme's `util.blend`.
fn blend(fg: (u8, u8, u8), bg: (u8, u8, u8), percent: u16) -> Color {
    let mix = |a: u8, b: u8| {
        let v = (u16::from(a) * percent + u16::from(b) * (100 - percent) + 50) / 100;
        u8::try_from(v).unwrap_or(u8::MAX)
    };
    Color::Rgb(mix(fg.0, bg.0), mix(fg.1, bg.1), mix(fg.2, bg.2))
}

/// Map the tokens of `p` onto the renderer's roles.
pub(super) fn build(name: &str, p: &Palette) -> Theme {
    let visual = blend(p.violet, p.bg, p.visual);
    Theme {
        name: name.into(),
        transparent: false,
        bg: rgb(p.bg),
        user_bg: rgb(p.bg_raised),
        assistant_rule: rgb(p.fg_gutter),
        user_rule: rgb(p.lantern),
        tool_bg: rgb(p.bg_highlight),
        tool_error_bg: blend(p.red, p.bg, 14),
        tool_pending_bg: blend(p.yellow, p.bg, 12),
        tool_rule: rgb(p.sand),
        tool_error_rule: rgb(p.red),
        tool_pending_rule: rgb(p.yellow),
        assistant_fg: rgb(p.fg),
        thinking_fg: rgb(p.comment),
        tool_result_fg: rgb(p.fg_dark),
        tool_error_fg: rgb(p.red),
        custom_fg: rgb(p.fg_dark),
        status_bg: rgb(p.bg),
        status_dim_fg: rgb(p.fg_dark),
        muted_fg: rgb(p.fg_dark),
        match_color: rgb(p.lantern),
        selection_color: visual,
        focus_color: rgb(p.fg_strong),
        input_border_normal: rgb(p.fg_gutter),
        input_border_insert: rgb(p.lantern),
        input_border_visual: rgb(p.violet),
        input_pill_normal_bg: blend(p.fg, p.bg, 16),
        input_pill_normal_fg: rgb(p.fg),
        input_pill_insert_bg: rgb(p.lantern),
        input_pill_insert_fg: rgb(p.bg_dark),
        input_pill_visual_bg: rgb(p.violet),
        input_pill_visual_fg: rgb(p.bg_dark),
        input_glyph_fg: rgb(p.lantern),
        input_placeholder_fg: rgb(p.comment),
        input_hint_fg: rgb(p.fg_dark),
        modeline_bg: rgb(p.bg),
        modeline_fg: rgb(p.fg_dark),
        overlay_fg: rgb(p.fg),
        overlay_border: rgb(p.fg_gutter),
        overlay_selected_bg: visual,
        overlay_selected_fg: rgb(p.fg_strong),
        selection_fg: rgb(p.fg_strong),
        warning_fg: rgb(p.yellow),
        md_h1_fg: rgb(p.lantern),
        md_h2_fg: rgb(p.violet),
        md_link_fg: rgb(p.blue),
        md_code_fg: rgb(p.mist),
        success_fg: rgb(p.green),
        groups: Highlights::new(),
    }
}

impl Theme {
    /// kage shadow: ink surfaces with a violet undertone and one warm
    /// lantern accent.
    #[must_use]
    pub fn kage_shadow() -> Self {
        build("kage-shadow", &SHADOW)
    }

    /// kage dawn: ink on a violet-tinted paper, accents deepened to
    /// keep 4.5:1 contrast.
    #[must_use]
    pub fn kage_dawn() -> Self {
        build("kage-dawn", &DAWN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_matches_kage_theme() {
        // kage-theme's bg_visual for each variant (PALETTE.md)
        assert_eq!(
            blend(SHADOW.violet, SHADOW.bg, 22),
            Color::Rgb(0x31, 0x2a, 0x46)
        );
        assert_eq!(
            blend(DAWN.violet, DAWN.bg, 16),
            Color::Rgb(0xe0, 0xd9, 0xf2)
        );
        assert_eq!(blend(SHADOW.fg, SHADOW.bg, 100), rgb(SHADOW.fg));
        assert_eq!(blend(SHADOW.fg, SHADOW.bg, 0), rgb(SHADOW.bg));
    }

    #[test]
    fn selections_match_kage_theme() {
        assert_eq!(
            Theme::kage_shadow().selection_color,
            Color::Rgb(0x31, 0x2a, 0x46)
        );
        assert_eq!(
            Theme::kage_dawn().selection_color,
            Color::Rgb(0xe0, 0xd9, 0xf2)
        );
    }
}
