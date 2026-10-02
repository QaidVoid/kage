//! Engine events as ACP traffic.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use kage_acp::acp::{
    AvailableCommandsUpdate, CompactionUpdate, ConfigOptionUpdate, ContentBlock, Cost,
    CurrentModeUpdate, DiffContent, KageMeta, McpStatusUpdate, MessageChunk, NoticeTone,
    NoticeUpdate, Plan, SessionConfigSelectOption, SessionInfoUpdate, SessionUpdate,
    SubagentSessionCapabilities, SubagentState, SubagentSwarm, SubagentUpdate, SubagentUsage,
    SwarmMeta, ToolCall, ToolCallContent, ToolCallMeta, ToolCallStatus, ToolCallUpdate, ToolKind,
    TurnPhase, TurnReason, TurnUpdate, UsageUpdate,
};
use kage_acp::agent::{PermissionDecision, PlanReviewDecision, request_plan_review, send_update};
use kage_core::protocol::{
    AgentNode, AgentTree, Command, CommandKind, Delivery, EXIT_PLAN_TOOL, Envelope, Event,
    HostEvent, McpServerInfo, McpServerStatus, NoticeLevel, PermissionDecision as Decision,
    RequestId, RunOutcome, SessionState, Usage, with_canonical_tool_names,
};
use kage_core::sync::lock;
use kage_core::{
    CancelFlag, Content, LoopError, LoopEvent, Message, Role, SessionId,
    StopReason as CoreStopReason, TokenUsage, ToolCallId, ToolOutput,
};
use kage_jsonrpc::Peer;

use super::content::image_block;
use super::live::{Live, Seed};
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
    /// The host's live engine state, for the prompt owner of a session.
    pub(super) live: Arc<Mutex<Live>>,
    pub(super) ids: Arc<Mutex<Ids>>,
    pub(super) waiters: Waiters,
    pub(super) models: Arc<[SessionConfigSelectOption]>,
    pub(super) shown: ShownBySession,
    /// Advertised name to real name for tools the host renamed, so a
    /// call the model makes under its advertised name still renders
    /// with the real tool's title, kind, and read-only grouping.
    pub(super) aliases: BTreeMap<String, String>,
    pub(super) seen: HashMap<SessionId, HashMap<String, SeenCall>>,
    pub(super) stops: HashMap<SessionId, CoreStopReason>,
    pub(super) tree: AgentTree,
    /// Whether the client advertised the subagents capability.
    pub(super) subagents: Arc<AtomicBool>,
    /// Announced subagents whose terminal state is not sent yet.
    pub(super) streaming: HashSet<SessionId>,
    /// Ended runs held back until their live subagents end.
    pub(super) ended: HashMap<SessionId, PromptEnd>,
    /// The commands last sent to each client session.
    pub(super) commands: HashMap<SessionId, Vec<serde_json::Value>>,
    /// The mode id `current_mode_update` last reported per session, so
    /// only a change sends one.
    pub(super) modes: HashMap<SessionId, String>,
    /// The tool of every in-flight call per session, so a completed
    /// `todo_list` write can become a `plan` update.
    pub(super) names: HashMap<SessionId, HashMap<String, String>>,
    /// Last-known MCP statuses per session, so `_kage/mcp_status` only
    /// carries a server whose status changed.
    pub(super) statuses: HashMap<SessionId, HashMap<String, McpServerStatus>>,
    /// Last known context fill per session, in tokens, from usage
    /// reports.
    pub(super) fills: HashMap<SessionId, u64>,
    /// The cost of a client session's agents already pruned from the
    /// tree, which a client without the subagents capability sees in
    /// the session's own cost.
    pub(super) pruned_cost: HashMap<SessionId, f64>,
    /// Compactions waiting for the post-compaction usage: the turn
    /// count kept and the fill before the compaction.
    pub(super) compacting: HashMap<SessionId, (u64, u64)>,
    /// Streaming children the engine requeued after a rate limit, so
    /// the `RunEnded` that precedes their next run does not end them.
    pub(super) paused: HashSet<SessionId>,
    pub(super) held: Held,
    /// Agent calls waiting for approval, by session and call id, with the
    /// line their card shows again once they run.
    pub(super) approving: HashMap<(SessionId, String), String>,
    /// This connection's open asks, shared with its agent so a detach
    /// can withdraw them.
    pub(super) asks: AskSet,
    /// Attaches waiting to be applied: what a connection that loaded a
    /// hosted session needs before its next envelope routes.
    pub(super) seeds: Arc<Mutex<Vec<Seed>>>,
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
    /// sends no decision. Returns the engine request it was for.
    pub(super) fn stop(self) -> RequestId {
        self.withdraw.cancel();
        let _ = self.thread.join();
        self.request_id
    }
}

/// The open asks of one connection, keyed by the session that asked.
pub(super) type AskSet = Arc<Mutex<HashMap<SessionId, Vec<Ask>>>>;

/// What an ask shows the client: an ordinary tool-call permission, or
/// the plan-mode review of the plan document `exit_plan` presented.
pub(super) enum AskKind {
    /// Allow or refuse one tool call.
    Permission(ToolCallUpdate),
    /// Approve, revise or reject a plan. A revise answer denies the
    /// call and queues the user's text as the session's next prompt.
    Review {
        /// The `exit_plan` call awaiting a verdict.
        tool_call: ToolCallUpdate,
        /// The plan document under review.
        plan: String,
    },
}

/// Spawns the thread that asks the client on `client_id` about `kind`
/// and resolves `request_id` of `session` with the answer, recording
/// the ask under `asks` so a withdraw can stop it. Used by the bridge
/// for events and by an attach for asks already open.
pub(super) fn spawn_ask(
    peer: &Peer,
    commander: &Commander,
    asks: &AskSet,
    session: SessionId,
    client_id: String,
    request_id: RequestId,
    kind: AskKind,
) {
    let withdraw = CancelFlag::new();
    let flag = withdraw.clone();
    let peer = peer.clone();
    let commander = commander.clone();
    let thread = std::thread::spawn(move || {
        let (tool_call, review) = match kind {
            AskKind::Permission(tool_call) => (tool_call, None),
            AskKind::Review { tool_call, plan } => (tool_call, Some(plan)),
        };
        let title = tool_call.title.clone().unwrap_or_default();
        let (decision, revision) = if let Some(plan) = review {
            let decision = request_plan_review(&peer, &client_id, tool_call, &plan, &flag);
            match decision {
                PlanReviewDecision::Unanswered => return,
                PlanReviewDecision::Approve => (Decision::AllowOnce, None),
                PlanReviewDecision::Revise(text) => (Decision::Deny, Some(text)),
                PlanReviewDecision::Reject => (Decision::Deny, None),
            }
        } else {
            let decision =
                kage_acp::agent::request_permission(&peer, &client_id, tool_call, &title, &flag);
            let decision = match decision {
                PermissionDecision::Unanswered => return,
                PermissionDecision::Allow => Decision::AllowOnce,
                PermissionDecision::AllowSession => Decision::AllowSession,
                PermissionDecision::Deny(_) => Decision::Deny,
            };
            (decision, None)
        };
        commander.send(Command::to(
            session,
            CommandKind::ResolvePermission {
                request_id,
                decision,
            },
        ));
        if let Some(text) = revision.filter(|text| !text.is_empty()) {
            commander.send(Command::to(
                session,
                CommandKind::Prompt {
                    content: vec![Content::Text { text }],
                    delivery: Delivery::Queue,
                },
            ));
        }
    });
    lock(asks).entry(session).or_default().push(Ask {
        request_id,
        withdraw,
        thread,
    });
}

impl Bridge {
    pub(super) fn handle(&mut self, envelope: &Envelope) {
        self.apply_seeds();
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
                if let LoopEvent::MessageEnd {
                    stop_reason, usage, ..
                } = event
                {
                    self.stops.insert(session, *stop_reason);
                    let fill = usage.input + usage.output + usage.cache_read + usage.cache_write;
                    self.observe_turn_end(session, &client_id, fill);
                }
                if let LoopEvent::Compaction { kept, .. } = event {
                    self.observe_compaction(session, *kept);
                }
                if let LoopEvent::MessageAppended { message } = event {
                    self.echo(session, &client_id, message);
                }
                if let LoopEvent::ToolCallStart { id, name, .. } = event {
                    self.names
                        .entry(session)
                        .or_default()
                        .insert(id.to_string(), name.clone());
                }
                let seen = self.seen.entry(session).or_default();
                if let Some(update) = to_update(seen, event) {
                    self.send(session, &client_id, update);
                }
                if let LoopEvent::ToolCallEnd { id, output } = event
                    && self.call_name(session, id) == Some("todo_list")
                    && let Some(plan) = todo_plan(output)
                {
                    self.send(session, &client_id, plan);
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
                let review = (tool == EXIT_PLAN_TOOL)
                    .then(|| input["plan"].as_str().unwrap_or_default().to_owned());
                self.ask(session, *request_id, client_id, tool_call, review);
            }
            Event::Host(HostEvent::UsageUpdated { usage }) => {
                self.fills.insert(session, usage.context_used);
                let mut usage = *usage;
                if !self.subagents.load(Ordering::SeqCst) {
                    usage.cost += self.tree.usage_under(session).cost
                        + self.pruned_cost.get(&session).copied().unwrap_or_default();
                }
                if let Some(update) = usage_update(&usage) {
                    self.send(session, &client_id, update);
                }
            }
            Event::Host(HostEvent::StateChanged { state }) => {
                self.state_changed(session, &client_id, state);
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
            Event::Host(HostEvent::Notice { level, text, .. }) => {
                let update = notice_update(*level, text.clone());
                self.send(session, &client_id, update);
            }
            Event::Host(HostEvent::McpServers { servers })
                if !self.streaming.contains(&session) =>
            {
                self.mcp_servers(session, &client_id, servers);
            }
            Event::Host(HostEvent::RunEnded { outcome }) => {
                self.end_asks(session);
                self.seen.remove(&session);
                self.names.remove(&session);
                self.compacting.remove(&session);
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

    /// Refreshes a session's config options when the engine reported a
    /// change the client did not make, and reports a changed mode id as
    /// `current_mode_update`. The first reported state only seeds the
    /// mode: a client that opens or attaches learns the mode from the
    /// config options of the session response.
    fn state_changed(&mut self, session: SessionId, client_id: &str, state: &SessionState) {
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
                client_id,
                SessionUpdate::ConfigOptionUpdate(update),
            );
        }
        let mode = settings.mode_id();
        let last = self.modes.insert(session, mode.to_owned());
        if last.is_some_and(|last| last != mode) {
            self.send(
                session,
                client_id,
                SessionUpdate::CurrentModeUpdate(CurrentModeUpdate {
                    current_mode_id: mode.to_owned(),
                }),
            );
        }
    }

    /// The tool of an in-flight call of `session`.
    fn call_name(&self, session: SessionId, id: &ToolCallId) -> Option<&str> {
        self.names
            .get(&session)
            .and_then(|names| names.get(&id.to_string()).map(String::as_str))
    }

    /// Refreshes a session's MCP commands and statuses: the prompt
    /// commands as one `available_commands_update` when they changed,
    /// and one `_kage/mcp_status` per server whose status changed.
    fn mcp_servers(&mut self, session: SessionId, client_id: &str, servers: &[McpServerInfo]) {
        let commands = prompt_commands(servers);
        let last = self.commands.insert(session, commands.clone());
        if last.unwrap_or_default() != commands {
            let update = AvailableCommandsUpdate {
                available_commands: commands,
            };
            self.send(
                session,
                client_id,
                SessionUpdate::AvailableCommandsUpdate(update),
            );
        }
        let statuses: HashMap<String, McpServerStatus> = servers
            .iter()
            .map(|server| (server.name.clone(), server.status.clone()))
            .collect();
        let last = self.statuses.insert(session, statuses.clone());
        for (name, status) in &statuses {
            if last.as_ref().and_then(|last| last.get(name)) != Some(status) {
                let update = McpStatusUpdate {
                    name: name.clone(),
                    status: status.clone(),
                };
                self.send(session, client_id, SessionUpdate::McpStatus(update));
            }
        }
    }

    /// Remembers a compaction until the post-compaction usage arrives.
    fn observe_compaction(&mut self, session: SessionId, kept: usize) {
        let before = self.fills.get(&session).copied().unwrap_or_default();
        let kept = u64::try_from(kept).unwrap_or_default();
        self.compacting.insert(session, (kept, before));
    }

    /// Records the fill a finished turn reported and answers a pending
    /// compaction with it.
    fn observe_turn_end(&mut self, session: SessionId, client_id: &str, fill: u64) {
        self.fills.insert(session, fill);
        if let Some((kept, before)) = self.compacting.remove(&session) {
            let update = SessionUpdate::Compaction(CompactionUpdate {
                kept,
                before,
                after: fill,
            });
            self.send(session, client_id, update);
        }
    }

    /// Applies the seeds of finished attaches: the `agent` calls the
    /// attached session's tree was built from, the tool inputs the
    /// attach replay already showed, and the subagents this connection
    /// streams from now on.
    fn apply_seeds(&mut self) {
        let seeds = lock(&self.seeds).drain(..).collect::<Vec<_>>();
        for seed in seeds {
            for spawn in &seed.spawns {
                self.tree.apply(&spawn.envelope());
            }
            self.seen.insert(seed.session, seed.seen);
            self.streaming.extend(seed.running);
            self.paused.extend(seed.paused);
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

    /// Reports a streamed agent's usage and model to its parent's client
    /// session, after the agent's own usage update.
    fn report_agent_usage(&self, session: SessionId) {
        let Some(node) = self.tree.get(session) else {
            return;
        };
        let Some(parent_id) = self.client_of(node.parent) else {
            return;
        };
        let update = SubagentUpdate {
            subagent_session_id: session.to_string(),
            usage: agent_usage(node),
            model: agent_model(node),
            ..SubagentUpdate::default()
        };
        send_update(
            &self.peer,
            &parent_id,
            SessionUpdate::SubagentUpdate(update),
        );
    }

    /// Announces an agent as a subagent of its parent's client session,
    /// then shows its activity on its own session until it ends. A
    /// requeued child reports `paused` with its reason to the parent
    /// instead of ending, and `running` again when its next run starts.
    fn handle_subagent(&mut self, session: SessionId, event: &Event) {
        if let Event::Host(HostEvent::AgentSpawned {
            parent,
            tool_call_id,
            agent,
            description,
            swarm,
            ..
        }) = event
        {
            let Some(parent_id) = self.client_of(*parent) else {
                return;
            };
            self.streaming.insert(session);
            lock(&self.ids)
                .subagents
                .insert(session.to_string(), session);
            let update = SubagentUpdate {
                subagent_session_id: session.to_string(),
                name: Some(agent.clone()),
                task: Some(description.clone()),
                capabilities: Some(SubagentSessionCapabilities { cancel: true }),
                swarm: swarm.as_ref().and_then(|member| {
                    let batch = member.batch.as_ref()?;
                    Some(SubagentSwarm {
                        id: batch.0.clone(),
                        item: member.item.clone(),
                        index: member.index,
                        total: member.total,
                    })
                }),
                // Explicit, so a member announced again for a resume
                // leaves the state its last run ended in.
                state: Some(SubagentState::Running),
                reason: None,
                tool_call_id: Some(tool_call_id.to_string()),
                usage: None,
                model: None,
            };
            send_update(
                &self.peer,
                &parent_id,
                SessionUpdate::SubagentUpdate(update),
            );
        } else if let Event::Host(HostEvent::AgentPaused { reason }) = event
            && self.streaming.contains(&session)
        {
            let Some(parent) = self.tree.get(session).map(|node| node.parent) else {
                return;
            };
            let Some(parent_id) = self.client_of(parent) else {
                return;
            };
            self.paused.insert(session);
            let update = SubagentUpdate {
                subagent_session_id: session.to_string(),
                state: Some(SubagentState::Paused),
                reason: Some(reason.clone()),
                ..SubagentUpdate::default()
            };
            send_update(
                &self.peer,
                &parent_id,
                SessionUpdate::SubagentUpdate(update),
            );
        } else if let Event::Host(HostEvent::RunStarted) = event
            && self.paused.remove(&session)
        {
            if let Some(parent_id) = self
                .tree
                .get(session)
                .and_then(|node| self.client_of(node.parent))
            {
                let update = SubagentUpdate {
                    subagent_session_id: session.to_string(),
                    state: Some(SubagentState::Running),
                    ..SubagentUpdate::default()
                };
                send_update(
                    &self.peer,
                    &parent_id,
                    SessionUpdate::SubagentUpdate(update),
                );
            }
            self.handle_client(session, session.to_string(), event);
        } else if self.streaming.contains(&session) {
            self.handle_client(session, session.to_string(), event);
            if let Event::Host(HostEvent::UsageUpdated { .. }) = event {
                self.report_agent_usage(session);
            }
        }
    }

    /// The client's id for `session`: a client session's own id, or a
    /// live subagent's engine id.
    fn client_of(&self, session: SessionId) -> Option<String> {
        let client_id = lock(&self.ids).by_engine.get(&session).cloned();
        client_id.or_else(|| {
            self.streaming
                .contains(&session)
                .then(|| session.to_string())
        })
    }

    /// Finishes the ended run of `session` once none of its foreground
    /// subagents is live: a subagent sends its terminal state to its
    /// parent, and a client session answers its waiting prompt. The RFD
    /// wants every subagent to end before its parent does; a background
    /// agent outlives the run that started it, and its end reaches the
    /// parent later.
    fn settle(&mut self, session: SessionId) {
        let waits = self.streaming.iter().any(|s| {
            self.tree
                .get(*s)
                .is_some_and(|node| node.parent == session && !node.background)
        });
        if waits {
            return;
        }
        let Some(end) = self.ended.remove(&session) else {
            return;
        };
        // A requeued child stays paused until its next run starts.
        let requeued = self.paused.contains(&session)
            && matches!(
                &end.outcome,
                RunOutcome::Failed {
                    error: LoopError::RateLimited { .. }
                }
            );
        if requeued {
            return;
        }
        self.paused.remove(&session);
        if !self.streaming.remove(&session) {
            for waiter in lock(&self.waiters).remove(&session).unwrap_or_default() {
                let _ = waiter.send(end.clone());
            }
            self.prune_tree(session);
            return;
        }
        lock(&self.ids).subagents.remove(&session.to_string());
        let Some(node) = self.tree.get(session) else {
            return;
        };
        let (parent, usage, model) = (node.parent, agent_usage(node), agent_model(node));
        if let Some(parent_id) = self.client_of(parent) {
            let update = SubagentUpdate {
                subagent_session_id: session.to_string(),
                state: Some(match end.outcome {
                    RunOutcome::Completed => SubagentState::Completed,
                    RunOutcome::Cancelled => SubagentState::Cancelled,
                    RunOutcome::Failed { .. } => SubagentState::Failed,
                }),
                usage,
                model,
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

    /// Forgets the finished agents under a client session whose run
    /// just settled, keeping their cost. The nodes, each carrying its
    /// agent's latest tool input, would otherwise outlive the agents they
    /// describe. A later run re-announces its agents with fresh
    /// `AgentSpawned` envelopes. Background agents still at work stay.
    fn prune_tree(&mut self, session: SessionId) {
        let roots: Vec<SessionId> = self
            .tree
            .under(session)
            .into_iter()
            .filter(|(depth, node)| *depth == 1 && !(node.background && is_live(node)))
            .map(|(_, node)| node.session)
            .collect();
        for root in roots {
            let cost = self.tree.get(root).map_or(0.0, |node| node.usage.cost)
                + self.tree.usage_under(root).cost;
            *self.pruned_cost.entry(session).or_default() += cost;
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
                self.ask(session, *request_id, client_id, tool_call, None);
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
            Event::Host(HostEvent::AgentPaused { reason }) => {
                progress(format!("paused: {reason}"));
            }
            _ => {}
        }
    }

    /// Asks the client on `client_id` and resolves `request_id` of
    /// `session` with the answer. A review `plan` document raises the
    /// plan-mode review instead of the ordinary allow-and-reject ask.
    /// The ask is withdrawn when that session's run ends first, when
    /// another client's answer resolves it, or when the connection
    /// detaches; a withdrawn ask sends no decision.
    fn ask(
        &mut self,
        session: SessionId,
        request_id: RequestId,
        client_id: String,
        tool_call: ToolCallUpdate,
        review: Option<String>,
    ) {
        let kind = match review {
            Some(plan) => AskKind::Review { tool_call, plan },
            None => AskKind::Permission(tool_call),
        };
        spawn_ask(
            &self.peer,
            &self.commander,
            &self.asks,
            session,
            client_id,
            request_id,
            kind,
        );
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
        let owner = lock(&self.live).owner_of(session);
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
    SessionUpdate::UserMessageChunk(kage_acp::acp::MessageChunk {
        content,
        meta: None,
    })
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
pub(super) fn permission_call(
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
pub(super) fn top_agent(tree: &AgentTree, session: SessionId) -> Option<&AgentNode> {
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
/// changed, and the later steps always do. `seen` holds the input and
/// the swarm meta last sent for each call, so the meta goes out only
/// when it changes.
pub(super) fn to_update(
    seen: &mut HashMap<String, SeenCall>,
    event: &LoopEvent,
) -> Option<SessionUpdate> {
    match event {
        LoopEvent::TextDelta { delta, .. } => {
            Some(SessionUpdate::AgentMessageChunk(MessageChunk {
                content: ContentBlock::text(delta.clone()),
                meta: None,
            }))
        }
        LoopEvent::ThinkingDelta { delta, .. } => {
            Some(SessionUpdate::AgentThoughtChunk(MessageChunk {
                content: ContentBlock::text(delta.clone()),
                meta: None,
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
        } => streamed_call(seen, id, name, input_partial),
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
            let mut content = vec![text_content(output.text.clone())];
            if !output.is_error
                && let Some(call) = seen.get(&id.to_string())
            {
                content.extend(diff_contents(&call.name, &call.input));
            }
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: id.to_string(),
                status: Some(if output.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                }),
                content,
                raw_output: output.structured.clone(),
                ..ToolCallUpdate::default()
            }))
        }
        LoopEvent::TurnStarted { .. } => Some(SessionUpdate::Turn(TurnUpdate {
            phase: TurnPhase::Start,
            reason: None,
            at: None,
            took_ms: None,
        })),
        LoopEvent::TurnEnded { had_tool_calls, .. } => Some(SessionUpdate::Turn(TurnUpdate {
            phase: TurnPhase::End,
            reason: Some(turn_reason(*had_tool_calls)),
            at: None,
            took_ms: None,
        })),
        LoopEvent::ProviderRetry {
            attempt,
            max_attempts,
            wait_secs,
            error,
            ..
        } => Some(notice_update(
            NoticeLevel::Info,
            format!("provider error ({error}); retrying {attempt}/{max_attempts} in {wait_secs}s"),
        )),
        LoopEvent::Error { kind } => Some(notice_update(NoticeLevel::Error, kind.to_string())),
        _ => None,
    }
}

/// The update a streamed tool call input sends: `tool_call` on the
/// first sighting, then a `tool_call_update` when the input changed,
/// carrying the swarm meta only when it differs from the one last sent.
fn streamed_call(
    seen: &mut HashMap<String, SeenCall>,
    id: &ToolCallId,
    name: &str,
    input_partial: &serde_json::Value,
) -> Option<SessionUpdate> {
    let key = id.to_string();
    let last = seen.get(&key);
    if last.is_some_and(|last| last.input == *input_partial) {
        return None;
    }
    let sent = last.and_then(|last| last.meta.clone());
    let meta = swarm_meta(name, input_partial).filter(|meta| sent.as_ref() != Some(meta));
    let first = seen
        .insert(
            key,
            SeenCall {
                name: name.to_owned(),
                input: input_partial.clone(),
                meta: meta.clone().or(sent),
            },
        )
        .is_none();
    Some(if first {
        SessionUpdate::ToolCall(ToolCall {
            tool_call_id: id.to_string(),
            title: tool_title(name),
            kind: tool_kind(name),
            status: ToolCallStatus::Pending,
            content: Vec::new(),
            raw_input: Some(input_partial.clone()),
            meta,
        })
    } else {
        SessionUpdate::ToolCallUpdate(ToolCallUpdate {
            tool_call_id: id.to_string(),
            raw_input: Some(input_partial.clone()),
            meta,
            ..ToolCallUpdate::default()
        })
    })
}

/// What a tool call announced so far: the tool it calls, its input as
/// last streamed and the swarm meta last sent for it.
#[derive(Debug, Clone)]
pub(super) struct SeenCall {
    name: String,
    input: serde_json::Value,
    meta: Option<ToolCallMeta>,
}

/// The `diff` content a finished `edit` or `write` call carries, read
/// from its own input: one diff per text replacement, and the whole new
/// file for a write. A line-range change names no old text, so it
/// carries none rather than a diff that would read as a new file.
fn diff_contents(name: &str, input: &serde_json::Value) -> Vec<ToolCallContent> {
    let Some(path) = input["path"].as_str() else {
        return Vec::new();
    };
    match name {
        "write" => input["content"]
            .as_str()
            .map(|content| {
                ToolCallContent::Diff(DiffContent {
                    path: path.to_owned(),
                    old_text: None,
                    new_text: content.to_owned(),
                })
            })
            .into_iter()
            .collect(),
        "edit" => {
            let changes = match input["changes"].as_array() {
                Some(changes) => changes.iter().collect(),
                None => vec![input],
            };
            changes
                .into_iter()
                .filter_map(|change| {
                    let old = change["old_str"].as_str()?;
                    let new = change["new_str"].as_str()?;
                    Some(ToolCallContent::Diff(DiffContent {
                        path: path.to_owned(),
                        old_text: Some(old.to_owned()),
                        new_text: new.to_owned(),
                    }))
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// The turn-end reason for whether the model requested tool calls.
fn turn_reason(had_tool_calls: bool) -> TurnReason {
    if had_tool_calls {
        TurnReason::ToolCalls
    } else {
        TurnReason::NoToolCalls
    }
}

/// The `_meta.kage.swarm` of a `swarm` call whose input names its
/// members: one entry per item plus one per resumed child. `None` for
/// any other tool, and for a `swarm` input that has not streamed
/// whole yet, so a later update carries the meta instead.
fn swarm_meta(name: &str, input: &serde_json::Value) -> Option<ToolCallMeta> {
    if name != "swarm" {
        return None;
    }
    let mut members = Vec::new();
    if let Some(items) = input["items"].as_array() {
        members.extend(
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::to_owned),
        );
    }
    if let Some(resume) = input["resume"].as_object() {
        members.extend(resume.keys().cloned());
    }
    if members.is_empty() {
        return None;
    }
    Some(ToolCallMeta {
        kage: KageMeta {
            swarm: Some(SwarmMeta {
                members,
                template: input["prompt_template"].as_str().map(str::to_owned),
            }),
            ..KageMeta::default()
        },
    })
}

/// The `plan` update a completed `todo_list` write carries: one entry
/// per todo, with the plan 019 `_meta.kage` fields when supplied. The
/// write is what returns the list as structured output, so a read-only
/// call, which has none, sends no plan.
fn todo_plan(output: &ToolOutput) -> Option<SessionUpdate> {
    let todos = output.structured.as_ref()?.as_array()?;
    Some(SessionUpdate::Plan(Plan {
        entries: todos.iter().map(todo_plan_entry).collect(),
    }))
}

/// One ACP plan entry for one todo: the title as content, the status
/// mapped (`done` becomes `completed`), and `id`, `owner` and
/// `blockedBy` under `_meta.kage` when the model supplied them.
fn todo_plan_entry(todo: &serde_json::Value) -> serde_json::Value {
    let status = match todo["status"].as_str() {
        Some("in_progress") => "in_progress",
        Some("done") => "completed",
        _ => "pending",
    };
    let mut entry = serde_json::json!({
        "content": todo["title"].as_str().unwrap_or_default(),
        "priority": "medium",
        "status": status,
    });
    let mut kage = serde_json::Map::new();
    for field in ["id", "owner", "blockedBy"] {
        if let Some(value) = todo.get(field) {
            kage.insert(field.to_owned(), value.clone());
        }
    }
    if !kage.is_empty() {
        entry["_meta"] = serde_json::json!({ "kage": kage });
    }
    entry
}

/// The `_kage/notice` carrying an engine notice, with its tone from
/// the notice level.
pub(super) fn notice_update(level: NoticeLevel, text: String) -> SessionUpdate {
    SessionUpdate::Notice(NoticeUpdate {
        tone: match level {
            NoticeLevel::Info => NoticeTone::Info,
            NoticeLevel::Success => NoticeTone::Success,
            NoticeLevel::Warning => NoticeTone::Warn,
            NoticeLevel::Error => NoticeTone::Error,
        },
        text,
    })
}

/// Whether the agent of `node` is queued or running.
fn is_live(node: &kage_core::protocol::AgentNode) -> bool {
    matches!(
        node.state,
        kage_core::protocol::AgentState::Queued | kage_core::protocol::AgentState::Running
    )
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
        meta: None,
    })
}

/// The title a client shows for tool `name`: `server.tool` for an MCP
/// tool, as the TUI shows it, else the name.
pub(super) fn tool_title(name: &str) -> String {
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
        "grep" | "find" | "web_search" => ToolKind::Search,
        "write" | "edit" => ToolKind::Edit,
        "shell" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// What a subagent update reports of `node`'s use: its token totals,
/// cost and the time it ran so far. `None` before it used anything.
pub(super) fn agent_usage(node: &AgentNode) -> Option<SubagentUsage> {
    let usage = &node.usage;
    let took = node.elapsed();
    if usage.total == TokenUsage::default() && took.is_none() {
        return None;
    }
    Some(SubagentUsage {
        input: usage.total.input,
        output: usage.total.output,
        cache_read: usage.total.cache_read,
        cache_write: usage.total.cache_write,
        cost: usage.cost,
        run_ms: took.map(|took| u64::try_from(took.as_millis()).unwrap_or(u64::MAX)),
    })
}

/// The model a subagent update reports of `node`, once known.
pub(super) fn agent_model(node: &AgentNode) -> Option<String> {
    (!node.model.is_empty()).then(|| node.model.clone())
}
