//! `_kage/plugins/install` and `_kage/plugins/remove`: plugin files in
//! the plugin directory. A plugin is one `.lua` file; it loads when the
//! next session opens.

use std::path::Path;
use std::time::Duration;

use kage_acp::acp::{PluginInstallRequest, PluginRemoveRequest};
use kage_jsonrpc::RpcError;

/// The largest plugin read.
const MAX_BYTES: u64 = 1024 * 1024;

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::new(-32602, message.into())
}

/// Whether `name` may name a plugin file: letters, digits, dashes and
/// underscores, not starting with `@`, which kage keeps for itself.
fn check_name(name: &str) -> Result<(), RpcError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(invalid(format!(
            "`{name}` is not a plugin name: use letters, digits, dashes or underscores"
        )));
    }
    Ok(())
}

/// Installs the plugin `req` names into `dir` and returns its name.
pub(super) fn install(dir: &Path, req: &PluginInstallRequest) -> Result<String, RpcError> {
    let source = req.source.trim();
    let stem = source
        .rsplit(['/', '\\'])
        .next()
        .and_then(|file| file.strip_suffix(".lua"))
        .unwrap_or_default();
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(stem)
        .to_owned();
    check_name(&name)?;
    let target = dir.join(format!("{name}.lua"));
    if target.exists() && !req.replace {
        return Err(invalid(format!(
            "a plugin named {name} is already installed"
        )));
    }
    let text = if source.starts_with("http://") {
        return Err(invalid(format!(
            "{source} uses plain http; a plugin source must be an https URL or a local path"
        )));
    } else if source.starts_with("https://") {
        fetch(source)?
    } else {
        let path = match source.strip_prefix("~/") {
            Some(rest) => dirs::home_dir()
                .ok_or_else(|| invalid("no home directory"))?
                .join(rest),
            None => source.into(),
        };
        read_local(&path)?
    };
    kage_plugin::check_syntax(&text)
        .map_err(|e| invalid(format!("{source} is not a Lua plugin: {e}")))?;
    std::fs::create_dir_all(dir).map_err(|e| RpcError::internal(e.to_string()))?;
    kage_core::fsutil::atomic_write(&target, text.as_bytes())
        .map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(name)
}

/// Removes plugin `req.name` from `dir`.
pub(super) fn remove(dir: &Path, req: &PluginRemoveRequest) -> Result<(), RpcError> {
    check_name(&req.name)?;
    let target = dir.join(format!("{}.lua", req.name));
    if !target.is_file() {
        return Err(invalid(format!(
            "no plugin named {} is installed",
            req.name
        )));
    }
    std::fs::remove_file(&target).map_err(|e| RpcError::internal(e.to_string()))
}

/// The plugin file at `path`, capped at [`MAX_BYTES`] like a fetched
/// one, so a huge file is refused instead of loaded.
fn read_local(path: &Path) -> Result<String, RpcError> {
    use std::io::Read as _;

    let mut reader = std::fs::File::open(path)
        .map_err(|e| invalid(format!("cannot read {}: {e}", path.display())))?
        .take(MAX_BYTES);
    let mut text = String::new();
    reader
        .read_to_string(&mut text)
        .map_err(|e| invalid(format!("cannot read {}: {e}", path.display())))?;
    let mut extra = [0u8; 1];
    if reader.get_ref().read(&mut extra).unwrap_or(0) != 0 {
        return Err(invalid(format!(
            "{} is larger than the {} byte plugin limit",
            path.display(),
            MAX_BYTES
        )));
    }
    Ok(text)
}

/// GETs an https plugin source. The host is resolved and vetted before
/// the dial, and the guarded agent re-vets every redirect hop, so a
/// source URL cannot reach an internal address.
fn fetch(url: &str) -> Result<String, RpcError> {
    use std::io::Read as _;

    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|e| invalid(format!("{url} is not a URL: {e}")))?;
    let host = uri.host().unwrap_or_default();
    if host.is_empty() {
        return Err(invalid(format!("{url} has no host")));
    }
    super::directory::vet(host, uri.port_u16().unwrap_or(443))
        .map_err(|e| invalid(format!("{url}: {e}")))?;
    let agent = kage_tools::ssrf::guarded_agent(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .build(),
    );
    let response = agent
        .get(url)
        .header("user-agent", concat!("kage/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|e| invalid(format!("get {url}: {e}")))?;
    let mut text = String::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_BYTES)
        .read_to_string(&mut text)
        .map_err(|e| invalid(format!("read {url}: {e}")))?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use kage_acp::acp::{PluginInstallRequest, PluginRemoveRequest};

    use super::{MAX_BYTES, install, remove};

    #[test]
    fn a_plugin_installs_from_a_path_and_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let plugins = dir.path().join("plugins");
        let file = dir.path().join("clock.lua");
        std::fs::write(&file, "kage.log('hi')\n").unwrap();
        let req = PluginInstallRequest {
            source: file.display().to_string(),
            ..PluginInstallRequest::default()
        };
        assert_eq!(install(&plugins, &req).unwrap(), "clock");
        assert!(plugins.join("clock.lua").is_file());
        let again = install(&plugins, &req).unwrap_err();
        assert!(
            again.message.contains("already installed"),
            "{}",
            again.message
        );
        let replace = PluginInstallRequest {
            replace: true,
            ..req.clone()
        };
        install(&plugins, &replace).unwrap();
        let gone = PluginRemoveRequest {
            name: "clock".into(),
        };
        remove(&plugins, &gone).unwrap();
        assert!(!plugins.join("clock.lua").exists());
        assert!(remove(&plugins, &gone).is_err());
    }

    #[test]
    fn a_broken_or_badly_named_plugin_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("broken.lua");
        std::fs::write(&broken, "local x = = 1\n").unwrap();
        let req = PluginInstallRequest {
            source: broken.display().to_string(),
            ..PluginInstallRequest::default()
        };
        let plugins = dir.path().join("plugins");
        let err = install(&plugins, &req).unwrap_err();
        assert!(err.message.contains("not a Lua plugin"), "{}", err.message);
        let reserved = PluginInstallRequest {
            name: Some("@kage".into()),
            ..req
        };
        assert!(install(&plugins, &reserved).is_err());
    }

    #[test]
    fn a_plain_http_source_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let req = PluginInstallRequest {
            source: "http://127.0.0.1/clock.lua".into(),
            ..PluginInstallRequest::default()
        };
        let err = install(&dir.path().join("plugins"), &req).unwrap_err();
        assert!(err.message.contains("https"), "{}", err.message);
        assert!(
            !dir.path().join("plugins").join("clock.lua").exists(),
            "nothing is installed from a refused source"
        );
    }

    #[test]
    fn an_https_source_on_an_internal_address_is_refused_before_the_dial() {
        let dir = tempfile::tempdir().unwrap();
        for source in [
            "https://127.0.0.1/clock.lua",
            "https://10.0.0.1/clock.lua",
            "https://[::1]/clock.lua",
        ] {
            let req = PluginInstallRequest {
                source: source.into(),
                ..PluginInstallRequest::default()
            };
            let err = install(&dir.path().join("plugins"), &req).unwrap_err();
            assert!(err.message.contains("non-routable"), "{source}: {err}");
        }
    }

    #[test]
    fn an_oversized_local_plugin_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.lua");
        let max = usize::try_from(MAX_BYTES).unwrap_or(usize::MAX);
        let body = "kage.log('hi')\n".repeat(max / 8 + 1);
        assert!(body.len() > max);
        std::fs::write(&big, body).unwrap();
        let req = PluginInstallRequest {
            source: big.display().to_string(),
            ..PluginInstallRequest::default()
        };
        let err = install(&dir.path().join("plugins"), &req).unwrap_err();
        assert!(err.message.contains("larger than"), "{}", err.message);
    }
}
