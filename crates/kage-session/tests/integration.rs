//! End-to-end checks that exercise multiple kage-session APIs together.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use kage_core::{Content, Message, Role, ThinkingSignature, ToolCallId};
use kage_session::{
    Compaction, EntryId, FORMAT_VERSION, Header, Label, MessageEntry, ModelChange, SessionEntry,
    SessionError, SessionId, SessionReader, SessionWriter, fork, replay, resolve_entry_prefix,
    search,
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

fn label_entry(text: &str) -> SessionEntry {
    SessionEntry::Label(Label {
        id: EntryId::new(),
        ts: Utc::now(),
        text: text.into(),
        anchor: EntryId::new(),
    })
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
        message: Arc::new(Message::new(
            Role::User,
            vec![Content::Text { text: "hi".into() }],
            None,
        )),
        usage: None,
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
        message: Arc::new(Message::new(
            Role::User,
            vec![Content::Text { text: "go".into() }],
            None,
        )),
        usage: None,
    };
    let assistant = MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Arc::new(Message::new(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: "thinking".into(),
                    signature: Some(ThinkingSignature {
                        model: "claude-sonnet-4-6".into(),
                        data: "sig".into(),
                        redacted: false,
                    }),
                    duration_ms: Some(11_000),
                },
                Content::Thinking {
                    text: String::new(),
                    signature: Some(ThinkingSignature {
                        model: "claude-sonnet-4-6".into(),
                        data: "encrypted".into(),
                        redacted: true,
                    }),
                    duration_ms: None,
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
        )),
        usage: None,
    };
    let tool_result = MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Arc::new(Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: ToolCallId::new("c1"),
                output: "echoed".into(),
                is_error: false,
            }],
            None,
        )),
        usage: None,
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
fn thinking_from_older_sessions_loads_and_writes_unchanged() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("old.jsonl");
    let entry = SessionEntry::Message(MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Arc::new(Message::new(
            Role::Assistant,
            vec![Content::Thinking {
                text: "hmm".into(),
                signature: None,
                duration_ms: None,
            }],
            None,
        )),
        usage: None,
    });
    let mut w = SessionWriter::create(&path, fresh_header()).unwrap();
    w.append(&entry).unwrap();
    drop(w);

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains(r#"{"type":"thinking","text":"hmm"}"#), "{raw}");
    assert!(!raw.contains("signature"), "{raw}");
    assert!(!raw.contains("duration_ms"), "{raw}");
    let read_back: Vec<SessionEntry> = SessionReader::iter(&path)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(read_back[1], entry);
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
        message: Arc::new(Message::new(
            Role::User,
            vec![Content::Text { text: "ask".into() }],
            None,
        )),
        usage: None,
    };
    let m2 = MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Arc::new(Message::new(
            Role::Assistant,
            vec![Content::Text {
                text: "answer".into(),
            }],
            None,
        )),
        usage: None,
    };
    let m3 = MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Arc::new(Message::new(
            Role::User,
            vec![Content::Text {
                text: "follow up".into(),
            }],
            None,
        )),
        usage: None,
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
        message: Arc::new(Message::new(
            Role::User,
            vec![Content::Text {
                text: "tell me about migration safety".into(),
            }],
            None,
        )),
        usage: None,
    }))
    .unwrap();
    w.append(&SessionEntry::Message(MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Arc::new(Message::new(
            Role::Assistant,
            vec![Content::Text {
                text: "migrations should be reversible".into(),
            }],
            None,
        )),
        usage: None,
    }))
    .unwrap();
    drop(w);

    // A second session in the same dir whose text also matches, so hit
    // identity across files is observable.
    let path_b = dir.path().join("b.jsonl");
    let mut wb = SessionWriter::create(&path_b, fresh_header()).unwrap();
    wb.append(&SessionEntry::Message(MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Arc::new(Message::new(
            Role::User,
            vec![Content::Text {
                text: "migration rollback plan".into(),
            }],
            None,
        )),
        usage: None,
    }))
    .unwrap();
    drop(wb);

    let hits = search(dir.path(), "migration", 100).unwrap();
    assert_eq!(hits.len(), 3);
    let parsed: Vec<_> = hits
        .iter()
        .filter_map(kage_session::SearchHit::entry)
        .collect();
    assert_eq!(parsed.len(), 3);
    // Every hit names its file, so a wrong-file hit cannot pass.
    let mut count_by_file: std::collections::BTreeMap<std::path::PathBuf, usize> =
        std::collections::BTreeMap::new();
    for hit in &hits {
        *count_by_file.entry(hit.path.clone()).or_default() += 1;
    }
    let mut counted: Vec<(std::path::PathBuf, usize)> = count_by_file.into_iter().collect();
    counted.sort();
    assert_eq!(
        counted,
        vec![(path_a.clone(), 2), (path_b.clone(), 1)],
        "each file contributes its own hits"
    );
    let hit_for = |path: &std::path::Path| -> Vec<&kage_session::SearchHit> {
        hits.iter().filter(|h| h.path == path).collect()
    };
    let hits_a = hit_for(&path_a);
    assert_eq!(hits_a.len(), 2);
    for hit in &hits_a {
        match hit.entry().expect("hit decodes to a message") {
            SessionEntry::Message(message) => {
                let text = message
                    .message
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                assert!(
                    text.contains("migration"),
                    "hit line is the matched message: {text:?}"
                );
            }
            other => panic!("hit decoded to {other:?}"),
        }
    }
    let hits_b = hit_for(&path_b);
    assert_eq!(hits_b.len(), 1);
}

/// The reader and writer sides of crash tolerance must agree: over
/// hand-damaged files, `open` plus append plus `replay` either
/// round-trips every durable terminated line or returns an error,
/// never a history silently missing a parseable line.
#[test]
fn damaged_files_round_trip_or_error_but_never_lose_a_line() {
    let dir = tempdir().unwrap();

    // A torn (unterminated) tail is repaired: `open` truncates the
    // fragment, appends land whole, and replay reads every line.
    let torn = dir.path().join("torn.jsonl");
    {
        let mut w = SessionWriter::create(&torn, fresh_header()).unwrap();
        w.append(&label_entry("kept")).unwrap();
    }
    let mut raw = std::fs::read(&torn).unwrap();
    raw.extend_from_slice(b"{\"type\":\"label\",\"id\":\"01");
    std::fs::write(&torn, raw).unwrap();
    let mut w = SessionWriter::open(&torn).unwrap();
    w.append(&label_entry("after")).unwrap();
    drop(w);
    let entries: Vec<_> = SessionReader::iter(&torn)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(entries.len(), 3, "every durable line round-trips");

    // A terminated corrupt tail is a hard decode error on replay,
    // never a silently shortened history.
    let corrupt = dir.path().join("corrupt.jsonl");
    {
        let mut w = SessionWriter::create(&corrupt, fresh_header()).unwrap();
        w.append(&label_entry("kept")).unwrap();
    }
    let mut raw = std::fs::read(&corrupt).unwrap();
    raw.extend_from_slice(b"{\"corrupt\": true}\n");
    std::fs::write(&corrupt, raw).unwrap();
    SessionWriter::open(&corrupt).unwrap();
    assert!(
        matches!(replay(&corrupt), Err(SessionError::Decode { .. })),
        "the corrupt line must surface, not vanish"
    );

    // A crash mid-header: `open` refuses, `replay` fails with the
    // same cause, and the bytes on disk are untouched.
    let partial = dir.path().join("partial.jsonl");
    let partial_bytes = b"{\"type\":\"header\",\"vers";
    std::fs::write(&partial, partial_bytes).unwrap();
    assert!(matches!(
        SessionWriter::open(&partial),
        Err(SessionError::MissingHeader { .. })
    ));
    assert!(matches!(
        replay(&partial),
        Err(SessionError::MissingHeader { .. })
    ));
    assert_eq!(std::fs::read(&partial).unwrap(), partial_bytes);

    // An empty file is refused on both paths.
    let empty = dir.path().join("empty.jsonl");
    std::fs::write(&empty, b"").unwrap();
    assert!(matches!(
        SessionWriter::open(&empty),
        Err(SessionError::Empty { .. })
    ));
    assert!(matches!(replay(&empty), Err(SessionError::Empty { .. })));
}
