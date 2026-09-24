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

use crate::permissions::PermissionsConfig;

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
    /// Custom providers and overrides for registered providers
    /// (`[providers.custom.*]` / `[providers.<provider-id>]`).
    #[serde(skip_serializing_if = "ProvidersConfig::is_default")]
    pub providers: ProvidersConfig,
    /// Tool permission rules (`[permissions]`): allow / ask / deny
    /// per tool, plus the opt-in path-confinement flag.
    #[serde(default, skip_serializing_if = "PermissionsConfig::is_default")]
    pub permissions: crate::permissions::PermissionsConfig,
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

    /// Directory for kage's mutable state, XDG-resolved:
    /// `$XDG_STATE_HOME/kage`, or `~/.local/state/kage` when
    /// `XDG_STATE_HOME` is unset. `None` only when there is no home
    /// directory.
    #[must_use]
    pub fn state_dir() -> Option<PathBuf> {
        let base = match std::env::var("XDG_STATE_HOME") {
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => dirs::home_dir()?.join(".local").join("state"),
        };
        Some(base.join("kage"))
    }

    /// Load layered configuration: defaults < user file < project file < env.
    ///
    /// The project file is `<workdir>/.kage/config.toml`. Either file may be
    /// absent; only the layers that exist contribute. Env overrides win
    /// over both files just like in [`Self::load`].
    ///
    /// The project file's `mcp`, `permissions` and
    /// `plugins.capabilities` tables are dropped unless the project is
    /// trusted (see [`crate::trust`]).
    pub fn load_layered(workdir: &Path) -> Result<Self> {
        let mut figment = Figment::new().merge(Serialized::defaults(Self::default()));
        if let Some(user) = Self::default_path() {
            figment = figment.merge(Toml::file(user));
        }
        figment = match crate::trust::filtered_project(workdir) {
            Some(filtered) => figment.merge(Toml::string(&filtered)),
            None => figment.merge(Toml::file(Self::project_path(workdir))),
        };
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

/// Custom providers and overrides for registered providers, loaded from
/// the `[providers]` section.
///
/// Two shapes share the section:
///
/// * `[providers.custom.<id>]` defines a brand-new provider with its own
///   id, base URL, and model list.
/// * `[providers.<registered-id>]` overrides settings of a provider kage
///   registers itself (base URL, key variable, extra headers).
///
/// The named `custom` field holds the first shape; every other key is
/// flattened into `overrides`, keyed by provider id.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    /// Providers defined in this config file, keyed by their id.
    pub custom: BTreeMap<String, CustomProviderConfig>,
    /// Overrides for providers kage registers itself, keyed by provider
    /// id (`[providers.deepseek]`).
    #[serde(flatten)]
    pub overrides: BTreeMap<String, BuiltinProviderOverride>,
}

impl ProvidersConfig {
    /// True when nothing is configured, so `save` skips the section.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.custom.is_empty() && self.overrides.is_empty()
    }

    /// Check the cross-references serde cannot express. `builtin_ids`
    /// lists the provider ids kage always registers (including `acp`);
    /// a custom provider must not shadow one. `overridable_ids` lists
    /// the ids a `[providers.<id>]` override may target: the built-in
    /// HTTP providers and the OpenAI-compatible ones, but not `acp`.
    ///
    /// Structural problems (missing `base_url`, missing `models`) are
    /// already load errors; this catches the semantic ones so
    /// `kage` refuses to start on a config it would silently ignore.
    pub fn validate(&self, builtin_ids: &[&str], overridable_ids: &[&str]) -> Result<()> {
        for (id, cfg) in &self.custom {
            if id.is_empty()
                || !id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                return Err(config_error(format!(
                    "[providers.custom.{id}] id must be non-empty lowercase letters, digits, or dashes"
                )));
            }
            if builtin_ids.contains(&id.as_str()) {
                return Err(config_error(format!(
                    "[providers.custom.{id}] id conflicts with registered provider `{id}`; use [providers.{id}] to override it instead"
                )));
            }
            if cfg.models.is_empty() {
                return Err(config_error(format!(
                    "[providers.custom.{id}] must declare at least one [[providers.custom.{id}.models]] entry"
                )));
            }
        }
        for id in self.overrides.keys() {
            if !overridable_ids.contains(&id.as_str()) {
                return Err(config_error(format!(
                    "[providers.{id}] does not name a provider that can be overridden"
                )));
            }
        }
        Ok(())
    }
}

pub(crate) fn config_error(message: String) -> crate::error::Error {
    crate::error::Error::Config(Box::new(figment::Error::from(message)))
}

/// Which wire protocol a custom provider speaks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CustomProviderKind {
    /// OpenAI-compatible `POST {base_url}/chat/completions`.
    #[default]
    OpenAi,
    /// Anthropic Messages `POST {base_url}/v1/messages`.
    Anthropic,
    /// Gemini `POST {base_url}/v1beta/models/<model>:streamGenerateContent`.
    Gemini,
}

/// One custom provider definition (`[providers.custom.<id>]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomProviderConfig {
    /// Which wire protocol the endpoint speaks. Defaults to `openai`.
    #[serde(default)]
    pub kind: CustomProviderKind,
    /// Endpoint base URL, without the protocol-specific path.
    pub base_url: String,
    /// Name shown in the model picker. Defaults to the provider id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Environment variable holding the API key. Defaults to
    /// `<ID>_API_KEY` uppercased; set it to `""` for endpoints that
    /// need no auth. As everywhere else, a set environment variable
    /// wins over a key stored via `kage auth login`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Extra HTTP headers sent on every request, e.g. `Authorization`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Models this provider serves. At least one is required.
    pub models: Vec<CustomProviderModel>,
    /// Whether the endpoint accepts tool definitions. Defaults to true.
    #[serde(default = "default_true")]
    pub tool_use: bool,
    /// Whether the endpoint preserves thinking blocks across turns.
    /// Defaults to false.
    #[serde(default)]
    pub thinking: bool,
    /// Whether the endpoint supports prompt caching. Defaults to false.
    #[serde(default)]
    pub caching: bool,
}

fn default_true() -> bool {
    true
}

/// One model a custom provider serves
/// (`[[providers.custom.<id>.models]]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomProviderModel {
    /// Model id sent to the endpoint; addressable from kage as
    /// `<provider-id>:<this id>`.
    pub id: String,
    /// Display name shown in the picker.
    pub name: String,
    /// Context window in tokens, when known. Surfaces to the modeline
    /// like a catalog entry's context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// Maximum output tokens per turn, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u32>,
}

/// Settings overriding a provider kage registers itself
/// (`[providers.<id>]`). Fields left out keep the built-in value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuiltinProviderOverride {
    /// Replace the provider's default base URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Replace the environment variable the API key is read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Extra HTTP headers sent on every request to this provider.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
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
    /// Minimum content rows the input card holds (before chrome).
    /// Clamped to a sane floor of 1.
    pub input_min_lines: u16,
    /// Maximum content rows the input card grows to before it stops
    /// growing and scrolls internally. Lets a user enlarge the
    /// composing area; clamped to a sane ceiling.
    pub input_max_lines: u16,
    /// Default thinking level for new sessions: one of the
    /// `ThinkingLevel` ladder strings (`off`, `minimal`, `low`,
    /// `medium`, `high`, `xhigh`). Parsed and seeded at session
    /// start; the Shift+Tab cycle still overrides it per session.
    /// Kept as a raw string here because the ladder itself lives in
    /// kage-provider, which sits above kage-core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "default".into(),
            mouse: true,
            editor: EditorMode::default(),
            input_min_lines: 1,
            input_max_lines: 8,
            thinking_level: None,
        }
    }
}

/// How the prompt input behaves.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditorMode {
    /// Modal editing in the vim style: a normal mode, an insert
    /// mode, and a visual mode, with motions, operators, and
    /// registers. Opt in with `editor = "vim"`.
    Vim,
    /// Always-editable, non-modal editing. Readline / Emacs keys do
    /// all editing (`Ctrl+A`/`E`/`W`/`U`/`K`/`Y`, `Ctrl+/`,
    /// `Alt+B`/`F`); `Esc` cancels the in-flight turn; the buffer
    /// scrolls with `PageUp` / `PageDown` and the mouse. No modal
    /// states. The default.
    #[default]
    Modeless,
}

/// Plugin loader configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Override the default plugin directory `~/.config/kage/plugins/`.
    /// `~` expands to the home directory; relative paths resolve
    /// against the kage config directory.
    pub dir: Option<PathBuf>,
    /// If non-empty, only plugins in this allowlist are loaded.
    pub enabled: Vec<String>,
    /// Per-plugin elevated capability grants, keyed by plugin file
    /// stem (`[plugins.capabilities] my-plugin = ["session_write"]`).
    /// Plugins get only the sandboxed base surface by default; a
    /// capability listed here is exposed solely to the named plugin,
    /// and only after that plugin requests it via
    /// `kage.request_capabilities`. Unknown capability names are a
    /// hard error at startup, not silently ignored.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub capabilities: BTreeMap<String, Vec<String>>,
    /// Per-plugin settings, keyed by plugin file stem
    /// (`[plugins.config.my-plugin] key = "value"`). The named plugin
    /// reads its own slice via `kage.plugin_config()`; a plugin sees
    /// only its own entry, never another's. Arbitrary nested tables are
    /// allowed.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, serde_json::Value>,
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
    /// Reserved for a future sandbox backend (post-0.1) and not yet
    /// enforced: no tool consults this list today. Plugin `kage.http`
    /// egress is governed by the `net` capability and SSRF filtering,
    /// not by this field.
    pub network_allowlist: Vec<String>,
}

/// Keybinding overrides expressed as a flat map of chord to command name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    /// Map of keybinding chord (for example `<leader>cf`) to command name.
    pub bindings: BTreeMap<String, String>,
    /// The key `<leader>` expands to, as one key in keymap notation.
    #[serde(skip_serializing_if = "is_default_leader")]
    pub leader: String,
    /// How long a mapping that is also a prefix waits for more keys,
    /// in milliseconds.
    #[serde(skip_serializing_if = "is_default_timeoutlen")]
    pub timeoutlen: u32,
}

const DEFAULT_LEADER: &str = "\\";
const DEFAULT_TIMEOUTLEN: u32 = 1000;

fn is_default_leader(leader: &str) -> bool {
    leader == DEFAULT_LEADER
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_timeoutlen(timeoutlen: &u32) -> bool {
    *timeoutlen == DEFAULT_TIMEOUTLEN
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            bindings: BTreeMap::new(),
            leader: DEFAULT_LEADER.to_owned(),
            timeoutlen: DEFAULT_TIMEOUTLEN,
        }
    }
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
    /// Allow MCP servers to drive `sampling/createMessage`: an LLM
    /// completion run against the host's default model and billed to the
    /// user. Off by default because it lets a server spend the user's
    /// token budget; opt in per project with `[mcp] allow_sampling =
    /// true`.
    #[serde(default)]
    pub allow_sampling: bool,
}

/// How to reach one external MCP server.
///
/// Two transports, selected by which field is set (exactly one of
/// `command` or `url` is required; setting both, or neither, is a
/// configuration error surfaced when the server is brought up):
///
/// * stdio: set `command` (and optionally `args` / `env`). kage spawns
///   the child and speaks JSON-RPC over its stdio.
/// * Streamable HTTP: set `url`. kage POSTs each JSON-RPC message to
///   that endpoint; `headers` are sent on every request (use it for
///   `Authorization`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpServer {
    /// Executable to spawn for a stdio transport (e.g. `npx`). Mutually
    /// exclusive with `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments passed to `command`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Base URL for a remote Streamable HTTP transport. Mutually
    /// exclusive with `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra headers sent on every request (HTTP transport only),
    /// e.g. `Authorization`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// When `true`, the server is configured but not spawned/connected.
    #[serde(default)]
    pub disabled: bool,
}

#[cfg(test)]
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;

    use crate::test_support::process_globals;

    #[test]
    fn default_provider_model_is_anthropic_sonnet() {
        let cfg = Config::default();
        assert_eq!(cfg.provider.default_model, "anthropic:claude-sonnet-4-6");
    }

    #[test]
    fn missing_file_yields_defaults() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            let cfg = Config::load(jail.directory().join("nope.toml").as_path()).unwrap();
            assert_eq!(cfg, Config::default());
            Ok(())
        });
    }

    #[test]
    fn acp_agents_default_empty_and_parse_from_table() {
        assert!(Config::default().acp.agents.is_empty());
        let _globals = process_globals();
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
    fn mcp_server_parses_stdio_and_http_transports() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [mcp.servers.local]
                command = "npx"
                args = ["-y", "server"]

                [mcp.servers.remote]
                url = "https://mcp.example.com/sse"
                [mcp.servers.remote.headers]
                Authorization = "Bearer x"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let local = cfg.mcp.servers.get("local").expect("stdio server");
            assert_eq!(local.command.as_deref(), Some("npx"));
            assert!(local.url.is_none());
            let remote = cfg.mcp.servers.get("remote").expect("http server");
            assert!(remote.command.is_none());
            assert_eq!(remote.url.as_deref(), Some("https://mcp.example.com/sse"));
            assert_eq!(
                remote.headers.get("Authorization").map(String::as_str),
                Some("Bearer x")
            );
            Ok(())
        });
    }

    #[test]
    fn plugins_config_parses_nested_tables_per_stem() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [plugins.config.my-plugin]
                api_url = "https://example.com"
                retries = 3

                [plugins.config.my-plugin.nested]
                flag = true
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let slice = cfg.plugins.config.get("my-plugin").expect("slice parsed");
            assert_eq!(slice["api_url"], serde_json::json!("https://example.com"));
            assert_eq!(slice["retries"], serde_json::json!(3));
            assert_eq!(slice["nested"]["flag"], serde_json::json!(true));
            Ok(())
        });
    }

    #[test]
    fn file_overrides_defaults() {
        let _globals = process_globals();
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
        let _globals = process_globals();
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
        let _globals = process_globals();
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
    fn editor_mode_defaults_modeless_and_parses_vim() {
        assert_eq!(Config::default().ui.editor, EditorMode::Modeless);
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [ui]
                editor = "vim"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.ui.editor, EditorMode::Vim);
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
        let _globals = process_globals();
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
        let _globals = process_globals();
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
        let _globals = process_globals();
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
        let _globals = process_globals();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let mut cfg = Config::default();
        cfg.ui.theme = "ayu".to_owned();
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.ui.theme, "ayu");
    }

    fn sample_custom_provider() -> CustomProviderConfig {
        CustomProviderConfig {
            kind: CustomProviderKind::OpenAi,
            base_url: "https://api.together.xyz/v1".into(),
            display_name: None,
            api_key_env: None,
            headers: BTreeMap::new(),
            models: vec![CustomProviderModel {
                id: "meta-llama/Llama-3.3-70B-Instruct-Turbo".into(),
                name: "Llama 3.3 70B".into(),
                context: Some(131_072),
                max_output: None,
            }],
            tool_use: true,
            thinking: false,
            caching: false,
        }
    }

    #[test]
    fn keybindings_leader_and_timeoutlen_skip_defaults() {
        let body = toml::to_string(&Config::default()).unwrap();
        assert!(!body.contains("leader"), "{body}");
        assert!(!body.contains("timeoutlen"), "{body}");

        let mut cfg = Config::default();
        cfg.keybindings.leader = ",".to_owned();
        cfg.keybindings.timeoutlen = 300;
        let body = toml::to_string(&cfg).unwrap();
        assert!(body.contains("leader = \",\""), "{body}");
        assert!(body.contains("timeoutlen = 300"), "{body}");
    }

    #[test]
    fn keybindings_leader_and_timeoutlen_parse() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [keybindings]
                leader = "<Space>"
                timeoutlen = 250
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.keybindings.leader, "<Space>");
            assert_eq!(cfg.keybindings.timeoutlen, 250);
            assert!(cfg.keybindings.bindings.is_empty());
            Ok(())
        });
    }

    #[test]
    fn providers_section_defaults_empty() {
        let cfg = Config::default();
        assert!(cfg.providers.is_default());
        assert!(cfg.providers.custom.is_empty());
        assert!(cfg.providers.overrides.is_empty());
        let body = toml::to_string(&cfg).unwrap();
        assert!(!body.contains("[providers"), "{body}");
    }

    #[test]
    fn permissions_section_defaults_empty_and_hidden() {
        let cfg = Config::default();
        assert!(cfg.permissions.is_default());
        let body = toml::to_string(&cfg).unwrap();
        assert!(!body.contains("[permissions"), "{body}");
    }

    #[test]
    fn permissions_table_parses_under_config() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [permissions]
                confine_paths = true

                [permissions.tools.bash]
                default = "ask"
                allow = ["git *"]
                deny = ["rm -rf *"]
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert!(cfg.permissions.confine_paths);
            assert_eq!(
                cfg.permissions.check("bash", "git status"),
                crate::permissions::PermissionAction::Allow
            );
            assert_eq!(
                cfg.permissions.check("bash", "rm -rf /"),
                crate::permissions::PermissionAction::Deny
            );
            Ok(())
        });
    }

    #[test]
    fn thinking_level_parses_and_roundtrips() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [ui]
                thinking_level = "medium"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            assert_eq!(cfg.ui.thinking_level.as_deref(), Some("medium"));
            let body = toml::to_string(&cfg).unwrap();
            assert!(body.contains("thinking_level = \"medium\""), "{body}");
            Ok(())
        });
    }

    #[test]
    fn providers_parses_custom_and_override_tables_side_by_side() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [providers.custom.together]
                base_url = "https://api.together.xyz/v1"

                [[providers.custom.together.models]]
                id = "meta-llama/Llama-3.3-70B-Instruct-Turbo"
                name = "Llama 3.3 70B"
                context = 131072

                [providers.deepseek]
                base_url = "https://relay.example.com/v1"
                [providers.deepseek.headers]
                X-Team = "infra"
                "#,
            )?;
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let together = cfg.providers.custom.get("together").expect("custom parsed");
            assert_eq!(
                together.kind,
                CustomProviderKind::OpenAi,
                "kind defaults to openai"
            );
            assert_eq!(together.base_url, "https://api.together.xyz/v1");
            assert!(together.tool_use, "tool_use defaults to true");
            assert!(!together.thinking);
            assert!(!together.caching);
            assert_eq!(together.models.len(), 1);
            assert_eq!(
                together.models[0].id,
                "meta-llama/Llama-3.3-70B-Instruct-Turbo"
            );
            assert_eq!(together.models[0].name, "Llama 3.3 70B");
            assert_eq!(together.models[0].context, Some(131_072));
            assert_eq!(together.models[0].max_output, None);
            let deepseek = cfg
                .providers
                .overrides
                .get("deepseek")
                .expect("override parsed via flatten");
            assert_eq!(
                deepseek.base_url.as_deref(),
                Some("https://relay.example.com/v1")
            );
            assert_eq!(deepseek.api_key_env, None);
            assert_eq!(
                deepseek.headers.get("X-Team").map(String::as_str),
                Some("infra")
            );
            assert!(!cfg.providers.custom.contains_key("deepseek"));
            Ok(())
        });
    }

    #[test]
    fn providers_custom_requires_base_url_and_models() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "no-base-url.toml",
                r#"
                [providers.custom.broken]
                display_name = "No base URL"
                "#,
            )?;
            let err = Config::load(jail.directory().join("no-base-url.toml").as_path())
                .expect_err("missing base_url must fail the load");
            assert!(err.to_string().contains("base_url"), "{err}");

            jail.create_file(
                "no-models.toml",
                r#"
                [providers.custom.broken]
                base_url = "https://api.example.com/v1"
                "#,
            )?;
            let err = Config::load(jail.directory().join("no-models.toml").as_path())
                .expect_err("missing models must fail the load");
            assert!(err.to_string().contains("models"), "{err}");
            Ok(())
        });
    }

    #[test]
    fn providers_env_overrides_file_values() {
        let _globals = process_globals();
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [providers.deepseek]
                base_url = "https://api.deepseek.com/v1"
                "#,
            )?;
            jail.set_env(
                "KAGE_PROVIDERS__DEEPSEEK__BASE_URL",
                "https://relay.example.com/v1",
            );
            let cfg = Config::load(jail.directory().join("config.toml").as_path()).unwrap();
            let deepseek = cfg
                .providers
                .overrides
                .get("deepseek")
                .expect("override parsed");
            assert_eq!(
                deepseek.base_url.as_deref(),
                Some("https://relay.example.com/v1")
            );
            Ok(())
        });
    }

    #[test]
    fn providers_validate_accepts_well_formed_config() {
        let providers = ProvidersConfig {
            custom: BTreeMap::from([("together".to_owned(), sample_custom_provider())]),
            overrides: BTreeMap::from([(
                "deepseek".to_owned(),
                BuiltinProviderOverride {
                    base_url: Some("https://relay.example.com/v1".into()),
                    api_key_env: None,
                    headers: BTreeMap::from([("X-Team".to_owned(), "infra".to_owned())]),
                },
            )]),
        };
        providers
            .validate(
                &["acp", "anthropic", "openai", "openai-responses", "gemini"],
                &[
                    "anthropic",
                    "openai",
                    "openai-responses",
                    "gemini",
                    "deepseek",
                ],
            )
            .expect("well-formed providers validate");
    }

    #[test]
    fn providers_validate_rejects_invalid_custom_id() {
        for id in ["", "DeepSeek", "has_underscore", "has space", "has.dot"] {
            let providers = ProvidersConfig {
                custom: BTreeMap::from([(id.to_owned(), sample_custom_provider())]),
                overrides: BTreeMap::new(),
            };
            let err = providers
                .validate(&["anthropic"], &["anthropic"])
                .expect_err("invalid custom id must be rejected");
            assert!(err.to_string().contains("id must be"), "{err}");
        }
    }

    #[test]
    fn providers_validate_rejects_custom_id_shadowing_registered() {
        let providers = ProvidersConfig {
            custom: BTreeMap::from([("openai".to_owned(), sample_custom_provider())]),
            overrides: BTreeMap::new(),
        };
        let err = providers
            .validate(&["openai", "acp"], &["openai"])
            .expect_err("custom id must not shadow a registered provider");
        assert!(err.to_string().contains("conflicts"), "{err}");
    }

    #[test]
    fn providers_validate_requires_models() {
        let mut provider = sample_custom_provider();
        provider.models.clear();
        let providers = ProvidersConfig {
            custom: BTreeMap::from([("together".to_owned(), provider)]),
            overrides: BTreeMap::new(),
        };
        let err = providers
            .validate(&[], &[])
            .expect_err("custom provider without models must be rejected");
        assert!(err.to_string().contains("models"), "{err}");
    }

    #[test]
    fn providers_validate_rejects_unknown_override_id() {
        let providers = ProvidersConfig {
            custom: BTreeMap::new(),
            overrides: BTreeMap::from([(
                "not-a-provider".to_owned(),
                BuiltinProviderOverride::default(),
            )]),
        };
        let err = providers
            .validate(&["openai"], &["openai"])
            .expect_err("unknown override id must be rejected");
        assert!(err.to_string().contains("not-a-provider"), "{err}");

        let providers = ProvidersConfig {
            custom: BTreeMap::new(),
            overrides: BTreeMap::from([("acp".to_owned(), BuiltinProviderOverride::default())]),
        };
        let err = providers
            .validate(&["acp"], &["openai"])
            .expect_err("acp must not be overridable");
        assert!(err.to_string().contains("acp"), "{err}");
    }

    #[test]
    fn providers_save_then_load_roundtrips() {
        let _globals = process_globals();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.providers
            .custom
            .insert("together".to_owned(), sample_custom_provider());
        cfg.providers.overrides.insert(
            "deepseek".to_owned(),
            BuiltinProviderOverride {
                base_url: Some("https://relay.example.com/v1".into()),
                api_key_env: Some("DEEPSEEK_API_KEY".into()),
                headers: BTreeMap::from([("X-Team".to_owned(), "infra".to_owned())]),
            },
        );
        cfg.save(&path).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("[providers.custom.together]"), "{body}");
        assert!(
            body.contains("[[providers.custom.together.models]]"),
            "{body}"
        );
        assert!(body.contains("[providers.deepseek]"), "{body}");
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, cfg);
    }
}
