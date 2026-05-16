//! Workspace configuration loaded from `~/.config/kage/config.toml` with env overrides.
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

use crate::error::Result;

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
    /// Path to the user config file, XDG-resolved:
    /// `$XDG_CONFIG_HOME/kage/config.toml`, or `~/.config/kage/config.toml`
    /// when `XDG_CONFIG_HOME` is unset. This mirrors how the rest of kage
    /// resolves config-tier paths (plugins, skills); only the home
    /// directory case returns `None`.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        let base = match std::env::var("XDG_CONFIG_HOME") {
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => dirs::home_dir()?.join(".config"),
        };
        Some(base.join("kage").join("config.toml"))
    }

    /// Load configuration from `path`, merging with defaults and env overrides.
    ///
    /// A missing file is not an error: defaults plus env are returned.
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Figment::new()
            .merge(Serialized::defaults(Self::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("KAGE_").split("__"))
            .extract()?)
    }

    /// Load configuration from [`Self::default_path`], or pure defaults plus
    /// env if no home directory is available.
    pub fn load_default() -> Result<Self> {
        match Self::default_path() {
            Some(p) => Self::load(&p),
            None => Ok(Figment::new()
                .merge(Serialized::defaults(Self::default()))
                .merge(Env::prefixed("KAGE_").split("__"))
                .extract()?),
        }
    }

    /// Path to the project-local config inside `workdir`:
    /// `<workdir>/.kage/config.toml`.
    #[must_use]
    pub fn project_path(workdir: &Path) -> PathBuf {
        workdir.join(".kage").join("config.toml")
    }

    /// Load layered configuration: defaults < user file < project file < env.
    ///
    /// The project file is `<workdir>/.kage/config.toml`. Either file may be
    /// absent; only the layers that exist contribute. Env overrides win
    /// over both files just like in [`Self::load`].
    pub fn load_layered(workdir: &Path) -> Result<Self> {
        let mut figment = Figment::new().merge(Serialized::defaults(Self::default()));
        if let Some(user) = Self::default_path() {
            figment = figment.merge(Toml::file(user));
        }
        figment = figment.merge(Toml::file(Self::project_path(workdir)));
        figment = figment.merge(Env::prefixed("KAGE_").split("__"));
        Ok(figment.extract()?)
    }

    /// Serialize this config and write it to `path`, creating the parent
    /// directory if needed. The write is atomic: the TOML is written to a
    /// sibling temp file and renamed over `path`, so an interrupted save
    /// never truncates an existing config.
    ///
    /// This rewrites the whole file from the struct, so comments and any
    /// keys this version does not model are dropped. The settings dialog
    /// is the intended caller; hand-edited files keep working until the
    /// first dialog save.
    pub fn save(&self, path: &Path) -> Result<()> {
        let body = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
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
    /// Theme name (loaded from bundled themes or
    /// `~/.config/kage/themes/<name>.toml`).
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
    /// Override the default plugin directory `~/.config/kage/plugins/`.
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
    fn project_file_overrides_user_file() {
        figment::Jail::expect_with(|jail| {
            // Pin XDG_CONFIG_HOME at the jail so default_path resolves
            // the user config beneath it deterministically, regardless
            // of any XDG_CONFIG_HOME inherited by the test runner.
            let home = jail.directory().to_path_buf();
            jail.set_env("HOME", home.to_string_lossy().as_ref());
            let xdg_config = home.join(".config");
            jail.set_env("XDG_CONFIG_HOME", xdg_config.to_string_lossy().as_ref());
            let user_cfg = xdg_config.join("kage");
            std::fs::create_dir_all(&user_cfg).map_err(|e| figment::Error::from(e.to_string()))?;
            std::fs::write(
                user_cfg.join("config.toml"),
                r#"
                [ui]
                theme = "user-theme"
                mouse = true
                "#,
            )
            .map_err(|e| figment::Error::from(e.to_string()))?;
            let project = home.join("project");
            std::fs::create_dir_all(project.join(".kage"))
                .map_err(|e| figment::Error::from(e.to_string()))?;
            std::fs::write(
                project.join(".kage").join("config.toml"),
                r#"
                [ui]
                theme = "project-theme"
                "#,
            )
            .map_err(|e| figment::Error::from(e.to_string()))?;

            let cfg =
                Config::load_layered(&project).map_err(|e| figment::Error::from(e.to_string()))?;
            assert_eq!(cfg.ui.theme, "project-theme");
            // mouse not set in project file -> inherited from user file.
            assert!(cfg.ui.mouse);
            Ok(())
        });
    }

    #[test]
    fn sandbox_backend_serializes_kebab_case() {
        let json = serde_json::to_string(&SandboxBackend::SandboxExec).unwrap();
        assert_eq!(json, "\"sandbox-exec\"");
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let mut cfg = Config::default();
        cfg.ui.theme = "tokyo-night".to_owned();
        cfg.ui.mouse = false;
        cfg.provider.default_model = "anthropic:claude-opus-4-7".to_owned();
        cfg.save(&path).unwrap();
        // Parent directory was created and the file is valid TOML that
        // parses back to an equal config.
        assert!(path.exists());
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let tmp = path.with_extension("toml.tmp");
        assert!(!tmp.exists(), "temp file should be renamed away");
        assert!(path.exists());
    }

    #[test]
    fn save_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let mut cfg = Config::default();
        cfg.ui.theme = "ayu".to_owned();
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.ui.theme, "ayu");
    }
}
