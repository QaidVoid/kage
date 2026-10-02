//! The ACP wire schema kage speaks, re-exported from
//! [`kage_acp_wire`].
//!
//! [`kage_acp_wire`] owns the serde-only schema so a wasm client can
//! share the exact types `kage rpc` speaks. This module re-exports it
//! under the paths the kage crates already use, and keeps the three
//! types whose shape is kage's own engine state: [`ConfigGetResult`]
//! (the live config sections), [`McpStatusUpdate`] (the engine's
//! [`McpServerStatus`]) and the [`SessionUpdate`] form that carries
//! them. Every update converts losslessly into the wire union at the
//! send boundary ([`crate::agent::send_update`]); the serde shape is
//! identical either way.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use kage_acp_wire as wire;
use kage_core::config::{AcpConfig, McpConfig, PluginsConfig, ProvidersConfig, UiConfig};
use kage_core::permissions::PermissionsConfig;
use kage_core::protocol::McpServerStatus;

pub use kage_acp_wire::schema::{
    AcpProbe, AgentCapabilities, AgentMeta, AuthSetRequest, AvailableCommandsUpdate, BlobContent,
    CancelNotification, ChunkMeta, ClientCapabilities, CloseSessionRequest, CloseSessionResponse,
    CompactionUpdate, ConfigGetRequest, ConfigOptionUpdate, ConfigSetRequest, ConfigTestRequest,
    ConfigTestResult, ContentBlock, Cost, CurrentModeUpdate, DiffContent, DirectoryCost,
    DirectoryModel, DirectoryProvider, DirectoryRequest, DirectoryResult, EmbeddedResource,
    EnvVariable, FsCapability, FsEntry, FsKind, FsListResult, FsOp, FsReadResult, FsRequest,
    FsResult, HttpHeader, Implementation, InitializeRequest, InitializeResponse, KageAgentInfo,
    KageMeta, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    McpCapabilities, McpProbe, McpServer, McpServerHttp, McpServerStdio, MessageChunk, ModelEntry,
    ModelProvider, ModelsResponse, NewSessionRequest, NewSessionResponse, NoticeTone, NoticeUpdate,
    OptionEntry, OptionSetRequest, OptionsResponse, PROTOCOL_VERSION, PermissionOption,
    PermissionOptionKind, PermissionOutcome, Plan, PlanReview, PluginInstallRequest,
    PluginRemoveRequest, ProbeModel, ProbeTool, PromptCapabilities, PromptDelivery, PromptRef,
    PromptRequest, PromptResponse, ProviderProbe, QuestionChoice, QuestionMeta, QuestionPrompt,
    RequestMeta, RequestPermissionRequest, RequestPermissionResponse, RequestPermissionResult,
    ResourceLink, ResumeSessionRequest, ResumeSessionResponse, SelectedOption, SessionCapabilities,
    SessionConfigCategory, SessionConfigKind, SessionConfigOption, SessionConfigSelectOption,
    SessionExportResponse, SessionForkRequest, SessionForkResponse, SessionInfo, SessionInfoKage,
    SessionInfoMeta, SessionInfoUpdate, SessionNotification, SessionRenameRequest, SessionRequest,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
    SubagentSessionCapabilities, SubagentState, SubagentSwarm, SubagentUpdate, SubagentUsage,
    Supported, SwarmMeta, SwarmResumeRequest, SwarmResumeResponse, TerminalRef, TextContent,
    ToolCall, ToolCallContent, ToolCallMeta, ToolCallStatus, ToolCallUpdate, ToolKind, TurnPhase,
    TurnReason, TurnUpdate, UsageUpdate,
};

/// `_kage/config/get` result: the read-only sections a settings page
/// renders. Serving them never writes `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigGetResult {
    /// Custom providers and overrides of registered providers.
    pub providers: ProvidersConfig,
    /// External MCP tool servers.
    pub mcp: McpConfig,
    /// Tool permission rules.
    pub permissions: PermissionsConfig,
    /// Plugin loader settings.
    pub plugins: PluginsConfig,
    /// User interface settings.
    pub ui: UiConfig,
    /// External ACP agents usable as `acp:<name>`.
    #[serde(default)]
    pub acp: AcpConfig,
    /// The plugin files in the plugin directory, by name.
    #[serde(default)]
    pub installed_plugins: Vec<InstalledPlugin>,
    /// Where each provider kage registers or the config defines finds
    /// its key, by provider id.
    #[serde(default)]
    pub provider_keys: BTreeMap<String, ProviderKey>,
}

/// Where one provider finds its API key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderKey {
    /// The environment variable it reads; empty when it needs no key.
    pub env: String,
    /// Where the key is now.
    pub source: KeySource,
}

/// Where a provider's key is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// Set in the environment variable.
    Env,
    /// Saved in the credential store.
    Auth,
    /// Nowhere: the provider is not usable until one is set.
    Missing,
    /// The provider needs no key.
    Unneeded,
}

/// One plugin file in the plugin directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPlugin {
    /// The file stem, which `config.toml` names the plugin by.
    pub name: String,
    /// Whether the `[plugins] enabled` allowlist lets it load.
    pub enabled: bool,
}

/// One MCP server's reachability, as the kage engine reports it,
/// carried by the `_kage/mcp_status` update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatusUpdate {
    /// Configured server name.
    pub name: String,
    /// Whether the server is usable.
    #[serde(flatten)]
    pub status: McpServerStatus,
}

/// The `sessionUpdate`-tagged update union, in the form kage's engine
/// produces. It mirrors the wire union
/// ([`kage_acp_wire::SessionUpdate`]) variant for variant; only
/// [`SessionUpdate::McpStatus`] differs, carrying the engine's own
/// [`McpServerStatus`] instead of the wire copy. The serde form is the
/// wire union, which every update converts into losslessly.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionUpdate {
    /// Echo of a user message chunk.
    UserMessageChunk(MessageChunk),
    /// Assistant text chunk.
    AgentMessageChunk(MessageChunk),
    /// Assistant reasoning chunk.
    AgentThoughtChunk(MessageChunk),
    /// A tool call started.
    ToolCall(ToolCall),
    /// A tool call's status/output changed.
    ToolCallUpdate(ToolCallUpdate),
    /// The agent's plan.
    Plan(Plan),
    /// Slash-command list changed.
    AvailableCommandsUpdate(AvailableCommandsUpdate),
    /// The active session mode changed.
    CurrentModeUpdate(CurrentModeUpdate),
    /// Context window usage changed.
    UsageUpdate(UsageUpdate),
    /// Session metadata (title, activity time) changed.
    SessionInfoUpdate(SessionInfoUpdate),
    /// The session's config options changed.
    ConfigOptionUpdate(ConfigOptionUpdate),
    /// A subagent was announced or changed (draft RFD PR #1992).
    SubagentUpdate(SubagentUpdate),
    /// A turn of the running prompt began or ended.
    Turn(TurnUpdate),
    /// A message for the user outside the conversation.
    Notice(NoticeUpdate),
    /// Older context turns were summarized.
    Compaction(CompactionUpdate),
    /// An MCP server's reachability changed.
    McpStatus(McpStatusUpdate),
    /// Any update kind kage does not know. Never sent.
    Unknown,
}

impl From<SessionUpdate> for wire::SessionUpdate {
    fn from(update: SessionUpdate) -> Self {
        match update {
            SessionUpdate::UserMessageChunk(update) => Self::UserMessageChunk(update),
            SessionUpdate::AgentMessageChunk(update) => Self::AgentMessageChunk(update),
            SessionUpdate::AgentThoughtChunk(update) => Self::AgentThoughtChunk(update),
            SessionUpdate::ToolCall(update) => Self::ToolCall(update),
            SessionUpdate::ToolCallUpdate(update) => Self::ToolCallUpdate(update),
            SessionUpdate::Plan(update) => Self::Plan(update),
            SessionUpdate::AvailableCommandsUpdate(update) => Self::AvailableCommandsUpdate(update),
            SessionUpdate::CurrentModeUpdate(update) => Self::CurrentModeUpdate(update),
            SessionUpdate::UsageUpdate(update) => Self::UsageUpdate(update),
            SessionUpdate::SessionInfoUpdate(update) => Self::SessionInfoUpdate(update),
            SessionUpdate::ConfigOptionUpdate(update) => Self::ConfigOptionUpdate(update),
            SessionUpdate::SubagentUpdate(update) => Self::SubagentUpdate(update),
            SessionUpdate::Turn(update) => Self::Turn(update),
            SessionUpdate::Notice(update) => Self::Notice(update),
            SessionUpdate::Compaction(update) => Self::Compaction(update),
            SessionUpdate::McpStatus(update) => Self::McpStatus(update.into()),
            SessionUpdate::Unknown => Self::Unknown,
        }
    }
}

impl From<McpStatusUpdate> for wire::McpStatusUpdate {
    fn from(update: McpStatusUpdate) -> Self {
        let status = match update.status {
            McpServerStatus::Connected => wire::McpServerStatus::Connected,
            McpServerStatus::Starting => wire::McpServerStatus::Starting,
            McpServerStatus::Failed { error } => wire::McpServerStatus::Failed { error },
            McpServerStatus::NeedsAuth => wire::McpServerStatus::NeedsAuth,
        };
        Self {
            name: update.name,
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: &T, json: serde_json::Value)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let encoded = serde_json::to_value(value).unwrap();
        assert_eq!(encoded, json, "serialized shape must match the spec");
        let decoded: T = serde_json::from_value(json).unwrap();
        assert_eq!(&decoded, value, "round-trip must be lossless");
    }

    #[test]
    fn config_get_shapes() {
        roundtrip(&ConfigGetRequest::default(), serde_json::json!({}));
        let value = serde_json::to_value(ConfigGetResult::default()).unwrap();
        for section in ["providers", "mcp", "permissions", "plugins", "ui"] {
            assert!(value.get(section).is_some(), "{section} must be present");
        }
        let back: ConfigGetResult = serde_json::from_value(value).unwrap();
        assert_eq!(back, ConfigGetResult::default());
    }

    /// The engine's `_kage/mcp_status` updates reach the wire union
    /// with the serde shape unchanged.
    #[test]
    fn mcp_status_updates_convert_to_the_wire_shape() {
        let cases = [
            (
                "fs",
                McpServerStatus::Connected,
                serde_json::json!({"sessionUpdate": "_kage/mcp_status", "name": "fs", "status": "connected"}),
            ),
            (
                "db",
                McpServerStatus::Failed {
                    error: "spawn failed".to_owned(),
                },
                serde_json::json!({"sessionUpdate": "_kage/mcp_status", "name": "db", "status": "failed", "error": "spawn failed"}),
            ),
            (
                "api",
                McpServerStatus::NeedsAuth,
                serde_json::json!({"sessionUpdate": "_kage/mcp_status", "name": "api", "status": "needs_auth"}),
            ),
        ];
        for (name, status, json) in cases {
            let update = SessionUpdate::McpStatus(McpStatusUpdate {
                name: name.to_owned(),
                status,
            });
            let wire_update: wire::SessionUpdate = update.into();
            assert_eq!(serde_json::to_value(wire_update).unwrap(), json);
        }
    }
}
