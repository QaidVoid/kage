//! Listing, loading and resuming recorded sessions.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kage_acp::acp::{
    ContentBlock, ListSessionsRequest, ListSessionsResponse, McpServer, PromptRef,
    SessionConfigOption, SessionExportResponse, SessionForkRequest, SessionForkResponse,
    SessionInfo, SessionInfoKage, SessionInfoMeta, SessionInfoUpdate, SessionUpdate,
    SubagentSessionCapabilities, SubagentState, SubagentSwarm, SubagentUpdate, TurnPhase,
    TurnReason, TurnUpdate,
};
use kage_acp::agent::{PromptContext, send_update};
use kage_core::protocol::{AgentNode, AgentState, AgentTree};
use kage_core::sync::lock;
use kage_core::{Content, LoopEvent, Message, MessageId, Role, SessionId, ToolOutput};
use kage_jsonrpc::RpcError;
use kage_loop::TokenBudget;
use kage_session::{EntryId, SessionEntry, SessionReader, SessionWriter};

use super::bridge::to_update;
use super::bridge::user_chunk;
use super::content::image_block;
use super::mcp::editor_servers;
use super::options::config_options;
use crate::engine::{Recorder, render_session_markdown};

/// Sessions per `session/list` page.
const LIST_PAGE: usize = 50;

impl super::CliAcpAgent {
    /// Opens the recorded session `client_id` names the way the TUI resumes
    /// one: on its recorded model when that resolves, with its thinking
    /// level and token totals, and with the client's MCP `servers`. With
    /// `ctx`, streams the turn in flight, the title, the live subagents
    /// and the open asks of a session the host already runs, and with
    /// `replay_file` also its recorded history. Returns the session's
    /// config options.
    pub(super) fn open_recorded(
        &self,
        client_id: &str,
        cwd: &str,
        servers: &[McpServer],
        ctx: Option<&PromptContext>,
        replay_file: bool,
    ) -> Result<Vec<SessionConfigOption>, RpcError> {
        let servers = editor_servers(servers)?;
        let path = self.recorded_path(client_id)?;
        if kage_session::is_agent_session(&path) {
            return self.open_agent(client_id, &path, ctx, replay_file);
        }
        let id = crate::engine::session_id_of(&path).ok_or_else(|| {
            RpcError::internal(format!("bad session file name {}", path.display()))
        })?;
        if lock(&self.ids).by_engine.contains_key(&id) {
            if replay_file && let Some(ctx) = ctx {
                let replay =
                    kage_session::replay(&path).map_err(|e| RpcError::internal(e.to_string()))?;
                for update in replay_updates(&replay.history) {
                    ctx.update(update);
                }
                self.announce_restored(id, &replay.history, ctx);
                if let Some(title) = replay.title {
                    ctx.update(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                        title: Some(title),
                        updated_at: None,
                    }));
                }
            }
            lock(&self.ids).insert(client_id.to_owned(), id);
            let shown = lock(&self.shown);
            return Ok(shown
                .get(&id)
                .map(|shown| config_options(&self.host.models, &shown.settings))
                .unwrap_or_default());
        }
        if let Some(settings) = self.host.open_settings(id) {
            return self.attach_live(id, client_id, &path, &settings, replay_file, ctx);
        }
        let replay = kage_session::replay(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        if replay_file && let Some(ctx) = ctx {
            for update in replay_updates(&replay.history) {
                ctx.update(update);
            }
            self.announce_restored(id, &replay.history, ctx);
            if let Some(title) = replay.title {
                ctx.update(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                    title: Some(title),
                    updated_at: None,
                }));
            }
        }
        let writer = SessionWriter::open(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        let model = if self.host.registry.resolve(&replay.model).is_ok() {
            replay.model
        } else {
            eprintln!(
                "kage: rpc: session model {} unavailable; using {} instead",
                replay.model, self.host.default_model
            );
            self.host.default_model.clone()
        };
        let mut spec = self.session_spec(id, cwd, &model, servers)?;
        spec.cx.history = replay.history.into_iter().map(Arc::new).collect();
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

    /// Shows the recorded agent session at `path` without hosting it:
    /// an agent belongs to the call that started it, so a client reads
    /// its transcript but never prompts it. With `replay_file`, streams
    /// its history and title.
    fn open_agent(
        &self,
        client_id: &str,
        path: &Path,
        ctx: Option<&PromptContext>,
        replay_file: bool,
    ) -> Result<Vec<SessionConfigOption>, RpcError> {
        if replay_file && let Some(ctx) = ctx {
            let replay =
                kage_session::replay(path).map_err(|e| RpcError::internal(e.to_string()))?;
            for update in replay_updates(&replay.history) {
                ctx.update(update);
            }
            self.announce_restored(replay.header.session, &replay.history, ctx);
            if let Some(title) = replay.title {
                ctx.update(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                    title: Some(title),
                    updated_at: None,
                }));
            }
        }
        lock(&self.ids).read_only.insert(client_id.to_owned());
        Ok(Vec::new())
    }

    /// Announces the finished agents the recorded session `root`
    /// started, at every depth: `root`'s own through `ctx`, deeper ones
    /// on the session of the agent that started them.
    pub(super) fn announce_restored(
        &self,
        root: SessionId,
        history: &[Message],
        ctx: &PromptContext,
    ) {
        for (parent, update) in restored_subagents(&self.host.sessions, root, history) {
            let update = SessionUpdate::SubagentUpdate(update);
            if parent == root {
                ctx.update(update);
            } else {
                send_update(&self.peer, &parent.to_string(), update);
            }
        }
    }

    /// The file of the recorded session `client_id` names.
    fn recorded_path(&self, client_id: &str) -> Result<PathBuf, RpcError> {
        kage_session::find_by_prefix(&self.host.sessions, client_id)
            .map_err(|e| RpcError::internal(e.to_string()))?
            .ok_or_else(|| RpcError::new(-32602, format!("unknown session {client_id}")))
    }

    /// Copies the recorded session `req.session_id` names into a new
    /// session, whole or up to the prompt `req.before` names.
    pub(super) fn fork_recorded(
        &self,
        req: &SessionForkRequest,
    ) -> Result<SessionForkResponse, RpcError> {
        let path = self.recorded_path(&req.session_id)?;
        let at = fork_point(&path, req.before.as_ref())?;
        let id = SessionId::new();
        let dst = self.host.sessions.join(format!("{id}.jsonl"));
        kage_session::fork(&path, &dst, id, at).map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(SessionForkResponse {
            session_id: id.to_string(),
        })
    }

    /// The recorded session `client_id` names as a Markdown transcript.
    pub(super) fn export_recorded(
        &self,
        client_id: &str,
    ) -> Result<SessionExportResponse, RpcError> {
        let path = self.recorded_path(client_id)?;
        let replay = kage_session::replay(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(SessionExportResponse {
            markdown: render_session_markdown(&replay),
        })
    }
}

/// The entry a fork of `path` copies through: the one just before the
/// prompt `before` names, or the last entry when `before` is absent.
fn fork_point(path: &Path, before: Option<&PromptRef>) -> Result<EntryId, RpcError> {
    let internal = |e: kage_session::SessionError| RpcError::internal(e.to_string());
    let mut last = None;
    let mut seen = 0;
    for entry in SessionReader::iter(path).map_err(internal)? {
        let entry = entry.map_err(internal)?;
        if let (Some(before), SessionEntry::Message(m)) = (before, &entry)
            && m.message.role == Role::User
            && prompt_text(&m.message) == before.text
        {
            if seen == before.occurrence {
                return last.ok_or_else(|| RpcError::internal("the prompt has no entry before it"));
            }
            seen += 1;
        }
        last = Some(entry.id());
    }
    match before {
        Some(_) => Err(RpcError::new(-32602, "the session has no such prompt")),
        None => last.ok_or_else(|| RpcError::internal("the session has no entries")),
    }
}

/// The first text block of a user message: the prompt as typed, ahead
/// of any attachment rendered as text.
fn prompt_text(message: &Message) -> &str {
    message
        .content
        .iter()
        .find_map(|block| match block {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The subagent updates that rebuild the finished agents of the
/// recorded session `root`: the ones its `history` started, then each
/// agent's own from that agent's file in `dir`, parents first. Each
/// comes with the session whose call started it. The records come
/// from the `<agent>` and `<swarm>` results the engine wrote, so they
/// survive a restart.
pub(super) fn restored_subagents(
    dir: &Path,
    root: SessionId,
    history: &[Message],
) -> Vec<(SessionId, SubagentUpdate)> {
    let mut tree = AgentTree::default();
    tree.restore(root, history);
    let mut pending: Vec<SessionId> = tree
        .under(root)
        .into_iter()
        .map(|(_, node)| node.session)
        .collect();
    let mut seen = HashSet::new();
    while let Some(session) = pending.pop() {
        if !seen.insert(session) {
            continue;
        }
        let Ok(replay) = kage_session::replay(&crate::build_session_path(dir, session)) else {
            continue;
        };
        tree.restore(session, &replay.history);
        pending.extend(
            tree.under(session)
                .into_iter()
                .map(|(_, node)| node.session),
        );
    }
    tree.under(root)
        .into_iter()
        .map(|(_, node)| (node.parent, restored_update(node)))
        .collect()
}

/// The announcement of a finished agent rebuilt from a stored result.
fn restored_update(node: &AgentNode) -> SubagentUpdate {
    SubagentUpdate {
        subagent_session_id: node.session.to_string(),
        name: Some(node.agent.clone()),
        task: Some(node.description.clone()),
        capabilities: Some(SubagentSessionCapabilities { cancel: false }),
        state: Some(match node.state {
            AgentState::Done => SubagentState::Completed,
            AgentState::Failed => SubagentState::Failed,
            AgentState::Cancelled => SubagentState::Cancelled,
            AgentState::Queued | AgentState::Running => SubagentState::Running,
        }),
        swarm: node.swarm.as_ref().map(|member| SubagentSwarm {
            id: member
                .batch
                .as_ref()
                .map_or_else(|| node.tool_call_id.to_string(), ToString::to_string),
            item: member.item.clone(),
            index: member.index,
            total: member.total,
        }),
        reason: None,
        tool_call_id: Some(node.tool_call_id.to_string()),
        usage: super::bridge::agent_usage(node),
        model: super::bridge::agent_model(node),
    }
}

/// The `session/update`s that show `history` as the live bridge showed
/// it: user chunks, then each assistant block and tool result mapped
/// through [`to_update`]. Thoughts and tool calls carry the durations
/// the file recorded, and each run that ended closes with a `_kage/turn`
/// end saying when and how long. No `plan` update: the todo list lives
/// in memory only, so a resumed session starts with an empty plan.
pub(super) fn replay_updates(history: &[Message]) -> Vec<SessionUpdate> {
    let mut seen = HashMap::new();
    let mut called = HashMap::new();
    let mut run: Option<(&Message, Option<&Message>)> = None;
    let mut updates = Vec::new();
    for message in history {
        let prompt = message.role == Role::User
            && message
                .content
                .iter()
                .any(|block| matches!(block, Content::Text { .. } | Content::Image { .. }));
        if prompt {
            updates.extend(run.and_then(|(start, last)| run_end(start, last?)));
            run = Some((message, None));
        } else if let Some((_, last)) = &mut run {
            *last = Some(message);
        }
        for block in &message.content {
            let update = match (message.role, block) {
                (_, Content::Text { text } | Content::Thinking { text, .. }) if text.is_empty() => {
                    None
                }
                (Role::User, Content::Text { text }) => Some(user_chunk(ContentBlock::text(text))),
                (Role::User, Content::Image { source, mime }) => {
                    Some(user_chunk(image_block(source, mime)))
                }
                (_, block) => replay_event(message.id, block)
                    .and_then(|e| to_update(&mut seen, &e))
                    .map(|update| timed(update, recorded_ms(message, block, &mut called))),
            };
            updates.extend(update);
        }
    }
    // A run whose last word is a reply without calls has ended; any
    // other may still be in flight.
    if let Some((start, Some(last))) = run
        && last.role == Role::Assistant
        && !last
            .content
            .iter()
            .any(|block| matches!(block, Content::ToolCall { .. }))
    {
        updates.extend(run_end(start, last));
    }
    updates
}

/// The `_kage/turn` end of the run `start` prompted and `last` closed,
/// with when it ended and how long it took.
fn run_end(start: &Message, last: &Message) -> Option<SessionUpdate> {
    let took = (last.ts - start.ts).to_std().ok()?;
    Some(SessionUpdate::Turn(TurnUpdate {
        phase: TurnPhase::End,
        reason: Some(TurnReason::NoToolCalls),
        at: Some(last.ts.timestamp()),
        took_ms: Some(u64::try_from(took.as_millis()).unwrap_or(u64::MAX)),
    }))
}

/// How long `block` of `message` took, as recorded: a thought's own
/// duration, or for a tool result the time since its call's message.
/// `called` holds when each call was made.
fn recorded_ms(
    message: &Message,
    block: &Content,
    called: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
) -> Option<u64> {
    match block {
        Content::Thinking { duration_ms, .. } => *duration_ms,
        Content::ToolCall { id, .. } => {
            called.insert(id.to_string(), message.ts);
            None
        }
        Content::ToolResultBlock { call_id, .. } => {
            let took = (message.ts - called.remove(&call_id.to_string())?)
                .to_std()
                .ok()?;
            Some(u64::try_from(took.as_millis()).unwrap_or(u64::MAX))
        }
        _ => None,
    }
}

/// `update` with the recorded duration `ms` on its thought chunk or
/// tool call update.
fn timed(mut update: SessionUpdate, ms: Option<u64>) -> SessionUpdate {
    if ms.is_none() {
        return update;
    }
    match &mut update {
        SessionUpdate::AgentThoughtChunk(chunk) => {
            chunk.meta.get_or_insert_default().kage.duration_ms = ms;
        }
        SessionUpdate::ToolCallUpdate(call) => {
            call.meta.get_or_insert_default().kage.duration_ms = ms;
        }
        _ => {}
    }
    update
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
            meta: s.parent_session.map(|parent| SessionInfoMeta {
                kage: Some(SessionInfoKage {
                    parent_session_id: Some(parent.to_string()),
                }),
            }),
        })
        .collect();
    Ok(ListSessionsResponse {
        sessions,
        next_cursor,
    })
}
