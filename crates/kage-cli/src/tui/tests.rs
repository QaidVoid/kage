//! Tests for the TUI host glue.

use std::path::{Path, PathBuf};

use chrono::Utc;
use kage_session::{
    EntryId, FORMAT_VERSION, Header, MessageEntry, SessionEntry, SessionId, SessionReader,
    SessionWriter,
};

use super::*;

fn write_session(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let header = Header {
        version: FORMAT_VERSION,
        session: SessionId::new(),
        id: EntryId::new(),
        ts: Utc::now(),
        cwd: PathBuf::from("/work"),
        model: "anthropic:claude".into(),
        system_prompt: "be helpful".into(),
        parent_session: None,
        parent_entry: None,
    };
    let mut writer = SessionWriter::create(&path, header).unwrap();
    writer
        .append(&SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Message::new(
                Role::User,
                vec![Content::Text {
                    text: "hello".to_owned(),
                }],
                None,
            ),
            usage: None,
        }))
        .unwrap();
    path
}

fn session_id_of(path: &Path) -> SessionId {
    let mut reader = SessionReader::iter(path).unwrap();
    match reader.next().unwrap().unwrap() {
        SessionEntry::Header(h) => h.session,
        other => panic!("expected header, got {other:?}"),
    }
}

#[test]
fn delete_session_removes_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_session(dir.path(), "doomed.jsonl");
    let buffer = shared_buffer();
    let toasts = shared_toasts();
    handle_delete_session(&path, None, &buffer, &toasts);
    assert!(!path.exists());
}

#[test]
fn delete_session_refuses_the_active_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_session(dir.path(), "live.jsonl");
    let active = Arc::new(Mutex::new(path.clone()));
    let buffer = shared_buffer();
    let toasts = shared_toasts();
    handle_delete_session(&path, Some(&active), &buffer, &toasts);
    assert!(path.exists(), "active session must not be deleted");
}

#[test]
fn render_session_markdown_covers_roles_and_blocks() {
    let header = Header {
        version: FORMAT_VERSION,
        session: SessionId::new(),
        id: EntryId::new(),
        ts: Utc::now(),
        cwd: PathBuf::from("/work"),
        model: "anthropic:claude".into(),
        system_prompt: "sp".into(),
        parent_session: None,
        parent_entry: None,
    };
    let history = vec![
        Message::new(
            Role::User,
            vec![Content::Text {
                text: "hi there".into(),
            }],
            None,
        ),
        Message::new(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: "consider\noptions".into(),
                },
                Content::Text {
                    text: "answer".into(),
                },
                Content::ToolCall {
                    id: kage_core::ToolCallId::new("c1"),
                    name: "read".into(),
                    input: serde_json::json!({"path": "x"}),
                },
            ],
            None,
        ),
        Message::new(
            Role::ToolResult,
            vec![Content::ToolResultBlock {
                call_id: kage_core::ToolCallId::new("c1"),
                output: "file body".into(),
                is_error: false,
            }],
            None,
        ),
    ];
    let replay = kage_session::ReplayResult {
        header,
        history,
        model: "anthropic:claude".into(),
        tool_durations: std::collections::HashMap::new(),
        usage_total: kage_session::ReplayUsage::default(),
        thinking_level: None,
    };
    let md = render_session_markdown(&replay);
    assert!(md.starts_with("# kage session "));
    assert!(md.contains("## User"));
    assert!(md.contains("## Assistant"));
    assert!(md.contains("## Tool"));
    assert!(md.contains("**thinking**"));
    assert!(md.contains("> consider"));
    assert!(md.contains("**tool call: `read`**"));
    assert!(md.contains("```json"));
    assert!(md.contains("file body"));
}

#[test]
fn handle_export_writes_markdown_to_the_given_path() {
    let dir = tempfile::tempdir().unwrap();
    let src = write_session(dir.path(), "s.jsonl");
    let active = Arc::new(Mutex::new(src.clone()));
    let out = dir.path().join("out.md");
    let buffer = shared_buffer();
    let toasts = shared_toasts();
    handle_export(Some(&active), Some(out.clone()), &buffer, &toasts);
    assert!(out.exists());
    let body = std::fs::read_to_string(&out).unwrap();
    assert!(body.contains("# kage session "));
    assert!(body.contains("## User"));
    assert!(body.contains("hello"));
}

#[test]
fn fork_file_creates_a_parent_linked_session() {
    let dir = tempfile::tempdir().unwrap();
    let src = write_session(dir.path(), "src.jsonl");
    let src_id = session_id_of(&src);
    let buffer = shared_buffer();
    let toasts = shared_toasts();
    handle_fork_file(&src, &buffer, &toasts);

    let forked = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl") && *p != src)
        .expect("a new session file should exist");
    let mut reader = SessionReader::iter(&forked).unwrap();
    match reader.next().unwrap().unwrap() {
        SessionEntry::Header(h) => assert_eq!(h.parent_session, Some(src_id)),
        other => panic!("expected header, got {other:?}"),
    }
}

fn summary(title: Option<&str>, prompt: Option<&str>) -> SessionSummary {
    SessionSummary {
        id: SessionId::new(),
        path: PathBuf::from("/s/a.jsonl"),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        cwd: PathBuf::from("/work"),
        model: "anthropic:claude".into(),
        parent_session: None,
        last_user_prompt: prompt.map(str::to_owned),
        title: title.map(str::to_owned),
        entry_count: 3,
    }
}

#[test]
fn relative_day_labels_today_yesterday_then_date() {
    let now = Utc::now();
    assert_eq!(relative_day(now), "Today");
    assert_eq!(relative_day(now - chrono::Duration::days(1)), "Yesterday");
    let old = now - chrono::Duration::days(9);
    assert_eq!(
        relative_day(old),
        old.date_naive().format("%Y-%m-%d").to_string()
    );
}

#[test]
fn label_prefers_title_then_prompt_then_placeholder() {
    let with_title = format_session_label(&summary(
        Some("Refactor auth"),
        Some("please refactor auth now"),
    ));
    assert!(with_title.contains("Refactor auth"), "{with_title}");
    assert!(!with_title.contains("please refactor"), "{with_title}");

    let prompt_only = format_session_label(&summary(None, Some("just the prompt")));
    assert!(prompt_only.contains("just the prompt"), "{prompt_only}");

    let neither = format_session_label(&summary(None, None));
    assert_eq!(
        neither, "(untitled session)",
        "label is just the title; date is the section header and \
             time is the picker's right column, neither baked in"
    );
}
