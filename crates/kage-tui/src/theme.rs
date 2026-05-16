//! Theme palette: every color used by the renderer in one place.
//!
//! [`Theme`] is the single source of truth for every color choice the
//! conversation buffer, status bar, and overlay rendering paths make.
//! Default values match what `view.rs` previously hardcoded; bundled
//! variants ([`Theme::tokyo_night`], [`Theme::catppuccin_mocha`]) give
//! the user a couple of recognisable palettes out of the box without
//! needing a TOML loader yet.
//!
//! The renderer reads the active theme via [`current`] (returns a
//! cheap clone of the global). The host process picks one with
//! [`set_current`] - typically once at startup, but `:theme set <name>`
//! also goes through this path so a swap takes effect on the next
//! frame without restarting the TUI.

use std::path::Path;
use std::sync::RwLock;

use ratatui::style::Color;

static CURRENT: RwLock<Option<Theme>> = RwLock::new(None);

/// Snapshot of the active theme. Returns the default palette when no
/// host has called [`set_current`] yet, so leaf style helpers don't
/// need to special-case startup ordering.
#[must_use]
pub fn current() -> Theme {
    CURRENT
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_default()
}

/// Replace the process-wide theme. Subsequent renders pick up the
/// new palette; in-flight frames continue with the snapshot they
/// already captured.
pub fn set_current(theme: Theme) {
    if let Ok(mut guard) = CURRENT.write() {
        *guard = Some(theme);
    }
}

/// Every color the TUI renderer might paint with. Add entries when a
/// new visual element shows up; never reach for a hardcoded `Color`
/// inside `view.rs`.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Display name (`"default"`, `"tokyo-night"`, etc.).
    pub name: String,
    /// When `true`, kage does not paint the whole-frame opaque base,
    /// letting a blurred/transparent terminal show through the entire
    /// UI. A terminal grid has no per-cell alpha, so this is the only
    /// meaningful "transparency" knob: opaque (default) or not.
    pub transparent: bool,
    /// Base canvas painted behind the whole conversation so blocks
    /// sit on one uniform surface (no terminal-background patchwork
    /// between blocks). Slightly darker than the block tints.
    pub bg: Color,
    /// Background of the user-prompt bubble.
    pub user_bg: Color,
    /// Persistent left-spine accent for assistant turns when idle
    /// (focus/search override it). Recessive: it anchors the turn
    /// without competing with `user_rule` / `tool_rule`.
    pub assistant_rule: Color,
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
    /// Tool block left rule for errored tool blocks.
    pub tool_error_rule: Color,
    /// Tool block left rule for in-flight tool blocks.
    pub tool_pending_rule: Color,
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
    /// Status bar background. Bundled palettes set this equal to
    /// [`Self::bg`] so the top bar blends into the canvas instead of
    /// rendering as a heavy band; a user theme can give it a distinct
    /// value to get a banded look back.
    pub status_bg: Color,
    /// Status bar secondary text (session pill). Bundled palettes keep
    /// this for compatibility; the built-in renderer now uses
    /// [`Self::muted_fg`] so chrome text stays legible without a band.
    pub status_dim_fg: Color,
    /// Secondary-but-readable text: fold hints (`zo to expand`),
    /// byte/timing metadata. A real mid-contrast grey, not `DIM` on
    /// dim, so affordance hints stay discoverable.
    pub muted_fg: Color,
    /// Search-match emphasis color (rule + status counter).
    pub match_color: Color,
    /// Visual-selection emphasis color.
    pub selection_color: Color,
    /// Focus emphasis color (always `White` by convention).
    pub focus_color: Color,
    /// Border color of the input card while [`crate::Mode::Normal`].
    pub input_border_normal: Color,
    /// Border color of the input card while [`crate::Mode::Insert`].
    pub input_border_insert: Color,
    /// Border color of the input card while [`crate::Mode::Visual`].
    pub input_border_visual: Color,
    /// Background of the mode pill rendered on the input card's top
    /// border, keyed by mode.
    pub input_pill_normal_bg: Color,
    /// Foreground of the mode pill on the input card, keyed by mode.
    pub input_pill_normal_fg: Color,
    /// Background of the mode pill while [`crate::Mode::Insert`].
    pub input_pill_insert_bg: Color,
    /// Foreground of the mode pill while [`crate::Mode::Insert`].
    pub input_pill_insert_fg: Color,
    /// Background of the mode pill while [`crate::Mode::Visual`].
    pub input_pill_visual_bg: Color,
    /// Foreground of the mode pill while [`crate::Mode::Visual`].
    pub input_pill_visual_fg: Color,
    /// Color of the leading prompt glyph (`>`) inside the input card.
    pub input_glyph_fg: Color,
    /// Foreground of the dim placeholder text shown when the input is
    /// empty.
    pub input_placeholder_fg: Color,
    /// Foreground of the contextual hint shown on the right side of
    /// the input card's top border.
    pub input_hint_fg: Color,
    /// Background of the bottom modeline. Bundled palettes set this
    /// equal to [`Self::bg`] so the strip blends into the canvas; a
    /// user theme can override it for a banded modeline.
    pub modeline_bg: Color,
    /// Foreground of the bottom modeline's text.
    pub modeline_fg: Color,
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
            transparent: false,
            bg: Color::Rgb(18, 20, 28),
            user_bg: Color::Rgb(36, 42, 58),
            assistant_rule: Color::Rgb(70, 80, 105),
            user_rule: Color::Cyan,
            tool_bg: Color::Rgb(30, 34, 44),
            tool_error_bg: Color::Rgb(58, 22, 28),
            tool_pending_bg: Color::Rgb(54, 42, 22),
            tool_rule: Color::Yellow,
            tool_error_rule: Color::Red,
            tool_pending_rule: Color::Rgb(204, 153, 0),
            assistant_fg: Color::White,
            thinking_fg: Color::Rgb(100, 108, 130),
            tool_result_fg: Color::Gray,
            tool_error_fg: Color::Red,
            custom_fg: Color::Magenta,
            status_bg: Color::Rgb(18, 20, 28),
            status_dim_fg: Color::Gray,
            muted_fg: Color::Rgb(140, 148, 165),
            match_color: Color::Yellow,
            selection_color: Color::Magenta,
            focus_color: Color::White,
            input_border_normal: Color::Rgb(80, 92, 120),
            input_border_insert: Color::Cyan,
            input_border_visual: Color::Magenta,
            input_pill_normal_bg: Color::Blue,
            input_pill_normal_fg: Color::White,
            input_pill_insert_bg: Color::Cyan,
            input_pill_insert_fg: Color::Black,
            input_pill_visual_bg: Color::Magenta,
            input_pill_visual_fg: Color::White,
            input_glyph_fg: Color::Cyan,
            input_placeholder_fg: Color::DarkGray,
            input_hint_fg: Color::Gray,
            modeline_bg: Color::Rgb(18, 20, 28),
            modeline_fg: Color::Gray,
        }
    }

    /// Tokyo Night-inspired palette: cooler blue/purple bubbles,
    /// warm cyan accents.
    #[must_use]
    pub fn tokyo_night() -> Self {
        Self {
            name: "tokyo-night".into(),
            transparent: false,
            bg: Color::Rgb(22, 23, 34),
            user_bg: Color::Rgb(36, 40, 59),
            assistant_rule: Color::Rgb(86, 95, 137),
            user_rule: Color::Rgb(125, 207, 255),
            tool_bg: Color::Rgb(26, 30, 46),
            tool_error_bg: Color::Rgb(63, 22, 30),
            tool_pending_bg: Color::Rgb(58, 50, 28),
            tool_rule: Color::Rgb(224, 175, 104),
            tool_error_rule: Color::Rgb(247, 118, 142),
            tool_pending_rule: Color::Rgb(224, 175, 104),
            assistant_fg: Color::Rgb(192, 202, 245),
            thinking_fg: Color::Rgb(86, 95, 137),
            tool_result_fg: Color::Rgb(169, 177, 214),
            tool_error_fg: Color::Rgb(247, 118, 142),
            custom_fg: Color::Rgb(187, 154, 247),
            status_bg: Color::Rgb(22, 23, 34),
            status_dim_fg: Color::Rgb(86, 95, 137),
            muted_fg: Color::Rgb(130, 140, 180),
            match_color: Color::Rgb(224, 175, 104),
            selection_color: Color::Rgb(187, 154, 247),
            focus_color: Color::Rgb(192, 202, 245),
            input_border_normal: Color::Rgb(86, 95, 137),
            input_border_insert: Color::Rgb(125, 207, 255),
            input_border_visual: Color::Rgb(187, 154, 247),
            input_pill_normal_bg: Color::Rgb(86, 95, 137),
            input_pill_normal_fg: Color::Rgb(192, 202, 245),
            input_pill_insert_bg: Color::Rgb(125, 207, 255),
            input_pill_insert_fg: Color::Rgb(20, 22, 34),
            input_pill_visual_bg: Color::Rgb(187, 154, 247),
            input_pill_visual_fg: Color::Rgb(20, 22, 34),
            input_glyph_fg: Color::Rgb(125, 207, 255),
            input_placeholder_fg: Color::Rgb(86, 95, 137),
            input_hint_fg: Color::Rgb(108, 119, 165),
            modeline_bg: Color::Rgb(22, 23, 34),
            modeline_fg: Color::Rgb(108, 119, 165),
        }
    }

    /// Catppuccin Mocha-inspired palette: warmer mauves and peaches.
    #[must_use]
    pub fn catppuccin_mocha() -> Self {
        Self {
            name: "catppuccin-mocha".into(),
            transparent: false,
            bg: Color::Rgb(24, 24, 37),
            user_bg: Color::Rgb(49, 50, 68),
            assistant_rule: Color::Rgb(88, 91, 112),
            user_rule: Color::Rgb(137, 220, 235),
            tool_bg: Color::Rgb(30, 30, 46),
            tool_error_bg: Color::Rgb(69, 26, 36),
            tool_pending_bg: Color::Rgb(58, 47, 28),
            tool_rule: Color::Rgb(249, 226, 175),
            tool_error_rule: Color::Rgb(243, 139, 168),
            tool_pending_rule: Color::Rgb(249, 226, 175),
            assistant_fg: Color::Rgb(205, 214, 244),
            thinking_fg: Color::Rgb(108, 112, 134),
            tool_result_fg: Color::Rgb(166, 173, 200),
            tool_error_fg: Color::Rgb(243, 139, 168),
            custom_fg: Color::Rgb(203, 166, 247),
            status_bg: Color::Rgb(24, 24, 37),
            status_dim_fg: Color::Rgb(108, 112, 134),
            muted_fg: Color::Rgb(147, 153, 178),
            match_color: Color::Rgb(249, 226, 175),
            selection_color: Color::Rgb(203, 166, 247),
            focus_color: Color::Rgb(205, 214, 244),
            input_border_normal: Color::Rgb(108, 112, 134),
            input_border_insert: Color::Rgb(137, 220, 235),
            input_border_visual: Color::Rgb(203, 166, 247),
            input_pill_normal_bg: Color::Rgb(108, 112, 134),
            input_pill_normal_fg: Color::Rgb(205, 214, 244),
            input_pill_insert_bg: Color::Rgb(137, 220, 235),
            input_pill_insert_fg: Color::Rgb(24, 24, 37),
            input_pill_visual_bg: Color::Rgb(203, 166, 247),
            input_pill_visual_fg: Color::Rgb(24, 24, 37),
            input_glyph_fg: Color::Rgb(137, 220, 235),
            input_placeholder_fg: Color::Rgb(108, 112, 134),
            input_hint_fg: Color::Rgb(127, 132, 156),
            modeline_bg: Color::Rgb(24, 24, 37),
            modeline_fg: Color::Rgb(127, 132, 156),
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

    /// Every selectable theme name: the bundled set first, then the
    /// stems of every `*.toml` under `themes_dir` (sorted, with any
    /// that shadow a bundled name dropped). Drives `:theme list`,
    /// tab-completion, the settings dialog, and the plugin snapshot so
    /// user themes are first-class everywhere a bundled one is.
    #[must_use]
    pub fn available_names(themes_dir: Option<&Path>) -> Vec<String> {
        let mut names: Vec<String> = Self::bundled_names()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        if let Some(dir) = themes_dir
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            let mut user: Vec<String> = entries
                .filter_map(Result::ok)
                .filter_map(|e| {
                    let path = e.path();
                    if path.extension().and_then(|x| x.to_str()) != Some("toml") {
                        return None;
                    }
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .map(str::to_owned)
                        .filter(|n| !names.contains(n))
                })
                .collect();
            user.sort();
            user.dedup();
            names.extend(user);
        }
        names
    }

    /// Override one role by name. Unknown roles error so a typo in a
    /// user theme is reported, not silently ignored.
    fn set_role(&mut self, role: &str, c: Color) -> Result<(), String> {
        macro_rules! roles {
            ($($n:literal => $f:ident),+ $(,)?) => {
                match role {
                    $($n => self.$f = c,)+
                    other => return Err(format!("unknown theme color `{other}`")),
                }
            };
        }
        roles! {
            "bg" => bg, "user_bg" => user_bg, "assistant_rule" => assistant_rule,
            "user_rule" => user_rule, "tool_bg" => tool_bg, "tool_error_bg" => tool_error_bg,
            "tool_pending_bg" => tool_pending_bg, "tool_rule" => tool_rule,
            "tool_error_rule" => tool_error_rule, "tool_pending_rule" => tool_pending_rule,
            "assistant_fg" => assistant_fg, "thinking_fg" => thinking_fg,
            "tool_result_fg" => tool_result_fg, "tool_error_fg" => tool_error_fg,
            "custom_fg" => custom_fg, "status_bg" => status_bg,
            "status_dim_fg" => status_dim_fg, "muted_fg" => muted_fg,
            "match_color" => match_color, "selection_color" => selection_color,
            "focus_color" => focus_color, "input_border_normal" => input_border_normal,
            "input_border_insert" => input_border_insert,
            "input_border_visual" => input_border_visual,
            "input_pill_normal_bg" => input_pill_normal_bg,
            "input_pill_normal_fg" => input_pill_normal_fg,
            "input_pill_insert_bg" => input_pill_insert_bg,
            "input_pill_insert_fg" => input_pill_insert_fg,
            "input_pill_visual_bg" => input_pill_visual_bg,
            "input_pill_visual_fg" => input_pill_visual_fg,
            "input_glyph_fg" => input_glyph_fg,
            "input_placeholder_fg" => input_placeholder_fg,
            "input_hint_fg" => input_hint_fg, "modeline_bg" => modeline_bg,
            "modeline_fg" => modeline_fg,
        }
        Ok(())
    }

    /// Build a theme from a user TOML document: start from the
    /// bundled `base` (default `"default"`), flip `transparent`, then
    /// apply every `[colors]` override.
    ///
    /// # Errors
    ///
    /// Returns a message when the TOML is malformed, a color does not
    /// parse, or a role name is unknown.
    pub fn from_toml(toml: &str) -> Result<Self, String> {
        let file: ThemeFile = toml::from_str(toml).map_err(|e| e.to_string())?;
        let mut theme = Self::by_name(file.base.as_deref().unwrap_or("default"));
        if let Some(t) = file.transparent {
            theme.transparent = t;
        }
        for (role, value) in &file.colors {
            let color = parse_color(value)?;
            theme.set_role(role, color)?;
        }
        Ok(theme)
    }

    /// Resolve a theme name to a palette: a bundled name wins;
    /// otherwise `themes_dir/<name>.toml` is loaded.
    ///
    /// # Errors
    ///
    /// Returns a message when `name` is neither bundled nor a
    /// readable, valid `<name>.toml` under `themes_dir`.
    pub fn resolve(name: &str, themes_dir: Option<&Path>) -> Result<Self, String> {
        if Self::bundled_names().contains(&name) {
            return Ok(Self::by_name(name));
        }
        if let Some(dir) = themes_dir {
            let path = dir.join(format!("{name}.toml"));
            if path.exists() {
                let body = std::fs::read_to_string(&path)
                    .map_err(|e| format!("read {}: {e}", path.display()))?;
                let mut theme =
                    Self::from_toml(&body).map_err(|e| format!("theme `{name}`: {e}"))?;
                name.clone_into(&mut theme.name);
                return Ok(theme);
            }
        }
        Err(format!(
            "unknown theme `{name}` (not bundled and no `{name}.toml`)"
        ))
    }
}

/// A user theme file (`~/.config/kage/themes/<name>.toml`).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    /// Bundled palette to start from. Defaults to `"default"`.
    #[serde(default)]
    base: Option<String>,
    /// Whole-UI opaque (`false`, default) vs let-terminal-through.
    #[serde(default)]
    transparent: Option<bool>,
    /// `role = "#rrggbb"` overrides under `[colors]`.
    #[serde(default)]
    colors: std::collections::BTreeMap<String, String>,
}

/// Parse a color string via ratatui's grammar: `#rrggbb` / `#rgb`
/// hex, a named color (`cyan`), or an indexed number.
fn parse_color(s: &str) -> Result<Color, String> {
    s.trim()
        .parse::<Color>()
        .map_err(|_| format!("invalid color `{s}` (use `#rrggbb`, a name, or an index)"))
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

    #[test]
    fn from_toml_overrides_color_on_chosen_base() {
        let t = Theme::from_toml(
            r##"
            base = "tokyo-night"
            [colors]
            bg = "#010203"
            focus_color = "cyan"
            "##,
        )
        .expect("valid theme");
        assert_eq!(t.bg, Color::Rgb(1, 2, 3));
        assert_eq!(t.focus_color, Color::Cyan);
        assert_eq!(t.assistant_rule, Theme::tokyo_night().assistant_rule);
    }

    #[test]
    fn from_toml_defaults_base_to_default_dark() {
        let t = Theme::from_toml("[colors]\nbg = \"#0a0b0c\"").expect("valid");
        assert_eq!(t.bg, Color::Rgb(10, 11, 12));
        assert_eq!(t.muted_fg, Theme::default_dark().muted_fg);
    }

    #[test]
    fn from_toml_carries_transparent_switch() {
        let t = Theme::from_toml("transparent = true").expect("valid");
        assert!(t.transparent);
        assert!(!Theme::default_dark().transparent);
    }

    #[test]
    fn from_toml_rejects_unknown_role() {
        let err = Theme::from_toml("[colors]\nnope = \"#000000\"").unwrap_err();
        assert!(err.contains("unknown theme color `nope`"), "{err}");
    }

    #[test]
    fn from_toml_rejects_bad_color() {
        let err = Theme::from_toml("[colors]\nbg = \"not-a-color\"").unwrap_err();
        assert!(err.contains("invalid color"), "{err}");
    }

    #[test]
    fn from_toml_rejects_unknown_top_level_key() {
        assert!(Theme::from_toml("wat = 1").is_err());
    }

    #[test]
    fn resolve_bundled_name_skips_disk() {
        let t = Theme::resolve("tokyo-night", None).expect("bundled");
        assert_eq!(t.name, "tokyo-night");
    }

    #[test]
    fn resolve_loads_user_file_and_keeps_name() {
        let dir = std::env::temp_dir().join(format!("kage-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("solar.toml");
        std::fs::write(&path, "[colors]\nbg = \"#102030\"").expect("write");
        let t = Theme::resolve("solar", Some(&dir)).expect("resolved");
        assert_eq!(t.name, "solar");
        assert_eq!(t.bg, Color::Rgb(16, 32, 48));
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn resolve_errors_on_unknown_theme() {
        let err = Theme::resolve("ghost", None).unwrap_err();
        assert!(err.contains("unknown theme `ghost`"), "{err}");
    }

    #[test]
    fn available_names_lists_bundled_then_user_files() {
        let dir = std::env::temp_dir().join(format!("kage-avail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("zenburn.toml"), "").expect("write");
        std::fs::write(dir.join("aurora.toml"), "").expect("write");
        std::fs::write(dir.join("default.toml"), "").expect("write");
        std::fs::write(dir.join("notes.txt"), "").expect("write");
        let names = Theme::available_names(Some(&dir));
        assert_eq!(&names[..3], Theme::bundled_names());
        assert_eq!(&names[3..], ["aurora", "zenburn"]);
        assert_eq!(names.iter().filter(|n| *n == "default").count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn available_names_without_dir_is_just_bundled() {
        assert_eq!(Theme::available_names(None), Theme::bundled_names());
    }
}
