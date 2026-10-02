//! `_kage/config/set`: replaces or removes one entry of the user
//! `config.toml` under a section the settings pages edit. The edited
//! file must load and pass the checks kage runs at startup before it
//! is written; a refused edit changes nothing.
//!
//! The snapshot `_kage/config/get` serves blanks secret values, so a
//! client that sends a redacted value back keeps the value on file.

use std::path::Path;

use kage_core::config::Config;
use kage_core::config_edit;
use kage_jsonrpc::RpcError;
use serde_json::Value;

use super::REDACTED;

/// The top-level tables a client may edit.
const SECTIONS: [&str; 5] = ["providers", "mcp", "permissions", "plugins", "acp"];

/// Sets the entry at `keys` of the config at `path` to `value`, or
/// removes it when `value` is `None`.
pub(super) fn set(path: &Path, keys: &[String], value: Option<&Value>) -> Result<(), RpcError> {
    let invalid = |message: String| RpcError::new(-32602, message);
    let Some(section) = keys.first() else {
        return Err(invalid("name the entry to set".to_owned()));
    };
    if !SECTIONS.contains(&section.as_str()) {
        return Err(invalid(format!(
            "`{section}` cannot be set here; settable sections are {}",
            SECTIONS.join(", ")
        )));
    }
    if keys.iter().any(String::is_empty) {
        return Err(invalid("a key on the path is empty".to_owned()));
    }
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    let value = match value {
        Some(value) => {
            let old = config_edit::current(path, &keys).map_err(|e| invalid(e.to_string()))?;
            Some(unredacted(value, old.as_ref(), &keys.join("."))?)
        }
        None => None,
    };
    let text =
        config_edit::edited(path, &keys, value.as_ref()).map_err(|e| invalid(e.to_string()))?;
    let config = config_edit::parse(&text).map_err(|e| invalid(e.to_string()))?;
    validate(&config).map_err(invalid)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RpcError::internal(e.to_string()))?;
    }
    kage_core::fsutil::atomic_write(path, text.as_bytes())
        .map_err(|e| RpcError::internal(e.to_string()))
}

/// `value` with every redacted string replaced by the value `old` holds
/// at the same place. `at` names the place for the error a redacted
/// value with nothing on file gets.
pub(super) fn unredacted(value: &Value, old: Option<&Value>, at: &str) -> Result<Value, RpcError> {
    Ok(match value {
        Value::String(text) if text == REDACTED => old.cloned().ok_or_else(|| {
            RpcError::new(-32602, format!("{at} is redacted and has no value on file"))
        })?,
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let old = old.and_then(|old| old.get(key));
                    Ok((key.clone(), unredacted(value, old, &format!("{at}.{key}"))?))
                })
                .collect::<Result<_, RpcError>>()?,
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(ix, value)| {
                    let old = old.and_then(|old| old.get(ix));
                    unredacted(value, old, &format!("{at}[{ix}]"))
                })
                .collect::<Result<_, RpcError>>()?,
        ),
        value => value.clone(),
    })
}

/// The checks kage runs on these sections when it starts or opens a
/// session.
fn validate(config: &Config) -> Result<(), String> {
    crate::validate_providers(&config.providers)?;
    config
        .permissions
        .validate()
        .map_err(|e| format!("permissions: {e}"))?;
    for (name, server) in &config.mcp.servers {
        match (&server.command, &server.url) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "[mcp.servers.{name}] sets both `command` and `url`; set exactly one"
                ));
            }
            (None, None) => {
                return Err(format!(
                    "[mcp.servers.{name}] needs `command` (stdio) or `url` (http)"
                ));
            }
            _ => {}
        }
    }
    for (name, agent) in &config.acp.agents {
        if agent.command.trim().is_empty() {
            return Err(format!("[acp.agents.{name}] needs a command"));
        }
    }
    for (plugin, grants) in &config.plugins.capabilities {
        for grant in grants {
            kage_plugin::check_capability(grant)
                .map_err(|e| format!("[plugins.capabilities] {plugin}: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::set;

    fn keys(path: &str) -> Vec<String> {
        path.split('.').map(str::to_owned).collect()
    }

    #[test]
    fn a_server_lands_with_the_file_comments_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[ui]\ntheme = \"kage-dawn\"\n").unwrap();
        let hub = json!({ "url": "https://hub/mcp", "headers": { "Authorization": "Bearer t" } });
        set(&path, &keys("mcp.servers.hub"), Some(&hub)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\n[ui]"), "{text}");
        assert!(text.contains("[mcp.servers.hub]"), "{text}");
    }

    #[test]
    fn a_redacted_value_keeps_the_one_on_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[mcp.servers.hub]\nurl = \"https://hub\"\nheaders = { Authorization = \"Bearer secret\" }\n",
        )
        .unwrap();
        let edit = json!({
            "url": "https://hub/v2",
            "headers": { "Authorization": "<redacted>" },
        });
        set(&path, &keys("mcp.servers.hub"), Some(&edit)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("Bearer secret") && text.contains("hub/v2"),
            "{text}"
        );

        let fresh = json!({ "url": "https://x", "headers": { "X": "<redacted>" } });
        let err = set(&path, &keys("mcp.servers.new"), Some(&fresh)).unwrap_err();
        assert!(
            err.message.contains("mcp.servers.new.headers.X"),
            "{}",
            err.message
        );
    }

    #[test]
    fn an_invalid_edit_is_refused_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let both = json!({ "command": "npx", "url": "https://hub" });
        let err = set(&path, &keys("mcp.servers.hub"), Some(&both)).unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("exactly one"), "{}", err.message);
        let shadow = json!({ "base_url": "http://x", "models": [{ "id": "m", "name": "M" }] });
        assert!(set(&path, &keys("providers.custom.anthropic"), Some(&shadow)).is_err());
        let grant = json!(["telepathy"]);
        assert!(set(&path, &keys("plugins.capabilities.tokps"), Some(&grant)).is_err());
        assert!(set(&path, &keys("ui.theme"), Some(&json!("x"))).is_err());
        assert!(set(&path, &keys("mcp.servers.hub.args"), Some(&json!(3))).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn a_removal_drops_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[permissions.tools.shell]\ndefault = \"ask\"\n\n[permissions.tools.write]\ndefault = \"deny\"\n",
        )
        .unwrap();
        set(&path, &keys("permissions.tools.shell"), None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("shell") && text.contains("write"), "{text}");
    }
}
