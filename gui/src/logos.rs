//! Provider logos for the settings screens, fetched once from
//! models.dev and cached on disk.
//!
//! models.dev publishes one SVG per provider id at
//! `https://models.dev/logos/{id}.svg`; kage's model catalog is a
//! snapshot of the same source, so an id the engine registers is an
//! id this can usually render. A logo is fetched at most once per
//! run: the memory cache holds the decoded [`Image`], the disk cache
//! under the kage cache directory holds the SVG itself, and an id
//! that names no logo stays `Missing` so a custom provider's id does
//! not re-fetch on every render.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use gpui_kit::{App, Entity, Image, ImageFormat};

use crate::views::settings::SettingsView;

/// The logo an id resolves to, after at most one fetch.
#[derive(Debug, Clone)]
enum Logo {
    /// Decoded and ready to render.
    Ready(Arc<Image>),
    /// models.dev names no logo for the id, or the fetch failed this
    /// run; the client shows its fallback glyph.
    Missing,
}

/// One run's logo cache. `Missing` doubles as the fetch-in-progress
/// marker: a second render before the fetch lands also shows the
/// fallback instead of stacking fetches.
static CACHE: LazyLock<Mutex<BTreeMap<String, Logo>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Where the fetched SVGs live: `$XDG_CACHE_HOME/kage/logos`, else
/// the platform's own cache base. Native only; the web build keeps
/// its cache in this process alone.
#[cfg(not(target_arch = "wasm32"))]
fn disk_dir() -> Option<PathBuf> {
    let env = |key: &str| std::env::var_os(key).map(|value| value.to_string_lossy().into_owned());
    let home = env("HOME").map(PathBuf::from);
    let base = match std::env::consts::OS {
        "macos" => home.map(|home| home.join("Library").join("Caches")),
        "windows" => env("LOCALAPPDATA").map(PathBuf::from),
        _ => env("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join(".cache"))),
    }?;
    Some(base.join("kage").join("logos"))
}

/// The id's logo, fetching it in the background when this run has
/// not seen it yet. `Some` right away when the cache has it; `None`
/// means the caller renders its fallback, and a completed fetch
/// notifies `view` so the next render picks the logo up.
pub(crate) fn ensure(id: &str, view: &Entity<SettingsView>, cx: &App) -> Option<Arc<Image>> {
    if let Ok(cache) = CACHE.lock() {
        if let Some(Logo::Ready(image)) = cache.get(id) {
            return Some(image.clone());
        }
        if cache.contains_key(id) {
            return None;
        }
    } else {
        return None;
    }
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(id.to_owned(), Logo::Missing);
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let id = id.to_owned();
        let view = view.clone();
        let disk = disk_dir().map(|dir| dir.join(format!("{id}.svg")));
        cx.spawn(async move |cx| {
            let fetched = cx
                .background_executor()
                .spawn({
                    let id = id.clone();
                    async move { fetch(&id, disk) }
                })
                .await;
            if let Ok(mut cache) = CACHE.lock() {
                cache.insert(id, fetched);
            }
            view.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }
    #[cfg(target_arch = "wasm32")]
    let _ = (view, cx);
    None
}

/// Fetches the id's SVG, from the disk cache when it is there and
/// from models.dev otherwise. Whatever fails reads as [`Logo::Missing`].
#[cfg(not(target_arch = "wasm32"))]
fn fetch(id: &str, disk: Option<PathBuf>) -> Logo {
    const LOGO_CAP: usize = 256 * 1024;
    if let Some(path) = &disk
        && let Ok(bytes) = std::fs::read(path)
        && !bytes.is_empty()
    {
        return decode(bytes);
    }
    let url = format!("https://models.dev/logos/{id}.svg");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("kage-desktop/", env!("CARGO_PKG_VERSION")))
        .build();
    let bytes = match agent.get(&url).call() {
        Ok(response) => {
            let mut bytes = Vec::new();
            if response
                .into_reader()
                .take(LOGO_CAP as u64)
                .read_to_end(&mut bytes)
                .is_err()
            {
                return Logo::Missing;
            }
            bytes
        }
        Err(_) => return Logo::Missing,
    };
    if bytes.is_empty() {
        return Logo::Missing;
    }
    if let Some(path) = disk
        && let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        let _ = std::fs::write(&path, &bytes);
    }
    decode(bytes)
}

/// Decodes SVG bytes into a renderable image.
#[cfg(not(target_arch = "wasm32"))]
fn decode(bytes: Vec<u8>) -> Logo {
    if !bytes.starts_with(b"<") {
        return Logo::Missing;
    }
    Logo::Ready(Arc::new(Image::from_bytes(ImageFormat::Svg, bytes)))
}
