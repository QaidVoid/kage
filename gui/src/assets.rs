//! The app's own assets, ahead of the bundled icon catalog.
//!
//! The kage glyph, the kanji the web client draws its wordmark with,
//! is not a Lucide icon, so the catalog does not carry it. This source
//! serves the glyph from an embedded copy and hands every other path
//! to the toolkit's catalog, which keeps loading icons the way each
//! platform already does: embedded on the desktop, fetched from the
//! page origin in the browser.

use std::borrow::Cow;

use gpui_kit::assets::Assets;
use gpui_kit::{AssetSource, Result, SharedString};

/// The asset path the kage glyph outline lives under.
pub const GLYPH_PATH: &str = "icons/kage-glyph.svg";

/// The gradient-filled glyph for the dark kage palette: the same
/// outline the wordmark's face layer draws, pre-rendered at 8x with
/// the design's orb-1 to orb-2 diagonal, because the toolkit's SVG
/// element paints one flat color only.
pub const GLYPH_FACE_SHADOW: &str = "brand/kage-glyph-shadow.png";

/// The gradient-filled glyph for the light kage-dawn palette.
pub const GLYPH_FACE_DAWN: &str = "brand/kage-glyph-dawn.png";

/// The welcome pane's radial glow for the dark kage palette: the web
/// client's `radial-gradient(ellipse 50% 40% at 50% 38%)`, pre-rendered
/// in element-relative space and stretched to the pane at paint time,
/// because the toolkit has no radial-gradient background.
pub const WELCOME_GLOW: &str = "brand/welcome-glow.png";

/// The welcome glow for the light kage-dawn palette.
pub const WELCOME_GLOW_DAWN: &str = "brand/welcome-glow-dawn.png";

/// The glyph outline bytes: the kanji path the web client's `dom.js`
/// carries, from Noto Serif CJK JP Bold (SIL OFL 1.1).
static GLYPH: &[u8] = include_bytes!("../assets/brand/kage-glyph.svg");

/// The dark-palette gradient face bytes, rendered by `resvg` from the
/// same outline and the palette's orb stops.
static GLYPH_FACE_SHADOW_PNG: &[u8] = include_bytes!("../assets/brand/kage-glyph-shadow.png");

/// The dawn-palette gradient face bytes.
static GLYPH_FACE_DAWN_PNG: &[u8] = include_bytes!("../assets/brand/kage-glyph-dawn.png");

/// The dark-palette welcome glow bytes.
static WELCOME_GLOW_PNG: &[u8] = include_bytes!("../assets/brand/welcome-glow.png");

/// The dawn-palette welcome glow bytes.
static WELCOME_GLOW_DAWN_PNG: &[u8] = include_bytes!("../assets/brand/welcome-glow-dawn.png");

/// The app asset source: the glyph faces first, the bundled catalog
/// next.
pub struct KageAssets {
    catalog: Assets,
}

impl KageAssets {
    /// A source over the toolkit's catalog.
    #[must_use]
    pub fn new(catalog: Assets) -> Self {
        Self { catalog }
    }
}

impl AssetSource for KageAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            GLYPH_PATH => Ok(Some(Cow::Borrowed(GLYPH))),
            GLYPH_FACE_SHADOW => Ok(Some(Cow::Borrowed(GLYPH_FACE_SHADOW_PNG))),
            GLYPH_FACE_DAWN => Ok(Some(Cow::Borrowed(GLYPH_FACE_DAWN_PNG))),
            WELCOME_GLOW => Ok(Some(Cow::Borrowed(WELCOME_GLOW_PNG))),
            WELCOME_GLOW_DAWN => Ok(Some(Cow::Borrowed(WELCOME_GLOW_DAWN_PNG))),
            _ => self.catalog.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        self.catalog.list(path)
    }
}
