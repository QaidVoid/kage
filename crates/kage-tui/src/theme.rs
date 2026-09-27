//! Theme palette: every color used by the renderer in one place.
//!
//! [`Theme`] is the single source of truth for every color choice the
//! conversation buffer, status bar, and overlay rendering paths make.
//! Two palettes are bundled, kage shadow ([`Theme::kage_shadow`]) and
//! kage dawn ([`Theme::kage_dawn`]). The `default` theme follows the
//! terminal: shadow on a dark background, dawn on a light one. User
//! themes load from TOML files.
//!
//! The renderer reads the active theme via [`current`] (returns a
//! cheap clone of the global). The host process picks one with
//! [`set_current`], typically once at startup. `/theme set <name>`
//! also goes through this path, so a swap takes effect on the next
//! frame without restarting the TUI.
//!
//! Highlight groups sit behind the palette: [`groups_for`] gives a
//! theme's base groups and [`Theme::from_groups`] compiles them back
//! into a [`Theme`].

mod depth;
mod groups;
mod kage;
mod terminal;

use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use kage_core::highlight::Highlights;
use kage_core::sync::{read, write};
use ratatui::style::Color;

pub use depth::ColorDepth;
pub use groups::{ROLE_GROUPS, Slot, ThemeGroups, Themes, groups_for};
pub use terminal::{detect_terminal_background, set_terminal_light, terminal_light};

#[cfg(not(test))]
static CURRENT: RwLock<Option<Arc<Theme>>> = RwLock::new(None);
// Per thread under test, so a test that switches the palette never
// repaints a test running beside it.
#[cfg(test)]
thread_local! {
    static CURRENT: RwLock<Option<Arc<Theme>>> = const { RwLock::new(None) };
}
static DEFAULT: OnceLock<Arc<Theme>> = OnceLock::new();

/// Snapshot of the active theme as a cheap `Arc` clone. Returns the
/// default palette when no host has called [`set_current`] yet, so
/// leaf style helpers don't need to special-case startup ordering.
#[must_use]
pub fn current() -> Arc<Theme> {
    with_current(|current| read(current).clone())
        .unwrap_or_else(|| DEFAULT.get_or_init(|| Arc::new(Theme::default())).clone())
}

/// Replace the process-wide theme. Subsequent renders pick up the
/// new palette; in-flight frames continue with the snapshot they
/// already captured.
pub fn set_current(theme: Theme) {
    with_current(|current| *write(current) = Some(Arc::new(theme)));
}

#[cfg(not(test))]
fn with_current<R>(f: impl FnOnce(&RwLock<Option<Arc<Theme>>>) -> R) -> R {
    f(&CURRENT)
}

#[cfg(test)]
fn with_current<R>(f: impl FnOnce(&RwLock<Option<Arc<Theme>>>) -> R) -> R {
    CURRENT.with(f)
}

/// Drop any host-installed theme so later tests see the default again.
#[cfg(test)]
pub(crate) fn reset_current_for_tests() {
    with_current(|current| *write(current) = None);
}

/// Every color the TUI renderer might paint with. Add entries when a
/// new visual element shows up; never reach for a hardcoded `Color`
/// inside `view.rs`.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Display name (`"default"`, `"kage-dawn"`, a user theme, ...).
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
    /// The highlight table this palette was compiled from. Plugin
    /// spans resolve group names against it. Empty for a palette
    /// built directly.
    pub groups: Highlights,
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
    /// The dark palette of the `default` theme (kage shadow). Used as
    /// the base wherever no terminal detection applies, such as user
    /// theme files and tests.
    #[must_use]
    pub fn default_dark() -> Self {
        Self {
            name: "default".into(),
            ..Self::kage_shadow()
        }
    }

    /// The `default` theme: kage dawn on a light terminal, kage shadow
    /// on a dark one (see [`detect_terminal_background`]).
    #[must_use]
    pub fn default_for_terminal() -> Self {
        let base = if terminal_light() {
            Self::kage_dawn()
        } else {
            Self::kage_shadow()
        };
        Self {
            name: "default".into(),
            ..base
        }
    }

    /// Resolve a theme by name. Unknown names return the default.
    #[must_use]
    pub fn by_name(name: &str) -> Self {
        match name {
            "kage-shadow" => Self::kage_shadow(),
            "kage-dawn" => Self::kage_dawn(),
            _ => Self::default_for_terminal(),
        }
    }

    /// Names of every bundled theme; useful for tab-completion in
    /// `:theme set`. `default` follows the terminal background.
    #[must_use]
    pub fn bundled_names() -> &'static [&'static str] {
        &["default", "kage-shadow", "kage-dawn"]
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
        for name in ["default", "kage-shadow", "kage-dawn"] {
            assert!(names.contains(&name), "{name}");
            assert_eq!(Theme::by_name(name).name, name);
        }
    }

    #[test]
    fn default_follows_the_terminal_background() {
        set_terminal_light(false);
        let dark = Theme::by_name("default");
        assert_eq!(dark.name, "default");
        assert_eq!(dark.bg, Theme::kage_shadow().bg);
        assert!(!dark.bg_is_light());

        set_terminal_light(true);
        let light = Theme::by_name("default");
        assert_eq!(light.name, "default");
        assert_eq!(light.bg, Theme::kage_dawn().bg);
        assert!(light.bg_is_light());
        set_terminal_light(false);
    }

    #[test]
    fn default_dark_is_kage_shadow() {
        let t = Theme::default_dark();
        assert_eq!(t.name, "default");
        assert_eq!(t.bg, Color::Rgb(0x0f, 0x0e, 0x13));
        assert_eq!(t.user_rule, Theme::kage_shadow().user_rule);
    }

    #[test]
    fn from_toml_overrides_color_on_chosen_base() {
        let t = Theme::from_toml(
            r##"
            base = "kage-dawn"
            [colors]
            bg = "#010203"
            focus_color = "cyan"
            "##,
        )
        .expect("valid theme");
        assert_eq!(t.bg, Color::Rgb(1, 2, 3));
        assert_eq!(t.focus_color, Color::Cyan);
        assert_eq!(t.assistant_rule, Theme::kage_dawn().assistant_rule);
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
        let t = Theme::resolve("kage-dawn", None).expect("bundled");
        assert_eq!(t.name, "kage-dawn");
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
        let bundled = Theme::bundled_names().len();
        assert_eq!(&names[..bundled], Theme::bundled_names());
        assert_eq!(&names[bundled..], ["aurora", "zenburn"]);
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
        assert!(!Theme::kage_shadow().bg_is_light());
        assert!(Theme::kage_dawn().bg_is_light());
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
