//! Workspace configuration loaded from `~/.kage/config.toml` with env overrides.
//!
//! Layered loading via [`figment`]: defaults are merged with the file
//! (if present), which is then overridden by environment variables prefixed
//! with `KAGE_`. Nested keys use `__` as the separator, for example
//! `KAGE_UI__THEME=catppuccin-mocha`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};

/// Top-level configuration for a kage process.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// LLM provider settings.
    pub provider: ProviderConfig,
    /// User interface settings (theme, mouse, etc.).
    pub ui: UiConfig,
    /// Plugin loader settings.
    pub plugins: PluginsConfig,
    /// Sandbox settings (backend selection, warnings, network).
    pub sandbox: SandboxConfig,
    /// Keybinding overrides.
    pub keybindings: KeybindingsConfig,
}

impl Config {
    /// Path to the default config file: `$HOME/.kage/config.toml`.
    ///
    /// Returns `None` when the home directory cannot be determined.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".kage").join("config.toml"))
    }

    /// Load configuration from `path`, merging with defaults and env overrides.
    ///
    /// A missing file is not an error: defaults plus env are returned.
    pub fn load(path: &Path) -> Result<Self, Box<figment::Error>> {
        Figment::new()
            .merge(Serialized::defaults(Self::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("KAGE_").split("__"))
            .extract()
            .map_err(Box::new)
    }

    /// Load configuration from [`Self::default_path`], or pure defaults plus
    /// env if no home directory is available.
    pub fn load_default() -> Result<Self, Box<figment::Error>> {
        match Self::default_path() {
            Some(p) => Self::load(&p),
            None => Figment::new()
                .merge(Serialized::defaults(Self::default()))
                .merge(Env::prefixed("KAGE_").split("__"))
                .extract()
                .map_err(Box::new),
        }
    }
}

/// LLM provider configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Model identifier in `provider:model` form.
    pub default_model: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            default_model: "anthropic:claude-sonnet-4-6".into(),
        }
    }
}

/// User interface configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Theme name (loaded from bundled themes or `~/.kage/themes/<name>.toml`).
    pub theme: String,
    /// Whether mouse events are captured by the TUI.
    pub mouse: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "default".into(),
            mouse: true,
        }
    }
}

/// Plugin loader configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Override the default plugin directory `~/.kage/plugins/`.
    pub dir: Option<PathBuf>,
    /// If non-empty, only plugins in this allowlist are loaded.
    pub enabled: Vec<String>,
}

/// Sandbox backend selection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxBackend {
    /// No isolation (default in 0.1).
    #[default]
    Local,
    /// Linux bubblewrap (post-0.1).
    Bubblewrap,
    /// macOS sandbox-exec (post-0.1).
    SandboxExec,
}

/// Sandbox configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    /// Which sandbox implementation to use.
    pub backend: SandboxBackend,
    /// Suppress the "running unsandboxed" startup warning.
    pub suppress_warning: bool,
    /// Hosts allowed for outbound network access from sandboxed tools.
    pub network_allowlist: Vec<String>,
}

/// Keybinding overrides expressed as a flat map of chord to command name.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    /// Map of keybinding chord (for example `<leader>cf`) to command name.
    pub bindings: BTreeMap<String, String>,
}

#[cfg(test)]
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;

    #[test]
    fn default_provider_model_is_anthropic_sonnet() {
        let cfg = Config::default();
        assert_eq!(cfg.provider.default_model, "anthropic:claude-sonnet-4-6");
    }

    #[test]
    fn missing_file_yields_defaults() {
        figment::Jail::expect_with(|jail| {
            let cfg = Config::load(jail.directory().join("nope.toml").as_path()).unwrap();
            assert_eq!(cfg, Config::default());
            Ok(())
        });
    }

    #[test]
    fn file_overrides_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [ui]
                theme = "tokyo-night"
                mouse = false

                [provider]
                default_model = "openai:gpt-4o"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.ui.theme, "tokyo-night");
            assert!(!cfg.ui.mouse);
            assert_eq!(cfg.provider.default_model, "openai:gpt-4o");
            Ok(())
        });
    }

    #[test]
    fn env_overrides_file() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [ui]
                theme = "tokyo-night"
                "#,
            )?;
            jail.set_env("KAGE_UI__THEME", "catppuccin-mocha");
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.ui.theme, "catppuccin-mocha");
            Ok(())
        });
    }

    #[test]
    fn sandbox_backend_serializes_kebab_case() {
        let json = serde_json::to_string(&SandboxBackend::SandboxExec).unwrap();
        assert_eq!(json, "\"sandbox-exec\"");
    }
}
