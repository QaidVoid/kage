//! The model lists of custom providers that name a `models_url`. kage
//! keeps each in its cache, `$XDG_CACHE_HOME/kage/providers/<id>.json`,
//! instead of the config, and refreshes it like the model catalog. The
//! provider's `models` entries in the config are merged over it (see
//! [`kage_core::config::CustomProviderConfig::models_over`]).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use kage_acp::acp::{ProbeModel, ProviderProbe};
use kage_core::config::{Config, CustomProviderModel};
use kage_core::modality::Modality;
use kage_core::thinking::Effort;
use serde_json::{Value, json};

/// What a provider's cache file holds: the list, and the URL it was read
/// from, so a changed `models_url` reads as no cache at all.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Cached {
    url: String,
    models: Vec<CustomProviderModel>,
}

/// The file custom provider `id`'s listed models are cached in.
///
/// # Errors
///
/// When `id` is not a provider id the config accepts, so it cannot name
/// a file outside the cache, or there is no cache directory.
pub(crate) fn cache_path(id: &str) -> Result<PathBuf, String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!("`{id}` is not a provider id"));
    }
    Ok(crate::paths::cache_root()?
        .join("providers")
        .join(format!("{id}.json")))
}

/// The models cached for custom provider `id` from `url`, empty when
/// none are, the cache does not read, or it holds another URL's list.
pub(crate) fn cached(id: &str, url: &str) -> Vec<CustomProviderModel> {
    cache_path(id)
        .ok()
        .and_then(|path| read(&path, url))
        .unwrap_or_default()
}

/// When custom provider `id`'s list from `url` was cached or last found
/// current, `None` when no cache holds it.
pub(crate) fn written(id: &str, url: &str) -> Option<SystemTime> {
    let path = cache_path(id).ok()?;
    read(&path, url)?;
    std::fs::metadata(path).ok()?.modified().ok()
}

/// The list cached at `path` when it was read from `url`.
fn read(path: &Path, url: &str) -> Option<Vec<CustomProviderModel>> {
    let cached: Cached = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (cached.url == url).then_some(cached.models)
}

/// The custom providers in `config` whose models come from a
/// `models_url`, with that URL.
pub(crate) fn listed_providers(config: &Config) -> impl Iterator<Item = (&str, &str)> {
    config
        .providers
        .custom
        .iter()
        .filter_map(|(id, provider)| Some((id.as_str(), provider.models_url.as_deref()?)))
}

/// Fetches the list of every provider in `config` with a `models_url`
/// that has none cached yet, so a provider saved a moment ago lists its
/// models at once. Returns whether any list was cached; one that fails
/// waits for the next refresh.
pub(crate) fn fetch_missing(config: &Config) -> bool {
    let store = crate::auth::AuthStore::load().unwrap_or_else(|_| crate::auth::AuthStore::empty());
    let mut fetched = false;
    for (id, url) in listed_providers(config) {
        if written(id, url).is_none() {
            fetched |= refresh(id, config, &store).is_ok_and(|count| count.is_some());
        }
    }
    fetched
}

/// Fetches custom provider `id`'s list from its `models_url` with the
/// saved key, and caches it. Returns how many models it lists when the
/// list changed.
///
/// # Errors
///
/// When the provider names no `models_url`, the list cannot be fetched
/// or names no model, or the cache cannot be written.
pub(crate) fn refresh(
    id: &str,
    config: &Config,
    store: &crate::auth::AuthStore,
) -> Result<Option<usize>, String> {
    let url = config
        .providers
        .custom
        .get(id)
        .and_then(|provider| provider.models_url.clone())
        .ok_or_else(|| format!("{id} names no models_url"))?;
    let probe = ProviderProbe {
        id: id.to_owned(),
        ..ProviderProbe::default()
    };
    let result = crate::rpc::probe::probe(&probe, config, store);
    if !result.ok {
        return Err(result.message);
    }
    save(&cache_path(id)?, &url, &result.models)
}

/// Caches `fetched`, read from `url`, at `path`. Returns how many models
/// it holds when it differs from the cached list; an unchanged list only
/// has its cache marked as checked now.
///
/// # Errors
///
/// When no fetched model is one kage can use, which keeps the cached
/// list over an endpoint's passing fault, or the cache cannot be
/// written.
pub(crate) fn save(
    path: &Path,
    url: &str,
    fetched: &[ProbeModel],
) -> Result<Option<usize>, String> {
    let models: Vec<CustomProviderModel> = fetched
        .iter()
        .filter_map(|model| serde_json::from_value(row(model)).ok())
        .collect();
    if models.is_empty() {
        return Err("the list names no model; the cached one stays".to_owned());
    }
    if read(path, url).is_some_and(|cached| cached == models) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(SystemTime::now()))
            .map_err(|e| format!("touch {}: {e}", path.display()))?;
        return Ok(None);
    }
    let count = models.len();
    let body = serde_json::to_vec_pretty(&Cached {
        url: url.to_owned(),
        models,
    })
    .map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    kage_core::fsutil::atomic_write(path, &body)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(Some(count))
}

/// A fetched model as a `[[providers.custom.<id>.models]]` entry. Only
/// values the config's own types re-parse make it in: an endpoint
/// naming an unknown kind would fail the next load.
pub(crate) fn row(model: &ProbeModel) -> Value {
    let mut row = json!({
        "id": model.id,
        "name": model.name.clone().unwrap_or_else(|| model.id.clone()),
    });
    if let Some(context) = model.context {
        row["context"] = json!(context);
    }
    if let Some(out) = model.max_output.and_then(|out| u32::try_from(out).ok()) {
        row["max_output"] = json!(out);
    }
    let input: Vec<&str> = model
        .input
        .iter()
        .map(String::as_str)
        .filter(|kind| Modality::parse(kind).is_some())
        .collect();
    if !input.is_empty() {
        row["input"] = json!(input);
    }
    let produces: Vec<&str> = model
        .produces
        .iter()
        .map(String::as_str)
        .filter(|kind| Modality::parse(kind).is_some())
        .collect();
    if produces.iter().any(|kind| *kind != "text") {
        row["produces"] = json!(produces);
    }
    let efforts: Vec<&str> = model
        .efforts
        .iter()
        .map(String::as_str)
        .filter(|level| Effort::parse(level).is_some())
        .collect();
    if !efforts.is_empty() {
        row["efforts"] = json!(efforts);
    }
    if let Some(reasoning) = model.reasoning {
        row["reasoning"] = json!(reasoning);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetched(id: &str, context: u64) -> ProbeModel {
        ProbeModel {
            id: id.to_owned(),
            name: Some(format!("Model {id}")),
            context: Some(context),
            efforts: vec!["high".to_owned(), "galaxy".to_owned()],
            ..ProbeModel::default()
        }
    }

    #[test]
    fn a_fetched_list_is_cached_and_an_unchanged_one_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers").join("lab.json");
        let url = "https://lab.example/models.json";

        assert_eq!(save(&path, url, &[fetched("a", 1000)]), Ok(Some(1)));
        assert_eq!(read(&path, "https://elsewhere.example/"), None);
        let models = read(&path, url).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "Model a");
        assert_eq!(models[0].context, Some(1000));
        assert!(
            models[0].efforts.contains(Effort::High),
            "an unknown effort is dropped, the rest kept"
        );

        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(save(&path, url, &[fetched("a", 1000)]), Ok(None));
        assert!(std::fs::metadata(&path).unwrap().modified().unwrap() > old);
        assert_eq!(
            save(&path, url, &[fetched("a", 2000), fetched("b", 10)]),
            Ok(Some(2))
        );
        assert!(
            save(&path, url, &[]).is_err(),
            "an empty list keeps the cached one"
        );
        assert_eq!(read(&path, url).unwrap()[0].context, Some(2000));
        assert!(cache_path("../x").is_err());
    }
}
