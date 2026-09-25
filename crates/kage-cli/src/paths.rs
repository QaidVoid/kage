//! The XDG directories kage reads and writes.

use std::path::PathBuf;

/// Resolve `$XDG_DATA_HOME/kage` (default `~/.local/share/kage`).
pub(crate) fn data_root() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_DATA_HOME", ".local/share")?.join("kage"))
}

/// Resolve `$XDG_STATE_HOME/kage` (default `~/.local/state/kage`).
pub(crate) fn state_root() -> Result<PathBuf, String> {
    kage_core::config::Config::state_dir().ok_or_else(|| "no home directory".to_owned())
}

/// Resolve `$XDG_CACHE_HOME/kage` (default `~/.cache/kage`).
pub(crate) fn cache_root() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_CACHE_HOME", ".cache")?.join("kage"))
}

/// The model cache `kage models refresh` writes:
/// `$XDG_CACHE_HOME/kage/models.json`.
pub(crate) fn models_cache_path() -> Result<PathBuf, String> {
    Ok(cache_root()?.join("models.json"))
}

/// Resolve the XDG-style directory holding session files:
/// `$XDG_DATA_HOME/kage/sessions` (default `~/.local/share/kage/sessions`).
pub(crate) fn sessions_dir() -> Result<PathBuf, String> {
    Ok(data_root()?.join("sessions"))
}

/// Resolve the kage config directory: `$XDG_CONFIG_HOME/kage` (default
/// `~/.config/kage`), the same resolution as `Config::default_path`. It
/// holds `config.toml`, the trusted `init.lua` and its `lua/` modules.
pub(crate) fn config_dir() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("kage"))
}

/// Resolve the plugin directory: `[plugins] dir` from the user config
/// when set (path semantics in [`resolve_plugin_dir`]), else the XDG
/// default `$XDG_CONFIG_HOME/kage/plugins` (default `~/.config/kage/plugins`).
pub(crate) fn plugins_dir() -> Result<PathBuf, String> {
    if let Ok(cfg) = kage_core::config::Config::load_default()
        && let Some(dir) = cfg.plugins.dir
    {
        return Ok(resolve_plugin_dir(dir));
    }
    Ok(config_dir()?.join("plugins"))
}

/// Apply `[plugins] dir` path semantics: absolute paths as-is, `~`
/// expanded to the home directory, relative paths resolved against the
/// kage config directory (`~/.config/kage`). Pure so tests need no
/// environment isolation.
fn resolve_plugin_dir(dir: PathBuf) -> PathBuf {
    if dir.is_absolute() {
        return dir;
    }
    if let Ok(rest) = dir.strip_prefix("~")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    match config_dir() {
        Ok(base) => base.join(dir),
        Err(_) => dir,
    }
}

/// Resolve the XDG-style user theme directory:
/// `$XDG_CONFIG_HOME/kage/themes` (default `~/.config/kage/themes`).
pub(crate) fn themes_dir() -> Result<PathBuf, String> {
    Ok(config_dir()?.join("themes"))
}

/// Resolve an XDG base directory: prefers `$ENV_VAR` if set and non-empty,
/// otherwise falls back to `$HOME/<fallback_subpath>`.
pub(crate) fn xdg_dir(env_var: &str, fallback_subpath: &str) -> Result<PathBuf, String> {
    if let Ok(v) = std::env::var(env_var)
        && !v.is_empty()
    {
        return Ok(PathBuf::from(v));
    }
    let home = dirs::home_dir().ok_or_else(|| "no home directory".to_owned())?;
    Ok(home.join(fallback_subpath))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_dir_override_absolute_asis() {
        let p = resolve_plugin_dir(PathBuf::from("/opt/kage-plugins"));
        assert_eq!(p, PathBuf::from("/opt/kage-plugins"));
    }

    #[test]
    fn plugin_dir_override_tilde_expands_home() {
        let p = resolve_plugin_dir(PathBuf::from("~/my-plugins"));
        let home = dirs::home_dir().expect("test needs a home directory");
        assert_eq!(p, home.join("my-plugins"));
    }

    #[test]
    fn plugin_dir_override_relative_resolves_against_config_dir() {
        let p = resolve_plugin_dir(PathBuf::from("extra-plugins"));
        let base = xdg_dir("XDG_CONFIG_HOME", ".config").expect("test needs a home directory");
        assert_eq!(p, base.join("kage").join("extra-plugins"));
    }
}
