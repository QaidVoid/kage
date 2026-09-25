//! Agent Client Protocol wire types (spec-conformant).
//!
//! Field names are `camelCase` and update/content tags are
//! `snake_case`, matching the published ACP schema. Internally-tagged
//! enums wrap a per-variant `camelCase` struct so the discriminant
//! (`sessionUpdate`, `type`, `outcome`) sits beside the struct's
//! fields exactly as the spec shows.
//!
//! Only the surface kage needs is modelled; unknown optional fields
//! are tolerated on the way in and omitted on the way out, and an
//! unknown `sessionUpdate` kind parses as [`SessionUpdate::Unknown`].

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
    /// The draft subagents capability (RFD PR #1992), kept raw because
    /// its shape may still change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<serde_json::Value>,
}

impl ClientCapabilities {
    /// Whether the client understands `subagent_update` and child
    /// sessions: any advertised value other than `false`.
    #[must_use]
    pub fn supports_subagents(&self) -> bool {
        self.subagents
            .as_ref()
            .is_some_and(|v| *v != serde_json::Value::Bool(false))
    }
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
    /// MCP transports the agent can connect to from `mcpServers`.
    #[serde(default)]
    pub mcp_capabilities: McpCapabilities,
    /// Optional session methods the agent implements.
    #[serde(default)]
    pub session_capabilities: SessionCapabilities,
}

/// MCP transports the agent accepts in `mcpServers` (stdio is always
/// supported).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct McpCapabilities {
    /// Accepts `http` servers.
    #[serde(default)]
    pub http: bool,
    /// Accepts `sse` servers.
    #[serde(default)]
    pub sse: bool,
}

/// Optional session methods. Each is an empty object when supported
/// and omitted otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionCapabilities {
    /// Agent implements `session/list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<Supported>,
    /// Agent implements `session/resume`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Supported>,
}

/// An empty capability object whose presence means supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Supported {}

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
    /// MCP servers the client wants the session connected to.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
}

/// `session/new` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    /// Opaque session id the client uses on later calls.
    pub session_id: String,
    /// The session's config options, if the agent offers any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<SessionConfigOption>,
}

/// `session/load` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    /// Session to replay.
    pub session_id: String,
    /// Working directory.
    pub cwd: String,
    /// MCP servers the client wants the session connected to.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
}

/// `session/load` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionResponse {
    /// The session's config options, if the agent offers any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<SessionConfigOption>,
}

/// `session/resume` request params: reopen a session without replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionRequest {
    /// Session to reopen.
    pub session_id: String,
    /// Working directory.
    pub cwd: String,
    /// MCP servers the client wants the session connected to.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
}

/// `session/resume` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionResponse {
    /// The session's config options, if the agent offers any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<SessionConfigOption>,
}

/// `session/list` request params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsRequest {
    /// Only sessions recorded in this working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Opaque cursor from a previous page's `nextCursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// `session/list` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResponse {
    /// One page of sessions.
    pub sessions: Vec<SessionInfo>,
    /// Cursor for the next page, absent on the last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// One entry of a `session/list` page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Session id, usable with `session/load` and `session/resume`.
    pub session_id: String,
    /// Working directory the session was recorded in.
    pub cwd: String,
    /// Display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// ISO 8601 time of the last activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// `session/set_config_option` request params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionRequest {
    /// Target session.
    pub session_id: String,
    /// The [`SessionConfigOption::id`] to change.
    pub config_id: String,
    /// The chosen [`SessionConfigSelectOption::value`].
    pub value: String,
}

/// `session/set_config_option` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionResponse {
    /// Every config option of the session, after the change.
    pub config_options: Vec<SessionConfigOption>,
}

/// A session setting the client can show and change (a select).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOption {
    /// Stable id sent back in `session/set_config_option`.
    pub id: String,
    /// Human label.
    pub name: String,
    /// Optional longer explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// What the option controls, so clients can place it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<SessionConfigCategory>,
    /// The option's control type.
    #[serde(rename = "type")]
    pub kind: SessionConfigKind,
    /// The selected [`SessionConfigSelectOption::value`].
    pub current_value: String,
    /// The values to choose from.
    pub options: Vec<SessionConfigSelectOption>,
}

/// The control type of a [`SessionConfigOption`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionConfigKind {
    /// Pick one of `options`.
    Select,
}

/// What a [`SessionConfigOption`] controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionConfigCategory {
    /// The permission mode.
    Mode,
    /// The model.
    Model,
    /// The thinking level.
    ThoughtLevel,
}

/// One value of a select [`SessionConfigOption`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelectOption {
    /// Value sent back in `session/set_config_option`.
    pub value: String,
    /// Human label.
    pub name: String,
    /// Optional longer explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// An MCP server the client asks the agent to connect to. Stdio
/// entries carry no `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpServer {
    /// Streamable HTTP.
    Http(McpServerHttp),
    /// Legacy HTTP+SSE, which kage cannot connect to.
    Sse(McpServerHttp),
    /// A local process over stdio.
    #[serde(untagged)]
    Stdio(McpServerStdio),
}

/// A stdio [`McpServer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStdio {
    /// Server name, unique within the session.
    pub name: String,
    /// Program to run.
    pub command: String,
    /// Program arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: Vec<EnvVariable>,
}

/// An `http` or `sse` [`McpServer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerHttp {
    /// Server name, unique within the session.
    pub name: String,
    /// Endpoint URL.
    pub url: String,
    /// Headers sent with every request.
    #[serde(default)]
    pub headers: Vec<HttpHeader>,
}

/// One environment variable of a stdio [`McpServer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVariable {
    /// Variable name.
    pub name: String,
    /// Variable value.
    pub value: String,
}

/// One header of a remote [`McpServer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpHeader {
    /// Header name.
    pub name: String,
    /// Header value.
    pub value: String,
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
    /// Context window usage changed.
    UsageUpdate(UsageUpdate),
    /// Session metadata (title, activity time) changed.
    SessionInfoUpdate(SessionInfoUpdate),
    /// The session's config options changed.
    ConfigOptionUpdate(ConfigOptionUpdate),
    /// A subagent was announced or changed (draft RFD PR #1992).
    SubagentUpdate(SubagentUpdate),
    /// Any update kind kage does not know. Never sent.
    #[serde(other)]
    Unknown,
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    /// Correlation id.
    pub tool_call_id: String,
    /// New title, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// New kind, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    /// New status, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    /// Replacement content, if any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolCallContent>,
    /// Raw tool input, if changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<serde_json::Value>,
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

/// A `usage_update`: context tokens in use and the window size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageUpdate {
    /// Tokens currently in context.
    pub used: u64,
    /// Context window size in tokens.
    pub size: u64,
    /// Cumulative session cost, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
}

/// A monetary amount.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    /// The amount.
    pub amount: f64,
    /// ISO 4217 currency code.
    pub currency: String,
}

/// A `session_info_update`. Omitted fields are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoUpdate {
    /// New display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// ISO 8601 time of the last activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// A `config_option_update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionUpdate {
    /// Every config option of the session, as it is now.
    pub config_options: Vec<SessionConfigOption>,
}

/// A `subagent_update`, sent on the parent session. The first one for
/// an id announces the child. Omitted fields are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentUpdate {
    /// The child's session id, used by its own `session/update`s.
    pub subagent_session_id: String,
    /// Short label for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Summary of the work delegated to the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Operations the client may perform on the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<SubagentSessionCapabilities>,
    /// Lifecycle state. A child never given one is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<SubagentState>,
}

/// Client operations permitted on one subagent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SubagentSessionCapabilities {
    /// The client may `session/cancel` the child.
    #[serde(default)]
    pub cancel: bool,
}

/// Lifecycle state of a subagent. Every state but `running` is final.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    /// Working on its task.
    Running,
    /// Finished its task.
    Completed,
    /// Could not finish its task.
    Failed,
    /// Was cancelled.
    Cancelled,
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
                    subagents: Some(serde_json::json!({})),
                },
                client_info: None,
            },
            serde_json::json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": true, "writeTextFile": true},
                    "terminal": true,
                    "subagents": {}
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
                    mcp_capabilities: McpCapabilities {
                        http: true,
                        sse: false,
                    },
                    session_capabilities: SessionCapabilities {
                        list: Some(Supported {}),
                        resume: Some(Supported {}),
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
                    },
                    "mcpCapabilities": {"http": true, "sse": false},
                    "sessionCapabilities": {"list": {}, "resume": {}}
                },
                "agentInfo": {"name": "kage", "version": "0.1.0"},
                "authMethods": []
            }),
        );
    }

    #[test]
    fn subagents_capability_is_any_value_but_false() {
        let caps = |json| serde_json::from_value::<ClientCapabilities>(json).unwrap();
        assert!(caps(serde_json::json!({"subagents": {}})).supports_subagents());
        assert!(caps(serde_json::json!({"subagents": true})).supports_subagents());
        assert!(!caps(serde_json::json!({"subagents": false})).supports_subagents());
        assert!(!caps(serde_json::json!({"subagents": null})).supports_subagents());
        assert!(!caps(serde_json::json!({})).supports_subagents());
    }

    #[test]
    fn default_agent_capabilities_advertise_nothing_new() {
        roundtrip(
            &AgentCapabilities::default(),
            serde_json::json!({
                "loadSession": false,
                "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false},
                "mcpCapabilities": {"http": false, "sse": false},
                "sessionCapabilities": {}
            }),
        );
    }

    fn model_option() -> SessionConfigOption {
        SessionConfigOption {
            id: "model".into(),
            name: "Model".into(),
            description: None,
            category: Some(SessionConfigCategory::Model),
            kind: SessionConfigKind::Select,
            current_value: "a/one".into(),
            options: vec![
                SessionConfigSelectOption {
                    value: "a/one".into(),
                    name: "One".into(),
                    description: Some("the first".into()),
                },
                SessionConfigSelectOption {
                    value: "a/two".into(),
                    name: "Two".into(),
                    description: None,
                },
            ],
        }
    }

    fn model_option_json() -> serde_json::Value {
        serde_json::json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": "a/one",
            "options": [
                {"value": "a/one", "name": "One", "description": "the first"},
                {"value": "a/two", "name": "Two"}
            ]
        })
    }

    #[test]
    fn config_option_shapes() {
        roundtrip(&model_option(), model_option_json());
        roundtrip(
            &SessionConfigOption {
                id: "thinking".into(),
                name: "Thinking".into(),
                description: Some("reasoning effort".into()),
                category: Some(SessionConfigCategory::ThoughtLevel),
                kind: SessionConfigKind::Select,
                current_value: "high".into(),
                options: vec![],
            },
            serde_json::json!({
                "id": "thinking",
                "name": "Thinking",
                "description": "reasoning effort",
                "category": "thought_level",
                "type": "select",
                "currentValue": "high",
                "options": []
            }),
        );
        roundtrip(
            &SetSessionConfigOptionRequest {
                session_id: "s1".into(),
                config_id: "model".into(),
                value: "a/two".into(),
            },
            serde_json::json!({"sessionId": "s1", "configId": "model", "value": "a/two"}),
        );
        roundtrip(
            &SetSessionConfigOptionResponse {
                config_options: vec![model_option()],
            },
            serde_json::json!({"configOptions": [model_option_json()]}),
        );
    }

    #[test]
    fn session_method_shapes() {
        roundtrip(
            &NewSessionResponse {
                session_id: "s1".into(),
                config_options: vec![model_option()],
            },
            serde_json::json!({"sessionId": "s1", "configOptions": [model_option_json()]}),
        );
        roundtrip(
            &NewSessionResponse {
                session_id: "s1".into(),
                config_options: vec![],
            },
            serde_json::json!({"sessionId": "s1"}),
        );
        roundtrip(&LoadSessionResponse::default(), serde_json::json!({}));
        roundtrip(
            &LoadSessionResponse {
                config_options: vec![model_option()],
            },
            serde_json::json!({"configOptions": [model_option_json()]}),
        );
        roundtrip(
            &ListSessionsRequest {
                cwd: Some("/w".into()),
                cursor: Some("50".into()),
            },
            serde_json::json!({"cwd": "/w", "cursor": "50"}),
        );
        roundtrip(&ListSessionsRequest::default(), serde_json::json!({}));
        roundtrip(
            &ListSessionsResponse {
                sessions: vec![
                    SessionInfo {
                        session_id: "s1".into(),
                        cwd: "/w".into(),
                        title: Some("Fix the build".into()),
                        updated_at: Some("2026-09-25T10:00:00Z".into()),
                    },
                    SessionInfo {
                        session_id: "s2".into(),
                        cwd: "/w".into(),
                        title: None,
                        updated_at: None,
                    },
                ],
                next_cursor: Some("50".into()),
            },
            serde_json::json!({
                "sessions": [
                    {
                        "sessionId": "s1",
                        "cwd": "/w",
                        "title": "Fix the build",
                        "updatedAt": "2026-09-25T10:00:00Z"
                    },
                    {"sessionId": "s2", "cwd": "/w"}
                ],
                "nextCursor": "50"
            }),
        );
        roundtrip(
            &ResumeSessionRequest {
                session_id: "s1".into(),
                cwd: "/w".into(),
                mcp_servers: vec![],
            },
            serde_json::json!({"sessionId": "s1", "cwd": "/w", "mcpServers": []}),
        );
        roundtrip(&ResumeSessionResponse::default(), serde_json::json!({}));
    }

    #[test]
    fn mcp_server_shapes() {
        roundtrip(
            &McpServer::Stdio(McpServerStdio {
                name: "fs".into(),
                command: "/bin/mcp-fs".into(),
                args: vec!["--root".into(), "/w".into()],
                env: vec![EnvVariable {
                    name: "TOKEN".into(),
                    value: "x".into(),
                }],
            }),
            serde_json::json!({
                "name": "fs",
                "command": "/bin/mcp-fs",
                "args": ["--root", "/w"],
                "env": [{"name": "TOKEN", "value": "x"}]
            }),
        );
        let remote = McpServerHttp {
            name: "api".into(),
            url: "https://example.com/mcp".into(),
            headers: vec![HttpHeader {
                name: "Authorization".into(),
                value: "Bearer x".into(),
            }],
        };
        let remote_json = |kind| {
            serde_json::json!({
                "type": kind,
                "name": "api",
                "url": "https://example.com/mcp",
                "headers": [{"name": "Authorization", "value": "Bearer x"}]
            })
        };
        roundtrip(&McpServer::Http(remote.clone()), remote_json("http"));
        roundtrip(&McpServer::Sse(remote), remote_json("sse"));
        let load: LoadSessionRequest = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "cwd": "/w",
            "mcpServers": [
                {"name": "fs", "command": "mcp-fs", "args": [], "env": []},
                remote_json("http")
            ]
        }))
        .unwrap();
        assert!(matches!(load.mcp_servers[0], McpServer::Stdio(_)));
        assert!(matches!(load.mcp_servers[1], McpServer::Http(_)));
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
    fn usage_info_and_config_update_shapes() {
        roundtrip(
            &SessionUpdate::UsageUpdate(UsageUpdate {
                used: 1200,
                size: 200_000,
                cost: Some(Cost {
                    amount: 0.25,
                    currency: "USD".into(),
                }),
            }),
            serde_json::json!({
                "sessionUpdate": "usage_update",
                "used": 1200,
                "size": 200_000,
                "cost": {"amount": 0.25, "currency": "USD"}
            }),
        );
        roundtrip(
            &SessionUpdate::UsageUpdate(UsageUpdate {
                used: 5,
                size: 10,
                cost: None,
            }),
            serde_json::json!({"sessionUpdate": "usage_update", "used": 5, "size": 10}),
        );
        roundtrip(
            &SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                title: Some("Fix the build".into()),
                updated_at: Some("2026-09-25T10:00:00Z".into()),
            }),
            serde_json::json!({
                "sessionUpdate": "session_info_update",
                "title": "Fix the build",
                "updatedAt": "2026-09-25T10:00:00Z"
            }),
        );
        roundtrip(
            &SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate {
                config_options: vec![model_option()],
            }),
            serde_json::json!({
                "sessionUpdate": "config_option_update",
                "configOptions": [model_option_json()]
            }),
        );
    }

    #[test]
    fn subagent_update_shapes() {
        roundtrip(
            &SessionUpdate::SubagentUpdate(SubagentUpdate {
                subagent_session_id: "child".into(),
                name: Some("reviewer".into()),
                task: Some("review the diff".into()),
                capabilities: Some(SubagentSessionCapabilities { cancel: true }),
                state: None,
            }),
            serde_json::json!({
                "sessionUpdate": "subagent_update",
                "subagentSessionId": "child",
                "name": "reviewer",
                "task": "review the diff",
                "capabilities": {"cancel": true}
            }),
        );
        for (state, name) in [
            (SubagentState::Running, "running"),
            (SubagentState::Completed, "completed"),
            (SubagentState::Failed, "failed"),
            (SubagentState::Cancelled, "cancelled"),
        ] {
            roundtrip(
                &SessionUpdate::SubagentUpdate(SubagentUpdate {
                    subagent_session_id: "child".into(),
                    state: Some(state),
                    ..SubagentUpdate::default()
                }),
                serde_json::json!({
                    "sessionUpdate": "subagent_update",
                    "subagentSessionId": "child",
                    "state": name
                }),
            );
        }
    }

    #[test]
    fn unknown_session_update_parses_as_unknown() {
        let note: SessionNotification = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "update": {"sessionUpdate": "compaction_update", "phase": "started"}
        }))
        .unwrap();
        assert_eq!(note.update, SessionUpdate::Unknown);
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
