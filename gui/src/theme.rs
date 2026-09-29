//! The kage shadow palette mapped onto the component library's theme roles.
//!
//! Token values come from the palette the repository bundles for the TUI
//! (`crates/kage-tui/src/theme/kage.rs`), so the desktop client renders the
//! same colors as the terminal frontend. The mapping table lives in
//! `gui/SPIKE.md`.

use gpui_kit::component::theme::{Theme, ThemeColor, ThemeMode};
use gpui_kit::{App, Hsla, rgb};

const BG: u32 = 0x0f0e13;
const BG_DARK: u32 = 0x0b0a0e;
const BG_HIGHLIGHT: u32 = 0x17151d;
const BG_RAISED: u32 = 0x221f2a;
const FG: u32 = 0xcdc9d6;
const FG_STRONG: u32 = 0xf4f1fa;
const FG_DARK: u32 = 0x9895a0;
const FG_GUTTER: u32 = 0x46444c;
const LANTERN: u32 = 0xf2a65a;
const VIOLET: u32 = 0xa98bfa;
const GREEN: u32 = 0x8bd49c;
const YELLOW: u32 = 0xe8c96a;
const RED: u32 = 0xf2727f;
const BLUE: u32 = 0x9ab4ff;
const MIST: u32 = 0xb8b0d4;
/// Violet over `bg` at 22 percent, the kage shadow selection color.
const VISUAL: u32 = 0x312a46;
/// Violet over `bg` at 14 percent, used for hover states.
const VISUAL_HOVER: u32 = 0x252033;

fn ink(hex: u32) -> Hsla {
    rgb(hex).into()
}

/// Install kage shadow as the theme of the running app.
pub fn apply_shadow(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    Theme::update(cx, |theme| theme.colors = shadow_colors());
}

/// The kage shadow role colors, over the toolkit's dark defaults.
fn shadow_colors() -> ThemeColor {
    let mut c = *ThemeColor::dark();
    c.background = ink(BG);
    c.foreground = ink(FG);
    c.border = ink(FG_GUTTER);
    c.window_border = ink(BG_RAISED);
    c.title_bar = ink(BG);
    c.title_bar_border = ink(FG_GUTTER);
    c.status_bar = ink(BG);
    c.status_bar_border = ink(FG_GUTTER);
    c.muted = ink(BG_HIGHLIGHT);
    c.muted_foreground = ink(FG_DARK);
    c.secondary = ink(BG_RAISED);
    c.secondary_foreground = ink(FG_STRONG);
    c.secondary_hover = ink(VISUAL_HOVER);
    c.secondary_active = ink(VISUAL);
    c.primary = ink(LANTERN);
    c.primary_foreground = ink(BG_DARK);
    c.primary_hover = ink(0xd8914b);
    c.primary_active = ink(0xbf7f3f);
    c.accent = ink(VISUAL);
    c.accent_foreground = ink(FG_STRONG);
    c.selection = ink(VISUAL);
    c.ring = ink(VIOLET);
    c.caret = ink(LANTERN);
    c.link = ink(BLUE);
    c.link_hover = ink(FG_STRONG);
    c.input = ink(BG_HIGHLIGHT);
    c.list = ink(BG);
    c.list_even = ink(BG_HIGHLIGHT);
    c.list_hover = ink(VISUAL_HOVER);
    c.list_active = ink(VISUAL);
    c.list_active_border = ink(VIOLET);
    c.popover = ink(BG_RAISED);
    c.popover_foreground = ink(FG);
    c.overlay = ink(BG_RAISED);
    c.tab_bar = ink(BG);
    c.tab = ink(BG);
    c.tab_foreground = ink(FG_DARK);
    c.tab_active = ink(BG_HIGHLIGHT);
    c.tab_active_foreground = ink(FG_STRONG);
    c.sidebar = ink(BG_DARK);
    c.sidebar_foreground = ink(FG_DARK);
    c.sidebar_border = ink(FG_GUTTER);
    c.sidebar_accent = ink(VISUAL);
    c.sidebar_accent_foreground = ink(FG_STRONG);
    c.sidebar_primary = ink(LANTERN);
    c.scrollbar_thumb = ink(FG_GUTTER);
    c.scrollbar_thumb_hover = ink(FG_DARK);
    c.danger = ink(RED);
    c.danger_foreground = ink(BG_DARK);
    c.warning = ink(YELLOW);
    c.warning_foreground = ink(BG_DARK);
    c.success = ink(GREEN);
    c.success_foreground = ink(BG_DARK);
    c.info = ink(BLUE);
    c.info_foreground = ink(BG_DARK);
    c.red = ink(RED);
    c.green = ink(GREEN);
    c.blue = ink(BLUE);
    c.yellow = ink(YELLOW);
    c.magenta = ink(VIOLET);
    c.cyan = ink(MIST);
    c.red_light = ink(FG_STRONG);
    c.green_light = ink(FG_STRONG);
    c.blue_light = ink(FG_STRONG);
    c.yellow_light = ink(FG_STRONG);
    c.magenta_light = ink(FG_STRONG);
    c.cyan_light = ink(FG_STRONG);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_palette_matches_the_bundled_tokens() {
        let colors = shadow_colors();
        assert_eq!(colors.background, ink(BG));
        assert_eq!(colors.selection, ink(VISUAL));
        assert_eq!(colors.primary, ink(LANTERN));
    }

    #[test]
    fn visual_is_violet_blended_over_background() {
        // violet 0xa98bfa over bg 0x0f0e13 at 22 percent, matching the
        // blend the TUI palette uses for selections
        let mix = |a: u32, b: u32| (a * 22 + b * 78 + 50) / 100;
        let r = mix(0xa9, 0x0f);
        let g = mix(0x8b, 0x0e);
        let b = mix(0xfa, 0x13);
        assert_eq!(VISUAL, (r << 16) | (g << 8) | b);
    }
}
