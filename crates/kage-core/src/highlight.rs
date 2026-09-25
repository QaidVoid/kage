//! Highlight groups: named styles with links, shared by the TUI and the
//! plugin runtime.
//!
//! A theme provides the base groups. Lua overrides them or adds its own
//! with [`Highlights::set`]. Switching theme replaces the base and drops
//! overrides of `Kage*` groups, while other groups persist. Colors stay
//! strings here and are checked by [`parse_color`]; the TUI turns them
//! into terminal colors.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Links followed before resolution gives up with an empty style.
const MAX_LINK_DEPTH: usize = 16;

/// The 16 color names [`parse_color`] accepts, in ANSI order.
pub const COLOR_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "gray",
    "darkgray",
    "lightred",
    "lightgreen",
    "lightyellow",
    "lightblue",
    "lightmagenta",
    "lightcyan",
    "white",
];

/// A parsed color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// `#rrggbb`.
    Rgb(u8, u8, u8),
    /// One of [`COLOR_NAMES`], by index.
    Ansi(u8),
    /// A 256-color palette index.
    Indexed(u8),
}

/// Parse `#rrggbb`, one of [`COLOR_NAMES`] (case, spaces, dashes and
/// underscores ignored), or a palette index `0` to `255`.
pub fn parse_color(s: &str) -> Result<Color, HighlightError> {
    let s = s.trim();
    let invalid = || HighlightError::Color(s.to_owned());
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| invalid());
        return Ok(Color::Rgb(channel(0)?, channel(2)?, channel(4)?));
    }
    let name = s.to_ascii_lowercase().replace([' ', '-', '_'], "");
    if let Some((index, _)) = (0u8..).zip(COLOR_NAMES).find(|(_, known)| *known == name) {
        return Ok(Color::Ansi(index));
    }
    s.parse().map(Color::Indexed).map_err(|_| invalid())
}

/// A highlight group spec. When `link` is set it wins over every other
/// field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors the independent style flags of a highlight group"
)]
pub struct HlSpec {
    /// Foreground color.
    pub fg: Option<String>,
    /// Background color.
    pub bg: Option<String>,
    /// Bold text.
    pub bold: bool,
    /// Italic text.
    pub italic: bool,
    /// Underlined text.
    pub underline: bool,
    /// Dim text.
    pub dim: bool,
    /// Swapped foreground and background.
    pub reverse: bool,
    /// Name of the group this one follows.
    pub link: Option<String>,
}

impl HlSpec {
    /// Check that `fg` and `bg` parse as colors.
    pub fn validate(&self) -> Result<(), HighlightError> {
        for color in [&self.fg, &self.bg].into_iter().flatten() {
            parse_color(color)?;
        }
        Ok(())
    }
}

/// The highlight table: theme base groups plus overrides.
#[derive(Clone, Debug, Default)]
pub struct Highlights {
    base: BTreeMap<String, HlSpec>,
    overrides: BTreeMap<String, HlSpec>,
    theme: String,
    transparent: bool,
    generation: u64,
}

impl Highlights {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the base groups with those of `theme`, drop overrides of
    /// `Kage*` groups, and keep every other override.
    pub fn set_base(&mut self, theme: String, transparent: bool, groups: BTreeMap<String, HlSpec>) {
        self.base = groups;
        self.overrides.retain(|name, _| !name.starts_with("Kage"));
        self.theme = theme;
        self.transparent = transparent;
        self.generation += 1;
    }

    /// Set or replace the override for `name`.
    pub fn set(&mut self, name: &str, spec: HlSpec) -> Result<(), HighlightError> {
        if name.is_empty() {
            return Err(HighlightError::EmptyName);
        }
        spec.validate()?;
        self.overrides.insert(name.to_owned(), spec);
        self.generation += 1;
        Ok(())
    }

    /// The spec for `name` as set, without following links. An override
    /// shadows the base group.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&HlSpec> {
        self.overrides.get(name).or_else(|| self.base.get(name))
    }

    /// The spec for `name` with links followed. A missing group, a
    /// cycle, or a chain deeper than 16 links gives an empty style.
    #[must_use]
    pub fn resolve(&self, name: &str) -> HlSpec {
        let mut name = name;
        for _ in 0..=MAX_LINK_DEPTH {
            let Some(spec) = self.get(name) else {
                break;
            };
            match &spec.link {
                Some(next) => name = next,
                None => return spec.clone(),
            }
        }
        HlSpec::default()
    }

    /// Name of the theme the base groups came from.
    #[must_use]
    pub fn theme(&self) -> &str {
        &self.theme
    }

    /// Whether the theme leaves the terminal background showing.
    #[must_use]
    pub fn transparent(&self) -> bool {
        self.transparent
    }

    /// Counter bumped on every change.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Errors from colors and highlight groups.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HighlightError {
    /// The color does not parse.
    #[error("invalid color `{0}` (use `#rrggbb`, a color name, or 0 to 255)")]
    Color(String),
    /// The group name is empty.
    #[error("highlight group name must not be empty")]
    EmptyName,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fg(color: &str) -> HlSpec {
        HlSpec {
            fg: Some(color.to_owned()),
            ..HlSpec::default()
        }
    }

    fn link(to: &str) -> HlSpec {
        HlSpec {
            link: Some(to.to_owned()),
            ..HlSpec::default()
        }
    }

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#0a1B2c"), Ok(Color::Rgb(10, 27, 44)));
        assert_eq!(parse_color("red"), Ok(Color::Ansi(1)));
        assert_eq!(parse_color("Light-Blue"), Ok(Color::Ansi(12)));
        assert_eq!(parse_color("dark_gray"), Ok(Color::Ansi(8)));
        assert_eq!(parse_color("white"), Ok(Color::Ansi(15)));
        assert_eq!(parse_color("0"), Ok(Color::Indexed(0)));
        assert_eq!(parse_color("255"), Ok(Color::Indexed(255)));
        for bad in [
            "", "#abc", "#gggggg", "#1234567", "256", "-1", "reset", "teal",
        ] {
            assert_eq!(
                parse_color(bad),
                Err(HighlightError::Color(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn set_validates_colors_and_name() {
        let mut hl = Highlights::new();
        assert!(hl.set("Mine", fg("#112233")).is_ok());
        assert!(matches!(
            hl.set("Mine", fg("nope")),
            Err(HighlightError::Color(_))
        ));
        assert_eq!(hl.set("", fg("red")), Err(HighlightError::EmptyName));
        assert_eq!(hl.get("Mine"), Some(&fg("#112233")));
    }

    #[test]
    fn links_resolve_and_win_over_fields() {
        let mut hl = Highlights::new();
        hl.set_base(
            "default".to_owned(),
            false,
            BTreeMap::from([("KageMuted".to_owned(), fg("#808080"))]),
        );
        hl.set("A", link("B")).unwrap();
        hl.set(
            "B",
            HlSpec {
                bold: true,
                ..link("KageMuted")
            },
        )
        .unwrap();
        assert_eq!(hl.resolve("A"), fg("#808080"));
        assert_eq!(hl.get("A"), Some(&link("B")));
        assert_eq!(hl.resolve("Missing"), HlSpec::default());
        hl.set("Dangling", link("Missing")).unwrap();
        assert_eq!(hl.resolve("Dangling"), HlSpec::default());
    }

    #[test]
    fn cycles_and_deep_chains_resolve_empty() {
        let mut hl = Highlights::new();
        hl.set("A", link("B")).unwrap();
        hl.set("B", link("A")).unwrap();
        hl.set("Self", link("Self")).unwrap();
        assert_eq!(hl.resolve("A"), HlSpec::default());
        assert_eq!(hl.resolve("Self"), HlSpec::default());

        hl.set("G16", fg("red")).unwrap();
        for i in 0..16 {
            hl.set(&format!("G{i}"), link(&format!("G{}", i + 1)))
                .unwrap();
        }
        assert_eq!(hl.resolve("G0"), fg("red"));
        hl.set("Deeper", link("G0")).unwrap();
        assert_eq!(hl.resolve("Deeper"), HlSpec::default());
    }

    #[test]
    fn set_base_drops_kage_overrides_only() {
        let mut hl = Highlights::new();
        hl.set_base(
            "default".to_owned(),
            false,
            BTreeMap::from([("KageMuted".to_owned(), fg("#808080"))]),
        );
        hl.set("KageMuted", fg("#ffffff")).unwrap();
        hl.set("Mine", link("KageMuted")).unwrap();
        assert_eq!(hl.resolve("Mine"), fg("#ffffff"));

        let generation = hl.generation();
        hl.set_base(
            "tokyo-night".to_owned(),
            true,
            BTreeMap::from([("KageMuted".to_owned(), fg("#565f89"))]),
        );
        assert!(hl.generation() > generation);
        assert_eq!(hl.theme(), "tokyo-night");
        assert!(hl.transparent());
        assert_eq!(hl.get("KageMuted"), Some(&fg("#565f89")));
        assert_eq!(hl.resolve("Mine"), fg("#565f89"));
    }

    #[test]
    fn spec_deserializes_from_toml() {
        let groups: BTreeMap<String, HlSpec> = toml::from_str(
            r##"
            KageUser = { fg = "#ffffff", bold = true }
            KageHint = { link = "KageMuted" }
            "##,
        )
        .unwrap();
        assert_eq!(
            groups["KageUser"],
            HlSpec {
                bold: true,
                ..fg("#ffffff")
            }
        );
        assert_eq!(groups["KageHint"], link("KageMuted"));
        assert!(toml::from_str::<HlSpec>("colour = \"red\"").is_err());
    }
}
