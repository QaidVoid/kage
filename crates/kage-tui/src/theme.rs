//! Theme palette: every color used by the renderer in one place.
//!
//! [`Theme`] is the single source of truth for every color choice the
//! conversation buffer, status bar, and overlay rendering paths make.
//! Default values match what `view.rs` previously hardcoded; bundled
//! variants ([`Theme::tokyo_night`], [`Theme::catppuccin_mocha`]) give
//! the user a couple of recognisable palettes out of the box without
//! needing a TOML loader yet.

use ratatui::style::Color;

/// Every color the TUI renderer might paint with. Add entries when a
/// new visual element shows up; never reach for a hardcoded `Color`
/// inside `view.rs`.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Display name (`"default"`, `"tokyo-night"`, etc.).
    pub name: String,
    /// Background of the user-prompt bubble.
    pub user_bg: Color,
    /// Left rule of the user-prompt bubble.
    pub user_rule: Color,
    /// Background of a successful tool block.
    pub tool_bg: Color,
    /// Background of an errored tool block.
    pub tool_error_bg: Color,
    /// Background of an in-flight (no result yet) tool block.
    pub tool_pending_bg: Color,
    /// Tool block left rule when none of the emphasis states apply.
    pub tool_rule: Color,
    /// Foreground for assistant text.
    pub assistant_fg: Color,
    /// Foreground for thinking text (rendered dim).
    pub thinking_fg: Color,
    /// Foreground for tool result body (success).
    pub tool_result_fg: Color,
    /// Foreground for tool result body when `is_error`.
    pub tool_error_fg: Color,
    /// Foreground for `[kage:notify]` and similar custom blocks.
    pub custom_fg: Color,
    /// Status bar background.
    pub status_bg: Color,
    /// Status bar dim text (model id, session pill).
    pub status_dim_fg: Color,
    /// Search-match emphasis color (rule + status counter).
    pub match_color: Color,
    /// Visual-selection emphasis color.
    pub selection_color: Color,
    /// Focus emphasis color (always `White` by convention).
    pub focus_color: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::default_dark()
    }
}

impl Theme {
    /// Built-in default palette. Matches the colors `view.rs` used
    /// before the theme refactor so existing screenshots stay valid.
    #[must_use]
    pub fn default_dark() -> Self {
        Self {
            name: "default".into(),
            user_bg: Color::Rgb(45, 53, 70),
            user_rule: Color::Cyan,
            tool_bg: Color::Rgb(30, 34, 44),
            tool_error_bg: Color::Rgb(58, 22, 28),
            tool_pending_bg: Color::Rgb(54, 42, 22),
            tool_rule: Color::Yellow,
            assistant_fg: Color::White,
            thinking_fg: Color::Gray,
            tool_result_fg: Color::Gray,
            tool_error_fg: Color::Red,
            custom_fg: Color::Magenta,
            status_bg: Color::DarkGray,
            status_dim_fg: Color::Gray,
            match_color: Color::Yellow,
            selection_color: Color::Magenta,
            focus_color: Color::White,
        }
    }

    /// Tokyo Night-inspired palette: cooler blue/purple bubbles,
    /// warm cyan accents.
    #[must_use]
    pub fn tokyo_night() -> Self {
        Self {
            name: "tokyo-night".into(),
            user_bg: Color::Rgb(36, 40, 59),
            user_rule: Color::Rgb(125, 207, 255),
            tool_bg: Color::Rgb(26, 30, 46),
            tool_error_bg: Color::Rgb(63, 22, 30),
            tool_pending_bg: Color::Rgb(58, 50, 28),
            tool_rule: Color::Rgb(224, 175, 104),
            assistant_fg: Color::Rgb(192, 202, 245),
            thinking_fg: Color::Rgb(86, 95, 137),
            tool_result_fg: Color::Rgb(169, 177, 214),
            tool_error_fg: Color::Rgb(247, 118, 142),
            custom_fg: Color::Rgb(187, 154, 247),
            status_bg: Color::Rgb(30, 32, 48),
            status_dim_fg: Color::Rgb(86, 95, 137),
            match_color: Color::Rgb(224, 175, 104),
            selection_color: Color::Rgb(187, 154, 247),
            focus_color: Color::Rgb(192, 202, 245),
        }
    }

    /// Catppuccin Mocha-inspired palette: warmer mauves and peaches.
    #[must_use]
    pub fn catppuccin_mocha() -> Self {
        Self {
            name: "catppuccin-mocha".into(),
            user_bg: Color::Rgb(49, 50, 68),
            user_rule: Color::Rgb(137, 220, 235),
            tool_bg: Color::Rgb(30, 30, 46),
            tool_error_bg: Color::Rgb(69, 26, 36),
            tool_pending_bg: Color::Rgb(58, 47, 28),
            tool_rule: Color::Rgb(249, 226, 175),
            assistant_fg: Color::Rgb(205, 214, 244),
            thinking_fg: Color::Rgb(108, 112, 134),
            tool_result_fg: Color::Rgb(166, 173, 200),
            tool_error_fg: Color::Rgb(243, 139, 168),
            custom_fg: Color::Rgb(203, 166, 247),
            status_bg: Color::Rgb(24, 24, 37),
            status_dim_fg: Color::Rgb(108, 112, 134),
            match_color: Color::Rgb(249, 226, 175),
            selection_color: Color::Rgb(203, 166, 247),
            focus_color: Color::Rgb(205, 214, 244),
        }
    }

    /// Resolve a theme by name. Unknown names return the default.
    #[must_use]
    pub fn by_name(name: &str) -> Self {
        match name {
            "tokyo-night" => Self::tokyo_night(),
            "catppuccin-mocha" => Self::catppuccin_mocha(),
            _ => Self::default_dark(),
        }
    }

    /// Names of every bundled theme; useful for tab-completion in
    /// `:theme set`.
    #[must_use]
    pub fn bundled_names() -> &'static [&'static str] {
        &["default", "tokyo-night", "catppuccin-mocha"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_is_default_dark() {
        let t = Theme::default();
        assert_eq!(t.name, "default");
    }

    #[test]
    fn by_name_falls_back_for_unknown() {
        let t = Theme::by_name("totally-not-a-theme");
        assert_eq!(t.name, "default");
    }

    #[test]
    fn bundled_names_includes_known_themes() {
        let names = Theme::bundled_names();
        assert!(names.contains(&"default"));
        assert!(names.contains(&"tokyo-night"));
        assert!(names.contains(&"catppuccin-mocha"));
    }
}
