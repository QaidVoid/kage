//! The state a [`Client`](crate::Client) mirrors: the agent identity
//! and capabilities the handshake delivered, the open sessions with
//! their grouped transcripts, and the recorded-session directory.
//!
//! Every field is only ever written because a frame carried it, so a
//! view rendering this state shows exactly what the agent said and
//! nothing it did not.

use std::collections::BTreeMap;

use serde_json::Value;

use kage_acp_wire::{
    AgentCapabilities, ContentBlock, Cost, Implementation, McpServerStatus, PermissionOption,
    PermissionOptionKind, PromptRef, SessionConfigOption, SessionInfo, StopReason,
    SubagentSessionCapabilities, SubagentState, SubagentSwarm, SwarmMeta, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolKind, TurnReason,
};

/// What the client knows after the frames it handled.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// The negotiated protocol version, once `initialize` answered.
    pub protocol_version: Option<i64>,
    /// The agent's identity, once `initialize` answered.
    pub agent: Option<Implementation>,
    /// The agent's capabilities, once `initialize` answered.
    pub capabilities: Option<AgentCapabilities>,
    /// The directory a session opened with an empty `cwd` runs in, as
    /// a kage agent reported it at `initialize`.
    pub agent_cwd: Option<String>,
    /// The config options a new session opens with, as a kage agent
    /// reported them at `initialize`.
    pub agent_defaults: Vec<kage_acp_wire::SessionConfigOption>,
    /// The sessions this connection holds or hears updates for, by id.
    pub sessions: BTreeMap<String, Session>,
    /// The recorded sessions a `session/list` answered with, in page
    /// order. A page whose entries are already listed replaces them.
    pub directory: Vec<SessionInfo>,
}

impl State {
    /// Whether the agent accepts a `session/prompt` marked
    /// [`kage_acp_wire::PromptDelivery::Steer`] while a run is in
    /// flight: only what `initialize` advertised.
    #[must_use]
    pub fn steer_available(&self) -> bool {
        self.capabilities.as_ref().is_some_and(|caps| caps.steer)
    }

    /// The open or heard-from session `id`.
    #[must_use]
    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    /// Stores the composer text of `session_id`, replacing any earlier
    /// draft. A session the state has not heard of holds no draft, so
    /// the call reports false.
    pub fn set_draft(&mut self, session_id: &str, text: &str) -> bool {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return false;
        };
        session.draft = Some(text.to_owned());
        true
    }

    /// The composer text stored for `session_id`.
    #[must_use]
    pub fn draft(&self, session_id: &str) -> Option<&str> {
        self.sessions
            .get(session_id)
            .and_then(|session| session.draft.as_deref())
    }

    /// Every open permission ask with the session that holds it, in
    /// session id order, asks in arrival order within a session. The
    /// approval surface renders this queue; the asking session id
    /// travels with each ask so the card can name its context.
    #[must_use]
    pub fn open_asks(&self) -> Vec<(&str, &PermissionAsk)> {
        self.sessions
            .values()
            .flat_map(|session| {
                session
                    .permissions
                    .iter()
                    .map(move |ask| (session.id.as_str(), ask))
            })
            .collect()
    }

    /// How `session_id` presents itself on an approval: the subagent's
    /// name when another session announced it as a child, else the
    /// session title. `None` when the state knows neither, in which
    /// case the card names no byline rather than inventing one.
    #[must_use]
    pub fn asker_byline(&self, session_id: &str) -> Option<String> {
        for session in self.sessions.values() {
            if let Some(agent) = session.agents.get(session_id) {
                return Some(match &agent.name {
                    Some(name) => format!("sub agent \u{b7} {name}"),
                    None => "sub agent".to_owned(),
                });
            }
        }
        self.sessions
            .get(session_id)
            .and_then(|session| session.title.clone())
    }
}

/// One session: a run the client is driving, a subagent it hears, or a
/// session it asked to open. Updates create a session entry the first
/// time they name it, so a subagent's transcript exists before and
/// without the client opening it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    /// The id every frame about this session carries.
    pub id: String,
    /// Whether the client opened the session itself with `session/new`,
    /// `session/load` or `session/resume`, as opposed to hearing about
    /// it through updates alone.
    pub opened: bool,
    /// The session that announced this one as its subagent. A child
    /// session is reached through its parent, never listed on its own.
    pub parent: Option<String>,
    /// The working directory the session was opened with.
    pub cwd: Option<String>,
    /// The display title, as `session_info_update` last set it.
    pub title: Option<String>,
    /// ISO 8601 time of the last activity, as the agent reported it.
    pub updated_at: Option<String>,
    /// The grouped transcript so far, oldest first.
    pub items: Vec<TranscriptItem>,
    /// Context fill and cost, as `usage_update` last reported it.
    pub usage: Usage,
    /// The active mode id, as `current_mode_update` last reported it.
    pub mode: Option<String>,
    /// The session's config options, as the agent last reported them.
    pub config_options: Vec<SessionConfigOption>,
    /// The slash commands `available_commands_update` last listed.
    pub commands: Vec<Value>,
    /// Per-server MCP reachability, as `_kage/mcp_status` reported it.
    pub mcp: BTreeMap<String, McpServerStatus>,
    /// The permission asks waiting for a decision, oldest first.
    pub permissions: Vec<PermissionAsk>,
    /// Prompts held back because a run was in flight and the agent
    /// cannot steer, oldest first.
    pub queue: Vec<QueuedPrompt>,
    /// Whether a prompt the client sent is still unanswered.
    pub running: bool,
    /// Whether a `_kage/turn` started and has not ended yet.
    pub in_turn: bool,
    /// The subagents announced on this session, by session id.
    pub agents: BTreeMap<String, Subagent>,
    /// The plan entries the latest `plan` update carried.
    pub plan: Option<Vec<Value>>,
    /// Why the last answered prompt ended.
    pub last_stop: Option<StopReason>,
    /// The composer text the user typed here and has not sent, kept
    /// across switches away from the session.
    pub draft: Option<String>,
}

impl Session {
    /// An empty session record for `id`.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Session::default()
        }
    }

    /// The prompt at transcript `index` as the agent finds it again: its
    /// first text block and how many earlier prompts carried the same.
    /// `None` when the item is not a prompt with text.
    #[must_use]
    pub fn prompt_ref(&self, index: usize) -> Option<PromptRef> {
        let text = prompt_text(self.items.get(index)?)?;
        let occurrence = self.items[..index]
            .iter()
            .filter(|item| prompt_text(item) == Some(text))
            .count();
        Some(PromptRef {
            text: text.to_owned(),
            occurrence: u32::try_from(occurrence).unwrap_or(u32::MAX),
        })
    }

    /// Appends a streamed message or thought chunk, merging it into
    /// the trailing item when that item is of the same kind.
    pub(crate) fn append_chunk(&mut self, chunk: &ContentBlock, assistant: bool) {
        let Some(text) = chunk.as_text() else {
            return;
        };
        let extends = match self.items.last() {
            Some(TranscriptItem::Assistant { .. }) => assistant,
            Some(TranscriptItem::Thinking { .. }) => !assistant,
            _ => false,
        };
        if extends {
            if let Some(
                TranscriptItem::Assistant { text: held }
                | TranscriptItem::Thinking { text: held, .. },
            ) = self.items.last_mut()
            {
                held.push_str(text);
            }
        } else {
            self.items.push(if assistant {
                TranscriptItem::Assistant {
                    text: text.to_owned(),
                }
            } else {
                TranscriptItem::Thinking {
                    text: text.to_owned(),
                    took_ms: None,
                }
            });
        }
    }
}

/// Context fill and cost as `usage_update` reported them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    /// Tokens currently in context.
    pub used: u64,
    /// Context window size in tokens.
    pub size: u64,
    /// Cumulative cost, when the agent priced the model.
    pub cost: Option<Cost>,
}

impl Usage {
    /// The share of the context window in use, in `0.0 ..= 1.0`. A
    /// session with no reported window fills nothing.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn fill(&self) -> f64 {
        if self.size == 0 {
            return 0.0;
        }
        self.used.min(self.size) as f64 / self.size as f64
    }
}

/// One open permission ask: the agent's `session/request_permission`
/// request waiting for the user's verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionAsk {
    /// The id the reply answers.
    pub request_id: u64,
    /// The tool call awaiting the verdict.
    pub tool_call: ToolCallUpdate,
    /// The choices offered.
    pub options: Vec<PermissionOption>,
    /// The plan document a plan-mode review asks about, from the
    /// request's `_meta.kage.planReview`.
    pub plan: Option<String>,
}

/// The tool input keys that name a call's primary argument, most
/// specific first: what an ask and its decision are about.
pub const SUBJECT_KEYS: [&str; 5] = ["command", "path", "pattern", "url", "description"];

impl PermissionAsk {
    /// What the ask is about: the primary argument of the tool input,
    /// else the tool title, else the call id.
    #[must_use]
    pub fn subject(&self) -> String {
        let input = self.tool_call.raw_input.as_ref();
        SUBJECT_KEYS
            .iter()
            .find_map(|key| input?.get(*key)?.as_str())
            .map(str::to_owned)
            .or_else(|| self.tool_call.title.clone())
            .unwrap_or_else(|| self.tool_call.tool_call_id.clone())
    }

    /// The id of the first option of `kind`, when the ask offers one.
    #[must_use]
    pub fn option_of(&self, kind: PermissionOptionKind) -> Option<&str> {
        self.options
            .iter()
            .find(|option| option.kind == kind)
            .map(|option| option.option_id.as_str())
    }
}

/// A prompt the client holds because a run was in flight and the agent
/// cannot steer. The client sends it itself, plain, once the run ends.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedPrompt {
    /// The prompt content.
    pub prompt: Vec<ContentBlock>,
}

/// One subagent of a session. The first `subagent_update` for an id
/// announces it; later updates change the fields they carry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Subagent {
    /// Short label, such as the agent kind.
    pub name: Option<String>,
    /// Summary of the delegated work.
    pub task: Option<String>,
    /// Operations the client may perform on the child.
    pub capabilities: Option<SubagentSessionCapabilities>,
    /// Lifecycle state. A child never given one is running.
    pub state: Option<SubagentState>,
    /// The swarm batch the child belongs to, when a `swarm` call started
    /// it: batch id, item, launch index and batch size.
    pub swarm: Option<SubagentSwarm>,
    /// Why the child paused, while it is paused.
    pub reason: Option<String>,
    /// The parent's `agent` or `swarm` call that started the child.
    pub tool_call_id: Option<String>,
    /// Tokens, cost and run time the child reported, latest first.
    pub usage: Option<kage_acp_wire::SubagentUsage>,
    /// The model the child runs, as `provider/model`.
    pub model: Option<String>,
    /// Whether the child runs in the background.
    pub background: bool,
}

impl Subagent {
    /// Applies the fields `update` carries. A field the update omits
    /// stays as it was, so a late state never wipes the announcement.
    pub(crate) fn merge(&mut self, update: &kage_acp_wire::SubagentUpdate) {
        if update.name.is_some() {
            self.name.clone_from(&update.name);
        }
        if update.task.is_some() {
            self.task.clone_from(&update.task);
        }
        if update.capabilities.is_some() {
            self.capabilities.clone_from(&update.capabilities);
        }
        if update.state.is_some() {
            self.state.clone_from(&update.state);
            // A reason belongs to the pause it explained.
            if update.state != Some(SubagentState::Paused) {
                self.reason = None;
            }
        }
        if update.swarm.is_some() {
            self.swarm.clone_from(&update.swarm);
        }
        if update.tool_call_id.is_some() {
            self.tool_call_id.clone_from(&update.tool_call_id);
        }
        if update.reason.is_some() {
            self.reason.clone_from(&update.reason);
        }
        if update.usage.is_some() {
            self.usage.clone_from(&update.usage);
        }
        if update.model.is_some() {
            self.model.clone_from(&update.model);
        }
        // Only the announcement says so; later updates leave it out.
        self.background |= update.background;
    }
}

/// One grouped transcript entry. Consecutive message chunks of a kind
/// merge into the trailing item, tool call updates merge into the call
/// they name, and a plan update replaces the plan item before it.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptItem {
    /// One user prompt: the blocks this client sent, or the blocks an
    /// echo or a replay carried, text first.
    User {
        /// The prompt's blocks.
        content: Vec<ContentBlock>,
        /// Whether it steered a run in flight instead of starting one.
        steered: bool,
    },
    /// Assistant reply text.
    Assistant {
        /// The text so far.
        text: String,
    },
    /// Assistant reasoning text.
    Thinking {
        /// The text so far.
        text: String,
        /// How long it took, as recorded, on a replayed thought.
        took_ms: Option<u64>,
    },
    /// A tool call, with everything reported about it since.
    ToolCall(ToolCallItem),
    /// A turn boundary the run closed.
    TurnEnd {
        /// Why the turn ended: whether tool calls follow.
        reason: Option<TurnReason>,
        /// When the run ended, in seconds since the Unix epoch, as
        /// recorded on a replayed run end.
        at: Option<i64>,
        /// How long the run took, as recorded on a replayed run end.
        took_ms: Option<u64>,
    },
    /// A message for the user outside the conversation.
    Notice {
        /// Severity.
        tone: kage_acp_wire::NoticeTone,
        /// Message text.
        text: String,
    },
    /// Older context turns were summarized.
    Compaction {
        /// Recent turns kept verbatim.
        kept: u64,
        /// Context fill before the compaction.
        before: u64,
        /// Context fill after the compaction.
        after: u64,
    },
    /// The agent's plan. Only one plan item stays in the transcript; a
    /// later plan replaces the latest one.
    Plan {
        /// Plan entries, as the wire carried them.
        entries: Vec<Value>,
    },
    /// A permission ask was answered from this client: what it was
    /// about and what was chosen. A one-line record; an ask withdrawn
    /// through `$/cancel_request` leaves no record, because this
    /// client never chose.
    Decision {
        /// What the ask was about: the tool call title, or its id when
        /// the agent sent no title.
        subject: String,
        /// The chosen option's label, as offered, or that the ask was
        /// dismissed without a choice.
        label: String,
        /// Whether the choice lets the tool call proceed.
        allowed: bool,
        /// The feedback text the answer carried, when it did.
        feedback: Option<String>,
    },
}

/// A tool call and the updates it has collected so far.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallItem {
    /// Correlation id every update for this call carries.
    pub tool_call_id: String,
    /// Human title, usually the tool name.
    pub title: String,
    /// Tool kind hint.
    pub kind: ToolKind,
    /// Lifecycle status.
    pub status: ToolCallStatus,
    /// The tool input, as last reported.
    pub input: Option<Value>,
    /// The swarm batch a `swarm` call announced under
    /// `_meta.kage.swarm`: its members and prompt template.
    pub swarm: Option<SwarmMeta>,
    /// Rich content, replaced by every update that carries content:
    /// streamed output tails, diffs, terminal references.
    pub content: Vec<ToolCallContent>,
    /// Structured output, when the call finished with one.
    pub raw_output: Option<Value>,
    /// How long it ran, as recorded, on a replayed call.
    pub took_ms: Option<u64>,
}

impl ToolCallItem {
    /// The text the call's content blocks contribute: the output tail
    /// the agent last streamed. Diff and terminal content adds none.
    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|content| match content {
                ToolCallContent::Content(chunk) => chunk.content.as_text(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Applies the fields `update` carries. Content replaces, so a
    /// streamed output tail does not grow without bound, and a field
    /// the update omits stays as it was.
    pub(crate) fn merge(&mut self, update: &ToolCallUpdate) {
        if let Some(title) = &update.title {
            self.title.clone_from(title);
        }
        if let Some(kind) = update.kind {
            self.kind = kind;
        }
        if let Some(status) = update.status {
            self.status = status;
        }
        if update.raw_input.is_some() {
            self.input.clone_from(&update.raw_input);
        }
        if !update.content.is_empty() {
            self.content.clone_from(&update.content);
        }
        if update.raw_output.is_some() {
            self.raw_output.clone_from(&update.raw_output);
        }
        if let Some(meta) = &update.meta {
            if meta.kage.swarm.is_some() {
                self.swarm.clone_from(&meta.kage.swarm);
            }
            if meta.kage.duration_ms.is_some() {
                self.took_ms = meta.kage.duration_ms;
            }
        }
    }
}

/// The first text block of a prompt item.
fn prompt_text(item: &TranscriptItem) -> Option<&str> {
    match item {
        TranscriptItem::User { content, .. } => content.iter().find_map(ContentBlock::as_text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kage_acp_wire::{DiffContent, MessageChunk};

    #[test]
    fn usage_fill_reports_a_fraction_of_the_window() {
        let usage = Usage {
            used: 500,
            size: 1000,
            cost: None,
        };
        assert!((usage.fill() - 0.5).abs() < f64::EPSILON);
        assert!(Usage::default().fill() < f64::EPSILON);
        let over = Usage {
            used: 1500,
            size: 1000,
            cost: None,
        };
        assert!((over.fill() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn chunks_merge_only_into_a_trailing_item_of_their_kind() {
        let mut session = Session::new("s1");
        session.append_chunk(&ContentBlock::text("Hel"), true);
        session.append_chunk(&ContentBlock::text("lo"), true);
        session.append_chunk(&ContentBlock::text("thinking"), false);
        session.append_chunk(&ContentBlock::text(" more"), false);
        session.append_chunk(&ContentBlock::text("again"), true);
        assert_eq!(
            session.items,
            vec![
                TranscriptItem::Assistant {
                    text: "Hello".into()
                },
                TranscriptItem::Thinking {
                    text: "thinking more".into(),
                    took_ms: None,
                },
                TranscriptItem::Assistant {
                    text: "again".into()
                },
            ]
        );
    }

    #[test]
    fn tool_call_updates_replace_content_and_patch_fields() {
        let mut call = ToolCallItem {
            tool_call_id: "c1".into(),
            title: "shell".into(),
            kind: ToolKind::Execute,
            status: ToolCallStatus::InProgress,
            input: None,
            swarm: None,
            content: vec![ToolCallContent::Content(MessageChunk {
                content: ContentBlock::text("line 1\nline 2"),
                meta: None,
            })],
            raw_output: None,
            took_ms: None,
        };
        assert_eq!(call.text(), "line 1\nline 2");
        call.merge(&ToolCallUpdate {
            tool_call_id: "c1".into(),
            status: Some(ToolCallStatus::Completed),
            content: vec![ToolCallContent::Content(MessageChunk {
                content: ContentBlock::text("line 2\nline 3"),
                meta: None,
            })],
            raw_output: Some(serde_json::json!({"exit_code": 0})),
            ..ToolCallUpdate::default()
        });
        assert_eq!(call.status, ToolCallStatus::Completed);
        assert_eq!(call.text(), "line 2\nline 3");
        assert_eq!(call.raw_output, Some(serde_json::json!({"exit_code": 0})));
        let diff = ToolCallContent::Diff(DiffContent {
            path: "a.rs".into(),
            old_text: None,
            new_text: "fn a() {}".into(),
        });
        call.merge(&ToolCallUpdate {
            tool_call_id: "c1".into(),
            content: vec![diff.clone()],
            ..ToolCallUpdate::default()
        });
        assert_eq!(call.content, vec![diff]);
        assert_eq!(call.status, ToolCallStatus::Completed, "status untouched");
    }

    #[test]
    fn permission_asks_find_options_by_kind() {
        let ask = PermissionAsk {
            request_id: 1,
            tool_call: ToolCallUpdate::default(),
            options: vec![
                kage_acp_wire::PermissionOption {
                    option_id: "allow".into(),
                    name: "Allow".into(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                kage_acp_wire::PermissionOption {
                    option_id: "reject".into(),
                    name: "Reject".into(),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ],
            plan: None,
        };
        assert_eq!(
            ask.option_of(PermissionOptionKind::AllowOnce),
            Some("allow")
        );
        assert_eq!(ask.option_of(PermissionOptionKind::AllowAlways), None);
    }
}
