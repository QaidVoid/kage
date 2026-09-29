//! Engine events as ACP traffic.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use kage_acp::acp::{
    AvailableCommandsUpdate, ConfigOptionUpdate, ContentBlock, Cost, MessageChunk,
    SessionConfigSelectOption, SessionInfoUpdate, SessionUpdate, SubagentSessionCapabilities,
    SubagentState, SubagentUpdate, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    ToolKind, UsageUpdate,
};
use kage_acp::agent::{PermissionDecision, send_update};
use kage_core::protocol::{
    AgentNode, AgentTree, Command, CommandKind, Envelope, Event, HostEvent,
    PermissionDecision as Decision, RequestId, RunOutcome, Usage, with_canonical_tool_names,
};
use kage_core::sync::lock;
use kage_core::{
    CancelFlag, Content, LoopEvent, Message, Role, SessionId, StopReason as CoreStopReason,
    ToolCallId,
};
use kage_jsonrpc::Peer;

use super::content::image_block;
use super::host::Owners;
use super::mcp::prompt_commands;
use super::options::{Settings, config_options};
use super::{Held, Ids, PromptEnd, ShownBySession, Waiters};
use crate::engine::Commander;

/// Updates buffered for one unannounced client session before later
/// ones are dropped. A client that opens a session but never announces
/// it would otherwise pin every update it generates, full tool
/// outputs included, for as long as the connection lives.
pub(super) const HELD_CAP: usize = 4096;

/// Turns engine events into ACP traffic for the sessions a client opened
/// and the agents under them.
pub(super) struct Bridge {
    pub(super) peer: Peer,
    pub(super) commander: Commander,
    /// This connection's id on the host.
    pub(super) connection: u64,
    /// The connection each session's running prompt came from.
    pub(super) owners: Owners,
    pub(super) ids: Arc<Mutex<Ids>>,
    pub(super) waiters: Waiters,
    pub(super) models: Arc<[SessionConfigSelectOption]>,
    pub(super) shown: ShownBySession,
    /// Advertised name to real name for tools the host renamed, so a
    /// call the model makes under its advertised name still renders
    /// with the real tool's title, kind, and read-only grouping.
    pub(super) aliases: BTreeMap<String, String>,
    pub(super) seen: HashMap<SessionId, HashMap<String, serde_json::Value>>,
    pub(super) stops: HashMap<SessionId, CoreStopReason>,
    pub(super) tree: AgentTree,
    /// Whether the client advertised the subagents capability.
    pub(super) subagents: Arc<AtomicBool>,
    /// Announced subagents whose terminal state is not sent yet.
    pub(super) live: HashSet<SessionId>,
    /// Ended runs held back until their live subagents end.
    pub(super) ended: HashMap<SessionId, PromptEnd>,
    /// The commands last sent to each client session.
    pub(super) commands: HashMap<SessionId, Vec<serde_json::Value>>,
    pub(super) held: Held,
    /// Agent calls waiting for approval, by session and call id, with the
    /// line their card shows again once they run.
    pub(super) approving: HashMap<(SessionId, String), String>,
    /// This connection's open asks, shared with its agent so a detach
    /// can withdraw them.
    pub(super) asks: AskSet,
}

/// A permission question in flight on its own thread.
pub(super) struct Ask {
    /// The engine request the ask came from.
    request_id: RequestId,
    withdraw: CancelFlag,
    thread: std::thread::JoinHandle<()>,
}

impl Ask {
    /// Stops the ask and waits until its thread is done. A withdrawn ask
    /// sends no decision.
    pub(super) fn stop(self) {
        self.withdraw.cancel();
        let _ = self.thread.join();
    }
}

/// The open asks of one connection, keyed by the session that asked.
pub(super) type AskSet = Arc<Mutex<HashMap<SessionId, Vec<Ask>>>>;

impl Bridge {
    pub(super) fn handle(&mut self, envelope: &Envelope) {
        let envelope = &with_canonical_tool_names(envelope.clone(), &self.aliases);
        if let Event::Host(HostEvent::PermissionResolved { request_id }) = &envelope.event {
            self.withdraw(*request_id);
        }
        let session = envelope.session;
        let is_agent = self.tree.apply(envelope);
        let client_id = lock(&self.ids).by_engine.get(&session).cloned();
        match client_id {
            Some(client_id) => self.handle_client(session, client_id, &envelope.event),
            None if is_agent && self.subagents.load(Ordering::SeqCst) => {
                self.handle_subagent(session, &envelope.event);
            }
            None if is_agent => self.handle_agent(session, &envelope.event),
            None => {}
        }
    }

    fn handle_client(&mut self, session: SessionId, client_id: String, event: &Event) {
        match event {
            Event::Loop(event) => {
                if let LoopEvent::MessageEnd { stop_reason, .. } = event {
                    self.stops.insert(session, *stop_reason);
                }
                if let LoopEvent::MessageAppended { message } = event {
                    self.echo(session, &client_id, message);
                }
                let seen = self.seen.entry(session).or_default();
                if let Some(update) = to_update(seen, event) {
                    self.send(session, &client_id, update);
                }
            }
            Event::Host(HostEvent::PermissionRequested {
                request_id,
                tool_call_id,
                tool,
                input,
                ..
            }) => {
                let tool_call = permission_call(tool_call_id.as_ref(), tool, input);
                self.ask(session, *request_id, client_id, tool_call);
            }
            Event::Host(HostEvent::UsageUpdated { usage }) => {
                if let Some(update) = usage_update(usage) {
                    self.send(session, &client_id, update);
                }
            }
            Event::Host(HostEvent::StateChanged { state }) => {
                let settings = Settings::from(state);
                let changed = lock(&self.shown)
                    .get_mut(&session)
                    .is_some_and(|shown| shown.observe(&settings));
                if changed {
                    let update = ConfigOptionUpdate {
                        config_options: config_options(&self.models, &settings),
                    };
                    self.send(
                        session,
                        &client_id,
                        SessionUpdate::ConfigOptionUpdate(update),
                    );
                }
            }
            Event::Host(HostEvent::TitleChanged { title }) => {
                let update = SessionInfoUpdate {
                    title: Some(title.clone()),
                    updated_at: None,
                };
                self.send(
                    session,
                    &client_id,
                    SessionUpdate::SessionInfoUpdate(update),
                );
            }
            Event::Host(HostEvent::McpServers { servers }) if !self.live.contains(&session) => {
                let commands = prompt_commands(servers);
                let last = self.commands.insert(session, commands.clone());
                if last.unwrap_or_default() != commands {
                    let update = AvailableCommandsUpdate {
                        available_commands: commands,
                    };
                    self.send(
                        session,
                        &client_id,
                        SessionUpdate::AvailableCommandsUpdate(update),
                    );
                }
            }
            Event::Host(HostEvent::RunEnded { outcome }) => {
                lock(&self.owners).remove(&session);
                self.end_asks(session);
                self.seen.remove(&session);
                let stop = self.stops.remove(&session);
                let end = PromptEnd {
                    outcome: outcome.clone(),
                    stop,
                };
                self.ended.insert(session, end);
                self.settle(session);
            }
            Event::Host(_) => {}
        }
    }

    /// Sends `update` to the client, or holds it while the response that
    /// names the session is still on its way.
    fn send(&self, session: SessionId, client_id: &str, update: SessionUpdate) {
        let mut held = lock(&self.held);
        if let Some(updates) = held.get_mut(&session) {
            hold(updates, update);
            return;
        }
        drop(held);
        send_update(&self.peer, client_id, update);
    }

    /// Announces an agent as a subagent of its parent's client session,
    /// then shows its activity on its own session until it ends.
    fn handle_subagent(&mut self, session: SessionId, event: &Event) {
        if let Event::Host(HostEvent::AgentSpawned {
            parent,
            agent,
            description,
            ..
        }) = event
        {
            let Some(parent_id) = self.client_of(*parent) else {
                return;
            };
            self.live.insert(session);
            lock(&self.ids)
                .subagents
                .insert(session.to_string(), session);
            let update = SubagentUpdate {
                subagent_session_id: session.to_string(),
                name: Some(agent.clone()),
                task: Some(description.clone()),
                capabilities: Some(SubagentSessionCapabilities { cancel: true }),
                state: None,
            };
            send_update(
                &self.peer,
                &parent_id,
                SessionUpdate::SubagentUpdate(update),
            );
        } else if self.live.contains(&session) {
            self.handle_client(session, session.to_string(), event);
        }
    }

    /// The client's id for `session`: a client session's own id, or a
    /// live subagent's engine id.
    fn client_of(&self, session: SessionId) -> Option<String> {
        let client_id = lock(&self.ids).by_engine.get(&session).cloned();
        client_id.or_else(|| self.live.contains(&session).then(|| session.to_string()))
    }

    /// Finishes the ended run of `session` once none of its subagents is
    /// live: a subagent sends its terminal state to its parent, and a
    /// client session answers its waiting prompt. The RFD wants every
    /// subagent to end before its parent does.
    fn settle(&mut self, session: SessionId) {
        let waits = self
            .live
            .iter()
            .any(|s| self.tree.get(*s).is_some_and(|node| node.parent == session));
        if waits {
            return;
        }
        let Some(end) = self.ended.remove(&session) else {
            return;
        };
        if !self.live.remove(&session) {
            if let Some(waiter) = lock(&self.waiters).remove(&session) {
                let _ = waiter.send(end);
            }
            self.prune_tree(session);
            return;
        }
        lock(&self.ids).subagents.remove(&session.to_string());
        let Some(parent) = self.tree.get(session).map(|node| node.parent) else {
            return;
        };
        if let Some(parent_id) = self.client_of(parent) {
            let update = SubagentUpdate {
                subagent_session_id: session.to_string(),
                state: Some(match end.outcome {
                    RunOutcome::Completed => SubagentState::Completed,
                    RunOutcome::Cancelled => SubagentState::Cancelled,
                    RunOutcome::Failed { .. } => SubagentState::Failed,
                }),
                ..SubagentUpdate::default()
            };
            send_update(
                &self.peer,
                &parent_id,
                SessionUpdate::SubagentUpdate(update),
            );
        }
        self.settle(parent);
    }

    /// Forgets the agents under a client session whose run just
    /// settled. Every member is terminal by then, so the nodes, each
    /// carrying its agent's latest tool input, would otherwise outlive
    /// the agents they describe. A later run re-announces its agents
    /// with fresh `AgentSpawned` envelopes.
    fn prune_tree(&mut self, session: SessionId) {
        let roots: Vec<SessionId> = self
            .tree
            .under(session)
            .into_iter()
            .filter_map(|(depth, node)| (depth == 1).then_some(node.session))
            .collect();
        for root in roots {
            self.tree.remove_subtree(root);
        }
    }

    /// Shows an agent's activity as the content of the root session's
    /// top-level `agent` call, and forwards its permission requests to
    /// that session.
    fn handle_agent(&mut self, session: SessionId, event: &Event) {
        let Some(top) = top_agent(&self.tree, session) else {
            return;
        };
        let Some(client_id) = lock(&self.ids).by_engine.get(&top.parent).cloned() else {
            return;
        };
        let call_id = top.tool_call_id.to_string();
        let agent = self
            .tree
            .get(session)
            .map_or_else(String::new, |node| node.agent.clone());
        let progress = |text: String| {
            let update = ToolCallUpdate {
                tool_call_id: call_id.clone(),
                content: vec![text_content(format!("{agent}: {text}"))],
                ..ToolCallUpdate::default()
            };
            send_update(
                &self.peer,
                &client_id,
                SessionUpdate::ToolCallUpdate(update),
            );
        };
        match event {
            Event::Loop(LoopEvent::ToolCallStart {
                name,
                input_partial,
                ..
            }) => progress(describe_call(name, input_partial)),
            Event::Loop(LoopEvent::ToolExecutionStart { id }) => {
                if let Some(line) = self.approving.remove(&(session, id.to_string())) {
                    progress(line);
                }
            }
            Event::Host(HostEvent::PermissionRequested {
                request_id,
                tool_call_id,
                tool,
                input,
                ..
            }) => {
                let line = describe_call(tool, input);
                progress(format!("Waiting for approval: {line}"));
                if let Some(id) = tool_call_id {
                    self.approving.insert((session, id.to_string()), line);
                }
                let tool_call = ToolCallUpdate {
                    tool_call_id: call_id.clone(),
                    title: Some(format!("{agent}: {}", tool_title(tool))),
                    kind: Some(tool_kind(tool)),
                    raw_input: Some(input.clone()),
                    ..ToolCallUpdate::default()
                };
                self.ask(session, *request_id, client_id, tool_call);
            }
            Event::Host(HostEvent::RunEnded { outcome }) => {
                progress(
                    match outcome {
                        RunOutcome::Completed => "done",
                        RunOutcome::Cancelled => "stopped",
                        RunOutcome::Failed { .. } => "failed",
                    }
                    .to_owned(),
                );
                self.approving.retain(|(s, _), _| *s != session);
                self.end_asks(session);
            }
            _ => {}
        }
    }

    /// Asks the client on `client_id` and resolves `request_id` of
    /// `session` with the answer. The ask is withdrawn when that
    /// session's run ends first, when another client's answer resolves
    /// it, or when the connection detaches; a withdrawn ask sends no
    /// decision.
    fn ask(
        &mut self,
        session: SessionId,
        request_id: RequestId,
        client_id: String,
        tool_call: ToolCallUpdate,
    ) {
        let withdraw = CancelFlag::new();
        let flag = withdraw.clone();
        let peer = self.peer.clone();
        let commander = self.commander.clone();
        let thread = std::thread::spawn(move || {
            let title = tool_call.title.clone().unwrap_or_default();
            let decision =
                kage_acp::agent::request_permission(&peer, &client_id, tool_call, &title, &flag);
            let decision = match decision {
                PermissionDecision::Unanswered => return,
                PermissionDecision::Allow => Decision::AllowOnce,
                PermissionDecision::AllowSession => Decision::AllowSession,
                PermissionDecision::Deny(_) => Decision::Deny,
            };
            commander.send(Command::to(
                session,
                CommandKind::ResolvePermission {
                    request_id,
                    decision,
                },
            ));
        });
        lock(&self.asks).entry(session).or_default().push(Ask {
            request_id,
            withdraw,
            thread,
        });
    }

    /// Withdraws the open asks of `session` and waits until each is
    /// answered or withdrawn, so no update that follows overtakes them.
    fn end_asks(&mut self, session: SessionId) {
        let asks = lock(&self.asks).remove(&session).unwrap_or_default();
        for ask in asks {
            ask.stop();
        }
    }

    /// Withdraws this connection's open ask for `request_id`: another
    /// client's answer resolved it, so the dialog closes here and a late
    /// answer sends nothing.
    fn withdraw(&mut self, request_id: RequestId) {
        let mut matched = Vec::new();
        let mut asks = lock(&self.asks);
        for session_asks in asks.values_mut() {
            let (taken, kept): (Vec<_>, Vec<_>) = session_asks
                .drain(..)
                .partition(|ask| ask.request_id == request_id);
            *session_asks = kept;
            matched.extend(taken);
        }
        asks.retain(|_, session_asks| !session_asks.is_empty());
        drop(asks);
        for ask in matched {
            ask.stop();
        }
    }

    /// Shows the user message a run opened with to the attached clients
    /// that did not send it, so their reply does not arrive without its
    /// question. The client that owns the run sees nothing.
    fn echo(&self, session: SessionId, client_id: &str, message: &Message) {
        if message.role != Role::User {
            return;
        }
        let owner = lock(&self.owners).get(&session).copied();
        if owner.is_none_or(|owner| owner == self.connection) {
            return;
        }
        for block in &message.content {
            let block = match block {
                Content::Text { text } if !text.is_empty() => {
                    kage_acp::acp::ContentBlock::text(text.clone())
                }
                Content::Image { source, mime } => image_block(source, mime),
                _ => continue,
            };
            self.send(session, client_id, user_chunk(block));
        }
    }
}

/// The `user_message_chunk` showing `content` to a client that did not
/// send it.
pub(super) fn user_chunk(content: kage_acp::acp::ContentBlock) -> SessionUpdate {
    SessionUpdate::UserMessageChunk(kage_acp::acp::MessageChunk { content })
}

/// Buffers `update` for an unannounced session, keeping the first/// [`HELD_CAP`] and dropping later ones. The early updates are the
/// ones a client replays first; nothing here is worth an unbounded
/// buffer on a session that never announces.
pub(super) fn hold(updates: &mut Vec<SessionUpdate>, update: SessionUpdate) -> bool {
    if updates.len() >= HELD_CAP {
        return false;
    }
    updates.push(update);
    true
}

/// The tool call a permission request for `tool` shows.
fn permission_call(
    tool_call_id: Option<&ToolCallId>,
    tool: &str,
    input: &serde_json::Value,
) -> ToolCallUpdate {
    ToolCallUpdate {
        tool_call_id: tool_call_id.map_or_else(String::new, ToString::to_string),
        title: Some(tool_title(tool)),
        kind: Some(tool_kind(tool)),
        status: Some(ToolCallStatus::Pending),
        raw_input: Some(input.clone()),
        ..ToolCallUpdate::default()
    }
}

/// The agent that a client session's own `agent` call started, at the top
/// of `session`'s branch.
fn top_agent(tree: &AgentTree, session: SessionId) -> Option<&AgentNode> {
    let mut node = tree.get(session)?;
    while let Some(parent) = tree.get(node.parent) {
        node = parent;
    }
    Some(node)
}

/// One line naming what a tool call does, such as `Read src/lib.rs`.
fn describe_call(name: &str, input: &serde_json::Value) -> String {
    let label = kage_tui::view::tool_view::describe(name, input);
    if label.target.is_empty() {
        label.verb.to_owned()
    } else {
        format!("{} {}", label.verb, label.target)
    }
}

/// Translate a loop event into the matching ACP `session/update`. The
/// first sighting of a tool call id sends `tool_call`. After it, streamed
/// input for that id sends a `tool_call_update` only when the input
/// changed, and the later steps always do. `seen` holds the input last
/// sent for each call.
pub(super) fn to_update(
    seen: &mut HashMap<String, serde_json::Value>,
    event: &LoopEvent,
) -> Option<SessionUpdate> {
    match event {
        LoopEvent::TextDelta { delta, .. } => {
            Some(SessionUpdate::AgentMessageChunk(MessageChunk {
                content: ContentBlock::text(delta.clone()),
            }))
        }
        LoopEvent::ThinkingDelta { delta, .. } => {
            Some(SessionUpdate::AgentThoughtChunk(MessageChunk {
                content: ContentBlock::text(delta.clone()),
            }))
        }
        LoopEvent::ToolCallArgsDelta {
            id,
            name,
            input_partial,
        }
        | LoopEvent::ToolCallStart {
            id,
            name,
            input_partial,
        } => match seen.insert(id.to_string(), input_partial.clone()) {
            None => Some(SessionUpdate::ToolCall(ToolCall {
                tool_call_id: id.to_string(),
                title: tool_title(name),
                kind: tool_kind(name),
                status: ToolCallStatus::Pending,
                content: Vec::new(),
                raw_input: Some(input_partial.clone()),
            })),
            Some(last) if last == *input_partial => None,
            Some(_) => Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: id.to_string(),
                raw_input: Some(input_partial.clone()),
                ..ToolCallUpdate::default()
            })),
        },
        LoopEvent::ToolExecutionStart { id } => {
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: id.to_string(),
                status: Some(ToolCallStatus::InProgress),
                ..ToolCallUpdate::default()
            }))
        }
        LoopEvent::ToolUpdate { id, update } => {
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: id.to_string(),
                content: vec![text_content(update.content.clone())],
                ..ToolCallUpdate::default()
            }))
        }
        LoopEvent::ToolCallEnd { id, output } => {
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: id.to_string(),
                status: Some(if output.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                }),
                content: vec![text_content(output.text.clone())],
                raw_output: output.structured.clone(),
                ..ToolCallUpdate::default()
            }))
        }
        _ => None,
    }
}

/// The `usage_update` for `usage`, or `None` while the context window is
/// unknown. Cost is left out until the model has pricing.
pub(super) fn usage_update(usage: &Usage) -> Option<SessionUpdate> {
    if usage.context_window == 0 {
        return None;
    }
    let cost = (usage.cost > 0.0).then(|| Cost {
        amount: usage.cost,
        currency: "USD".to_owned(),
    });
    Some(SessionUpdate::UsageUpdate(UsageUpdate {
        used: usage.context_used,
        size: usage.context_window,
        cost,
    }))
}

fn text_content(text: String) -> ToolCallContent {
    ToolCallContent::Content(MessageChunk {
        content: ContentBlock::text(text),
    })
}

/// The title a client shows for tool `name`: `server.tool` for an MCP
/// tool, as the TUI shows it, else the name.
fn tool_title(name: &str) -> String {
    match name.split_once("__") {
        Some((server, tool)) if !server.is_empty() && !tool.is_empty() => {
            format!("{server}.{tool}")
        }
        _ => name.to_owned(),
    }
}

/// ACP kind hint for a built-in tool name.
pub(super) fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" | "ls" => ToolKind::Read,
        "grep" | "find" => ToolKind::Search,
        "write" | "edit" => ToolKind::Edit,
        "shell" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}
