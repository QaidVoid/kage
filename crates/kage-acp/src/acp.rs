//! Agent Client Protocol wire types (spec-conformant).
//!
//! Field names are `camelCase` and update/content tags are
//! `snake_case`, matching the published ACP schema. Internally-tagged
//! enums wrap a per-variant `camelCase` struct so the discriminant
//! (`sessionUpdate`, `type`, `outcome`) sits beside the struct's
//! fields exactly as the spec shows.
//!
//! Only the surface kage needs is modelled; unknown optional fields
//! are tolerated on the way in and omitted on the way out.

use serde::{Deserialize, Serialize};

/// ACP protocol version kage implements.
pub const PROTOCOL_VERSION: i64 = 1;

/// Name/version pair exchanged in `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    /// Program name.
    pub name: String,
    /// Optional display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Program version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Client-side filesystem capability flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapability {
    /// Client can serve `fs/read_text_file`.
    #[serde(default)]
    pub read_text_file: bool,
    /// Client can serve `fs/write_text_file`.
    #[serde(default)]
    pub write_text_file: bool,
}

/// What the client can do for the agent.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    /// Filesystem capabilities.
    #[serde(default)]
    pub fs: FsCapability,
    /// Client can serve `terminal/*`.
    #[serde(default)]
    pub terminal: bool,
}

/// Prompt content kinds the agent accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    /// Accepts image content blocks.
    #[serde(default)]
    pub image: bool,
    /// Accepts audio content blocks.
    #[serde(default)]
    pub audio: bool,
    /// Accepts embedded `resource` context.
    #[serde(default)]
    pub embedded_context: bool,
}

/// What the agent can do.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    /// Agent implements `session/load`.
    #[serde(default)]
    pub load_session: bool,
    /// Prompt content the agent accepts.
    #[serde(default)]
    pub prompt_capabilities: PromptCapabilities,
}

/// `initialize` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    /// Highest protocol version the client speaks.
    pub protocol_version: i64,
    /// Client capabilities.
    #[serde(default)]
    pub client_capabilities: ClientCapabilities,
    /// Optional client identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<Implementation>,
}

/// `initialize` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    /// Negotiated protocol version.
    pub protocol_version: i64,
    /// Agent capabilities.
    pub agent_capabilities: AgentCapabilities,
    /// Optional agent identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<Implementation>,
    /// Supported auth methods (empty: no auth).
    #[serde(default)]
    pub auth_methods: Vec<serde_json::Value>,
}

/// `session/new` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    /// Working directory for the session (absolute path).
    pub cwd: String,
    /// MCP servers the client wants wired in (opaque to kage v1).
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,
}

/// `session/new` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    /// Opaque session id the client uses on later calls.
    pub session_id: String,
}

/// `session/load` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    /// Session to replay.
    pub session_id: String,
    /// Working directory.
    pub cwd: String,
    /// MCP servers (opaque to kage v1).
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,
}

/// `session/prompt` request params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    /// Target session.
    pub session_id: String,
    /// The user's turn as content blocks.
    pub prompt: Vec<ContentBlock>,
}

/// Why a prompt turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn.
    EndTurn,
    /// Hit the output token cap.
    MaxTokens,
    /// Hit the max-requests-per-turn cap.
    MaxTurnRequests,
    /// The model refused.
    Refusal,
    /// The turn was cancelled.
    Cancelled,
}

/// `session/prompt` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    /// Why the turn ended.
    pub stop_reason: StopReason,
}

/// `session/cancel` notification params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelNotification {
    /// Session to cancel.
    pub session_id: String,
}

/// One content block (`type`-tagged, ACP/MCP aligned).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text(TextContent),
    /// Inline image bytes.
    Image(BlobContent),
    /// Inline audio bytes.
    Audio(BlobContent),
    /// A pointer to a resource.
    ResourceLink(ResourceLink),
    /// An embedded resource.
    Resource(EmbeddedResource),
}

impl ContentBlock {
    /// Shorthand for a text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent { text: text.into() })
    }

    /// The concatenated text this block contributes, if any.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(&t.text),
            _ => None,
        }
    }
}

/// `text` content payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextContent {
    /// The text.
    pub text: String,
}

/// `image` / `audio` content payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobContent {
    /// Base64 bytes.
    pub data: String,
    /// MIME type.
    pub mime_type: String,
    /// Optional source uri.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

/// `resource_link` content payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLink {
    /// Resource uri.
    pub uri: String,
    /// Resource name.
    pub name: String,
    /// Optional MIME type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// `resource` embedded payload (kept opaque; we only forward it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddedResource {
    /// The embedded resource object (uri + text|blob + mimeType).
    pub resource: serde_json::Value,
}

/// A `session/update` notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    /// Session this update belongs to.
    pub session_id: String,
    /// The update payload.
    pub update: SessionUpdate,
}

/// The `sessionUpdate`-tagged update union.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
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
}

/// A message/thought chunk payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageChunk {
    /// The chunk content.
    pub content: ContentBlock,
}

/// Tool kind hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads data.
    Read,
    /// Edits a file.
    Edit,
    /// Deletes something.
    Delete,
    /// Moves something.
    Move,
    /// Searches.
    Search,
    /// Executes a command.
    Execute,
    /// Reasoning step.
    Think,
    /// Fetches remote data.
    Fetch,
    /// Anything else.
    #[default]
    Other,
}

/// Tool-call lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Not started.
    Pending,
    /// Running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Failed.
    Failed,
}

/// A `tool_call` update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// Correlation id.
    pub tool_call_id: String,
    /// Human title (usually the tool name).
    pub title: String,
    /// Tool kind hint.
    #[serde(default)]
    pub kind: ToolKind,
    /// Lifecycle status.
    pub status: ToolCallStatus,
    /// Rich content (output, diffs, terminals).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolCallContent>,
    /// Raw tool input echoed for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<serde_json::Value>,
}

/// A `tool_call_update` (partial; only changed fields set).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    /// Correlation id.
    pub tool_call_id: String,
    /// New status, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    /// Replacement content, if any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolCallContent>,
    /// Raw tool output, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<serde_json::Value>,
}

/// Rich tool-call content (`type`-tagged).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    /// A content block (typically the tool's text output).
    Content(MessageChunk),
    /// A unified diff.
    Diff(DiffContent),
    /// A reference to a created terminal.
    Terminal(TerminalRef),
}

/// `diff` tool-call content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffContent {
    /// File path.
    pub path: String,
    /// Prior text, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    /// New text.
    pub new_text: String,
}

/// `terminal` tool-call content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalRef {
    /// Terminal id.
    pub terminal_id: String,
}

/// A `plan` update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Plan entries.
    pub entries: Vec<serde_json::Value>,
}

/// An `available_commands_update`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommandsUpdate {
    /// The current slash-command list.
    pub available_commands: Vec<serde_json::Value>,
}

/// A `current_mode_update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentModeUpdate {
    /// The newly active mode id.
    pub current_mode_id: String,
}

/// How a permission option resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    /// Allow this one call.
    AllowOnce,
    /// Allow and remember.
    AllowAlways,
    /// Reject this one call.
    RejectOnce,
    /// Reject and remember.
    RejectAlways,
}

/// One choice offered in a permission prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    /// Stable id echoed back in the response.
    pub option_id: String,
    /// Human label.
    pub name: String,
    /// What picking it means.
    pub kind: PermissionOptionKind,
}

/// `session/request_permission` request params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    /// Session the call belongs to.
    pub session_id: String,
    /// The tool call awaiting a verdict.
    pub tool_call: ToolCallUpdate,
    /// The choices offered.
    pub options: Vec<PermissionOption>,
}

/// The client's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    /// The turn was cancelled before the user chose.
    Cancelled,
    /// The user picked an option.
    Selected(SelectedOption),
}

/// The chosen option id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedOption {
    /// Which [`PermissionOption::option_id`] was picked.
    pub option_id: String,
}

/// `session/request_permission` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestPermissionResponse {
    /// The verdict.
    pub outcome: PermissionOutcome,
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
    fn initialize_shapes_match_spec() {
        roundtrip(
            &InitializeRequest {
                protocol_version: 1,
                client_capabilities: ClientCapabilities {
                    fs: FsCapability {
                        read_text_file: true,
                        write_text_file: true,
                    },
                    terminal: true,
                },
                client_info: None,
            },
            serde_json::json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": true, "writeTextFile": true},
                    "terminal": true
                }
            }),
        );
        roundtrip(
            &InitializeResponse {
                protocol_version: 1,
                agent_capabilities: AgentCapabilities {
                    load_session: true,
                    prompt_capabilities: PromptCapabilities {
                        image: false,
                        audio: false,
                        embedded_context: true,
                    },
                },
                agent_info: Some(Implementation {
                    name: "kage".into(),
                    title: None,
                    version: Some("0.1.0".into()),
                }),
                auth_methods: vec![],
            },
            serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": {
                        "image": false, "audio": false, "embeddedContext": true
                    }
                },
                "agentInfo": {"name": "kage", "version": "0.1.0"},
                "authMethods": []
            }),
        );
    }

    #[test]
    fn prompt_and_stop_reason_shapes() {
        roundtrip(
            &PromptRequest {
                session_id: "s1".into(),
                prompt: vec![ContentBlock::text("hello")],
            },
            serde_json::json!({
                "sessionId": "s1",
                "prompt": [{"type": "text", "text": "hello"}]
            }),
        );
        roundtrip(
            &PromptResponse {
                stop_reason: StopReason::EndTurn,
            },
            serde_json::json!({"stopReason": "end_turn"}),
        );
    }

    #[test]
    fn session_update_variants_shapes() {
        roundtrip(
            &SessionNotification {
                session_id: "s1".into(),
                update: SessionUpdate::AgentMessageChunk(MessageChunk {
                    content: ContentBlock::text("hi"),
                }),
            },
            serde_json::json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "hi"}
                }
            }),
        );
        roundtrip(
            &SessionUpdate::ToolCall(ToolCall {
                tool_call_id: "t1".into(),
                title: "bash".into(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Pending,
                content: vec![],
                raw_input: Some(serde_json::json!({"cmd": "ls"})),
            }),
            serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "t1",
                "title": "bash",
                "kind": "execute",
                "status": "pending",
                "rawInput": {"cmd": "ls"}
            }),
        );
    }

    #[test]
    fn request_permission_outcome_shapes() {
        roundtrip(
            &RequestPermissionResponse {
                outcome: PermissionOutcome::Selected(SelectedOption {
                    option_id: "allow".into(),
                }),
            },
            serde_json::json!({"outcome": {"outcome": "selected", "optionId": "allow"}}),
        );
        roundtrip(
            &RequestPermissionResponse {
                outcome: PermissionOutcome::Cancelled,
            },
            serde_json::json!({"outcome": {"outcome": "cancelled"}}),
        );
        roundtrip(
            &PermissionOption {
                option_id: "a".into(),
                name: "Allow".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            serde_json::json!({"optionId": "a", "name": "Allow", "kind": "allow_once"}),
        );
    }
}
