//! Agent sessions an `agent` call starts.

use std::path::Path;
use std::sync::Arc;

use kage_core::agents::AgentDef;
use kage_core::protocol::{HostEvent, NoticeLevel, RunOutcome};
use kage_core::sync::lock;
use kage_core::{Content, Message, Role, SessionId, ToolOutput};
use kage_loop::AgentContext;
use kage_tools::ToolRegistry;

use super::agent_tool::{self, AGENT_TOOL, Spawn};
use super::runner::Work;
use super::{AgentSetup, Recorder, Session, SessionSpec, notice};

/// How an agent session hangs off the session that started it.
pub(super) struct AgentLink {
    pub(super) parent: SessionId,
    pub(super) agent: String,
    /// 1 for agents of the main session, 2 for theirs, and so on.
    pub(super) depth: u8,
    /// Delivers the result to the waiting `agent` call. Taken by the
    /// first run that finishes, so later runs a user starts in the
    /// agent never answer the parent twice.
    pub(super) reply: Option<crossbeam_channel::Sender<ToolOutput>>,
}

impl super::Dispatcher {
    /// Open a child session for an `agent` call and start it, or queue it
    /// when the running limit is reached. Errors reply at once.
    pub(super) fn spawn(&mut self, spawn: Spawn) {
        let Spawn {
            parent,
            tool_call_id,
            agent,
            description,
            prompt,
            reply,
        } = spawn;
        let fail = |text: String| {
            let _ = reply.send(agent_tool::error_output(text));
        };
        let Some(from) = self.sessions.get(&parent) else {
            return fail(format!("session {parent} is gone"));
        };
        let Some(setup) = from.agents.clone() else {
            return fail("agents are turned off".to_owned());
        };
        let depth = depth_of(from) + 1;
        if depth > setup.max_depth {
            return fail(format!(
                "agents may nest {} level(s) deep (agent_max_depth)",
                setup.max_depth
            ));
        }
        let Some(def) = setup.defs.get(&agent) else {
            let names: Vec<&str> = setup.defs.iter().map(|d| d.name.as_str()).collect();
            return fail(format!(
                "unknown agent `{agent}`. Available agents: {}",
                names.join(", ")
            ));
        };

        let id = SessionId::new();
        let (spec, missing) = agent_spec(from, parent, id, def, &setup);
        let cancel = from.cancel.child();
        let link = AgentLink {
            parent,
            agent: agent.clone(),
            depth,
            reply: Some(reply),
        };
        let marker = serde_json::json!({
            "parent": parent,
            "tool_call_id": tool_call_id,
            "agent": agent,
            "description": description,
        });

        self.bus.publish(
            id,
            HostEvent::AgentSpawned {
                parent,
                tool_call_id,
                agent,
                description: description.clone(),
            },
        );
        self.open(spec, cancel, Some(link));
        self.record_agent_entries(id, marker, description);
        for name in missing {
            notice(
                &self.bus,
                id,
                NoticeLevel::Warning,
                format!("agent tools: no tool named `{name}`"),
            );
        }
        let content = vec![Content::Text { text: prompt }];
        if self.running_agents() < setup.max_running {
            self.start_run(id, Work::Prompt(Message::new(Role::User, content, None)));
        } else {
            let session = self.sessions.get_mut(&id).expect("session opened");
            session.queued.push_back(content);
            self.waiting.push_back(id);
        }
    }

    /// Write the `kage:agent` marker and the title right after the header.
    fn record_agent_entries(&mut self, id: SessionId, marker: serde_json::Value, title: String) {
        let Some(recorder) = self
            .sessions
            .get_mut(&id)
            .and_then(|s| s.idle.as_mut())
            .and_then(|i| i.recorder.as_mut())
        else {
            return;
        };
        let ts = chrono::Utc::now();
        let entries = [
            kage_session::SessionEntry::Custom(kage_session::Custom {
                id: kage_session::EntryId::new(),
                ts,
                kind: kage_session::list::AGENT_ENTRY_KIND.to_owned(),
                data: marker,
            }),
            kage_session::SessionEntry::Title(kage_session::SessionTitle {
                id: kage_session::EntryId::new(),
                ts,
                title,
            }),
        ];
        for entry in &entries {
            if let Err(err) = recorder.append(entry) {
                notice(
                    &self.bus,
                    id,
                    NoticeLevel::Error,
                    format!("session write failed: {err}"),
                );
                return;
            }
        }
    }

    /// Send an agent's result to its `agent` call, once. Callers publish
    /// the agent's `RunEnded` first, so clients see the agent end before
    /// the parent continues.
    pub(super) fn deliver(&mut self, id: SessionId, outcome: &RunOutcome, history: &[Message]) {
        if let Some((reply, output)) = self.take_reply(id, outcome, history) {
            let _ = reply.send(output);
        }
    }

    /// Take an agent's `agent` call reply and its result, once, to send
    /// later.
    pub(super) fn take_reply(
        &mut self,
        id: SessionId,
        outcome: &RunOutcome,
        history: &[Message],
    ) -> Option<(crossbeam_channel::Sender<ToolOutput>, ToolOutput)> {
        let link = self.sessions.get_mut(&id)?.link.as_mut()?;
        let reply = link.reply.take()?;
        Some((
            reply,
            agent_tool::agent_result(id, &link.agent, outcome, history),
        ))
    }

    /// Agent runs in flight that hold a slot of the running limit. An
    /// agent waiting on its own agents holds none, so nesting cannot
    /// deadlock the limit.
    fn running_agents(&self) -> usize {
        let waits_on_agents = |id: &SessionId| {
            self.sessions.values().any(|s| {
                s.link
                    .as_ref()
                    .is_some_and(|l| l.parent == *id && l.reply.is_some())
            })
        };
        self.sessions
            .iter()
            .filter(|(id, s)| s.link.is_some() && s.idle.is_none() && !waits_on_agents(id))
            .count()
    }

    /// Start waiting agents while the running limit allows.
    pub(super) fn start_waiting(&mut self) {
        while !self.shutting_down
            && let Some(&id) = self.waiting.front()
        {
            let max = self
                .sessions
                .get(&id)
                .and_then(|s| s.agents.as_ref())
                .map_or(usize::MAX, |a| a.max_running);
            if self.running_agents() >= max {
                return;
            }
            self.waiting.pop_front();
            let next = self
                .sessions
                .get_mut(&id)
                .and_then(|s| s.queued.pop_front());
            if let Some(content) = next {
                self.start_run(id, Work::Prompt(Message::new(Role::User, content, None)));
            }
        }
    }

    /// End a waiting agent that never started as cancelled.
    pub(super) fn end_waiting(&mut self, id: SessionId) {
        self.waiting.retain(|w| *w != id);
        if let Some(session) = self.sessions.get_mut(&id) {
            session.queued.clear();
            lock(&session.steering).clear();
        }
        self.bus.publish(
            id,
            HostEvent::RunEnded {
                outcome: RunOutcome::Cancelled,
            },
        );
        self.deliver(id, &RunOutcome::Cancelled, &[]);
    }

    pub(super) fn parent_of(&self, id: SessionId) -> Option<SessionId> {
        self.sessions.get(&id)?.link.as_ref().map(|l| l.parent)
    }

    /// Whether `id` is an agent somewhere below `ancestor`.
    pub(super) fn descends_from(&self, id: SessionId, ancestor: SessionId) -> bool {
        let mut current = self.parent_of(id);
        while let Some(parent) = current {
            if parent == ancestor {
                return true;
            }
            current = self.parent_of(parent);
        }
        false
    }
}

/// The session an agent of `from` runs in: the definition's model,
/// thinking, role and tools over `from`'s, `from`'s gate and loop
/// settings, no plugins or MCP of its own, and a file next to `from`'s
/// when `from` records. Also returns listed tools that match nothing.
fn agent_spec(
    from: &Session,
    parent: SessionId,
    id: SessionId,
    def: &AgentDef,
    setup: &AgentSetup,
) -> (SessionSpec, Vec<String>) {
    let model = def
        .model
        .clone()
        .unwrap_or_else(|| from.state.model.clone());
    let system_prompt =
        crate::runtime_env::build_system_prompt(&def.body, &from.workdir, &model, &[]);
    let mut cx = AgentContext::new(model.clone(), &system_prompt).with_workdir(&from.workdir);
    cx.confine_paths = from.confine_paths;
    cx.thinking_level = def.thinking.or(from.state.thinking);
    let (tools, missing) = agent_tools(&from.tools, def.tools.as_deref());
    let recorder = from.path.as_deref().and_then(Path::parent).map(|dir| {
        let header = kage_session::Header {
            version: kage_session::FORMAT_VERSION,
            session: id,
            id: kage_session::EntryId::new(),
            ts: chrono::Utc::now(),
            cwd: from.workdir.clone(),
            model: model.clone(),
            system_prompt,
            parent_session: Some(parent),
            parent_entry: None,
        };
        Recorder::planned(crate::build_session_path(dir, id), header, None)
    });
    let spec = SessionSpec {
        id,
        model,
        cx,
        recorder,
        tools,
        plugins: None,
        gate: from.gate.clone(),
        loop_cfg: from.loop_cfg,
        mcp: None,
        interactive: from.interactive,
        title: false,
        agents: Some(setup.clone()),
    };
    (spec, missing)
}

/// 0 for a main session, 1 for its agents, and so on.
pub(super) fn depth_of(session: &Session) -> u8 {
    session.link.as_ref().map_or(0, |l| l.depth)
}

/// The tools an agent gets: `parent`'s, narrowed to `only` when the
/// definition lists tools. Also returns listed names that match nothing.
fn agent_tools(parent: &ToolRegistry, only: Option<&[String]>) -> (ToolRegistry, Vec<String>) {
    let Some(only) = only else {
        return (parent.clone(), Vec::new());
    };
    let mut tools = ToolRegistry::new();
    let mut missing = Vec::new();
    for name in only {
        match parent.get(name) {
            Some(tool) => tools.register(Arc::clone(tool)),
            None if name == AGENT_TOOL => {}
            None => missing.push(name.clone()),
        }
    }
    (tools, missing)
}
