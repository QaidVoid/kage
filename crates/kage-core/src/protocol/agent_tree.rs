//! Client-side projection of agent sessions.
//!
//! Every agent is a session started by another session's `agent` call and
//! announced with [`HostEvent::AgentSpawned`]. An [`AgentTree`] folds the
//! envelopes a client receives into one [`AgentNode`] per agent, so the
//! client can route agent events and show what each agent is doing. It
//! holds [`Instant`]s, so it lives in memory only and is never serialized.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::{Envelope, Event, HostEvent, RunOutcome, SessionId, Usage};
use crate::{LoopEvent, ToolCallId};

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
    /// When the current or last run started.
    pub started: Option<Instant>,
    /// How long the last run took, once it ended.
    pub took: Option<Duration>,
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
        }) = &envelope.event
        {
            // A parent inside the new session's own ancestry would close a cycle.
            if !self.index.contains_key(&session) && self.root_of(*parent) != session {
                self.index.insert(session, self.nodes.len());
                self.nodes.push(AgentNode {
                    session,
                    parent: *parent,
                    tool_call_id: tool_call_id.clone(),
                    agent: agent.clone(),
                    description: description.clone(),
                    state: AgentState::Queued,
                    model: String::new(),
                    usage: Usage::default(),
                    tool_calls: 0,
                    last_tool: None,
                    waiting: 0,
                    started: None,
                    took: None,
                });
            }
            return self.index.contains_key(&session);
        }
        let Some(&i) = self.index.get(&session) else {
            return false;
        };
        let node = &mut self.nodes[i];
        match &envelope.event {
            Event::Host(HostEvent::RunStarted) => {
                node.state = AgentState::Running;
                node.started = Some(Instant::now());
                node.took = None;
            }
            Event::Host(HostEvent::RunEnded { outcome }) => {
                node.state = match outcome {
                    RunOutcome::Completed => AgentState::Done,
                    RunOutcome::Cancelled => AgentState::Cancelled,
                    RunOutcome::Failed { .. } => AgentState::Failed,
                };
                node.took = node.started.map(|started| started.elapsed());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{RequestId, SessionState};
    use crate::{LoopError, ThinkingLevel};

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
            thinking: ThinkingLevel::Off,
            permission_mode: None,
            working: true,
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
        };
        assert!(!tree.apply(&envelope(root, spawned)));
        assert_eq!(tree.root_of(child), root);
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
