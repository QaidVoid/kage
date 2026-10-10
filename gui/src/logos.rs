//! Provider logos for the settings screens, fetched once and cached
//! on disk.
//!
//! The primary source is models.dev, which serves one SVG per
//! provider at `https://models.dev/logos/{id}.svg` - the same source
//! kage's model catalog snapshots. Two wrinkles shape this module: a
//! few kage ids differ from models.dev's ([`MODELS_DEV_ID`] maps
//! them), and models.dev answers an unknown id with HTTP 200 and a
//! generic sparkle glyph ([`FALLBACK`]), so a reply must be checked
//! against that placeholder before it counts as a logo.
//!
//! A provider models.dev does not know at all falls back to the
//! favicon of its own site: the endpoint host's `/favicon.ico`,
//! reached by dropping a leading `api.` label (`api.example.com`
//! serves no icon; `example.com` does).
//!
//! Each logo is fetched at most once per run: the memory cache holds
//! the decoded [`Image`], the disk cache under the kage cache
//! directory holds the bytes, and an id that resolves to nothing
//! stays `Missing` so it does not re-fetch on every render.

// On wasm the fetch machinery is compiled out; the module keeps its
// API shape and never resolves a logo.
#![cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]

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
    /// No logo anywhere for the id, or every fetch failed this run;
    /// the client shows its fallback glyph.
    Missing,
}

/// One run's logo cache. `Missing` doubles as the fetch-in-progress
/// marker: a second render before the fetch lands also shows the
/// fallback instead of stacking fetches.
static CACHE: LazyLock<Mutex<BTreeMap<String, Logo>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Kage ids whose models.dev logo lives under another name. Only ids
/// kage's own catalog registers get aliases; a custom provider's id
/// passes through untouched and falls back to its site's favicon.
fn models_dev_id(id: &str) -> &str {
    match id {
        "openai-responses" => "openai",
        "kimi-for-coding" => "kimi-code-plan-cn",
        other => other,
    }
}

/// The generic glyph models.dev serves, with HTTP 200, for any id it
/// has no logo for. A reply that matches it reads as no logo.
const FALLBACK: &str = r#"<svg viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg">
  <path
    shape-rendering="geometricPrecision"
    d="M9.8132 15.9038L9 18.75L8.1868 15.9038C7.75968 14.4089 6.59112 13.2403 5.09619 12.8132L2.25 12L5.09619 11.1868C6.59113 10.7597 7.75968 9.59112 8.1868 8.09619L9 5.25L9.8132 8.09619C10.2403 9.59113 11.4089 10.7597 12.9038 11.1868L15.75 12L12.9038 12.8132C11.4089 13.2403 10.2403 14.4089 9.8132 15.9038Z"
    stroke="currentColor"
    stroke-width="1.5"
    stroke-linecap="round"
    stroke-linejoin="round"
  />
  <path
    d="M18.2589 8.71454L18 9.75L17.7411 8.71454C17.4388 7.50533 16.4947 6.56117 15.2855 6.25887L14.25 6L15.2855 5.74113C16.4947 5.43883 17.4388 4.49467 17.7411 3.28546L18 2.25L18.2589 3.28546C18.5612 4.49467 19.5053 5.43883 20.7145 5.74113L21.75 6L20.7145 6.25887C19.5053 6.56117 18.5612 7.50533 18.2589 8.71454Z"
    stroke="currentColor"
    stroke-width="1.5"
    stroke-linecap="round"
    stroke-linejoin="round"
  />
  <path
    d="M16.8942 20.5673L16.5 21.75L16.1058 20.5673C15.8818 19.8954 15.3546 19.3682 14.6827 19.1442L13.5 18.75L14.6827 18.3558C15.3546 18.1318 15.8818 17.6046 16.1058 16.9327L16.5 15.75L16.8942 16.9327C17.1182 17.6046 17.6454 18.1318 18.3173 18.3558L19.5 18.75L18.3173 19.1442C17.6454 19.3682 17.1182 19.8954 16.8942 20.5673Z"
    stroke="currentColor"
    stroke-width="1.5"
    stroke-linecap="round"
    stroke-linejoin="round"
  />
</svg>
"#;

/// Whether the bytes are a logo this module renders: a Windows icon,
/// or SVG text that is not the placeholder.
fn ready(bytes: &[u8]) -> Option<Arc<Image>> {
    let format = if bytes.starts_with(b"\x00\x00\x01\x00") {
        ImageFormat::Ico
    } else if bytes.starts_with(b"<") && bytes != FALLBACK.as_bytes() {
        ImageFormat::Svg
    } else {
        return None;
    };
    Some(Arc::new(Image::from_bytes(format, bytes.to_vec())))
}

/// Where the fetched logos live: `$XDG_CACHE_HOME/kage/logos`, else
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
/// not seen it yet. `site` is the provider's endpoint URL when the
/// config names one; its host's favicon is the fallback source.
/// `Some` right away when the cache has it; `None` means the caller
/// renders its fallback, and a completed fetch notifies `view` so
/// the next render picks the logo up.
pub(crate) fn ensure(
    id: &str,
    site: Option<&str>,
    view: &Entity<SettingsView>,
    cx: &App,
) -> Option<Arc<Image>> {
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
        let site = site.map(str::to_owned);
        let dir = disk_dir();
        cx.spawn(async move |cx| {
            let fetched = cx
                .background_executor()
                .spawn({
                    let id = id.clone();
                    async move { fetch(&id, site.as_deref(), dir) }
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
    let _ = (site, view, cx);
    None
}

/// Fetches the id's logo: the disk cache first, then models.dev
/// under the mapped id, then the endpoint host's favicon. Whatever
/// fails or reads as the placeholder ends as [`Logo::Missing`].
#[cfg(not(target_arch = "wasm32"))]
fn fetch(id: &str, site: Option<&str>, dir: Option<PathBuf>) -> Logo {
    const LOGO_CAP: usize = 256 * 1024;
    if let Some(dir) = &dir {
        for ext in ["svg", "ico"] {
            let path = dir.join(format!("{id}.{ext}"));
            if let Ok(bytes) = std::fs::read(&path)
                && let Some(image) = ready(&bytes)
            {
                return Logo::Ready(image);
            }
        }
    }
    let upstream = fetch_bytes(
        &format!("https://models.dev/logos/{}.svg", models_dev_id(id)),
        LOGO_CAP,
    );
    if let Some(bytes) = upstream.filter(|bytes| ready(bytes).is_some()) {
        store(dir.as_deref(), id, "svg", &bytes);
        return Logo::Ready(ready(&bytes).expect("checked above"));
    }
    // models.dev does not know the id; the provider's own site may
    // still carry an icon. `api.example.com` serves none, its
    // web home does, so the machine label steps aside.
    if let Some(domain) = site.and_then(site_domain)
        && let Some(bytes) = fetch_bytes(&format!("https://{domain}/favicon.ico"), LOGO_CAP)
            .filter(|bytes| ready(bytes).is_some())
    {
        store(dir.as_deref(), id, "ico", &bytes);
        return Logo::Ready(ready(&bytes).expect("checked above"));
    }
    Logo::Missing
}

/// The host of an endpoint URL, port and path dropped, a leading
/// `api.` label with them.
fn site_domain(site: &str) -> Option<&str> {
    let rest = site.split_once("://").map(|(_, rest)| rest).unwrap_or(site);
    let host = rest.split('/').next()?;
    let host = host.split(':').next()?;
    let host = host.strip_prefix("api.").unwrap_or(host);
    (!host.is_empty()).then_some(host)
}

/// Gets a URL's body as bytes, refusing past `cap`. Any failure,
/// including a non-2xx status, reads as `None`.
#[cfg(not(target_arch = "wasm32"))]
fn fetch_bytes(url: &str, cap: usize) -> Option<Vec<u8>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("kage-desktop/", env!("CARGO_PKG_VERSION")))
        .build();
    let response = agent.get(url).call().ok()?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(cap as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

/// Writes the fetched bytes into the disk cache, best effort.
#[cfg(not(target_arch = "wasm32"))]
fn store(dir: Option<&std::path::Path>, id: &str, ext: &str, bytes: &[u8]) {
    if let Some(dir) = dir
        && std::fs::create_dir_all(dir).is_ok()
    {
        let _ = std::fs::write(dir.join(format!("{id}.{ext}")), bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_aliases_reach_their_models_dev_ids() {
        assert_eq!(models_dev_id("openai"), "openai");
        assert_eq!(models_dev_id("openai-responses"), "openai");
        assert_eq!(models_dev_id("kimi-for-coding"), "kimi-code-plan-cn");
        assert_eq!(models_dev_id("google"), "google");
        assert_eq!(models_dev_id("some-custom"), "some-custom");
    }

    #[test]
    fn the_pinned_fallback_never_reads_as_a_logo() {
        assert!(ready(FALLBACK.as_bytes()).is_none());
        assert!(ready(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>").is_some());
    }

    #[test]
    fn favicon_bytes_read_as_a_logo_and_other_magic_does_not() {
        let ico = [0u8, 0, 1, 0, 2, 0].into_iter().chain([0u8; 32]);
        assert!(ready(&ico.collect::<Vec<_>>()).is_some());
        assert!(ready(b"").is_none());
        assert!(ready(b"GIF89a").is_none());
    }

    #[test]
    fn site_domains_drop_scheme_path_port_and_api_label() {
        assert_eq!(
            site_domain("https://api.example.com/v1"),
            Some("example.com")
        );
        assert_eq!(site_domain("http://localhost:11434"), Some("localhost"));
        assert_eq!(site_domain("example.com/v1"), Some("example.com"));
        assert_eq!(site_domain("https://"), None);
    }
}
