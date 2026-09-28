//! Whether the terminal has a light background, which picks the
//! variant of the `default` theme (kage dawn on light, kage shadow on
//! dark).

use terminal_colorsaurus::{QueryOptions, ThemeMode};

#[cfg(not(test))]
static LIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// Per thread under test, so a test that flips it never changes the
// theme of a test running beside it.
#[cfg(test)]
thread_local! {
    static LIGHT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Ask the terminal for its background color and remember whether it
/// is light. Falls back to `COLORFGBG`, then to dark.
///
/// Call once at startup, before raw mode and before the event reader
/// starts: the terminal answers on stdin, and the reply must not reach
/// the input handler.
pub fn detect_terminal_background() {
    let light = query().or_else(|| colorfgbg_is_light(std::env::var("COLORFGBG").ok().as_deref()));
    set_terminal_light(light.unwrap_or(false));
}

/// Record whether the terminal background is light.
pub fn set_terminal_light(light: bool) {
    #[cfg(not(test))]
    LIGHT.store(light, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    LIGHT.with(|l| l.set(light));
}

/// Whether the terminal background was detected as light.
#[must_use]
pub fn terminal_light() -> bool {
    #[cfg(not(test))]
    return LIGHT.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    return LIGHT.with(std::cell::Cell::get);
}

fn query() -> Option<bool> {
    // A short timeout: terminals that answer do so in milliseconds,
    // and one that never answers (some multiplexers) must not hold
    // the launch for the library default of a full second. COLORFGBG
    // covers the non-answering case.
    let mut options = QueryOptions::default();
    options.timeout = std::time::Duration::from_millis(120);
    let mode = terminal_colorsaurus::theme_mode(options).ok()?;
    Some(matches!(mode, ThemeMode::Light))
}

/// `COLORFGBG` is `fg;bg` (sometimes `fg;default;bg`) with ANSI
/// indexes; 7 and 15 are the light backgrounds.
fn colorfgbg_is_light(value: Option<&str>) -> Option<bool> {
    let bg: u8 = value?.rsplit(';').next()?.trim().parse().ok()?;
    Some(matches!(bg, 7 | 15))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorfgbg_reads_the_last_field() {
        assert_eq!(colorfgbg_is_light(Some("15;0")), Some(false));
        assert_eq!(colorfgbg_is_light(Some("0;15")), Some(true));
        assert_eq!(colorfgbg_is_light(Some("0;default;7")), Some(true));
        assert_eq!(colorfgbg_is_light(Some("garbage")), None);
        assert_eq!(colorfgbg_is_light(None), None);
    }

    #[test]
    fn light_flag_round_trips() {
        set_terminal_light(true);
        assert!(terminal_light());
        set_terminal_light(false);
        assert!(!terminal_light());
    }
}
