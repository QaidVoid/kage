//! End-to-end checks that exercise multiple kage-session APIs together.

use std::path::PathBuf;

use chrono::Utc;
use kage_core::{Content, Message, Role, ToolCallId};
use kage_session::{
    Compaction, EntryId, FORMAT_VERSION, Header, Label, MessageEntry, ModelChange, SessionEntry,
    SessionId, SessionReader, SessionWriter, fork, replay, resolve_entry_prefix, search,
};
use tempfile::tempdir;

fn fresh_header() -> Header {
    Header {
        version: FORMAT_VERSION,
        session: SessionId::new(),
        id: EntryId::new(),
        ts: Utc::now(),
        cwd: PathBuf::from("/tmp/work"),
        model: "anthropic:claude-sonnet-4-6".into(),
        system_prompt: "You are kage.".into(),
        parent_session: None,
        parent_entry: None,
    }
}

#[test]
fn header_always_carries_explicit_version() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    let header = fresh_header();
    let mut w = SessionWriter::create(&path, header.clone()).unwrap();
    // Add one of each non-header entry type; none of them should accidentally
    // carry a version field, but the header line must always have `"version": 1`.
    w.append(&SessionEntry::Message(MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Message::new(Role::User, vec![Content::Text { text: "hi".into() }], None),
    }))
    .unwrap();
    w.append(&SessionEntry::Compaction(Compaction {
        id: EntryId::new(),
        ts: Utc::now(),
        kept: 4,
        summarized: 8,
        summary: "[summary of 8 turns]\nthings happened".into(),
    }))
    .unwrap();
    drop(w);

    let raw = std::fs::read_to_string(&path).unwrap();
    let mut lines = raw.split_terminator('\n');
    let header_line = lines.next().expect("header present");
    let header_json: serde_json::Value = serde_json::from_str(header_line).unwrap();
    assert_eq!(header_json["version"], serde_json::json!(FORMAT_VERSION));
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(
            v.get("version").is_none(),
            "non-header entry must not carry a `version` field: {line}"
        );
    }
}

#[test]
fn full_round_trip_preserves_every_entry_kind() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("kitchen-sink.jsonl");
    let header = fresh_header();

    let user = MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Message::new(Role::User, vec![Content::Text { text: "go".into() }], None),
    };
    let assistant = MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Message::new(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: "thinking".into(),
                },
                Content::Text {
                    text: "answer".into(),
                },
                Content::ToolCall {
                    id: ToolCallId::new("c1"),
                    name: "echo".into(),
                    input: serde_json::json!({"k": "v"}),
                },
            ],
            None,
        ),
    };
    let tool_result = MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("c1"),
                output: "echoed".into(),
                is_error: false,
            }],
            None,
        ),
    };
    let label = Label {
        id: EntryId::new(),
        ts: Utc::now(),
        text: "milestone".into(),
        anchor: assistant.id,
    };
    let model_change = ModelChange {
        id: EntryId::new(),
        ts: Utc::now(),
        model: "openai:gpt-4o".into(),
    };

    let entries: Vec<SessionEntry> = vec![
        SessionEntry::Message(user.clone()),
        SessionEntry::Message(assistant.clone()),
        SessionEntry::Message(tool_result.clone()),
        SessionEntry::Label(label.clone()),
        SessionEntry::ModelChange(model_change.clone()),
    ];
    let mut w = SessionWriter::create(&path, header.clone()).unwrap();
    for entry in &entries {
        w.append(entry).unwrap();
    }
    drop(w);

    let read_back: Vec<SessionEntry> = SessionReader::iter(&path)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(read_back.len(), entries.len() + 1);
    assert!(matches!(&read_back[0], SessionEntry::Header(h) if h.session == header.session));
    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(&read_back[i + 1], entry);
    }
}

#[test]
fn fork_is_self_consistent_with_replay() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.jsonl");
    let header = fresh_header();
    let parent_session = header.session;
    let m1 = MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Message::new(Role::User, vec![Content::Text { text: "ask".into() }], None),
    };
    let m2 = MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Message::new(
            Role::Assistant,
            vec![Content::Text {
                text: "answer".into(),
            }],
            None,
        ),
    };
    let m3 = MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Message::new(
            Role::User,
            vec![Content::Text {
                text: "follow up".into(),
            }],
            None,
        ),
    };
    {
        let mut w = SessionWriter::create(&src_path, header).unwrap();
        for entry in [&m1, &m2, &m3] {
            w.append(&SessionEntry::Message(entry.clone())).unwrap();
        }
    }

    // Fork at m2: forked session should replay back to [m1, m2].
    let dst_path = dir.path().join("forked.jsonl");
    let new_session = SessionId::new();
    fork(&src_path, &dst_path, new_session, m2.id).unwrap();

    let forked = replay(&dst_path).unwrap();
    assert_eq!(forked.history.len(), 2);
    assert_eq!(forked.header.session, new_session);
    assert_eq!(forked.header.parent_session, Some(parent_session));
    assert_eq!(forked.header.parent_entry, Some(m2.id));

    // Resolving an entry id by full string round-trips through the API.
    let resolved = resolve_entry_prefix(&src_path, &m3.id.to_string()).unwrap();
    assert_eq!(resolved, m3.id);
}

#[test]
fn search_indexes_assistant_text_and_user_prompts() {
    let dir = tempdir().unwrap();
    let path_a = dir.path().join("a.jsonl");
    let mut w = SessionWriter::create(&path_a, fresh_header()).unwrap();
    w.append(&SessionEntry::Message(MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Message::new(
            Role::User,
            vec![Content::Text {
                text: "tell me about migration safety".into(),
            }],
            None,
        ),
    }))
    .unwrap();
    w.append(&SessionEntry::Message(MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Message::new(
            Role::Assistant,
            vec![Content::Text {
                text: "migrations should be reversible".into(),
            }],
            None,
        ),
    }))
    .unwrap();
    drop(w);

    let hits = search(dir.path(), "migration").unwrap();
    assert_eq!(hits.len(), 2);
    let parsed: Vec<_> = hits
        .iter()
        .filter_map(kage_session::SearchHit::entry)
        .collect();
    assert_eq!(parsed.len(), 2);
}
