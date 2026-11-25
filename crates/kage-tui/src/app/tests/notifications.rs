//! Retained notifications, modal input routing, and transcript boundaries.

use super::*;
use crate::toast::{ToastKind, prune_expired, push_toast};
use kage_core::protocol::{HostEvent, NoticeLevel};
use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;

#[test]
fn slash_notifications_opens_retained_history_without_submitting() {
    let (mut app, rx, _events) = app_with_events();
    app.push_error("retained command failure");
    type_str(&mut app, "/notifications");
    app.handle_key(code(KeyCode::Enter));

    assert!(app.notifications_overlay.is_some());
    assert!(app.input.text().is_empty());
    assert!(rx_idle(&rx));
    assert!(lock(&app.buffer).blocks().is_empty());
    let rows = rendered(&mut app, 80, 18);
    assert!(
        rows.iter()
            .any(|row| row.contains("retained command failure")),
        "{rows:?}"
    );
}

#[test]
fn notifications_command_preserves_the_draft_and_editor_state() {
    let (mut app, rx, _events) = app_with_events();
    type_str(&mut app, "unfinished prompt");
    app.handle_key(code(KeyCode::Left));
    let cursor = app.input.cursor();
    let mode = app.input.mode();
    let pane = app.input.focused_pane();
    let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();
    let result = app.run_command_validated("notifications", &registry);
    assert!(matches!(result, CommandResult::Done(None)), "{result:?}");
    assert!(app.notifications_overlay.is_some());
    assert!(app.modal_open());
    app.handle_key(code(KeyCode::Esc));

    assert!(app.notifications_overlay.is_none());
    assert_eq!(app.input.text(), "unfinished prompt");
    assert_eq!(app.input.cursor(), cursor);
    assert_eq!(app.input.mode(), mode);
    assert_eq!(app.input.focused_pane(), pane);
    assert!(rx_idle(&rx));
    assert!(lock(&app.buffer).blocks().is_empty());
}

#[test]
fn notifications_swallow_unrelated_keys_and_paste() {
    let (mut app, rx, _events) = app_with_events();
    type_str(&mut app, "keep this draft");
    let cursor = app.input.cursor();
    app.open_notifications();
    for event in [
        key('x'),
        key('/'),
        code(KeyCode::Enter),
        code(KeyCode::Backspace),
        ctrl('p'),
    ] {
        assert_eq!(app.handle_key(event), None);
        assert!(app.notifications_overlay.is_some());
    }
    app.handle_paste("do not insert this\nsecond line");
    app.handle_paste("/missing/notification-paste.png");

    assert_eq!(app.input.text(), "keep this draft");
    assert_eq!(app.input.cursor(), cursor);
    assert!(app.picker.is_none());
    assert!(app.cmdline.is_none());
    assert!(app.input.attached().is_empty());
    assert!(lock(app.toasts.as_ref().unwrap()).history.is_empty());
    assert!(rx_idle(&rx));
}

#[test]
fn notifications_block_mouse_scroll_selection_and_context_menus() {
    let (mut app, _rx, _events) = app_with_events();
    lock(&app.buffer).push_user("transcript remains unchanged");
    let _terminal = render_app(&mut app);
    app.set_scroll(4);
    let pane = app.input.focused_pane();
    app.open_notifications();
    for kind in [
        MouseEventKind::ScrollDown,
        MouseEventKind::ScrollUp,
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.handle_mouse_event(MouseEvent {
            kind,
            column: 2,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
    }

    assert_eq!(lock(&app.buffer).scroll(), Some(4));
    assert!(app.context_menu.is_none());
    assert!(app.screen_selection.is_none());
    assert_eq!(app.input.focused_pane(), pane);
    assert!(app.notifications_overlay.is_some());
}

#[test]
fn expired_warnings_remain_in_history_without_entering_the_transcript() {
    let (mut app, _rx, events) = app_with_events();
    lock(&app.buffer).push_user("existing transcript");
    feed(
        &mut app,
        &events,
        vec![
            HostEvent::Notice {
                level: NoticeLevel::Warning,
                text: "warning details\nsecond diagnostic line".into(),
                transient: true,
            }
            .into(),
        ],
    );
    let toasts = app.toasts.as_ref().unwrap().clone();
    let notification = last_notification(&app);
    assert_eq!(notification.kind, ToastKind::Warning);
    assert_eq!(notification.text, "warning details\nsecond diagnostic line");
    assert!(!notification.reviewed);
    assert_eq!(lock(&app.buffer).blocks().len(), 1);

    prune_expired(
        &toasts,
        Instant::now() + crate::toast::DEFAULT_TOAST_DURATION + Duration::from_secs(1),
    );
    assert!(app.live_toasts().is_empty());
    assert_eq!(lock(&toasts).history.len(), 1);
    app.open_notifications();
    let rows = rendered(&mut app, 80, 18);
    for expected in ["WARNING", "warning details", "second diagnostic line"] {
        assert!(rows.iter().any(|row| row.contains(expected)), "{rows:?}");
    }
    assert_eq!(lock(&app.buffer).blocks().len(), 1);
}

#[test]
fn contextual_errors_and_run_failures_stay_inline() {
    let (mut app, _rx, events) = app_with_events();
    feed(
        &mut app,
        &events,
        vec![
            HostEvent::Notice {
                level: NoticeLevel::Error,
                text: "contextual failure for this turn".into(),
                transient: false,
            }
            .into(),
            kage_core::LoopEvent::Error {
                kind: kage_core::LoopError::Provider {
                    message: "provider run failure".into(),
                },
            }
            .into(),
        ],
    );

    let buffer = lock(&app.buffer);
    let text: Vec<_> = buffer
        .blocks()
        .iter()
        .filter_map(|block| match block.as_ref() {
            crate::buffer::Block::Custom { kind, text, .. } if kind == "kage:error" => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(text.len(), 2, "{text:?}");
    assert_eq!(text[0], "contextual failure for this turn");
    assert!(text[1].contains("provider run failure"), "{text:?}");
    assert!(lock(app.toasts.as_ref().unwrap()).history.is_empty());
}

#[test]
fn unread_attention_survives_expiry_until_explicitly_reviewed() {
    let (mut app, _rx, _events) = app_with_events();
    let toasts = app.toasts.as_ref().unwrap().clone();
    push_toast(
        &toasts,
        Toast::with_kind("warning", ToastKind::Warning, Duration::ZERO),
    );
    push_toast(
        &toasts,
        Toast::with_kind("error", ToastKind::Error, Duration::ZERO),
    );
    push_toast(
        &toasts,
        Toast::with_kind("information", ToastKind::Info, Duration::ZERO),
    );
    prune_expired(&toasts, Instant::now());
    let rows = rendered(&mut app, 80, 18);
    assert_footer_alerts(&rows, 2);

    app.open_notifications();
    app.handle_key(code(KeyCode::Down));
    assert_eq!(lock(&toasts).unread_count(), 2);
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(lock(&toasts).unread_count(), 2);
    app.open_notifications();
    app.handle_key(key('r'));
    assert!(app.notifications_overlay.is_some());
    app.handle_key(code(KeyCode::Esc));
    assert_eq!(lock(&toasts).unread_count(), 0);
    assert_eq!(lock(&toasts).history.len(), 3);
    assert!(lock(&toasts).history.iter().all(|entry| entry.reviewed));
    let rows = rendered(&mut app, 80, 18);
    assert_footer_alerts(&rows, 0);
}

#[test]
fn notification_badge_lifecycle_preserves_conversation_and_input_geometry() {
    let (mut app, _rx, _events) = app_with_events();
    {
        let mut buffer = lock(&app.buffer);
        for index in 0..40 {
            buffer.push_user(format!("conversation row {index:02}"));
        }
    }
    type_str(&mut app, "draft stays in place");
    app.set_scroll(4);
    let toasts = app.toasts.as_ref().unwrap().clone();
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    app.render_into(&mut terminal).unwrap();
    let plain = snapshot_rows(&terminal);
    assert_footer_alerts(&plain, 0);
    let original = conversation_geometry(&app);
    let input_top = usize::from(original.1 + original.3);
    assert!(plain[input_top].starts_with('\u{2500}'), "{plain:?}");
    let assert_frame = |app: &mut App, terminal: &mut Terminal<TestBackend>, count| {
        app.render_into(terminal).unwrap();
        let rows = snapshot_rows(terminal);
        assert_footer_alerts(&rows, count);
        assert_eq!(conversation_geometry(app), original);
        assert_eq!(
            &rows[input_top..rows.len() - 1],
            &plain[input_top..plain.len() - 1],
            "the input and existing footer rows must not move"
        );
        assert_eq!(app.input.text(), "draft stays in place");
        rows
    };
    let duration = Duration::from_secs(3600);
    push_toast(
        &toasts,
        Toast::with_kind("current information", ToastKind::Info, duration),
    );
    assert_frame(&mut app, &mut terminal, 0);
    let current = app.toast_hit.unwrap();
    mouse_at(&mut app, MouseEventKind::Moved, current.area.as_position());
    push_toast(
        &toasts,
        Toast::with_kind("queued warning", ToastKind::Warning, duration),
    );
    let rows = assert_frame(&mut app, &mut terminal, 1);
    assert!(rows.iter().any(|row| row.contains("current information")));
    assert!(rows.iter().all(|row| !row.contains("queued warning")));
    assert_eq!(app.toast_hit.unwrap().toast_id, current.toast_id);
    assert!(app.live_toasts()[0].deadline().is_none());

    push_toast(
        &toasts,
        Toast::with_kind("queued warning", ToastKind::Warning, duration),
    );
    assert_frame(&mut app, &mut terminal, 1);
    assert_eq!(lock(&toasts).history.back().unwrap().count, 2);
    push_toast(
        &toasts,
        Toast::with_kind("queued error", ToastKind::Error, duration),
    );
    assert_frame(&mut app, &mut terminal, 2);

    mouse_at(&mut app, MouseEventKind::Moved, Position::new(0, 23));
    let information_deadline = app.live_toasts()[0].deadline().unwrap();
    let warning_deadline = prune_expired(&toasts, information_deadline).unwrap();
    let rows = assert_frame(&mut app, &mut terminal, 2);
    assert!(rows.iter().any(|row| row.contains("queued warning")));
    prune_expired(&toasts, warning_deadline);
    let rows = assert_frame(&mut app, &mut terminal, 2);
    assert!(rows.iter().any(|row| row.contains("queued error")));
    assert!(rows.iter().all(|row| !row.contains("queued warning")));

    let error = app.toast_hit.unwrap();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        mouse_at(&mut app, kind, error.area.as_position());
    }
    assert_frame(&mut app, &mut terminal, 2);
    assert!(app.toast_hit.is_none());
    assert_eq!(lock(&toasts).unread_count(), 2);
    assert_eq!(lock(&toasts).history.len(), 3);
    app.open_notifications();
    app.handle_key(code(KeyCode::Esc));
    assert_frame(&mut app, &mut terminal, 2);

    push_toast(
        &toasts,
        Toast::with_kind("unrelated active card", ToastKind::Info, duration),
    );
    let rows = assert_frame(&mut app, &mut terminal, 2);
    assert!(rows.iter().any(|row| row.contains("unrelated active card")));
    let unrelated = app.toast_hit.unwrap();
    app.open_notifications();
    app.handle_key(key('r'));
    app.handle_key(code(KeyCode::Esc));
    let rows = assert_frame(&mut app, &mut terminal, 0);
    assert!(rows.iter().any(|row| row.contains("unrelated active card")));
    assert_eq!(app.toast_hit.unwrap().toast_id, unrelated.toast_id);
    assert_eq!(lock(&toasts).history.len(), 4);
    assert_eq!(lock(&toasts).unread_count(), 0);
    assert!(lock(&toasts).dismiss(unrelated.toast_id));
    let restored = assert_frame(&mut app, &mut terminal, 0);
    assert_eq!(restored, plain);
}

#[test]
fn completion_hides_the_card_but_leaves_unread_attention_visible() {
    let (mut app, _rx, _events) = app_with_events();
    let toasts = app.toasts.as_ref().unwrap().clone();
    push_toast(
        &toasts,
        Toast::with_kind("retained warning", ToastKind::Warning, Duration::ZERO),
    );
    prune_expired(&toasts, Instant::now());
    app.notify("floating notification");
    let visible = rendered(&mut app, 80, 18);
    assert!(
        visible
            .iter()
            .any(|row| row.contains("floating notification")),
        "{visible:?}"
    );
    let hit = app.toast_hit.unwrap();
    mouse_at(&mut app, MouseEventKind::Moved, hit.area.as_position());
    assert!(app.live_toasts()[0].deadline().is_none());
    app.input_completion =
        crate::overlay::InputCompletion::new(vec![kage_plugin::AutocompleteItem {
            label: "completion stays visible".into(),
            detail: None,
            value: "completed".into(),
            range: None,
        }]);
    let rows = rendered(&mut app, 80, 18);
    assert!(
        rows.iter()
            .all(|row| !row.contains("floating notification")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("completion stays visible")),
        "{rows:?}"
    );
    assert_footer_alerts(&rows, 1);
    assert!(app.toast_hit.is_none());
    assert!(app.live_toasts()[0].deadline().is_some());
    app.input_completion = None;
    let rows = rendered(&mut app, 80, 18);
    assert!(
        rows.iter().any(|row| row.contains("floating notification")),
        "{rows:?}"
    );
    assert!(app.toast_hit.is_some());
}

#[test]
fn approval_feedback_paste_outranks_open_notification_history() {
    let (mut app, rx, events) = app_with_events();
    type_str(&mut app, "keep my draft");
    app.open_notifications();
    feed(&mut app, &events, vec![permission_request("c1", 1)]);
    let ready = Instant::now() + crate::overlay::approval::TYPE_AHEAD_GUARD;
    app.approval_key_at(key('t'), ready);
    assert!(app.approval_panel.as_ref().unwrap().in_feedback());
    app.handle_paste("use a read-only command instead");
    app.approval_key_at(code(KeyCode::Enter), ready);
    let feedback = rx.try_iter().find_map(|request| match request {
        RunRequest::Submit { text, .. } => Some(text),
        _ => None,
    });
    assert_eq!(feedback.as_deref(), Some("use a read-only command instead"));
    assert_eq!(app.input.text(), "keep my draft");
}

fn conversation_geometry(app: &App) -> (u16, u16, u16, u16, usize, Option<usize>) {
    let buffer = lock(&app.buffer);
    (
        buffer.last_area_x(),
        buffer.last_area_y(),
        buffer.last_area_width(),
        buffer.last_area_height(),
        buffer.last_virtual_top(),
        buffer.scroll(),
    )
}

fn assert_footer_alerts(rows: &[String], count: usize) {
    assert!(
        rows.iter().all(|row| !row.contains("unread notification")),
        "obsolete unread summary: {rows:?}"
    );
    if count == 0 {
        assert!(rows.iter().all(|row| !row.contains(" alert")), "{rows:?}");
        return;
    }
    let badge = format!("{count} alert{}", if count == 1 { "" } else { "s" });
    let (footer, above) = rows.split_last().unwrap();
    assert!(footer.trim_start().starts_with(&badge), "{rows:?}");
    assert!(above.iter().all(|row| !row.contains(&badge)), "{rows:?}");
}

fn mouse_at(app: &mut App, kind: MouseEventKind, position: Position) {
    app.handle_mouse_event(MouseEvent {
        kind,
        column: position.x,
        row: position.y,
        modifiers: KeyModifiers::NONE,
    });
}

fn card_fixture() -> App {
    let (mut app, _rx, _events) = app_with_events();
    {
        let mut buffer = lock(&app.buffer);
        for i in 0..20 {
            buffer.push_tool_call(
                format!("call-{i}"),
                "read",
                serde_json::json!({"path": format!("file-{i}.rs")}),
            );
        }
        buffer.set_scroll(0);
    }
    type_str(&mut app, "unfinished draft");
    app.notify("floating card");
    let _rows = rendered(&mut app, 80, 24);
    assert!(app.toast_hit.is_some());
    app
}

#[test]
fn configured_startup_places_cards_in_each_conversation_corner() {
    use crate::view::toast::ToastPosition;
    for (name, expected) in [
        ("top-left", ToastPosition::TopLeft),
        ("top-right", ToastPosition::TopRight),
        ("bottom-left", ToastPosition::BottomLeft),
        ("bottom-right", ToastPosition::BottomRight),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, format!("[ui]\ntoast_position = \"{name}\"\n")).unwrap();
        let config = kage_core::config::Config::load(&path).unwrap();
        let (store, errors) = kage_core::options::OptionStore::from_config(&config);
        assert!(errors.is_empty(), "{errors:?}");
        let (mut app, _rx, _events) = app_with_events();
        app.set_options(Arc::new(Mutex::new(store)), None);
        lock(&app.buffer).push_user("conversation");
        app.notify("configured card");
        let rows = rendered(&mut app, 100, 24);
        assert!(
            rows.iter().any(|row| row.contains("configured card")),
            "{name}: {rows:?}"
        );
        assert_eq!(app.toast_position, expected);
        let hit = app.toast_hit.unwrap();
        let buffer = lock(&app.buffer);
        let left = buffer.last_area_x();
        let top = buffer.last_area_y();
        let bottom = top + buffer.last_area_height();
        if name.ends_with("left") {
            assert_eq!(hit.area.x, left + 2, "{name}");
        } else {
            assert_eq!(hit.area.right(), 98, "{name}");
        }
        if name.starts_with("top") {
            assert_eq!(hit.area.y, top + 1, "{name}");
        } else {
            assert_eq!(hit.area.bottom(), bottom - 1, "{name}");
        }
    }
}

#[test]
fn unread_attention_and_an_unrelated_card_are_independent() {
    let (mut app, _rx, _events) = app_with_events();
    let toasts = app.toasts.as_ref().unwrap().clone();
    push_toast(
        &toasts,
        Toast::with_kind(
            "original warning\nfull diagnostic",
            ToastKind::Warning,
            Duration::ZERO,
        ),
    );
    prune_expired(&toasts, Instant::now());
    app.notify("unrelated information");
    let rows = rendered(&mut app, 80, 18);
    assert!(
        rows.iter().any(|row| row.contains("unrelated information")),
        "{rows:?}"
    );
    assert_footer_alerts(&rows, 1);
    assert!(usize::from(app.toast_hit.unwrap().area.bottom()) < rows.len() - 1);
    assert_eq!(lock(&toasts).unread_count(), 1);
    app.open_notifications();
    let rows = rendered(&mut app, 80, 18);
    assert!(
        rows.iter().any(|row| row.contains("full diagnostic")),
        "{rows:?}"
    );
    assert_eq!(lock(&toasts).unread_count(), 1);
    app.handle_key(key('r'));
    app.handle_key(code(KeyCode::Esc));
    let rows = rendered(&mut app, 80, 18);
    assert_footer_alerts(&rows, 0);
    assert!(
        rows.iter().any(|row| row.contains("unrelated information")),
        "{rows:?}"
    );
    assert_eq!(
        lock(&toasts).history[0].text,
        "original warning\nfull diagnostic"
    );
}

#[test]
fn card_mouse_gestures_preserve_the_buffer_draft_and_unread_history() {
    let mut app = card_fixture();
    let toasts = app.toasts.as_ref().unwrap().clone();
    push_toast(
        &toasts,
        Toast::with_kind("warning card", ToastKind::Warning, Duration::from_secs(5)),
    );
    let rows = rendered(&mut app, 80, 24);
    assert!(rows.iter().any(|row| row.contains("floating card")));
    assert!(rows.iter().all(|row| !row.contains("warning card")));
    assert_footer_alerts(&rows, 1);
    let hit = app.toast_hit.unwrap();
    assert_eq!(hit.toast_id, lock(&toasts).active[0].id);
    let point = Position::new(hit.area.x + 1, hit.area.y + 1);
    let before = {
        let buffer = lock(&app.buffer);
        (
            buffer.scroll(),
            buffer.focus(),
            format!("{:?}", buffer.blocks()),
        )
    };
    let pane = app.input.focused_pane();
    let cursor = app.input.cursor();
    for kind in [
        MouseEventKind::ScrollDown,
        MouseEventKind::ScrollUp,
        MouseEventKind::Down(MouseButton::Right),
    ] {
        mouse_at(&mut app, kind, point);
    }
    mouse_at(&mut app, MouseEventKind::Down(MouseButton::Left), point);
    mouse_at(&mut app, MouseEventKind::Up(MouseButton::Left), point);
    assert!(app.context_menu.is_none());
    assert!(app.screen_selection.is_none());
    assert!(app.mouse_drag_anchor.is_none());
    assert_eq!(app.input.text(), "unfinished draft");
    assert_eq!(app.input.cursor(), cursor);
    assert_eq!(app.input.focused_pane(), pane);
    let rows = rendered(&mut app, 80, 24);
    assert!(rows.iter().any(|row| row.contains("warning card")));
    assert!(rows.iter().all(|row| !row.contains("floating card")));
    let buffer = lock(&app.buffer);
    assert_eq!(
        (
            buffer.scroll(),
            buffer.focus(),
            format!("{:?}", buffer.blocks())
        ),
        before
    );
    let state = lock(&toasts);
    assert_eq!(state.active.len(), 1);
    assert_eq!(state.active[0].text, "warning card");
    assert!(state.active.iter().all(|toast| toast.id != hit.toast_id));
    assert_eq!(state.history.len(), 2);
    assert_eq!(state.unread_count(), 1);
    assert!(state.history.iter().all(|entry| !entry.reviewed));
    assert!(app.notifications_overlay.is_none());
}

#[test]
fn a_new_arrival_does_not_steal_the_pressed_card() {
    let mut app = card_fixture();
    let first = app.toast_hit.unwrap();
    mouse_at(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        first.area.as_position(),
    );
    app.notify("queued card");
    let rows = rendered(&mut app, 80, 24);
    let current = app.toast_hit.unwrap();
    assert_eq!(current.toast_id, first.toast_id);
    assert!(rows.iter().any(|row| row.contains("floating card")));
    assert!(rows.iter().all(|row| !row.contains("queued card")));
    mouse_at(
        &mut app,
        MouseEventKind::Up(MouseButton::Left),
        current.area.as_position(),
    );
    assert!(app.toast_mouse_down.is_none());
    let rows = rendered(&mut app, 80, 24);
    assert_ne!(app.toast_hit.unwrap().toast_id, first.toast_id);
    assert!(rows.iter().any(|row| row.contains("queued card")));
    assert_eq!(app.live_toasts().len(), 1);
    assert_eq!(app.live_toasts()[0].text, "queued card");
    assert!(app.screen_selection.is_none());
}

#[test]
fn a_stale_card_press_cannot_dismiss_the_next_card_after_advancement() {
    let mut app = card_fixture();
    let toasts = app.toasts.as_ref().unwrap().clone();
    app.notify("next queued card");
    let _rows = rendered(&mut app, 80, 24);
    let first = app.toast_hit.unwrap();
    mouse_at(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        first.area.as_position(),
    );
    assert!(lock(&toasts).dismiss(first.toast_id));
    let _rows = rendered(&mut app, 80, 24);
    let next = app.toast_hit.unwrap();
    assert_ne!(next.toast_id, first.toast_id);
    mouse_at(
        &mut app,
        MouseEventKind::Up(MouseButton::Left),
        next.area.as_position(),
    );
    assert!(app.toast_mouse_down.is_none());
    assert_eq!(app.live_toasts().len(), 1);
    assert_eq!(app.live_toasts()[0].id, next.toast_id);
    assert!(app.screen_selection.is_none());
    assert_eq!(lock(&toasts).history.len(), 2);
}

#[test]
fn dragging_outside_a_card_does_not_select_or_scroll_the_conversation() {
    let mut app = card_fixture();
    let hit = app.toast_hit.unwrap();
    let before = lock(&app.buffer).scroll();
    mouse_at(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        hit.area.as_position(),
    );
    for point in [Position::new(0, 0), Position::new(0, 23)] {
        mouse_at(&mut app, MouseEventKind::Drag(MouseButton::Left), point);
        assert!(app.screen_selection.is_none());
        assert!(app.mouse_drag_anchor.is_none());
        assert_eq!(lock(&app.buffer).scroll(), before);
    }
    mouse_at(
        &mut app,
        MouseEventKind::Up(MouseButton::Left),
        Position::new(0, 23),
    );
    assert!(app.toast_mouse_down.is_none());
    assert!(
        app.live_toasts()
            .iter()
            .any(|toast| toast.id == hit.toast_id)
    );
    assert_eq!(app.input.text(), "unfinished draft");
}

#[test]
fn a_modal_hides_the_card_resumes_expiry_and_owns_the_mouse() {
    let mut app = card_fixture();
    let hit = app.toast_hit.unwrap();
    mouse_at(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        hit.area.as_position(),
    );
    assert!(app.live_toasts()[0].deadline().is_none());
    let before = lock(&app.buffer).scroll();
    app.open_notifications();
    let _rows = rendered(&mut app, 80, 24);
    assert!(app.toast_hit.is_none());
    assert!(app.live_toasts()[0].deadline().is_some());
    for kind in [
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::ScrollDown,
        MouseEventKind::Down(MouseButton::Right),
    ] {
        mouse_at(&mut app, kind, hit.area.as_position());
    }
    assert!(app.toast_mouse_down.is_none());
    assert_eq!(app.live_toasts().len(), 1);
    assert_eq!(lock(&app.buffer).scroll(), before);
    assert!(app.context_menu.is_none());
    assert!(app.screen_selection.is_none());
    assert!(app.notifications_overlay.is_some());
    app.handle_key(code(KeyCode::Esc));
    let _rows = rendered(&mut app, 80, 24);
    assert!(app.toast_hit.is_some());
}

#[test]
fn pointer_hover_pauses_expiry_until_the_pointer_leaves() {
    let mut app = card_fixture();
    let toasts = app.toasts.as_ref().unwrap().clone();
    let hit = app.toast_hit.unwrap();
    let original = app.live_toasts()[0].deadline().unwrap();
    mouse_at(&mut app, MouseEventKind::Moved, hit.area.as_position());
    assert_eq!(app.toast_pointer, Some(hit.area.as_position()));
    assert!(app.live_toasts()[0].deadline().is_none());
    assert_eq!(
        prune_expired(&toasts, original + Duration::from_secs(60)),
        None
    );
    let _rows = rendered(&mut app, 80, 24);
    assert_eq!(app.toast_hit.unwrap().toast_id, hit.toast_id);
    mouse_at(&mut app, MouseEventKind::Moved, Position::new(0, 23));
    let resumed = app.live_toasts()[0].deadline().unwrap();
    assert!(resumed >= original);
    prune_expired(&toasts, resumed + Duration::from_secs(1));
    let _rows = rendered(&mut app, 80, 24);
    assert!(app.toast_hit.is_none());
    assert!(app.live_toasts().is_empty());
    assert_eq!(lock(&toasts).history.len(), 1);
}

#[test]
fn a_burst_displays_fifo_with_full_lifetimes_and_retained_history() {
    let (mut app, _rx, _events) = app_with_events();
    let toasts = app.toasts.as_ref().unwrap().clone();
    let messages = ["first information", "second warning", "third error"];
    for (text, kind) in
        messages
            .into_iter()
            .zip([ToastKind::Info, ToastKind::Warning, ToastKind::Error])
    {
        push_toast(
            &toasts,
            Toast::with_kind(text, kind, crate::toast::DEFAULT_TOAST_DURATION),
        );
    }
    let ids: Vec<_> = lock(&toasts).active.iter().map(|toast| toast.id).collect();
    let first_deadline = lock(&toasts).active[0].deadline().unwrap();
    let assert_current = |app: &mut App, index: usize| {
        let rows = rendered(app, 100, 24);
        assert_eq!(app.toast_hit.unwrap().toast_id, ids[index]);
        for (message_index, message) in messages.iter().enumerate() {
            assert_eq!(
                rows.iter().any(|row| row.contains(message)),
                message_index == index,
                "{rows:?}"
            );
        }
        let pending = messages.len() - index - 1;
        if pending > 0 {
            assert!(
                rows.iter().any(|row| row.contains(&format!("+{pending}"))),
                "{rows:?}"
            );
        } else {
            assert!(
                rows.iter()
                    .all(|row| !row.contains("+1") && !row.contains("+2")),
                "{rows:?}"
            );
        }
    };
    assert_current(&mut app, 0);
    let rows = rendered(&mut app, 100, 24);
    assert_footer_alerts(&rows, 2);
    assert!(
        lock(&toasts)
            .active
            .iter()
            .skip(1)
            .all(|toast| toast.deadline().is_none())
    );
    let promoted_at = first_deadline + Duration::from_secs(6);
    let second_deadline = prune_expired(&toasts, promoted_at).unwrap();
    assert_eq!(
        second_deadline,
        promoted_at + crate::toast::DEFAULT_TOAST_DURATION
    );
    assert_current(&mut app, 1);
    assert_eq!(app.live_toasts().len(), 2);
    assert_eq!(
        prune_expired(
            &toasts,
            second_deadline
                .checked_sub(Duration::from_millis(1))
                .unwrap()
        ),
        Some(second_deadline)
    );
    assert_current(&mut app, 1);
    let third_deadline = prune_expired(&toasts, second_deadline).unwrap();
    assert_eq!(
        third_deadline,
        second_deadline + crate::toast::DEFAULT_TOAST_DURATION
    );
    assert_current(&mut app, 2);
    assert_eq!(prune_expired(&toasts, third_deadline), None);
    let rows = rendered(&mut app, 100, 24);
    assert!(app.toast_hit.is_none());
    assert!(
        messages
            .iter()
            .all(|message| rows.iter().all(|row| !row.contains(message)))
    );
    assert_eq!(lock(&toasts).history.len(), 3);
    assert_eq!(lock(&toasts).unread_count(), 2);
    assert!(lock(&toasts).history.iter().all(|entry| !entry.reviewed));
    assert!(lock(&app.buffer).blocks().is_empty());
    app.open_notifications();
    let rows = rendered(&mut app, 100, 24);
    for message in messages {
        assert!(rows.iter().any(|row| row.contains(message)), "{rows:?}");
    }
}

#[test]
fn a_new_arrival_keeps_the_hovered_card_and_pending_lifetimes_unchanged() {
    let mut app = card_fixture();
    let toasts = app.toasts.as_ref().unwrap().clone();
    let first = app.toast_hit.unwrap();
    let original_deadline = app.live_toasts()[0].deadline().unwrap();
    let point = Position::new(first.area.right() - 2, first.area.y + 1);
    mouse_at(&mut app, MouseEventKind::Moved, point);
    push_toast(
        &toasts,
        Toast::with_kind(
            "queued urgent error",
            ToastKind::Error,
            crate::toast::DEFAULT_TOAST_DURATION,
        ),
    );
    let rows = rendered(&mut app, 80, 24);
    assert_eq!(app.toast_hit.unwrap().toast_id, first.toast_id);
    assert!(app.toast_hit.unwrap().area.contains(point));
    assert!(rows.iter().any(|row| row.contains("floating card")));
    assert!(rows.iter().any(|row| row.contains("+1")));
    assert!(rows.iter().all(|row| !row.contains("queued urgent error")));
    assert!(
        app.live_toasts()
            .iter()
            .all(|toast| toast.deadline().is_none())
    );
    assert_eq!(
        prune_expired(&toasts, original_deadline + Duration::from_secs(60)),
        None
    );
    assert_eq!(app.live_toasts().len(), 2);
    mouse_at(&mut app, MouseEventKind::Moved, Position::new(0, 23));
    let resumed = app.live_toasts()[0].deadline().unwrap();
    assert!(resumed >= original_deadline);
    assert!(app.live_toasts()[1].deadline().is_none());
    let promoted_at = resumed + Duration::from_secs(1);
    assert_eq!(
        prune_expired(&toasts, promoted_at),
        Some(promoted_at + crate::toast::DEFAULT_TOAST_DURATION)
    );
    let rows = rendered(&mut app, 80, 24);
    assert_ne!(app.toast_hit.unwrap().toast_id, first.toast_id);
    assert!(rows.iter().any(|row| row.contains("queued urgent error")));
    assert!(rows.iter().all(|row| !row.contains("floating card")));
    assert_eq!(lock(&toasts).history.len(), 2);
    assert_eq!(lock(&toasts).unread_count(), 1);
}

#[test]
fn disabling_mouse_capture_resumes_a_hovered_card() {
    use kage_core::options::{OptionSource, OptionValue};
    let mut app = card_fixture();
    let store = Arc::new(Mutex::new(kage_core::options::OptionStore::default()));
    app.set_options(store.clone(), None);
    let hit = app.toast_hit.unwrap();
    mouse_at(&mut app, MouseEventKind::Moved, hit.area.as_position());
    assert!(app.live_toasts()[0].deadline().is_none());
    lock(&store)
        .set("mouse", OptionValue::Bool(false), OptionSource::Runtime)
        .unwrap();
    assert!(app.apply_option_changes());
    assert_eq!(app.pending_mouse_capture, Some(false));
    assert!(app.toast_pointer.is_none());
    assert!(app.live_toasts()[0].deadline().is_some());
}

#[test]
fn selection_capture_keeps_covered_text_and_excludes_the_floating_card() {
    let (mut app, _rx, _events) = app_with_events();
    let text = (0..30)
        .map(|i| format!("underlying line {i:02}: alpha beta gamma delta epsilon zeta eta theta"))
        .collect::<Vec<_>>()
        .join("\n");
    lock(&app.buffer).append_assistant_delta(&text);
    lock(&app.buffer).finish_streaming();
    app.set_scroll(0);
    app.screen_selection = Some(((0, 0), (999, 79)));
    let _rows = rendered(&mut app, 80, 24);
    let original = app.extract_selection_text();
    assert!(original.contains("alpha beta gamma"), "{original}");
    app.notify("toast must never be copied");
    let rows = rendered(&mut app, 80, 24);
    assert!(
        rows.iter()
            .any(|row| row.contains("toast must never be copied")),
        "{rows:?}"
    );
    assert_eq!(app.extract_selection_text(), original);
    let hit = app.toast_hit.unwrap();
    let buffer = lock(&app.buffer);
    let virtual_row =
        buffer.last_virtual_top() + usize::from(hit.area.y + 1 - buffer.last_area_y());
    drop(buffer);
    let captured = &app.captured_rows[&virtual_row];
    assert!(
        captured[usize::from(hit.area.x)..usize::from(hit.area.right())]
            .iter()
            .any(|cell| !cell.decoration && cell.ch.is_alphanumeric())
    );
}

#[test]
fn a_conversation_selection_drag_continues_across_the_card() {
    let (mut app, _rx, _events) = app_with_events();
    lock(&app.buffer).append_assistant_delta(
        &vec!["underlying content alpha beta gamma delta epsilon zeta eta theta"; 30].join("\n"),
    );
    lock(&app.buffer).finish_streaming();
    app.set_scroll(0);
    app.notify("floating card");
    let _rows = rendered(&mut app, 80, 24);
    let hit = app.toast_hit.unwrap();
    let start = Position::new(2, hit.area.y + 1);
    let end = Position::new(hit.area.right() - 2, start.y);
    assert!(!hit.area.contains(start));
    assert!(hit.area.contains(end));
    mouse_at(&mut app, MouseEventKind::Down(MouseButton::Left), start);
    assert!(app.mouse_drag_anchor.is_some());
    mouse_at(&mut app, MouseEventKind::Drag(MouseButton::Left), end);
    let _rows = rendered(&mut app, 80, 24);
    let selection = app.extract_selection_text();
    assert!(selection.contains("underlying content"), "{selection}");
    assert!(!selection.contains("floating card"), "{selection}");
    assert_eq!(app.screen_selection.unwrap().1.1, end.x);
    assert!(app.toast_mouse_down.is_none());
    mouse_at(&mut app, MouseEventKind::Up(MouseButton::Left), end);
    assert!(app.screen_selection.is_none());
    assert!(
        app.live_toasts()
            .iter()
            .any(|toast| toast.id == hit.toast_id)
    );
}

#[test]
fn an_agents_modal_cancels_a_card_press_before_the_next_gesture() {
    let (mut app, _rx, events) = app_with_events();
    spawn_agent(&mut app, &events, "agent-call", "explore");
    app.notify("floating card");
    let _rows = rendered(&mut app, 80, 24);
    let hit = app.toast_hit.unwrap();
    mouse_at(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        hit.area.as_position(),
    );
    assert_eq!(app.toast_mouse_down, Some(hit.toast_id));
    app.handle_key(ctrl('t'));
    assert!(app.agents_overlay.is_some());
    let _rows = rendered(&mut app, 80, 24);
    assert!(app.toast_hit.is_none());
    mouse_at(
        &mut app,
        MouseEventKind::Up(MouseButton::Left),
        hit.area.as_position(),
    );
    assert!(app.toast_mouse_down.is_none());
    assert!(app.mouse_drag_anchor.is_none());
    assert!(
        app.live_toasts()
            .iter()
            .any(|toast| toast.id == hit.toast_id)
    );
    app.handle_key(code(KeyCode::Esc));
    let _rows = rendered(&mut app, 80, 24);
    let hit = app.toast_hit.unwrap();
    mouse_at(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        hit.area.as_position(),
    );
    mouse_at(
        &mut app,
        MouseEventKind::Up(MouseButton::Left),
        hit.area.as_position(),
    );
    assert!(
        app.live_toasts()
            .iter()
            .all(|toast| toast.id != hit.toast_id)
    );
    assert!(app.screen_selection.is_none());
}

#[test]
fn history_takeover_cancels_an_underlying_drag_before_the_next_card_click() {
    let mut app = card_fixture();
    let top = lock(&app.buffer).last_area_y();
    mouse_at(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        Position::new(2, top),
    );
    assert!(app.mouse_drag_anchor.is_some());
    let selection = app.screen_selection;
    let before = {
        let buffer = lock(&app.buffer);
        (
            buffer.scroll(),
            buffer.focus(),
            format!("{:?}", buffer.blocks()),
        )
    };
    app.open_notifications();
    let _rows = rendered(&mut app, 80, 24);
    mouse_at(
        &mut app,
        MouseEventKind::Up(MouseButton::Left),
        Position::new(2, top),
    );
    assert!(app.mouse_drag_anchor.is_none());
    app.handle_key(code(KeyCode::Esc));
    let _rows = rendered(&mut app, 80, 24);
    let hit = app.toast_hit.unwrap();
    mouse_at(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        hit.area.as_position(),
    );
    mouse_at(
        &mut app,
        MouseEventKind::Up(MouseButton::Left),
        hit.area.as_position(),
    );
    assert!(
        app.live_toasts()
            .iter()
            .all(|toast| toast.id != hit.toast_id)
    );
    assert_eq!(app.screen_selection, selection);
    let buffer = lock(&app.buffer);
    assert_eq!(
        (
            buffer.scroll(),
            buffer.focus(),
            format!("{:?}", buffer.blocks())
        ),
        before
    );
    assert_eq!(app.input.text(), "unfinished draft");
}

#[test]
fn a_context_menu_hides_the_card_and_resumes_its_expiry() {
    let mut app = card_fixture();
    let hit = app.toast_hit.unwrap();
    mouse_at(&mut app, MouseEventKind::Moved, hit.area.as_position());
    assert!(app.live_toasts()[0].deadline().is_none());
    let top = lock(&app.buffer).last_area_y();
    mouse_at(
        &mut app,
        MouseEventKind::Down(MouseButton::Right),
        Position::new(2, top),
    );
    assert!(app.context_menu.is_some());
    let _rows = rendered(&mut app, 80, 24);
    assert!(app.toast_hit.is_none());
    assert!(app.live_toasts()[0].deadline().is_some());
    app.handle_key(code(KeyCode::Esc));
    let _rows = rendered(&mut app, 80, 24);
    assert!(app.context_menu.is_none());
    assert_eq!(app.toast_hit.unwrap().toast_id, hit.toast_id);
}
