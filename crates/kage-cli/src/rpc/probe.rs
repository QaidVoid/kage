//! `_kage/config/test`: asks a provider for its model list, so a
//! settings form can test a connection and fill its model table through
//! the engine's network access instead of the client's, which a browser
//! does not have.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use kage_acp::acp::{
    ConfigTestResult, KeySource, ProbeModel, ProbeTool, ProviderKey, ProviderProbe,
};
use kage_core::config::{Config, CustomProviderKind};
use kage_provider::catalog;
use kage_provider::compat::COMPAT_PROVIDERS;
use serde_json::Value;

use super::REDACTED;
use crate::auth::{self, AuthStore};

/// How long the model list may take.
const TIMEOUT: Duration = Duration::from_secs(15);

/// The wire protocol a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    OpenAi,
    Anthropic,
    Gemini,
}

/// Where and how to ask for the model list.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    kind: Kind,
    base: String,
    key: String,
    headers: BTreeMap<String, String>,
}

/// Asks the provider `probe` describes for its models, with `config`
/// and `store` filling what the probe leaves out.
pub(crate) fn probe(probe: &ProviderProbe, config: &Config, store: &AuthStore) -> ConfigTestResult {
    let target = match resolve(probe, config, store) {
        Ok(target) => target,
        Err(message) => {
            return ConfigTestResult {
                message,
                ..ConfigTestResult::default()
            };
        }
    };
    list(&probe.id, &target)
}

/// The target `probe` names: its own fields, then the saved entry for
/// its id, then the provider's defaults.
fn resolve(probe: &ProviderProbe, config: &Config, store: &AuthStore) -> Result<Target, String> {
    let id = probe.id.as_str();
    if id.is_empty() {
        return Err("name the provider to test".to_owned());
    }
    let custom = config.providers.custom.get(id);
    let builtin = config.providers.overrides.get(id);
    let compat = COMPAT_PROVIDERS.iter().find(|entry| entry.id == id);
    let kind = match probe.kind.as_deref() {
        Some("openai" | "openai-responses") => Kind::OpenAi,
        Some("anthropic") => Kind::Anthropic,
        Some("gemini") => Kind::Gemini,
        Some(other) => return Err(format!("unknown protocol `{other}`")),
        None => match custom.map(|custom| custom.kind) {
            Some(CustomProviderKind::Anthropic) => Kind::Anthropic,
            Some(CustomProviderKind::Gemini) => Kind::Gemini,
            None if id == "anthropic" => Kind::Anthropic,
            None if id == "google" => Kind::Gemini,
            Some(CustomProviderKind::OpenAi) | None => Kind::OpenAi,
        },
    };
    let default_base = match id {
        "anthropic" => Some("https://api.anthropic.com"),
        "google" => Some("https://generativelanguage.googleapis.com"),
        "openai" | "openai-responses" => Some("https://api.openai.com/v1"),
        _ => compat.map(|entry| entry.base_url),
    };
    let base = probe
        .base_url
        .clone()
        .filter(|base| !base.trim().is_empty())
        .or_else(|| custom.map(|custom| custom.base_url.clone()))
        .or_else(|| builtin.and_then(|o| o.base_url.clone()))
        .or_else(|| default_base.map(str::to_owned))
        .ok_or_else(|| "set a base URL".to_owned())?;
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(format!("{base} is not an http(s) URL"));
    }
    let key = if let Some(key) = probe.api_key.as_deref().filter(|key| !key.is_empty()) {
        key.to_owned()
    } else {
        let env = probe
            .api_key_env
            .clone()
            .unwrap_or_else(|| key_env(id, config));
        if env.is_empty() {
            String::new()
        } else {
            crate::providers::lookup_key_with_env(id, &env, store)
                .ok_or_else(|| format!("no key: {env} is not set and no key is saved for {id}"))?
        }
    };
    let saved = custom
        .map(|custom| &custom.headers)
        .or_else(|| builtin.map(|o| &o.headers));
    let headers = probe
        .headers
        .iter()
        .filter_map(|(name, value)| {
            if value == REDACTED {
                saved?.get(name).map(|value| (name.clone(), value.clone()))
            } else {
                Some((name.clone(), value.clone()))
            }
        })
        .collect();
    Ok(Target {
        kind,
        base: base.trim_end_matches('/').to_owned(),
        key,
        headers,
    })
}

/// The ids of the providers kage registers itself.
const BUILTIN: [&str; 4] = ["anthropic", "google", "openai", "openai-responses"];

/// The environment variable provider `id` reads its key from: the
/// config's, else the one kage knows for a registered provider, else
/// `<ID>_API_KEY`. Empty means the provider needs no key.
fn key_env(id: &str, config: &Config) -> String {
    let custom = config.providers.custom.get(id);
    let saved = custom.map_or_else(
        || {
            config
                .providers
                .overrides
                .get(id)
                .and_then(|o| o.api_key_env.clone())
        },
        |custom| custom.api_key_env.clone(),
    );
    let registered = BUILTIN.contains(&id) || COMPAT_PROVIDERS.iter().any(|entry| entry.id == id);
    saved.unwrap_or_else(|| {
        if custom.is_none() && registered {
            auth::env_var_for(id).to_owned()
        } else {
            format!("{}_API_KEY", id.to_uppercase())
        }
    })
}

/// Where one provider finds its API key.
pub(super) fn provider_keys(config: &Config, store: &AuthStore) -> BTreeMap<String, ProviderKey> {
    BUILTIN
        .iter()
        .copied()
        .chain(COMPAT_PROVIDERS.iter().map(|entry| entry.id))
        .chain(config.providers.custom.keys().map(String::as_str))
        .map(|id| {
            let env = key_env(id, config);
            let source = if env.is_empty() {
                KeySource::Unneeded
            } else if std::env::var(&env).is_ok_and(|value| !value.is_empty()) {
                KeySource::Env
            } else if store.access_token(id).is_some() {
                KeySource::Auth
            } else {
                KeySource::Missing
            };
            let name = display_name(id, config);
            (id.to_owned(), ProviderKey { env, source, name })
        })
        .collect()
}

/// The display name a client should show for provider `id`: the
/// compat list's curated one, else the model catalog's, else the
/// config's own display name for a custom provider, else nothing and
/// the client keeps the id.
fn display_name(id: &str, config: &Config) -> Option<String> {
    if let Some(entry) = COMPAT_PROVIDERS.iter().find(|entry| entry.id == id) {
        return Some(entry.display_name.to_owned());
    }
    if let Some(known) = kage_provider::catalog::provider(id) {
        return Some(known.name.to_owned());
    }
    config
        .providers
        .custom
        .get(id)
        .and_then(|custom| custom.display_name.clone())
}

/// How long an MCP server or ACP agent may take to answer.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// Connects to MCP server `name` as `spec` describes and lists its
/// tools. The worker owns the connection and closes it when done; a
/// server that never answers leaves only that worker behind.
pub(super) fn probe_mcp(name: &str, spec: &kage_core::config::McpServer) -> ConfigTestResult {
    let (tx, rx) = std::sync::mpsc::channel();
    let server = name.to_owned();
    let spec = spec.clone();
    let started = Instant::now();
    std::thread::spawn(move || {
        let listed = kage_mcp::McpServerHandle::spawn(&server, &spec, &[], None)
            .map_err(|e| e.to_string())
            .and_then(|handle| handle.connection().list_tools().map_err(|e| e.to_string()));
        let _ = tx.send(listed);
    });
    let outcome = rx
        .recv_timeout(START_TIMEOUT)
        .unwrap_or_else(|_| Err(format!("no answer within {}s", START_TIMEOUT.as_secs())));
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match outcome {
        Ok(tools) => ConfigTestResult {
            ok: true,
            message: format!("connected \u{b7} {} tools \u{b7} {millis} ms", tools.len()),
            millis,
            tools: tools
                .into_iter()
                .map(|tool| ProbeTool {
                    name: tool.name,
                    description: tool.description,
                })
                .collect(),
            ..ConfigTestResult::default()
        },
        Err(why) => ConfigTestResult {
            message: why,
            millis,
            ..ConfigTestResult::default()
        },
    }
}

/// Starts ACP agent `agent`, sends `initialize` and waits for the
/// answer, then stops it.
pub(super) fn probe_acp(agent: &kage_core::config::AcpAgent) -> ConfigTestResult {
    use std::io::{BufRead as _, Write as _};

    let started = Instant::now();
    let failed = |message: String| ConfigTestResult {
        message,
        millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ..ConfigTestResult::default()
    };
    let mut child = match std::process::Command::new(&agent.command)
        .args(&agent.args)
        .envs(&agent.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return failed(format!("cannot start {}: {e}", agent.command)),
    };
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": 1, "clientCapabilities": {} },
    });
    // Held open until the answer: closing stdin ends some agents.
    let mut stdin = child.stdin.take();
    if let Some(stdin) = &mut stdin {
        let _ = writeln!(stdin, "{request}");
    }
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if value["id"] == 1 {
                    let _ = tx.send(value);
                    return;
                }
            }
        });
    }
    let answer = rx.recv_timeout(START_TIMEOUT);
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let Ok(answer) = answer else {
        return failed(format!(
            "no initialize answer within {}s",
            START_TIMEOUT.as_secs()
        ));
    };
    if let Some(error) = answer.get("error") {
        return failed(format!(
            "initialize failed: {}",
            error["message"].as_str().unwrap_or("error")
        ));
    }
    let info = &answer["result"]["agentInfo"];
    let who = [info["name"].as_str(), info["version"].as_str()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    ConfigTestResult {
        ok: true,
        message: format!(
            "initialize \u{b7} {} \u{b7} {millis} ms",
            if who.is_empty() {
                "agent answered"
            } else {
                who.as_str()
            }
        ),
        millis,
        agent: (!who.is_empty()).then_some(who),
        ..ConfigTestResult::default()
    }
}

/// Lists the models of provider `id` at `target`, warning when the key
/// rides a cleartext request.
fn list(id: &str, target: &Target) -> ConfigTestResult {
    let mut result = list_models(id, target);
    if let Some(warning) = cleartext_warning(target) {
        result.message = format!("{warning}\n{}", result.message);
    }
    result
}

/// The warning when a non-empty key would be sent over `http://`, which
/// any network peer can read. `None` for https and for providers that
/// need no key, where `http://` is the normal local case.
fn cleartext_warning(target: &Target) -> Option<&'static str> {
    let cleartext = target.base.starts_with("http://") && !target.key.is_empty();
    cleartext.then_some("warning: the key is sent over cleartext http; use https")
}

/// The model list request of `list`, without the cleartext warning.
fn list_models(id: &str, target: &Target) -> ConfigTestResult {
    let url = match target.kind {
        Kind::OpenAi => format!("{}/models", target.base),
        Kind::Anthropic => format!("{}/v1/models", target.base),
        Kind::Gemini => format!("{}/v1beta/models", target.base),
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .build()
        .new_agent();
    let mut request = agent.get(url.as_str());
    if !target.key.is_empty() {
        request = match target.kind {
            Kind::OpenAi => request.header("authorization", format!("Bearer {}", target.key)),
            Kind::Anthropic => request
                .header("x-api-key", target.key.as_str())
                .header("anthropic-version", "2023-06-01"),
            Kind::Gemini => request.header("x-goog-api-key", target.key.as_str()),
        };
    }
    for (name, value) in &target.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let started = Instant::now();
    let response = request.call();
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut response = match response {
        Ok(response) => response,
        Err(error) => {
            return ConfigTestResult {
                message: format!("GET {url} failed: {error}"),
                millis,
                ..ConfigTestResult::default()
            };
        }
    };
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        let detail: String = body.trim().chars().take(200).collect();
        return ConfigTestResult {
            status: Some(status),
            message: format!("GET {url} \u{b7} {status} \u{b7} {detail}"),
            millis,
            ..ConfigTestResult::default()
        };
    }
    let Some(models) = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|json| parse_models(target.kind, &json))
    else {
        return ConfigTestResult {
            status: Some(status),
            message: format!("GET {url} \u{b7} {status} \u{b7} the answer is not a model list"),
            millis,
            ..ConfigTestResult::default()
        };
    };
    let models: Vec<ProbeModel> = models
        .into_iter()
        .map(|model| with_catalog(id, model))
        .collect();
    ConfigTestResult {
        ok: true,
        status: Some(status),
        message: format!(
            "GET {url} \u{b7} {status} \u{b7} {} models \u{b7} {millis} ms",
            models.len()
        ),
        millis,
        models,
        ..ConfigTestResult::default()
    }
}

/// The models a model list answer names, sorted by id. `None` when the
/// answer has no list.
fn parse_models(kind: Kind, json: &Value) -> Option<Vec<ProbeModel>> {
    let number = |value: &Value| value.as_u64();
    let mut models: Vec<ProbeModel> = match kind {
        Kind::OpenAi | Kind::Anthropic => json["data"]
            .as_array()?
            .iter()
            .filter_map(|entry| {
                Some(ProbeModel {
                    id: entry["id"].as_str()?.to_owned(),
                    name: entry["display_name"]
                        .as_str()
                        .or_else(|| entry["name"].as_str())
                        .map(str::to_owned),
                    context: number(&entry["context_length"])
                        .or_else(|| number(&entry["context_window"])),
                    max_output: number(&entry["top_provider"]["max_completion_tokens"]),
                })
            })
            .collect(),
        Kind::Gemini => json["models"]
            .as_array()?
            .iter()
            .filter(|entry| {
                entry["supportedGenerationMethods"]
                    .as_array()
                    .is_none_or(|methods| methods.iter().any(|m| m == "generateContent"))
            })
            .filter_map(|entry| {
                let name = entry["name"].as_str()?;
                Some(ProbeModel {
                    id: name.strip_prefix("models/").unwrap_or(name).to_owned(),
                    name: entry["displayName"].as_str().map(str::to_owned),
                    context: number(&entry["inputTokenLimit"]),
                    max_output: number(&entry["outputTokenLimit"]),
                })
            })
            .collect(),
    };
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Some(models)
}

/// `model` with what the model catalog knows filling the gaps: the
/// entry under provider `id`, else the first provider listing the id.
fn with_catalog(id: &str, mut model: ProbeModel) -> ProbeModel {
    let known = catalog::model(id, &model.id).or_else(|| {
        catalog::providers()
            .iter()
            .find_map(|provider| provider.models.iter().find(|m| m.id == model.id))
    });
    if let Some(known) = known {
        model.name.get_or_insert_with(|| known.name.to_owned());
        if model.context.is_none() {
            model.context = known.context;
        }
        if model.max_output.is_none() {
            model.max_output = known.output;
        }
    }
    model
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use kage_acp::acp::ProviderProbe;
    use kage_core::config::Config;

    use kage_acp::acp::KeySource;

    use super::{Kind, Target, cleartext_warning, probe, provider_keys, resolve};
    use crate::auth::AuthStore;

    /// Answers one request with `status` and `body`, handing back the
    /// request head it read.
    fn serve_once(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            head
        });
        (base, thread)
    }

    fn draft(id: &str, base: &str) -> ProviderProbe {
        ProviderProbe {
            id: id.to_owned(),
            base_url: Some(base.to_owned()),
            api_key: Some("sk-typed".to_owned()),
            ..ProviderProbe::default()
        }
    }

    #[test]
    fn a_draft_lists_its_models_with_the_typed_key() {
        let (base, server) = serve_once(
            200,
            r#"{"data":[{"id":"zeta"},{"id":"alpha","context_length":32000}]}"#,
        );
        let result = probe(
            &draft("lab", &format!("{base}/v1")),
            &Config::default(),
            &AuthStore::empty(),
        );
        let head = server.join().unwrap();
        assert!(head.starts_with("GET /v1/models "), "{head}");
        assert!(
            head.to_lowercase()
                .contains("authorization: bearer sk-typed"),
            "{head}"
        );
        assert!(result.ok, "{}", result.message);
        assert_eq!(result.status, Some(200));
        let ids: Vec<_> = result.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["alpha", "zeta"]);
        assert_eq!(result.models[0].context, Some(32_000));
        assert!(result.message.contains("2 models"), "{}", result.message);
        assert!(result.message.contains("cleartext"), "{}", result.message);
    }

    #[test]
    fn only_an_http_base_with_a_key_warns_about_cleartext() {
        let warned = Target {
            kind: Kind::OpenAi,
            base: "http://localhost:11434/v1".to_owned(),
            key: "sk-typed".to_owned(),
            headers: BTreeMap::new(),
        };
        assert_eq!(
            cleartext_warning(&warned),
            Some("warning: the key is sent over cleartext http; use https")
        );
        let secure = Target {
            base: "https://api.example.com/v1".to_owned(),
            ..warned
        };
        assert_eq!(cleartext_warning(&secure), None);
        let keyless = Target {
            base: "http://localhost:11434/v1".to_owned(),
            key: String::new(),
            ..secure
        };
        assert_eq!(cleartext_warning(&keyless), None);
    }

    #[test]
    fn a_refusal_is_a_result_with_the_status() {
        let (base, server) = serve_once(401, r#"{"error":"bad key"}"#);
        let result = probe(
            &draft("lab", &base),
            &Config::default(),
            &AuthStore::empty(),
        );
        server.join().unwrap();
        assert!(!result.ok);
        assert_eq!(result.status, Some(401));
        assert!(
            result.message.contains("401") && result.message.contains("bad key"),
            "{}",
            result.message
        );
        assert!(!result.message.contains("sk-typed"));
    }

    #[test]
    fn a_saved_entry_fills_the_probe() {
        let config: Config = kage_core::config_edit::parse(
            "[providers.custom.lab]\nkind = \"anthropic\"\nbase_url = \"http://lab:1/\"\napi_key_env = \"\"\nheaders = { X-Team = \"red\" }\n[[providers.custom.lab.models]]\nid = \"m\"\nname = \"M\"\n",
        )
        .unwrap();
        let probe = ProviderProbe {
            id: "lab".to_owned(),
            headers: [("X-Team".to_owned(), "<redacted>".to_owned())].into(),
            ..ProviderProbe::default()
        };
        let target = resolve(&probe, &config, &AuthStore::empty()).unwrap();
        assert_eq!(target.kind, Kind::Anthropic);
        assert_eq!(target.base, "http://lab:1");
        assert_eq!(target.key, "");
        assert_eq!(target.headers["X-Team"], "red");
    }

    #[test]
    fn each_provider_says_where_its_key_is() {
        let config: Config = kage_core::config_edit::parse(
            "[providers.custom.local]\nbase_url = \"http://l\"\napi_key_env = \"\"\n[[providers.custom.local.models]]\nid = \"m\"\nname = \"M\"\n\n[providers.custom.lab]\nbase_url = \"http://lab\"\napi_key_env = \"KAGE_TEST_UNSET_LAB_KEY\"\n[[providers.custom.lab.models]]\nid = \"m\"\nname = \"M\"\n",
        )
        .unwrap();
        let mut store = AuthStore::empty();
        store.set_api_key("deepseek", "sk-saved");
        let keys = provider_keys(&config, &store);
        assert_eq!(keys["local"].source, KeySource::Unneeded);
        assert_eq!(keys["lab"].env, "KAGE_TEST_UNSET_LAB_KEY");
        assert_eq!(keys["lab"].source, KeySource::Missing);
        assert_eq!(keys["deepseek"].env, "DEEPSEEK_API_KEY");
        assert!(matches!(
            keys["deepseek"].source,
            KeySource::Auth | KeySource::Env
        ));
        assert!(keys.contains_key("anthropic"));
        assert_eq!(keys["deepseek"].name.as_deref(), Some("DeepSeek"));
        assert_eq!(keys["anthropic"].name.as_deref(), Some("Anthropic"));
        assert_eq!(keys["local"].name, None, "a bare custom id has no name");
    }

    #[test]
    fn an_acp_agent_answers_its_handshake() {
        let answer = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"fake","version":"1.0"}}}"#;
        let agent = kage_core::config::AcpAgent {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                format!("read line; printf '%s\\n' '{answer}'; sleep 5"),
            ],
            env: std::collections::BTreeMap::new(),
        };
        let result = super::probe_acp(&agent);
        assert!(result.ok, "{}", result.message);
        assert_eq!(result.agent.as_deref(), Some("fake 1.0"));
        let missing = kage_core::config::AcpAgent {
            command: "/nonexistent/agent".into(),
            ..agent
        };
        let result = super::probe_acp(&missing);
        assert!(!result.ok);
        assert!(
            result.message.contains("cannot start"),
            "{}",
            result.message
        );
    }

    #[test]
    fn an_mcp_server_that_cannot_start_says_why() {
        let spec = kage_core::config::McpServer {
            startup_timeout_secs: None,
            command: Some("/nonexistent/server".into()),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            disabled: false,
            oauth: None,
            disabled_tools: Vec::new(),
        };
        let result = super::probe_mcp("gone", &spec);
        assert!(!result.ok);
        assert!(!result.message.is_empty());
    }

    #[test]
    fn a_missing_key_or_url_says_so() {
        let probe = ProviderProbe {
            id: "lab".to_owned(),
            base_url: Some("http://lab".to_owned()),
            api_key_env: Some("KAGE_TEST_UNSET_KEY_VAR".to_owned()),
            ..ProviderProbe::default()
        };
        let err = resolve(&probe, &Config::default(), &AuthStore::empty()).unwrap_err();
        assert!(err.contains("KAGE_TEST_UNSET_KEY_VAR"), "{err}");
        let nowhere = ProviderProbe {
            id: "lab".to_owned(),
            ..ProviderProbe::default()
        };
        assert_eq!(
            resolve(&nowhere, &Config::default(), &AuthStore::empty()).unwrap_err(),
            "set a base URL"
        );
    }
}
