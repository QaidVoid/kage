//! Edit tool cards: whole-line diffs and the edit approval preview.

use super::*;

const SAMPLE_BEFORE: &str = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
const SAMPLE_AFTER: &str = "fn main() {\n    let x = 1;\n    let y = 3;\n    let z = 4;\n}\n";

fn sample_edit(old: &str) -> serde_json::Value {
    serde_json::json!({"path": "sample.rs", "old_str": old, "new_str": "let y = 3;\n    let z = 4;"})
}

fn edit_events(id: &str, old: &str, is_error: bool) -> Vec<kage_core::protocol::Event> {
    let id = kage_core::ToolCallId::new(id);
    vec![
        kage_core::LoopEvent::ToolCallStart {
            id: id.clone(),
            name: "edit".into(),
            input_partial: sample_edit(old),
        }
        .into(),
        kage_core::LoopEvent::ToolExecutionStart { id: id.clone() }.into(),
        kage_core::LoopEvent::ToolCallEnd {
            id,
            output: kage_core::ToolOutput {
                is_error,
                text: if is_error {
                    "`old_str` not found in sample.rs"
                } else {
                    "edited"
                }
                .into(),
                structured: None,
                terminate: false,
            },
        }
        .into(),
    ]
}

fn edit_rows(app: &mut App) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    app.render_into(&mut terminal).unwrap();
    snapshot_rows(&terminal)
}

#[test]
fn a_finished_edit_shows_whole_lines_of_its_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.rs"), SAMPLE_AFTER).unwrap();
    let (mut app, _rx, events) = app_with_events();
    app.set_workdir(dir.path().to_path_buf());
    feed(&mut app, &events, edit_events("c1", "let y = 2;", false));
    let rows = edit_rows(&mut app);
    let has = |tail: &str| rows.iter().any(|r| r.ends_with(tail));
    assert!(
        rows.iter().any(|r| r.contains("Edited sample.rs (+2 -1)")),
        "{rows:#?}"
    );
    assert!(has("-     let y = 2;"), "{rows:#?}");
    assert!(has("+     let y = 3;"), "{rows:#?}");
    assert!(has("+     let z = 4;"), "{rows:#?}");
}

#[test]
fn a_resumed_session_shows_line_diffs_and_no_diff_for_a_failed_edit() {
    use kage_core::{Content, Message, Role, ToolCallId};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.rs"), SAMPLE_AFTER).unwrap();
    let (mut app, _rx, events) = app_with_events();
    app.set_workdir(dir.path().to_path_buf());
    let turn = |id: &str, old: &str, is_error: bool| {
        [
            Message::new(
                Role::Assistant,
                vec![Content::ToolCall {
                    id: ToolCallId::new(id),
                    name: "edit".into(),
                    input: sample_edit(old),
                }],
                None,
            ),
            Message::new(
                Role::ToolResult,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId::new(id),
                    output: if is_error {
                        "`old_str` not found"
                    } else {
                        "edited"
                    }
                    .into(),
                    is_error,
                }],
                None,
            ),
        ]
    };
    let messages = turn("c1", "let y = 2;", false)
        .into_iter()
        .chain(turn("c2", "let q = 9;", true))
        .collect();
    let changed = kage_core::protocol::HostEvent::SessionChanged {
        path: dir.path().join("s.jsonl"),
        title: None,
        messages,
        compaction: None,
    };
    feed(&mut app, &events, vec![changed.into()]);
    app.set_all_folds(false);
    let rows = edit_rows(&mut app);
    let count = |tail: &str| rows.iter().filter(|r| r.ends_with(tail)).count();
    assert_eq!(count("-     let y = 2;"), 1, "{rows:#?}");
    assert_eq!(count("- let q = 9;"), 0, "{rows:#?}");
    assert_eq!(count("`old_str` not found"), 1, "{rows:#?}");
}

#[test]
fn the_edit_approval_previews_the_file_lines_or_says_the_text_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.rs"), SAMPLE_BEFORE).unwrap();
    let (mut app, _rx, events) = app_with_events();
    app.set_workdir(dir.path().to_path_buf());
    let request = |old: &str, request: u64| -> kage_core::protocol::Event {
        kage_core::protocol::HostEvent::PermissionRequested {
            request_id: kage_core::protocol::RequestId(request),
            tool_call_id: None,
            tool: "edit".into(),
            subject: "sample.rs".into(),
            input: sample_edit(old),
        }
        .into()
    };
    feed(&mut app, &events, vec![request("let y = 2;", 1)]);
    let rows = edit_rows(&mut app);
    assert!(rows.iter().any(|r| r == "   -     let y = 2;"), "{rows:#?}");
    assert!(rows.iter().any(|r| r == "   +     let z = 4;"), "{rows:#?}");
    app.answer_permission(PermissionDecision::Deny);

    feed(&mut app, &events, vec![request("let q = 9;", 2)]);
    let rows = edit_rows(&mut app);
    assert!(
        rows.iter()
            .any(|r| r == "   the text to replace is not in sample.rs"),
        "{rows:#?}"
    );
    assert!(!rows.iter().any(|r| r.contains("let q = 9;")), "{rows:#?}");
}

#[test]
fn a_waiting_edit_without_its_text_shows_no_diffstat() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.rs"), SAMPLE_BEFORE).unwrap();
    let waiting_rows = |old: &str| {
        let (mut app, _rx, events) = app_with_events();
        app.set_workdir(dir.path().to_path_buf());
        let id = kage_core::ToolCallId::new("c1");
        let start = kage_core::LoopEvent::ToolCallStart {
            id: id.clone(),
            name: "edit".into(),
            input_partial: sample_edit(old),
        };
        let request = kage_core::protocol::HostEvent::PermissionRequested {
            request_id: kage_core::protocol::RequestId(1),
            tool_call_id: Some(id),
            tool: "edit".into(),
            subject: "sample.rs".into(),
            input: sample_edit(old),
        };
        feed(&mut app, &events, vec![start.into(), request.into()]);
        edit_rows(&mut app)
    };
    let found = waiting_rows("let y = 2;");
    assert!(found.iter().any(|r| r.contains("(+2 -1)")), "{found:#?}");
    let missing = waiting_rows("let q = 9;");
    assert!(
        missing.iter().any(|r| r.contains("Edit sample.rs")),
        "{missing:#?}"
    );
    assert!(!missing.iter().any(|r| r.contains("(+")), "{missing:#?}");
}
