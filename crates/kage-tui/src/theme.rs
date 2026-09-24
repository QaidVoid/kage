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
//!
//! Highlight groups sit behind the palette: [`groups_for`] gives a
//! theme's base groups and [`Theme::from_groups`] compiles them back
//! into a [`Theme`].

mod groups;

use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use kage_core::sync::{read, write};
use ratatui::style::Color;

pub use groups::{ROLE_GROUPS, Slot, ThemeGroups, groups_for};

static CURRENT: RwLock<Option<Arc<Theme>>> = RwLock::new(None);
static DEFAULT: OnceLock<Arc<Theme>> = OnceLock::new();

/// Snapshot of the active theme as a cheap `Arc` clone. Returns the
/// default palette when no host has called [`set_current`] yet, so
/// leaf style helpers don't need to special-case startup ordering.
#[must_use]
pub fn current() -> Arc<Theme> {
    read(&CURRENT)
        .clone()
        .unwrap_or_else(|| DEFAULT.get_or_init(|| Arc::new(Theme::default())).clone())
}

/// Replace the process-wide theme. Subsequent renders pick up the
/// new palette; in-flight frames continue with the snapshot they
/// already captured.
pub fn set_current(theme: Theme) {
    let mut guard = write(&CURRENT);
    *guard = Some(Arc::new(theme));
}

/// Serialize tests that touch the process-global theme. Tests that
/// `set_current` and tests that assert against [`current`] must hold
/// this lock, or parallel test threads read each other's palette.
#[cfg(test)]
pub(crate) static THEME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Block until the theme global is free for the duration of the test.
/// Poison-safe: a panicking holder must not wedge every later test.
#[cfg(test)]
pub(crate) fn theme_test_lock() -> std::sync::MutexGuard<'static, ()> {
    THEME_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Drop any host-installed theme so later tests see the default again.
#[cfg(test)]
pub(crate) fn reset_current_for_tests() {
    *write(&CURRENT) = None;
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
    /// Foreground for normal text inside overlays and command-line
    /// popups (pickers, confirm dialogs, the external editor).
    pub overlay_fg: Color,
    /// Border color of overlay/dialog cards.
    pub overlay_border: Color,
    /// Background of the selected row (or cursor cell) in overlays.
    pub overlay_selected_bg: Color,
    /// Foreground of the selected row in overlays.
    pub overlay_selected_fg: Color,
    /// Foreground painted on `match_color` / `selection_color` /
    /// `focus_color` backgrounds so highlighted text stays readable.
    pub selection_fg: Color,
    /// Warning accent: destructive-confirm borders, summary headers.
    pub warning_fg: Color,
    /// Markdown H1 foreground.
    pub md_h1_fg: Color,
    /// Markdown H2 foreground.
    pub md_h2_fg: Color,
    /// Markdown link foreground.
    pub md_link_fg: Color,
    /// Markdown inline-code foreground.
    pub md_code_fg: Color,
    /// Affirmative state, e.g. the `*` badge in pickers.
    pub success_fg: Color,
}

/// Expand `$m! { "role" => field, ... }` over every color role a
/// theme TOML or a plugin span can name.
macro_rules! with_roles {
    ($m:ident) => {
        $m! {
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
            "modeline_fg" => modeline_fg, "overlay_fg" => overlay_fg,
            "overlay_border" => overlay_border,
            "overlay_selected_bg" => overlay_selected_bg,
            "overlay_selected_fg" => overlay_selected_fg,
            "selection_fg" => selection_fg, "warning_fg" => warning_fg,
            "md_h1_fg" => md_h1_fg, "md_h2_fg" => md_h2_fg,
            "md_link_fg" => md_link_fg, "md_code_fg" => md_code_fg,
            "success_fg" => success_fg,
        }
    };
}

#[cfg(test)]
macro_rules! role_names {
    ($($n:literal => $f:ident),+ $(,)?) => {
        [$($n),+]
    };
}

/// Every role name, in declaration order.
#[cfg(test)]
const ROLE_NAMES: &[&str] = &with_roles!(role_names);

impl Default for Theme {
    fn default() -> Self {
        Self::default_dark()
    }
}

impl Theme {
    /// Built-in default palette: a quiet slate dark with one soft
    /// steel-blue accent. Deliberately no raw ANSI colors - the
    /// terminal's Cyan/Magenta/Blue are far too saturated to live
    /// in a full-screen UI. Errors stay red; everything else is
    /// tuned to the background.
    #[must_use]
    pub fn default_dark() -> Self {
        Self {
            name: "default".into(),
            transparent: false,
            bg: Color::Rgb(18, 20, 28),
            user_bg: Color::Rgb(36, 42, 58),
            assistant_rule: Color::Rgb(70, 80, 105),
            user_rule: Color::Rgb(110, 160, 210),
            tool_bg: Color::Rgb(30, 34, 44),
            tool_error_bg: Color::Rgb(58, 22, 28),
            tool_pending_bg: Color::Rgb(54, 42, 22),
            tool_rule: Color::Rgb(206, 173, 116),
            tool_error_rule: Color::Red,
            tool_pending_rule: Color::Rgb(204, 153, 0),
            assistant_fg: Color::Rgb(226, 230, 238),
            thinking_fg: Color::Rgb(100, 108, 130),
            tool_result_fg: Color::Rgb(168, 175, 190),
            tool_error_fg: Color::Rgb(228, 108, 108),
            custom_fg: Color::Rgb(140, 148, 165),
            status_bg: Color::Rgb(18, 20, 28),
            status_dim_fg: Color::Rgb(140, 148, 165),
            muted_fg: Color::Rgb(140, 148, 165),
            match_color: Color::Rgb(224, 175, 104),
            selection_color: Color::Rgb(84, 70, 122),
            focus_color: Color::Rgb(226, 230, 238),
            input_border_normal: Color::Rgb(80, 92, 120),
            input_border_insert: Color::Rgb(125, 161, 210),
            input_border_visual: Color::Rgb(167, 142, 214),
            input_pill_normal_bg: Color::Rgb(58, 66, 90),
            input_pill_normal_fg: Color::Rgb(226, 230, 238),
            input_pill_insert_bg: Color::Rgb(96, 136, 182),
            input_pill_insert_fg: Color::Rgb(14, 16, 22),
            input_pill_visual_bg: Color::Rgb(140, 116, 190),
            input_pill_visual_fg: Color::Rgb(14, 16, 22),
            input_glyph_fg: Color::Rgb(110, 150, 196),
            input_placeholder_fg: Color::Rgb(104, 112, 134),
            input_hint_fg: Color::Rgb(140, 148, 165),
            modeline_bg: Color::Rgb(18, 20, 28),
            modeline_fg: Color::Rgb(168, 175, 190),
            overlay_fg: Color::Rgb(226, 230, 238),
            overlay_border: Color::Rgb(88, 102, 138),
            overlay_selected_bg: Color::Rgb(58, 72, 106),
            overlay_selected_fg: Color::Rgb(226, 230, 238),
            selection_fg: Color::Rgb(14, 16, 22),
            warning_fg: Color::Rgb(224, 175, 104),
            md_h1_fg: Color::Rgb(178, 148, 224),
            md_h2_fg: Color::Rgb(120, 170, 216),
            md_link_fg: Color::Rgb(120, 170, 216),
            md_code_fg: Color::Rgb(214, 178, 120),
            success_fg: Color::Rgb(140, 190, 130),
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
            overlay_fg: Color::Rgb(192, 202, 245),
            overlay_border: Color::Rgb(86, 95, 137),
            overlay_selected_bg: Color::Rgb(86, 95, 137),
            overlay_selected_fg: Color::Rgb(192, 202, 245),
            selection_fg: Color::Rgb(26, 27, 38),
            warning_fg: Color::Rgb(224, 175, 104),
            md_h1_fg: Color::Rgb(187, 154, 247),
            md_h2_fg: Color::Rgb(125, 207, 255),
            md_link_fg: Color::Rgb(125, 207, 255),
            md_code_fg: Color::Rgb(224, 175, 104),
            success_fg: Color::Rgb(158, 206, 106),
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
            overlay_fg: Color::Rgb(205, 214, 244),
            overlay_border: Color::Rgb(88, 91, 112),
            overlay_selected_bg: Color::Rgb(88, 91, 112),
            overlay_selected_fg: Color::Rgb(205, 214, 244),
            selection_fg: Color::Rgb(30, 30, 46),
            warning_fg: Color::Rgb(249, 226, 175),
            md_h1_fg: Color::Rgb(203, 166, 247),
            md_h2_fg: Color::Rgb(137, 220, 235),
            md_link_fg: Color::Rgb(137, 220, 235),
            md_code_fg: Color::Rgb(249, 226, 175),
            success_fg: Color::Rgb(166, 227, 161),
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

    /// Look up a color role by name (`"muted_fg"`, `"tool_error_fg"`).
    /// `None` for anything that is not a role.
    #[must_use]
    pub fn role(&self, name: &str) -> Option<Color> {
        macro_rules! get {
            ($($n:literal => $f:ident),+ $(,)?) => {
                match name {
                    $($n => Some(self.$f),)+
                    _ => None,
                }
            };
        }
        with_roles!(get)
    }

    /// Override one role by name. Unknown roles error so a typo in a
    /// user theme is reported, not silently ignored.
    fn set_role(&mut self, role: &str, c: Color) -> Result<(), String> {
        macro_rules! set {
            ($($n:literal => $f:ident),+ $(,)?) => {
                match role {
                    $($n => self.$f = c,)+
                    other => return Err(format!("unknown theme color `{other}`")),
                }
            };
        }
        with_roles!(set);
        Ok(())
    }

    /// Build a theme from a user TOML document: start from the
    /// bundled `base` (default `"default"`), flip `transparent`, apply
    /// every `[colors]` override by role, then every `[groups]` spec.
    ///
    /// # Errors
    ///
    /// Returns a message when the TOML is malformed, a color does not
    /// parse, a role name is unknown, or a `Kage*` group is unknown.
    pub fn from_toml(toml: &str) -> Result<Self, String> {
        let (base, groups) = groups::parse_theme_file(toml)?;
        Ok(Self::from_groups(&groups.into_highlights(&base)))
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
        let groups = groups_for(name, themes_dir)?;
        Ok(Self::from_groups(&groups.into_highlights(name)))
    }

    /// Whether the canvas background reads as light. Used to pair a
    /// syntect highlight theme with the palette so code colors keep
    /// contrast with the background. [`Color::Reset`] counts as dark,
    /// matching the common terminal default.
    #[must_use]
    pub fn bg_is_light(&self) -> bool {
        color_luminance(self.bg) >= 128
    }
}

/// Perceptual luminance of a terminal color on a 0-255 scale
/// (ITU-R BT.601 weights). Named colors use the xterm default
/// palette; indexed colors decode the 256-color cube and grayscale
/// ramp; [`Color::Reset`] maps to black.
fn color_luminance(c: Color) -> u32 {
    let (r, g, b) = match c {
        Color::Reset => (0, 0, 0),
        Color::Rgb(r, g, b) => (u32::from(r), u32::from(g), u32::from(b)),
        Color::Black => ansi_rgb(0),
        Color::Red => ansi_rgb(1),
        Color::Green => ansi_rgb(2),
        Color::Yellow => ansi_rgb(3),
        Color::Blue => ansi_rgb(4),
        Color::Magenta => ansi_rgb(5),
        Color::Cyan => ansi_rgb(6),
        Color::Gray => ansi_rgb(7),
        Color::DarkGray => ansi_rgb(8),
        Color::LightRed => ansi_rgb(9),
        Color::LightGreen => ansi_rgb(10),
        Color::LightYellow => ansi_rgb(11),
        Color::LightBlue => ansi_rgb(12),
        Color::LightMagenta => ansi_rgb(13),
        Color::LightCyan => ansi_rgb(14),
        Color::White => ansi_rgb(15),
        Color::Indexed(i) if i < 16 => ansi_rgb(i),
        Color::Indexed(i) if i < 232 => {
            let i = u32::from(i - 16);
            let level = |n: u32| [0, 95, 135, 175, 215, 255][n as usize];
            (level(i / 36), level((i % 36) / 6), level(i % 6))
        }
        Color::Indexed(i) => {
            let v = u32::from(8 + 10 * (i - 232));
            (v, v, v)
        }
    };
    (2126 * r + 7152 * g + 722 * b) / 10_000
}

/// Approximate RGB of an ANSI base color under the xterm defaults.
/// The terminal's real palette varies, but only the light/dark
/// classification of the result matters here.
fn ansi_rgb(i: u8) -> (u32, u32, u32) {
    match i {
        0 => (0, 0, 0),
        1 => (205, 0, 0),
        2 => (0, 205, 0),
        3 => (205, 205, 0),
        4 => (0, 0, 238),
        5 => (205, 0, 205),
        6 => (0, 205, 205),
        7 => (229, 229, 229),
        8 => (127, 127, 127),
        9 => (255, 95, 95),
        10 => (95, 255, 95),
        11 => (255, 255, 95),
        12 => (95, 95, 255),
        13 => (255, 95, 255),
        14 => (95, 255, 255),
        _ => (255, 255, 255),
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
    fn role_reads_what_set_role_writes() {
        let mut t = Theme::default();
        t.set_role("muted_fg", Color::Rgb(9, 8, 7)).unwrap();
        assert_eq!(t.role("muted_fg"), Some(Color::Rgb(9, 8, 7)));
        assert_eq!(t.role("tool_error_fg"), Some(t.tool_error_fg));
        assert_eq!(t.role("red"), None);
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

    #[test]
    fn bg_is_light_classifies_representative_colors() {
        assert!(!Theme::default_dark().bg_is_light());
        assert!(!Theme::tokyo_night().bg_is_light());
        assert!(!Theme::catppuccin_mocha().bg_is_light());
        let with = |bg| Theme {
            bg,
            ..Theme::default_dark()
        };
        assert!(with(Color::White).bg_is_light());
        assert!(with(Color::Gray).bg_is_light());
        assert!(!with(Color::Reset).bg_is_light());
        assert!(!with(Color::Black).bg_is_light());
        // Indexed 196 decodes to #ff0000 (dark); 243/244 straddle the
        // 128 luminance threshold on the grayscale ramp.
        assert!(!with(Color::Indexed(196)).bg_is_light());
        assert!(!with(Color::Indexed(243)).bg_is_light());
        assert!(with(Color::Indexed(244)).bg_is_light());
        assert!(with(Color::Indexed(250)).bg_is_light());
    }
}
