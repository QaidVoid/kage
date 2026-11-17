//! Provider/model catalog.
//!
//! The bundled snapshot lives in `generated.rs`, which is rewritten by
//! `cargo xtask refresh-models` from the curated catalog at
//! [`source::catalog_url`]. `kage models refresh`, and a running kage on
//! its own, write a newer copy of that same catalog to the model cache;
//! [`use_cache`] lays its model entries over the snapshot. This module exposes a hand-curated API on
//! top so callers don't depend on the generator's exact shape.
//!
//! Provider impls are still hardcoded in their respective modules
//! (`anthropic`, `openai`, `gemini`, `compat`); the catalog only
//! carries the metadata needed for pickers, default-model selection,
//! thinking and display. The cache never changes a provider's id,
//! name or endpoint, only its models.

mod generated;
pub mod source;

use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

pub use kage_core::ModelCost;
use kage_core::{Modalities, Reasoning, ReasoningField};

use source::{SourceModel, SourceProvider};

/// Hard cap on a downloaded catalog body.
const DOWNLOAD_LIMIT: usize = 32 * 1024 * 1024;

/// Description of one provider in the catalog.
#[derive(Debug, Clone, Copy)]
pub struct ProviderInfo {
    /// Stable id used as the prefix in `provider/model` strings, and
    /// as the key in the auth credential store.
    pub id: &'static str,
    /// Human-friendly display name.
    pub name: &'static str,
    /// Documented API endpoint, when the catalog publishes one.
    pub api: Option<&'static str>,
    /// Models the provider exposes that support tool calling.
    pub models: &'static [ModelInfo],
}

/// Description of one model in the catalog.
#[derive(Debug, Clone, Copy)]
pub struct ModelInfo {
    /// Provider-scoped model id (e.g. `claude-sonnet-4-6`, `glm-4.6`).
    pub id: &'static str,
    /// Human-friendly display name.
    pub name: &'static str,
    /// Context window in tokens, when the catalog reports one.
    pub context: Option<u64>,
    /// Largest prompt in tokens, when the catalog reports one smaller
    /// than the context window.
    pub input_limit: Option<u64>,
    /// Maximum output tokens per turn, when reported.
    pub output: Option<u64>,
    /// Thinking settings the model accepts.
    pub reasoning: Reasoning,
    /// Modalities the model accepts.
    pub input: Modalities,
    /// Modalities the model produces. Empty means unknown, which is
    /// read as text only.
    pub produces: Modalities,
    /// Field an OpenAI-compatible model reads its reasoning back from
    /// during a tool loop (the catalog's `interleaved.field`).
    pub interleaved: Option<ReasoningField>,
    /// ISO-8601 date string the catalog associates with this model.
    pub release_date: Option<&'static str>,
    /// Per-million-token pricing in USD, when the catalog reports it.
    pub cost: Option<ModelCost>,
}

impl ModelInfo {
    /// Tokens a prompt may fill: the input limit when reported, else
    /// the context window.
    #[must_use]
    pub fn prompt_window(&self) -> Option<u64> {
        self.input_limit.or(self.context)
    }
}

/// The catalog laid over the snapshot, and the digest of the cache body
/// it was read from.
#[derive(Clone, Copy)]
struct Loaded {
    providers: &'static [ProviderInfo],
    digest: [u8; 32],
}

static CATALOG: RwLock<Option<Loaded>> = RwLock::new(None);

/// Every provider in the catalog: the bundled snapshot, with the model
/// cache laid over it when [`use_cache`] loaded one.
#[must_use]
pub fn providers() -> &'static [ProviderInfo] {
    CATALOG
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .map_or(generated::PROVIDERS, |loaded| loaded.providers)
}

/// The digest of the cache body the catalog in effect was laid from,
/// `None` while it is the bundled snapshot alone.
#[must_use]
pub fn loaded_digest() -> Option<[u8; 32]> {
    CATALOG
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .map(|loaded| loaded.digest)
}

/// The SHA-256 digest of `bytes`.
fn digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).into()
}

/// Lay the model cache at `path` over the bundled snapshot, replacing
/// what an earlier call laid. Lookups after it see the cache's models.
/// A catalog is never freed, since lookups hand out `'static`
/// references into it, so each load a running process takes keeps one
/// more copy: loads are meant for a changed cache only.
///
/// The cache may add models to a bundled provider or update their
/// metadata. It never adds a provider or changes a provider's id,
/// name or endpoint.
///
/// # Errors
///
/// The reason the cache was ignored: unreadable file, or a body that
/// does not parse. Callers should surface the reason; a silently
/// stale catalog is exactly the drift the cache exists to prevent.
pub fn use_cache(path: &Path) -> Result<usize, String> {
    load(path, false).map(Option::unwrap_or_default)
}

/// [`use_cache`] when the cache at `path` differs from the one this
/// process laid last, as after another process refreshed it. Returns
/// the model count when it was laid.
///
/// # Errors
///
/// As [`use_cache`].
pub fn use_cache_if_changed(path: &Path) -> Result<Option<usize>, String> {
    load(path, true)
}

fn load(path: &Path, only_changed: bool) -> Result<Option<usize>, String> {
    let json =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let digest = digest(json.as_bytes());
    let mut catalog = CATALOG.write().unwrap_or_else(PoisonError::into_inner);
    if only_changed && catalog.is_some_and(|loaded| loaded.digest == digest) {
        return Ok(None);
    }
    let providers = merge_json(generated::PROVIDERS, &json)
        .ok_or_else(|| "catalog cache does not parse".to_owned())?;
    *catalog = Some(Loaded { providers, digest });
    Ok(Some(providers.iter().map(|p| p.models.len()).sum()))
}

/// What [`refresh_if_changed`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refreshed {
    /// A new catalog was written, exposing this many models.
    Changed(usize),
    /// The published catalog is the one already cached.
    Unchanged,
}

/// Read the curated catalog from `url`, check it parses to at least
/// one of kage's providers, and write it to `dest`, replacing it
/// atomically. A `file://` URL reads a local file, which is how an
/// offline run or a test pins a catalog. Returns the number of models
/// kage's providers expose.
///
/// The body is written as it came, unpruned: the curated feed is
/// already scoped to kage's providers and their tool-calling models,
/// and re-pruning it here would only risk dropping a provider on a
/// schema change. The cache loader reads it through
/// `SUPPORTED_PROVIDERS`, so an unexpected extra key is ignored rather
/// than cached.
///
/// # Errors
///
/// The download fails, the body exceeds [`DOWNLOAD_LIMIT`], the body is
/// not a catalog or names none of kage's providers, or `dest` cannot be
/// written.
pub fn refresh(url: &str, dest: &Path) -> Result<usize, String> {
    let (body, etag) = fetch(url, None)?.ok_or("the catalog server answered 304 unasked")?;
    store(url, &body, etag.as_deref(), dest)
}

/// [`refresh`], asking the server for the catalog only when it differs
/// from the one cached at `dest`, by the entity tag stored beside it.
/// The tag is only sent while the cache is the body `url` served with it.
/// An unchanged catalog keeps its file, whose modification time is set
/// to now so the next check counts from this one.
///
/// # Errors
///
/// As [`refresh`], and when the unchanged cache cannot be touched.
pub fn refresh_if_changed(url: &str, dest: &Path) -> Result<Refreshed, String> {
    if let Some((body, etag)) = fetch(url, cached_etag(url, dest).as_deref())? {
        return store(url, &body, etag.as_deref(), dest).map(Refreshed::Changed);
    }
    std::fs::File::options()
        .write(true)
        .open(dest)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        .map_err(|e| format!("touch {}: {e}", dest.display()))?;
    Ok(Refreshed::Unchanged)
}

/// The entity tag `url` served the cache at `dest` with, when the file
/// beside it records one for this very body from this very URL: a body
/// another process or a hand edit replaced, or one another source
/// served, is fetched whole.
fn cached_etag(url: &str, dest: &Path) -> Option<String> {
    let body = std::fs::read(dest).ok()?;
    let recorded = std::fs::read_to_string(etag_path(dest)).ok()?;
    let mut lines = recorded.lines();
    let (etag, of, from) = (lines.next()?, lines.next()?, lines.next()?);
    (of == hex(&digest(&body)) && from == url).then(|| etag.to_owned())
}

/// `bytes` as lowercase hexadecimal.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The file beside the cache at `dest` holding the entity tag its body
/// was served with, the digest of that body and the URL that served it.
fn etag_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".etag");
    dest.with_file_name(name)
}

/// The catalog body at `url` and its entity tag, or `None` when the
/// server answers that the body tagged `etag` is still current.
fn fetch(url: &str, etag: Option<&str>) -> Result<Option<(String, Option<String>)>, String> {
    if let Some(path) = url.strip_prefix("file://") {
        let file = std::fs::File::open(path).map_err(|e| format!("read {path}: {e}"))?;
        return Ok(Some((read_limited(file, DOWNLOAD_LIMIT)?, None)));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .into();
    let mut request = agent
        .get(url)
        .header("user-agent", concat!("kage/", env!("CARGO_PKG_VERSION")));
    if let Some(etag) = etag {
        request = request.header("if-none-match", etag.trim());
    }
    let response = request.call().map_err(|e| format!("get {url}: {e}"))?;
    if response.status() == 304 {
        return Ok(None);
    }
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = read_limited(response.into_body().into_reader(), DOWNLOAD_LIMIT)?;
    Ok(Some((body, etag)))
}

/// Checks `body`, served by `url`, is a catalog naming at least one of
/// kage's providers, writes it to `dest` and its entity tag beside it,
/// and returns how many models those providers expose.
fn store(url: &str, body: &str, etag: Option<&str>, dest: &Path) -> Result<usize, String> {
    let providers = source::parse(body)?;
    if providers.is_empty() {
        return Err("catalog has none of kage's providers".to_owned());
    }
    let count: usize = providers.iter().map(|p| p.models.len()).sum();
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    kage_core::fsutil::atomic_write(dest, body.as_bytes())
        .map_err(|e| format!("write {}: {e}", dest.display()))?;
    let tag = etag_path(dest);
    match etag {
        Some(etag) => {
            let recorded = format!(
                "{}\n{}\n{url}\n",
                etag.trim(),
                hex(&digest(body.as_bytes()))
            );
            kage_core::fsutil::atomic_write(&tag, recorded.as_bytes())
                .map_err(|e| format!("write {}: {e}", tag.display()))?;
        }
        None => {
            let _ = std::fs::remove_file(&tag);
        }
    }
    Ok(count)
}

/// `reader` decoded as UTF-8, failing once it exceeds `limit` bytes
/// instead of silently truncating mid-frame.
fn read_limited(reader: impl std::io::Read, limit: usize) -> Result<String, String> {
    use std::io::Read as _;
    let mut reader = reader;
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read body: {e}"))?;
    if bytes.len() > limit {
        return Err(format!("catalog exceeds {} MiB", limit / (1024 * 1024)));
    }
    String::from_utf8(bytes).map_err(|e| format!("catalog is not utf-8: {e}"))
}

/// `bundled` with the models of the catalog `json` laid over it:
/// matching ids are replaced, new ids added. Providers outside
/// `bundled` are dropped and provider fields always come from
/// `bundled`. `None` when `json` does not parse. The merged catalog is
/// leaked: it is built once and lives for the process.
fn merge_json(bundled: &'static [ProviderInfo], json: &str) -> Option<&'static [ProviderInfo]> {
    let cached = source::parse(json).ok()?;
    let merged: Vec<ProviderInfo> = bundled
        .iter()
        .map(|p| match cached.iter().find(|c| c.id == p.id) {
            Some(c) => ProviderInfo {
                models: merge_models(p.models, c),
                ..*p
            },
            None => *p,
        })
        .collect();
    Some(Box::leak(merged.into_boxed_slice()))
}

fn merge_models(bundled: &[ModelInfo], cached: &SourceProvider) -> &'static [ModelInfo] {
    let mut models = bundled.to_vec();
    for m in &cached.models {
        let info = leak_model(m);
        match models.iter_mut().find(|b| b.id == info.id) {
            Some(slot) => *slot = info,
            None => models.push(info),
        }
    }
    models.sort_by(|a, b| a.id.cmp(b.id));
    Box::leak(models.into_boxed_slice())
}

fn leak_model(m: &SourceModel) -> ModelInfo {
    let leak = |s: &str| -> &'static str { Box::leak(s.to_owned().into_boxed_str()) };
    ModelInfo {
        id: leak(&m.id),
        name: leak(&m.name),
        context: m.context,
        input_limit: m.input_limit,
        output: m.output,
        reasoning: m.reasoning,
        input: m.input,
        produces: m.produces,
        interleaved: m.interleaved,
        release_date: m.release_date.as_deref().map(leak),
        cost: m.cost,
    }
}

/// Find a provider by its kage id.
#[must_use]
pub fn provider(id: &str) -> Option<&'static ProviderInfo> {
    providers().iter().find(|p| p.id == id)
}

/// Find a model under `provider`.
#[must_use]
pub fn model(provider_id: &str, model_id: &str) -> Option<&'static ModelInfo> {
    provider(provider_id)?
        .models
        .iter()
        .find(|m| m.id == model_id)
}

/// Pick a sensible default model for `provider`. Prefers the
/// most-recently-released model in the catalog; falls back to the
/// first listed. Returns `None` for unknown providers.
#[must_use]
pub fn preferred_model(provider_id: &str) -> Option<&'static ModelInfo> {
    let p = provider(provider_id)?;
    latest_model(p.models).or_else(|| p.models.first())
}

/// The most-recently-released model: release dates compared as parsed
/// dates, an unparseable date the oldest, and ties broken by model id
/// so the choice is stable regardless of list order.
fn latest_model(models: &[ModelInfo]) -> Option<&ModelInfo> {
    models
        .iter()
        .filter(|m| m.release_date.is_some())
        .max_by_key(|m| (parse_date(m.release_date.unwrap_or_default()), m.id))
}

/// A `YYYY-MM-DD` date as comparable `(year, month, day)`; a date that
/// does not parse sorts as the oldest.
fn parse_date(date: &str) -> (u32, u32, u32) {
    let mut parts = date.split('-').map(|p| p.parse::<u32>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_lookup() {
        // Catalog ships with at least the four hand-curated providers.
        assert!(provider("anthropic").is_some());
        assert!(provider("openai").is_some());
        assert!(provider("google").is_some());
        assert!(provider("zai").is_some());
        assert!(provider("nope").is_none());
    }

    #[test]
    fn each_provider_has_at_least_one_model() {
        for p in providers() {
            assert!(!p.models.is_empty(), "{} has no models", p.id);
        }
    }

    /// Every compat provider id must have a snapshot row, or the picker
    /// silently lists no models for it.
    #[test]
    fn compat_provider_ids_have_catalog_rows() {
        for entry in crate::compat::COMPAT_PROVIDERS {
            let info =
                provider(entry.id).unwrap_or_else(|| panic!("{} has no catalog entry", entry.id));
            assert!(!info.models.is_empty(), "{} has no models", entry.id);
        }
    }

    #[test]
    fn kimi_for_coding_has_models_with_cost_data() {
        let p = provider("kimi-for-coding").expect("kimi-for-coding in the snapshot");
        assert!(!p.models.is_empty());
        assert!(p.models.iter().all(|m| m.cost.is_some()));
    }

    #[test]
    fn preferred_model_returns_some_for_known_provider() {
        assert!(preferred_model("anthropic").is_some());
        assert!(preferred_model("nope").is_none());
    }

    #[test]
    fn cache_updates_and_adds_models_but_never_provider_fields() {
        let cache = r#"{
            "anthropic": {
                "name": "Evil",
                "api": "https://evil.example/v1",
                "models": {
                    "claude-opus-4-7": {
                        "id": "claude-opus-4-7", "name": "Opus Renamed", "tool_call": true,
                        "limit": {"context": 42, "output": 7}
                    },
                    "claude-new": {"id": "claude-new", "name": "New", "tool_call": true},
                    "bad": {"tool_call": true}
                }
            },
            "not-a-provider": {
                "name": "X", "api": "https://x.example",
                "models": {"m": {"id": "m", "tool_call": true}}
            }
        }"#;
        let merged = merge_json(generated::PROVIDERS, cache).unwrap();
        assert_eq!(merged.len(), generated::PROVIDERS.len());
        let anthropic = merged.iter().find(|p| p.id == "anthropic").unwrap();
        let bundled = provider("anthropic").unwrap();
        assert_eq!(anthropic.name, bundled.name);
        assert_eq!(anthropic.api, bundled.api);
        let opus = anthropic
            .models
            .iter()
            .find(|m| m.id == "claude-opus-4-7")
            .unwrap();
        assert_eq!(opus.name, "Opus Renamed");
        assert_eq!(opus.context, Some(42));
        assert!(anthropic.models.iter().any(|m| m.id == "claude-new"));
        assert_eq!(anthropic.models.len(), bundled.models.len() + 1);
        assert!(merged.iter().all(|p| p.id != "not-a-provider"));
    }

    #[test]
    fn an_unchanged_cache_is_not_laid_again() {
        let dir = std::env::temp_dir().join(format!("kage-cache-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        // A provider without models changes no lookup another test makes.
        std::fs::write(
            &path,
            r#"{"anthropic": {"name": "Anthropic", "models": {}}}"#,
        )
        .unwrap();
        assert!(use_cache_if_changed(&path).unwrap().is_some());
        assert_eq!(use_cache_if_changed(&path), Ok(None));
        std::fs::write(
            &path,
            r#"{"anthropic": {"name": "Anthropic", "models": { }}}"#,
        )
        .unwrap();
        assert!(
            use_cache_if_changed(&path).unwrap().is_some(),
            "a cache another process rewrote is laid"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unparseable_cache_falls_back() {
        assert!(merge_json(generated::PROVIDERS, "not json").is_none());
        let err = use_cache(Path::new("/nonexistent/kage/models.json"))
            .expect_err("missing cache is ignored with a reason");
        assert!(err.contains("read"), "{err}");
        assert!(provider("anthropic").is_some());
    }

    /// An unreadable or unparseable cache must be reported, never
    /// silently skipped: the caller warns that the snapshot may be
    /// stale.
    #[test]
    fn use_cache_reports_the_reason_it_ignored_a_cache() {
        let path =
            std::env::temp_dir().join(format!("kage-catalog-test-{}.json", std::process::id()));
        std::fs::write(&path, "not json").expect("write cache fixture");
        let err = use_cache(&path).expect_err("bad cache is ignored with a reason");
        assert!(err.contains("does not parse"), "{err}");
        std::fs::remove_file(&path).ok();
        assert!(provider("anthropic").is_some());
    }

    #[test]
    fn read_limited_rejects_oversized_bodies_instead_of_truncating() {
        let body = vec![b'a'; 16];
        let err = read_limited(std::io::Cursor::new(&body), 8).unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
        let ok = read_limited(std::io::Cursor::new(&body), 16).unwrap();
        assert_eq!(ok.len(), 16);
        assert!(read_limited(std::io::Cursor::new(&body), 15).is_err());
    }

    #[test]
    fn refresh_reads_a_local_catalog_and_writes_the_cache() {
        let dir = std::env::temp_dir().join(format!("kage-refresh-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let source = dir.join("catalog.json");
        std::fs::write(
            &source,
            r#"{"anthropic": {"name": "Anthropic", "models": {
                "a": {"id": "a", "name": "A", "tool_call": true}}}}"#,
        )
        .expect("write catalog fixture");
        let dest = dir.join("cache").join("models.json");
        let url = format!("file://{}", source.display());
        let count = refresh(&url, &dest).expect("refresh reads the file");
        assert_eq!(count, 1);
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            std::fs::read_to_string(&source).unwrap()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Answers one request on `listener` with `status`, `headers` and
    /// `body`, returning the request it read.
    fn answer_once(
        listener: &std::net::TcpListener,
        status: &str,
        headers: &str,
        body: &str,
    ) -> String {
        use std::io::{Read as _, Write as _};
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
            request.push(byte[0]);
        }
        write!(
            stream,
            "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        String::from_utf8(request).unwrap()
    }

    #[test]
    fn an_unchanged_catalog_is_not_downloaded_again() {
        let dir = std::env::temp_dir().join(format!("kage-refresh-etag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("models.json");
        let body = r#"{"anthropic": {"name": "Anthropic", "models": {
            "a": {"id": "a", "name": "A", "tool_call": true}}}}"#;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let first = answer_once(&listener, "200 OK", "etag: \"v1\"\r\n", body);
            let second = answer_once(&listener, "304 Not Modified", "", "");
            let third = answer_once(&listener, "200 OK", "etag: \"v1\"\r\n", body);
            (first, second, third)
        });

        assert_eq!(refresh_if_changed(&url, &dest), Ok(Refreshed::Changed(1)));
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&dest)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(refresh_if_changed(&url, &dest), Ok(Refreshed::Unchanged));
        let touched = std::fs::metadata(&dest).unwrap().modified().unwrap();
        std::fs::write(&dest, body.replace("\"A\"", "\"Edited\"")).unwrap();
        assert_eq!(refresh_if_changed(&url, &dest), Ok(Refreshed::Changed(1)));

        let (first, second, third) = server.join().unwrap();
        assert!(!first.to_lowercase().contains("if-none-match"), "{first}");
        assert!(
            second.to_lowercase().contains("if-none-match: \"v1\""),
            "{second}"
        );
        assert!(
            !third.to_lowercase().contains("if-none-match"),
            "a body that is not the tagged one is fetched whole: {third}"
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), body);
        assert!(touched > old, "the check counts from now");
        assert!(
            cached_etag("http://elsewhere.example/", &dest).is_none(),
            "a tag never goes to another source"
        );
        assert!(cached_etag(&url, &dest).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refresh_refuses_a_catalog_without_kages_providers() {
        let dir = std::env::temp_dir().join(format!("kage-refresh-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let source = dir.join("catalog.json");
        std::fs::write(&source, r#"{"unrelated": {}}"#).expect("write catalog fixture");
        let dest = dir.join("cache").join("models.json");
        let url = format!("file://{}", source.display());
        let err = refresh(&url, &dest).unwrap_err();
        assert!(err.contains("none of kage's providers"), "{err}");
        assert!(!dest.parent().unwrap().exists(), "nothing written");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn model(id: &'static str, release_date: Option<&'static str>) -> ModelInfo {
        ModelInfo {
            id,
            name: id,
            context: None,
            input_limit: None,
            output: None,
            reasoning: Reasoning::None,
            input: Modalities::of(&[]),
            produces: Modalities::of(&[]),
            interleaved: None,
            release_date,
            cost: None,
        }
    }

    #[test]
    fn preferred_model_breaks_date_ties_by_id_and_dates_bad_dates_oldest() {
        let models = vec![
            model("b", Some("2026-01-15")),
            model("a", Some("2026-01-15")),
            model("c", Some("not-a-date")),
            model("d", None),
        ];
        assert_eq!(latest_model(&models).unwrap().id, "b");
        let later = vec![
            model("old", Some("2025-12-31")),
            model("new", Some("2026-02-01")),
        ];
        assert_eq!(latest_model(&later).unwrap().id, "new");
    }
}
