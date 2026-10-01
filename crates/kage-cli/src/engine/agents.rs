//! Agent sessions an `agent` call starts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kage_core::agents::AgentDef;
use kage_core::protocol::{HostEvent, NoticeLevel, RunOutcome, SwarmMember, Usage};
use kage_core::sync::lock;
use kage_core::{Content, Message, MessageId, Role, SessionId, ToolCallId, ToolOutput};
use kage_loop::{AgentContext, TokenBudget};
use kage_provider::ProviderRegistry;
use kage_tools::ToolRegistry;

use super::agent_tool::{self, AGENT_TOOL, Spawn};
use super::mailbox_tool::MAILBOX_TOOL;
use super::runner::Work;
use super::swarm_tool::{SWARM_TOOL, SwarmInfo};
use super::{
    AgentSetup, Attach, CONTINUE_PROMPT, Recorder, ResumeChild, Session, SessionSpec, notice,
};

/// Tells a forked child that the conversation it starts with is
/// inherited reference material, not its own past. Ported from
/// kimi-code's `FORK_CONTEXT_NOTICE`.
const FORK_CONTEXT_NOTICE: &str = "The conversation above is not your own history. It is a \
snapshot inherited from the session that forked you, so treat it as reference material only. \
You are an independent agent, not a continuation of that agent. Do the task in the next message \
yourself, then report the result.";

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
    /// Delivers the result to the waiting `agent` call. Taken by the
    /// first run that finishes, so later runs a user starts in the
    /// agent never answer the parent twice.
    pub(super) reply: Option<crossbeam_channel::Sender<ToolOutput>>,
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
        } = spawn;
        let child = swarm.as_ref().map(|info| info.id);
        let fail = |text: String| {
            let output = match child {
                Some(id) => agent_tool::refused(id, &agent, &text),
                None => agent_tool::error_output(text),
            };
            let _ = reply.send(output);
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

        // A swarm call names its children up front so the tool can
        // cancel the ones that never reported.
        let (id, swarm) = match swarm {
            Some(info) => (info.id, Some(info)),
            None => (SessionId::new(), None),
        };
        let (spec, missing, lazy_history) = if fork {
            match forked_spec(from, parent, id, def, &setup) {
                Ok((spec, missing, path)) => (spec, missing, Some(path)),
                Err(text) => return fail(text),
            }
        } else {
            let (spec, missing) = agent_spec(from, parent, id, def, &setup);
            (spec, missing, None)
        };
        let cancel = from.cancel.child();
        let batch_id = swarm.as_ref().map(|info| info.batch_id.clone());
        let link = AgentLink {
            parent,
            agent: agent.clone(),
            depth,
            batch_id,
            reply: Some(reply),
            lazy_history,
            inherited_until: None,
            tools: def.tools.clone(),
        };
        let mut marker = serde_json::json!({
            "parent": parent,
            "tool_call_id": tool_call_id,
            "agent": agent,
            "description": description,
        });
        if let Some(info) = &swarm {
            marker["batch_id"] = serde_json::Value::String(info.batch_id.0.clone());
            marker["index"] = serde_json::Value::from(info.index);
            marker["item"] = serde_json::Value::String(info.item.clone());
        }

        let max = setup.max_running;
        let member = swarm.as_ref().map(|info| SwarmMember {
            batch: Some(info.batch_id.clone()),
            item: info.item.clone(),
            index: u32::try_from(info.index).unwrap_or(u32::MAX),
            total: u32::try_from(info.total).unwrap_or(u32::MAX),
        });
        self.publish_agent_opened(id, parent, tool_call_id, agent, description.clone(), member);
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
        self.launch_agent(id, max, content);
    }

    /// Publish the `AgentSpawned` event that opens a child's card.
    fn publish_agent_opened(
        &self,
        id: SessionId,
        parent: SessionId,
        tool_call_id: ToolCallId,
        agent: String,
        description: String,
        swarm: Option<SwarmMember>,
    ) {
        self.bus.publish(
            id,
            HostEvent::AgentSpawned {
                parent,
                tool_call_id,
                agent,
                description,
                swarm,
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
            children.push(ResumeChild {
                id: *id,
                item: text("item"),
                agent: text("agent"),
                description: text("description"),
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
            batch_id,
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
        let content = vec![Content::Text { text: prompt }];
        if let Some(session) = self.sessions.get_mut(&id) {
            let Some(link) = session.link.as_mut() else {
                return fail(format!("session {id} is not a swarm child of this session"));
            };
            if link.parent != parent || link.batch_id.is_none() {
                return fail(format!("session {id} is not a swarm child of this session"));
            }
            if session.idle.is_none() || link.reply.is_some() || self.waiting.contains(&id) {
                return fail(format!(
                    "session {id} is still working on an earlier call; resume it once that \
                     call has its result"
                ));
            }
            link.reply = Some(reply);
            let max = setup.max_running;
            self.launch_agent(id, max, content);
            return;
        }
        // Not hosted: open the child from its session file.
        let Some(dir) = self
            .sessions
            .get(&parent)
            .and_then(|s| s.path.as_deref())
            .and_then(Path::parent)
            .map(Path::to_path_buf)
        else {
            return fail(format!("session {id} is not a swarm child of this session"));
        };
        let path = dir.join(format!("{id}.jsonl"));
        let replay = match kage_session::replay(&path) {
            Ok(replay) => replay,
            Err(err) => return fail(format!("cannot read session {id}: {err}")),
        };
        if replay.header.session != id {
            return fail(format!("session {id} is not a swarm child of this session"));
        }
        let writer = match kage_session::SessionWriter::open(&path) {
            Ok(writer) => writer,
            Err(err) => return fail(format!("cannot append to session {id}: {err}")),
        };
        let opened = {
            let from = self.sessions.get(&parent).expect("checked above");
            let Some(def) = setup.defs.get(&agent) else {
                return fail(format!(
                    "agent definition `{agent}` is gone; cannot resume session {id}"
                ));
            };
            let (spec, missing, note) =
                resumed_spec(from, id, &replay, def, &setup, writer, &self.registry);
            let cancel = from.cancel.child();
            let link = AgentLink {
                parent,
                agent: agent.clone(),
                depth: depth_of(from) + 1,
                batch_id: Some(batch_id.clone()),
                reply: Some(reply),
                lazy_history: None,
                inherited_until: None,
                tools: def.tools.clone(),
            };
            (spec, missing, note, cancel, link)
        };
        let (spec, missing, note, cancel, link) = opened;
        let member = swarm.as_ref().map(|info| SwarmMember {
            batch: Some(info.batch_id.clone()),
            item: info.item.clone(),
            index: u32::try_from(info.index).unwrap_or(u32::MAX),
            total: u32::try_from(info.total).unwrap_or(u32::MAX),
        });
        self.publish_agent_opened(id, parent, batch_id, agent, description, member);
        self.open(spec, cancel, Some(link));
        if let Some(note) = note {
            notice(&self.bus, id, NoticeLevel::Warning, note);
        }
        for name in missing {
            notice(
                &self.bus,
                id,
                NoticeLevel::Warning,
                format!("agent tools: no tool named `{name}`"),
            );
        }
        let max = setup.max_running;
        self.launch_agent(id, max, content);
    }

    /// Continue the named swarm children of `id`, the engine answer to
    /// a `_kage/swarm/resume` request. Each verified child joins a
    /// fresh batch and runs again; a child that is not a swarm member
    /// of `id` refuses the whole request with a notice.
    pub(super) fn resume_members(&mut self, id: SessionId, members: &BTreeMap<SessionId, String>) {
        if members.is_empty() {
            return;
        }
        let ids: Vec<SessionId> = members.keys().copied().collect();
        let children = match self.verify_resume(id, &ids) {
            Ok(children) => children,
            Err(text) => {
                notice(&self.bus, id, NoticeLevel::Warning, text);
                return;
            }
        };
        let batch_id = ToolCallId::new(format!("swarm_{}", ulid::Ulid::generate()));
        let total = children.len();
        notice(
            &self.bus,
            id,
            NoticeLevel::Info,
            format!("resuming {} swarm member(s)", children.len()),
        );
        for (index, child) in children.into_iter().enumerate() {
            let prompt = members
                .get(&child.id)
                .filter(|prompt| !prompt.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| CONTINUE_PROMPT.to_owned());
            let (reply, _result) = crossbeam_channel::bounded(1);
            self.attach(Attach {
                parent: id,
                id: child.id,
                agent: child.agent,
                description: child.description,
                batch_id: batch_id.clone(),
                prompt,
                reply,
                swarm: Some(SwarmInfo {
                    id: child.id,
                    batch_id: batch_id.clone(),
                    index,
                    item: child.item,
                    total,
                }),
            });
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
        if let Some((reply, output)) = self.take_reply(id, outcome, history, usage, run_time) {
            let _ = reply.send(output);
        }
    }

    /// Take an agent's `agent` call reply and its result, once, to send
    /// later.
    pub(super) fn take_reply(
        &mut self,
        id: SessionId,
        outcome: &RunOutcome,
        history: &[Arc<Message>],
        usage: Usage,
        run_time: Duration,
    ) -> Option<(crossbeam_channel::Sender<ToolOutput>, ToolOutput)> {
        let link = self.sessions.get_mut(&id)?.link.as_mut()?;
        let reply = link.reply.take()?;
        self.swarm_requeues.remove(&id);
        let own = link
            .inherited_until
            .and_then(|last| history.iter().position(|m| m.id == last))
            .map_or(history, |at| &history[at + 1..]);
        Some((
            reply,
            agent_tool::agent_result(id, &link.agent, outcome, own, &usage, run_time),
        ))
    }

    /// Agent runs in flight that hold a slot of the running limit. An
    /// agent waiting on its own agents holds none, so nesting cannot
    /// deadlock the limit.
    pub(super) fn running_agents(&self) -> usize {
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
            .filter(|_| session.link.as_ref().is_some_and(|l| l.reply.is_some()))
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
        self.reap_swarm_child(id);
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
    let system_prompt = crate::runtime_env::build_system_prompt(
        &def.body,
        &from.workdir,
        &model,
        &[],
        from.shell.as_deref(),
    );
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
        shell: from.shell.clone(),
    };
    (spec, missing)
}

/// 0 for a main session, 1 for its agents, and so on.
pub(super) fn depth_of(session: &Session) -> u8 {
    session.link.as_ref().map_or(0, |l| l.depth)
}

/// The entry a forked child copies up to, and whether any conversation
/// was copied at all: the parent's latest entry the copy may end on.
/// An assistant message carrying tool calls is skipped, so the copied
/// history never ends on a tool call that was never answered. The
/// header id when no message qualifies, which forks an empty
/// conversation.
fn fork_point(path: &Path) -> Option<(kage_session::EntryId, bool)> {
    let reader = kage_session::SessionReader::iter(path).ok()?;
    let mut header_id = None;
    let mut at = None;
    for entry in reader {
        let entry = entry.ok()?;
        match &entry {
            kage_session::SessionEntry::Header(header) => header_id = Some(header.id),
            kage_session::SessionEntry::Message(message) => {
                let calls_tools = message.message.role == Role::Assistant
                    && message
                        .message
                        .content
                        .iter()
                        .any(|c| matches!(c, Content::ToolCall { .. }));
                if !calls_tools {
                    at = Some(entry.id());
                }
            }
            _ => {}
        }
    }
    match at {
        Some(at) => Some((at, true)),
        None => header_id.map(|id| (id, false)),
    }
}

/// The session spec for a child spawned from a snapshot of `from`'s
/// conversation instead of zero context: `from`'s session file is
/// forked into the child's own file up to [`fork_point`], and the
/// child's context history and token budget load from that copy at
/// the first run start ([`AgentLink::lazy_history`]), so a child
/// queued behind the running limit holds kilobytes instead of the
/// whole snapshot. Model, system prompt, thinking level and tools
/// still follow the definition. Errors when `from` does not record,
/// or the fork fails. Returns the child's file path for the lazy
/// load.
fn forked_spec(
    from: &Session,
    parent: SessionId,
    id: SessionId,
    def: &AgentDef,
    setup: &AgentSetup,
) -> Result<(SessionSpec, Vec<String>, PathBuf), String> {
    let Some(src) = from.path.as_deref() else {
        return Err(
            "cannot fork: this session is not recorded, so there is no conversation \
             to snapshot"
                .to_owned(),
        );
    };
    let dir = src
        .parent()
        .ok_or_else(|| "cannot fork: the session file has no directory".to_owned())?;
    let (at, copied) = fork_point(src)
        .ok_or_else(|| "cannot fork: the session file could not be read".to_owned())?;
    let model = def
        .model
        .clone()
        .unwrap_or_else(|| from.state.model.clone());
    let system_prompt = crate::runtime_env::build_system_prompt(
        &def.body,
        &from.workdir,
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
        cwd: from.workdir.clone(),
        model: model.clone(),
        system_prompt: system_prompt.clone(),
        parent_session: Some(parent),
        parent_entry: Some(at),
    };
    kage_session::fork_as(src, &child_path, header, at)
        .map_err(|err| format!("cannot fork into session {id}: {err}"))?;
    let mut writer = kage_session::SessionWriter::open(&child_path)
        .map_err(|err| format!("cannot append to session {id}: {err}"))?;
    if copied {
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
    let mut cx = AgentContext::new(model.clone(), system_prompt).with_workdir(from.workdir.clone());
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
    replay: &kage_session::ReplayResult,
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
