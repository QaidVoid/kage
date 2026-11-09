//! Live engine state for clients attaching to a hosted session.
//!
//! One engine subscriber per host folds every envelope into [`Live`]:
//! the message still streaming, the tool calls it started, the open
//! permission asks, the agent tree, prompt owners, working flags, and
//! how many connections hold each session. A connection attaching to a
//! hosted session replays its file and then a [`Live`] snapshot inside
//! [`Engine::hold_events`](crate::engine::Engine::hold_events), so
//! nothing reaches it twice. Live also sends [`CommandKind::Close`]
//! once a session has no attachment, no run and no open ask, so idle
//! sessions stop holding MCP servers and plugin runtimes.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kage_acp::acp::{
    SessionConfigOption, SessionInfoUpdate, SessionUpdate, SubagentSessionCapabilities,
    SubagentState, SubagentSwarm, SubagentUpdate, SubagentUsage, ToolCallUpdate,
};
use kage_acp::agent::{PromptContext, send_update};
use kage_core::protocol::{
    ASK_USER_QUESTION_TOOL, AgentNode, AgentState, AgentTree, Command, CommandKind, Envelope,
    Event, HostEvent, NoticeLevel, RequestId, RunOutcome, SwarmMember,
};
use kage_core::sync::lock;
use kage_core::{LoopError, LoopEvent, MessageId, SessionId, ToolCallId};
use kage_jsonrpc::RpcError;

use super::CliAcpAgent;
use super::bridge::{
    HeldUpdates, ask_kind, permission_call, question_input, spawn_ask, to_update, tool_kind,
    tool_title, top_agent,
};
use super::options::{Settings, Shown, config_options};
use crate::engine::Commander;

/// The notice the engine publishes when a close lands on a session
/// with a run or shell still in flight (`Dispatcher::close`).
const CLOSE_REFUSED: &str = "close: wait for the current run to finish or cancel it";

/// What a connection attaching to a hosted session is given, copied
/// under the bus lock so no envelope moves during the copy.
pub(super) struct Snapshot {
    /// The subagents under the attached session, parents first.
    pub(super) subagents: Vec<SubagentSeed>,
    /// The session's title, when one was set since the host started.
    pub(super) title: Option<String>,
    /// Loop events replaying the message in flight, in stream order.
    pub(super) flight: Vec<LoopEvent>,
    /// The message the in-flight events belong to.
    pub(super) flight_message: Option<MessageId>,
    /// Open permission requests under the attached session.
    pub(super) asks: Vec<AskSeed>,
    /// The `agent` calls the attached session's tree was built from.
    pub(super) spawns: Vec<Spawn>,
}

/// One subagent an attaching client hears of.
pub(super) struct SubagentSeed {
    pub(super) session: SessionId,
    /// Session whose `agent` call started this one.
    pub(super) parent: SessionId,
    pub(super) agent: String,
    pub(super) description: String,
    /// The parent's call that started the agent.
    pub(super) tool_call_id: String,
    /// The state to announce: running, paused, or where it ended.
    pub(super) state: SubagentState,
    /// Why a paused agent paused.
    pub(super) reason: Option<String>,
    /// Swarm batch membership, when a `swarm` call started it.
    pub(super) swarm: Option<SwarmMember>,
    /// What it used so far.
    pub(super) usage: Option<SubagentUsage>,
    /// The model it runs, once known.
    pub(super) model: Option<String>,
    /// Whether it runs in the background.
    pub(super) background: bool,
}

impl SubagentSeed {
    /// Whether the agent still runs or will again, so the bridge keeps
    /// streaming it.
    pub(super) fn live(&self) -> bool {
        matches!(self.state, SubagentState::Running | SubagentState::Paused)
    }
}

/// An open permission request with the call it reports under.
pub(super) struct AskSeed {
    pub(super) ask: OpenAsk,
    /// The root `agent` call the ask reports under, when it came from
    /// an agent instead of the attached session itself.
    pub(super) top: Option<(ToolCallId, String)>,
}

/// An open ask a client that left is to decline: the session that
/// raised it, the engine request, and whether declining means
/// answering questions with no answers rather than denying a
/// permission.
pub(super) struct AskRef {
    pub(super) session: SessionId,
    pub(super) request_id: RequestId,
    pub(super) question: bool,
}

/// A permission request still open in the engine.
#[derive(Clone)]
pub(super) struct OpenAsk {
    pub(super) request_id: RequestId,
    /// Session that asked: the attached session or an agent under it.
    pub(super) session: SessionId,
    pub(super) tool_call_id: Option<ToolCallId>,
    pub(super) tool: String,
    pub(super) input: serde_json::Value,
}

/// What a bridge seeded at attach applies before its next envelope.
pub(super) struct Seed {
    /// The attached session.
    pub(super) session: SessionId,
    /// The `agent` calls the session's tree was built from, in spawn
    /// order. An agent tree is not cloneable, so the bridge folds a
    /// fresh one from these.
    pub(super) spawns: Vec<Spawn>,
    /// Tool inputs the attach replay already showed, by call id.
    pub(super) seen: HashMap<String, super::bridge::SeenCall>,
    /// Running subagents the bridge streams and settles.
    pub(super) running: HashSet<SessionId>,
    /// The running ones paused for a rate limit, which report running
    /// again when their next run starts.
    pub(super) paused: HashSet<SessionId>,
}

/// One `agent` call Live recorded, enough to rebuild its tree node.
#[derive(Clone)]
pub(super) struct Spawn {
    /// The agent's own session.
    session: SessionId,
    /// Session whose `agent` call started this agent.
    parent: SessionId,
    /// The parent's `agent` call that started this agent.
    tool_call_id: ToolCallId,
    agent: String,
    description: String,
    swarm: Option<SwarmMember>,
}

impl Spawn {
    /// The envelope the tree folds to insert this agent.
    pub(super) fn envelope(&self) -> Envelope {
        Envelope {
            session: self.session,
            seq: 0,
            event: HostEvent::AgentSpawned {
                parent: self.parent,
                tool_call_id: self.tool_call_id.clone(),
                agent: self.agent.clone(),
                description: self.description.clone(),
                swarm: self.swarm.clone(),
                background: false,
            }
            .into(),
        }
    }
}

/// The message a session is still streaming, none of which its file
/// holds yet. Bounded by one message per session: cleared when the
/// message lands in the file and when the run ends.
#[derive(Default)]
struct Flight {
    message: Option<MessageId>,
    text: String,
    thinking: String,
    tools: Vec<LiveTool>,
}

impl Flight {
    /// Records a tool call of the message, or refreshes the input of
    /// the one already recorded under `id`.
    fn tool(&mut self, id: &ToolCallId, name: &str, input: &serde_json::Value) {
        match self.tools.iter_mut().find(|tool| &tool.id == id) {
            Some(tool) => {
                name.clone_into(&mut tool.name);
                tool.input = input.clone();
            }
            None => self.tools.push(LiveTool {
                id: id.clone(),
                name: name.to_owned(),
                input: input.clone(),
                running: false,
            }),
        }
    }
}

/// A tool call of the message in flight, with its latest input.
struct LiveTool {
    id: ToolCallId,
    name: String,
    input: serde_json::Value,
    running: bool,
}

/// Live engine state, folded by one subscriber per host.
pub(super) struct Live {
    commander: Commander,
    flight: HashMap<SessionId, Flight>,
    asks: HashMap<RequestId, OpenAsk>,
    titles: HashMap<SessionId, String>,
    tree: AgentTree,
    /// The `agent` calls behind the tree, per root client session, so
    /// an attaching bridge folds a tree of its own.
    spawns: HashMap<SessionId, Vec<Spawn>>,
    /// Agent sessions announced and not ended yet.
    running: HashSet<SessionId>,
    /// Agents waiting out a rate limit before their next run, with the
    /// reason they gave.
    paused: HashMap<SessionId, String>,
    owners: HashMap<SessionId, u64>,
    /// One admission lifetime per announcement, expired at its true terminal end.
    steering_lifetimes: HashMap<SessionId, Arc<AtomicBool>>,
    working: HashSet<SessionId>,
    /// Connections holding each session open.
    attached: HashMap<SessionId, usize>,
    /// Sessions a close was sent for and not refused.
    closing: HashSet<SessionId>,
    /// Sessions no client holds, with the moment their current
    /// ask-hold window started: an ask open on one holds the host
    /// only up to the ask-hold cap from that moment, then the host
    /// declines it. A new ask on a clientless session starts its
    /// own window.
    orphaned: HashMap<SessionId, Instant>,
}

impl Live {
    /// Whether any session is working, any agent is running or waiting
    /// out a rate limit, or any ask is open.
    #[cfg_attr(not(unix), allow(dead_code, reason = "the shared host is unix only"))]
    pub(super) fn busy(&self) -> bool {
        !self.working.is_empty()
            || !self.running.is_empty()
            || !self.paused.is_empty()
            || !self.asks.is_empty()
    }

    /// The sessions whose ask-hold window started at least `cap` ago
    /// and that still hold an open ask with no client: the host
    /// declines those, so an ask nobody can answer stops holding the
    /// session and the host.
    pub(super) fn expired_ask_roots(&self, cap: Duration) -> Vec<SessionId> {
        let now = Instant::now();
        self.orphaned
            .iter()
            .filter(|(root, since)| {
                now.duration_since(**since) >= cap
                    && !self.attached.contains_key(root)
                    && self
                        .asks
                        .values()
                        .any(|ask| self.tree.root_of(ask.session) == **root)
            })
            .map(|(root, _)| *root)
            .collect()
    }

    /// Hands out the open asks of the sessions [`Self::expired_ask_roots`]
    /// names and forgets those sessions' windows, all under the one
    /// borrow. A client attaching meanwhile, or a second caller, cannot
    /// see a session this call took: each ask is handed out once.
    pub(super) fn take_expired_asks(&mut self, cap: Duration) -> Vec<AskRef> {
        let mut taken = Vec::new();
        for root in self.expired_ask_roots(cap) {
            taken.extend(self.asks_under(root));
            self.orphaned.remove(&root);
        }
        taken
    }

    /// The moment `root`'s current ask-hold window started.
    #[cfg(test)]
    pub(super) fn orphan_stamp(&self, root: SessionId) -> Option<Instant> {
        self.orphaned.get(&root).copied()
    }

    /// Whether `id` is open, closing, running or part of a live agent tree
    /// on the host, so its file must stay.
    pub(super) fn in_use(&self, id: SessionId) -> bool {
        self.attached.contains_key(&id)
            || self.closing.contains(&id)
            || self.working.contains(&id)
            || self.running.contains(&id)
            || self.tree.get(id).is_some()
    }

    pub(super) fn new(commander: Commander) -> Self {
        Self {
            commander,
            flight: HashMap::new(),
            asks: HashMap::new(),
            titles: HashMap::new(),
            tree: AgentTree::default(),
            spawns: HashMap::new(),
            running: HashSet::new(),
            paused: HashMap::new(),
            owners: HashMap::new(),
            steering_lifetimes: HashMap::new(),
            working: HashSet::new(),
            attached: HashMap::new(),
            closing: HashSet::new(),
            orphaned: HashMap::new(),
        }
    }

    /// Folds one envelope in. Runs on the publishing thread under the
    /// bus lock.
    pub(super) fn observe(&mut self, envelope: &Envelope) {
        let is_agent = self.tree.apply(envelope);
        match &envelope.event {
            Event::Loop(event) => self.observe_loop(envelope.session, event),
            Event::Host(event) => {
                self.observe_host(envelope.session, event, is_agent);
            }
        }
    }

    fn observe_loop(&mut self, session: SessionId, event: &LoopEvent) {
        match event {
            LoopEvent::MessageStart { id } => {
                self.flight.insert(
                    session,
                    Flight {
                        message: Some(*id),
                        ..Flight::default()
                    },
                );
            }
            LoopEvent::TextDelta { delta, .. } => self.flight(session).text.push_str(delta),
            LoopEvent::ThinkingDelta { delta, .. } => self.flight(session).thinking.push_str(delta),
            LoopEvent::ToolCallStart {
                id,
                name,
                input_partial,
            }
            | LoopEvent::ToolCallArgsDelta {
                id,
                name,
                input_partial,
            } => {
                self.flight(session).tool(id, name, input_partial);
            }
            LoopEvent::ToolExecutionStart { id } => {
                if let Some(tool) = self
                    .flight
                    .get_mut(&session)
                    .and_then(|flight| flight.tools.iter_mut().find(|tool| &tool.id == id))
                {
                    tool.running = true;
                }
            }
            LoopEvent::MessageAppended { .. } => {
                self.flight.remove(&session);
            }
            _ => {}
        }
    }

    fn flight(&mut self, session: SessionId) -> &mut Flight {
        self.flight.entry(session).or_default()
    }

    /// Holds the permission request or the questions `event` raises as
    /// an open ask of `session`, for a client that attaches before the
    /// answer.
    fn track_ask(&mut self, session: SessionId, event: &HostEvent) {
        let ask = match event {
            HostEvent::PermissionRequested {
                request_id,
                tool_call_id,
                tool,
                subject: _,
                input,
            } => OpenAsk {
                session,
                request_id: *request_id,
                tool_call_id: tool_call_id.clone(),
                tool: tool.clone(),
                input: input.clone(),
            },
            HostEvent::QuestionAsked {
                request_id,
                tool_call_id,
                questions,
            } => OpenAsk {
                session,
                request_id: *request_id,
                tool_call_id: tool_call_id.clone(),
                tool: ASK_USER_QUESTION_TOOL.to_owned(),
                input: question_input(questions),
            },
            _ => return,
        };
        let root = self.tree.root_of(session);
        if !self.attached.contains_key(&root) {
            // Nobody can answer yet, so this ask holds the session
            // from now for a full window, however long the session sat
            // orphaned before it.
            self.orphaned.insert(root, Instant::now());
        }
        self.asks.insert(ask.request_id, ask);
    }

    fn observe_host(&mut self, session: SessionId, event: &HostEvent, is_agent: bool) {
        match event {
            HostEvent::PermissionRequested { .. } | HostEvent::QuestionAsked { .. } => {
                self.track_ask(session, event);
            }
            HostEvent::PermissionResolved { request_id }
            | HostEvent::QuestionClosed { request_id } => {
                self.asks.remove(request_id);
                if !is_agent {
                    self.maybe_close(session);
                }
            }
            HostEvent::AgentPaused { reason } => {
                self.paused.insert(session, reason.clone());
            }
            HostEvent::RunStarted => {
                self.paused.remove(&session);
            }
            HostEvent::RunEnded { outcome } => {
                let requeued = matches!(
                    outcome,
                    RunOutcome::Failed {
                        error: LoopError::RateLimited { .. }
                    }
                );
                if !requeued {
                    self.paused.remove(&session);
                }
                self.flight.remove(&session);
                if !(is_agent && requeued && self.paused.contains_key(&session)) {
                    self.owners.remove(&session);
                    if let Some(expired) = self.steering_lifetimes.remove(&session) {
                        expired.store(true, Ordering::SeqCst);
                    }
                }
                self.running.remove(&session);
                if !is_agent {
                    self.prune_ended_agents(session);
                    self.maybe_close(session);
                } else if !requeued {
                    // A background agent the parent run left behind has
                    // ended: drop its node so a later attach reads its
                    // report from the file, then close the parent when
                    // nothing else keeps it open.
                    let root = self.tree.root_of(session);
                    self.tree.remove_subtree(session);
                    self.retain_spawns(root);
                    if root != session {
                        self.maybe_close(root);
                    }
                }
            }
            HostEvent::AgentSpawned { .. } => {
                if let Some(expired) = self
                    .steering_lifetimes
                    .insert(session, Arc::new(AtomicBool::new(false)))
                {
                    expired.store(true, Ordering::SeqCst);
                }
                self.running.insert(session);
                if let HostEvent::AgentSpawned { parent, .. } = event {
                    let root = self.tree.root_of(*parent);
                    let spawn = self.tree.get(session).map(|node| Spawn {
                        session,
                        parent: node.parent,
                        tool_call_id: node.tool_call_id.clone(),
                        agent: node.agent.clone(),
                        description: node.description.clone(),
                        swarm: node.swarm.clone(),
                    });
                    if let Some(spawn) = spawn {
                        self.spawns.entry(root).or_default().push(spawn);
                    }
                }
            }
            HostEvent::StateChanged { state } => {
                if state.working {
                    self.working.insert(session);
                } else {
                    let is_agent = self.tree.get(session).is_some();
                    self.working.remove(&session);
                    if !is_agent {
                        self.maybe_close(session);
                    }
                }
            }
            HostEvent::TitleChanged { title } => {
                self.titles.insert(session, title.clone());
            }
            HostEvent::Notice {
                level: NoticeLevel::Warning,
                text,
                ..
            } if text == CLOSE_REFUSED => {
                self.closing.remove(&session);
            }
            _ => {}
        }
    }

    /// Sends `Close` for an idle session no connection holds, so its
    /// MCP servers and plugin runtimes go away. An agent still at work
    /// under the session keeps it open; the agent's own end retries
    /// the close. The engine keeps a session it cannot close yet and
    /// says so.
    fn maybe_close(&mut self, session: SessionId) {
        if self.attached.get(&session).is_some_and(|count| *count > 0) {
            return;
        }
        // No client is left. Whatever keeps the session open now keeps
        // it only until the ask-hold cap: the moment is what the cap
        // counts from, and an ask raised later starts its own window.
        // A stamp on a session kept by work alone is harmless, since
        // only an open ask expires one.
        self.orphaned.entry(session).or_insert_with(Instant::now);
        if self.working.contains(&session) {
            return;
        }
        if self
            .asks
            .values()
            .any(|ask| self.tree.root_of(ask.session) == session)
        {
            return;
        }
        if self.agents_live_under(session) {
            return;
        }
        self.orphaned.remove(&session);
        self.closing.insert(session);
        self.commander
            .send(Command::to(session, CommandKind::Close));
    }

    /// Whether an agent under `root` still runs or will again, so the
    /// session stays open until its work is done.
    fn agents_live_under(&self, root: SessionId) -> bool {
        self.tree.under(root).iter().any(|(_, node)| {
            matches!(node.state, AgentState::Queued | AgentState::Running)
                || self.running.contains(&node.session)
                || self.paused.contains_key(&node.session)
        })
    }

    /// Whether the end of a parent run leaves `node` in the tree: a
    /// background agent still queued or running, or waiting out a rate
    /// limit.
    fn agent_kept(&self, node: &AgentNode) -> bool {
        node.background
            && (matches!(node.state, AgentState::Queued | AgentState::Running)
                || self.running.contains(&node.session)
                || self.paused.contains_key(&node.session))
    }

    /// Drops the tree nodes of the depth-1 children of `session` whose
    /// run ended, keeping the background agents still at work, and
    /// drops the spawn records of every agent no longer in the tree.
    fn prune_ended_agents(&mut self, session: SessionId) {
        let roots: Vec<SessionId> = self
            .tree
            .under(session)
            .into_iter()
            .filter(|(depth, node)| *depth == 1 && !self.agent_kept(node))
            .map(|(_, node)| node.session)
            .collect();
        for root in roots {
            self.tree.remove_subtree(root);
        }
        self.retain_spawns(session);
    }

    /// Drops the spawn records of agents no longer in the tree.
    fn retain_spawns(&mut self, root: SessionId) {
        if let Some(spawns) = self.spawns.get_mut(&root) {
            spawns.retain(|spawn| self.tree.get(spawn.session).is_some());
            if spawns.is_empty() {
                self.spawns.remove(&root);
            }
        }
    }

    /// The message `session` is streaming, with the loop events that
    /// replay it in stream order.
    fn flight_events(&self, session: SessionId) -> (Option<MessageId>, Vec<LoopEvent>) {
        let flight = self.flight.get(&session);
        let message = flight.and_then(|flight| flight.message);
        let mut events = Vec::new();
        if let Some(flight) = flight {
            let id = message.unwrap_or_default();
            if !flight.thinking.is_empty() {
                events.push(LoopEvent::ThinkingDelta {
                    id,
                    delta: flight.thinking.clone(),
                });
            }
            if !flight.text.is_empty() {
                events.push(LoopEvent::TextDelta {
                    id,
                    delta: flight.text.clone(),
                });
            }
            for tool in &flight.tools {
                events.push(LoopEvent::ToolCallStart {
                    id: tool.id.clone(),
                    name: tool.name.clone(),
                    input_partial: tool.input.clone(),
                });
                if tool.running {
                    events.push(LoopEvent::ToolExecutionStart {
                        id: tool.id.clone(),
                    });
                }
            }
        }
        (message, events)
    }

    /// What a connection attaching to `attached` needs.
    pub(super) fn snapshot(&self, attached: SessionId) -> Snapshot {
        let (message, events) = self.flight_events(attached);
        let mut asks: Vec<AskSeed> = self
            .asks
            .values()
            .filter(|ask| self.tree.root_of(ask.session) == attached)
            .map(|ask| AskSeed {
                ask: ask.clone(),
                top: self.tree.get(ask.session).and_then(|node| {
                    let top = top_agent(&self.tree, ask.session)?;
                    Some((top.tool_call_id.clone(), node.agent.clone()))
                }),
            })
            .collect();
        asks.sort_by_key(|seed| seed.ask.request_id.0);
        let subagents = self
            .tree
            .under(attached)
            .into_iter()
            .map(|(_, node)| {
                let reason = self.paused.get(&node.session).cloned();
                let state = if reason.is_some() {
                    SubagentState::Paused
                } else if self.running.contains(&node.session) {
                    SubagentState::Running
                } else {
                    match node.state {
                        AgentState::Done => SubagentState::Completed,
                        AgentState::Failed => SubagentState::Failed,
                        AgentState::Cancelled => SubagentState::Cancelled,
                        AgentState::Queued | AgentState::Running => SubagentState::Running,
                    }
                };
                SubagentSeed {
                    session: node.session,
                    parent: node.parent,
                    agent: node.agent.clone(),
                    description: node.description.clone(),
                    tool_call_id: node.tool_call_id.to_string(),
                    state,
                    reason,
                    swarm: node.swarm.clone(),
                    usage: super::bridge::agent_usage(node),
                    model: super::bridge::agent_model(node),
                    background: node.background,
                }
            })
            .collect();
        Snapshot {
            title: self.titles.get(&attached).cloned(),
            flight: events,
            flight_message: message,
            asks,
            subagents,
            spawns: self.spawns.get(&attached).cloned().unwrap_or_default(),
        }
    }

    /// Records that a connection opened `id`. A fresh open follows a
    /// close of the same session file, so stale state goes first.
    pub(super) fn attach(&mut self, id: SessionId, fresh: bool) {
        if fresh {
            self.flight.remove(&id);
            self.titles.remove(&id);
            self.owners.remove(&id);
            self.working.remove(&id);
            let roots: Vec<SessionId> = self
                .tree
                .under(id)
                .into_iter()
                .filter_map(|(depth, node)| (depth == 1).then_some(node.session))
                .collect();
            for root in roots {
                self.tree.remove_subtree(root);
            }
            self.spawns.remove(&id);
        }
        *self.attached.entry(id).or_default() += 1;
        self.closing.remove(&id);
        self.orphaned.remove(&id);
    }

    /// Records that a connection released `id`, closing the session
    /// when it was the last attachment and the session is idle.
    pub(super) fn release(&mut self, id: SessionId) {
        let Some(count) = self.attached.get_mut(&id) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.attached.remove(&id);
            self.maybe_close(id);
        }
    }

    /// Whether an announced child can still receive live steering.
    pub(super) fn steerable_subagent(&self, id: SessionId) -> bool {
        self.tree
            .get(id)
            .is_some_and(|node| matches!(node.state, AgentState::Queued | AgentState::Running))
            || self.paused.contains_key(&id)
    }

    /// The admission token for the child's current live announcement.
    pub(super) fn steering_lifetime(&self, id: SessionId) -> Option<Arc<AtomicBool>> {
        self.steerable_subagent(id)
            .then(|| self.steering_lifetimes.get(&id).cloned())
            .flatten()
    }

    /// Releases a refused admission without changing a newer announcement's owner.
    pub(super) fn release_child_prompt(
        &mut self,
        id: SessionId,
        connection: u64,
        expired: &Arc<AtomicBool>,
    ) {
        if self.owners.get(&id) == Some(&connection)
            && self
                .steering_lifetimes
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, expired))
        {
            self.owners.remove(&id);
        }
    }

    /// Records `connection` as the owner of `id`'s running prompt, or
    /// returns false when another connection owns it or an ancestor.
    pub(super) fn claim_prompt(&mut self, id: SessionId, connection: u64) -> bool {
        let mut ancestor = id;
        loop {
            if self
                .owners
                .get(&ancestor)
                .is_some_and(|owner| *owner != connection)
            {
                return false;
            }
            let Some(node) = self.tree.get(ancestor) else {
                break;
            };
            ancestor = node.parent;
        }
        self.owners.insert(id, connection);
        true
    }

    /// Forgets every prompt `connection` owns, after it disconnected.
    pub(super) fn release_prompts_of(&mut self, connection: u64) {
        self.owners.retain(|_, owner| *owner != connection);
    }

    /// The connection a session's running prompt came from.
    pub(super) fn owner_of(&self, id: SessionId) -> Option<u64> {
        self.owners.get(&id).copied()
    }

    /// Whether some connection holds the session `session` hangs under:
    /// the session itself, or the root of the agent tree it is part of.
    pub(super) fn held(&self, session: SessionId) -> bool {
        let root = self.tree.root_of(session);
        self.attached.get(&root).is_some_and(|count| *count > 0)
    }

    /// Whether a close was sent for `id` and not refused, so a load
    /// reopens its file instead of attaching to a dropped session.
    pub(super) fn is_closing(&self, id: SessionId) -> bool {
        self.closing.contains(&id)
    }

    /// Clears the close-in-flight record of a session whose close the
    /// engine confirmed: nothing holds the session open any more, so
    /// its file may be deleted. A close a reopen superseded was
    /// already dropped by that reopen's attach.
    pub(super) fn closed(&mut self, id: SessionId) {
        self.closing.remove(&id);
    }

    /// Whether the live state tracks `session` itself, so a recorded
    /// file's stale state must not be announced over it.
    pub(super) fn tracks(&self, session: SessionId) -> bool {
        self.tree.get(session).is_some()
            || self.running.contains(&session)
            || self.paused.contains_key(&session)
            || self.spawns.contains_key(&session)
    }

    /// The open asks under `root`, with the session that raised each
    /// and how to decline it.
    pub(super) fn asks_under(&self, root: SessionId) -> Vec<AskRef> {
        self.asks
            .values()
            .filter(|ask| self.tree.root_of(ask.session) == root)
            .map(|ask| AskRef {
                session: ask.session,
                request_id: ask.request_id,
                question: ask.tool == ASK_USER_QUESTION_TOOL,
            })
            .collect()
    }
}

impl CliAcpAgent {
    /// Attaches to the hosted session `id`: replays its file, then the
    /// turn in flight, the title, the live subagents and the open asks,
    /// all inside `hold_events` so no envelope slips between the file
    /// and the snapshot. `replay_file` is false for `session/resume`,
    /// which shows no history. Returns the session's config options.
    pub(super) fn attach_live(
        &self,
        id: SessionId,
        client_id: &str,
        path: &Path,
        settings: &Settings,
        replay_file: bool,
        ctx: Option<&PromptContext>,
    ) -> Result<Vec<SessionConfigOption>, RpcError> {
        self.host.engine.hold_events(|| {
            self.attach_inside(id, client_id, path, settings, replay_file, ctx)?;
            Ok(config_options(&self.host.models(), settings))
        })
    }

    fn attach_inside(
        &self,
        id: SessionId,
        client: &str,
        path: &Path,
        settings: &Settings,
        replay_file: bool,
        ctx: Option<&PromptContext>,
    ) -> Result<(), RpcError> {
        let replay = kage_session::replay(path).map_err(|e| RpcError::internal(e.to_string()))?;
        let file_ids: HashSet<MessageId> = replay.history.iter().map(|m| m.id).collect();
        let snap = lock(&self.host.live).snapshot(id);
        if replay_file && let Some(ctx) = ctx {
            for update in super::sessions::replay_updates(&replay.timeline) {
                let _ = ctx.update(update);
            }
            self.announce_restored(id, &replay, ctx);
        }
        // The turn in flight, unless the file already holds its message.
        let mut seen = HashMap::new();
        if let Some(ctx) = ctx
            && !snap
                .flight_message
                .is_some_and(|message| file_ids.contains(&message))
        {
            for event in &snap.flight {
                if let Some(update) = to_update(&mut seen, event) {
                    let _ = ctx.update(update);
                }
            }
        }
        lock(&self.shown).insert(id, Shown::fresh(settings.clone()));
        lock(&self.held).insert(id, HeldUpdates::default());
        lock(&self.ids).insert(client.to_owned(), id);
        if let Some(ctx) = ctx {
            let file_title = replay_file.then(|| replay.title.clone()).flatten();
            if let Some(title) = snap.title.clone().or(file_title) {
                let _ = ctx.update(SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                    title: Some(title),
                    updated_at: None,
                }));
            }
        }
        // The subagents the client missed, parents first, then every
        // open permission request under the session.
        let running = self.announce_subagents(id, client, &snap.subagents);
        let commander = self.host.engine.commander();
        for seed in &snap.asks {
            let ask = &seed.ask;
            let (tool_call, client_id) = match &seed.top {
                None => (
                    permission_call(ask.tool_call_id.as_ref(), &ask.tool, &ask.input),
                    client.to_owned(),
                ),
                Some((call_id, agent)) => (
                    ToolCallUpdate {
                        tool_call_id: call_id.to_string(),
                        title: Some(format!("{agent}: {}", tool_title(&ask.tool))),
                        kind: Some(tool_kind(&ask.tool)),
                        raw_input: Some(ask.input.clone()),
                        ..ToolCallUpdate::default()
                    },
                    ask.session.to_string(),
                ),
            };
            let kind = ask_kind(&ask.tool, &ask.input, tool_call);
            spawn_ask(
                &self.peer,
                &commander,
                &self.asks,
                ask.session,
                client_id,
                ask.request_id,
                kind,
            );
        }
        let paused = snap
            .subagents
            .iter()
            .filter(|seed| seed.state == SubagentState::Paused)
            .map(|seed| seed.session)
            .collect();
        lock(&self.seeds).push(Seed {
            session: id,
            spawns: snap.spawns,
            seen,
            running,
            paused,
        });
        self.host.attach(id, false);
        Ok(())
    }

    /// Announces `subagents` to the client on their parent's session,
    /// and returns the running ones the bridge streams from now on.
    fn announce_subagents(
        &self,
        id: SessionId,
        client: &str,
        subagents: &[SubagentSeed],
    ) -> HashSet<SessionId> {
        let mut running = HashSet::new();
        for seed in subagents {
            let parent = if seed.parent == id {
                client.to_owned()
            } else {
                seed.parent.to_string()
            };
            let _ = send_update(
                &self.peer,
                &parent,
                SessionUpdate::SubagentUpdate(SubagentUpdate {
                    subagent_session_id: seed.session.to_string(),
                    name: Some(seed.agent.clone()),
                    task: Some(seed.description.clone()),
                    capabilities: Some(SubagentSessionCapabilities {
                        cancel: seed.live(),
                    }),
                    state: Some(seed.state),
                    swarm: seed.swarm.as_ref().map(|member| SubagentSwarm {
                        id: member
                            .batch
                            .as_ref()
                            .map_or_else(|| seed.tool_call_id.clone(), ToString::to_string),
                        item: member.item.clone(),
                        index: member.index,
                        total: member.total,
                    }),
                    reason: seed.reason.clone(),
                    tool_call_id: Some(seed.tool_call_id.clone()),
                    usage: seed.usage.clone(),
                    model: seed.model.clone(),
                    background: seed.background,
                }),
            );
            if seed.live() {
                running.insert(seed.session);
                lock(&self.ids)
                    .subagents
                    .insert(seed.session.to_string(), seed.session);
            }
        }
        running
    }
}
