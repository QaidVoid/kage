//! Update discovery for the engine and this client.
//!
//! GitHub's releases list is the one source both checks read: engine
//! releases carry `v<semver>` tags, desktop releases
//! `kage-desktop-v<semver>`. The parse half is pure, so tests run
//! without a network; the fetch and install half exists only on
//! native targets, and every entry point degrades to a no-op or a
//! report on the web build.

use gpui_kit::AsyncApp;
use gpui_kit::Entity;

use crate::store::Store;

/// The releases list the checks read, newest first. `per_page` spans
/// both families, so a burst of desktop releases cannot hide the
/// newest engine tag.
#[cfg(not(target_arch = "wasm32"))]
const RELEASES_API: &str = "https://api.github.com/repos/QaidVoid/kage/releases?per_page=50";

/// The page a download action opens in the user's browser.
pub const RELEASES_PAGE: &str = "https://github.com/QaidVoid/kage/releases/latest";

/// Where release artifacts live.
const RELEASES_DOWNLOAD: &str = "https://github.com/QaidVoid/kage/releases/download";

/// How long a check result stands before the next launch checks again.
#[cfg(not(target_arch = "wasm32"))]
const CHECK_INTERVAL: i64 = 24 * 60 * 60;

/// The most bytes a downloaded engine binary may weigh. The binary
/// has stayed far under this; the cap bounds a broken mirror's reply.
#[cfg(not(target_arch = "wasm32"))]
const BINARY_CAP: usize = 256 * 1024 * 1024;

/// The most bytes a checksum sidecar may weigh.
#[cfg(not(target_arch = "wasm32"))]
const SIDECAR_CAP: usize = 1024 * 1024;

/// One release the feed named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The full tag, such as `v0.2.0`.
    pub tag: String,
    /// The version without the engine's leading `v`.
    pub version: String,
}

/// The newest release of each family the feed names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Feed {
    /// The newest engine release, the first parsable `v*` tag.
    pub cli: Option<Release>,
    /// The newest desktop release, the first parsable
    /// `kage-desktop-v*` tag.
    pub desktop: Option<Release>,
}

/// Reads the releases list. Within each family the first parsable tag
/// wins, which is GitHub's newest; prerelease suffixes do not parse
/// and are skipped so a broken comparison never reaches the interface.
#[must_use]
pub fn parse_feed(text: &str) -> Feed {
    let mut feed = Feed::default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return feed;
    };
    let Some(releases) = value.as_array() else {
        return feed;
    };
    for release in releases {
        let Some(tag) = release.get("tag_name").and_then(|tag| tag.as_str()) else {
            continue;
        };
        if let Some(version) = tag.strip_prefix("kage-desktop-v") {
            if feed.desktop.is_none() && parses(version) {
                feed.desktop = Some(Release {
                    tag: tag.to_owned(),
                    version: version.to_owned(),
                });
            }
        } else if let Some(version) = tag.strip_prefix('v') {
            if feed.cli.is_none() && parses(version) {
                feed.cli = Some(Release {
                    tag: tag.to_owned(),
                    version: version.to_owned(),
                });
            }
        }
    }
    feed
}

/// Whether a dotted version like `0.2.0` reads as one.
fn parses(version: &str) -> bool {
    crate::gate::parse(version).is_some()
}

/// Whether `latest` is a step ahead of the version running now. A
/// current version that does not parse, such as a dev build, counts
/// as behind.
#[must_use]
pub fn is_newer(latest: &str, current: &str) -> bool {
    crate::gate::is_below(current, latest)
}

/// The raw engine artifact built for `os` and `arch`, named the way
/// the release workflow uploads it. `None` where no release is built.
#[must_use]
pub fn cli_asset_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("kage-x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("kage-aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Some("kage-x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("kage-aarch64-apple-darwin"),
        ("windows", "x86_64") => Some("kage-x86_64-pc-windows-msvc.exe"),
        _ => None,
    }
}

/// The raw engine artifact built for the platform running this code.
#[must_use]
pub fn cli_asset() -> Option<&'static str> {
    cli_asset_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// The download URL for one release artifact.
#[must_use]
pub fn asset_url(tag: &str, asset: &str) -> String {
    format!("{RELEASES_DOWNLOAD}/{tag}/{asset}")
}

/// Where an installed engine lands, for the setup screen's caption.
#[must_use]
pub fn install_hint() -> &'static str {
    if cfg!(windows) {
        "Saved to %LOCALAPPDATA%\\Programs\\kage and wired into the app."
    } else {
        "Saved to ~/.local/bin and wired into the app."
    }
}

/// The digest a `sha256sum` sidecar names: the first field of the
/// first line, lowercased.
#[must_use]
pub fn parse_sha256(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    let digest = line.split_whitespace().next()?;
    let hex = digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit());
    hex.then(|| digest.to_ascii_lowercase())
}

/// Opens the releases page in the user's browser. A no-op on the web
/// build, where the page is one tab away already.
pub fn open_releases() {
    #[cfg(not(target_arch = "wasm32"))]
    native::open_in_browser(RELEASES_PAGE);
}

/// Downloads the newest engine release for this platform, verifies it
/// against its checksum sidecar and installs it where the user's
/// account owns it, returning the binary's path. The web build
/// reports the platform as unable.
pub fn install_latest() -> Result<std::path::PathBuf, String> {
    #[cfg(not(target_arch = "wasm32"))]
    return native::install_latest();

    #[cfg(target_arch = "wasm32")]
    Err("this platform cannot download kage".to_owned())
}

/// Runs one release check when one is due and stores what it saw in
/// the preferences, where the About page reads it. `force` skips the
/// daily throttle, for the button. The web build never checks.
pub async fn run_check(store: Entity<Store>, cx: &mut AsyncApp, force: bool) {
    #[cfg(target_arch = "wasm32")]
    let _ = (store, cx, force);

    #[cfg(not(target_arch = "wasm32"))]
    {
        let due = store.update(cx, |store, _| due_for_check(store.prefs(), force));
        if !due {
            return;
        }
        let fetched = cx
            .background_executor()
            .spawn(async { native::fetch_releases() })
            .await;
        let Ok(text) = fetched else { return };
        let feed = parse_feed(&text);
        if feed.cli.is_none() && feed.desktop.is_none() {
            return;
        }
        store.update(cx, |store, _| {
            store.update_prefs(|prefs| {
                if let Some(release) = &feed.cli {
                    prefs.latest_cli_version = Some(release.version.clone());
                }
                if let Some(release) = &feed.desktop {
                    prefs.latest_desktop_version = Some(release.version.clone());
                }
                prefs.update_checked_at = Some(crate::clock::unix_seconds());
            });
        });
    }
}

/// Whether enough time has passed since the last completed check.
#[cfg(not(target_arch = "wasm32"))]
fn due_for_check(prefs: &crate::prefs::Prefs, force: bool) -> bool {
    force
        || prefs
            .update_checked_at
            .is_none_or(|at| at.saturating_add(CHECK_INTERVAL) <= crate::clock::unix_seconds())
}

/// The lowercase hex SHA-256 of `bytes`.
#[cfg(not(target_arch = "wasm32"))]
fn digest(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::io::Read as _;
    use std::path::PathBuf;
    use std::time::Duration;

    /// The client the GitHub API asks for; it requires a user agent.
    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("kage-desktop/", env!("CARGO_PKG_VERSION")))
            .build()
    }

    /// Gets a URL's body as text.
    fn fetch_text(url: &str, what: &str) -> Result<String, String> {
        agent()
            .get(url)
            .call()
            .map_err(|error| format!("{what} did not load: {error}"))?
            .into_string()
            .map_err(|error| format!("{what} did not read: {error}"))
    }

    /// Gets a URL's body as bytes, refusing past `cap`.
    fn fetch_bytes(url: &str, cap: usize, what: &str) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        agent()
            .get(url)
            .call()
            .map_err(|error| format!("{what} did not load: {error}"))?
            .into_reader()
            .take(cap as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("{what} did not read: {error}"))?;
        Ok(bytes)
    }

    /// Fetches the releases list as text.
    pub(super) fn fetch_releases() -> Result<String, String> {
        fetch_text(super::RELEASES_API, "the release list")
    }

    /// Downloads, verifies and installs the newest engine release.
    pub(super) fn install_latest() -> Result<PathBuf, String> {
        let Some(asset) = super::cli_asset() else {
            return Err("no kage release is built for this platform".to_owned());
        };
        let feed = super::parse_feed(&fetch_releases()?);
        let Some(release) = feed.cli else {
            return Err("the release list names no engine release".to_owned());
        };
        let url = super::asset_url(&release.tag, asset);
        let bytes = fetch_bytes(&url, super::BINARY_CAP, "the kage binary")?;
        let sidecar = fetch_bytes(&format!("{url}.sha256"), super::SIDECAR_CAP, "its checksum")?;
        let text = String::from_utf8(sidecar).map_err(|_| "its checksum is not text".to_owned())?;
        let expected = super::parse_sha256(&text)
            .ok_or_else(|| "its checksum file is unreadable".to_owned())?;
        if super::digest(&bytes) != expected {
            return Err("the download does not match its checksum".to_owned());
        }
        install(&bytes)
    }

    /// Writes the binary under the user's own bin and returns its path.
    fn install(bytes: &[u8]) -> Result<PathBuf, String> {
        let dir = install_dir()?;
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let name = if cfg!(windows) { "kage.exe" } else { "kage" };
        let target = dir.join(name);
        let staging = dir.join(format!("{name}.new"));
        std::fs::write(&staging, bytes)
            .map_err(|error| format!("cannot write {}: {error}", staging.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755));
        }
        if target.exists() {
            let _ = std::fs::remove_file(&target);
        }
        std::fs::rename(&staging, &target)
            .map_err(|error| format!("cannot move it into place: {error}"))?;
        Ok(target)
    }

    /// The user-owned directory an installed engine goes to.
    fn install_dir() -> Result<PathBuf, String> {
        if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA")
                .map(|base| PathBuf::from(base).join("Programs").join("kage"))
                .ok_or_else(|| "LOCALAPPDATA is not set".to_owned())
        } else {
            std::env::var_os("HOME")
                .map(|base| PathBuf::from(base).join(".local").join("bin"))
                .ok_or_else(|| "HOME is not set".to_owned())
        }
    }

    /// Opens a URL in the user's browser and does not wait for it.
    pub(super) fn open_in_browser(url: &str) {
        let opened = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .spawn()
        } else if cfg!(target_os = "macos") {
            std::process::Command::new("open").arg(url).spawn()
        } else {
            std::process::Command::new("xdg-open").arg(url).spawn()
        };
        if let Err(error) = opened {
            crate::warn(&format!("the browser did not open: {error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_tag_of_each_family_wins() {
        let feed = parse_feed(
            r#"[
                {"tag_name": "kage-desktop-v0.9.0"},
                {"tag_name": "v0.2.0"},
                {"tag_name": "v0.1.0"},
                {"tag_name": "kage-desktop-v0.8.0"}
            ]"#,
        );
        assert_eq!(
            feed.cli.map(|release| release.tag),
            Some("v0.2.0".to_owned())
        );
        assert_eq!(
            feed.desktop.map(|release| release.version),
            Some("0.9.0".to_owned())
        );
    }

    #[test]
    fn unparsable_tags_are_skipped_within_a_family() {
        let feed = parse_feed(
            r#"[
                {"tag_name": "v0.3.0-rc.1"},
                {"tag_name": "v0.2.0"}
            ]"#,
        );
        assert_eq!(
            feed.cli.map(|release| release.version),
            Some("0.2.0".to_owned())
        );
        assert!(feed.desktop.is_none());
    }

    #[test]
    fn a_garbage_feed_names_nothing() {
        assert_eq!(parse_feed("not json"), Feed::default());
        assert_eq!(parse_feed("{}"), Feed::default());
        assert_eq!(parse_feed("[]"), Feed::default());
    }

    #[test]
    fn newer_compares_dotted_parts() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.2.0", "dev"));
    }

    #[test]
    fn every_release_target_maps_to_an_asset() {
        for (os, arch, asset) in [
            ("linux", "x86_64", "kage-x86_64-unknown-linux-musl"),
            ("linux", "aarch64", "kage-aarch64-unknown-linux-musl"),
            ("macos", "x86_64", "kage-x86_64-apple-darwin"),
            ("macos", "aarch64", "kage-aarch64-apple-darwin"),
            ("windows", "x86_64", "kage-x86_64-pc-windows-msvc.exe"),
        ] {
            assert_eq!(cli_asset_for(os, arch), Some(asset));
        }
        assert_eq!(cli_asset_for("plan9", "x86_64"), None);
        assert_eq!(cli_asset_for("linux", "riscv64"), None);
    }

    #[test]
    fn asset_urls_point_at_the_release_download() {
        assert_eq!(
            asset_url("v0.2.0", "kage-x86_64-apple-darwin"),
            "https://github.com/QaidVoid/kage/releases/download/v0.2.0/kage-x86_64-apple-darwin"
        );
    }

    #[test]
    fn checksum_sidecars_read_as_sha256sum_output() {
        let digest = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert_eq!(
            parse_sha256(&format!("{digest}  kage-x86_64-apple-darwin\n")),
            Some(digest.to_owned())
        );
        assert_eq!(parse_sha256("abc  x"), None);
        assert_eq!(parse_sha256(""), None);
    }

    #[test]
    fn digests_match_the_sha256_vectors() {
        assert_eq!(
            digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
