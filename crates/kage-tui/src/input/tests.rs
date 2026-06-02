//! Tests for the input state machine.

use super::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn shift_enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
}

fn alt_enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
}

fn img(label: &str) -> crate::image::AttachedImage {
    crate::image::AttachedImage {
        source: kage_core::ImageSource::Base64 {
            data: "AAAA".into(),
        },
        mime: "image/png".into(),
        label: label.into(),
        bytes: 3,
    }
}

#[test]
fn marker_scan_and_strip_round_trip() {
    let s = "see [image #1 a.png image/png 3 B] and [image #2 b.png ...] done";
    let ids = image_marker_ids(s);
    assert!(ids.contains(&1) && ids.contains(&2) && ids.len() == 2);
    assert_eq!(strip_image_markers(s), "see and done");
    // A non-marker bracket is left alone.
    assert_eq!(strip_image_markers("keep [this] text"), "keep [this] text");
    assert!(image_marker_ids("no markers here").is_empty());
}

#[test]
fn attach_inserts_marker_and_submit_strips_it_keeping_image() {
    let mut s = InputState::new();
    s.attach_image(img("shot.png"));
    assert!(
        s.text().contains("[image #1 shot.png"),
        "marker inserted: {:?}",
        s.text()
    );
    for c in "look at this".chars() {
        s.handle_key(key(KeyCode::Char(c)));
    }
    let acts = s.handle_key(key(KeyCode::Enter));
    match acts.as_slice() {
        [InputAction::Submit(t)] => {
            assert!(!t.contains("[image #"), "marker stripped: {t:?}");
            assert!(t.contains("look at this"), "prose kept: {t:?}");
        }
        other => panic!("expected Submit, got {other:?}"),
    }
    let kept = s.take_attached();
    assert_eq!(kept.len(), 1, "image survives because its marker did");
}

#[test]
fn one_backspace_deletes_the_whole_marker_and_drops_the_image() {
    let mut s = InputState::new();
    s.attach_image(img("a.png"));
    // Cursor sits just past `[image #1 ...] `; a single
    // Backspace removes the entire chip, not one char.
    assert!(s.text().contains("[image #1"));
    s.handle_key(key(KeyCode::Backspace));
    assert_eq!(s.text(), "", "whole marker (and its space) removed");
    assert!(
        s.attached().is_empty(),
        "the image is dropped with its chip immediately"
    );
}

#[test]
fn backspace_after_typed_text_still_deletes_one_char() {
    let mut s = InputState::new();
    s.attach_image(img("a.png"));
    for c in "hi".chars() {
        s.handle_key(key(KeyCode::Char(c)));
    }
    s.handle_key(key(KeyCode::Backspace));
    assert!(s.text().ends_with('h'), "char-wise delete past the chip");
    assert_eq!(s.attached().len(), 1, "chip untouched");
}

#[test]
fn backspace_with_cursor_inside_the_chip_still_deletes_the_whole_block() {
    let mut s = InputState::new();
    s.attach_image(img("a.png"));
    for _ in 0..10 {
        s.handle_key(key(KeyCode::Left));
    }
    assert!(
        s.armed_image_range().is_some(),
        "the chip is armed while the cursor is inside its brackets"
    );
    s.handle_key(key(KeyCode::Backspace));
    assert_eq!(s.text(), "", "the whole chip went, not one char");
    assert!(s.attached().is_empty(), "image dropped with its chip");
}

#[test]
fn forward_delete_in_front_of_a_chip_removes_the_whole_block() {
    let mut s = InputState::new();
    s.attach_image(img("a.png"));
    s.handle_key(key(KeyCode::Home));
    assert!(
        s.armed_image_range().is_some(),
        "the chip highlights while the caret sits against it"
    );
    s.handle_key(key(KeyCode::Delete));
    assert_eq!(s.text(), "", "Delete took the chip, not one char");
    assert!(s.attached().is_empty(), "image dropped with its chip");
}

#[test]
fn forward_delete_off_a_chip_still_deletes_one_char() {
    let mut s = InputState::new();
    for c in "abc".chars() {
        s.handle_key(key(KeyCode::Char(c)));
    }
    s.handle_key(key(KeyCode::Home));
    s.handle_key(key(KeyCode::Delete));
    assert_eq!(s.text(), "bc", "plain forward delete removes one char");
}

#[test]
fn i_enters_insert_then_esc_returns_to_normal() {
    let mut state = InputState::new();
    state.force_normal();
    let acts = state.handle_key(key(KeyCode::Char('i')));
    assert_eq!(acts, vec![InputAction::EnterMode(Mode::Insert)]);
    assert_eq!(state.mode(), Mode::Insert);
    let acts = state.handle_key(key(KeyCode::Esc));
    assert_eq!(acts, vec![InputAction::EnterMode(Mode::Normal)]);
}

#[test]
fn default_mode_is_insert() {
    let state = InputState::new();
    assert_eq!(state.mode(), Mode::Insert);
}

#[test]
fn typing_in_insert_appends_to_text() {
    let mut state = InputState::new();
    for c in "hello".chars() {
        state.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(state.text(), "hello");
    assert_eq!(state.cursor(), 5);
}

#[test]
fn enter_in_insert_submits_and_stays_in_insert() {
    let mut state = InputState::new();
    for c in "hi".chars() {
        state.handle_key(key(KeyCode::Char(c)));
    }
    let acts = state.handle_key(key(KeyCode::Enter));
    assert_eq!(acts, vec![InputAction::Submit("hi".into())]);
    assert_eq!(state.text(), "");
    assert_eq!(state.mode(), Mode::Insert);
}

#[test]
fn enter_on_empty_buffer_in_insert_is_a_no_op() {
    let mut state = InputState::new();
    let acts = state.handle_key(key(KeyCode::Enter));
    assert!(acts.is_empty());
    assert_eq!(state.mode(), Mode::Insert);
}

#[test]
fn shift_enter_inserts_a_newline_in_insert() {
    let mut state = InputState::new();
    state.handle_key(key(KeyCode::Char('a')));
    state.handle_key(shift_enter());
    state.handle_key(key(KeyCode::Char('b')));
    assert_eq!(state.text(), "a\nb");
}

#[test]
fn alt_enter_also_inserts_a_newline_for_terminals_that_remap_shift_enter() {
    let mut state = InputState::new();
    state.handle_key(key(KeyCode::Char('a')));
    state.handle_key(alt_enter());
    state.handle_key(key(KeyCode::Char('b')));
    assert_eq!(state.text(), "a\nb");
}

#[test]
fn jk_scroll_in_normal_when_buffer_focused() {
    let mut state = InputState::new();
    state.force_normal();
    state.set_focused_pane(Pane::Buffer);
    assert_eq!(
        state.handle_key(key(KeyCode::Char('j'))),
        vec![InputAction::Scroll(1)]
    );
    assert_eq!(
        state.handle_key(key(KeyCode::Char('k'))),
        vec![InputAction::Scroll(-1)]
    );
}

#[test]
fn jk_move_input_cursor_when_input_focused() {
    let mut state = InputState::new();
    state.paste("first\nsecond");
    state.handle_key(key(KeyCode::Esc));
    assert!(state.handle_key(key(KeyCode::Char('j'))).is_empty());
    assert_eq!(state.cursor(), 12);
    let acts = state.handle_key(key(KeyCode::Char('k')));
    assert!(acts.is_empty());
    assert_eq!(state.cursor(), 5);
}

#[test]
fn gg_scrolls_to_top_when_buffer_focused() {
    let mut state = InputState::new();
    state.force_normal();
    state.set_focused_pane(Pane::Buffer);
    let first = state.handle_key(key(KeyCode::Char('g')));
    assert!(first.is_empty());
    assert!(state.has_pending());
    let second = state.handle_key(key(KeyCode::Char('g')));
    assert_eq!(second, vec![InputAction::ScrollToTop]);
    assert!(!state.has_pending());
}

#[test]
fn gg_jumps_input_cursor_to_start_when_input_focused() {
    let mut state = InputState::new();
    state.paste("hello");
    state.handle_key(key(KeyCode::Esc));
    assert_eq!(state.cursor(), 5);
    state.handle_key(key(KeyCode::Char('g')));
    let acts = state.handle_key(key(KeyCode::Char('g')));
    assert!(acts.is_empty());
    assert_eq!(state.cursor(), 0);
}

#[test]
fn capital_g_scrolls_to_bottom_when_buffer_focused() {
    let mut state = InputState::new();
    state.force_normal();
    state.set_focused_pane(Pane::Buffer);
    assert_eq!(
        state.handle_key(key(KeyCode::Char('G'))),
        vec![InputAction::ScrollToBottom]
    );
}

#[test]
fn capital_g_jumps_input_cursor_to_end_when_input_focused() {
    let mut state = InputState::new();
    state.paste("hello world");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('h')));
    assert!(state.cursor() < 11);
    state.handle_key(key(KeyCode::Char('G')));
    assert_eq!(state.cursor(), 11);
}

#[test]
fn z_prefix_handles_fold_keys() {
    let mut state = InputState::new();
    state.force_normal();
    for (suffix, expected) in [
        ('o', InputAction::ToggleFold),
        ('c', InputAction::ToggleFold),
        ('R', InputAction::UnfoldAll),
        ('M', InputAction::FoldAll),
    ] {
        state.handle_key(key(KeyCode::Char('z')));
        let acts = state.handle_key(key(KeyCode::Char(suffix)));
        assert_eq!(acts, vec![expected.clone()]);
    }
}

#[test]
fn colon_and_slash_open_command_and_search() {
    let mut state = InputState::new();
    state.force_normal();
    assert_eq!(
        state.handle_key(key(KeyCode::Char(':'))),
        vec![InputAction::BeginCommand]
    );
    assert_eq!(
        state.handle_key(key(KeyCode::Char('/'))),
        vec![InputAction::BeginSearch]
    );
}

#[test]
fn ctrl_c_in_normal_emits_cancel() {
    let mut state = InputState::new();
    state.force_normal();
    assert_eq!(state.handle_key(ctrl('c')), vec![InputAction::Cancel]);
}

#[test]
fn normal_y_emits_yank_when_buffer_focused() {
    let mut state = InputState::new();
    state.force_normal();
    state.set_focused_pane(Pane::Buffer);
    let acts = state.handle_key(key(KeyCode::Char('y')));
    assert_eq!(acts, vec![InputAction::Yank]);
}

#[test]
fn normal_y_in_input_pane_starts_yank_operator() {
    let mut state = InputState::new();
    state.force_normal();
    let acts = state.handle_key(key(KeyCode::Char('y')));
    assert!(acts.is_empty());
    assert!(state.pending_op.is_some());
}

#[test]
fn normal_esc_emits_clear_selection() {
    let mut state = InputState::new();
    state.force_normal();
    let acts = state.handle_key(key(KeyCode::Esc));
    assert_eq!(acts, vec![InputAction::ClearSelection]);
}

#[test]
fn backspace_removes_previous_char() {
    let mut state = InputState::new();
    for c in "abc".chars() {
        state.handle_key(key(KeyCode::Char(c)));
    }
    state.handle_key(key(KeyCode::Backspace));
    assert_eq!(state.text(), "ab");
    assert_eq!(state.cursor(), 2);
}

#[test]
fn left_right_move_cursor_in_insert() {
    let mut state = InputState::new();
    for c in "abc".chars() {
        state.handle_key(key(KeyCode::Char(c)));
    }
    state.handle_key(key(KeyCode::Left));
    state.handle_key(key(KeyCode::Left));
    assert_eq!(state.cursor(), 1);
    state.handle_key(key(KeyCode::Char('X')));
    assert_eq!(state.text(), "aXbc");
}

#[test]
fn paste_in_insert_inserts_verbatim_with_newlines() {
    let mut state = InputState::new();
    state.handle_key(key(KeyCode::Char('a')));
    state.paste("multi\nline\npaste");
    state.handle_key(key(KeyCode::Char('z')));
    assert_eq!(state.text(), "amulti\nline\npastez");
}

#[test]
fn submit_pushes_text_into_history_skipping_dupes() {
    let mut state = InputState::new();
    for c in "foo".chars() {
        state.handle_key(key(KeyCode::Char(c)));
    }
    state.handle_key(key(KeyCode::Enter));
    for c in "foo".chars() {
        state.handle_key(key(KeyCode::Char(c)));
    }
    state.handle_key(key(KeyCode::Enter));
    assert_eq!(state.history(), &["foo".to_owned()]);
}

#[test]
fn up_in_insert_walks_back_through_history() {
    let mut state = InputState::new();
    state.set_history(vec!["alpha".into(), "beta".into(), "gamma".into()]);
    state.handle_key(key(KeyCode::Up));
    assert_eq!(state.text(), "gamma");
    state.handle_key(key(KeyCode::Up));
    assert_eq!(state.text(), "beta");
    state.handle_key(key(KeyCode::Up));
    assert_eq!(state.text(), "alpha");
    state.handle_key(key(KeyCode::Up));
    assert_eq!(state.text(), "alpha", "stops at oldest");
}

#[test]
fn down_in_insert_returns_to_stashed_draft() {
    let mut state = InputState::new();
    state.set_history(vec!["one".into(), "two".into()]);
    for c in "draft".chars() {
        state.handle_key(key(KeyCode::Char(c)));
    }
    state.handle_key(key(KeyCode::Up));
    assert_eq!(state.text(), "two");
    state.handle_key(key(KeyCode::Down));
    assert_eq!(state.text(), "draft");
}

#[test]
fn up_in_multiline_input_moves_cursor_within_text_first() {
    let mut state = InputState::new();
    state.set_history(vec!["history-entry".into()]);
    state.paste("first\nsecond");
    state.handle_key(key(KeyCode::Up));
    assert_eq!(state.text(), "first\nsecond", "text untouched");
    state.handle_key(key(KeyCode::Up));
    assert_eq!(state.text(), "history-entry");
}

#[test]
fn down_in_multiline_input_moves_cursor_before_walking_history() {
    let mut state = InputState::new();
    state.set_history(vec!["older".into(), "newer".into()]);
    state.paste("aa\nbb");
    state.handle_key(key(KeyCode::Home));
    state.handle_key(key(KeyCode::Down));
    assert_eq!(state.text(), "aa\nbb");
    state.handle_key(key(KeyCode::Down));
    assert_eq!(state.text(), "aa\nbb");
}

#[test]
fn up_clamps_column_to_shorter_previous_row() {
    let mut state = InputState::new();
    state.paste("hi\nlonger-line");
    state.handle_key(key(KeyCode::Up));
    assert_eq!(state.cursor(), 2);
}

#[test]
fn typing_after_history_walk_resets_navigation() {
    let mut state = InputState::new();
    state.set_history(vec!["a".into(), "b".into()]);
    state.handle_key(key(KeyCode::Up));
    assert_eq!(state.text(), "b");
    state.handle_key(key(KeyCode::Char('z')));
    assert_eq!(state.text(), "bz");
    state.handle_key(key(KeyCode::Down));
    assert_eq!(state.text(), "bz", "down with no cursor is a no-op");
}

#[test]
fn set_history_truncates_to_cap() {
    let mut state = InputState::new();
    let entries: Vec<String> = (0..(HISTORY_MAX + 5)).map(|i| format!("e{i}")).collect();
    state.set_history(entries);
    assert_eq!(state.history().len(), HISTORY_MAX);
    assert_eq!(state.history().first().unwrap(), "e5");
}

#[test]
fn paste_in_normal_is_ignored() {
    let mut state = InputState::new();
    state.force_normal();
    state.paste("oops");
    assert_eq!(state.text(), "");
}

#[test]
fn home_and_end_jump_in_insert() {
    let mut state = InputState::new();
    for c in "abc".chars() {
        state.handle_key(key(KeyCode::Char(c)));
    }
    state.handle_key(key(KeyCode::Home));
    assert_eq!(state.cursor(), 0);
    state.handle_key(key(KeyCode::End));
    assert_eq!(state.cursor(), 3);
}

#[test]
fn default_pane_focus_is_input() {
    let state = InputState::new();
    assert_eq!(state.focused_pane(), Pane::Input);
}

#[test]
fn ctrl_w_in_normal_emits_cycle_pane() {
    let mut state = InputState::new();
    state.force_normal();
    let acts = state.handle_key(ctrl('w'));
    assert_eq!(acts, vec![InputAction::CyclePane]);
}

#[test]
fn toggle_focused_pane_round_trips() {
    let mut state = InputState::new();
    assert!(state.toggle_focused_pane());
    assert_eq!(state.focused_pane(), Pane::Buffer);
    assert!(state.toggle_focused_pane());
    assert_eq!(state.focused_pane(), Pane::Input);
}

#[test]
fn set_focused_pane_reports_changes() {
    let mut state = InputState::new();
    assert!(!state.set_focused_pane(Pane::Input));
    assert!(state.set_focused_pane(Pane::Buffer));
    assert!(!state.set_focused_pane(Pane::Buffer));
}

#[test]
fn backtab_in_insert_emits_cycle_thinking_level() {
    let mut state = InputState::new();
    let acts = state.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
    assert_eq!(acts, vec![InputAction::CycleThinkingLevel]);
}

#[test]
fn shift_tab_in_normal_emits_cycle_thinking_level() {
    let mut state = InputState::new();
    state.force_normal();
    let acts = state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
    assert_eq!(acts, vec![InputAction::CycleThinkingLevel]);
}

#[test]
fn plain_tab_in_insert_does_not_emit_cycle_thinking_level() {
    let mut state = InputState::new();
    let acts = state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(!acts.contains(&InputAction::CycleThinkingLevel));
}

fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

fn alt_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::ALT)
}

#[test]
fn alt_backspace_kills_word_back_in_insert() {
    let mut state = InputState::new();
    state.paste("hello world");
    state.handle_key(alt_key(KeyCode::Backspace));
    assert_eq!(state.text(), "hello ");
    assert_eq!(state.cursor(), 6);
    state.handle_key(alt_key(KeyCode::Backspace));
    assert_eq!(state.text(), "");
    assert_eq!(state.cursor(), 0);
}

#[test]
fn ctrl_w_kills_back_to_whitespace() {
    let mut state = InputState::new();
    state.paste("foo-bar baz");
    state.handle_key(ctrl('w'));
    assert_eq!(state.text(), "foo-bar ");
    state.handle_key(ctrl('w'));
    assert_eq!(state.text(), "");
}

#[test]
fn alt_d_kills_word_forward() {
    let mut state = InputState::new();
    state.paste("hello world");
    for _ in 0..11 {
        state.handle_key(key(KeyCode::Left));
    }
    state.handle_key(alt('d'));
    assert_eq!(state.text(), " world");
}

#[test]
fn alt_b_alt_f_navigate_words() {
    let mut state = InputState::new();
    state.paste("foo bar baz");
    state.handle_key(alt('b'));
    assert_eq!(state.cursor(), 8);
    state.handle_key(alt('b'));
    assert_eq!(state.cursor(), 4);
    state.handle_key(alt('b'));
    assert_eq!(state.cursor(), 0);
    state.handle_key(alt('f'));
    assert_eq!(state.cursor(), 3);
    state.handle_key(alt('f'));
    assert_eq!(state.cursor(), 7);
}

#[test]
fn ctrl_a_e_jump_to_line_edges() {
    let mut state = InputState::new();
    state.paste("first\nsecond line");
    state.handle_key(ctrl('a'));
    assert_eq!(state.cursor(), 6);
    state.handle_key(ctrl('e'));
    assert_eq!(state.cursor(), state.text().len());
}

#[test]
fn ctrl_u_deletes_to_line_start() {
    let mut state = InputState::new();
    state.paste("first\nsecond line");
    state.handle_key(ctrl('u'));
    assert_eq!(state.text(), "first\n");
    assert_eq!(state.cursor(), 6);
}

#[test]
fn h_l_move_cursor_in_input_pane() {
    let mut state = InputState::new();
    state.paste("abcd");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('h')));
    state.handle_key(key(KeyCode::Char('h')));
    assert_eq!(state.cursor(), 2);
    state.handle_key(key(KeyCode::Char('l')));
    assert_eq!(state.cursor(), 3);
}

#[test]
fn dollar_zero_caret_jump_to_line_edges_in_input_pane() {
    let mut state = InputState::new();
    state.paste("  hello world");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    assert_eq!(state.cursor(), 0);
    state.handle_key(key(KeyCode::Char('^')));
    assert_eq!(state.cursor(), 2);
    state.handle_key(key(KeyCode::Char('$')));
    assert_eq!(state.cursor(), state.text().len());
}

#[test]
fn vim_w_b_e_word_motions_in_input_pane() {
    let mut state = InputState::new();
    state.paste("foo bar baz");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    assert_eq!(state.cursor(), 0);
    state.handle_key(key(KeyCode::Char('w')));
    assert_eq!(state.cursor(), 4);
    state.handle_key(key(KeyCode::Char('e')));
    assert_eq!(state.cursor(), 6);
    state.handle_key(key(KeyCode::Char('b')));
    assert_eq!(state.cursor(), 4);
}

#[test]
fn x_deletes_char_at_cursor_in_input_pane() {
    let mut state = InputState::new();
    state.paste("abcd");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('x')));
    assert_eq!(state.text(), "bcd");
    assert_eq!(state.cursor(), 0);
}

#[test]
fn capital_x_deletes_char_before_cursor_in_input_pane() {
    let mut state = InputState::new();
    state.paste("abcd");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('X')));
    assert_eq!(state.text(), "abc");
}

#[test]
fn lowercase_a_advances_cursor_then_inserts() {
    let mut state = InputState::new();
    state.paste("ab");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('a')));
    assert_eq!(state.mode(), Mode::Insert);
    assert_eq!(state.cursor(), 1);
    state.handle_key(key(KeyCode::Char('X')));
    assert_eq!(state.text(), "aXb");
}

#[test]
fn capital_a_jumps_to_end_of_line_then_inserts() {
    let mut state = InputState::new();
    state.paste("hello\nworld");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('k')));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('A')));
    assert_eq!(state.mode(), Mode::Insert);
    assert_eq!(state.cursor(), 5);
}

#[test]
fn capital_o_opens_line_above() {
    let mut state = InputState::new();
    state.paste("hello");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('O')));
    assert_eq!(state.mode(), Mode::Insert);
    assert!(state.text().starts_with('\n'));
    assert_eq!(state.cursor(), 0);
}

#[test]
fn lowercase_o_opens_line_below() {
    let mut state = InputState::new();
    state.paste("hello");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('o')));
    assert_eq!(state.mode(), Mode::Insert);
    assert_eq!(state.text(), "hello\n");
    assert_eq!(state.cursor(), 6);
}

#[test]
fn dw_deletes_word_in_input_pane() {
    let mut state = InputState::new();
    state.paste("foo bar baz");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('d')));
    state.handle_key(key(KeyCode::Char('w')));
    assert_eq!(state.text(), "bar baz");
    assert_eq!(state.cursor(), 0);
    assert_eq!(state.register, "foo ");
}

#[test]
fn dd_deletes_current_line() {
    let mut state = InputState::new();
    state.paste("first\nsecond\nthird");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('g')));
    state.handle_key(key(KeyCode::Char('g')));
    state.handle_key(key(KeyCode::Char('d')));
    state.handle_key(key(KeyCode::Char('d')));
    assert_eq!(state.text(), "second\nthird");
    assert_eq!(state.cursor(), 0);
    assert_eq!(state.register, "first\n");
    assert!(state.register_linewise);
}

#[test]
fn cw_deletes_word_and_enters_insert() {
    let mut state = InputState::new();
    state.paste("foo bar");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('c')));
    state.handle_key(key(KeyCode::Char('w')));
    assert_eq!(state.mode(), Mode::Insert);
    assert_eq!(state.text(), "bar");
}

#[test]
fn yw_yanks_word_into_register() {
    let mut state = InputState::new();
    state.paste("hello world");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('y')));
    state.handle_key(key(KeyCode::Char('w')));
    assert_eq!(state.text(), "hello world");
    assert_eq!(state.cursor(), 0);
    assert_eq!(state.register, "hello ");
    assert!(!state.register_linewise);
}

#[test]
fn count_prefix_repeats_motion() {
    let mut state = InputState::new();
    state.paste("a b c d e");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('3')));
    state.handle_key(key(KeyCode::Char('w')));
    assert_eq!(state.cursor(), 6);
}

#[test]
fn count_prefix_with_operator_3dw() {
    let mut state = InputState::new();
    state.paste("foo bar baz qux");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('3')));
    state.handle_key(key(KeyCode::Char('d')));
    state.handle_key(key(KeyCode::Char('w')));
    assert_eq!(state.text(), "qux");
}

#[test]
fn capital_d_deletes_to_end_of_line() {
    let mut state = InputState::new();
    state.paste("hello world");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('5')));
    state.handle_key(key(KeyCode::Char('l')));
    state.handle_key(key(KeyCode::Char('D')));
    assert_eq!(state.text(), "hello");
}

#[test]
fn r_replaces_char_at_cursor() {
    let mut state = InputState::new();
    state.paste("foo");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('r')));
    state.handle_key(key(KeyCode::Char('B')));
    assert_eq!(state.text(), "Boo");
    assert_eq!(state.mode(), Mode::Normal);
}

#[test]
fn undo_reverts_last_insert_session() {
    let mut state = InputState::new();
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('i')));
    state.paste("hello");
    state.handle_key(key(KeyCode::Esc));
    assert_eq!(state.text(), "hello");
    state.handle_key(key(KeyCode::Char('u')));
    assert_eq!(state.text(), "");
}

#[test]
fn undo_reverts_dw_delete() {
    let mut state = InputState::new();
    state.paste("foo bar");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('d')));
    state.handle_key(key(KeyCode::Char('w')));
    assert_eq!(state.text(), "bar");
    state.handle_key(key(KeyCode::Char('u')));
    assert_eq!(state.text(), "foo bar");
}

#[test]
fn redo_replays_undone_change() {
    let mut state = InputState::new();
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('i')));
    state.paste("ab");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('u')));
    assert_eq!(state.text(), "");
    state.handle_key(ctrl('r'));
    assert_eq!(state.text(), "ab");
}

#[test]
fn new_change_after_undo_clears_redo_stack() {
    let mut state = InputState::new();
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('i')));
    state.paste("first");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('u')));
    state.handle_key(key(KeyCode::Char('i')));
    state.paste("second");
    state.handle_key(key(KeyCode::Esc));
    assert_eq!(state.text(), "second");
    state.handle_key(ctrl('r'));
    assert_eq!(state.text(), "second");
}

#[test]
fn v_in_input_pane_starts_input_visual() {
    let mut state = InputState::new();
    state.paste("hello");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('v')));
    assert_eq!(state.mode(), Mode::Visual);
    assert!(state.input_visual_range().is_some());
    assert_eq!(state.input_visual_range(), Some((0, 1)));
}

#[test]
fn input_visual_d_deletes_selected_range() {
    let mut state = InputState::new();
    state.paste("hello world");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('v')));
    for _ in 0..4 {
        state.handle_key(key(KeyCode::Char('l')));
    }
    state.handle_key(key(KeyCode::Char('d')));
    assert_eq!(state.text(), " world");
    assert_eq!(state.mode(), Mode::Normal);
    assert!(state.input_visual_range().is_none());
}

#[test]
fn input_visual_y_yanks_selection() {
    let mut state = InputState::new();
    state.paste("foo bar");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('v')));
    for _ in 0..2 {
        state.handle_key(key(KeyCode::Char('l')));
    }
    state.handle_key(key(KeyCode::Char('y')));
    assert_eq!(state.text(), "foo bar");
    assert_eq!(state.register, "foo");
    assert_eq!(state.mode(), Mode::Normal);
}

#[test]
fn p_pastes_charwise_register_after_cursor() {
    let mut state = InputState::new();
    state.paste("hello world");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('0')));
    state.handle_key(key(KeyCode::Char('y')));
    state.handle_key(key(KeyCode::Char('w')));
    state.handle_key(key(KeyCode::Char('p')));
    assert_eq!(state.text(), "hhello ello world");
}

#[test]
fn capital_p_pastes_linewise_register_above() {
    let mut state = InputState::new();
    state.paste("first\nsecond");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('y')));
    state.handle_key(key(KeyCode::Char('y')));
    assert_eq!(state.register, "second");
    assert!(state.register_linewise);
    state.handle_key(key(KeyCode::Char('P')));
    assert_eq!(state.text(), "first\nsecondsecond");
}

#[test]
fn p_pastes_linewise_register_below() {
    let mut state = InputState::new();
    state.paste("first\nsecond");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('y')));
    state.handle_key(key(KeyCode::Char('y')));
    state.handle_key(key(KeyCode::Char('p')));
    assert_eq!(state.text(), "first\nsecond\nsecond");
}

#[test]
fn ctrl_s_in_buffer_pane_opens_session_picker() {
    let mut state = InputState::new();
    state.force_normal();
    state.set_focused_pane(Pane::Buffer);
    let acts = state.handle_key(ctrl('s'));
    assert_eq!(acts, vec![InputAction::OpenSessionPicker]);
}

#[test]
fn ctrl_s_in_insert_mode_opens_session_picker() {
    let mut state = InputState::new();
    // Default mode is Insert.
    let acts = state.handle_key(ctrl('s'));
    assert_eq!(acts, vec![InputAction::OpenSessionPicker]);
}

#[test]
fn ctrl_s_in_normal_input_opens_session_picker() {
    let mut state = InputState::new();
    state.force_normal();
    let acts = state.handle_key(ctrl('s'));
    assert_eq!(acts, vec![InputAction::OpenSessionPicker]);
}

#[test]
fn d_then_esc_cancels_operator() {
    let mut state = InputState::new();
    state.paste("hello");
    state.handle_key(key(KeyCode::Esc));
    state.handle_key(key(KeyCode::Char('d')));
    assert!(state.pending_op.is_some());
    state.handle_key(key(KeyCode::Esc));
    assert!(state.pending_op.is_none());
    assert_eq!(state.text(), "hello");
}

#[test]
fn gw_in_normal_emits_cycle_pane() {
    let mut state = InputState::new();
    state.force_normal();
    state.handle_key(key(KeyCode::Char('g')));
    let acts = state.handle_key(key(KeyCode::Char('w')));
    assert_eq!(acts, vec![InputAction::CyclePane]);
}

#[test]
fn ctrl_k_deletes_to_line_end() {
    let mut state = InputState::new();
    state.paste("first\nsecond");
    for _ in 0..6 {
        state.handle_key(key(KeyCode::Left));
    }
    assert_eq!(state.cursor(), 6);
    state.handle_key(ctrl('k'));
    assert_eq!(state.text(), "first\n");
}

#[test]
fn ctrl_w_kills_word_and_ctrl_y_yanks_it_back() {
    let mut state = InputState::new();
    state.paste("hello world");
    state.handle_key(ctrl('w'));
    assert_eq!(state.text(), "hello ");
    assert_eq!(state.kill_ring(), ["world".to_owned()]);
    state.handle_key(ctrl('y'));
    assert_eq!(state.text(), "hello world");
    assert_eq!(state.cursor(), 11);
}

#[test]
fn ctrl_u_and_ctrl_k_feed_the_kill_ring() {
    let mut state = InputState::new();
    state.paste("abc def");
    state.handle_key(ctrl('u'));
    assert_eq!(state.text(), "");
    assert_eq!(state.kill_ring(), ["abc def".to_owned()]);

    let mut state = InputState::new();
    state.paste("one two");
    state.handle_key(ctrl('a'));
    state.handle_key(ctrl('k'));
    assert_eq!(state.text(), "");
    assert_eq!(state.kill_ring(), ["one two".to_owned()]);
}

#[test]
fn ctrl_y_yanks_the_most_recent_kill() {
    let mut state = InputState::new();
    state.paste("aaa bbb");
    state.handle_key(ctrl('w'));
    state.handle_key(ctrl('w'));
    assert_eq!(state.kill_ring().len(), 2);
    let last = state.kill_ring().last().unwrap().clone();
    state.handle_key(ctrl('y'));
    assert_eq!(state.text(), last);
}

#[test]
fn ctrl_y_on_empty_ring_is_a_noop() {
    let mut state = InputState::new();
    state.paste("abc");
    state.handle_key(ctrl('y'));
    assert_eq!(state.text(), "abc");
    assert!(state.kill_ring().is_empty());
}

#[test]
fn ctrl_slash_undoes_a_kill() {
    let mut state = InputState::new();
    state.paste("keep this");
    state.handle_key(ctrl('w'));
    assert_eq!(state.text(), "keep ");
    state.handle_key(ctrl('/'));
    assert_eq!(state.text(), "keep this");
}

#[test]
fn large_paste_collapses_to_placeholder() {
    let mut state = InputState::new();
    let blob = "row\n".repeat(12);
    state.paste(&blob);
    assert_eq!(state.collapsed_paste_count(), 1);
    assert!(state.text().starts_with("[paste #1: "));
    assert!(state.text().ends_with(" lines]"));
    assert!(!state.text().contains("row"));
}

#[test]
fn small_paste_is_inserted_verbatim() {
    let mut state = InputState::new();
    state.paste("a\nb\nc");
    assert_eq!(state.text(), "a\nb\nc");
    assert_eq!(state.collapsed_paste_count(), 0);
}

#[test]
fn submit_resolves_collapsed_paste_to_full_text() {
    let mut state = InputState::new();
    let blob = "x\n".repeat(15);
    state.paste(&blob);
    state.handle_key(key(KeyCode::Char('!')));
    let acts = state.handle_key(key(KeyCode::Enter));
    match acts.as_slice() {
        [InputAction::Submit(t)] => {
            assert!(t.contains(&blob), "full paste text must be sent");
            assert!(t.ends_with('!'));
            assert!(!t.contains("[paste #"), "placeholder must not leak");
        }
        other => panic!("expected Submit, got {other:?}"),
    }
    assert_eq!(state.collapsed_paste_count(), 0);
}

#[test]
fn ctrl_o_expands_collapsed_paste_inline() {
    let mut state = InputState::new();
    let blob = "L\n".repeat(11);
    state.paste(&blob);
    assert_eq!(state.collapsed_paste_count(), 1);
    state.handle_key(ctrl('o'));
    assert_eq!(state.text(), blob);
    assert_eq!(state.cursor(), blob.len());
    assert_eq!(state.collapsed_paste_count(), 0);
}

#[test]
fn two_large_pastes_get_distinct_placeholders() {
    let mut state = InputState::new();
    state.paste(&"a\n".repeat(10));
    state.paste(&"b\n".repeat(10));
    assert_eq!(state.collapsed_paste_count(), 2);
    assert!(state.text().contains("[paste #1: "));
    assert!(state.text().contains("[paste #2: "));
    let acts = state.handle_key(key(KeyCode::Enter));
    match acts.as_slice() {
        [InputAction::Submit(t)] => {
            assert!(t.contains(&"a\n".repeat(10)));
            assert!(t.contains(&"b\n".repeat(10)));
            assert!(!t.contains("[paste #"));
        }
        other => panic!("expected Submit, got {other:?}"),
    }
}

#[test]
fn set_modeless_forces_insert() {
    let mut state = InputState::new();
    state.force_normal();
    assert_eq!(state.mode(), Mode::Normal);
    state.set_modeless(true);
    assert!(state.is_modeless());
    assert_eq!(state.mode(), Mode::Insert);
}

#[test]
fn modeless_esc_cancels_turn_and_stays_insert() {
    let mut state = InputState::new();
    state.set_modeless(true);
    let acts = state.handle_key(key(KeyCode::Esc));
    assert_eq!(acts, vec![InputAction::Cancel]);
    // Never leaves the insert-like state.
    assert_eq!(state.mode(), Mode::Insert);
    // ... and is still editable afterward.
    state.handle_key(key(KeyCode::Char('x')));
    assert_eq!(state.text(), "x");
    assert_eq!(state.mode(), Mode::Insert);
}

#[test]
fn modeless_pageup_pagedown_scroll_the_buffer() {
    let mut state = InputState::new();
    state.set_modeless(true);
    assert_eq!(
        state.handle_key(key(KeyCode::PageUp)),
        vec![InputAction::Scroll(-10)]
    );
    assert_eq!(
        state.handle_key(key(KeyCode::PageDown)),
        vec![InputAction::Scroll(10)]
    );
}

#[test]
fn modeless_slash_on_empty_opens_command_palette() {
    let mut state = InputState::new();
    state.set_modeless(true);
    assert_eq!(
        state.handle_key(key(KeyCode::Char('/'))),
        vec![InputAction::OpenCommandPalette]
    );
}

#[test]
fn modeless_enter_submits_like_insert() {
    let mut state = InputState::new();
    state.set_modeless(true);
    state.handle_key(key(KeyCode::Char('h')));
    state.handle_key(key(KeyCode::Char('i')));
    let acts = state.handle_key(key(KeyCode::Enter));
    assert_eq!(acts, vec![InputAction::Submit("hi".into())]);
}

#[test]
fn modeless_emacs_keys_still_work() {
    let mut state = InputState::new();
    state.set_modeless(true);
    state.paste("hello world");
    state.handle_key(ctrl('w')); // kill word back
    assert_eq!(state.text(), "hello ");
    state.handle_key(ctrl('y')); // yank it back
    assert_eq!(state.text(), "hello world");
}

#[test]
fn vim_default_esc_still_enters_normal() {
    // Regression: the default (non-modeless) editor is unchanged.
    let mut state = InputState::new();
    assert!(!state.is_modeless());
    state.handle_key(key(KeyCode::Esc));
    assert_eq!(state.mode(), Mode::Normal);
}

// --- Ctrl+arrow buffer scroll tests ---

fn ctrl_arrow(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

#[test]
fn ctrl_up_down_in_insert_scrolls_buffer() {
    let mut state = InputState::new();
    let acts = state.handle_key(ctrl_arrow(KeyCode::Down));
    assert_eq!(acts, vec![InputAction::Scroll(1)]);
    let acts = state.handle_key(ctrl_arrow(KeyCode::Up));
    assert_eq!(acts, vec![InputAction::Scroll(-1)]);
}

#[test]
fn ctrl_home_end_in_insert_snaps_to_top_bottom() {
    let mut state = InputState::new();
    let acts = state.handle_key(ctrl_arrow(KeyCode::Home));
    assert_eq!(acts, vec![InputAction::ScrollToTop]);
    let acts = state.handle_key(ctrl_arrow(KeyCode::End));
    assert_eq!(acts, vec![InputAction::ScrollToBottom]);
}

#[test]
fn ctrl_n_p_in_insert_focuses_blocks() {
    let mut state = InputState::new();
    let acts = state.handle_key(ctrl('p'));
    assert_eq!(acts, vec![InputAction::FocusPrev]);
    let acts = state.handle_key(ctrl('n'));
    assert_eq!(acts, vec![InputAction::FocusNext]);
}

#[test]
fn ctrl_arrows_do_not_insert_text() {
    let mut state = InputState::new();
    state.handle_key(ctrl_arrow(KeyCode::Down));
    state.handle_key(ctrl_arrow(KeyCode::Up));
    state.handle_key(ctrl_arrow(KeyCode::Home));
    state.handle_key(ctrl_arrow(KeyCode::End));
    assert_eq!(state.text(), "", "ctrl+arrows must not insert characters");
}

#[test]
fn modeless_ctrl_up_down_scrolls_buffer() {
    let mut state = InputState::new();
    state.set_modeless(true);
    let acts = state.handle_key(ctrl_arrow(KeyCode::Down));
    assert_eq!(acts, vec![InputAction::Scroll(1)]);
    let acts = state.handle_key(ctrl_arrow(KeyCode::Up));
    assert_eq!(acts, vec![InputAction::Scroll(-1)]);
}

#[test]
fn modeless_ctrl_home_end_snaps_to_top_bottom() {
    let mut state = InputState::new();
    state.set_modeless(true);
    let acts = state.handle_key(ctrl_arrow(KeyCode::Home));
    assert_eq!(acts, vec![InputAction::ScrollToTop]);
    let acts = state.handle_key(ctrl_arrow(KeyCode::End));
    assert_eq!(acts, vec![InputAction::ScrollToBottom]);
}

#[test]
fn modeless_ctrl_n_p_focuses_blocks() {
    let mut state = InputState::new();
    state.set_modeless(true);
    let acts = state.handle_key(ctrl('p'));
    assert_eq!(acts, vec![InputAction::FocusPrev]);
    let acts = state.handle_key(ctrl('n'));
    assert_eq!(acts, vec![InputAction::FocusNext]);
}

#[test]
fn modeless_ctrl_arrows_do_not_insert_text() {
    let mut state = InputState::new();
    state.set_modeless(true);
    state.handle_key(ctrl_arrow(KeyCode::Down));
    state.handle_key(ctrl_arrow(KeyCode::Up));
    assert_eq!(state.text(), "");
}

#[test]
fn modeless_plain_arrows_still_move_cursor() {
    let mut state = InputState::new();
    state.set_modeless(true);
    state.handle_key(key(KeyCode::Char('a')));
    state.handle_key(key(KeyCode::Char('b')));
    state.handle_key(key(KeyCode::Left));
    assert_eq!(
        state.cursor(),
        1,
        "plain Left should still move the input cursor"
    );
}

// --- Ctrl+O fold toggle tests ---

#[test]
fn ctrl_o_toggles_fold_when_no_collapsed_paste() {
    let mut state = InputState::new();
    assert!(state.pastes.is_empty(), "no collapsed pastes by default");
    let acts = state.handle_key(ctrl('o'));
    assert_eq!(acts, vec![InputAction::ToggleFold]);
    assert_eq!(state.text(), "", "ctrl+o must not insert text");
}

#[test]
fn ctrl_o_expands_paste_when_one_is_collapsed() {
    let mut state = InputState::new();
    let blob = "row\n".repeat(12);
    state.paste(&blob);
    assert_eq!(state.collapsed_paste_count(), 1);
    let acts = state.handle_key(ctrl('o'));
    assert!(acts.is_empty(), "expanding a paste emits no InputAction");
    assert_eq!(state.text(), blob, "paste expanded inline");
    assert_eq!(state.collapsed_paste_count(), 0);
}

#[test]
fn ctrl_o_fold_toggle_in_modeless() {
    let mut state = InputState::new();
    state.set_modeless(true);
    let acts = state.handle_key(ctrl('o'));
    assert_eq!(acts, vec![InputAction::ToggleFold]);
}

#[test]
fn alt_b_f_still_move_words_not_scroll() {
    let mut state = InputState::new();
    state.paste("hello world");
    state.handle_key(key(KeyCode::Home));
    state.handle_key(alt('f'));
    assert_eq!(state.cursor(), 5, "alt+f should move forward one word");
    state.handle_key(alt('b'));
    assert_eq!(state.cursor(), 0, "alt+b should move backward one word");
}
