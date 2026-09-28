//! Client-side projection of agent sessions.
//!
//! Every agent is a session started by another session's `agent` call and
//! announced with [`HostEvent::AgentSpawned`]. An [`AgentTree`] folds the
//! envelopes a client receives into one [`AgentNode`] per agent, so the
//! client can route agent events and show what each agent is doing. It
//! holds [`Instant`]s, so it lives in memory only and is never serialized.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::{Envelope, Event, HostEvent, RunOutcome, SessionId, SwarmMember, TokenUsage, Usage};
use crate::{Content, LoopEvent, Message, ToolCallId};

/// What a client knows about agent sessions, built from envelopes.
#[derive(Debug, Default)]
pub struct AgentTree {
    nodes: Vec<AgentNode>,
    index: HashMap<SessionId, usize>,
}

/// One agent session as seen through its envelopes.
#[derive(Clone, Debug)]
pub struct AgentNode {
    /// The agent's own session.
    pub session: SessionId,
    /// Session whose `agent` call started this agent.
    pub parent: SessionId,
    /// The parent's `agent` call that started this agent.
    pub tool_call_id: ToolCallId,
    /// Name of the agent definition.
    pub agent: String,
    /// Short task description the model wrote for the user.
    pub description: String,
    /// Swarm batch membership, when a `swarm` call started this
    /// agent. `None` for a plain `agent` call.
    pub swarm: Option<SwarmMember>,
    /// Where the agent is.
    pub state: AgentState,
    /// Provider-qualified model from the latest state snapshot.
    pub model: String,
    /// Latest usage totals.
    pub usage: Usage,
    /// Tool calls started across every run.
    pub tool_calls: u32,
    /// Name and input of the latest tool call.
    pub last_tool: Option<(String, serde_json::Value)>,
    /// Open permission requests.
    pub waiting: u32,
    /// When the run in flight started. `None` between runs.
    pub started: Option<Instant>,
    /// Time spent in finished runs, summed across runs. `None` until
    /// the first run ends.
    pub took: Option<Duration>,
    /// Rebuilt from a stored conversation by [`AgentTree::restore`]. No
    /// engine runs it, so it cannot be messaged.
    pub restored: bool,
}

impl AgentNode {
    /// How long the agent has run: its finished runs plus the run in
    /// flight. `None` before its first run starts.
    #[must_use]
    pub fn elapsed(&self) -> Option<Duration> {
        let current = self.started.map(|started| started.elapsed());
        match (self.took, current) {
            (None, None) => None,
            (took, current) => Some(took.unwrap_or_default() + current.unwrap_or_default()),
        }
    }
}

/// Where an agent is. `Running` with `waiting > 0` reads as waiting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentState {
    /// Announced, but no run has started yet.
    Queued,
    /// A run is in flight.
    Running,
    /// The last run completed.
    Done,
    /// The last run stopped on an error.
    Failed,
    /// The last run was cancelled.
    Cancelled,
}

impl AgentTree {
    /// Fold one envelope in. Returns whether it belongs to a known agent.
    pub fn apply(&mut self, envelope: &Envelope) -> bool {
        let session = envelope.session;
        if let Event::Host(HostEvent::AgentSpawned {
            parent,
            tool_call_id,
            agent,
            description,
            swarm,
        }) = &envelope.event
        {
            return self.insert(AgentNode {
                session,
                parent: *parent,
                tool_call_id: tool_call_id.clone(),
                agent: agent.clone(),
                description: description.clone(),
                swarm: swarm.clone(),
                state: AgentState::Queued,
                model: String::new(),
                usage: Usage::default(),
                tool_calls: 0,
                last_tool: None,
                waiting: 0,
                started: None,
                took: None,
                restored: false,
            });
        }
        let Some(&i) = self.index.get(&session) else {
            return false;
        };
        let node = &mut self.nodes[i];
        match &envelope.event {
            Event::Host(HostEvent::RunStarted) => {
                node.state = AgentState::Running;
                node.started = Some(Instant::now());
            }
            Event::Host(HostEvent::RunEnded { outcome }) => {
                node.state = match outcome {
                    RunOutcome::Completed => AgentState::Done,
                    RunOutcome::Cancelled => AgentState::Cancelled,
                    RunOutcome::Failed { .. } => AgentState::Failed,
                };
                if let Some(started) = node.started.take() {
                    node.took = Some(node.took.unwrap_or_default() + started.elapsed());
                }
                node.waiting = 0;
            }
            Event::Host(HostEvent::StateChanged { state }) => node.model.clone_from(&state.model),
            Event::Host(HostEvent::UsageUpdated { usage }) => node.usage = *usage,
            Event::Host(HostEvent::PermissionRequested { .. }) => node.waiting += 1,
            Event::Host(HostEvent::PermissionResolved { .. }) => {
                node.waiting = node.waiting.saturating_sub(1);
            }
            Event::Loop(LoopEvent::ToolCallStart {
                name,
                input_partial,
                ..
            }) => {
                node.tool_calls += 1;
                node.last_tool = Some((name.clone(), input_partial.clone()));
            }
            _ => {}
        }
        true
    }

    /// Add the agents that `parent`'s stored conversation started, as
    /// finished nodes. Each comes from an `agent` call whose result
    /// carries the `<agent name=.. session=.. state=..>` wrapper, or
    /// from a `swarm` call whose aggregate wraps one child per
    /// `<swarm>` element. Usage, tool counts and run time come from
    /// the wrapper's recorded stats; without a recorded time, an
    /// `agent` call falls back to the gap between the call and its
    /// result. Sessions already known are skipped.
    pub fn restore(&mut self, parent: SessionId, messages: &[Message]) {
        // Recorded calls: the task text, the call time and whether the
        // call was a `swarm` batch.
        let mut calls = HashMap::new();
        for message in messages {
            for block in &message.content {
                match block {
                    Content::ToolCall { id, name, input } if name == "agent" || name == "swarm" => {
                        let description = input.get("description").and_then(|d| d.as_str());
                        calls.insert(
                            id.clone(),
                            (
                                description.unwrap_or("").to_owned(),
                                message.ts,
                                name == "swarm",
                            ),
                        );
                    }
                    Content::ToolResultBlock {
                        call_id, output, ..
                    } => {
                        let Some((description, called, swarm)) = calls.remove(call_id) else {
                            continue;
                        };
                        let wrapped = wrapped_agents(output);
                        let total = wrapped.len();
                        for (index, (agent, item)) in wrapped.into_iter().enumerate() {
                            let index = u32::try_from(index).unwrap_or(u32::MAX);
                            // A recorded run time is the truth; the gap
                            // between call and result is the fallback
                            // for older transcripts, and a batch child
                            // of one has no own time at all.
                            let took = match agent.run_ms {
                                Some(ms) => Some(Duration::from_millis(ms)),
                                None if !swarm => (message.ts - called).to_std().ok(),
                                None => None,
                            };
                            self.insert(AgentNode {
                                session: agent.session,
                                parent,
                                tool_call_id: call_id.clone(),
                                agent: agent.agent,
                                description: description.clone(),
                                swarm: swarm.then_some(SwarmMember {
                                    item,
                                    index,
                                    total: u32::try_from(total).unwrap_or(u32::MAX),
                                }),
                                state: agent.state,
                                model: String::new(),
                                usage: agent.usage,
                                tool_calls: agent.tool_calls,
                                last_tool: None,
                                waiting: 0,
                                started: None,
                                took,
                                restored: true,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// The agent running as `session`, if it is one.
    #[must_use]
    pub fn get(&self, session: SessionId) -> Option<&AgentNode> {
        self.index.get(&session).map(|&i| &self.nodes[i])
    }

    /// The nearest ancestor that is not an agent. `session` itself when it
    /// is not an agent.
    #[must_use]
    pub fn root_of(&self, mut session: SessionId) -> SessionId {
        while let Some(node) = self.get(session) {
            session = node.parent;
        }
        session
    }

    /// Agents under `root`, depth first in spawn order. Depth 1 is a
    /// direct child of `root`.
    #[must_use]
    pub fn under(&self, root: SessionId) -> Vec<(usize, &AgentNode)> {
        let mut out = Vec::new();
        self.push_children(root, 1, &mut out);
        out
    }

    /// Forget every agent.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.index.clear();
    }

    /// Add `node` unless its session is known or its parent sits in
    /// its own ancestry, which would close a cycle. Returns whether the
    /// session is known afterwards.
    fn insert(&mut self, node: AgentNode) -> bool {
        let session = node.session;
        if !self.index.contains_key(&session) && self.root_of(node.parent) != session {
            self.index.insert(session, self.nodes.len());
            self.nodes.push(node);
        }
        self.index.contains_key(&session)
    }

    fn push_children<'a>(
        &'a self,
        parent: SessionId,
        depth: usize,
        out: &mut Vec<(usize, &'a AgentNode)>,
    ) {
        for node in self.nodes.iter().filter(|node| node.parent == parent) {
            out.push((depth, node));
            self.push_children(node.session, depth + 1, out);
        }
    }
}

/// The value of `key="..."` in a wrapper's attribute list.
fn attr_value<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let value = attrs.split_once(&format!("{key}=\""))?.1;
    value.split_once('"').map(|(value, _)| value)
}

/// One `<agent ...>` header of a stored result: who ran, how it
/// ended, and the stats the engine recorded on the header.
struct RestoredAgent {
    agent: String,
    session: SessionId,
    state: AgentState,
    usage: Usage,
    tool_calls: u32,
    /// Recorded run time in milliseconds, when the header carries it.
    run_ms: Option<u64>,
}

/// The `<agent ...>` header of a stored result, parsed. The stats
/// attrs are written by the engine and may be missing on older
/// transcripts; they default to zero.
fn agent_header(line: &str) -> Option<RestoredAgent> {
    let attrs = line.strip_prefix("<agent ")?.split_once('>')?.0;
    let attr = |key: &str| attr_value(attrs, key);
    let num = |key: &str| -> u64 { attr(key).and_then(|value| value.parse().ok()).unwrap_or(0) };
    let session = ulid::Ulid::from_string(attr("session")?).ok()?;
    let state = match attr("state")? {
        "completed" => AgentState::Done,
        "cancelled" => AgentState::Cancelled,
        "failed" => AgentState::Failed,
        _ => return None,
    };
    Some(RestoredAgent {
        agent: attr("name")?.to_owned(),
        session: SessionId(session),
        state,
        tool_calls: u32::try_from(num("tools")).unwrap_or(u32::MAX),
        usage: Usage {
            total: TokenUsage {
                input: num("in"),
                output: num("out"),
                cache_read: num("cache_read"),
                cache_write: num("cache_write"),
            },
            context_used: num("ctx"),
            context_window: num("win"),
            cost: attr("cost")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0.0),
        },
        run_ms: attr("run_ms").and_then(|value| value.parse().ok()),
    })
}

/// The agents a stored result wraps, with each one's batch item: one
/// for an `agent` call's wrapper, one per `<swarm>` element of a
/// `swarm` call's aggregate. Bodies may be truncated or missing; the
/// headers survive.
fn wrapped_agents(output: &str) -> Vec<(RestoredAgent, String)> {
    let mut found = Vec::new();
    let mut rest = output;
    while let Some(at) = rest.find("<swarm ") {
        rest = &rest[at..];
        let Some((attrs, after)) = rest.split_once(">\n") else {
            break;
        };
        let item = attr_value(attrs, "item").unwrap_or_default().to_owned();
        let Some((line, remainder)) = after.split_once('\n') else {
            break;
        };
        if let Some(agent) = agent_header(line) {
            found.push((agent, item));
        }
        rest = remainder;
    }
    if found.is_empty()
        && let Some(agent) = agent_header(output)
    {
        found.push((agent, String::new()));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LoopError;
    use crate::protocol::{RequestId, SessionState};

    fn envelope(session: SessionId, event: impl Into<Event>) -> Envelope {
        Envelope {
            session,
            seq: 1,
            event: event.into(),
        }
    }

    fn spawn(tree: &mut AgentTree, parent: SessionId, agent: &str) -> SessionId {
        let session = SessionId::new();
        let spawned = HostEvent::AgentSpawned {
            parent,
            tool_call_id: ToolCallId(format!("call_{agent}")),
            agent: agent.into(),
            description: format!("{agent} task"),
            swarm: None,
        };
        assert!(tree.apply(&envelope(session, spawned)));
        session
    }

    fn end(tree: &mut AgentTree, session: SessionId, outcome: RunOutcome) -> AgentState {
        tree.apply(&envelope(session, HostEvent::RunStarted));
        tree.apply(&envelope(session, HostEvent::RunEnded { outcome }));
        tree.get(session).unwrap().state
    }

    #[test]
    fn an_agent_moves_from_queued_through_waiting_to_done() {
        let mut tree = AgentTree::default();
        let root = SessionId::new();
        let child = spawn(&mut tree, root, "explore");
        let node = tree.get(child).unwrap();
        assert_eq!(node.state, AgentState::Queued);
        assert_eq!(node.parent, root);
        assert_eq!(node.tool_call_id, ToolCallId("call_explore".into()));
        assert_eq!(node.description, "explore task");
        assert!(node.started.is_none());

        assert!(tree.apply(&envelope(child, HostEvent::RunStarted)));
        let state = SessionState {
            model: "mock:m".into(),
            permission_mode: None,
            working: true,
            ..SessionState::default()
        };
        assert!(tree.apply(&envelope(child, HostEvent::StateChanged { state })));
        let usage = Usage {
            context_used: 42,
            ..Usage::default()
        };
        assert!(tree.apply(&envelope(child, HostEvent::UsageUpdated { usage })));
        let node = tree.get(child).unwrap();
        assert_eq!(node.state, AgentState::Running);
        assert!(node.started.is_some());
        assert_eq!(node.model, "mock:m");
        assert_eq!(node.usage.context_used, 42);

        let call = LoopEvent::ToolCallStart {
            id: ToolCallId("call_1".into()),
            name: "read".into(),
            input_partial: serde_json::json!({ "path": "src/lib.rs" }),
        };
        assert!(tree.apply(&envelope(child, call)));
        let ask = HostEvent::PermissionRequested {
            request_id: RequestId(1),
            tool_call_id: Some(ToolCallId("call_1".into())),
            tool: "read".into(),
            subject: "src/lib.rs".into(),
            input: serde_json::json!({ "path": "src/lib.rs" }),
        };
        assert!(tree.apply(&envelope(child, ask)));
        let node = tree.get(child).unwrap();
        assert_eq!(node.tool_calls, 1);
        let (name, input) = node.last_tool.as_ref().unwrap();
        assert_eq!(name, "read");
        assert_eq!(input["path"], "src/lib.rs");
        assert_eq!(node.waiting, 1);
        assert_eq!(node.state, AgentState::Running);

        let resolved = HostEvent::PermissionResolved {
            request_id: RequestId(1),
        };
        assert!(tree.apply(&envelope(child, resolved)));
        assert_eq!(tree.get(child).unwrap().waiting, 0);

        let ended = HostEvent::RunEnded {
            outcome: RunOutcome::Completed,
        };
        assert!(tree.apply(&envelope(child, ended)));
        let node = tree.get(child).unwrap();
        assert_eq!(node.state, AgentState::Done);
        assert!(node.took.is_some());
    }

    #[test]
    fn run_outcomes_map_to_end_states() {
        let mut tree = AgentTree::default();
        let child = spawn(&mut tree, SessionId::new(), "general");
        assert_eq!(
            end(&mut tree, child, RunOutcome::Cancelled),
            AgentState::Cancelled
        );
        let failed = RunOutcome::Failed {
            error: LoopError::ContextOverflow,
        };
        assert_eq!(end(&mut tree, child, failed), AgentState::Failed);
        assert_eq!(
            end(&mut tree, child, RunOutcome::Completed),
            AgentState::Done
        );
    }

    #[test]
    fn root_of_walks_up_to_the_first_session_that_is_not_an_agent() {
        let mut tree = AgentTree::default();
        let root = SessionId::new();
        let child = spawn(&mut tree, root, "general");
        let grandchild = spawn(&mut tree, child, "explore");
        assert_eq!(tree.root_of(grandchild), root);
        assert_eq!(tree.root_of(child), root);
        assert_eq!(tree.root_of(root), root);
    }

    #[test]
    fn under_lists_depth_first_in_spawn_order() {
        let mut tree = AgentTree::default();
        let root = SessionId::new();
        let first = spawn(&mut tree, root, "general");
        let second = spawn(&mut tree, root, "explore");
        let nested = spawn(&mut tree, first, "test");
        spawn(&mut tree, SessionId::new(), "other");

        let rows: Vec<(usize, SessionId)> = tree
            .under(root)
            .into_iter()
            .map(|(depth, node)| (depth, node.session))
            .collect();
        assert_eq!(rows, vec![(1, first), (2, nested), (1, second)]);
        assert_eq!(tree.under(first).len(), 1);
    }

    #[test]
    fn envelopes_from_unknown_sessions_are_not_agents() {
        let mut tree = AgentTree::default();
        spawn(&mut tree, SessionId::new(), "explore");
        assert!(!tree.apply(&envelope(SessionId::new(), HostEvent::RunStarted)));
    }

    #[test]
    fn a_spawn_that_would_close_a_cycle_is_ignored() {
        let mut tree = AgentTree::default();
        let root = SessionId::new();
        let child = spawn(&mut tree, root, "general");
        let spawned = HostEvent::AgentSpawned {
            parent: child,
            tool_call_id: ToolCallId("call_x".into()),
            agent: "explore".into(),
            description: "loop".into(),
            swarm: None,
        };
        assert!(!tree.apply(&envelope(root, spawned)));
        assert_eq!(tree.root_of(child), root);
    }

    #[test]
    fn time_adds_up_across_runs_and_starts_with_the_first_run() {
        let mut tree = AgentTree::default();
        let child = spawn(&mut tree, SessionId::new(), "explore");
        assert_eq!(tree.get(child).unwrap().elapsed(), None);
        let ended = |tree: &mut AgentTree| {
            let outcome = RunOutcome::Completed;
            tree.apply(&envelope(child, HostEvent::RunEnded { outcome }));
        };
        ended(&mut tree);
        assert_eq!(tree.get(child).unwrap().elapsed(), None, "never started");

        tree.apply(&envelope(child, HostEvent::RunStarted));
        tree.nodes[0].started = Instant::now().checked_sub(Duration::from_secs(8));
        ended(&mut tree);
        let first = tree.get(child).unwrap().took.unwrap();
        assert!(first >= Duration::from_secs(8));

        tree.apply(&envelope(child, HostEvent::RunStarted));
        tree.nodes[0].started = Instant::now().checked_sub(Duration::from_secs(2));
        assert!(tree.get(child).unwrap().elapsed().unwrap() >= first + Duration::from_secs(2));
        ended(&mut tree);
        let node = tree.get(child).unwrap();
        assert!(node.took.unwrap() >= first + Duration::from_secs(2));
        assert!(node.started.is_none());
        assert_eq!(node.elapsed(), node.took);
    }

    #[test]
    fn restore_reads_the_agents_of_a_stored_conversation() {
        let parent = SessionId::new();
        let done = SessionId::new();
        let stopped = SessionId::new();
        let call = |id: &str, description: &str| Content::ToolCall {
            id: ToolCallId(id.into()),
            name: "agent".into(),
            input: serde_json::json!({ "agent": "explore", "description": description }),
        };
        let result = |id: &str, output: String| Content::ToolResultBlock {
            call_id: ToolCallId(id.into()),
            output,
            is_error: false,
        };
        let wrapped = |session: SessionId, state: &str| {
            format!(
                "<agent name=\"explore\" session=\"{session}\" state=\"{state}\">\nreply\n</agent>"
            )
        };
        let asked = Message::new(
            crate::Role::Assistant,
            vec![
                call("a1", "map src"),
                call("a2", "map docs"),
                call("a3", "x"),
            ],
            None,
        );
        let mut answered = Message::new(
            crate::Role::ToolResult,
            vec![
                result("a1", wrapped(done, "completed")),
                result("a2", wrapped(stopped, "cancelled")),
                result("a3", "unknown agent".into()),
            ],
            None,
        );
        answered.ts = asked.ts + chrono::Duration::milliseconds(8_800);

        let mut tree = AgentTree::default();
        tree.restore(parent, &[asked, answered]);
        let rows: Vec<(SessionId, AgentState, &str)> = tree
            .under(parent)
            .into_iter()
            .map(|(_, node)| (node.session, node.state, node.description.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (done, AgentState::Done, "map src"),
                (stopped, AgentState::Cancelled, "map docs"),
            ]
        );
        let node = tree.get(done).unwrap();
        assert!(node.restored);
        assert_eq!(node.agent, "explore");
        assert_eq!(node.tool_call_id, ToolCallId("a1".into()));
        assert_eq!(node.elapsed(), Some(Duration::from_millis(8_800)));
    }

    #[test]
    fn restore_reads_the_stats_recorded_on_a_wrapper() {
        let parent = SessionId::new();
        let child = SessionId::new();
        let asked = Message::new(
            crate::Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId("a1".into()),
                name: "agent".into(),
                input: serde_json::json!({ "agent": "explore", "description": "map src" }),
            }],
            None,
        );
        let answered = Message::new(
            crate::Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId("a1".into()),
                output: format!(
                    "<agent name=\"explore\" session=\"{child}\" state=\"completed\" \
                     tools=\"9\" in=\"120000\" out=\"40000\" cache_read=\"80000\" \
                     cache_write=\"8000\" cost=\"1.25\" ctx=\"248000\" win=\"1000000\" \
                     run_ms=\"4200\">\nreply\n</agent>"
                ),
                is_error: false,
            }],
            None,
        );
        let mut tree = AgentTree::default();
        tree.restore(parent, &[asked, answered]);
        let node = tree.get(child).unwrap();
        assert_eq!(node.tool_calls, 9);
        assert_eq!(node.usage.total.input, 120_000);
        assert_eq!(node.usage.total.output, 40_000);
        assert_eq!(node.usage.total.cache_read, 80_000);
        assert_eq!(node.usage.total.cache_write, 8_000);
        assert!((node.usage.cost - 1.25).abs() < 1e-9);
        assert_eq!(node.usage.context_used, 248_000);
        assert_eq!(node.usage.context_window, 1_000_000);
        assert_eq!(node.took, Some(Duration::from_millis(4_200)));
    }

    #[test]
    fn restore_rebuilds_a_swarm_batch_from_its_aggregate() {
        let parent = SessionId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let asked = Message::new(
            crate::Role::Assistant,
            vec![Content::ToolCall {
                id: ToolCallId("s1".into()),
                name: "swarm".into(),
                input: serde_json::json!({
                    "description": "review crates",
                    "items": ["kage-core", "kage-tui"],
                }),
            }],
            None,
        );
        let aggregate = format!(
            "completed: 2, failed: 0, cancelled: 0\n<swarm description=\"review crates\" \
             item=\"kage-core\">\n<agent name=\"explore\" session=\"{first}\" state=\"completed\" \
             tools=\"3\" in=\"100\" out=\"10\" cache_read=\"0\" cache_write=\"0\" \
             cost=\"0.01\" ctx=\"110\" win=\"200000\" run_ms=\"15000\">\ndid core\n</agent>\
             \n</swarm>\n<swarm description=\"review crates\" \
             item=\"kage-tui\">\n<agent name=\"explore\" session=\"{second}\" \
             state=\"completed\" tools=\"4\" in=\"200\" out=\"20\" cache_read=\"0\" \
             cache_write=\"0\" cost=\"0.02\" ctx=\"220\" win=\"200000\" run_ms=\"9000\">\
             \ndid tui\n</agent>\n</swarm>"
        );
        let answered = Message::new(
            crate::Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId("s1".into()),
                output: aggregate,
                is_error: false,
            }],
            None,
        );
        let mut tree = AgentTree::default();
        tree.restore(parent, &[asked, answered]);
        let members: Vec<(SessionId, u32, u32, String)> = tree
            .under(parent)
            .into_iter()
            .map(|(_, node)| {
                let member = node.swarm.as_ref().unwrap();
                (
                    node.session,
                    member.index,
                    member.total,
                    member.item.clone(),
                )
            })
            .collect();
        assert_eq!(members.len(), 2);
        let core = &members[0];
        let tui = &members[1];
        assert_eq!((core.1, core.2), (0, 2));
        assert_eq!((tui.1, tui.2), (1, 2));
        assert_eq!(core.3, "kage-core");
        assert_eq!(tui.3, "kage-tui");
        let node = tree.get(first).unwrap();
        assert_eq!(node.tool_calls, 3);
        assert_eq!(node.usage.total.input, 100);
        assert!((node.usage.cost - 0.01).abs() < 1e-9);
        assert_eq!(node.usage.context_window, 200_000);
        assert_eq!(node.took, Some(Duration::from_secs(15)));
        let node = tree.get(second).unwrap();
        assert_eq!(node.took, Some(Duration::from_secs(9)));
    }

    #[test]
    fn clear_forgets_every_agent() {
        let mut tree = AgentTree::default();
        let root = SessionId::new();
        let child = spawn(&mut tree, root, "explore");
        tree.clear();
        assert!(tree.get(child).is_none());
        assert!(tree.under(root).is_empty());
        assert!(!tree.apply(&envelope(child, HostEvent::RunStarted)));
    }
}
