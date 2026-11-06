//! Append-only JSONL session entry types.
//!
//! A session is a flat JSONL file. The first line is a [`SessionEntry::Header`];
//! subsequent lines are entries appended in order. Each entry has its own id
//! and timestamp so forks and rewinds can target any point. An undo or
//! rewind appends a [`SessionEntry::Branch`] marker instead of rewriting:
//! the conversation continues from the entry the marker names, and the
//! entries it abandons stay in the file. See [`crate::tree::SessionTree`].

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
pub use kage_core::SessionId;
use kage_core::{Message, TokenUsage};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ulid::Ulid;

/// Current on-disk session format version.
///
/// Bumped whenever the entry schema changes in a non-additive way. Readers
/// accept the versions whose entries they all interpret: version 2 added
/// the [`SessionEntry::Branch`] marker, which version 1 readers must not
/// see, and version 3 added `Content::Audio`, which version 2 readers
/// cannot decode. Version 4 marks external backend identity so older readers
/// cannot mistake an external session for a native session.
pub const FORMAT_VERSION: u32 = 4;

/// Stable identifier for a single entry within a session.
///
/// Used as the cut point for [`fork`](crate::fork()): a forked child copies
/// every entry up to and including the named id.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub Ulid);

impl EntryId {
    /// Generate a fresh entry id.
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::generate())
    }
}

impl Default for EntryId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One JSONL line in a session file.
///
/// The `Header` variant must appear exactly once, on the first line. All
/// other variants may appear any number of times in any order. The reader
/// preserves order on disk and the writer never rewrites prior lines.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    /// First line of every session file. Carries identity and provenance.
    Header(Header),
    /// A reassembled conversation message (user, assistant, or tool result).
    Message(MessageEntry),
    /// The model's thinking budget was changed mid-session.
    ThinkingLevelChange(ThinkingLevelChange),
    /// The active model was switched mid-session.
    ModelChange(ModelChange),
    /// Older turns were summarized into a synthetic message.
    Compaction(Compaction),
    /// A user-supplied bookmark anchored at a specific entry.
    Label(Label),
    /// A short generated title for the session, written once after
    /// the first assistant response. Latest one wins on read.
    Title(SessionTitle),
    /// The conversation continues from entry `at`. Entries between
    /// that entry and the marker stay in the file as an abandoned
    /// branch; replay, fork and the session list follow the marker and
    /// ignore them. Written by an in-place undo or rewind.
    Branch(BranchEntry),
    /// A plugin-defined entry the core does not interpret.
    Custom(Custom),
}

impl SessionEntry {
    /// Id assigned to this entry at append time.
    #[must_use]
    pub fn id(&self) -> EntryId {
        match self {
            Self::Header(h) => h.id,
            Self::Message(m) => m.id,
            Self::ThinkingLevelChange(t) => t.id,
            Self::ModelChange(m) => m.id,
            Self::Compaction(c) => c.id,
            Self::Label(l) => l.id,
            Self::Title(t) => t.id,
            Self::Branch(b) => b.id,
            Self::Custom(c) => c.id,
        }
    }

    /// UTC timestamp at which this entry was appended.
    #[must_use]
    pub fn ts(&self) -> DateTime<Utc> {
        match self {
            Self::Header(h) => h.ts,
            Self::Message(m) => m.ts,
            Self::ThinkingLevelChange(t) => t.ts,
            Self::ModelChange(m) => m.ts,
            Self::Compaction(c) => c.ts,
            Self::Label(l) => l.ts,
            Self::Title(t) => t.ts,
            Self::Branch(b) => b.ts,
            Self::Custom(c) => c.ts,
        }
    }
}

/// Backend identity for sessions whose execution is owned outside kage.
///
/// The local session entries are presentation history, not the authority for
/// restoring the upstream agent's state. Configuration, commands, environment
/// variables and credentials must not be stored here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionBackend {
    /// A persistent session owned by an external ACP agent.
    ExternalAcp(ExternalAcpSession),
}

/// The minimum identity needed to locate an external ACP session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAcpSession {
    /// Name of the locally configured agent, not its launch command.
    pub agent_name: String,
    /// Negotiated ACP protocol version.
    pub protocol_version: u32,
    /// Session identifier assigned by the upstream agent.
    pub upstream_session_id: String,
}

/// Identity and provenance of a session file. Always the first entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedHeader")]
pub struct Header {
    /// On-disk schema version. See [`FORMAT_VERSION`].
    pub version: u32,
    /// This session's unique id.
    pub session: SessionId,
    /// Entry id assigned to the header itself.
    pub id: EntryId,
    /// Creation timestamp.
    pub ts: DateTime<Utc>,
    /// Working directory at session creation. For external ACP sessions this
    /// is canonicalized by [`crate::SessionWriter::create`].
    pub cwd: PathBuf,
    /// Provider-qualified model id (`provider/model`) at creation.
    pub model: String,
    /// System prompt at creation. Subsequent changes are not reflected here.
    pub system_prompt: String,
    /// External execution identity, absent for native and legacy sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<SessionBackend>,
    /// The session this one was forked from or spawned by, if any. A
    /// spawned session marks itself with a `kage:agent` custom entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<SessionId>,
    /// Entry id within `parent_session` that this fork branched from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_entry: Option<EntryId>,
}

#[derive(Deserialize)]
struct UncheckedHeader {
    version: u32,
    session: SessionId,
    id: EntryId,
    ts: DateTime<Utc>,
    cwd: PathBuf,
    model: String,
    system_prompt: String,
    #[serde(default)]
    backend: Option<SessionBackend>,
    #[serde(default)]
    parent_session: Option<SessionId>,
    #[serde(default)]
    parent_entry: Option<EntryId>,
}

impl TryFrom<UncheckedHeader> for Header {
    type Error = &'static str;

    fn try_from(header: UncheckedHeader) -> Result<Self, Self::Error> {
        if header.backend.is_some() && header.version < 4 {
            return Err("external session backend requires format version 4 or later");
        }
        Ok(Self {
            version: header.version,
            session: header.session,
            id: header.id,
            ts: header.ts,
            cwd: header.cwd,
            model: header.model,
            system_prompt: header.system_prompt,
            backend: header.backend,
            parent_session: header.parent_session,
            parent_entry: header.parent_entry,
        })
    }
}

/// A reassembled conversation message persisted as a session entry.
///
/// Assistant messages may carry a [`TokenUsage`] snapshot from the
/// turn's `MessageEnd` event. User and tool-result messages set
/// `usage` to `None`. Older session files written before the field
/// existed deserialize cleanly: `serde(default)` falls back to
/// `None`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageEntry {
    /// Entry id.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// The full message including role, content blocks, and message id.
    /// Shared with the loop's history buffer; serializes identically to
    /// [`Message`].
    pub message: Arc<Message>,
    /// Provider-reported token usage for this turn, when available.
    /// Optional and defaulted to `None` so old sessions still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

/// Records a change to the model's thinking budget.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThinkingLevelChange {
    /// Entry id.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// New thinking budget level. Plugin-defined; the loop forwards it as is.
    pub level: String,
}

/// Records a switch to a different provider-qualified model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelChange {
    /// Entry id.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// New provider-qualified model id (`provider/model`).
    pub model: String,
}

/// Records a history compaction performed by the loop.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    /// Entry id.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// Number of recent turns kept verbatim after compaction.
    pub kept: usize,
    /// Number of older turns replaced by the summary.
    pub summarized: usize,
    /// Synthetic summary text inserted in place of the summarized turns.
    pub summary: String,
    /// Context tokens in use right before the loop spliced the summary
    /// in. Optional and defaulted so old sessions still parse; the
    /// other half of a compaction card's token delta. The post-turn
    /// fill is not stored: replay reads it from the first usage
    /// recorded after this entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
}

/// User-supplied label/bookmark anchored to a specific entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Label {
    /// Entry id of the label itself.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// Free-form label text shown by `kage list` and search.
    pub text: String,
    /// The entry the label is attached to.
    pub anchor: EntryId,
}

/// A short generated title for the session. Written once after the
/// first assistant response (the host generates it); the most recent
/// `Title` entry wins when a session is summarized, so a later
/// regeneration can override an earlier one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionTitle {
    /// Entry id of the title itself.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// The generated title text (already trimmed to a short length
    /// by the host).
    pub title: String,
}

/// Reserved custom kind for the external ACP presentation transcript.
///
/// These records are local display history, not upstream session state. Only
/// write them to a session whose header identifies an external ACP backend.
pub const EXTERNAL_ACP_PRESENTATION_KIND: &str = "kage:external_acp_presentation";

/// Maximum UTF-8 byte length of any text field in a presentation record.
pub const EXTERNAL_ACP_TEXT_LIMIT: usize = 64 * 1024;

/// Bounded, display-only text. It may still contain sensitive user content.
///
/// The host must choose deliberately what to display and persist. This type
/// bounds storage, but does not redact secrets in prompts or agent output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PresentationText(String);

impl PresentationText {
    /// Construct text if its UTF-8 representation fits the per-field limit.
    pub fn new(text: String) -> Result<Self, &'static str> {
        if text.len() > EXTERNAL_ACP_TEXT_LIMIT {
            return Err("external ACP presentation text exceeds the byte limit");
        }
        Ok(Self(text))
    }

    /// The original text, without truncation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PresentationText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::new(text).map_err(serde::de::Error::custom)
    }
}

/// A deliberately restricted external ACP presentation event.
///
/// Records are ordered by their JSONL position. A prompt is recorded once
/// the host admits it, before dispatch, so a failed dispatch is followed by a
/// failed turn end. Tool updates are partial replacements, not executable
/// requests. The host must omit unsupported content, raw input and output,
/// metadata, permission requests and decisions, and authentication data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalAcpPresentation {
    /// An accepted text prompt starts a turn.
    Prompt {
        /// User-supplied prompt text.
        text: PresentationText,
    },
    /// A streamed agent message fragment.
    AgentMessage {
        /// Host-assigned id grouping fragments of the same message.
        item_id: PresentationText,
        /// Display text.
        text: PresentationText,
    },
    /// A streamed agent thought fragment.
    AgentThought {
        /// Host-assigned id grouping fragments of the same thought.
        item_id: PresentationText,
        /// Display text.
        text: PresentationText,
    },
    /// A tool call announced for display only.
    ToolCall {
        /// Upstream correlation id.
        tool_call_id: PresentationText,
        /// Display title.
        title: PresentationText,
        /// Current display status.
        status: PresentationToolStatus,
    },
    /// A partial update to an announced tool call.
    ToolCallUpdate {
        /// Upstream correlation id.
        tool_call_id: PresentationText,
        /// Replacement display title, if supplied.
        title: Option<PresentationText>,
        /// Replacement status, if supplied.
        status: Option<PresentationToolStatus>,
        /// Replacement display text, if supplied, never raw tool JSON.
        text: Option<PresentationText>,
    },
    /// The terminal outcome of the current prompt.
    TurnEnd {
        /// Outcome observed by the host.
        outcome: PresentationTurnOutcome,
    },
    /// Recording stopped here, so later events of the session are missing.
    Gap,
}

/// Display status of an external tool call.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationToolStatus {
    /// The call has not started.
    Pending,
    /// The call is running.
    InProgress,
    /// The call completed successfully.
    Completed,
    /// The call failed.
    Failed,
    /// The upstream reported an unrecognized status.
    Unknown,
}

/// A prompt's terminal result, including transport failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationTurnOutcome {
    /// The agent finished its turn.
    EndTurn,
    /// The output token limit was reached.
    MaxTokens,
    /// The request limit was reached.
    MaxTurnRequests,
    /// The agent refused the prompt.
    Refusal,
    /// The turn was cancelled.
    Cancelled,
    /// The turn failed or the connection was lost.
    Failed,
}

impl ExternalAcpPresentation {
    /// Wrap a validated presentation event in an appendable session entry.
    ///
    /// The caller is responsible for backend identity, prompt acceptance,
    /// ordering and pairing each prompt with its terminal outcome.
    #[must_use]
    pub fn into_entry(self, id: EntryId, ts: DateTime<Utc>) -> SessionEntry {
        SessionEntry::Custom(Custom {
            id,
            ts,
            kind: EXTERNAL_ACP_PRESENTATION_KIND.to_owned(),
            data: serde_json::to_value(self).expect("presentation events serialize to JSON"),
        })
    }

    /// Decode a reserved custom record. Other custom kinds return `None`.
    ///
    /// Returns an error for malformed or unsupported reserved records. Callers
    /// must not silently discard such errors when reconstructing history.
    pub fn from_custom(custom: &Custom) -> Result<Option<Self>, serde_json::Error> {
        if custom.kind != EXTERNAL_ACP_PRESENTATION_KIND {
            return Ok(None);
        }
        serde_json::from_value(custom.data.clone()).map(Some)
    }
}

/// Plugin-defined entry. The reserved external presentation kind is validated.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "UncheckedCustom")]
pub struct Custom {
    /// Entry id.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// Plugin-defined kind tag, namespaced like `plugin:tps`.
    pub kind: String,
    /// Plugin payload. The external presentation kind has a strict schema.
    pub data: serde_json::Value,
}

#[derive(Deserialize)]
struct UncheckedCustom {
    id: EntryId,
    ts: DateTime<Utc>,
    kind: String,
    data: CustomData,
}

struct CustomData {
    value: serde_json::Value,
    duplicate_field: bool,
}

impl<'de> Deserialize<'de> for CustomData {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DataVisitor;

        impl<'de> Visitor<'de> for DataVisitor {
            type Value = CustomData;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut value = serde_json::Map::new();
                let mut duplicate_field = false;
                while let Some((key, item)) = map.next_entry::<String, serde_json::Value>()? {
                    duplicate_field |= value.insert(key, item).is_some();
                }
                Ok(CustomData {
                    value: value.into(),
                    duplicate_field,
                })
            }

            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                let value = serde_json::Value::deserialize(
                    serde::de::value::SeqAccessDeserializer::new(seq),
                )?;
                Ok(CustomData {
                    value,
                    duplicate_field: false,
                })
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(CustomData {
                    value: value.into(),
                    duplicate_field: false,
                })
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(CustomData {
                    value: value.into(),
                    duplicate_field: false,
                })
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(CustomData {
                    value: value.into(),
                    duplicate_field: false,
                })
            }

            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(CustomData {
                    value: serde_json::Value::from(value),
                    duplicate_field: false,
                })
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(CustomData {
                    value: value.into(),
                    duplicate_field: false,
                })
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(CustomData {
                    value: value.into(),
                    duplicate_field: false,
                })
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(CustomData {
                    value: serde_json::Value::Null,
                    duplicate_field: false,
                })
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                self.visit_unit()
            }
        }

        deserializer.deserialize_any(DataVisitor)
    }
}

impl TryFrom<UncheckedCustom> for Custom {
    type Error = serde_json::Error;

    fn try_from(value: UncheckedCustom) -> Result<Self, Self::Error> {
        if value.kind == EXTERNAL_ACP_PRESENTATION_KIND && value.data.duplicate_field {
            return Err(serde::de::Error::custom(
                "duplicate field in external ACP presentation",
            ));
        }
        let custom = Self {
            id: value.id,
            ts: value.ts,
            kind: value.kind,
            data: value.data.value,
        };
        ExternalAcpPresentation::from_custom(&custom)?;
        Ok(custom)
    }
}

impl Serialize for Custom {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct CustomRef<'a> {
            id: EntryId,
            ts: DateTime<Utc>,
            kind: &'a str,
            data: &'a serde_json::Value,
        }
        ExternalAcpPresentation::from_custom(self).map_err(serde::ser::Error::custom)?;
        CustomRef {
            id: self.id,
            ts: self.ts,
            kind: &self.kind,
            data: &self.data,
        }
        .serialize(serializer)
    }
}

/// Marks where the conversation continues after an undo or rewind.
///
/// The marker itself is not part of the conversation: the entry
/// [`BranchEntry::at`] names takes its previous entry's place as the
/// parent of whatever is appended next, and the entries between that
/// entry and the marker are abandoned but kept.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchEntry {
    /// Entry id of the marker itself.
    pub id: EntryId,
    /// Append time.
    pub ts: DateTime<Utc>,
    /// The entry the conversation continues from, or the header id to
    /// restart from the top.
    pub at: EntryId,
}

#[cfg(test)]
mod tests {
    use kage_core::{Content, Role};

    use super::*;

    #[test]
    fn header_round_trips() {
        let header = SessionEntry::Header(Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/tmp/work"),
            model: "anthropic:claude-sonnet-4-6".into(),
            system_prompt: "You are kage.".into(),
            parent_session: None,
            backend: None,
            parent_entry: None,
        });
        let line = serde_json::to_string(&header).unwrap();
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(header, back);
    }

    #[test]
    fn header_skips_none_parents() {
        let header = SessionEntry::Header(Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/tmp"),
            model: "x:y".into(),
            system_prompt: String::new(),
            parent_session: None,
            backend: None,
            parent_entry: None,
        });
        let json = serde_json::to_value(&header).unwrap();
        assert!(json.get("parent_session").is_none());
        assert!(json.get("parent_entry").is_none());
    }

    #[test]
    fn legacy_header_without_backend_reads_as_native() {
        let json = serde_json::json!({
            "type": "header", "version": 1, "session": SessionId::new(),
            "id": EntryId::new(), "ts": "2026-01-01T00:00:00Z",
            "cwd": "/tmp", "model": "x:y", "system_prompt": ""
        });
        let SessionEntry::Header(header) = serde_json::from_value(json).unwrap() else {
            panic!()
        };
        assert_eq!(header.backend, None);
        assert!(
            serde_json::to_value(SessionEntry::Header(header))
                .unwrap()
                .get("backend")
                .is_none()
        );
    }

    #[test]
    fn external_backend_round_trips_only_identity_fields() {
        let backend = SessionBackend::ExternalAcp(ExternalAcpSession {
            agent_name: "configured-agent".into(),
            protocol_version: 1,
            upstream_session_id: "upstream-id".into(),
        });
        let value = serde_json::to_value(&backend).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "external_acp", "agent_name": "configured-agent",
                "protocol_version": 1, "upstream_session_id": "upstream-id"
            })
        );
        assert_eq!(
            serde_json::from_value::<SessionBackend>(value.clone()).unwrap(),
            backend
        );
        for key in ["command", "args", "env", "secret"] {
            let mut injected = value.clone();
            injected[key] = serde_json::json!("sensitive-value");
            assert!(serde_json::from_value::<SessionBackend>(injected).is_err());
        }
        assert!(
            serde_json::from_value::<SessionBackend>(serde_json::json!({"kind": "future"}))
                .is_err()
        );
    }

    #[test]
    fn message_entry_round_trips() {
        let entry = SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Arc::new(Message::new(
                Role::User,
                vec![Content::Text { text: "hi".into() }],
                None,
            )),
            usage: None,
        });
        let line = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn entry_carries_type_tag() {
        let entry = SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "milestone".into(),
            anchor: EntryId::new(),
        });
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "label");
        assert_eq!(json["text"], "milestone");
    }

    #[test]
    fn compaction_round_trips() {
        let entry = SessionEntry::Compaction(Compaction {
            id: EntryId::new(),
            ts: Utc::now(),
            kept: 4,
            summarized: 12,
            summary: "[summary of 12 earlier turns]\nuser asked about X.".into(),
            before: Some(160_000),
        });
        let line = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(entry, back);
    }

    /// A compaction written before the `before` count existed parses
    /// with it as `None`.
    #[test]
    fn compaction_without_before_reads_none() {
        let line = format!(
            "{{\"type\":\"compaction\",\"id\":\"{}\",\"ts\":\"2026-01-01T00:00:00Z\",\"kept\":2,\"summarized\":4,\"summary\":\"s\"}}",
            ulid::Ulid::generate()
        );
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        match back {
            SessionEntry::Compaction(c) => {
                assert_eq!(c.before, None);
                assert_eq!(c.kept, 2);
            }
            other => panic!("expected compaction, got {other:?}"),
        }
    }

    #[test]
    fn custom_round_trips() {
        let entry = SessionEntry::Custom(Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: "plugin:notes".into(),
            data: serde_json::json!({ "note": "remember to test" }),
        });
        let line = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn branch_round_trips() {
        let entry = SessionEntry::Branch(BranchEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            at: EntryId::new(),
        });
        let line = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(entry, back);
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "branch");
    }

    #[test]
    fn entry_id_helpers() {
        let id = EntryId::new();
        let entry = SessionEntry::ThinkingLevelChange(ThinkingLevelChange {
            id,
            ts: Utc::now(),
            level: "high".into(),
        });
        assert_eq!(entry.id(), id);
    }

    #[test]
    fn external_presentation_records_round_trip_in_order() {
        let text = |s: &str| PresentationText::new(s.into()).unwrap();
        let events = vec![
            ExternalAcpPresentation::Prompt { text: text("hi") },
            ExternalAcpPresentation::AgentMessage {
                item_id: text("item-1"),
                text: text("hello"),
            },
            ExternalAcpPresentation::AgentThought {
                item_id: text("item-2"),
                text: text("thinking"),
            },
            ExternalAcpPresentation::ToolCall {
                tool_call_id: text("tool-1"),
                title: text("read"),
                status: PresentationToolStatus::Pending,
            },
            ExternalAcpPresentation::ToolCallUpdate {
                tool_call_id: text("tool-1"),
                title: None,
                status: Some(PresentationToolStatus::Completed),
                text: Some(text("done")),
            },
            ExternalAcpPresentation::TurnEnd {
                outcome: PresentationTurnOutcome::EndTurn,
            },
        ];
        let lines: Vec<_> = events
            .iter()
            .cloned()
            .map(|event| {
                serde_json::to_string(&event.into_entry(EntryId::new(), Utc::now())).unwrap()
            })
            .collect();
        let decoded: Vec<_> = lines
            .iter()
            .map(|line| {
                let SessionEntry::Custom(custom) = serde_json::from_str(line).unwrap() else {
                    panic!()
                };
                ExternalAcpPresentation::from_custom(&custom)
                    .unwrap()
                    .unwrap()
            })
            .collect();
        assert_eq!(decoded, events);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&lines[0]).unwrap()["kind"],
            EXTERNAL_ACP_PRESENTATION_KIND
        );
    }

    #[test]
    fn external_presentation_rejects_unknown_and_sensitive_fields_on_both_paths() {
        let valid = serde_json::json!({"event":"agent_message", "item_id":"1", "text":"hi"});
        let mut custom = Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: EXTERNAL_ACP_PRESENTATION_KIND.into(),
            data: valid.clone(),
        };
        for field in [
            "_meta",
            "raw_input",
            "raw_output",
            "permission",
            "authentication",
            "jsonrpc",
        ] {
            custom.data = valid.clone();
            custom.data[field] = serde_json::json!({"secret":"value"});
            assert!(ExternalAcpPresentation::from_custom(&custom).is_err());
            assert!(serde_json::to_string(&custom).is_err());
            let raw = serde_json::json!({"type":"custom", "id":custom.id, "ts":custom.ts, "kind":custom.kind, "data":custom.data});
            assert!(serde_json::from_value::<SessionEntry>(raw).is_err());
        }
        for event in [
            "permission_request",
            "permission_response",
            "authenticate",
            "future",
        ] {
            custom.data = serde_json::json!({"event":event});
            assert!(serde_json::to_string(&custom).is_err());
        }
        custom.data = serde_json::json!({"event":"tool_call", "tool_call_id":"1", "title":"x", "status":"invented"});
        assert!(serde_json::to_string(&custom).is_err());
        custom.kind = "plugin:notes".into();
        assert!(
            ExternalAcpPresentation::from_custom(&custom)
                .unwrap()
                .is_none()
        );
        assert!(serde_json::to_string(&custom).is_ok());
    }

    #[test]
    fn external_presentation_rejects_duplicate_fields_in_raw_json() {
        let id = EntryId::new();
        let ts = Utc::now();
        for data in [
            r#"{"event":"prompt","text":"first","text":"second"}"#,
            r#"{"event":"prompt","event":"prompt","text":"hi"}"#,
            r#"{"event":"prompt","text":"first","t\u0065xt":"second"}"#,
            r#"{"event":"tool_call_update","tool_call_id":"1","title":null,"title":"x"}"#,
        ] {
            for data_first in [true, false] {
                let fields = if data_first {
                    format!(r#""data":{data},"kind":"{EXTERNAL_ACP_PRESENTATION_KIND}""#)
                } else {
                    format!(r#""kind":"{EXTERNAL_ACP_PRESENTATION_KIND}","data":{data}"#)
                };
                let raw = format!(r#"{{"id":"{id}","ts":"{ts}",{fields}}}"#);
                assert!(serde_json::from_str::<Custom>(&raw).is_err(), "{raw}");
                let entry = format!(r#"{{"type":"custom","id":"{id}","ts":"{ts}",{fields}}}"#);
                assert!(
                    serde_json::from_str::<SessionEntry>(&entry).is_err(),
                    "{entry}"
                );
            }
        }

        let raw = format!(
            r#"{{"type":"custom","id":"{id}","ts":"{ts}","data":{{"event":"prompt","text":"ok"}},"kind":"{EXTERNAL_ACP_PRESENTATION_KIND}"}}"#
        );
        assert!(serde_json::from_str::<SessionEntry>(&raw).is_ok());

        let raw = format!(
            r#"{{"type":"custom","id":"{id}","ts":"{ts}","data":{{"key":1,"key":2}},"kind":"plugin:example"}}"#
        );
        let SessionEntry::Custom(custom) = serde_json::from_str(&raw).unwrap() else {
            panic!()
        };
        assert_eq!(custom.data["key"], 2);
    }

    #[test]
    fn external_presentation_text_is_bounded_in_bytes_on_read_and_write() {
        assert!(PresentationText::new("x".repeat(EXTERNAL_ACP_TEXT_LIMIT)).is_ok());
        assert!(PresentationText::new("\u{e9}".repeat(EXTERNAL_ACP_TEXT_LIMIT / 2 + 1)).is_err());
        let custom = Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: EXTERNAL_ACP_PRESENTATION_KIND.into(),
            data: serde_json::json!({"event":"prompt", "text":"x".repeat(EXTERNAL_ACP_TEXT_LIMIT + 1)}),
        };
        assert!(serde_json::to_string(&custom).is_err());
        let raw = serde_json::json!({"type":"custom", "id":custom.id, "ts":custom.ts, "kind":custom.kind, "data":custom.data});
        assert!(serde_json::from_value::<SessionEntry>(raw).is_err());
    }

    #[test]
    fn format_version_is_four() {
        assert_eq!(FORMAT_VERSION, 4);
    }
}
