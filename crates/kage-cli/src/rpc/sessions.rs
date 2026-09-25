//! Listing, loading and resuming recorded sessions.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::Path;

use kage_acp::acp::{
    ContentBlock, ListSessionsRequest, ListSessionsResponse, McpServer, MessageChunk,
    SessionConfigOption, SessionInfo, SessionInfoUpdate, SessionUpdate,
};
use kage_acp::agent::PromptContext;
use kage_core::sync::lock;
use kage_core::{Content, LoopEvent, Message, MessageId, Role, ToolOutput};
use kage_jsonrpc::RpcError;
use kage_loop::TokenBudget;
use kage_session::SessionWriter;

use super::bridge::to_update;
use super::content::image_block;
use super::mcp::editor_servers;
use super::options::config_options;
use crate::engine::Recorder;

/// Sessions per `session/list` page.
const LIST_PAGE: usize = 50;

impl super::CliAcpAgent {
    /// Opens the recorded session `client_id` names the way the TUI resumes
    /// one: on its recorded model when that resolves, with its thinking
    /// level and token totals, and with the client's MCP `servers`. With
    /// `ctx`, first replays its history and title to the client. Returns
    /// the session's config options.
    pub(super) fn open_recorded(
        &self,
        client_id: &str,
        cwd: &str,
        servers: &[McpServer],
        ctx: Option<&PromptContext>,
    ) -> Result<Vec<SessionConfigOption>, RpcError> {
        let servers = editor_servers(servers)?;
        let path = kage_session::find_by_prefix(&self.sessions, client_id)
            .map_err(|e| RpcError::internal(e.to_string()))?
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {client_id}")))?;
        let id = crate::engine::session_id_of(&path).ok_or_else(|| {
            RpcError::internal(format!("bad session file name {}", path.display()))
        })?;
        let replay = kage_session::replay(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        if let Some(ctx) = ctx {
            for update in replay_updates(&replay.history) {
                ctx.update(update);
            }
            if let Some(title) = replay.title {
                ctx.update(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                    title: Some(title),
                    updated_at: None,
                }));
            }
        }
        if lock(&self.ids).by_engine.contains_key(&id) {
            lock(&self.ids).insert(client_id.to_owned(), id);
            let shown = lock(&self.shown);
            return Ok(shown
                .get(&id)
                .map(|shown| config_options(&self.models, &shown.settings))
                .unwrap_or_default());
        }
        let writer = SessionWriter::open(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        let model = if self.registry.resolve(&replay.model).is_ok() {
            replay.model
        } else {
            eprintln!(
                "kage: rpc: session model {} unavailable; using {} instead",
                replay.model, self.default_model
            );
            self.default_model.clone()
        };
        let mut spec = (self.spec)(id, cwd, &model, servers)?;
        spec.cx.history = replay.history;
        spec.cx.budget = TokenBudget {
            used_input: replay.usage_total.input,
            used_output: replay.usage_total.output,
            used_cache_read: replay.usage_total.cache_read,
            used_cache_write: replay.usage_total.cache_write,
            current_context: replay.usage_total.last_context,
        };
        spec.cx.thinking_level = replay
            .thinking_level
            .as_deref()
            .and_then(kage_core::ThinkingLevel::parse);
        spec.recorder = Some(Recorder::new(writer, spec.plugins.clone()));
        Ok(self.open(client_id.to_owned(), spec))
    }
}

/// The `session/update`s that show `history` as the live bridge showed
/// it: user chunks, then each assistant block and tool result mapped
/// through [`to_update`].
fn replay_updates(history: &[Message]) -> Vec<SessionUpdate> {
    let mut seen = HashMap::new();
    let mut updates = Vec::new();
    for message in history {
        for block in &message.content {
            let update = match (message.role, block) {
                (_, Content::Text { text } | Content::Thinking { text, .. }) if text.is_empty() => {
                    None
                }
                (Role::User, Content::Text { text }) => Some(user_chunk(ContentBlock::text(text))),
                (Role::User, Content::Image { source, mime }) => {
                    Some(user_chunk(image_block(source, mime)))
                }
                (_, block) => {
                    replay_event(message.id, block).and_then(|e| to_update(&mut seen, &e))
                }
            };
            updates.extend(update);
        }
    }
    updates
}

/// The loop event that streamed `block` of message `id`, if it shows.
fn replay_event(id: MessageId, block: &Content) -> Option<LoopEvent> {
    match block {
        Content::Text { text } => Some(LoopEvent::TextDelta {
            id,
            delta: text.clone(),
        }),
        Content::Thinking { text, .. } => Some(LoopEvent::ThinkingDelta {
            id,
            delta: text.clone(),
        }),
        Content::ToolCall { id, name, input } => Some(LoopEvent::ToolCallStart {
            id: id.clone(),
            name: name.clone(),
            input_partial: input.clone(),
        }),
        Content::ToolResultBlock {
            call_id,
            output,
            is_error,
        } => Some(LoopEvent::ToolCallEnd {
            id: call_id.clone(),
            output: ToolOutput {
                is_error: *is_error,
                text: output.clone(),
                ..ToolOutput::default()
            },
        }),
        Content::Image { .. } | Content::Custom { .. } => None,
    }
}

fn user_chunk(content: ContentBlock) -> SessionUpdate {
    SessionUpdate::UserMessageChunk(MessageChunk { content })
}

/// One `session/list` page of the client sessions recorded in `dir`,
/// newest activity first. The cursor is the offset of the page.
pub(super) fn list_page(
    dir: &Path,
    req: &ListSessionsRequest,
) -> Result<ListSessionsResponse, RpcError> {
    let start = match &req.cursor {
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| RpcError::new(-32602, format!("invalid cursor {cursor}")))?,
        None => 0,
    };
    let cwd = req.cwd.as_deref().map(Path::new);
    let mut summaries: Vec<_> = kage_session::list(dir)
        .map_err(|e| RpcError::internal(e.to_string()))?
        .into_iter()
        .filter(|s| s.agent.is_none() && cwd.is_none_or(|cwd| s.cwd == cwd))
        .collect();
    summaries.sort_by_key(|s| Reverse(s.updated_at));
    let end = start.saturating_add(LIST_PAGE);
    let next_cursor = (end < summaries.len()).then(|| end.to_string());
    let sessions = summaries
        .into_iter()
        .skip(start)
        .take(LIST_PAGE)
        .map(|s| SessionInfo {
            session_id: s.id.to_string(),
            cwd: s.cwd.display().to_string(),
            title: s.title,
            updated_at: Some(
                s.updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
        })
        .collect();
    Ok(ListSessionsResponse {
        sessions,
        next_cursor,
    })
}
