//! Sticky Note widget — a vim-flavoured notepad with multi-note
//! navigation. Notes live as one `.md` file per note under
//! `~/.config/glint/notes/<instance>/`; the filesystem is the source of
//! truth, and the widget reloads from disk on startup.
//!
//! ## Modes
//!
//! - **Normal** (default): hjkl/arrow cursor motion, `i` enters insert,
//!   `dd` deletes the current line, `gg`/`G` jump to top/bottom, `y`
//!   yanks the active note to the clipboard, `+`/`-` create/delete a
//!   note (delete prompts a modal confirm), `h`/`l` at the left/right
//!   boundary toggles focus between the list column and the content pane.
//! - **Insert**: typing inserts at the cursor, Backspace deletes the
//!   char before the cursor, Enter splits the line, arrows move the
//!   cursor, ESC returns to normal.
//!
//! ## Layout
//!
//! Wide panes (≥ 60 cols) render a left list column plus right content
//! pane. Narrow panes render content only; the list is reachable via
//! the auto-resize once the pane grows.
//!
//! ## Persistence
//!
//! Saves are debounced via "save on every change" plus an explicit
//! save when leaving insert mode. The atomic-write helper in `store`
//! keeps partial writes from corrupting a note.

pub mod store;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use serde::Deserialize;

use crate::theme::Theme;
use crate::ui::apply_title_row;

use self::store::Note;

use super::{AppContext, EventResult, Widget, WidgetCtx};

pub const KIND: &str = "sticky";

/// User-facing config under `~/.config/glint/sticky.toml` (or
/// `sticky@<instance>.toml`). All fields optional; the widget is
/// usable with an empty file.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StickyConfig {
    /// Per-widget `Shift+<letter>` shortcut preferences. Empty falls
    /// through to a built-in default list.
    #[serde(default)]
    pub shortcuts: Vec<char>,
    /// Per-widget theme overrides (border + text colours). Same shape
    /// as every other widget's `[colors]` block.
    #[serde(default)]
    pub colors: crate::theme::ColorScheme,
}

/// Below this total pane width (in cells) the list column hides and
/// only the content pane is rendered. Above it the list takes up to
/// `LIST_COL_TARGET` cells (capped at `LIST_COL_PCT_OF_PANE` percent
/// of the pane). Picked so a 60-col pane still leaves a usable
/// content area and very wide panes don't waste real estate on the
/// list column.
const LIST_HIDE_BELOW: u16 = 60;
const LIST_COL_TARGET: u16 = 28;
const LIST_COL_PCT_OF_PANE: u16 = 35;

/// Max wrapped rows for a single list entry's title. Hanging indent
/// aligns continuations under the title's first character.
const LIST_WRAP_MAX_LINES: usize = 3;

/// Selection caret for the active list entry when the list has
/// sub-focus. Matches the stocks widget's `▸ ` marker for consistency.
/// All other first-line states use `LIST_PLAIN_PREFIX` so titles
/// across entries vertically align at column 2. Continuation rows
/// (for entries whose title wraps to multiple visual rows) use
/// `LIST_CONT_INDENT` which is one cell wider — that extra cell
/// visually separates a wrapped continuation from the next entry's
/// first line.
const LIST_CARET: &str = "▸ ";
const LIST_PLAIN_PREFIX: &str = "  ";
const LIST_CONT_INDENT: &str = "   ";
const LIST_PREFIX_W: usize = 2;
const LIST_CONT_W: usize = 3;

/// Hanging-indent width used when the content pane auto-wraps a long
/// source line onto multiple visual rows. Continuation rows get this
/// many spaces of left padding so wrapped text aligns under the
/// source line's first character.
const CONTENT_HANGING_INDENT: usize = 2;

/// One blank row at the top of each sub-pane gives the content + list
/// some breathing room below the title border.
const PANE_TOP_PAD: u16 = 1;

/// Matching blank row at the bottom for visual balance with the top pad.
const PANE_BOTTOM_PAD: u16 = 1;

/// One-cell pad on each side of the vertical separator between list
/// and content so neither pane's text crowds the divider.
const SEP_SIDE_PAD: u16 = 1;

/// One-cell pad on the right edge of the content pane so the rightmost
/// character (or the cursor) doesn't sit flush against the border.
const CONTENT_RIGHT_PAD: u16 = 1;

/// Rows reserved at the bottom of the list pane for the
/// `+ new · - delete` footer hint (1 blank spacer row + 1 label row).
const LIST_FOOTER_ROWS: u16 = 2;

/// Per-note undo cap. Above this we drop the oldest entry to keep
/// memory bounded — 100 undos cover essentially any in-session edit
/// trail without growing without bound on a long-running widget.
const UNDO_CAP: usize = 100;

/// Cursor blink half-period. Render polls `Instant::now()` so blink
/// runs without a tokio timer.
const CURSOR_BLINK_HALF_PERIOD_MS: u128 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Insert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubFocus {
    List,
    Content,
}

/// Pending two-key chord in normal mode. Currently only `gg` (jump to
/// top) needs the chord state; cleared on any non-matching key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingChord {
    None,
    G,
}

/// One undo / redo snapshot. Captures the editable state of a note —
/// body + cursor position — at a moment in time. Bodies are cloned
/// (notes are typically small enough that this is cheaper than a CoW
/// tree and far simpler).
#[derive(Debug, Clone)]
struct HistoryEntry {
    body: String,
    cursor_row: usize,
    cursor_col: usize,
}

/// Per-note undo / redo state held in `StickyState` keyed by note id.
/// Preserved across note switches so undoing a typo in note A still
/// works after you've navigated to note B and back. Dropped when a
/// note is deleted.
#[derive(Debug, Default)]
struct NoteHistory {
    undo: Vec<HistoryEntry>,
    redo: Vec<HistoryEntry>,
}

struct StickyState {
    notes: Vec<Note>,
    /// Index into `notes` of the currently-selected note. `None` when
    /// the store is empty.
    active: Option<usize>,
    /// Cursor position inside the active note's body. `row` is a line
    /// index (0-based); `col` is a character index inside that line.
    cursor_row: usize,
    cursor_col: usize,
    /// Scroll offset of the content viewport, in rows.
    content_scroll: u16,
    /// Scroll offset of the list viewport, in rows.
    list_scroll: u16,
    mode: Mode,
    focus: SubFocus,
    pending: PendingChord,
    /// `Some` when a delete is awaiting y/N confirmation.
    confirm_delete: Option<String>,
    /// Transient status line in the title bar — used for feedback like
    /// "Copied to clipboard" or "Delete this note?".
    status: Option<String>,
    /// Per-note undo / redo stacks. Keyed by note id so switching
    /// notes preserves each note's history.
    history: HashMap<String, NoteHistory>,
}

impl Default for StickyState {
    fn default() -> Self {
        Self {
            notes: Vec::new(),
            active: None,
            cursor_row: 0,
            cursor_col: 0,
            content_scroll: 0,
            list_scroll: 0,
            mode: Mode::Normal,
            focus: SubFocus::Content,
            pending: PendingChord::None,
            confirm_delete: None,
            status: None,
            history: HashMap::new(),
        }
    }
}

impl StickyState {
    /// Snapshot the active note's editable state into its undo stack
    /// and clear its redo stack. Called *before* every body-changing
    /// edit (insert, backspace, delete-line) so undo restores the
    /// pre-edit state. No-op when there's no active note.
    fn push_undo_snapshot(&mut self) {
        let Some(active) = self.active else { return };
        let Some(note) = self.notes.get(active) else { return };
        let entry = HistoryEntry {
            body: note.body.clone(),
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
        };
        let h = self.history.entry(note.id.clone()).or_default();
        h.undo.push(entry);
        if h.undo.len() > UNDO_CAP {
            h.undo.remove(0);
        }
        h.redo.clear();
    }
}

pub struct StickyWidget {
    id: String,
    instance: String,
    display_name_cache: String,
    config: StickyConfig,
    state: Arc<Mutex<StickyState>>,
    app_theme: Arc<Theme>,
    theme: Theme,
    shortcut: Option<char>,
    shortcut_prefs: Vec<char>,
    /// Cached most-recent content rect so the mouse handler can route
    /// scroll/click events without re-deriving the layout. Updated on
    /// every render.
    last_content_rect: Arc<Mutex<Rect>>,
    last_list_rect: Arc<Mutex<Rect>>,
    /// Highest valid `content_scroll` for the current note + pane width,
    /// updated by `render_content` after it knows the wrapped row count.
    /// The key handler reads this when bumping j/k so we never scroll
    /// past the last visual row.
    last_max_content_scroll: Arc<Mutex<u16>>,
}

impl StickyWidget {
    pub fn with_config(
        instance: String,
        config: StickyConfig,
        app_theme: Arc<Theme>,
    ) -> Self {
        let id = if instance == "main" {
            "sticky".to_string()
        } else {
            format!("sticky@{instance}")
        };
        let display_name_cache = if instance == "main" {
            "Notes".to_string()
        } else {
            format!("Notes ({instance})")
        };
        let theme = app_theme.with_overrides(&config.colors);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['n', 'o', 't', 'e', 's']
        } else {
            config.shortcuts.clone()
        };
        let mut state = StickyState::default();
        state.notes = store::load_all(&instance);
        if !state.notes.is_empty() {
            state.active = Some(0);
        }
        Self {
            id,
            instance,
            display_name_cache,
            config,
            state: Arc::new(Mutex::new(state)),
            app_theme,
            theme,
            shortcut: None,
            shortcut_prefs,
            last_content_rect: Arc::new(Mutex::new(Rect::default())),
            last_list_rect: Arc::new(Mutex::new(Rect::default())),
            last_max_content_scroll: Arc::new(Mutex::new(0)),
        }
    }

    fn create_note(&self) {
        let mut st = self.state.lock().expect("sticky state poisoned");
        let mut note = Note {
            id: store::new_id(),
            body: String::new(),
            modified: std::time::SystemTime::now(),
        };
        if let Err(err) = store::save(&self.instance, &mut note) {
            tracing::warn!(error = %err, "sticky: save new note failed");
            st.status = Some(format!("Save failed: {err}"));
            return;
        }
        // Insert at the front (most-recently-edited).
        st.notes.insert(0, note);
        st.active = Some(0);
        st.cursor_row = 0;
        st.cursor_col = 0;
        st.content_scroll = 0;
        st.mode = Mode::Insert;
        st.focus = SubFocus::Content;
        st.status = None;
    }

    fn delete_active(&self) {
        let mut st = self.state.lock().expect("sticky state poisoned");
        let Some(active) = st.active else { return };
        let Some(note) = st.notes.get(active) else { return };
        let id = note.id.clone();
        if let Err(err) = store::delete(&self.instance, &id) {
            tracing::warn!(error = %err, "sticky: delete failed");
            st.status = Some(format!("Delete failed: {err}"));
            return;
        }
        st.notes.remove(active);
        st.history.remove(&id);
        st.active = if st.notes.is_empty() {
            None
        } else {
            Some(active.min(st.notes.len() - 1))
        };
        st.cursor_row = 0;
        st.cursor_col = 0;
        st.content_scroll = 0;
        st.pending = PendingChord::None;
        st.confirm_delete = None;
        st.status = None;
    }

    /// Persist the active note's body to disk. Bumps mtime and re-sorts
    /// the list so the just-edited note bubbles to the top.
    fn save_active(&self) {
        let mut st = self.state.lock().expect("sticky state poisoned");
        let Some(active) = st.active else { return };
        let Some(note) = st.notes.get_mut(active) else { return };
        if let Err(err) = store::save(&self.instance, note) {
            tracing::warn!(error = %err, "sticky: save failed");
            st.status = Some(format!("Save failed: {err}"));
            return;
        }
        // Re-sort by mtime desc; restore active to follow the note.
        let active_id = st.notes[active].id.clone();
        st.notes.sort_by(|a, b| b.modified.cmp(&a.modified));
        st.active = st.notes.iter().position(|n| n.id == active_id);
    }

    fn yank_active(&self) {
        let mut st = self.state.lock().expect("sticky state poisoned");
        let Some(active) = st.active else { return };
        let Some(note) = st.notes.get(active) else { return };
        match crate::clipboard::copy(&note.body) {
            Ok(()) => st.status = Some("Copied to clipboard".into()),
            Err(err) => st.status = Some(format!("Copy failed: {err}")),
        }
    }

    fn move_cursor_h(&self, st: &mut StickyState, delta: i32) -> bool {
        // Returns `true` if the cursor moved within the line; `false`
        // if it hit the boundary (callers can interpret false as a
        // focus-toggle trigger when desired).
        if delta == 0 {
            return true;
        }
        let line_len = active_line_len(st);
        let new_col = st.cursor_col as i32 + delta;
        if new_col < 0 {
            return false;
        }
        let clamped = (new_col as usize).min(line_len);
        let moved = clamped != st.cursor_col;
        st.cursor_col = clamped;
        // Right boundary: only treat as "stuck" when we tried to go past
        // the end and the line is not at full width.
        if !moved && delta > 0 {
            return false;
        }
        moved || delta < 0
    }

    fn move_cursor_v(&self, st: &mut StickyState, delta: i32) {
        let total_rows = active_line_count(st);
        if total_rows == 0 {
            st.cursor_row = 0;
            st.cursor_col = 0;
            return;
        }
        let new_row = (st.cursor_row as i32 + delta).clamp(0, total_rows as i32 - 1);
        st.cursor_row = new_row as usize;
        let line_len = active_line_len(st);
        if st.cursor_col > line_len {
            st.cursor_col = line_len;
        }
    }

    fn insert_char(&self, c: char) {
        let mut st = self.state.lock().expect("sticky state poisoned");
        let Some(active) = st.active else { return };
        st.push_undo_snapshot();
        let cursor_row = st.cursor_row;
        let cursor_col = st.cursor_col;
        if let Some(note) = st.notes.get_mut(active) {
            let line_start = line_start_byte(&note.body, cursor_row);
            let byte_offset = char_offset_to_byte(&note.body[line_start..], cursor_col);
            note.body.insert(line_start + byte_offset, c);
            if c == '\n' {
                st.cursor_row += 1;
                st.cursor_col = 0;
            } else {
                st.cursor_col += 1;
            }
        }
        drop(st);
        self.save_active();
    }

    fn backspace(&self) {
        let mut st = self.state.lock().expect("sticky state poisoned");
        let Some(active) = st.active else { return };
        if st.cursor_col == 0 && st.cursor_row == 0 {
            return;
        }
        st.push_undo_snapshot();
        let cursor_row = st.cursor_row;
        let cursor_col = st.cursor_col;
        if let Some(note) = st.notes.get_mut(active) {
            if cursor_col == 0 {
                // Join with previous line.
                let prev_row = cursor_row - 1;
                let prev_len = line_char_len(&note.body, prev_row);
                let line_start = line_start_byte(&note.body, cursor_row);
                // The byte just before line_start is the '\n'; remove it.
                note.body.remove(line_start - 1);
                st.cursor_row = prev_row;
                st.cursor_col = prev_len;
            } else {
                let line_start = line_start_byte(&note.body, cursor_row);
                let line_text = &note.body[line_start..line_end_byte(&note.body, cursor_row)];
                let target_char = cursor_col - 1;
                let byte_offset = char_offset_to_byte(line_text, target_char);
                let absolute = line_start + byte_offset;
                // Remove the char at absolute byte offset.
                let next_char_byte = absolute
                    + note.body[absolute..]
                        .chars()
                        .next()
                        .map(|c| c.len_utf8())
                        .unwrap_or(1);
                note.body.replace_range(absolute..next_char_byte, "");
                st.cursor_col -= 1;
            }
        }
        drop(st);
        self.save_active();
    }

    /// Delete the line the cursor is on (Ctrl-U in insert mode).
    /// Captures an undo snapshot before mutating.
    fn delete_current_line(&self) {
        let mut st = self.state.lock().expect("sticky state poisoned");
        let Some(active) = st.active else { return };
        st.push_undo_snapshot();
        let cursor_row = st.cursor_row;
        if let Some(note) = st.notes.get_mut(active) {
            let start = line_start_byte(&note.body, cursor_row);
            let end = line_end_byte(&note.body, cursor_row);
            // Include the trailing '\n' (if any) so deleting line N
            // doesn't leave a blank line behind.
            let drop_end = if end < note.body.len() { end + 1 } else { end };
            // If this was the last line and there's a preceding '\n',
            // also drop that so the body doesn't end on a dangling newline.
            let drop_start = if drop_end == note.body.len() && start > 0 {
                start - 1
            } else {
                start
            };
            note.body.replace_range(drop_start..drop_end, "");
            let total = active_line_count_for(&note.body);
            if total == 0 {
                st.cursor_row = 0;
            } else if st.cursor_row >= total {
                st.cursor_row = total - 1;
            }
            st.cursor_col = 0;
        }
        drop(st);
        self.save_active();
    }

    fn undo(&self) {
        let mut st = self.state.lock().expect("sticky state poisoned");
        let Some(active) = st.active else { return };
        let note_id = match st.notes.get(active) {
            Some(n) => n.id.clone(),
            None => return,
        };
        // Snapshot the *current* state into redo, then restore the
        // top undo entry.
        let current = HistoryEntry {
            body: st.notes[active].body.clone(),
            cursor_row: st.cursor_row,
            cursor_col: st.cursor_col,
        };
        let entry = {
            let h = st.history.entry(note_id.clone()).or_default();
            let popped = h.undo.pop();
            if popped.is_some() {
                h.redo.push(current);
            }
            popped
        };
        match entry {
            Some(e) => {
                st.notes[active].body = e.body;
                st.cursor_row = e.cursor_row;
                st.cursor_col = e.cursor_col;
                drop(st);
                self.save_active();
            }
            None => {
                st.status = Some("Nothing to undo".into());
            }
        }
    }

    fn redo(&self) {
        let mut st = self.state.lock().expect("sticky state poisoned");
        let Some(active) = st.active else { return };
        let note_id = match st.notes.get(active) {
            Some(n) => n.id.clone(),
            None => return,
        };
        let current = HistoryEntry {
            body: st.notes[active].body.clone(),
            cursor_row: st.cursor_row,
            cursor_col: st.cursor_col,
        };
        let entry = {
            let h = st.history.entry(note_id.clone()).or_default();
            let popped = h.redo.pop();
            if popped.is_some() {
                h.undo.push(current);
            }
            popped
        };
        match entry {
            Some(e) => {
                st.notes[active].body = e.body;
                st.cursor_row = e.cursor_row;
                st.cursor_col = e.cursor_col;
                drop(st);
                self.save_active();
            }
            None => {
                st.status = Some("Nothing to redo".into());
            }
        }
    }

    fn select_note(&self, idx: usize) {
        let mut st = self.state.lock().expect("sticky state poisoned");
        if idx < st.notes.len() {
            st.active = Some(idx);
            st.cursor_row = 0;
            st.cursor_col = 0;
            st.content_scroll = 0;
            st.pending = PendingChord::None;
        }
    }

    fn cycle_active(&self, delta: i32) {
        let mut st = self.state.lock().expect("sticky state poisoned");
        if st.notes.is_empty() {
            return;
        }
        let n = st.notes.len() as i32;
        let cur = st.active.unwrap_or(0) as i32;
        let next = ((cur + delta).rem_euclid(n)) as usize;
        st.active = Some(next);
        st.cursor_row = 0;
        st.cursor_col = 0;
        st.content_scroll = 0;
        st.pending = PendingChord::None;
    }
}

fn active_line_count(st: &StickyState) -> usize {
    let Some(active) = st.active else { return 0 };
    let Some(note) = st.notes.get(active) else { return 0 };
    active_line_count_for(&note.body)
}

fn active_line_count_for(body: &str) -> usize {
    if body.is_empty() {
        return 1; // empty notes render as one blank line
    }
    let mut n = body.lines().count();
    if body.ends_with('\n') {
        n += 1;
    }
    n.max(1)
}

fn active_line_len(st: &StickyState) -> usize {
    let Some(active) = st.active else { return 0 };
    let Some(note) = st.notes.get(active) else { return 0 };
    line_char_len(&note.body, st.cursor_row)
}

fn line_char_len(body: &str, row: usize) -> usize {
    body.lines().nth(row).map(|l| l.chars().count()).unwrap_or(0)
}

/// Byte offset where `row` begins in `body`. `row` may equal the line
/// count (one-past-end) — returns `body.len()` in that case.
fn line_start_byte(body: &str, row: usize) -> usize {
    if row == 0 {
        return 0;
    }
    let mut seen_newlines = 0;
    for (i, b) in body.bytes().enumerate() {
        if b == b'\n' {
            seen_newlines += 1;
            if seen_newlines == row {
                return i + 1;
            }
        }
    }
    body.len()
}

/// Byte offset where `row`'s content ends (exclusive of the trailing
/// newline, if any). Returns `body.len()` if `row` is past the end.
fn line_end_byte(body: &str, row: usize) -> usize {
    let start = line_start_byte(body, row);
    body[start..]
        .find('\n')
        .map(|off| start + off)
        .unwrap_or(body.len())
}

/// Translate a character-count offset to a byte offset within a substring.
fn char_offset_to_byte(s: &str, char_offset: usize) -> usize {
    s.char_indices()
        .nth(char_offset)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

#[async_trait]
impl Widget for StickyWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.display_name_cache
    }

    fn kind(&self) -> &str {
        KIND
    }

    fn instance(&self) -> &str {
        &self.instance
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let st = self.state.lock().expect("sticky state poisoned");
        let title_metadata = self.derive_title_metadata(&st);
        let block = apply_title_row(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(self.theme.border_style(focused)),
            focused,
            &self.display_name_cache,
            title_metadata.as_deref(),
            self.shortcut,
            &self.theme,
            area.width,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        drop(st);

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let show_list = area.width >= LIST_HIDE_BELOW;
        let (list_rect, content_rect) = if show_list {
            // Layout: [list | sep_pad | sep | sep_pad | content]
            // The two SEP_SIDE_PAD columns give visual breathing room
            // around the `│` divider painted by `render_list`.
            let list_w = list_col_width(area.width);
            let split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(list_w),
                    Constraint::Length(SEP_SIDE_PAD),
                    Constraint::Min(20),
                ])
                .split(inner);
            (Some(split[0]), split[2])
        } else {
            (None, inner)
        };

        // Visual breathing: 1 row top + 1 row bottom on both sub-panes;
        // 1 col right pad on content (list's right edge is already
        // separated by the SEP_SIDE_PAD column).
        let list_rect_padded = list_rect.map(|r| {
            pad_rect(r, PANE_TOP_PAD, PANE_BOTTOM_PAD, 0)
        });
        let content_rect_padded = pad_rect(
            content_rect,
            PANE_TOP_PAD,
            PANE_BOTTOM_PAD,
            CONTENT_RIGHT_PAD,
        );

        *self.last_content_rect.lock().unwrap() = content_rect_padded;
        *self.last_list_rect.lock().unwrap() = list_rect_padded.unwrap_or(Rect::default());

        if let Some(rect) = list_rect_padded {
            self.render_list(frame, rect, focused);
        }
        self.render_content(frame, content_rect_padded, focused);

        // Modal delete confirm last so it sits above both panes.
        let st = self.state.lock().expect("sticky state poisoned");
        if let Some(name) = st.confirm_delete.clone() {
            drop(st);
            self.render_confirm_modal(frame, inner, &name);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // Normal mode + the confirm modal accept only unmodified / Shift
        // keystrokes. Insert mode also handles Ctrl-A / Ctrl-E for
        // line-start / line-end jumps, so we don't pre-filter modifiers
        // here — the per-mode handler decides.
        {
            let st = self.state.lock().expect("sticky state poisoned");
            if st.confirm_delete.is_some() {
                drop(st);
                return self.handle_confirm_key(key);
            }
        }
        let mode = self.state.lock().expect("sticky state poisoned").mode;
        match mode {
            Mode::Normal => {
                if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
                    return EventResult::Ignored;
                }
                self.handle_normal_key(key)
            }
            Mode::Insert => self.handle_insert_key(key),
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, _area: Rect) -> EventResult {
        let mode = self.state.lock().expect("sticky state poisoned").mode;
        let content_rect = *self.last_content_rect.lock().unwrap();
        let list_rect = *self.last_list_rect.lock().unwrap();
        let in_content = point_in(content_rect, mouse.column, mouse.row);
        let in_list = list_rect.width > 0 && point_in(list_rect, mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp if matches!(mode, Mode::Normal) => {
                let mut st = self.state.lock().expect("sticky state poisoned");
                if in_list {
                    st.list_scroll = st.list_scroll.saturating_sub(1);
                    return EventResult::Handled;
                } else if in_content {
                    st.content_scroll = st.content_scroll.saturating_sub(1);
                    return EventResult::Handled;
                }
                EventResult::Ignored
            }
            MouseEventKind::ScrollDown if matches!(mode, Mode::Normal) => {
                let mut st = self.state.lock().expect("sticky state poisoned");
                if in_list {
                    st.list_scroll = st.list_scroll.saturating_add(1);
                    return EventResult::Handled;
                } else if in_content {
                    let max = *self.last_max_content_scroll.lock().unwrap();
                    st.content_scroll = st.content_scroll.saturating_add(1).min(max);
                    return EventResult::Handled;
                }
                EventResult::Ignored
            }
            MouseEventKind::Down(_) if in_list => {
                // Each list entry takes 2 rows; mouse.row within
                // list_rect tells us which entry was clicked.
                let st = self.state.lock().expect("sticky state poisoned");
                let inner_y = mouse.row.saturating_sub(list_rect.y);
                let visible_idx = (inner_y / 2) as usize + st.list_scroll as usize;
                drop(st);
                self.select_note(visible_idx);
                let mut st = self.state.lock().expect("sticky state poisoned");
                st.focus = SubFocus::List;
                EventResult::Handled
            }
            MouseEventKind::Down(_) if in_content => {
                let mut st = self.state.lock().expect("sticky state poisoned");
                st.focus = SubFocus::Content;
                // In insert mode, clicking inside the content pane
                // jumps the cursor to the clicked position. In normal
                // mode the cursor is hidden, so we only do the focus
                // shift and skip the position math.
                if st.mode == Mode::Insert {
                    let local_y = mouse.row.saturating_sub(content_rect.y);
                    let local_x = mouse.column.saturating_sub(content_rect.x);
                    if let Some((row, col)) = cursor_position_for_click(
                        &st,
                        local_y,
                        local_x,
                        content_rect.width as usize,
                    ) {
                        st.cursor_row = row;
                        st.cursor_col = col;
                    }
                }
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_command(&mut self, _cmd: &str, _args: &[&str]) -> Result<bool> {
        Ok(false)
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "shortcuts": self.config.shortcuts,
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_cfg: StickyConfig = serde_json::from_value(config)?;
        self.theme = self.app_theme.with_overrides(&new_cfg.colors);
        self.shortcut_prefs = if new_cfg.shortcuts.is_empty() {
            vec!['s', 'n', 't', 'i', 'o']
        } else {
            new_cfg.shortcuts.clone()
        };
        self.config = new_cfg;
        Ok(())
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("+", "new note"),
            ("-", "delete note (confirm)"),
            ("i", "insert mode"),
            ("ESC", "exit insert → normal"),
            ("h / l", "focus list / focus content (normal mode)"),
            ("j / k", "scroll content (or switch notes in list focus)"),
            ("gg / G", "scroll to top / bottom"),
            ("y", "yank entire note to clipboard"),
            ("Ctrl-A / Ctrl-E", "line start / end (insert mode)"),
            ("Ctrl-U", "delete current line (insert mode)"),
            ("Ctrl-Z / Ctrl-Shift-Z", "undo / redo (insert mode)"),
            ("mouse click", "position cursor (insert mode)"),
            ("mouse wheel", "scroll list or content (normal mode)"),
        ]
    }

    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.app_theme = theme;
        self.theme = self.app_theme.with_overrides(&self.config.colors);
    }

    fn shortcut_preferences(&self) -> &[char] {
        &self.shortcut_prefs
    }

    fn set_shortcut(&mut self, shortcut: Option<char>) {
        self.shortcut = shortcut;
    }

    fn shortcut(&self) -> Option<char> {
        self.shortcut
    }

    fn title_metadata(&self) -> Option<String> {
        let st = self.state.lock().expect("sticky state poisoned");
        self.derive_title_metadata(&st)
    }
}

impl StickyWidget {
    fn derive_title_metadata(&self, st: &StickyState) -> Option<String> {
        if let Some(status) = &st.status {
            return Some(status.clone());
        }
        let active_label = match st.active.and_then(|i| st.notes.get(i)) {
            Some(n) => {
                let truncated = truncate_for_meta(n.display_name(), 28);
                let mode_tag = match st.mode {
                    Mode::Normal => "NORMAL",
                    Mode::Insert => "INSERT",
                };
                format!("{truncated} · {mode_tag} · ⧉ y")
            }
            None => format!(
                "no notes · + to create · {} total",
                st.notes.len()
            ),
        };
        Some(active_label)
    }

    fn render_list(&self, frame: &mut Frame, area: Rect, widget_focused: bool) {
        let st = self.state.lock().expect("sticky state poisoned");
        let list_focused = widget_focused && st.focus == SubFocus::List;
        let content_focused = widget_focused && st.focus == SubFocus::Content;

        // Separator picks up the widget's state in three tiers so it
        // reinforces the focus signal the entry styling already gives:
        //   - widget unfocused: dim (it's just chrome)
        //   - widget focused, normal mode: focused-border colour
        //   - insert mode: text_brilliant — matches the body text
        //     tier so the whole widget visibly "lights up" while typing.
        let sep_style = if !widget_focused {
            self.theme.border_unfocused.add_modifier(Modifier::DIM)
        } else if st.mode == Mode::Insert {
            self.theme.text_brilliant
        } else {
            self.theme.border_focused
        };

        let inner_w = area.width.saturating_sub(1) as usize; // reserve last col for separator
        // First-line text width = inner_w - first-line prefix (2 cells).
        // Continuation rows have a 3-cell hanging indent so they
        // visually offset from the next entry's first line — gets
        // one fewer cell for text.
        let text_w_first = inner_w.saturating_sub(LIST_PREFIX_W).max(1);
        let text_w_cont = inner_w.saturating_sub(LIST_CONT_W).max(1);

        let scroll = st.list_scroll as usize;
        let active = st.active.unwrap_or(usize::MAX);

        // Reserve the bottom rows for the footer hint so wrapped
        // entries never overlap with it. When the area is too short
        // for both the footer and any entry, the footer wins — users
        // can scroll to reveal entries but the footer hint is the
        // load-bearing affordance.
        let footer_rows = LIST_FOOTER_ROWS.min(area.height);
        let entries_height = area.height.saturating_sub(footer_rows);

        // Pack entries top-down by their wrapped row count (1..=3) until
        // we run out of vertical space. Don't render a partial entry —
        // either the whole entry fits or we stop.
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(entries_height as usize);
        let mut y_used: u16 = 0;
        for (real_idx, note) in st.notes.iter().enumerate().skip(scroll) {
            let is_active = real_idx == active;
            let name = note.display_name();
            let wrapped = wrap_title_lines(name, text_w_first, text_w_cont, LIST_WRAP_MAX_LINES);
            if y_used + wrapped.len() as u16 > entries_height {
                break;
            }
            let (caret, caret_style, title_style) =
                self.list_entry_styles(list_focused, content_focused, widget_focused, is_active);
            for (li, piece) in wrapped.iter().enumerate() {
                let lead = if li == 0 { caret } else { LIST_CONT_INDENT };
                lines.push(Line::from(vec![
                    Span::styled(lead.to_string(), caret_style),
                    Span::styled(piece.clone(), title_style),
                ]));
            }
            y_used += wrapped.len() as u16;
        }

        if lines.is_empty() {
            // The footer already shows `+ new note · - delete`, so
            // only the "(no notes)" placeholder belongs up top.
            lines.push(Line::from(Span::styled(
                "  (no notes)",
                self.theme.text_dim,
            )));
        }

        // Render text in the left columns, then a separator column.
        // Entries get the top portion of the area; the footer hint
        // sits in the reserved bottom rows.
        let text_rect = Rect {
            x: area.x,
            y: area.y,
            width: area.width.saturating_sub(1),
            height: entries_height,
        };
        let footer_rect = Rect {
            x: area.x,
            y: area.y + entries_height,
            width: area.width.saturating_sub(1),
            height: footer_rows,
        };
        let sep_rect = Rect {
            x: area.x + area.width.saturating_sub(1),
            y: area.y,
            width: 1,
            height: area.height,
        };
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(para, text_rect);
        if footer_rows > 0 {
            // Footer label sits on the last reserved row; the row
            // above it stays blank for visual breathing.
            let mut footer_lines: Vec<Line<'static>> = Vec::with_capacity(footer_rows as usize);
            for _ in 0..footer_rows.saturating_sub(1) {
                footer_lines.push(Line::from(""));
            }
            footer_lines.push(Line::from(Span::styled(
                "  + new note  ·  - delete",
                self.theme.text_dim,
            )));
            frame.render_widget(Paragraph::new(footer_lines), footer_rect);
        }
        // Paint the separator column as repeated │.
        let sep_glyph = "│";
        let sep_lines: Vec<Line<'static>> = (0..area.height)
            .map(|_| Line::from(Span::styled(sep_glyph.to_string(), sep_style)))
            .collect();
        frame.render_widget(Paragraph::new(sep_lines), sep_rect);
    }

    /// Per-entry styling for the list column. Encodes the four-way
    /// focus/active matrix so the rest of `render_list` stays linear.
    ///
    /// - List focused + entry active: `>` caret in the focused colour,
    ///   title in **bold** focused colour — most prominent state.
    /// - List focused + entry inactive: no caret, title in plain text
    ///   (visible enough to navigate to).
    /// - Content focused + entry active: no caret, title in the
    ///   selected colour — the user knows which note's body they're
    ///   currently editing.
    /// - Content focused + entry inactive: no caret, title dimmed —
    ///   pushes attention away from the list.
    /// - Widget not focused at all: same as content-focused (active
    ///   highlighted, others dim), since this just communicates
    ///   "which note is current" without claiming any in-widget focus.
    fn list_entry_styles(
        &self,
        list_focused: bool,
        content_focused: bool,
        widget_focused: bool,
        is_active: bool,
    ) -> (&'static str, Style, Style) {
        if list_focused && is_active {
            (
                LIST_CARET,
                self.theme.text_focused,
                self.theme.text_focused.add_modifier(Modifier::BOLD),
            )
        } else if list_focused {
            (
                LIST_PLAIN_PREFIX,
                self.theme.text_plain,
                self.theme.text_plain,
            )
        } else if (content_focused || !widget_focused) && is_active {
            (
                LIST_PLAIN_PREFIX,
                self.theme.text_selected,
                self.theme.text_selected,
            )
        } else {
            (
                LIST_PLAIN_PREFIX,
                self.theme.text_dim,
                self.theme.text_dim,
            )
        }
    }

    fn render_content(&self, frame: &mut Frame, area: Rect, widget_focused: bool) {
        let st = self.state.lock().expect("sticky state poisoned");
        let content_focused = widget_focused && st.focus == SubFocus::Content;
        let body = match st.active.and_then(|i| st.notes.get(i)) {
            Some(n) => n.body.clone(),
            None => String::new(),
        };

        if st.active.is_none() {
            let dim = self.theme.text_dim;
            let plain = self.theme.text_plain;
            // Group the help into Notes / Navigation / Editing
            // sections. Each section header uses plain (slightly
            // brighter) text; rows are dim. Two-column layout: a
            // narrow key column and a wider description column.
            let row = |key: &str, desc: &str| {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{:<14}", key), plain),
                    Span::styled(desc.to_string(), dim),
                ])
            };
            let header = |label: &str| {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(label.to_string(), plain),
                ])
            };
            let hint = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  No note selected. Press + to create one.",
                    dim,
                )),
                Line::from(""),
                header("Notes"),
                row("+", "new note"),
                row("-", "delete current note (confirm)"),
                row("y", "yank entire note to clipboard"),
                Line::from(""),
                header("Navigation (normal mode)"),
                row("h / l", "focus list / focus content"),
                row("j / k", "scroll content (or switch notes in list)"),
                row("gg / G", "scroll to top / bottom"),
                row("i", "enter insert mode at end of note"),
                Line::from(""),
                header("Editing (insert mode)"),
                row("ESC", "leave insert → normal"),
                row("Ctrl-A / Ctrl-E", "jump to line start / end"),
                row("Ctrl-U", "delete current line"),
                row("Ctrl-Z", "undo"),
                row("Ctrl-Shift-Z", "redo"),
                row("mouse click", "position cursor"),
            ];
            let para = Paragraph::new(hint).wrap(Wrap { trim: false });
            frame.render_widget(para, area);
            return;
        }

        // Each source line is word-wrapped to fit the pane width.
        // Continuation rows are prefixed with a 2-space hanging
        // indent. Each resulting visual row is rendered at its own
        // y offset; we map (cursor_row, cursor_col) onto whichever
        // visual row currently owns that source-column position.
        let source_lines: Vec<String> = if body.is_empty() {
            vec![String::new()]
        } else {
            // `split('\n')` returns one extra trailing "" when body ends
            // with '\n' — that's exactly the empty row the cursor needs
            // to sit on after pressing Enter at end-of-file. Keep it.
            body.split('\n').map(|s| s.to_string()).collect()
        };

        let pane_w = area.width as usize;
        let cont_w = pane_w.saturating_sub(CONTENT_HANGING_INDENT).max(1);

        // Build the flattened visual-row list. Each entry carries the
        // source row + source-column range it represents so the cursor
        // can be placed regardless of how the wrap splits the line.
        let mut visual: Vec<(usize, WrapRow, bool)> = Vec::new();
        for (sr, src) in source_lines.iter().enumerate() {
            let wraps = word_wrap(src, pane_w, cont_w);
            for (wi, wrap) in wraps.into_iter().enumerate() {
                visual.push((sr, wrap, wi > 0));
            }
        }

        let cursor_visible = content_focused && cursor_blink_visible(st.mode);
        let cursor_row = st.cursor_row;
        let cursor_col = st.cursor_col;
        let mode = st.mode;
        let max_rows = area.height as usize;

        // Publish the max valid scroll for the key handler to clamp
        // against; the user can only scroll when the note exceeds the
        // visible row count. Then clamp the rendered scroll so a
        // previously-set value doesn't paint an empty pane.
        let max_scroll: u16 = (visual.len() as u16).saturating_sub(area.height);
        *self.last_max_content_scroll.lock().unwrap() = max_scroll;
        let scroll = (st.content_scroll.min(max_scroll)) as usize;

        // Three-tier body brightness so the user can tell at a glance
        // what's happening: dim when input goes elsewhere, plain when
        // we own focus but aren't editing, brilliant when we're
        // actively editing. Mirrors the cursor blink as a redundant
        // "you are editing right now" signal.
        let body_style = if !content_focused {
            self.theme.text_dim
        } else if st.mode == Mode::Insert {
            self.theme.text_brilliant
        } else {
            self.theme.text_plain
        };

        // Identify which visual row owns the cursor. Prefer the *later*
        // matching row at a wrap boundary so the cursor "moves to the
        // next visual line" the moment the wrap kicks in, matching
        // editor convention.
        let cursor_visual_idx: Option<usize> = if cursor_visible {
            visual
                .iter()
                .enumerate()
                .rev()
                .find(|(_, (sr, w, _))| {
                    *sr == cursor_row
                        && cursor_col >= w.source_col_start
                        && cursor_col <= w.source_col_end
                })
                .map(|(i, _)| i)
        } else {
            None
        };

        for (rendered_idx, vi) in (scroll..visual.len().min(scroll + max_rows)).enumerate() {
            let (_, wrap, is_cont) = &visual[vi];
            let indent: &str = if *is_cont { "  " } else { "" };
            let line = if Some(vi) == cursor_visual_idx {
                let visual_col =
                    indent.chars().count() + (cursor_col - wrap.source_col_start);
                let combined = format!("{indent}{}", wrap.text);
                render_cursor_line(&combined, visual_col, mode, &self.theme)
            } else {
                Line::from(vec![
                    Span::styled(indent.to_string(), body_style),
                    Span::styled(wrap.text.clone(), body_style),
                ])
            };
            let row_rect = Rect {
                x: area.x,
                y: area.y + rendered_idx as u16,
                width: area.width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(line), row_rect);
        }
    }

    fn render_confirm_modal(&self, frame: &mut Frame, parent: Rect, name: &str) {
        let title = truncate_for_meta(name, 40);
        let inner_w = parent.width.min(54).max(28);
        let inner_h: u16 = 7;
        let x = parent.x + parent.width.saturating_sub(inner_w) / 2;
        let y = parent.y + parent.height.saturating_sub(inner_h) / 2;
        let modal = Rect {
            x,
            y,
            width: inner_w,
            height: inner_h,
        };
        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.theme.border_focused)
            .title(Span::styled(
                " Delete note? ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
        let lines: Vec<Line> = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(title, self.theme.text_brilliant),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  [y] confirm  ·  any other key cancels",
                self.theme.text_dim,
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn handle_confirm_key(&self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.delete_active();
                EventResult::Handled
            }
            _ => {
                let mut st = self.state.lock().expect("sticky state poisoned");
                st.confirm_delete = None;
                EventResult::Handled
            }
        }
    }

    fn handle_normal_key(&self, key: KeyEvent) -> EventResult {
        let mut st = self.state.lock().expect("sticky state poisoned");
        st.status = None;

        // Two-key chord handling. `gg` jumps to top in this view-only
        // mode; `dd` was removed because there's no cursor to anchor
        // a "current line" against — line edits happen in insert mode.
        match st.pending {
            PendingChord::G => {
                st.pending = PendingChord::None;
                if matches!(key.code, KeyCode::Char('g')) {
                    st.content_scroll = 0;
                    return EventResult::Handled;
                }
            }
            PendingChord::None => {}
        }

        match key.code {
            KeyCode::Char('+') => {
                drop(st);
                self.create_note();
                EventResult::Handled
            }
            KeyCode::Char('-') => {
                if let Some(active) = st.active {
                    if let Some(note) = st.notes.get(active) {
                        st.confirm_delete = Some(note.display_name().to_string());
                    }
                }
                EventResult::Handled
            }
            KeyCode::Char('i') => {
                st.mode = Mode::Insert;
                st.focus = SubFocus::Content;
                // Land the cursor at end-of-note so insert continues
                // from where typing would naturally pick up. Cleaner
                // than resuming wherever the cursor happened to be
                // from a previous insert session.
                if let Some(active) = st.active {
                    if let Some((row, col)) = st
                        .notes
                        .get(active)
                        .map(|note| {
                            let total = active_line_count_for(&note.body);
                            let last_row = total.saturating_sub(1);
                            (last_row, line_char_len(&note.body, last_row))
                        })
                    {
                        st.cursor_row = row;
                        st.cursor_col = col;
                    }
                }
                EventResult::Handled
            }
            KeyCode::Char('y') => {
                drop(st);
                self.yank_active();
                EventResult::Handled
            }
            KeyCode::Char('g') => {
                st.pending = PendingChord::G;
                EventResult::Handled
            }
            KeyCode::Char('G') => {
                // Scroll to bottom — use the render-published max so
                // we land exactly at the last visual row instead of
                // having render clamp from u16::MAX on the next pass.
                let max = *self.last_max_content_scroll.lock().unwrap();
                st.content_scroll = max;
                EventResult::Handled
            }
            // h / l switch sub-pane focus directly — they never move
            // a "cursor" in normal mode because normal mode has none.
            KeyCode::Char('h') | KeyCode::Left => {
                st.focus = SubFocus::List;
                EventResult::Handled
            }
            KeyCode::Char('l') | KeyCode::Right => {
                st.focus = SubFocus::Content;
                EventResult::Handled
            }
            // j / k:
            //   - List focused: cycle through notes.
            //   - Content focused: scroll the note viewport by one row.
            KeyCode::Char('j') | KeyCode::Down => {
                if st.focus == SubFocus::List {
                    drop(st);
                    self.cycle_active(1);
                } else {
                    // Only advance if the note actually has more rows
                    // than the viewport — `max_scroll` of zero means
                    // everything already fits, so j is a no-op.
                    let max = *self.last_max_content_scroll.lock().unwrap();
                    st.content_scroll = st.content_scroll.saturating_add(1).min(max);
                }
                EventResult::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if st.focus == SubFocus::List {
                    drop(st);
                    self.cycle_active(-1);
                } else {
                    st.content_scroll = st.content_scroll.saturating_sub(1);
                }
                EventResult::Handled
            }
            KeyCode::Esc => EventResult::Handled,
            _ => EventResult::Ignored,
        }
    }

    fn handle_insert_key(&self, key: KeyEvent) -> EventResult {
        // Ctrl chords (insert mode only): editor-style line/edit ops.
        //   Ctrl-A / Ctrl-E — line start / line end
        //   Ctrl-U          — delete current line
        //   Ctrl-Z          — undo
        //   Ctrl-Shift-Z    — redo
        // Anything else with a Ctrl modifier is ignored so the global
        // dispatcher can still handle Ctrl-C, etc.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            match key.code {
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    let mut st = self.state.lock().expect("sticky state poisoned");
                    st.cursor_col = 0;
                    return EventResult::Handled;
                }
                KeyCode::Char('e') | KeyCode::Char('E') => {
                    let mut st = self.state.lock().expect("sticky state poisoned");
                    let len = active_line_len(&st);
                    st.cursor_col = len;
                    return EventResult::Handled;
                }
                KeyCode::Char('u') | KeyCode::Char('U') => {
                    self.delete_current_line();
                    return EventResult::Handled;
                }
                KeyCode::Char('z') | KeyCode::Char('Z') => {
                    if shift {
                        self.redo();
                    } else {
                        self.undo();
                    }
                    return EventResult::Handled;
                }
                _ => return EventResult::Ignored,
            }
        }
        // Strip Shift — letter codes already encode case.
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        match key.code {
            KeyCode::Esc => {
                let mut st = self.state.lock().expect("sticky state poisoned");
                st.mode = Mode::Normal;
                EventResult::Handled
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                EventResult::Handled
            }
            KeyCode::Enter => {
                self.insert_char('\n');
                EventResult::Handled
            }
            KeyCode::Backspace => {
                self.backspace();
                EventResult::Handled
            }
            KeyCode::Left => {
                let mut st = self.state.lock().expect("sticky state poisoned");
                let _ = self.move_cursor_h(&mut st, -1);
                EventResult::Handled
            }
            KeyCode::Right => {
                let mut st = self.state.lock().expect("sticky state poisoned");
                let _ = self.move_cursor_h(&mut st, 1);
                EventResult::Handled
            }
            KeyCode::Up => {
                let mut st = self.state.lock().expect("sticky state poisoned");
                self.move_cursor_v(&mut st, -1);
                EventResult::Handled
            }
            KeyCode::Down => {
                let mut st = self.state.lock().expect("sticky state poisoned");
                self.move_cursor_v(&mut st, 1);
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }
}

/// Translate a click inside the content pane into a `(source_row,
/// source_col)` cursor position. Walks the same word-wrap the
/// renderer uses so the click lands on the source column the user
/// actually sees under their cursor cell. Returns `None` when the
/// click falls past the last rendered visual row (e.g. clicked into
/// the bottom-pad area) — caller leaves the cursor where it was.
fn cursor_position_for_click(
    st: &StickyState,
    local_y: u16,
    local_x: u16,
    pane_w: usize,
) -> Option<(usize, usize)> {
    let active = st.active?;
    let body = st.notes.get(active).map(|n| n.body.clone()).unwrap_or_default();
    let source_lines: Vec<String> = if body.is_empty() {
        vec![String::new()]
    } else {
        body.split('\n').map(|s| s.to_string()).collect()
    };
    let cont_w = pane_w.saturating_sub(CONTENT_HANGING_INDENT).max(1);

    // Build visual rows exactly as render_content does.
    let mut visual: Vec<(usize, WrapRow, bool)> = Vec::new();
    for (sr, src) in source_lines.iter().enumerate() {
        for (wi, wrap) in word_wrap(src, pane_w, cont_w).into_iter().enumerate() {
            visual.push((sr, wrap, wi > 0));
        }
    }

    let target_visual = (st.content_scroll as usize) + (local_y as usize);
    let (source_row, wrap, is_cont) = visual.get(target_visual)?;
    let indent_w = if *is_cont { CONTENT_HANGING_INDENT } else { 0 };
    // Column offset within the visual row, in source-char units:
    //   - clicks inside the indent map to col 0 of the wrap chunk
    //   - clicks past the rendered chars clamp to end-of-chunk
    let visual_col = local_x as usize;
    let in_chunk = visual_col.saturating_sub(indent_w);
    let chunk_chars = wrap.text.chars().count();
    let col_within = in_chunk.min(chunk_chars);
    Some((*source_row, wrap.source_col_start + col_within))
}

fn point_in(rect: Rect, x: u16, y: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

/// Compute the list column's target width for a given pane width.
/// Caps at `LIST_COL_TARGET` cells and `LIST_COL_PCT_OF_PANE` percent
/// of the pane — the percent cap keeps narrow panes from giving the
/// list a disproportionate share of the available cells.
fn list_col_width(pane_width: u16) -> u16 {
    let pct_cap = (pane_width as u32 * LIST_COL_PCT_OF_PANE as u32 / 100) as u16;
    pct_cap.min(LIST_COL_TARGET)
}

/// Shrink `rect` by trimming from each side. Each saturating-subtracts
/// against the relevant dimension, so over-padding yields a zero-sized
/// rect rather than panicking. Callers guard against rendering into
/// empty rects.
fn pad_rect(rect: Rect, top: u16, bottom: u16, right: u16) -> Rect {
    let top_take = top.min(rect.height);
    let after_top = Rect {
        x: rect.x,
        y: rect.y.saturating_add(top_take),
        width: rect.width,
        height: rect.height.saturating_sub(top_take),
    };
    let bottom_take = bottom.min(after_top.height);
    let after_bottom = Rect {
        height: after_top.height.saturating_sub(bottom_take),
        ..after_top
    };
    Rect {
        width: after_bottom.width.saturating_sub(right.min(after_bottom.width)),
        ..after_bottom
    }
}

/// Cursor visibility. Normal mode hides the cursor entirely — it's a
/// view/navigation mode, not an editing one (j/k scroll the note, h/l
/// switch focus, no editing primitives). Insert mode blinks at
/// 500 ms-on / 500 ms-off, which signals "you're typing here right now."
fn cursor_blink_visible(mode: Mode) -> bool {
    match mode {
        Mode::Normal => false,
        Mode::Insert => {
            let elapsed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            (elapsed / CURSOR_BLINK_HALF_PERIOD_MS) % 2 == 0
        }
    }
}

/// Render a single source line with the cursor overlay. Splits the
/// line into "before cursor" / "cursor cell" / "after cursor" spans;
/// the cursor cell shows the underlying char (or a space for end-of-line)
/// painted with reversed video.
fn render_cursor_line(
    line: &str,
    cursor_col: usize,
    mode: Mode,
    theme: &Theme,
) -> Line<'static> {
    let chars: Vec<char> = line.chars().collect();
    let cursor_col = cursor_col.min(chars.len());
    let before: String = chars[..cursor_col].iter().collect();
    let cursor_char: String = if cursor_col < chars.len() {
        chars[cursor_col].to_string()
    } else {
        " ".to_string()
    };
    let after: String = if cursor_col < chars.len() {
        chars[cursor_col + 1..].iter().collect()
    } else {
        String::new()
    };
    let cursor_style = match mode {
        Mode::Normal => Style::default()
            .fg(Color::Black)
            .bg(theme.text_focused.fg.unwrap_or(Color::Cyan))
            .add_modifier(Modifier::BOLD),
        Mode::Insert => Style::default()
            .fg(Color::Black)
            .bg(theme.text_selected.fg.unwrap_or(Color::Yellow))
            .add_modifier(Modifier::BOLD),
    };
    Line::from(vec![
        Span::styled(before, theme.text_plain),
        Span::styled(cursor_char, cursor_style),
        Span::styled(after, theme.text_plain),
    ])
}

/// Word-aware wrap. Splits `text` into rows that fit `first_w` (first
/// row) and `cont_w` (continuation rows) cells. Breaks at spaces when
/// one exists at or before the row boundary; falls back to a hard
/// mid-word break when a single word exceeds the row width. Trailing
/// space at a wrap point is consumed (not echoed) so two consecutive
/// rows don't both end with whitespace artifacts.
///
/// Returns at least one row even for empty input. The output carries
/// no indent prefix — the caller applies its own hanging-indent in
/// rendering.
fn word_wrap(text: &str, first_w: usize, cont_w: usize) -> Vec<WrapRow> {
    if first_w == 0 {
        return vec![WrapRow { text: text.to_string(), source_col_start: 0, source_col_end: text.chars().count() }];
    }
    if text.is_empty() {
        return vec![WrapRow { text: String::new(), source_col_start: 0, source_col_end: 0 }];
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < chars.len() {
        let w = if first { first_w } else { cont_w.max(1) };
        let remaining = chars.len() - start;
        if remaining <= w {
            out.push(WrapRow {
                text: chars[start..].iter().collect(),
                source_col_start: start,
                source_col_end: chars.len(),
            });
            break;
        }
        let upper = start + w;
        // Three cases:
        //  1. `chars[upper]` is a space → the window ends cleanly on a
        //     word boundary; take all of `chars[start..upper]` and skip
        //     the trailing space.
        //  2. Otherwise look for the rightmost space *inside* the
        //     window. If found, break there so we don't split a word.
        //  3. No in-window space → the whole window is one long word;
        //     hard-break mid-word so the user still sees their text.
        let (end_excl, next_start) = if chars.get(upper) == Some(&' ') {
            (upper, upper + 1)
        } else {
            let break_at = chars[start..upper]
                .iter()
                .rposition(|c| *c == ' ')
                .map(|i| start + i);
            match break_at {
                Some(i) if i > start => (i, i + 1),
                _ => (upper, upper),
            }
        };
        out.push(WrapRow {
            text: chars[start..end_excl].iter().collect(),
            source_col_start: start,
            source_col_end: end_excl,
        });
        start = next_start;
        first = false;
    }
    if out.is_empty() {
        out.push(WrapRow {
            text: String::new(),
            source_col_start: 0,
            source_col_end: 0,
        });
    }
    out
}

/// One wrapped visual row + its character-range within the source line.
/// The source-col range lets the content renderer map cursor positions
/// onto visual rows when auto-wrap reflows long lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WrapRow {
    text: String,
    source_col_start: usize,
    /// Exclusive end. May skip a trailing space when the wrap broke
    /// on whitespace, so `source_col_end` of row N may differ from
    /// `source_col_start` of row N+1.
    source_col_end: usize,
}

/// List-pane wrap helper. Word-wraps a title to at most `max_lines`
/// rows, ellipsizing the final row with `…` when more text remains.
/// First row uses `first_w` cells; continuation rows use `cont_w`
/// (typically smaller to account for the hanging-indent prefix).
fn wrap_title_lines(text: &str, first_w: usize, cont_w: usize, max_lines: usize) -> Vec<String> {
    if max_lines == 0 || first_w == 0 {
        return vec![text.to_string()];
    }
    let mut rows = word_wrap(text, first_w, cont_w);
    if rows.len() <= max_lines {
        return rows.into_iter().map(|r| r.text).collect();
    }
    // More text than max_lines can show — keep the first (max_lines-1)
    // rows verbatim, then re-wrap the remaining text into a single
    // row trimmed-and-ellipsized to `cont_w` cells.
    let head: Vec<String> = rows.drain(..max_lines - 1).map(|r| r.text).collect();
    let tail_start = rows.first().map(|r| r.source_col_start).unwrap_or(0);
    let chars: Vec<char> = text.chars().collect();
    let tail: String = chars[tail_start..].iter().collect();
    let last_w = cont_w.max(1);
    let mut tail_row = String::new();
    let tail_chars: Vec<char> = tail.chars().collect();
    let take = last_w.saturating_sub(1).min(tail_chars.len());
    tail_row.extend(tail_chars[..take].iter());
    if tail_chars.len() > take {
        tail_row.push('…');
    }
    let mut out = head;
    out.push(tail_row);
    out
}

fn truncate_for_meta(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Wizard descriptor. Sticky notes have no per-instance fields the
/// wizard manages — the on-disk note files are the data. Surfaces a
/// short blurb so the wizard's widget picker explains what it is.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::WizardDescriptor;
    WizardDescriptor {
        display_name: "Sticky Notes",
        blurb: "A vim-flavoured notepad. Multiple notes; the list \
                sorts by last-edited. Notes persist as one .md file \
                per note under ~/.config/glint/notes/<instance>/ so \
                they're easy to back up or hand-edit.",
        load_from_toml: None,
        render_toml: None,
        fields: Vec::new(),
    }
}

pub fn build(ctx: &WidgetCtx) -> Box<dyn Widget> {
    let cfg: StickyConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(StickyWidget::with_config(
        ctx.instance.clone(),
        cfg,
        ctx.theme.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_widget() -> StickyWidget {
        StickyWidget::with_config(
            "test-main".to_string(),
            StickyConfig::default(),
            Arc::new(Theme::builtin_defaults()),
        )
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
        let r = Rect { x: 0, y: 5, width: 20, height: 10 };
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
        assert_eq!(body_before, body_after, "redo after new edit must be a no-op");
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

    /// Single shared TempHome guard for tests that touch ~/.config/glint/notes.
    /// Sets XDG_CONFIG_HOME to a per-test directory and removes it on drop.
    struct TempHome(std::path::PathBuf);
    impl TempHome {
        fn set() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "glint-sticky-widget-test-{}-{:?}",
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
}
