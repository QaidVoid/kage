//! Agent sessions an `agent` call starts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kage_core::agents::{AgentDef, Isolation};
use kage_core::protocol::{HostEvent, NoticeLevel, RunOutcome, SwarmMember, Usage};
use kage_core::sync::lock;
use kage_core::{CancelFlag, Content, Message, MessageId, Role, SessionId, ToolCallId, ToolOutput};
use kage_loop::{AgentContext, TokenBudget};
use kage_provider::ProviderRegistry;
use kage_tools::ToolRegistry;

use super::agent_tool::{self, AGENT_TOOL, RunFacts, Spawn};
use super::mailbox_tool::MAILBOX_TOOL;
use super::runner::Work;
use super::swarm_tool::{self, Member, SWARM_TOOL, SwarmInfo};
use super::worktree::Worktree;
use super::{
    AgentSetup, Attach, Background, CONTINUE_PROMPT, Recorder, ResumeChild, Session, SessionSpec,
    notice,
};

/// Tells a forked child that the conversation it starts with is
/// inherited reference material, not its own past. Ported from
/// kimi-code's `FORK_CONTEXT_NOTICE`.
const FORK_CONTEXT_NOTICE: &str = "The conversation above is not your own history. It is a \
snapshot inherited from the session that forked you, so treat it as reference material only. \
You are an independent agent, not a continuation of that agent. Do the task in the next message \
yourself, then report the result.";

/// The children a client's swarm resume continued, once every one has
/// reported.
pub(super) struct SwarmResumed {
    pub(super) parent: SessionId,
    /// The description the first child was spawned under.
    pub(super) description: String,
    pub(super) members: Vec<Member>,
    pub(super) outputs: Vec<ToolOutput>,
}

/// How an agent session hangs off the session that started it.
pub(super) struct AgentLink {
    pub(super) parent: SessionId,
    pub(super) agent: String,
    /// 1 for agents of the main session, 2 for theirs, and so on.
    pub(super) depth: u8,
    /// The swarm call this child belongs to, when a `swarm` call
    /// spawned it. Names the child in its session marker, so a later
    /// phase can tell swarm children from plain agents.
    pub(super) batch_id: Option<ToolCallId>,
    /// Where the result of the agent's next finished run goes. Taken
    /// by the first run that finishes, so later runs a user starts in
    /// the agent never answer the parent twice.
    pub(super) report: Option<Report>,
    /// Whether the agent runs in the background: its cancel flag is its
    /// own, so stopping the parent's run leaves it running.
    pub(super) background: bool,
    /// Session file whose conversation loads into the context at the
    /// first run start instead of at spawn. Set for forked children:
    /// their copied transcript would otherwise sit in RAM, possibly
    /// for the whole batch, while the child waits for a run slot.
    pub(super) lazy_history: Option<PathBuf>,
    /// The last message of a forked child's inherited snapshot, set
    /// when the snapshot loads. The child's result reads only what
    /// comes after it, so the parent's own reply never passes as the
    /// child's.
    pub(super) inherited_until: Option<MessageId>,
    /// The definition's tool list. `None` allows every tool, the
    /// delegation and mailbox tools included.
    pub(super) tools: Option<Vec<String>>,
    /// The agent's own checkout, when its definition asks for one.
    /// Dropped with the agent, which removes the checkout.
    pub(super) worktree: Option<Arc<Worktree>>,
}

/// Where an agent's result goes.
pub(super) enum Report {
    /// The waiting `agent`, `swarm` or resume call.
    Call(crossbeam_channel::Sender<ToolOutput>),
    /// The parent's inbox, read at its next turn boundary.
    Message,
}

impl AgentLink {
    /// Whether the agent's definition allows the tool `name`.
    pub(super) fn allows(&self, name: &str) -> bool {
        self.tools
            .as_deref()
            .is_none_or(|only| only.iter().any(|n| n == name))
    }
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
            fork,
            swarm,
            background,
            ..
        } = spawn;
        let child = swarm.as_ref().map(|info| info.id);
        let fail = |text: String| {
            let output = match child {
                Some(id) => agent_tool::refused(id, &agent, &text),
                None => agent_tool::error_output(text),
            };
            let _ = reply.send(output);
        };
        let (setup, depth) = match self.spawn_setup(parent, &agent) {
            Ok(found) => found,
            Err(text) => return fail(text),
        };
        let from = &self.sessions[&parent];
        let def = setup.defs.get(&agent).expect("checked by spawn_setup");
        let def = AgentDef {
            model: spawn.model.or_else(|| def.model.clone()),
            thinking: spawn.thinking.or(def.thinking),
            ..def.clone()
        };

        // A swarm call names its children up front so the tool can
        // cancel the ones that never reported.
        let (id, swarm) = match swarm {
            Some(info) => (info.id, Some(info)),
            None => (SessionId::new(), None),
        };
        let (worktree, workdir) = match child_workdir(from, &def, id, &description) {
            Ok(found) => found,
            Err(text) => return fail(text),
        };
        let (mut spec, missing, lazy_history) = if fork {
            let batch = swarm.as_ref().map(|info| &info.batch_id);
            let forked = fork_snapshot(&mut self.fork_snapshot, from, batch).and_then(|snapshot| {
                forked_spec(from, parent, id, &def, &setup, snapshot, &workdir)
            });
            match forked {
                Ok((spec, missing, path)) => (spec, missing, Some(path)),
                Err(text) => return fail(text),
            }
        } else {
            let (spec, missing) = agent_spec(from, parent, id, &def, &setup, &workdir);
            (spec, missing, None)
        };
        spec.cx.confine_paths |= worktree.is_some();
        let background = background && depth == 1 && setup.background != Background::Off;
        // A background agent outlives the run that started it, so the
        // parent's cancel must not reach it.
        let cancel = if background {
            CancelFlag::new()
        } else {
            from.cancel.child()
        };
        self.release_snapshot(swarm.as_ref());
        let batch_id = swarm.as_ref().map(|info| info.batch_id.clone());
        let (report, started) = if background {
            (Report::Message, Some(reply))
        } else {
            (Report::Call(reply), None)
        };
        let link = AgentLink {
            parent,
            agent: agent.clone(),
            depth,
            batch_id,
            report: Some(report),
            background,
            lazy_history,
            inherited_until: None,
            tools: def.tools.clone(),
            worktree,
        };
        let marker = session_marker(parent, &tool_call_id, &agent, &description, swarm.as_ref());
        let member = swarm.as_ref().map(swarm_member);
        let opened = Opened {
            id,
            parent,
            tool_call_id,
            agent: agent.clone(),
            description: description.clone(),
            swarm: member,
            background,
        };
        self.publish_agent_opened(opened);
        self.open(spec, cancel, Some(link));
        self.record_agent_entries(id, marker, description);
        self.warn_all(id, missing.iter().map(missing_tool).collect());
        let content = vec![Content::Text { text: prompt }];
        self.launch_agent(id, setup.max_running, content);
        if let Some(reply) = started {
            let _ = reply.send(agent_tool::started(id, &agent));
        }
    }

    /// Drop the parent conversation a forking swarm call copies once its
    /// last child has spawned.
    fn release_snapshot(&mut self, swarm: Option<&SwarmInfo>) {
        if swarm.is_some_and(|info| info.index + 1 >= info.total) {
            self.fork_snapshot = None;
        }
    }

    /// The setup and depth an agent `agent` of `parent` starts with, or
    /// why it cannot start.
    fn spawn_setup(&self, parent: SessionId, agent: &str) -> Result<(AgentSetup, u8), String> {
        let from = self
            .sessions
            .get(&parent)
            .ok_or_else(|| format!("session {parent} is gone"))?;
        let setup = from
            .agents
            .clone()
            .ok_or_else(|| "agents are turned off".to_owned())?;
        if setup.budget > 0 && from.spend.is_spent() {
            return Err(format!(
                "the agents used the agent budget of {} tokens since the user's last prompt; \
                 no agent starts until the user prompts again",
                setup.budget
            ));
        }
        let depth = depth_of(from) + 1;
        if depth > setup.max_depth {
            return Err(format!(
                "agents may nest {} level(s) deep (agent_max_depth)",
                setup.max_depth
            ));
        }
        if setup.defs.get(agent).is_none() {
            let names: Vec<&str> = setup.defs.iter().map(|d| d.name.as_str()).collect();
            return Err(format!(
                "unknown agent `{agent}`. Available agents: {}",
                names.join(", ")
            ));
        }
        Ok((setup, depth))
    }

    /// Publish the `AgentSpawned` event that opens a child's card.
    fn publish_agent_opened(&self, opened: Opened) {
        self.bus.publish(
            opened.id,
            HostEvent::AgentSpawned {
                parent: opened.parent,
                tool_call_id: opened.tool_call_id,
                agent: opened.agent,
                description: opened.description,
                swarm: opened.swarm,
                background: opened.background,
            },
        );
    }

    /// Start `content` as the first run of the agent session `id`, or
    /// queue it past the running limit.
    pub(super) fn launch_agent(&mut self, id: SessionId, max: usize, content: Vec<Content>) {
        if self.running_agents() < max {
            self.start_run(id, Work::Prompt(Message::new(Role::User, content, None)));
        } else {
            let session = self.sessions.get_mut(&id).expect("session opened");
            session.queued.push_back(content);
            self.waiting.push_back(id);
        }
    }

    /// Check that every id names a swarm child of `parent` before the
    /// caller attaches to any of them. Each child's session file must
    /// carry a `kage:agent` marker that names this parent and a batch.
    /// Returns the marker facts in the order asked, or the reason the
    /// whole call is refused.
    pub(super) fn verify_resume(
        &self,
        parent: SessionId,
        ids: &[SessionId],
    ) -> Result<Vec<ResumeChild>, String> {
        let dir = self
            .sessions
            .get(&parent)
            .and_then(|s| s.path.as_deref())
            .and_then(Path::parent)
            .ok_or_else(|| {
                "this session is not recorded, so it has no swarm children to resume".to_owned()
            })?;
        let mut children = Vec::with_capacity(ids.len());
        for id in ids {
            let marker = agent_marker(&dir.join(format!("{id}.jsonl")))
                .ok_or_else(|| format!("session {id} is not a swarm child of this session"))?;
            let text = |key: &str| {
                marker
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            if text("parent") != parent.to_string() || text("batch_id").is_empty() {
                return Err(format!("session {id} is not a swarm child of this session"));
            }
            let number = |key: &str| {
                marker
                    .get(key)
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
            };
            children.push(ResumeChild {
                id: *id,
                item: text("item"),
                agent: text("agent"),
                description: text("description"),
                tool_call_id: ToolCallId::new(text("tool_call_id")),
                batch_id: ToolCallId::new(text("batch_id")),
                index: number("index"),
                total: number("total"),
            });
        }
        Ok(children)
    }

    /// Re-prompt an existing swarm child with a follow-up. A hosted
    /// child gets its reply channel re-armed and the prompt queued or
    /// started. A child no longer hosted (the parent's session was
    /// resumed or the engine restarted) is opened from its session
    /// file first, keeping its original agent definition and history.
    pub(super) fn attach(&mut self, attach: Attach) {
        let Attach {
            parent,
            id,
            agent,
            description,
            tool_call_id,
            prompt,
            reply,
            swarm,
        } = attach;
        let fail = |text: String| {
            let _ = reply.send(agent_tool::refused(id, &agent, &text));
        };
        let Some(setup) = self.sessions.get(&parent).and_then(|s| s.agents.clone()) else {
            return fail("agents are turned off".to_owned());
        };
        let reopened = !self.sessions.contains_key(&id);
        if reopened {
            match self.reopen_agent(parent, id) {
                Ok(warnings) => self.warn_all(id, warnings),
                Err(text) => return fail(text),
            }
        }
        let session = self.sessions.get_mut(&id).expect("hosted above");
        let Some(link) = session.link.as_mut() else {
            return fail(format!("session {id} is not a swarm child of this session"));
        };
        if link.parent != parent || link.batch_id.is_none() {
            return fail(format!("session {id} is not a swarm child of this session"));
        }
        if session.idle.is_none() || link.report.is_some() || self.waiting.contains(&id) {
            return fail(format!(
                "session {id} is still working on an earlier call; resume it once that \
                 call has its result"
            ));
        }
        link.report = Some(Report::Call(reply));
        if reopened {
            self.publish_agent_opened(Opened {
                id,
                parent,
                tool_call_id,
                agent,
                description,
                swarm: swarm.as_ref().map(swarm_member),
                background: false,
            });
        }
        let content = vec![Content::Text { text: prompt }];
        self.launch_agent(id, setup.max_running, content);
    }

    /// Host the swarm child `id` of the hosted session `parent` again
    /// from its session file, for a swarm resume. Agents are dropped
    /// once their result is delivered, and a resumed session hosts
    /// none. The child keeps its definition, history and model, appends
    /// to its own file and owes no call a result until the caller arms
    /// one. Returns the warnings to show on it.
    fn reopen_agent(&mut self, parent: SessionId, id: SessionId) -> Result<Vec<String>, String> {
        let no_session = || format!("session {id} is not a swarm child of this session");
        let path = self
            .sessions
            .get(&parent)
            .and_then(|s| s.path.as_deref())
            .and_then(Path::parent)
            .map(|dir| crate::build_session_path(dir, id))
            .ok_or_else(no_session)?;
        let marker = agent_marker(&path).ok_or_else(no_session)?;
        let text = |key: &str| {
            marker
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        if text("parent") != parent.to_string() {
            return Err(no_session());
        }
        let replay = kage_session::replay(&path)
            .map_err(|err| format!("cannot read session {id}: {err}"))?;
        if replay.header.session != id {
            return Err(no_session());
        }
        let writer = kage_session::SessionWriter::open(&path)
            .map_err(|err| format!("cannot append to session {id}: {err}"))?;
        let from = &self.sessions[&parent];
        let setup = from
            .agents
            .clone()
            .ok_or_else(|| "agents are turned off".to_owned())?;
        let agent = text("agent");
        let def = setup.defs.get(&agent).ok_or_else(|| {
            format!("agent definition `{agent}` is gone; cannot reopen session {id}")
        })?;
        if def.isolation == Isolation::Worktree {
            return Err(format!(
                "session {id} worked in a worktree that was removed. Start a new agent."
            ));
        }
        let (spec, missing, note) =
            resumed_spec(from, id, replay, def, &setup, writer, &self.registry);
        let batch_id = Some(text("batch_id"))
            .filter(|batch| !batch.is_empty())
            .map(ToolCallId::new);
        let link = AgentLink {
            parent,
            agent,
            depth: depth_of(from) + 1,
            batch_id,
            report: None,
            background: false,
            lazy_history: None,
            inherited_until: None,
            tools: def.tools.clone(),
            worktree: None,
        };
        let cancel = from.cancel.child();
        self.open(spec, cancel, Some(link));
        let mut warnings: Vec<String> = note.into_iter().collect();
        warnings.extend(missing.iter().map(missing_tool));
        Ok(warnings)
    }

    /// Show each of `warnings` on session `id`.
    pub(super) fn warn_all(&self, id: SessionId, warnings: Vec<String>) {
        for text in warnings {
            notice(&self.bus, id, NoticeLevel::Warning, text);
        }
    }

    /// Continue the named swarm children of `parent`, the engine answer
    /// to a client's `_kage/swarm/resume`. Every child is checked first,
    /// and one that is not a swarm child of `parent` or is still working
    /// refuses the whole request. Each child reports under the call that
    /// first spawned it, in its old place, so a client's card keeps it.
    /// Once all have reported, [`Self::swarm_resumed`] tells `parent`.
    pub(super) fn resume_members(
        &mut self,
        parent: SessionId,
        members: &BTreeMap<SessionId, String>,
    ) -> Result<Vec<SessionId>, String> {
        if members.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<SessionId> = members.keys().copied().collect();
        let children = self.verify_resume(parent, &ids)?;
        if let Some(busy) = ids.iter().find(|id| self.is_busy(**id)) {
            return Err(format!(
                "session {busy} is still working on an earlier call; resume it once that \
                 call has its result"
            ));
        }
        let (reply, results) = crossbeam_channel::bounded(children.len());
        let mut resumed = Vec::with_capacity(children.len());
        let description = children
            .first()
            .map(|child| child.description.clone())
            .unwrap_or_default();
        for child in children {
            let prompt = members
                .get(&child.id)
                .filter(|prompt| !prompt.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| CONTINUE_PROMPT.to_owned());
            let swarm = child
                .index
                .zip(child.total)
                .map(|(index, total)| SwarmInfo {
                    id: child.id,
                    batch_id: child.batch_id.clone(),
                    index,
                    item: child.item.clone(),
                    total,
                });
            resumed.push(Member {
                id: child.id,
                item: child.item,
                agent: child.agent.clone(),
            });
            self.attach(Attach {
                parent,
                id: child.id,
                agent: child.agent,
                description: child.description,
                tool_call_id: child.tool_call_id,
                prompt,
                reply: reply.clone(),
                swarm,
            });
        }
        drop(reply);
        notice(
            &self.bus,
            parent,
            NoticeLevel::Info,
            format!("resuming {} swarm member(s)", resumed.len()),
        );
        let accepted = resumed.iter().map(|member| member.id).collect();
        let engine = self.tx.clone();
        std::thread::spawn(move || {
            let outputs: Vec<ToolOutput> = results.iter().collect();
            let _ = engine.send(super::Input::SwarmResumed(Box::new(SwarmResumed {
                parent,
                description,
                members: resumed,
                outputs,
            })));
        });
        Ok(accepted)
    }

    /// Whether the agent `id` is hosted and still owes a call its
    /// result or has work in flight.
    fn is_busy(&self, id: SessionId) -> bool {
        self.sessions.get(&id).is_some_and(|session| {
            session.idle.is_none()
                || session
                    .link
                    .as_ref()
                    .is_some_and(|link| link.report.is_some())
                || self.waiting.contains(&id)
        })
    }

    /// Tell `parent` how the children a client resumed did: a notice
    /// with the counts, and their results as a note the parent's next
    /// run reads, so the conversation learns what the retry changed.
    pub(super) fn swarm_resumed(&mut self, resumed: SwarmResumed) {
        let SwarmResumed {
            parent,
            description,
            members,
            outputs,
        } = resumed;
        let Some(session) = self.sessions.get_mut(&parent) else {
            return;
        };
        let aggregate =
            swarm_tool::render(swarm_tool::RESULT_CAP, &description, &members, &outputs);
        let summary = aggregate.text.lines().next().unwrap_or_default().to_owned();
        let text = format!(
            "[swarm resume] The user continued members of an earlier swarm. Their \
             results:\n{}",
            aggregate.text
        );
        session
            .pending_history
            .push(Message::new(Role::User, vec![Content::Text { text }], None));
        let level = if aggregate.is_error {
            NoticeLevel::Warning
        } else {
            NoticeLevel::Info
        };
        notice(
            &self.bus,
            parent,
            level,
            format!("resumed swarm members finished: {summary}"),
        );
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

    /// Send an agent's result where it goes, once. Callers publish the
    /// agent's `RunEnded` first, so clients see the agent end before
    /// the parent continues. `usage` and `run_time` are the agent's
    /// final totals; a call that never ran a turn passes the defaults.
    pub(super) fn deliver(
        &mut self,
        id: SessionId,
        outcome: &RunOutcome,
        history: &[Arc<Message>],
        usage: Usage,
        run_time: Duration,
    ) {
        if let Some(taken) = self.take_report(id, outcome, history, usage, run_time) {
            self.send_report(taken);
        }
    }

    /// Take an agent's result and where it goes, once, to send later.
    pub(super) fn take_report(
        &mut self,
        id: SessionId,
        outcome: &RunOutcome,
        history: &[Arc<Message>],
        usage: Usage,
        run_time: Duration,
    ) -> Option<Taken> {
        let session = self.sessions.get_mut(&id)?;
        let model = &session.state.model;
        let link = session.link.as_mut()?;
        let report = link.report.take()?;
        let facts = RunFacts {
            usage,
            run_time,
            limit: session.limit.take(),
            note: session.note.take(),
        };
        self.swarm_requeues.remove(&id);
        let own = link
            .inherited_until
            .and_then(|last| history.iter().position(|m| m.id == last))
            .map_or(history, |at| &history[at + 1..]);
        Some(Taken {
            parent: link.parent,
            report,
            wake: *outcome != RunOutcome::Cancelled || facts.limit.is_some(),
            output: agent_tool::agent_result(id, &link.agent, model, outcome, own, &facts),
        })
    }

    /// Send a taken result: to the waiting call, or into the parent's
    /// inbox. A parent that wakes and is idle starts a run to read it.
    pub(super) fn send_report(&mut self, taken: Taken) {
        let Taken {
            parent,
            report,
            output,
            wake,
        } = taken;
        match report {
            Report::Call(reply) => {
                let _ = reply.send(output);
            }
            Report::Message => {
                let Some(session) = self.sessions.get(&parent) else {
                    return;
                };
                lock(&session.inbox).push_back(output.text);
                if wake {
                    self.wake(parent);
                }
            }
        }
    }

    /// Start a run of the idle session `id` that reads its inbox: an
    /// agent always, within the running limit, and a main session when
    /// its setup wakes for agent text. Returns whether a run starts or
    /// waits for a slot.
    pub(super) fn wake(&mut self, id: SessionId) -> bool {
        let Some(session) = self.sessions.get(&id) else {
            return false;
        };
        let wakes = session.link.is_some()
            || session
                .agents
                .as_ref()
                .is_some_and(|setup| setup.background == Background::Wake);
        let idle = session.idle.is_some()
            && !self.waiting.contains(&id)
            && !self.swarm_requeues.contains_key(&id);
        if !wakes || !idle || self.shutting_down {
            return false;
        }
        let entries: Vec<String> = lock(&session.inbox).drain(..).collect();
        if entries.is_empty() {
            return false;
        }
        let content = vec![Content::Text {
            text: entries.join("\n\n"),
        }];
        match session.agents.as_ref().filter(|_| session.link.is_some()) {
            Some(setup) => {
                let max = setup.max_running;
                self.launch_agent(id, max, content);
            }
            None => self.start_run(id, Work::Prompt(Message::new(Role::User, content, None))),
        }
        true
    }

    /// Agent runs in flight that hold a slot of the running limit. An
    /// agent waiting on its own agents holds none, so nesting cannot
    /// deadlock the limit.
    pub(super) fn running_agents(&self) -> usize {
        let waits_on_agents = |id: &SessionId| {
            self.sessions.values().any(|s| {
                s.link
                    .as_ref()
                    .is_some_and(|l| l.parent == *id && matches!(l.report, Some(Report::Call(_))))
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
        self.deliver(
            id,
            &RunOutcome::Cancelled,
            &[],
            Usage::default(),
            Duration::ZERO,
        );
    }

    /// End an idle agent that still owes its call a result, such as a
    /// swarm child backing off a rate limit, as cancelled. Its pending
    /// requeue then finds nothing to continue.
    pub(super) fn end_paused(&mut self, id: SessionId) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        let Some(idle) = session
            .idle
            .as_ref()
            .filter(|_| session.link.as_ref().is_some_and(|l| l.report.is_some()))
        else {
            return;
        };
        let history = idle.cx.history.clone();
        let usage = session.usage;
        self.bus.publish(
            id,
            HostEvent::RunEnded {
                outcome: RunOutcome::Cancelled,
            },
        );
        self.deliver(id, &RunOutcome::Cancelled, &history, usage, Duration::ZERO);
        self.reap_agent(id);
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

/// The session an agent of `from` runs in, in `workdir`: the
/// definition's model, thinking, role and tools over `from`'s, `from`'s
/// gate and loop settings, no plugins or MCP of its own, and a file
/// next to `from`'s when `from` records. Also returns listed tools that
/// match nothing.
fn agent_spec(
    from: &Session,
    parent: SessionId,
    id: SessionId,
    def: &AgentDef,
    setup: &AgentSetup,
    workdir: &Path,
) -> (SessionSpec, Vec<String>) {
    let model = def
        .model
        .clone()
        .unwrap_or_else(|| from.state.model.clone());
    let system_prompt = crate::runtime_env::build_system_prompt(
        &def.body,
        workdir,
        &model,
        &[],
        from.shell.as_deref(),
    );
    let mut cx = AgentContext::new(model.clone(), &system_prompt).with_workdir(workdir);
    cx.confine_paths = from.confine_paths;
    cx.thinking_level = def.thinking.or(from.state.thinking);
    let (tools, missing) = agent_tools(&from.tools, def.tools.as_deref());
    let recorder = from.path.as_deref().and_then(Path::parent).map(|dir| {
        let header = kage_session::Header {
            version: kage_session::FORMAT_VERSION,
            session: id,
            id: kage_session::EntryId::new(),
            ts: chrono::Utc::now(),
            cwd: workdir.to_path_buf(),
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
        shell: from.shell.clone(),
    };
    (spec, missing)
}

/// A checkout of its own for the agent `id` of `from`, when its
/// definition asks for one, under the state directory.
fn checkout(
    from: &Session,
    def: &AgentDef,
    id: SessionId,
    description: &str,
) -> Result<Option<Arc<Worktree>>, String> {
    if def.isolation != Isolation::Worktree {
        return Ok(None);
    }
    let dir = crate::paths::state_root()?.join("worktrees");
    let label = format!("{}: {description}", def.name);
    Worktree::create(&from.workdir, &dir, id, label).map(|w| Some(Arc::new(w)))
}

/// The worktree of the agent `id` of `from`, when its definition asks
/// for one, and the directory the agent works in.
fn child_workdir(
    from: &Session,
    def: &AgentDef,
    id: SessionId,
    description: &str,
) -> Result<(Option<Arc<Worktree>>, PathBuf), String> {
    let worktree = checkout(from, def, id, description)?;
    let workdir = worktree
        .as_ref()
        .map_or_else(|| from.workdir.clone(), |w| w.workdir().to_path_buf());
    Ok((worktree, workdir))
}

/// The `kage:agent` marker an agent's session file starts with.
fn session_marker(
    parent: SessionId,
    tool_call_id: &ToolCallId,
    agent: &str,
    description: &str,
    swarm: Option<&SwarmInfo>,
) -> serde_json::Value {
    let mut marker = serde_json::json!({
        "parent": parent,
        "tool_call_id": tool_call_id,
        "agent": agent,
        "description": description,
    });
    if let Some(info) = swarm {
        marker["batch_id"] = serde_json::Value::String(info.batch_id.0.clone());
        marker["index"] = serde_json::Value::from(info.index);
        marker["total"] = serde_json::Value::from(info.total);
        marker["item"] = serde_json::Value::String(info.item.clone());
    }
    marker
}

/// An agent's result, taken from its link, and where it goes.
pub(super) struct Taken {
    parent: SessionId,
    report: Report,
    output: ToolOutput,
    /// Whether a result for the parent's inbox may start a run of an
    /// idle parent. The result of an agent the user stopped waits for
    /// the next one.
    wake: bool,
}

/// The facts of an agent's `AgentSpawned` event.
struct Opened {
    id: SessionId,
    parent: SessionId,
    tool_call_id: ToolCallId,
    agent: String,
    description: String,
    swarm: Option<SwarmMember>,
    background: bool,
}

/// The card facts of a swarm child.
fn swarm_member(info: &SwarmInfo) -> SwarmMember {
    SwarmMember {
        batch: Some(info.batch_id.clone()),
        item: info.item.clone(),
        index: u32::try_from(info.index).unwrap_or(u32::MAX),
        total: u32::try_from(info.total).unwrap_or(u32::MAX),
    }
}

/// The warning for a listed tool that matches no tool of the parent.
fn missing_tool(name: &String) -> String {
    format!("agent tools: no tool named `{name}`")
}

/// 0 for a main session, 1 for its agents, and so on.
pub(super) fn depth_of(session: &Session) -> u8 {
    session.link.as_ref().map_or(0, |l| l.depth)
}

/// What a forked child copies from `from`'s session file: every entry
/// up to the latest message that is not an assistant's tool call, so
/// the copy never ends on a call that was never answered. Read once
/// per swarm call: the children of `batch` reuse `cache`.
fn fork_snapshot<'a>(
    cache: &'a mut Option<(ToolCallId, kage_session::Snapshot)>,
    from: &Session,
    batch: Option<&ToolCallId>,
) -> Result<&'a kage_session::Snapshot, String> {
    let Some(src) = from.path.as_deref() else {
        return Err(
            "cannot fork: this session is not recorded, so there is no conversation \
             to snapshot"
                .to_owned(),
        );
    };
    if cache
        .as_ref()
        .is_some_and(|(cached, _)| Some(cached) == batch)
    {
        return Ok(&cache.as_ref().expect("checked above").1);
    }
    let answered = |entry: &kage_session::SessionEntry| match entry {
        kage_session::SessionEntry::Message(message) => {
            message.message.role != Role::Assistant
                || !message
                    .message
                    .content
                    .iter()
                    .any(|c| matches!(c, Content::ToolCall { .. }))
        }
        _ => false,
    };
    let snapshot = kage_session::snapshot(src, answered)
        .map_err(|err| format!("cannot fork: the session file could not be read: {err}"))?;
    let batch = batch.cloned().unwrap_or_else(|| ToolCallId::new(""));
    Ok(&cache.insert((batch, snapshot)).1)
}

/// The session spec for a child spawned from `snapshot`, a copy of
/// `from`'s conversation, instead of zero context. The snapshot is
/// written into the child's own file, and the child's context history
/// and token budget load from that copy at the first run start
/// ([`AgentLink::lazy_history`]), so a child queued behind the running
/// limit holds kilobytes instead of the whole snapshot. Model, system
/// prompt, thinking level and tools still follow the definition.
/// Returns the child's file path for the lazy load.
fn forked_spec(
    from: &Session,
    parent: SessionId,
    id: SessionId,
    def: &AgentDef,
    setup: &AgentSetup,
    snapshot: &kage_session::Snapshot,
    workdir: &Path,
) -> Result<(SessionSpec, Vec<String>, PathBuf), String> {
    let dir = from
        .path
        .as_deref()
        .and_then(Path::parent)
        .ok_or_else(|| "cannot fork: the session file has no directory".to_owned())?;
    let model = def
        .model
        .clone()
        .unwrap_or_else(|| from.state.model.clone());
    let system_prompt = crate::runtime_env::build_system_prompt(
        &def.body,
        workdir,
        &model,
        &[],
        from.shell.as_deref(),
    );
    let child_path = crate::build_session_path(dir, id);
    let header = kage_session::Header {
        version: kage_session::FORMAT_VERSION,
        session: id,
        id: kage_session::EntryId::new(),
        ts: chrono::Utc::now(),
        cwd: workdir.to_path_buf(),
        model: model.clone(),
        system_prompt: system_prompt.clone(),
        parent_session: Some(parent),
        parent_entry: Some(snapshot.at),
    };
    let mut writer = snapshot
        .write(&child_path, header)
        .map_err(|err| format!("cannot fork into session {id}: {err}"))?;
    if !snapshot.is_empty() {
        let notice = kage_session::SessionEntry::Message(kage_session::MessageEntry {
            id: kage_session::EntryId::new(),
            ts: chrono::Utc::now(),
            message: Arc::new(Message::new(
                Role::User,
                vec![Content::Text {
                    text: FORK_CONTEXT_NOTICE.to_owned(),
                }],
                None,
            )),
            usage: None,
        });
        writer
            .append(&notice)
            .map_err(|err| format!("cannot write the fork notice into session {id}: {err}"))?;
    }
    let mut cx =
        AgentContext::new(model.clone(), system_prompt).with_workdir(workdir.to_path_buf());
    cx.confine_paths = from.confine_paths;
    cx.thinking_level = def.thinking.or(from.state.thinking);
    let (tools, missing) = agent_tools(&from.tools, def.tools.as_deref());
    let spec = SessionSpec {
        id,
        model,
        cx,
        recorder: Some(Recorder::new(writer, None)),
        tools,
        plugins: None,
        gate: from.gate.clone(),
        loop_cfg: from.loop_cfg,
        mcp: None,
        interactive: from.interactive,
        title: false,
        agents: Some(setup.clone()),
        shell: from.shell.clone(),
    };
    Ok((spec, missing, child_path))
}

/// The `kage:agent` marker data of the session file at `path`, from
/// the first marker entry in the file. `None` when the file carries no
/// marker, which includes a missing file. The marker sits right after
/// the header for an ordinary child, but after the copied history for
/// a forked one, so the file is scanned to the end.
fn agent_marker(path: &Path) -> Option<serde_json::Value> {
    let reader = kage_session::SessionReader::iter(path).ok()?;
    for entry in reader {
        let Ok(kage_session::SessionEntry::Custom(custom)) = entry else {
            continue;
        };
        if custom.kind == kage_session::list::AGENT_ENTRY_KIND {
            return Some(custom.data);
        }
    }
    None
}

/// The session spec that re-opens a swarm child from its session
/// file: the recorded model, or the parent's when the recorded one is
/// unavailable (with the note to show), the recorded system prompt,
/// thinking level, history and usage, and the definition's tools over
/// the parent's current tools. The child keeps appending to its own
/// session file.
fn resumed_spec(
    from: &Session,
    id: SessionId,
    replay: kage_session::ReplayResult,
    def: &AgentDef,
    setup: &AgentSetup,
    writer: kage_session::SessionWriter,
    registry: &ProviderRegistry,
) -> (SessionSpec, Vec<String>, Option<String>) {
    let (model, note) = if registry.resolve(&replay.model).is_ok() {
        (replay.model.clone(), None)
    } else {
        (
            from.state.model.clone(),
            Some(format!(
                "session model {} unavailable; using {} instead",
                replay.model, from.state.model
            )),
        )
    };
    let mut cx = AgentContext::new(model.clone(), replay.header.system_prompt.clone())
        .with_workdir(from.workdir.clone());
    cx.confine_paths = from.confine_paths;
    cx.thinking_level = replay
        .thinking_level
        .as_deref()
        .and_then(kage_core::ThinkingLevel::parse);
    cx.history = replay.history.into_iter().map(Arc::new).collect();
    cx.budget = TokenBudget {
        used_input: replay.usage_total.input,
        used_output: replay.usage_total.output,
        used_cache_read: replay.usage_total.cache_read,
        used_cache_write: replay.usage_total.cache_write,
        current_context: replay.usage_total.last_context,
    };
    let (tools, missing) = agent_tools(&from.tools, def.tools.as_deref());
    let spec = SessionSpec {
        id,
        model,
        cx,
        recorder: Some(Recorder::new(writer, None)),
        tools,
        plugins: None,
        gate: from.gate.clone(),
        loop_cfg: from.loop_cfg,
        mcp: None,
        interactive: from.interactive,
        title: false,
        agents: Some(setup.clone()),
        shell: from.shell.clone(),
    };
    (spec, missing, note)
}

/// The tools an agent gets: `parent`'s, narrowed to `only` when the
/// definition lists tools. Also returns listed names that match nothing.
///
/// Narrowing goes through [`ToolRegistry::retain_named`] so aliases
/// survive; a sub-agent whose model calls `bash` needs the alias as
/// much as the main session does.
fn agent_tools(parent: &ToolRegistry, only: Option<&[String]>) -> (ToolRegistry, Vec<String>) {
    let Some(only) = only else {
        return (parent.clone(), Vec::new());
    };
    let (tools, mut missing) = parent.retain_named(only);
    missing.retain(|name| ![AGENT_TOOL, SWARM_TOOL, MAILBOX_TOOL].contains(&name.as_str()));
    (tools, missing)
}
