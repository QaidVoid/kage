//! The directories kage reads and writes: XDG-style on Unix, the
//! matching `dirs` Known Folders on Windows.

use std::path::{Path, PathBuf};

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

/// Resolve the directory for sockets and other per-boot files:
/// `$XDG_RUNTIME_DIR/kage`, or `$XDG_DATA_HOME/kage/run` when no
/// runtime directory is set.
#[cfg(unix)]
pub(crate) fn runtime_dir() -> Result<PathBuf, String> {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir).join("kage")),
        _ => Ok(data_root()?.join("run")),
    }
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
        && let Some(dir) = cfg.plugins.dir.as_deref()
    {
        return Ok(resolve_plugin_dir(dir));
    }
    Ok(config_dir()?.join("plugins"))
}

/// Apply `[plugins] dir` path semantics: absolute paths as-is, `~`
/// expanded to the home directory, `~user` left unexpanded (it
/// resolves as a relative path against the kage config directory),
/// and relative paths resolved against the kage config directory
/// (`~/.config/kage`). The value is cleaned with
/// [`kage_core::fsutil::unquote_and_trim`] first, so a pasted or
/// quoted entry carries neither quotes nor padding spaces. Pure so
/// tests need no environment isolation.
fn resolve_plugin_dir(dir: &Path) -> PathBuf {
    let lossy = dir.to_string_lossy();
    let dir = Path::new(kage_core::fsutil::unquote_and_trim(&lossy));
    if dir.is_absolute() {
        return dir.to_path_buf();
    }
    if let Ok(rest) = dir.strip_prefix("~")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    match config_dir() {
        Ok(base) => base.join(dir),
        Err(_) => dir.to_path_buf(),
    }
}

/// Resolve the XDG-style user theme directory:
/// `$XDG_CONFIG_HOME/kage/themes` (default `~/.config/kage/themes`).
pub(crate) fn themes_dir() -> Result<PathBuf, String> {
    Ok(config_dir()?.join("themes"))
}

/// Resolve a base directory: prefers `$ENV_VAR` if set to something
/// non-empty once quotes and padding are stripped
/// ([`kage_core::fsutil::unquote_and_trim`], so a quoted or spaced
/// value still names the real directory), otherwise the platform
/// default for that tier: the `HOME`-relative XDG path on Unix, and
/// the matching `dirs` Known Folder on Windows (`%APPDATA%` for
/// config and data, `%LOCALAPPDATA%` for cache and state).
pub(crate) fn xdg_dir(env_var: &str, fallback_subpath: &str) -> Result<PathBuf, String> {
    if let Ok(v) = std::env::var(env_var) {
        let v = kage_core::fsutil::unquote_and_trim(&v);
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    #[cfg(windows)]
    return match env_var {
        "XDG_CONFIG_HOME" => dirs::config_dir(),
        "XDG_DATA_HOME" => dirs::data_dir(),
        "XDG_CACHE_HOME" => dirs::cache_dir(),
        "XDG_STATE_HOME" => dirs::data_local_dir(),
        _ => dirs::home_dir(),
    }
    .ok_or_else(|| "no home directory".to_owned());
    #[cfg(unix)]
    dirs::home_dir()
        .ok_or_else(|| "no home directory".to_owned())
        .map(|home| home.join(fallback_subpath))
}

#[cfg(test)]
#[expect(
    clippy::result_large_err,
    reason = "figment::Jail closures must return figment::Error"
)]
mod tests {
    use super::*;

    #[test]
    fn plugin_dir_override_absolute_asis() {
        let dir = std::env::temp_dir().join("kage-plugins");
        assert_eq!(resolve_plugin_dir(&dir), dir);
    }

    #[test]
    fn plugin_dir_override_tilde_expands_home() {
        let p = resolve_plugin_dir(Path::new("~/my-plugins"));
        let home = dirs::home_dir().expect("test needs a home directory");
        assert_eq!(p, home.join("my-plugins"));
    }

    #[test]
    fn plugin_dir_override_relative_resolves_against_config_dir() {
        let p = resolve_plugin_dir(Path::new("extra-plugins"));
        let base = xdg_dir("XDG_CONFIG_HOME", ".config").expect("test needs a home directory");
        assert_eq!(p, base.join("kage").join("extra-plugins"));
    }

    #[test]
    fn quoted_and_spaced_xdg_env_values_resolve_to_the_clean_path() {
        figment::Jail::expect_with(|jail| {
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            let quoted = format!("\"{}\"", home.join("config").display());
            jail.set_env("XDG_CONFIG_HOME", quoted.as_str());
            assert_eq!(config_dir().unwrap(), home.join("config").join("kage"));
            let spaced = format!(" {} ", home.join("config").display());
            jail.set_env("XDG_CONFIG_HOME", spaced.as_str());
            assert_eq!(config_dir().unwrap(), home.join("config").join("kage"));
            Ok(())
        });
    }

    #[test]
    fn an_empty_xdg_env_value_falls_back_to_the_platform_directory() {
        figment::Jail::expect_with(|jail| {
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            jail.set_env("XDG_CONFIG_HOME", "");
            // `dirs::home_dir()` follows `HOME` on Unix but reads the
            // Windows Known Folder API on Windows, where the jail's
            // `HOME` is inert; expect that platform directory.
            let expected = if cfg!(windows) {
                dirs::config_dir().expect("test needs a config directory")
            } else {
                dirs::home_dir()
                    .expect("test needs a home directory")
                    .join(".config")
            };
            assert_eq!(config_dir().unwrap(), expected.join("kage"));
            Ok(())
        });
    }

    #[test]
    fn a_quoted_and_spaced_tilde_plugin_dir_still_expands() {
        figment::Jail::expect_with(|jail| {
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            jail.set_env(
                "XDG_CONFIG_HOME",
                home.join(".config").to_string_lossy().as_ref(),
            );
            let expected = dirs::home_dir().expect("test needs a home directory");
            let p = resolve_plugin_dir(Path::new(" \"~/my-plugins\" "));
            assert_eq!(p, expected.join("my-plugins"));
            Ok(())
        });
    }

    #[test]
    fn a_tilde_user_plugin_dir_resolves_against_the_config_dir() {
        figment::Jail::expect_with(|jail| {
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            jail.set_env(
                "XDG_CONFIG_HOME",
                home.join(".config").to_string_lossy().as_ref(),
            );
            let p = resolve_plugin_dir(Path::new("~other/plugins"));
            assert_eq!(
                p,
                home.join(".config")
                    .join("kage")
                    .join("~other")
                    .join("plugins")
            );
            Ok(())
        });
    }
}
