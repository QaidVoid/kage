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
    /// Agent-loop tuning (`[loop]`): compaction threshold, etc.
    #[serde(rename = "loop")]
    pub loop_settings: LoopSettings,
    /// External ACP agents usable as `acp:<name>` (`[acp.agents.*]`).
    pub acp: AcpConfig,
    /// External MCP tool servers (`[mcp.servers.*]`).
    pub mcp: McpConfig,
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

    /// Write this config back to `path`, preserving the existing
    /// file's comments, formatting, key order, and any keys this
    /// version does not model. Modeled values are merged in over the
    /// existing document via `toml_edit`; a missing file is created
    /// from scratch and the parent directory is made as needed.
    ///
    /// The write is atomic: the TOML is rendered to a sibling temp
    /// file and renamed over `path`, so an interrupted save never
    /// truncates an existing config.
    pub fn save(&self, path: &Path) -> Result<()> {
        use toml_edit::DocumentMut;

        let mut doc = match std::fs::read_to_string(path) {
            Ok(existing) => existing
                .parse::<DocumentMut>()
                .map_err(|e| crate::error::Error::ConfigWrite(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
            Err(e) => return Err(e.into()),
        };
        // Serialize via the `toml` crate so nested structs render as
        // block tables (`[ui]`), not inline (`ui = { .. }`); merging
        // block-into-block is what keeps the user's section comments.
        let fresh = toml::to_string(self)
            .map_err(|e| crate::error::Error::ConfigWrite(e.to_string()))?
            .parse::<DocumentMut>()
            .map_err(|e| crate::error::Error::ConfigWrite(e.to_string()))?;
        merge_table(doc.as_table_mut(), fresh.as_table());

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, doc.to_string().as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Recursively copy every value from `src` into `dst`: overwrite
/// leaves and arrays, recurse into sub-tables. Keys present only in
/// `dst` (comments, blank lines, and any keys this version does not
/// model) are left untouched, so a hand-edited config keeps its
/// annotations across a settings-dialog save.
fn merge_table(dst: &mut toml_edit::Table, src: &toml_edit::Table) {
    for (key, src_item) in src {
        match (dst.get_mut(key), src_item) {
            (Some(toml_edit::Item::Table(dst_sub)), toml_edit::Item::Table(src_sub)) => {
                merge_table(dst_sub, src_sub);
            }
            _ => {
                dst.insert(key, src_item.clone());
            }
        }
    }
}

/// Agent-loop tuning persisted under `[loop]`. Mirrors the subset of
/// `kage-loop`'s `LoopConfig` the user is allowed to set from config /
/// the settings dialog; the host maps it onto the real loop config at
/// startup.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopSettings {
    /// Trigger compaction once estimated token usage exceeds this
    /// fraction of the model's context window. The loop clamps it to
    /// `(0.0, 1.0]`.
    pub compaction_threshold: f32,
}

impl Default for LoopSettings {
    fn default() -> Self {
        Self {
            compaction_threshold: 0.8,
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
    /// Theme name (loaded from bundled themes or
    /// `~/.config/kage/themes/<name>.toml`).
    pub theme: String,
    /// Whether mouse events are captured by the TUI.
    pub mouse: bool,
    /// Prompt-input editing model.
    pub editor: EditorMode,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "default".into(),
            mouse: true,
            editor: EditorMode::default(),
        }
    }
}

/// How the prompt input behaves.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditorMode {
    /// Modal editing in the vim style: a normal mode, an insert
    /// mode, and a visual mode, with motions, operators, and
    /// registers. The historical (and default) behavior.
    #[default]
    Vim,
    /// Always-editable, non-modal editing. Readline / Emacs keys do
    /// all editing (`Ctrl+A`/`E`/`W`/`U`/`K`/`Y`, `Ctrl+/`,
    /// `Alt+B`/`F`); `Esc` cancels the in-flight turn; the buffer
    /// scrolls with `PageUp` / `PageDown` and the mouse. No modal
    /// states.
    Modeless,
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

/// External ACP agents kage can drive as a provider. Each entry is
/// addressable as the model id `acp:<name>`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AcpConfig {
    /// Map of agent name to its launch spec.
    pub agents: BTreeMap<String, AcpAgent>,
}

/// How to launch one external ACP agent over stdio.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcpAgent {
    /// Executable to spawn.
    pub command: String,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// External MCP tool servers, keyed by name (`[mcp.servers.<name>]`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Map of server name to its launch spec.
    pub servers: BTreeMap<String, McpServer>,
}

/// How to launch one external MCP server over stdio.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpServer {
    /// Executable to spawn (e.g. `npx`).
    pub command: String,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// When `true`, the server is configured but not spawned.
    #[serde(default)]
    pub disabled: bool,
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
    fn acp_agents_default_empty_and_parse_from_table() {
        assert!(Config::default().acp.agents.is_empty());
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [acp.agents.claude-code]
                command = "npx"
                args = ["-y", "@zed-industries/claude-code-acp"]

                [acp.agents.claude-code.env]
                ANTHROPIC_API_KEY = "xxx"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let agent = cfg.acp.agents.get("claude-code").expect("agent parsed");
            assert_eq!(agent.command, "npx");
            assert_eq!(agent.args, ["-y", "@zed-industries/claude-code-acp"]);
            assert_eq!(
                agent.env.get("ANTHROPIC_API_KEY").map(String::as_str),
                Some("xxx")
            );
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
    fn editor_mode_defaults_vim_and_parses_modeless() {
        assert_eq!(Config::default().ui.editor, EditorMode::Vim);
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [ui]
                editor = "modeless"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.ui.editor, EditorMode::Modeless);
            Ok(())
        });
    }

    #[test]
    fn editor_mode_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&EditorMode::Modeless).unwrap(),
            "\"modeless\""
        );
        assert_eq!(serde_json::to_string(&EditorMode::Vim).unwrap(), "\"vim\"");
    }

    #[test]
    fn loop_section_defaults_and_parses() {
        assert!((Config::default().loop_settings.compaction_threshold - 0.8).abs() < f32::EPSILON);
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r"
                [loop]
                compaction_threshold = 0.6
                ",
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert!((cfg.loop_settings.compaction_threshold - 0.6).abs() < f32::EPSILON);
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
    fn save_preserves_comments_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# kage config header\n\
             \n\
             [ui]\n\
             # theme chosen by hand\n\
             theme = \"old\"\n\
             \n\
             [custom.plugin]\n\
             # a section this version does not model\n\
             retries = 5\n",
        )
        .unwrap();
        let mut cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.ui.theme, "old");
        cfg.ui.theme = "tokyo-night".to_owned();
        cfg.save(&path).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# kage config header"), "header comment kept");
        assert!(body.contains("[custom.plugin]"), "unknown section kept");
        assert!(body.contains("retries = 5"), "unknown key kept");
        assert!(
            body.contains("# a section this version does not model"),
            "comment on unknown section kept"
        );
        assert!(body.contains("tokyo-night"), "modeled value updated");
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.ui.theme, "tokyo-night");
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
