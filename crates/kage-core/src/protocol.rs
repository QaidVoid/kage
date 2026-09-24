//! Engine protocol: addressed events out, commands in.
//!
//! Every frontend observes the engine through [`Envelope`]s and drives it
//! through [`Command`]s. An envelope is serialized as one flat JSON object:
//! the `session` and `seq` fields sit next to the event's own `type` tag.
//!
//! Events are either durable (they describe state a client must keep, such
//! as [`LoopEvent::MessageAppended`]) or live (deltas and progress a client
//! may drop, such as [`LoopEvent::TextDelta`]). Each variant documents
//! which class it belongs to.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::permissions::PermissionAction;
use crate::{Content, LoopError, LoopEvent, Message, ThinkingLevel, TokenUsage, ToolCallId};

/// Stable identifier for one session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Ulid);

impl SessionId {
    /// Generate a fresh session id.
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Correlates a request the engine raised (permission, dialog) with the
/// command that answers it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One event on the engine bus, addressed to the session that produced it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Session that produced the event.
    pub session: SessionId,
    /// Per-session sequence number, starting at 1 and strictly increasing.
    pub seq: u64,
    /// The event itself.
    #[serde(flatten)]
    pub event: Event,
}

/// An event from either the agent loop or the engine around it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Event {
    /// Emitted by the agent loop.
    Loop(LoopEvent),
    /// Emitted by the engine.
    Host(HostEvent),
}

impl From<LoopEvent> for Event {
    fn from(event: LoopEvent) -> Self {
        Self::Loop(event)
    }
}

impl From<HostEvent> for Event {
    fn from(event: HostEvent) -> Self {
        Self::Host(event)
    }
}

/// Events the engine emits around agent runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostEvent {
    /// A run began. Durable.
    RunStarted,
    /// A run finished. Durable.
    RunEnded {
        /// How the run ended.
        outcome: RunOutcome,
    },
    /// The session's model, thinking level, permission mode, or working
    /// flag changed. Carries the full snapshot. Durable.
    StateChanged {
        /// Current session state.
        state: SessionState,
    },
    /// Token and cost totals changed. Durable.
    UsageUpdated {
        /// Current usage totals.
        usage: Usage,
    },
    /// A tool call needs an interactive decision. Answer with
    /// [`CommandKind::ResolvePermission`]. Durable.
    PermissionRequested {
        /// Id to answer with.
        request_id: RequestId,
        /// Tool call being gated, when the request comes from the loop.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<ToolCallId>,
        /// Tool name.
        tool: String,
        /// What the permission rules matched against: a command line or
        /// compact JSON input.
        subject: String,
        /// The full tool input.
        input: serde_json::Value,
    },
    /// A permission request was answered or abandoned. Durable.
    PermissionResolved {
        /// Id of the answered request.
        request_id: RequestId,
    },
    /// A message for the user that is not part of the conversation.
    /// Never recorded. Live.
    Notice {
        /// Severity.
        level: NoticeLevel,
        /// Message text.
        text: String,
        /// `true` for a passing toast, `false` for a transcript line.
        transient: bool,
    },
    /// The session now points at a different file, for example after a
    /// resume, fork, or new session. Carries the full transcript so a
    /// client can rebuild its view. Durable.
    SessionChanged {
        /// Session file path.
        path: PathBuf,
        /// Stored title, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Conversation history, oldest first.
        messages: Vec<Message>,
    },
    /// The session title was set or generated. Durable.
    TitleChanged {
        /// New title.
        title: String,
    },
    /// A user shell command finished. Its output is shared with the model
    /// on the next turn but is not part of the recorded conversation.
    ShellFinished {
        /// Command line that ran.
        command: String,
        /// Combined, truncated stdout and stderr.
        output: String,
        /// Exit code, or `None` when a signal ended the command.
        exit_code: Option<i32>,
    },
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunOutcome {
    /// The model finished without error.
    Completed,
    /// The user or a client cancelled the run.
    Cancelled,
    /// The run stopped on an error.
    Failed {
        /// What went wrong.
        error: LoopError,
    },
}

/// Snapshot of the settings that shape a session's next run.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    /// Provider-qualified model id, such as `anthropic:claude-sonnet-4-6`.
    pub model: String,
    /// Active thinking level.
    pub thinking: ThinkingLevel,
    /// Session permission override. `None` means the configured rules
    /// decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionAction>,
    /// `true` while a run is in flight.
    pub working: bool,
}

/// Running token and cost totals for a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Cumulative tokens across every turn.
    pub total: TokenUsage,
    /// Prompt size of the most recent turn, compared against
    /// `context_window` for the fill percentage.
    pub context_used: u64,
    /// Context window of the active model. `0` when unknown.
    pub context_window: u64,
    /// Cumulative cost in dollars. `0.0` when the model has no pricing.
    pub cost: f64,
}

/// Severity of a [`HostEvent::Notice`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    /// Informational.
    Info,
    /// Something the user may want to act on.
    Warning,
    /// Something failed.
    Error,
}

/// Answer to a [`HostEvent::PermissionRequested`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Run this one call. The next identical call asks again.
    AllowOnce,
    /// Run this call and allow the tool for the rest of the session
    /// without persisting anything.
    AllowSession,
    /// Run this call, allow the tool for the rest of the session, and
    /// persist an allow rule for it.
    AllowAlways,
    /// Refuse the call.
    Deny,
}

/// How a prompt submitted during a run is delivered.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Deliver at the next turn boundary of the running run.
    #[default]
    Steer,
    /// Deliver as a new run once the current run ends.
    Queue,
}

/// A request for the engine to act.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Command {
    /// Target session. `None` addresses the engine's active session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// What to do.
    #[serde(flatten)]
    pub kind: CommandKind,
}

impl Command {
    /// Address `kind` to the engine's active session.
    #[must_use]
    pub fn active(kind: CommandKind) -> Self {
        Self {
            session: None,
            kind,
        }
    }

    /// Address `kind` to `session`.
    #[must_use]
    pub fn to(session: SessionId, kind: CommandKind) -> Self {
        Self {
            session: Some(session),
            kind,
        }
    }
}

/// Everything a client can ask the engine to do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandKind {
    /// Submit a user prompt. Starts a run when idle; otherwise delivered
    /// according to `delivery`.
    Prompt {
        /// Prompt content: text first, then images.
        content: Vec<Content>,
        /// Delivery while a run is in flight.
        #[serde(default)]
        delivery: Delivery,
    },
    /// Cancel the in-flight run.
    Cancel,
    /// Answer a [`HostEvent::PermissionRequested`].
    ResolvePermission {
        /// Request being answered.
        request_id: RequestId,
        /// The decision.
        decision: PermissionDecision,
    },
    /// Use a different provider-qualified model from the next run on.
    SetModel {
        /// Provider-qualified model id.
        model: String,
    },
    /// Use a different thinking level from the next run on.
    SetThinking {
        /// New level.
        level: ThinkingLevel,
    },
    /// Override the configured permission rules for this session. `None`
    /// restores the configured rules.
    SetPermissionMode {
        /// Override to apply.
        #[serde(default)]
        mode: Option<PermissionAction>,
    },
    /// Compact the conversation now.
    Compact,
    /// Run a shell command in the session directory and share its output
    /// with the model on the next turn.
    Shell {
        /// Command line passed to `sh -c`.
        command: String,
    },
    /// Start a fresh, empty session.
    NewSession,
    /// Resume the session stored at `path`.
    LoadSession {
        /// Session file.
        path: PathBuf,
    },
    /// Copy the active session up to an entry into a new session file.
    Fork {
        /// Entry id prefix to stop at. `None` means the latest entry.
        #[serde(default)]
        at: Option<String>,
        /// Continue on the copy instead of leaving it as a snapshot.
        #[serde(default)]
        switch: bool,
    },
    /// Fork the session stored at `path` at its latest entry.
    ForkFile {
        /// Session file.
        path: PathBuf,
    },
    /// Duplicate the active session and continue on the copy.
    Clone,
    /// Delete the session stored at `path`. Refused for the active session.
    DeleteSession {
        /// Session file.
        path: PathBuf,
    },
    /// Render the active session as Markdown.
    Export {
        /// Destination. `None` writes next to the working directory.
        #[serde(default)]
        path: Option<PathBuf>,
    },
    /// Cancel every run and stop the engine.
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MessageId, Role};

    fn roundtrip(envelope: &Envelope) -> serde_json::Value {
        let value = serde_json::to_value(envelope).unwrap();
        let back: Envelope = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(&back, envelope);
        value
    }

    fn envelope(event: impl Into<Event>) -> Envelope {
        Envelope {
            session: SessionId::new(),
            seq: 7,
            event: event.into(),
        }
    }

    #[test]
    fn envelope_is_one_flat_object() {
        let env = envelope(LoopEvent::TextDelta {
            id: MessageId::new(),
            delta: "hi".into(),
        });
        let value = roundtrip(&env);
        assert_eq!(value["seq"], 7);
        assert_eq!(value["type"], "text_delta");
        assert_eq!(value["delta"], "hi");
        assert_eq!(value["session"], env.session.to_string());
    }

    #[test]
    fn events_roundtrip_and_tags_stay_disjoint() {
        let message = Message::new(Role::User, vec![Content::Text { text: "q".into() }], None);
        let events: Vec<Event> = vec![
            LoopEvent::TextDelta {
                id: MessageId::new(),
                delta: "d".into(),
            }
            .into(),
            LoopEvent::Error {
                kind: LoopError::Cancelled,
            }
            .into(),
            LoopEvent::MessageAppended {
                message: message.clone(),
            }
            .into(),
            LoopEvent::TurnStarted { index: 0 }.into(),
            LoopEvent::TurnEnded {
                index: 0,
                had_tool_calls: true,
            }
            .into(),
            HostEvent::RunStarted.into(),
            HostEvent::RunEnded {
                outcome: RunOutcome::Failed {
                    error: LoopError::Cancelled,
                },
            }
            .into(),
            HostEvent::StateChanged {
                state: SessionState {
                    model: "mock:m".into(),
                    thinking: ThinkingLevel::High,
                    permission_mode: Some(PermissionAction::Ask),
                    working: true,
                },
            }
            .into(),
            HostEvent::UsageUpdated {
                usage: Usage::default(),
            }
            .into(),
            HostEvent::PermissionRequested {
                request_id: RequestId(3),
                tool_call_id: Some(ToolCallId("call_1".into())),
                tool: "bash".into(),
                subject: "ls".into(),
                input: serde_json::json!({ "command": "ls" }),
            }
            .into(),
            HostEvent::PermissionResolved {
                request_id: RequestId(3),
            }
            .into(),
            HostEvent::Notice {
                level: NoticeLevel::Error,
                text: "boom".into(),
                transient: false,
            }
            .into(),
            HostEvent::SessionChanged {
                path: PathBuf::from("/tmp/s.jsonl"),
                title: Some("t".into()),
                messages: vec![message],
            }
            .into(),
            HostEvent::TitleChanged { title: "t".into() }.into(),
            HostEvent::ShellFinished {
                command: "ls".into(),
                output: "a".into(),
                exit_code: Some(0),
            }
            .into(),
        ];
        for event in events {
            let value = roundtrip(&envelope(event.clone()));
            match event {
                Event::Loop(_) => assert!(serde_json::from_value::<HostEvent>(value).is_err()),
                Event::Host(_) => assert!(serde_json::from_value::<LoopEvent>(value).is_err()),
            }
        }
    }

    #[test]
    fn command_is_one_flat_object() {
        let cmd = Command::active(CommandKind::Prompt {
            content: vec![Content::Text { text: "go".into() }],
            delivery: Delivery::Queue,
        });
        let value = serde_json::to_value(&cmd).unwrap();
        assert_eq!(value["type"], "prompt");
        assert_eq!(value["delivery"], "queue");
        assert!(value.get("session").is_none());
        let back: Command = serde_json::from_value(value).unwrap();
        assert_eq!(back, cmd);
    }
}
