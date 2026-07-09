// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the notes widget. Split out of `mod.rs` per the repo standard.

use super::*;

fn make_widget() -> NotesWidget {
    // Keep tests off the user's real ~/.glint/notes (the new
    // default) by pointing the resolver at a per-process temp dir.
    let tmp = std::env::temp_dir().join(format!(
        "glint-notes-mod-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let cfg = NotesConfig {
        notes_dir: Some(tmp.to_string_lossy().to_string()),
        ..NotesConfig::default()
    };
    NotesWidget::with_config(
        "test-main".to_string(),
        cfg,
        Arc::new(Theme::builtin_defaults()),
    )
}

/// In EDIT mode every char is text input, so `y` after `⧉` would
/// be misleading — the keystroke just types a letter. The icon
/// stays (it's a click target now), the letter is dropped.
#[test]
fn title_metadata_drops_y_letter_in_edit_mode() {
    let w = make_widget();
    w.create_note();
    // create_note drops into Insert mode.
    let st = w.state.lock().unwrap();
    let meta = w
        .derive_title_metadata(&st)
        .expect("active note → metadata");
    assert!(meta.contains("EDIT"), "should mention EDIT mode: {meta:?}");
    assert!(meta.contains('⧉'), "should still show the ⧉ icon: {meta:?}");
    assert!(
        !meta.contains("⧉ y"),
        "should not include the y shortcut hint in EDIT: {meta:?}"
    );
}

/// VIEW mode keeps the `y` hint because the shortcut works there.
#[test]
fn title_metadata_keeps_y_letter_in_view_mode() {
    let w = make_widget();
    w.create_note();
    w.state.lock().unwrap().mode = Mode::Normal;
    let st = w.state.lock().unwrap();
    let meta = w
        .derive_title_metadata(&st)
        .expect("active note → metadata");
    assert!(meta.contains("VIEW"), "should mention VIEW mode: {meta:?}");
    assert!(
        meta.contains("⧉ y"),
        "should show ⧉ y hint in VIEW: {meta:?}"
    );
}

/// Clicking the title-row metadata area (where ⧉ sits) yanks the
/// active note in both VIEW and EDIT modes — yank_active sets
/// `status` either way (success or copy-failed), so we just
/// assert the side effect.
#[test]
fn title_bar_metadata_click_yanks_active_note_in_both_modes() {
    let mut w = make_widget();
    w.create_note();
    let area = Rect::new(0, 0, 80, 20);
    *w.last_outer_area.lock().unwrap() = area;
    // Click on the title row at the rightmost interior column —
    // metadata is right-aligned and ⧉ sits near the right corner.
    let click_col = area.x + area.width - 3;
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: click_col,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    };
    // EDIT mode (default after create_note).
    w.state.lock().unwrap().status = None;
    assert_eq!(w.handle_mouse(click, area), EventResult::Handled);
    assert!(
        w.state.lock().unwrap().status.is_some(),
        "EDIT-mode click on metadata should trigger yank (status set)"
    );
    // VIEW mode.
    w.state.lock().unwrap().status = None;
    w.state.lock().unwrap().mode = Mode::Normal;
    assert_eq!(w.handle_mouse(click, area), EventResult::Handled);
    assert!(
        w.state.lock().unwrap().status.is_some(),
        "VIEW-mode click on metadata should also trigger yank"
    );
}

/// Status messages auto-revert after STATUS_TTL — the title bar
/// goes back to the normal `<name> · MODE · ⧉` once "Copied"
/// expires. Backdate the timestamp to simulate elapsed time
/// rather than sleeping in the test.
#[test]
fn title_metadata_reverts_after_status_ttl_elapses() {
    let w = make_widget();
    w.create_note();
    // Fresh status — should be displayed verbatim.
    w.state.lock().unwrap().set_status("Copied to clipboard");
    {
        let st = w.state.lock().unwrap();
        assert_eq!(
            w.derive_title_metadata(&st).as_deref(),
            Some("Copied to clipboard")
        );
    }
    // Backdate by 2× TTL so it's definitely expired.
    {
        let mut st = w.state.lock().unwrap();
        if let Some(s) = st.status.as_mut() {
            s.set_at = Instant::now() - STATUS_TTL - Duration::from_secs(1);
        }
    }
    let st = w.state.lock().unwrap();
    let meta = w.derive_title_metadata(&st).expect("should fall through");
    assert!(
        !meta.contains("Copied"),
        "expired status should not be returned: {meta:?}"
    );
    assert!(
        meta.contains("EDIT") || meta.contains("VIEW"),
        "should fall through to the normal metadata: {meta:?}"
    );
}

/// Clicking elsewhere on the title row (e.g. left edge near the
/// title text) must NOT trigger yank — the icon-click hit region
/// is the metadata cluster only.
#[test]
fn title_bar_click_outside_metadata_does_not_yank() {
    let mut w = make_widget();
    w.create_note();
    let area = Rect::new(0, 0, 80, 20);
    *w.last_outer_area.lock().unwrap() = area;
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x + 3, // far left, over the title text
        row: area.y,
        modifiers: KeyModifiers::NONE,
    };
    w.state.lock().unwrap().status = None;
    let _ = w.handle_mouse(click, area);
    assert!(
        w.state.lock().unwrap().status.is_none(),
        "click on the title text should not yank"
    );
}

#[test]
fn word_wrap_breaks_at_spaces_when_possible() {
    // 15 chars, width 8: "hello world" (11 chars) won't fit on one
    // row, so each word lands on its own row.
    let rows = word_wrap("hello world foo", 8, 8);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].text, "hello");
    assert_eq!(rows[0].source_col_start, 0);
    assert_eq!(rows[0].source_col_end, 5);
    assert_eq!(rows[1].text, "world");
    assert_eq!(rows[2].text, "foo");
    // The trailing space between "hello" and "world" is consumed
    // (not echoed as leading whitespace on the next row).
    assert_eq!(rows[1].source_col_start, 6);
}

#[test]
fn word_wrap_uses_full_window_when_boundary_is_a_space() {
    // 22 chars, width 12: "hello world " ends on a space, so the
    // first row gets the full 11-char "hello world" and "foo bar"
    // wraps to row 2.
    let rows = word_wrap("hello world foo bar", 12, 12);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].text, "hello world");
    assert_eq!(rows[1].text, "foo bar");
}

#[test]
fn word_wrap_falls_back_to_hard_break_when_word_exceeds_width() {
    // No space in "abcdefghij" — must hard-break.
    let rows = word_wrap("abcdefghij", 4, 4);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].text, "abcd");
    assert_eq!(rows[1].text, "efgh");
    assert_eq!(rows[2].text, "ij");
}

#[test]
fn word_wrap_uses_smaller_cont_width_for_continuations() {
    // First row 10 cells, continuation 6 cells (simulating hanging indent).
    let rows = word_wrap("alpha beta gamma delta", 10, 6);
    assert_eq!(rows[0].text, "alpha beta");
    // Continuation rows are narrower, so "gamma" (5 chars) fits on
    // its own row and "delta" (5 chars) on the next.
    assert!(rows[1].text.starts_with("gamma"));
}

#[test]
fn wrap_title_lines_caps_at_max_lines_with_ellipsis() {
    let rows = wrap_title_lines(
        "alpha beta gamma delta epsilon zeta eta theta iota",
        10,
        10,
        3,
    );
    assert_eq!(rows.len(), 3);
    assert!(rows[2].ends_with('…'));
}

#[test]
fn wrap_title_lines_returns_text_unchanged_when_short() {
    let rows = wrap_title_lines("hello", 20, 20, 3);
    assert_eq!(rows, vec!["hello".to_string()]);
}

#[test]
fn pad_rect_trims_top_bottom_and_right() {
    let r = Rect {
        x: 0,
        y: 5,
        width: 20,
        height: 10,
    };
    let p = pad_rect(r, 2, 1, 3);
    assert_eq!(p.y, 7);
    assert_eq!(p.height, 7);
    assert_eq!(p.width, 17);
    // Over-padding yields a zero-sized rect rather than panicking.
    let p2 = pad_rect(r, 99, 99, 99);
    assert_eq!(p2.height, 0);
    assert_eq!(p2.width, 0);
}

#[test]
fn line_start_byte_handles_multibyte_lines() {
    let body = "héllo\nwörld\n";
    assert_eq!(line_start_byte(body, 0), 0);
    // "héllo" is 6 bytes ('h' + 'é' (2) + 'l' + 'l' + 'o') + '\n' (1) = 7
    assert_eq!(line_start_byte(body, 1), 7);
}

#[test]
fn line_char_len_counts_chars_not_bytes() {
    let body = "héllo\nwörld";
    assert_eq!(line_char_len(body, 0), 5);
    assert_eq!(line_char_len(body, 1), 5);
}

#[test]
fn active_line_count_for_handles_empty_and_trailing_newline() {
    assert_eq!(active_line_count_for(""), 1);
    assert_eq!(active_line_count_for("one"), 1);
    assert_eq!(active_line_count_for("one\ntwo"), 2);
    assert_eq!(active_line_count_for("one\ntwo\n"), 3);
}

#[test]
fn handle_normal_plus_creates_a_note_and_drops_into_insert_mode() {
    let _g = TempHome::set();
    let mut w = make_widget();
    let r = w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    assert_eq!(r, EventResult::Handled);
    let st = w.state.lock().unwrap();
    assert_eq!(st.notes.len(), 1);
    assert_eq!(st.mode, Mode::Insert);
    assert_eq!(st.active, Some(0));
}

#[test]
fn typing_in_insert_mode_appends_chars_to_active_note() {
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    for c in "hi".chars() {
        w.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    let st = w.state.lock().unwrap();
    assert_eq!(st.notes[0].body, "hi");
    assert_eq!(st.cursor_col, 2);
}

#[test]
fn esc_returns_to_normal_mode() {
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    w.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let st = w.state.lock().unwrap();
    assert_eq!(st.mode, Mode::Normal);
}

#[test]
fn ctrl_a_in_insert_jumps_cursor_to_line_start() {
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    for c in "hello".chars() {
        w.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    // Cursor sits at col 5 (after "hello"). Ctrl-A → col 0.
    w.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert_eq!(w.state.lock().unwrap().cursor_col, 0);
}

#[test]
fn ctrl_e_in_insert_jumps_cursor_to_line_end() {
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    for c in "hello".chars() {
        w.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    w.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert_eq!(w.state.lock().unwrap().cursor_col, 0);
    w.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    assert_eq!(w.state.lock().unwrap().cursor_col, 5);
}

#[test]
fn ctrl_u_deletes_current_line_in_insert_mode() {
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    for c in "alpha".chars() {
        w.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    for c in "beta".chars() {
        w.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    // Cursor is now on row 1 ("beta") at col 4. Ctrl-U deletes
    // the current line (and the leading newline), leaving "alpha".
    w.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    let st = w.state.lock().unwrap();
    assert_eq!(st.notes[0].body, "alpha");
}

#[test]
fn ctrl_z_undoes_last_edit_and_ctrl_shift_z_redoes() {
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    for c in "abc".chars() {
        w.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    assert_eq!(w.state.lock().unwrap().notes[0].body, "abc");
    // Undo three times → empty.
    for _ in 0..3 {
        w.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
    }
    assert_eq!(w.state.lock().unwrap().notes[0].body, "");
    // Redo twice → "ab".
    for _ in 0..2 {
        w.handle_key(KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
    }
    assert_eq!(w.state.lock().unwrap().notes[0].body, "ab");
}

#[test]
fn new_edit_after_undo_clears_redo_stack() {
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    for c in "abc".chars() {
        w.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    w.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
    // Redo stack now has one entry. Typing a new char clears it.
    w.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
    // Redo must now be a no-op.
    let body_before = w.state.lock().unwrap().notes[0].body.clone();
    w.handle_key(KeyEvent::new(
        KeyCode::Char('z'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    let body_after = w.state.lock().unwrap().notes[0].body.clone();
    assert_eq!(
        body_before, body_after,
        "redo after new edit must be a no-op"
    );
}

#[test]
fn normal_mode_j_scrolls_content_when_content_focused() {
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    w.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    // Pretend the renderer published "the note overflows the
    // viewport by 5 rows," which is what would normally let j
    // advance. Without this the clamp keeps scroll at 0.
    *w.last_max_content_scroll.lock().unwrap() = 5;
    let before = w.state.lock().unwrap().content_scroll;
    w.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    let after = w.state.lock().unwrap().content_scroll;
    assert_eq!(after, before + 1);
}

#[test]
fn normal_mode_j_is_noop_when_note_fits_in_viewport() {
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    w.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    // max_content_scroll is 0 — note fits, no room to scroll.
    for _ in 0..5 {
        w.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    }
    assert_eq!(w.state.lock().unwrap().content_scroll, 0);
}

#[test]
fn delete_dash_arms_modal_then_y_confirms() {
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    w.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(w.state.lock().unwrap().notes.len(), 1);
    w.handle_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));
    assert!(w.state.lock().unwrap().confirm_delete.is_some());
    w.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    let st = w.state.lock().unwrap();
    assert!(st.notes.is_empty());
    assert!(st.confirm_delete.is_none());
}

#[test]
fn normal_h_jumps_focus_to_list_unconditionally() {
    // Normal mode h is a direct focus jump — it doesn't move a
    // cursor first (normal mode has no cursor).
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    w.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    w.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    assert_eq!(w.state.lock().unwrap().focus, SubFocus::List);
    w.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    assert_eq!(w.state.lock().unwrap().focus, SubFocus::Content);
}

#[test]
fn normal_enter_on_list_focuses_content_and_is_noop_on_content() {
    // Enter on the list pane mirrors `l` / → so "open this note"
    // works with the keystroke users naturally try first. Enter
    // while content is already focused must not consume the event
    // (so it can fall through to global handlers if any).
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    w.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    // List-focused → Enter advances to Content.
    w.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    assert_eq!(w.state.lock().unwrap().focus, SubFocus::List);
    let r = w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(r, EventResult::Handled);
    assert_eq!(w.state.lock().unwrap().focus, SubFocus::Content);
    // Content-focused → Enter is ignored, focus stays put.
    let r = w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(r, EventResult::Ignored);
    assert_eq!(w.state.lock().unwrap().focus, SubFocus::Content);
}

#[test]
fn paste_in_insert_mode_inserts_full_text_atomically() {
    // Reproduces the bug where pasting prose with smart quotes and a
    // newline kicked the user out of insert mode mid-paste — without
    // bracketed paste the embedded escape sequences would fire Esc.
    // Through handle_paste the whole payload arrives atomically and
    // the user stays in Insert.
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    let pasted = "\u{201C}Neque porro quisquam est\u{201D}\nThere is no one\u{2026}";
    let r = w.handle_paste(pasted);
    assert_eq!(r, EventResult::Handled);
    let st = w.state.lock().unwrap();
    assert_eq!(
        st.mode,
        Mode::Insert,
        "must stay in insert mode after paste"
    );
    assert_eq!(st.notes[0].body, pasted);
    assert_eq!(st.cursor_row, 1);
    assert_eq!(st.cursor_col, "There is no one\u{2026}".chars().count());
}

#[test]
fn paste_pushes_one_undo_entry_regardless_of_length() {
    // The whole point of paste_text vs. looping insert_char: a long
    // paste must NOT push one undo snapshot per char (would blow past
    // UNDO_CAP and ruin per-edit undo granularity).
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    let long: String = "Lorem ipsum dolor sit amet. ".repeat(200);
    w.handle_paste(&long);
    let st = w.state.lock().unwrap();
    let note_id = st.notes[0].id.clone();
    let undo_len = st.history.get(&note_id).map(|h| h.undo.len()).unwrap_or(0);
    assert_eq!(undo_len, 1, "paste must collapse to a single undo entry");
    assert_eq!(st.notes[0].body, long);
}

#[test]
fn paste_normalizes_crlf_and_drops_control_chars() {
    // Clipboard line endings differ by OS and pasted text occasionally
    // carries a stray BEL / form-feed from the source page. Normalize
    // CR/CRLF to LF and silently drop other ASCII control chars so the
    // rendered note doesn't include unprintables.
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    w.handle_paste("alpha\r\nbeta\rgamma\x07delta");
    let st = w.state.lock().unwrap();
    assert_eq!(st.notes[0].body, "alpha\nbeta\ngammadelta");
}

#[test]
fn paste_in_normal_mode_is_ignored() {
    // Paste only edits the body in insert mode. In normal mode it
    // must not mutate state — otherwise pasted commas / letters
    // would mascarade as commands.
    let _g = TempHome::set();
    let mut w = make_widget();
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    w.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let r = w.handle_paste("ignored");
    assert_eq!(r, EventResult::Ignored);
    assert_eq!(w.state.lock().unwrap().notes[0].body, "");
}

/// Zoom-contract requirement: normal-mode Esc must propagate to the app-level
/// zoom handler (EventResult::Ignored), not claim the key unconditionally.
/// Without this, pressing Esc while a Notes widget is zoomed would be swallowed
/// by Notes and zoom would never exit. This test is non-negotiable: any Notes
/// refactor that changes this behavior breaks the zoom-exit contract.
#[test]
fn normal_mode_esc_returns_ignored() {
    let mut w = make_widget();
    // Default state is Normal mode.
    assert_eq!(w.state.lock().unwrap().mode, Mode::Normal);
    let result = w.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        result,
        EventResult::Ignored,
        "normal-mode Esc must return Ignored so zoom can exit"
    );
}

#[test]
fn is_capturing_text_false_in_normal_mode() {
    use crate::widgets::Widget;
    let w = make_widget();
    assert_eq!(w.state.lock().unwrap().mode, Mode::Normal);
    assert!(
        !w.is_capturing_text(),
        "is_capturing_text should be false in Normal mode"
    );
}

#[test]
fn is_capturing_text_true_in_insert_mode() {
    use crate::widgets::Widget;
    let _g = TempHome::set();
    let mut w = make_widget();
    // Enter insert mode by creating a note ('+' in normal mode calls create_note,
    // which leaves the widget in Insert mode).
    w.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
    assert_eq!(w.state.lock().unwrap().mode, Mode::Insert);
    assert!(
        w.is_capturing_text(),
        "is_capturing_text should be true in Insert mode"
    );
    // Exit insert mode via Esc (insert-mode Esc → Normal, returns Handled).
    let _ = w.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(w.state.lock().unwrap().mode, Mode::Normal);
    assert!(
        !w.is_capturing_text(),
        "is_capturing_text should be false after returning to Normal mode"
    );
}

/// Single shared TempHome guard for tests that touch ~/.config/glint/notes.
/// Sets XDG_CONFIG_HOME to a per-test directory and removes it on drop.
struct TempHome(std::path::PathBuf);
impl TempHome {
    fn set() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "glint-notes-widget-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        TempHome(dir)
    }
}
impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
