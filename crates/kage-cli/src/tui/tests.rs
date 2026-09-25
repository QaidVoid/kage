//! Tests for the TUI host glue.

use std::path::PathBuf;

use chrono::Utc;
use kage_session::{EntryId, FORMAT_VERSION, Header, SessionId};

use kage_core::{Message, Role};

use super::*;

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
                    signature: None,
                    duration_ms: None,
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
        title: None,
        compaction: None,
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
        agent: None,
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
        old.with_timezone(&chrono::Local)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
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

#[test]
fn options_set_in_init_lua_seed_the_first_session() {
    let user = tempfile::tempdir().unwrap();
    std::fs::write(
        user.path().join("init.lua"),
        "kage.opt.thinking_level = 'high'\nkage.opt.compaction_threshold = 0.5\n\
         kage.opt.agent_max_depth = 2\nkage.opt.agent_max_running = 8",
    )
    .unwrap();
    let options = kage_plugin::SharedOptions::default();
    let rt = PluginRuntime::builder()
        .user_dir(Some(user.path().to_path_buf()))
        .options(Arc::clone(&options))
        .build()
        .unwrap();
    let report = kage_plugin::load_all(None, &rt).unwrap();
    assert_eq!(report.init, Some(Ok(())));
    let (loop_cfg, thinking, max_depth, max_running) = startup_options(&options);
    assert_eq!(thinking, Some(kage_core::ThinkingLevel::High));
    assert_eq!((max_depth, max_running), (2, 8));
    assert!((loop_cfg.compaction_threshold - 0.5).abs() < f32::EPSILON);
}
