//! `_kage/providers/directory`: the providers of a directory in the
//! models.dev `api.json` shape, fetched by the engine so a client with
//! no network access of its own, such as a browser, can import from it.
//! Only models that can call tools are listed, since kage drives tools.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kage_acp::acp::{
    DirectoryCost, DirectoryModel, DirectoryProvider, DirectoryRequest, DirectoryResult,
};
use kage_core::sync::lock;
use kage_jsonrpc::RpcError;
use serde_json::Value;

/// The directory a request without a URL reads.
const MODELS_DEV: &str = "https://models.dev/api.json";

/// How long a fetched directory is served again before it is fetched
/// anew.
const FRESH_FOR: Duration = Duration::from_secs(600);

/// The largest directory read.
const MAX_BYTES: u64 = 32 * 1024 * 1024;

/// A fetched directory: when, and what it listed.
type Fetched = (Instant, Vec<DirectoryProvider>);

/// Directories fetched lately, by URL.
static FETCHED: Mutex<Option<HashMap<String, Fetched>>> = Mutex::new(None);

/// The providers of the directory `req` names.
pub(super) fn directory(req: &DirectoryRequest) -> Result<DirectoryResult, RpcError> {
    let url = req
        .url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or(MODELS_DEV);
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(RpcError::new(
            -32602,
            format!("{url} is not an http(s) URL"),
        ));
    }
    if let Some((at, providers)) = lock(&FETCHED).as_ref().and_then(|cache| cache.get(url))
        && at.elapsed() < FRESH_FOR
    {
        return Ok(DirectoryResult {
            providers: providers.clone(),
        });
    }
    let body = fetch(url, req.api_key.as_deref()).map_err(RpcError::internal)?;
    let providers = parse(&body).map_err(RpcError::internal)?;
    lock(&FETCHED)
        .get_or_insert_with(HashMap::new)
        .insert(url.to_owned(), (Instant::now(), providers.clone()));
    Ok(DirectoryResult { providers })
}

fn fetch(url: &str, key: Option<&str>) -> Result<String, String> {
    use std::io::Read as _;

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into();
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
                Some("@ai-sdk/google") => Some("gemini"),
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
    use super::parse;

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
}
