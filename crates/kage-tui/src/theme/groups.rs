//! Highlight groups behind the theme palette.
//!
//! Every theme role lives in exactly one `Kage*` group, as its `fg` or
//! its `bg` ([`ROLE_GROUPS`]). A theme provides base groups
//! ([`groups_for`]) and [`Theme::from_groups`] compiles a highlight
//! table back into the [`Theme`] struct the renderer reads.
//!
//! [`Themes`] hands the same base groups to the plugin runtime, which
//! owns the live table. Plugin spans resolve group names at paint time
//! through [`Theme::group_style`] and [`Theme::span_color`].
//!
//! Colors in a group are kage-core color strings (`#rrggbb`, one of the
//! 16 names, or `0` to `255`). An absent color compiles to
//! [`Color::Reset`], the terminal default, which is also what `reset`
//! under `[colors]` turns into.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kage_core::highlight::{self, COLOR_NAMES, Highlights, HlSpec};
use ratatui::style::{Color, Modifier, Style};

use super::Theme;

/// Which color of a group a theme role reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    /// The group's foreground.
    Fg,
    /// The group's background.
    Bg,
}

/// Every theme role with the group and slot it lives in, as
/// `(role, group, slot)`. Roles and `(group, slot)` pairs are both
/// unique.
pub const ROLE_GROUPS: [(&str, &str, Slot); 46] = [
    ("bg", "KageNormal", Slot::Bg),
    ("assistant_fg", "KageNormal", Slot::Fg),
    ("user_bg", "KageUserBubble", Slot::Bg),
    ("user_rule", "KageUserRule", Slot::Fg),
    ("assistant_rule", "KageAssistantRule", Slot::Fg),
    ("thinking_fg", "KageThinking", Slot::Fg),
    ("tool_bg", "KageTool", Slot::Bg),
    ("tool_result_fg", "KageTool", Slot::Fg),
    ("tool_error_bg", "KageToolError", Slot::Bg),
    ("tool_error_fg", "KageToolError", Slot::Fg),
    ("tool_pending_bg", "KageToolPending", Slot::Bg),
    ("tool_rule", "KageToolRule", Slot::Fg),
    ("tool_error_rule", "KageToolErrorRule", Slot::Fg),
    ("tool_pending_rule", "KageToolPendingRule", Slot::Fg),
    ("custom_fg", "KageCustom", Slot::Fg),
    ("status_bg", "KageStatus", Slot::Bg),
    ("status_dim_fg", "KageStatus", Slot::Fg),
    ("muted_fg", "KageMuted", Slot::Fg),
    ("match_color", "KageMatch", Slot::Bg),
    ("selection_color", "KageSelection", Slot::Bg),
    ("selection_fg", "KageSelection", Slot::Fg),
    ("focus_color", "KageFocus", Slot::Bg),
    ("input_border_normal", "KageInputBorderNormal", Slot::Fg),
    ("input_border_insert", "KageInputBorderInsert", Slot::Fg),
    ("input_border_visual", "KageInputBorderVisual", Slot::Fg),
    ("input_pill_normal_bg", "KageInputPillNormal", Slot::Bg),
    ("input_pill_normal_fg", "KageInputPillNormal", Slot::Fg),
    ("input_pill_insert_bg", "KageInputPillInsert", Slot::Bg),
    ("input_pill_insert_fg", "KageInputPillInsert", Slot::Fg),
    ("input_pill_visual_bg", "KageInputPillVisual", Slot::Bg),
    ("input_pill_visual_fg", "KageInputPillVisual", Slot::Fg),
    ("input_glyph_fg", "KageInputGlyph", Slot::Fg),
    ("input_placeholder_fg", "KageInputPlaceholder", Slot::Fg),
    ("input_hint_fg", "KageInputHint", Slot::Fg),
    ("modeline_bg", "KageModeline", Slot::Bg),
    ("modeline_fg", "KageModeline", Slot::Fg),
    ("overlay_fg", "KageOverlay", Slot::Fg),
    ("overlay_border", "KageOverlayBorder", Slot::Fg),
    ("overlay_selected_bg", "KageOverlaySelected", Slot::Bg),
    ("overlay_selected_fg", "KageOverlaySelected", Slot::Fg),
    ("warning_fg", "KageWarning", Slot::Fg),
    ("success_fg", "KageSuccess", Slot::Fg),
    ("md_h1_fg", "KageMarkdownH1", Slot::Fg),
    ("md_h2_fg", "KageMarkdownH2", Slot::Fg),
    ("md_link_fg", "KageMarkdownLink", Slot::Fg),
    ("md_code_fg", "KageMarkdownCode", Slot::Fg),
];

/// The base groups of one theme, ready for [`Highlights::set_base`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThemeGroups {
    /// Whether the theme leaves the terminal background showing.
    pub transparent: bool,
    /// Group specs by name.
    pub groups: BTreeMap<String, HlSpec>,
}

impl ThemeGroups {
    /// The groups that compile back to exactly `theme`.
    #[must_use]
    pub fn from_theme(theme: &Theme) -> Self {
        let mut groups = BTreeMap::<String, HlSpec>::new();
        for (role, group, slot) in ROLE_GROUPS {
            let color = theme.role(role).and_then(color_string);
            *slot_mut(groups.entry(group.to_owned()).or_default(), slot) = color;
        }
        Self {
            transparent: theme.transparent,
            groups,
        }
    }

    /// A highlight table with these groups as the base of theme `name`.
    #[must_use]
    pub fn into_highlights(self, name: &str) -> Highlights {
        let mut hl = Highlights::new();
        hl.set_base(name.to_owned(), self.transparent, self.groups);
        hl
    }
}

/// Base groups for a theme name: a bundled name wins, otherwise
/// `themes_dir/<name>.toml` is loaded.
///
/// # Errors
///
/// Returns a message when `name` is neither bundled nor a readable,
/// valid `<name>.toml` under `themes_dir`.
pub fn groups_for(name: &str, themes_dir: Option<&Path>) -> Result<ThemeGroups, String> {
    if Theme::bundled_names().contains(&name) {
        return Ok(ThemeGroups::from_theme(&Theme::by_name(name)));
    }
    if let Some(dir) = themes_dir {
        let path = dir.join(format!("{name}.toml"));
        if path.exists() {
            let body = std::fs::read_to_string(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            return parse_theme_file(&body)
                .map(|(_, groups)| groups)
                .map_err(|e| format!("theme `{name}`: {e}"));
        }
    }
    Err(format!(
        "unknown theme `{name}` (not bundled and no `{name}.toml`)"
    ))
}

/// The theme registry the plugin runtime resolves the `theme` option
/// against: the bundled themes plus `<name>.toml` files in a themes
/// directory.
#[derive(Clone, Debug, Default)]
pub struct Themes {
    dir: Option<PathBuf>,
}

impl Themes {
    /// Themes from the bundled set and, when given, `dir`.
    #[must_use]
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir }
    }
}

impl kage_plugin::ThemeResolver for Themes {
    fn names(&self) -> Vec<String> {
        Theme::available_names(self.dir.as_deref())
    }

    fn groups(&self, name: &str) -> Result<kage_plugin::ThemeBase, String> {
        let groups = groups_for(name, self.dir.as_deref())?;
        Ok(kage_plugin::ThemeBase {
            transparent: groups.transparent,
            groups: groups.groups,
        })
    }
}

impl Theme {
    /// Compile a highlight table into the renderer palette. Each role
    /// reads its slot of its resolved group; a missing color becomes
    /// [`Color::Reset`]. The table is kept in [`Theme::groups`].
    #[must_use]
    pub fn from_groups(hl: &Highlights) -> Self {
        let mut theme = Self {
            name: hl.theme().to_owned(),
            transparent: hl.transparent(),
            groups: hl.clone(),
            ..Self::default_dark()
        };
        for (role, group, slot) in ROLE_GROUPS {
            let spec = hl.resolve(group);
            let color = match slot {
                Slot::Fg => spec.fg,
                Slot::Bg => spec.bg,
            };
            let set = theme.set_role(role, color.as_deref().map_or(Color::Reset, to_color));
            debug_assert!(set.is_ok(), "{role} is not a theme role");
        }
        theme
    }

    /// The style of group `name` with links followed: the colors it
    /// sets and its attributes. An unknown group gives the empty style.
    #[must_use]
    pub fn group_style(&self, name: &str) -> Style {
        let spec = self.groups.resolve(name);
        let mut style = Style::default();
        if let Some(fg) = &spec.fg {
            style = style.fg(to_color(fg));
        }
        if let Some(bg) = &spec.bg {
            style = style.bg(to_color(bg));
        }
        for (on, modifier) in [
            (spec.bold, Modifier::BOLD),
            (spec.italic, Modifier::ITALIC),
            (spec.underline, Modifier::UNDERLINED),
            (spec.dim, Modifier::DIM),
            (spec.reverse, Modifier::REVERSED),
        ] {
            if on {
                style = style.add_modifier(modifier);
            }
        }
        style
    }

    /// Resolve a plugin span color: a group name gives the group's
    /// `slot` color, then a theme role name, then ratatui's color
    /// grammar (`red`, `#rrggbb`, an index). `None` when nothing
    /// matches or the group leaves that color unset.
    #[must_use]
    pub fn span_color(&self, name: &str, slot: Slot) -> Option<Color> {
        if self.groups.get(name).is_some() {
            let spec = self.groups.resolve(name);
            let color = match slot {
                Slot::Fg => spec.fg,
                Slot::Bg => spec.bg,
            };
            return color.as_deref().map(to_color);
        }
        self.role(name).or_else(|| name.parse::<Color>().ok())
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
    colors: BTreeMap<String, String>,
    /// Whole group specs or links under `[groups]`, applied after
    /// `[colors]`. Each entry replaces its group.
    #[serde(default)]
    groups: BTreeMap<String, HlSpec>,
}

/// Parse a user theme document into its base theme name and groups:
/// the bundled `base` groups, then `transparent`, then `[colors]` by
/// role, then `[groups]`.
pub(super) fn parse_theme_file(toml: &str) -> Result<(String, ThemeGroups), String> {
    let file: ThemeFile = toml::from_str(toml).map_err(|e| e.to_string())?;
    let base = Theme::by_name(file.base.as_deref().unwrap_or("default"));
    let mut out = ThemeGroups::from_theme(&base);
    if let Some(t) = file.transparent {
        out.transparent = t;
    }
    for (role, value) in &file.colors {
        let Some((_, group, slot)) = ROLE_GROUPS.iter().find(|(r, _, _)| r == role) else {
            return Err(format!("unknown theme color `{role}`"));
        };
        let color = color_string(parse_color(value)?);
        *slot_mut(out.groups.entry((*group).to_owned()).or_default(), *slot) = color;
    }
    for (name, spec) in file.groups {
        if name.starts_with("Kage") && !ROLE_GROUPS.iter().any(|(_, g, _)| *g == name) {
            return Err(format!("unknown highlight group `{name}`"));
        }
        spec.validate()
            .map_err(|e| format!("group `{name}`: {e}"))?;
        out.groups.insert(name, spec);
    }
    Ok((base.name, out))
}

/// Parse a `[colors]` value via ratatui's grammar: `#rrggbb` hex, a
/// named color (`cyan`, `bright-red`, `reset`), or an indexed number.
fn parse_color(s: &str) -> Result<Color, String> {
    s.trim()
        .parse::<Color>()
        .map_err(|_| format!("invalid color `{s}` (use `#rrggbb`, a name, or an index)"))
}

fn slot_mut(spec: &mut HlSpec, slot: Slot) -> &mut Option<String> {
    match slot {
        Slot::Fg => &mut spec.fg,
        Slot::Bg => &mut spec.bg,
    }
}

/// The kage-core string for a terminal color. [`Color::Reset`] has none.
fn color_string(c: Color) -> Option<String> {
    let name = |i: usize| Some(COLOR_NAMES[i].to_owned());
    match c {
        Color::Reset => None,
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Indexed(i) => Some(i.to_string()),
        Color::Black => name(0),
        Color::Red => name(1),
        Color::Green => name(2),
        Color::Yellow => name(3),
        Color::Blue => name(4),
        Color::Magenta => name(5),
        Color::Cyan => name(6),
        Color::Gray => name(7),
        Color::DarkGray => name(8),
        Color::LightRed => name(9),
        Color::LightGreen => name(10),
        Color::LightYellow => name(11),
        Color::LightBlue => name(12),
        Color::LightMagenta => name(13),
        Color::LightCyan => name(14),
        Color::White => name(15),
    }
}

/// The terminal color for a kage-core color string. Invalid strings,
/// which validation keeps out of the table, become [`Color::Reset`].
fn to_color(s: &str) -> Color {
    const ANSI: [Color; 16] = [
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
    match highlight::parse_color(s) {
        Ok(highlight::Color::Rgb(r, g, b)) => Color::Rgb(r, g, b),
        Ok(highlight::Color::Ansi(i)) => ANSI.get(usize::from(i)).copied().unwrap_or(Color::Reset),
        Ok(highlight::Color::Indexed(i)) => Color::Indexed(i),
        Err(_) => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn assert_same_palette(got: &Theme, want: &Theme) {
        assert_eq!(got.name, want.name);
        assert_eq!(got.transparent, want.transparent);
        for role in super::super::ROLE_NAMES {
            assert_eq!(got.role(role), want.role(role), "{} {role}", want.name);
        }
    }

    #[test]
    fn alias_table_is_a_bijection_with_the_roles() {
        let roles: BTreeSet<&str> = ROLE_GROUPS.iter().map(|(r, _, _)| *r).collect();
        let pairs: BTreeSet<(&str, Slot)> = ROLE_GROUPS.iter().map(|(_, g, s)| (*g, *s)).collect();
        let want: BTreeSet<&str> = super::super::ROLE_NAMES.iter().copied().collect();
        assert_eq!(roles, want);
        assert_eq!(roles.len(), ROLE_GROUPS.len());
        assert_eq!(pairs.len(), ROLE_GROUPS.len());
        assert_eq!(super::super::ROLE_NAMES.len(), ROLE_GROUPS.len());
        assert!(ROLE_GROUPS.iter().all(|(_, g, _)| g.starts_with("Kage")));
    }

    #[test]
    fn bundled_themes_compile_to_their_palettes() {
        for name in Theme::bundled_names() {
            let hl = groups_for(name, None)
                .expect("bundled")
                .into_highlights(name);
            assert_same_palette(&Theme::from_groups(&hl), &Theme::by_name(name));
        }
    }

    #[test]
    fn colors_round_trip_through_group_strings() {
        let mut want = Theme::default_dark();
        want.bg = Color::Reset;
        want.muted_fg = Color::Indexed(244);
        want.focus_color = Color::White;
        want.warning_fg = Color::DarkGray;
        let hl = ThemeGroups::from_theme(&want).into_highlights("default");
        assert_eq!(hl.get("KageNormal").and_then(|s| s.bg.clone()), None);
        assert_same_palette(&Theme::from_groups(&hl), &want);
    }

    #[test]
    fn user_theme_with_colors_and_groups_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("dusk.toml"),
            r##"
            base = "tokyo-night"
            transparent = true
            [colors]
            muted_fg = "bright-red"
            tool_error_bg = "reset"
            [groups]
            KageWarning = { fg = "#102030", bold = true }
            KageMarkdownLink = { link = "KageWarning" }
            KageInputHint = { link = "KageMuted" }
            KageMuted = { fg = "#0a0b0c" }
            MyPlugin = { fg = "cyan" }
            "##,
        )
        .expect("write");
        let groups = groups_for("dusk", Some(dir.path())).expect("loads");
        assert!(groups.transparent);
        assert!(groups.groups["KageWarning"].bold);
        assert_eq!(groups.groups["MyPlugin"].fg.as_deref(), Some("cyan"));

        let t = Theme::from_groups(&groups.into_highlights("dusk"));
        let base = Theme::tokyo_night();
        assert_eq!(t.name, "dusk");
        assert!(t.transparent);
        assert_eq!(t.tool_error_bg, Color::Reset);
        assert_eq!(t.warning_fg, Color::Rgb(16, 32, 48));
        assert_eq!(t.md_link_fg, Color::Rgb(16, 32, 48));
        assert_eq!(t.muted_fg, Color::Rgb(10, 11, 12));
        assert_eq!(t.input_hint_fg, Color::Rgb(10, 11, 12));
        assert_eq!(t.tool_error_fg, base.tool_error_fg);
        assert_eq!(t.assistant_rule, base.assistant_rule);

        let resolved = Theme::resolve("dusk", Some(dir.path())).expect("resolves");
        assert_same_palette(&resolved, &t);
    }

    #[test]
    fn colors_role_uses_its_group_slot() {
        let (base, groups) = parse_theme_file("[colors]\nselection_fg = \"#010203\"").expect("ok");
        assert_eq!(base, "default");
        let spec = &groups.groups["KageSelection"];
        assert_eq!(spec.fg.as_deref(), Some("#010203"));
        assert!(spec.bg.is_some());
    }

    #[test]
    fn unknown_kage_group_errors() {
        let err = parse_theme_file("[groups]\nKageNope = { fg = \"red\" }").unwrap_err();
        assert!(err.contains("unknown highlight group `KageNope`"), "{err}");
    }

    #[test]
    fn invalid_group_color_or_field_errors() {
        let err = parse_theme_file("[groups]\nKageMuted = { fg = \"reset\" }").unwrap_err();
        assert!(err.contains("group `KageMuted`"), "{err}");
        assert!(parse_theme_file("[groups]\nKageMuted = { colour = \"red\" }").is_err());
    }
}
