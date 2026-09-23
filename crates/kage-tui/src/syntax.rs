//! Syntect-backed syntax highlighting for the TUI.
//!
//! [`highlight_extension`] renders an entire blob (typically a `read`
//! tool result) using a syntax inferred from the file extension, and
//! [`highlight_with_lang`] renders a fenced body whose info string
//! names the language; [`crate::markdown::render`] uses it for
//! ```` ```lang ... ``` ```` fences. [`plain_lines_styled`] is the
//! shared plain fallback.
//!
//! They share a single global [`SyntaxSet`] / [`ThemeSet`] loaded once
//! via [`std::sync::OnceLock`] - syntect's default loaders take ~10ms
//! and bring in ~150 syntaxes, so we deliberately avoid re-init per
//! call. The syntect highlight theme is paired with the active kage
//! theme by background luminance, and the per-thread cache keys on
//! that pairing so a theme switch re-highlights instead of serving
//! stale colors.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

/// Cache size cap. Holds up to this many distinct highlight results;
/// older entries get evicted in FIFO order. Sized for "a session's
/// worth of assistant blocks plus their tool reads", not unbounded.
const CACHE_CAP: usize = 64;

/// Skip syntect entirely for inputs above this many bytes. Even with
/// the result cache, the first render of a huge file would block the
/// UI for a noticeable beat; over this threshold we just emit plain
/// styled lines (still readable, just not highlighted). Sized to keep
/// worst-case syntect work under a few milliseconds on a typical
/// development machine.
const HIGHLIGHT_BYTE_LIMIT: usize = 64 * 1024;

thread_local! {
    /// Per-thread cache of highlight results keyed by a 64-bit hash
    /// of `(text, marker, paired syntect theme)` where `marker`
    /// distinguishes fenced-text vs extension-keyed renders. Rendering
    /// happens on the main thread so a `RefCell` is sufficient; we
    /// deliberately avoid a Mutex to keep the per-frame cost minimal.
    static HIGHLIGHT_CACHE: RefCell<HighlightCache> = RefCell::new(HighlightCache::new());
}

struct HighlightCache {
    entries: std::collections::HashMap<u64, Vec<Line<'static>>>,
    order: VecDeque<u64>,
}

impl HighlightCache {
    fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: u64) -> Option<Vec<Line<'static>>> {
        self.entries.get(&key).cloned()
    }

    fn insert(&mut self, key: u64, lines: Vec<Line<'static>>) {
        if self.entries.contains_key(&key) {
            return;
        }
        while self.order.len() >= CACHE_CAP {
            if let Some(stale) = self.order.pop_front() {
                self.entries.remove(&stale);
            }
        }
        self.order.push_back(key);
        self.entries.insert(key, lines);
    }
}

fn cache_key(text: &str, marker: &str, theme_name: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    marker.hash(&mut h);
    theme_name.hash(&mut h);
    h.finish()
}

fn cached_or<F>(key: u64, build: F) -> Vec<Line<'static>>
where
    F: FnOnce() -> Vec<Line<'static>>,
{
    HIGHLIGHT_CACHE.with(|c| {
        if let Some(hit) = c.borrow().get(key) {
            return hit;
        }
        let computed = build();
        c.borrow_mut().insert(key, computed.clone());
        computed
    })
}

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Name of the syntect theme paired with the active kage theme's
/// background: light canvases highlight with `base16-ocean.light`,
/// dark ones with `base16-ocean.dark`.
fn syntect_theme_name(light: bool) -> &'static str {
    if light {
        "base16-ocean.light"
    } else {
        "base16-ocean.dark"
    }
}

fn theme() -> &'static Theme {
    let ts = THEME_SET.get_or_init(ThemeSet::load_defaults);
    let light = crate::theme::current().bg_is_light();
    ts.themes
        .get(syntect_theme_name(light))
        .unwrap_or_else(|| ts.themes.values().next().expect("syntect ships themes"))
}

/// Render `code` using the syntax for the given file extension (no
/// leading dot). Falls back to plain text styled with `fallback` when
/// the extension is unknown. Each input line becomes one [`Line`].
///
/// Results are cached per-thread on `(code, extension, paired theme)`;
/// identical inputs reuse a previous render rather than re-running
/// syntect each frame. Cache caps at [`CACHE_CAP`] entries with FIFO
/// eviction.
#[must_use]
pub fn highlight_extension(code: &str, extension: &str, fallback: Style) -> Vec<Line<'static>> {
    if code.len() > HIGHLIGHT_BYTE_LIMIT {
        return plain_lines(code, fallback);
    }
    let light = crate::theme::current().bg_is_light();
    let key = cache_key(code, extension, syntect_theme_name(light));
    cached_or(key, || {
        let ss = syntax_set();
        match ss.find_syntax_by_extension(extension) {
            Some(syntax) => highlight_with_syntax(code, syntax, fallback),
            None => plain_lines(code, fallback),
        }
    })
}

/// Highlight `code` using the syntect grammar matching `lang`
/// (by token, then by name; e.g. `"rust"` or `"Rust"`). Falls back to
/// `plain_lines_styled` when the language is unknown or the input
/// exceeds [`HIGHLIGHT_BYTE_LIMIT`]. Used by [`crate::markdown::render`]
/// for fenced code blocks.
#[must_use]
pub fn highlight_with_lang(code: &str, lang: &str, fallback: Style) -> Vec<Line<'static>> {
    if code.len() > HIGHLIGHT_BYTE_LIMIT {
        return plain_lines_styled(code, fallback);
    }
    let ss = syntax_set();
    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_name(lang));
    match syntax {
        Some(s) => highlight_with_syntax(code, s, fallback),
        None => plain_lines_styled(code, fallback),
    }
}

/// One-line-per-`\n` plain styled lines. Used as a fallback when
/// syntect cannot match a language and as a building block for the
/// markdown renderer's plain text path.
#[must_use]
pub fn plain_lines_styled(text: &str, style: Style) -> Vec<Line<'static>> {
    plain_lines(text, style)
}

fn highlight_with_syntax(
    code: &str,
    syntax: &SyntaxReference,
    fallback: Style,
) -> Vec<Line<'static>> {
    let theme = theme();
    let mut h = HighlightLines::new(syntax, theme);
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in LinesWithEndings::from(code) {
        let Ok(regions) = h.highlight_line(line, syntax_set()) else {
            return plain_lines(code, fallback);
        };
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(regions.len());
        for (style, piece) in regions {
            let trimmed = piece.trim_end_matches('\n');
            spans.push(Span::styled(trimmed.to_owned(), to_ratatui_style(style)));
        }
        out.push(Line::from(spans));
    }
    out
}

fn plain_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    text.split('\n')
        .map(|line| Line::from(Span::styled(line.to_owned(), style)))
        .collect()
}

fn to_ratatui_style(s: SynStyle) -> Style {
    let mut out = Style::default().fg(Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b));
    if s.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_extension_falls_back_for_unknown_ext() {
        let lines = highlight_extension("hello", "xyzz", Style::default());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn highlight_extension_uses_known_syntax() {
        let lines = highlight_extension("fn main() {}", "rs", Style::default());
        assert_eq!(lines.len(), 1);
        // Highlighted spans should split into multiple pieces (kw, name, etc).
        assert!(lines[0].spans.len() > 1, "expected multiple spans");
    }

    #[test]
    fn syntect_theme_names_pair_with_background() {
        assert_eq!(syntect_theme_name(false), "base16-ocean.dark");
        assert_eq!(syntect_theme_name(true), "base16-ocean.light");
    }

    #[test]
    fn paired_themes_exist_in_syntect_defaults() {
        let ts = THEME_SET.get_or_init(ThemeSet::load_defaults);
        assert!(ts.themes.contains_key(syntect_theme_name(false)));
        assert!(ts.themes.contains_key(syntect_theme_name(true)));
    }

    #[test]
    fn cache_key_separates_paired_themes() {
        let text = "fn main() {}";
        let dark = cache_key(text, "rs", syntect_theme_name(false));
        let light = cache_key(text, "rs", syntect_theme_name(true));
        assert_ne!(dark, light);
        assert_eq!(dark, cache_key(text, "rs", syntect_theme_name(false)));
    }
}
