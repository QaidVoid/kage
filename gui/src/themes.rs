//! The themes the client draws: the bundled ones, and the user themes
//! the engine lists from its themes folder, each drawn as its base
//! theme with the `[gui]` table's tokens over it.
//!
//! Every palette is built once per name and kept for the life of the
//! app, so views hold it as `&'static` (see [`Palette::active`]). An
//! edited user theme builds a new one; the old stays, which costs a
//! few kilobytes per edit.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};

use gpui_kit::component::theme::ThemeMode;
use gpui_kit::{Global, Hsla, rgba};
use kage_client::wire::UserTheme;

use crate::theme::Palette;

/// The bundled themes, in the order the settings show them.
pub const BUILTIN: [&str; 4] = ["kage-shadow", "kage-dawn", "kimi-dark", "kimi-light"];

/// What the engine says about themes: the user themes it lists and the
/// themes its config names for a System choice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// The user themes, sorted by name.
    pub user: Vec<UserTheme>,
    /// `[ui] theme_dark`, the theme a System choice draws on a dark
    /// desktop.
    pub dark: Option<String>,
    /// `[ui] theme_light`, the theme a System choice draws on a light
    /// desktop.
    pub light: Option<String>,
}

impl Global for Catalog {}

impl Catalog {
    /// The theme a System choice draws on a light or a dark desktop:
    /// the one the config names, when it is known, else the kage one.
    #[must_use]
    pub fn system(&self, light: bool) -> String {
        let (pick, kage) = if light {
            (&self.light, "kage-dawn")
        } else {
            (&self.dark, "kage-shadow")
        };
        pick.clone()
            .filter(|name| self.knows(name))
            .unwrap_or_else(|| kage.to_owned())
    }

    /// Every theme name, the bundled ones first. A user theme named
    /// like a bundled one is left out, since the bundled one wins.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        BUILTIN
            .iter()
            .map(|name| (*name).to_owned())
            .chain(
                self.user
                    .iter()
                    .filter(|theme| !BUILTIN.contains(&theme.name.as_str()))
                    .map(|theme| theme.name.clone()),
            )
            .collect()
    }

    fn knows(&self, name: &str) -> bool {
        BUILTIN.contains(&name) || self.user.iter().any(|theme| theme.name == name)
    }
}

/// The palette and mode the theme `name` draws with, or `None` for a
/// name neither bundled nor listed. `light` settles the base of a user
/// theme that names none, as `default` does in the terminal.
#[must_use]
pub fn resolve(
    name: &str,
    catalog: &Catalog,
    light: bool,
) -> Option<(&'static Palette, ThemeMode)> {
    if let Some(found) = builtin(name) {
        return Some(found);
    }
    let theme = catalog.user.iter().find(|theme| theme.name == name)?;
    let base = theme
        .base
        .as_deref()
        .filter(|base| BUILTIN.contains(base))
        .unwrap_or(if light { "kage-dawn" } else { "kage-shadow" });
    let (palette, mode) = builtin(base)?;
    Some((custom(theme, base, palette), mode))
}

/// A bundled theme's palette and mode.
#[must_use]
pub fn builtin(name: &str) -> Option<(&'static Palette, ThemeMode)> {
    static SHADOW: LazyLock<Palette> = LazyLock::new(Palette::shadow);
    static DAWN: LazyLock<Palette> = LazyLock::new(Palette::dawn);
    static KIMI_DARK: LazyLock<Palette> = LazyLock::new(Palette::kimi_dark);
    static KIMI_LIGHT: LazyLock<Palette> = LazyLock::new(Palette::kimi_light);
    match name {
        "kage-shadow" => Some((&SHADOW, ThemeMode::Dark)),
        "kage-dawn" => Some((&DAWN, ThemeMode::Light)),
        "kimi-dark" => Some((&KIMI_DARK, ThemeMode::Dark)),
        "kimi-light" => Some((&KIMI_LIGHT, ThemeMode::Light)),
        _ => None,
    }
}

/// The name a theme card shows.
#[must_use]
pub fn label(name: &str) -> String {
    match name {
        "kage-shadow" => "kage shadow",
        "kage-dawn" => "kage dawn",
        "kimi-dark" => "Kimi dark",
        "kimi-light" => "Kimi light",
        other => other,
    }
    .to_owned()
}

/// `theme` drawn over `palette`, its base, built once per content.
fn custom(theme: &UserTheme, base: &str, palette: &Palette) -> &'static Palette {
    static BUILT: LazyLock<Mutex<HashMap<String, &'static Palette>>> =
        LazyLock::new(Mutex::default);
    let key = format!("{}\u{0}{base}\u{0}{:?}", theme.name, theme.gui);
    BUILT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(key)
        .or_insert_with(|| {
            let mut built = palette.clone();
            for (token, value) in &theme.gui {
                if let (Some(slot), Some(color)) =
                    (token_mut(&mut built, token), parse_color(value))
                {
                    *slot = color;
                }
            }
            Box::leak(Box::new(built))
        })
}

/// A `[gui]` color: `#rrggbb`, or `#rrggbbaa` with alpha.
#[must_use]
pub fn parse_color(text: &str) -> Option<Hsla> {
    let hex = text.strip_prefix('#')?;
    let value = u32::from_str_radix(hex, 16).ok()?;
    match hex.len() {
        6 => Some(rgba((value << 8) | 0xFF).into()),
        8 => Some(rgba(value).into()),
        _ => None,
    }
}

macro_rules! tokens {
    ($($name:ident),* $(,)?) => {
        /// Every palette token a theme's `[gui]` table may set.
        pub const TOKENS: &[&str] = &[$(stringify!($name)),*];

        /// The palette slot `name` names.
        fn token_mut<'a>(palette: &'a mut Palette, name: &str) -> Option<&'a mut Hsla> {
            match name {
                $(stringify!($name) => Some(&mut palette.$name),)*
                _ => None,
            }
        }
    };
}

tokens!(
    bg,
    sidebar,
    surface,
    raised,
    sunken,
    deep,
    well,
    fill,
    fill_hover,
    ink,
    ink_strong,
    muted,
    faint,
    ghost,
    line,
    subtle,
    line_strong,
    hover,
    selected,
    selected_hover,
    accent,
    accent_hover,
    accent_soft,
    accent_bd,
    ok,
    ok_soft,
    ok_bd,
    ok_ink,
    warn,
    warn_soft,
    warn_bd,
    danger,
    danger_soft,
    danger_bd,
    done,
    done_soft,
    done_bd,
    info,
    composer_bg,
    composer_line,
    composer_focus_line,
    send_bg,
    send_bg_hover,
    send_icon,
    send_bg_off,
    send_icon_off,
    stop_glyph,
    diff_add,
    diff_add_bg,
    diff_del,
    diff_del_bg,
    bubble,
    selection,
    menu,
    code_inline,
    orb_1,
    orb_2,
);

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn user(name: &str, base: Option<&str>, gui: &[(&str, &str)]) -> UserTheme {
        UserTheme {
            name: name.to_owned(),
            base: base.map(str::to_owned),
            gui: gui
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn a_user_theme_draws_its_tokens_over_its_base() {
        let catalog = Catalog {
            user: vec![user(
                "ember",
                Some("kimi-light"),
                &[("accent", "#fe8019"), ("bogus", "#000000"), ("bg", "nope")],
            )],
            ..Catalog::default()
        };
        let (palette, mode) = resolve("ember", &catalog, false).unwrap();
        assert_eq!(mode, ThemeMode::Light, "the base's mode");
        assert_eq!(palette.accent, parse_color("#fe8019").unwrap());
        assert_eq!(
            palette.bg,
            Palette::kimi_light().bg,
            "a bad color keeps the base"
        );
    }

    #[test]
    fn a_user_theme_without_a_base_follows_the_desktop() {
        let catalog = Catalog {
            user: vec![user("plain", None, &[])],
            ..Catalog::default()
        };
        assert_eq!(
            resolve("plain", &catalog, true).unwrap().1,
            ThemeMode::Light
        );
        assert_eq!(
            resolve("plain", &catalog, false).unwrap().1,
            ThemeMode::Dark
        );
        assert!(resolve("gone", &catalog, false).is_none());
    }

    #[test]
    fn system_draws_the_named_pick_or_the_kage_theme() {
        let mut catalog = Catalog {
            dark: Some("kimi-dark".to_owned()),
            light: Some("missing".to_owned()),
            ..Catalog::default()
        };
        assert_eq!(catalog.system(false), "kimi-dark");
        assert_eq!(
            catalog.system(true),
            "kage-dawn",
            "an unknown pick falls back"
        );
        catalog.user.push(user("kage-dawn", None, &[]));
        assert_eq!(catalog.names().len(), BUILTIN.len(), "bundled names win");
    }

    #[test]
    fn colors_take_six_or_eight_digits() {
        assert_eq!(parse_color("#ffffff").unwrap().a, 1.0);
        assert!((parse_color("#1a88ff47").unwrap().a - 0x47 as f32 / 255.0).abs() < 1e-6);
        assert!(parse_color("ffffff").is_none());
        assert!(parse_color("#fff").is_none());
    }

    #[test]
    fn every_token_names_a_palette_slot() {
        let mut palette = Palette::shadow();
        for token in TOKENS {
            assert!(token_mut(&mut palette, token).is_some(), "{token}");
        }
    }
}
