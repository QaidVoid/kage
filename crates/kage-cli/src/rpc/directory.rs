//! `_kage/providers/directory`: the providers of a directory in the
//! models.dev `api.json` shape, fetched by the engine so a client with
//! no network access of its own, such as a browser, can import from it.
//! Only models that can call tools are listed, since kage drives tools.

use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kage_acp::acp::{
    DirectoryCost, DirectoryModel, DirectoryProvider, DirectoryRequest, DirectoryResult,
};
use kage_core::sync::lock;
use kage_jsonrpc::RpcError;
use kage_tools::ssrf;
use serde_json::Value;

/// The directory a request without a URL reads.
const MODELS_DEV: &str = "https://models.dev/api.json";

/// How long a fetched directory is served again before it is fetched
/// anew.
const FRESH_FOR: Duration = Duration::from_secs(600);

/// The largest directory read.
const MAX_BYTES: u64 = 32 * 1024 * 1024;

/// How many redirects one fetch may follow. Every hop is resolved
/// through the SSRF guard, so a redirect cannot leave for an internal
/// target either.
const MAX_REDIRECTS: u32 = 4;

/// How many distinct directories stay cached.
const MAX_CACHED: usize = 8;

/// The approximate body budget across the cache.
const MAX_CACHED_BYTES: u64 = 64 * 1024 * 1024;

/// One fetched directory: when it was fetched, its body size, and what
/// it listed.
struct Entry {
    at: Instant,
    bytes: u64,
    providers: Vec<DirectoryProvider>,
}

/// The fetched directories, bounded in entries and bytes: a long-lived
/// serve must not grow with every distinct URL a client names.
#[derive(Default)]
struct FetchCache {
    entries: HashMap<String, Entry>,
}

impl FetchCache {
    /// The providers of `url` when they were fetched inside
    /// [`FRESH_FOR`] of `now`.
    fn fresh(&self, url: &str, now: Instant) -> Option<&[DirectoryProvider]> {
        let entry = self.entries.get(url)?;
        (now.duration_since(entry.at) < FRESH_FOR).then_some(entry.providers.as_slice())
    }

    /// Stores one fetch, dropping expired entries first and then the
    /// oldest until the entry and byte caps hold again.
    fn insert(&mut self, url: &str, bytes: u64, providers: Vec<DirectoryProvider>, now: Instant) {
        self.entries.insert(
            url.to_owned(),
            Entry {
                at: now,
                bytes,
                providers,
            },
        );
        self.entries
            .retain(|_, entry| now.duration_since(entry.at) < FRESH_FOR);
        while self.over_caps() {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(url, _)| url.clone());
            let Some(oldest) = oldest else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    /// Whether the cache is past either cap.
    fn over_caps(&self) -> bool {
        self.entries.len() > MAX_CACHED
            || self.entries.values().map(|entry| entry.bytes).sum::<u64>() > MAX_CACHED_BYTES
    }
}

/// Directories fetched lately, by URL.
static FETCHED: Mutex<Option<FetchCache>> = Mutex::new(None);

/// The providers of the directory `req` names.
pub(super) fn directory(req: &DirectoryRequest) -> Result<DirectoryResult, RpcError> {
    let url = req
        .url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or(MODELS_DEV);
    let (host, port) = fetch_url(url).map_err(|e| RpcError::new(-32602, e))?;
    vet(&host, port).map_err(RpcError::internal)?;
    let now = Instant::now();
    let hit = lock(&FETCHED)
        .as_ref()
        .and_then(|cache| cache.fresh(url, now))
        .map(<[DirectoryProvider]>::to_vec);
    if let Some(providers) = hit {
        return Ok(DirectoryResult { providers });
    }
    let body = fetch(url, req.api_key.as_deref()).map_err(RpcError::internal)?;
    let providers = parse(&body).map_err(RpcError::internal)?;
    lock(&FETCHED)
        .get_or_insert_with(FetchCache::default)
        .insert(url, body.len() as u64, providers.clone(), Instant::now());
    Ok(DirectoryResult { providers })
}

/// The URL shape one directory fetch accepts: https, with a host.
/// Returns the host to resolve and its port. The fetch dials only
/// addresses [`vet`] and the SSRF guard accept, so a client cannot
/// point kage's own network access at internal services.
fn fetch_url(url: &str) -> Result<(String, u16), String> {
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|e| format!("{url} is not a URL: {e}"))?;
    if uri.scheme_str() != Some("https") {
        return Err(format!("{url} must be an https URL"));
    }
    let host = uri.host().unwrap_or_default();
    if host.is_empty() {
        return Err(format!("{url} has no host"));
    }
    Ok((host.to_owned(), uri.port_u16().unwrap_or(443)))
}

/// Resolve `host` and refuse when any address it names is non-routable.
/// Runs before the connection so the error names the refused address.
pub(super) fn vet(host: &str, port: u16) -> Result<(), String> {
    let dial_host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let addrs: Vec<_> = (dial_host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .collect();
    for addr in addrs {
        if ssrf::is_unsafe(&addr.ip()) {
            return Err(format!(
                "refusing {host}: it resolves to the non-routable address {}",
                addr.ip()
            ));
        }
    }
    Ok(())
}

fn fetch(url: &str, key: Option<&str>) -> Result<String, String> {
    use std::io::Read as _;

    let agent = ssrf::guarded_agent(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .max_redirects(MAX_REDIRECTS)
            .build(),
    );
    let mut request = agent
        .get(url)
        .header("user-agent", concat!("kage/", env!("CARGO_PKG_VERSION")));
    if let Some(key) = key.filter(|key| !key.is_empty()) {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    let response = request.call().map_err(|e| format!("get {url}: {e}"))?;
    let mut body = String::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_BYTES)
        .read_to_string(&mut body)
        .map_err(|e| format!("read {url}: {e}"))?;
    Ok(body)
}

/// The providers an `api.json` lists, by name, each with its models that
/// call tools, by name.
pub(super) fn parse(json: &str) -> Result<Vec<DirectoryProvider>, String> {
    let root: serde_json::Map<String, Value> =
        serde_json::from_str(json).map_err(|e| format!("the directory is not api.json: {e}"))?;
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let mut providers: Vec<DirectoryProvider> = root
        .iter()
        .filter_map(|(key, provider)| {
            let models = provider["models"].as_object()?;
            let mut models: Vec<DirectoryModel> = models
                .iter()
                .filter(|(_, model)| model["tool_call"].as_bool() == Some(true))
                .map(|(key, model)| model_of(key, model))
                .collect();
            if models.is_empty() {
                return None;
            }
            models.sort_by(|a, b| a.name.cmp(&b.name));
            let kind = match provider["npm"].as_str() {
                Some("@ai-sdk/openai-compatible" | "@ai-sdk/openai") => Some("openai"),
                Some("@ai-sdk/anthropic") => Some("anthropic"),
                Some("@ai-sdk/google") => Some("google"),
                _ => None,
            };
            Some(DirectoryProvider {
                id: text(&provider["id"]).unwrap_or_else(|| key.clone()),
                name: text(&provider["name"]).unwrap_or_else(|| key.clone()),
                api: text(&provider["api"]),
                env: provider["env"]
                    .as_array()
                    .map(|env| env.iter().filter_map(text).collect())
                    .unwrap_or_default(),
                kind: kind.map(str::to_owned),
                models,
            })
        })
        .collect();
    if providers.is_empty() {
        return Err("the directory lists no providers with tool-calling models".to_owned());
    }
    providers.sort_by_key(|provider| provider.name.to_lowercase());
    Ok(providers)
}

fn model_of(key: &str, model: &Value) -> DirectoryModel {
    let cost = &model["cost"];
    DirectoryModel {
        id: model["id"].as_str().unwrap_or(key).to_owned(),
        name: model["name"].as_str().unwrap_or(key).to_owned(),
        context: model["limit"]["context"].as_u64(),
        output: model["limit"]["output"].as_u64(),
        reasoning: model["reasoning"].as_bool().unwrap_or(false),
        input: model["modalities"]["input"]
            .as_array()
            .map(|input| {
                input
                    .iter()
                    .filter_map(|kind| kind.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        cost: cost["input"]
            .as_f64()
            .zip(cost["output"].as_f64())
            .map(|(input, output)| DirectoryCost {
                input,
                output,
                cache_read: cost["cache_read"].as_f64(),
                cache_write: cost["cache_write"].as_f64(),
            }),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{FRESH_FOR, FetchCache, MAX_CACHED, fetch_url, parse, vet};

    #[test]
    fn a_directory_lists_its_tool_calling_models() {
        let json = r#"{
            "lab": {
                "id": "lab", "name": "Lab", "api": "https://lab/v1", "env": ["LAB_KEY"],
                "npm": "@ai-sdk/openai-compatible",
                "models": {
                    "b": {"id": "b", "name": "Bravo", "tool_call": true, "reasoning": true,
                          "limit": {"context": 128000, "output": 8192},
                          "modalities": {"input": ["text", "image"]},
                          "cost": {"input": 1.0, "output": 2.0, "cache_read": 0.1}},
                    "a": {"id": "a", "name": "Alpha", "tool_call": true},
                    "c": {"id": "c", "name": "Chat only", "tool_call": false}
                }
            },
            "words": {"id": "words", "name": "Words", "models": {
                "w": {"id": "w", "name": "W", "tool_call": false}
            }}
        }"#;
        let providers = parse(json).unwrap();
        assert_eq!(
            providers.len(),
            1,
            "a provider with no tool models drops out"
        );
        let lab = &providers[0];
        assert_eq!(lab.kind.as_deref(), Some("openai"));
        assert_eq!(lab.env, ["LAB_KEY"]);
        let names: Vec<_> = lab.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "Bravo"]);
        let bravo = &lab.models[1];
        assert_eq!((bravo.context, bravo.output), (Some(128_000), Some(8192)));
        assert!(bravo.reasoning);
        assert_eq!(bravo.input, ["text", "image"]);
        assert_eq!(bravo.cost.as_ref().unwrap().cache_read, Some(0.1));
        assert!(parse("[]").is_err());
    }

    #[test]
    fn only_https_urls_with_a_host_are_accepted() {
        assert!(fetch_url("http://models.dev/api.json").is_err());
        assert!(fetch_url("ftp://models.dev/api.json").is_err());
        assert!(fetch_url("https:///api.json").is_err());
        assert_eq!(
            fetch_url("https://models.dev/api.json").unwrap(),
            ("models.dev".to_owned(), 443)
        );
        assert_eq!(
            fetch_url("https://models.dev:8443/api.json").unwrap(),
            ("models.dev".to_owned(), 8443)
        );
    }

    #[test]
    fn the_vet_refuses_non_routable_hosts_without_dialing() {
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "::1",
            "[::1]",
            "localhost",
            "2001:db8::1",
        ] {
            let err = vet(host, 443).expect_err(host);
            assert!(err.contains("non-routable"), "{host}: {err}");
        }
        assert!(vet("1.1.1.1", 443).is_ok());
        let (host, port) = fetch_url("https://[2001:db8::1]/api.json").unwrap();
        assert_eq!(host, "[2001:db8::1]");
        assert!(vet(&host, port).is_err());
    }

    #[test]
    fn the_cache_evicts_the_oldest_beyond_the_entry_cap() {
        let mut cache = FetchCache::default();
        let now = Instant::now();
        for ix in 0..=MAX_CACHED {
            cache.insert(
                &format!("u{ix}"),
                1,
                Vec::new(),
                now + Duration::from_secs(ix as u64),
            );
        }
        assert!(cache.fresh("u0", now).is_none(), "oldest evicted");
        assert!(
            cache.fresh(&format!("u{MAX_CACHED}"), now).is_some(),
            "newest kept"
        );
    }

    #[test]
    fn an_expired_entry_is_neither_served_nor_kept() {
        let mut cache = FetchCache::default();
        let now = Instant::now();
        cache.insert("a", 1, Vec::new(), now);
        assert!(cache.fresh("a", now).is_some());
        assert!(
            cache.fresh("a", now + FRESH_FOR).is_none(),
            "stale not served"
        );
        cache.insert("b", 1, Vec::new(), now + FRESH_FOR + Duration::from_secs(1));
        assert!(
            !cache.entries.contains_key("a"),
            "the expired entry is dropped on insert"
        );
        assert!(cache.entries.contains_key("b"));
    }
}
