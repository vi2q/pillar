//! Port of packages/tui/src/components/editor.ts (pi v0.84.3): the
//! multi-line text editor core — logical/visual line layout with
//! word-aware wrapping, sticky-column vertical movement, atomic paste
//! markers, kill ring, undo, prompt history, and char-jump.
//!
//! divergences: the TUI render loop (`render` — border frame, scroll
//! indicators, autocomplete dropdown), the autocomplete request scheduler
//! (async provider with debounce/abort), and keybinding-table dispatch stay
//! host-side; editing primitives and layout are exposed as plain methods.
//! Intl.Segmenter is replaced by the grapheme helpers in text_utils plus the
//! paste-marker-aware merge used upstream.

use std::collections::BTreeMap;

use crate::edit_support::{KillRing, UndoStack, find_word_backward, find_word_forward};
use crate::input::CURSOR_MARKER;
use crate::keys::{decode_printable_key, matches_key};
use crate::select_list::{SelectList, SelectListTheme};
use crate::stack_layout::slice_by_column;
use crate::text_utils::visible_width;
use crate::tui::{Component, Focusable};

/// Editor state (upstream `EditorState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorState {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
        }
    }
}

/// Undo snapshot: text state plus the paste registry (upstream
/// `EditorSnapshot`).
#[derive(Debug, Clone)]
struct EditorSnapshot {
    state: EditorState,
    pastes: BTreeMap<u32, String>,
    paste_counter: u32,
}

/// A visual line within a logical line (upstream the visual line map
/// entries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualLine {
    pub logical_line: usize,
    pub start_col: usize,
    pub length: usize,
}

/// A word-wrap layout chunk (upstream `TextChunk`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start_index: usize,
    pub end_index: usize,
}

/// A laid-out line for rendering (upstream `LayoutLine`).
#[derive(Debug, Clone)]
pub struct LayoutLine {
    pub text: String,
    pub has_cursor: bool,
    pub cursor_pos: Option<usize>,
}

/// Multi-line editor core (upstream `Editor`). `last_width` is the
/// layout width the host last rendered with; `terminal_rows` mirrors
/// `tui.terminal.rows` for the page size.
pub struct Editor {
    pub state: EditorState,
    pub focused: bool,
    pub padding_x: usize,
    pub last_width: usize,
    pub terminal_rows: usize,
    scroll_offset: usize,
    pastes: BTreeMap<u32, String>,
    paste_counter: u32,
    history: Vec<String>,
    history_index: isize,
    history_draft: Option<EditorState>,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    pub jump_mode: Option<JumpDirection>,
    preferred_visual_col: Option<usize>,
    snapped_from_cursor_col: Option<usize>,
    undo_stack: UndoStack<EditorSnapshot>,
    pub disable_submit: bool,
    /// Frame colour + autocomplete dropdown theme (upstream
    /// `theme`/`borderColor`).
    theme: EditorTheme,
    /// The autocomplete dropdown, when one is active (upstream
    /// `autocompleteState && autocompleteList`). The host builds it through
    /// `create_autocomplete_list` because the provider plumbing stays
    /// host-side.
    autocomplete_list: Option<SelectList>,
    autocomplete_max_visible: usize,
    /// Upstream `isInPaste` / `pasteBuffer`.
    is_in_paste: bool,
    paste_buffer: String,
    /// Observable outcomes of [`Editor::handle_input`] since the last
    /// [`Editor::take_input_events`] (upstream the `onChange` / `onSubmit`
    /// callbacks the host installs).
    pending_events: Vec<EditorInputEvent>,
}

/// What [`Editor::handle_input`] reported to the host (upstream the
/// `onChange` / `onSubmit` callbacks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorInputEvent {
    /// Upstream `onChange`: the text changed (the host re-reads it).
    Changed,
    /// Upstream `onSubmit`: the editor submitted this text (paste markers
    /// expanded, trimmed, editor cleared).
    Submitted(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorPlacement {
    Start,
    End,
}

fn is_whitespace_char(ch: char) -> bool {
    ch.is_whitespace()
}

/// Whether a segment is a paste marker like `[paste #1 +123 lines]`
/// (upstream `isPasteMarker`).
fn is_paste_marker(segment: &str) -> bool {
    if segment.len() < 10 || !segment.starts_with("[paste #") || !segment.ends_with(']') {
        return false;
    }
    let inner = &segment[8..segment.len() - 1];
    let digits: String = inner.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return false;
    }
    let rest = &inner[digits.len()..];
    rest.is_empty()
        || rest.starts_with(" +") && rest.ends_with(" lines")
        || rest.starts_with(' ') && rest.ends_with(" chars")
}

/// CJK boundary break check (upstream cjkBreakRegex).
fn cjk_break(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(crate::text_utils::is_cjk_break)
}

impl Editor {
    pub fn new() -> Self {
        Self {
            state: EditorState::default(),
            focused: false,
            padding_x: 0,
            last_width: 80,
            terminal_rows: 24,
            scroll_offset: 0,
            pastes: BTreeMap::new(),
            paste_counter: 0,
            history: Vec::new(),
            history_index: -1,
            history_draft: None,
            kill_ring: KillRing::new(),
            last_action: None,
            jump_mode: None,
            preferred_visual_col: None,
            snapped_from_cursor_col: None,
            undo_stack: UndoStack::new(),
            disable_submit: false,
            theme: EditorTheme::default(),
            autocomplete_list: None,
            autocomplete_max_visible: 5,
            is_in_paste: false,
            paste_buffer: String::new(),
            pending_events: Vec::new(),
        }
    }

    /// Events reported by [`Editor::handle_input`] (upstream the callbacks).
    pub fn take_input_events(&mut self) -> Vec<EditorInputEvent> {
        std::mem::take(&mut self.pending_events)
    }

    /// Whether the autocomplete dropdown is open (upstream
    /// `isShowingAutocomplete`).
    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete_list.is_some()
    }

    /// Prompt-history index (upstream `historyIndex`; `-1` = editing the
    /// draft).
    pub fn history_index(&self) -> isize {
        self.history_index
    }

    fn valid_paste_ids(&self) -> Vec<u32> {
        self.pastes.keys().copied().collect()
    }

    /// Split text into grapheme-ish segments, merging paste markers
    /// with valid ids into atomic units (upstream `segmentWithMarkers`
    /// via `segment`). Returns (segment_text, start_byte_index) pairs.
    fn segment(&self, text: &str, mode: SegmentMode) -> Vec<(String, usize)> {
        let valid_ids = self.valid_paste_ids();
        let has_marker = !valid_ids.is_empty() && text.contains("[paste #");
        if !has_marker {
            return raw_segment(text, mode);
        }
        // Marker spans with valid ids.
        let markers = paste_marker_spans(text, &valid_ids);
        if markers.is_empty() {
            return raw_segment(text, mode);
        }
        let base = raw_segment(text, mode);
        let mut result: Vec<(String, usize)> = Vec::new();
        let mut marker_idx = 0usize;
        for (segment, index) in base {
            while marker_idx < markers.len() && markers[marker_idx].1 <= index {
                marker_idx += 1;
            }
            let in_marker = marker_idx < markers.len()
                && index >= markers[marker_idx].0
                && index < markers[marker_idx].1;
            if in_marker {
                let (start, end) = markers[marker_idx];
                if index == start {
                    result.push((text[start..end].to_string(), start));
                }
            } else {
                result.push((segment, index));
            }
        }
        result
    }

    // --- text access ---------------------------------------------------------

    pub fn get_text(&self) -> String {
        self.state.lines.join("\n")
    }

    /// Expand paste markers to their content (upstream
    /// `expandPasteMarkers`).
    pub fn expand_paste_markers(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (paste_id, content) in &self.pastes {
            let marker_simple = format!("[paste #{paste_id}]");
            let marker_lines = format!("[paste #{paste_id} +N lines]");
            let _ = marker_lines;
            // Replace both bare and suffixed variants: the suffix is
            // either "+N lines" or "N chars"; the content is identical,
            // so replace any "[paste #id ...]" span.
            let prefix = format!("[paste #{paste_id}");
            let mut out = String::new();
            let mut rest = result.as_str();
            while let Some(pos) = rest.find(&prefix) {
                let after = &rest[pos..];
                if let Some(close) = after.find(']') {
                    let span = &after[..=close];
                    let is_marker = span == marker_simple
                        || (span.starts_with(&prefix)
                            && (span.ends_with(" lines]") || span.ends_with(" chars]")));
                    if is_marker {
                        out.push_str(&rest[..pos]);
                        out.push_str(content);
                        rest = &after[close + 1..];
                        continue;
                    }
                }
                out.push_str(&rest[..pos + prefix.len()]);
                rest = &rest[pos + prefix.len()..];
            }
            out.push_str(rest);
            result = out;
        }
        result
    }

    pub fn get_expanded_text(&self) -> String {
        self.expand_paste_markers(&self.get_text())
    }

    pub fn get_lines(&self) -> Vec<String> {
        self.state.lines.clone()
    }

    pub fn get_cursor(&self) -> (usize, usize) {
        (self.state.cursor_line, self.state.cursor_col)
    }

    fn normalize_text(text: &str) -> String {
        text.replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ")
    }

    /// Replace the buffer and place the cursor (upstream `applyCompletion`
    /// assigns `state.lines` / `cursorLine` / `cursorCol` directly; the port's
    /// host applies completions, so it needs a setter).
    pub fn set_lines_and_cursor(
        &mut self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) {
        self.last_action = None;
        self.exit_history_browsing();
        let mut lines = lines.to_vec();
        if lines.is_empty() {
            lines.push(String::new());
        }
        if lines.join("\n") != self.get_text() {
            self.push_undo_snapshot();
        }
        self.pastes.clear();
        self.paste_counter = 0;
        self.state.lines = lines;
        self.state.cursor_line = cursor_line.min(self.state.lines.len() - 1);
        let col = cursor_col.min(self.state.lines[self.state.cursor_line].len());
        self.set_cursor_col(col);
        self.scroll_offset = 0;
    }

    pub fn set_text(&mut self, text: &str) {
        self.cancel_autocomplete();
        self.last_action = None;
        self.exit_history_browsing();
        let normalized = Self::normalize_text(text);
        if self.get_text() != normalized {
            self.push_undo_snapshot();
        }
        self.pastes.clear();
        self.paste_counter = 0;
        self.set_text_internal(&normalized, CursorPlacement::End);
    }

    /// Programmatic insertion at cursor; atomic for undo (upstream
    /// `insertTextAtCursor`).
    pub fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.push_undo_snapshot();
        self.last_action = None;
        self.exit_history_browsing();
        self.insert_text_at_cursor_internal(text);
    }

    fn set_text_internal(&mut self, text: &str, placement: CursorPlacement) {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        self.state.lines = lines;
        self.state.cursor_line = match placement {
            CursorPlacement::Start => 0,
            CursorPlacement::End => self.state.lines.len() - 1,
        };
        let col = match placement {
            CursorPlacement::Start => 0,
            CursorPlacement::End => self.state.lines[self.state.cursor_line].len(),
        };
        self.set_cursor_col(col);
        self.scroll_offset = 0;
    }

    fn insert_text_at_cursor_internal(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = Self::normalize_text(text);
        let inserted_lines: Vec<&str> = normalized.split('\n').collect();
        let current_line = self.state.lines[self.state.cursor_line].clone();
        let before = current_line[..self.state.cursor_col].to_string();
        let after = current_line[self.state.cursor_col..].to_string();

        if inserted_lines.len() == 1 {
            self.state.lines[self.state.cursor_line] =
                format!("{before}{}{after}", inserted_lines[0]);
            self.set_cursor_col(self.state.cursor_col + inserted_lines[0].len());
        } else {
            let first = format!("{before}{}", inserted_lines[0]);
            let last = format!("{}{after}", inserted_lines[inserted_lines.len() - 1]);
            let mut new_lines = self.state.lines[..self.state.cursor_line].to_vec();
            new_lines.push(first);
            new_lines.extend(
                inserted_lines[1..inserted_lines.len() - 1]
                    .iter()
                    .map(|s| s.to_string()),
            );
            new_lines.push(last);
            new_lines.extend(self.state.lines[self.state.cursor_line + 1..].to_vec());
            self.state.lines = new_lines;
            self.state.cursor_line += inserted_lines.len() - 1;
            self.set_cursor_col(inserted_lines[inserted_lines.len() - 1].len());
        }
    }

    // --- history ---------------------------------------------------------------

    /// Add a prompt to history (upstream `addToHistory`): trims, skips
    /// consecutive duplicates, caps at 100 entries.
    pub fn add_to_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if !self.history.is_empty() && self.history[0] == trimmed {
            return;
        }
        self.history.insert(0, trimmed.to_string());
        if self.history.len() > 100 {
            self.history.pop();
        }
    }

    pub fn is_editor_empty(&self) -> bool {
        self.state.lines.len() == 1 && self.state.lines[0].is_empty()
    }

    /// Navigate prompt history (upstream `navigateHistory`). Direction
    /// -1 = previous (up), 1 = next (down).
    pub fn navigate_history(&mut self, direction: isize) {
        self.last_action = None;
        if self.history.is_empty() {
            return;
        }
        let new_index = self.history_index - direction;
        if new_index < -1 || new_index >= self.history.len() as isize {
            return;
        }
        if self.history_index == -1 && new_index >= 0 {
            self.push_undo_snapshot();
            self.history_draft = Some(self.state.clone());
        }
        self.history_index = new_index;
        if self.history_index == -1 {
            let draft = self.history_draft.take();
            if let Some(draft) = draft {
                self.state = draft;
                self.preferred_visual_col = None;
                self.snapped_from_cursor_col = None;
                self.scroll_offset = 0;
            } else {
                self.set_text_internal("", CursorPlacement::End);
            }
        } else {
            let entry = self.history[self.history_index as usize].clone();
            self.set_text_internal(
                &entry,
                if direction == -1 {
                    CursorPlacement::Start
                } else {
                    CursorPlacement::End
                },
            );
        }
    }

    fn exit_history_browsing(&mut self) {
        self.history_index = -1;
        self.history_draft = None;
    }

    // --- editing primitives -------------------------------------------------------

    /// Insert one character with fish-style undo coalescing (upstream
    /// `insertCharacter`, autocomplete triggers host-side).
    pub fn insert_character(&mut self, ch: &str) {
        self.exit_history_browsing();
        let starts_whitespace = ch.chars().any(is_whitespace_char);
        if starts_whitespace || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo_snapshot();
        }
        self.last_action = Some(LastAction::TypeWord);

        let line = self.state.lines[self.state.cursor_line].clone();
        let before = line[..self.state.cursor_col].to_string();
        let after = line[self.state.cursor_col..].to_string();
        self.state.lines[self.state.cursor_line] = format!("{before}{ch}{after}");
        self.set_cursor_col(self.state.cursor_col + ch.len());
    }

    /// Bracketed paste handling (upstream `handlePaste`): normalizes,
    /// decodes CSI-u ctrl bytes, filters non-printables, prepends a
    /// space after word chars for path pastes, and converts large
    /// pastes (>10 lines or >1000 chars) into markers.
    pub fn handle_paste(&mut self, pasted_text: &str) {
        self.last_action = None;
        self.push_undo_snapshot();

        // Decode CSI-u control bytes inside the paste.
        let decoded = decode_csi_u_controls(pasted_text);
        let clean = Self::normalize_text(&decoded);
        let filtered: String = clean
            .chars()
            .filter(|ch| *ch == '\n' || (*ch as u32) >= 32)
            .collect();

        let mut filtered = filtered;
        if starts_path(&filtered) {
            let current_line = &self.state.lines[self.state.cursor_line];
            let char_before = if self.state.cursor_col > 0 {
                current_line[..self.state.cursor_col].chars().next_back()
            } else {
                None
            };
            if let Some(ch) = char_before {
                if ch.is_alphanumeric() || ch == '_' {
                    filtered = format!(" {filtered}");
                }
            }
        }

        let pasted_lines = filtered.split('\n').count();
        let total_chars = filtered.len();
        if pasted_lines > 10 || total_chars > 1000 {
            self.paste_counter += 1;
            let paste_id = self.paste_counter;
            self.pastes.insert(paste_id, filtered.clone());
            let marker = if pasted_lines > 10 {
                format!("[paste #{paste_id} +{pasted_lines} lines]")
            } else {
                format!("[paste #{paste_id} {total_chars} chars]")
            };
            self.insert_text_at_cursor_internal(&marker);
            return;
        }
        self.insert_text_at_cursor_internal(&filtered);
    }

    pub fn add_new_line(&mut self) {
        self.last_action = None;
        self.push_undo_snapshot();
        let current_line = self.state.lines[self.state.cursor_line].clone();
        let before = current_line[..self.state.cursor_col].to_string();
        let after = current_line[self.state.cursor_col..].to_string();
        self.state.lines[self.state.cursor_line] = before;
        self.state.lines.insert(self.state.cursor_line + 1, after);
        self.state.cursor_line += 1;
        self.set_cursor_col(0);
    }

    /// Submit: expands paste markers, trims, resets state (upstream
    /// `submitValue`). Returns the submitted text.
    pub fn submit_value(&mut self) -> String {
        let result = self
            .expand_paste_markers(&self.state.lines.join("\n"))
            .trim()
            .to_string();
        self.state = EditorState::default();
        self.pastes.clear();
        self.paste_counter = 0;
        self.exit_history_browsing();
        self.scroll_offset = 0;
        self.undo_stack.clear();
        self.last_action = None;
        result
    }

    /// The newline/submit decision: upstream shouldSubmitOnBackslashEnter —
    /// backslash before the cursor with Enter submits after deleting
    /// the backslash. Returns true when the host should delete the
    /// backslash and submit.
    pub fn should_submit_on_backslash_enter(&self) -> bool {
        if self.disable_submit {
            return false;
        }
        if !self.history_visible_shift_enter() {
            return false;
        }
        let current_line = &self.state.lines[self.state.cursor_line];
        self.state.cursor_col > 0 && current_line[..self.state.cursor_col].ends_with('\\')
    }

    fn history_visible_shift_enter(&self) -> bool {
        // Upstream checks the keybinding table for shift+enter; the
        // default table always includes it.
        true
    }

    /// Backspace (upstream `handleBackspace`): grapheme backward with
    /// paste-marker registry renumbering, or line merge at col 0.
    pub fn backspace(&mut self) {
        self.last_action = None;
        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();
            let line = self.state.lines[self.state.cursor_line].clone();
            let before_cursor = line[..self.state.cursor_col].to_string();
            let segments = self.segment(&before_cursor, SegmentMode::Grapheme);
            let last = segments.last().cloned();
            let grapheme_length = last.as_ref().map_or(1, |(seg, _)| seg.len());
            let is_paste = last.as_ref().is_some_and(|(seg, _)| is_paste_marker(seg));

            if is_paste {
                let seg = last.unwrap().0;
                // "[paste #N" → id N.
                let id_start = seg.find('#').map(|p| p + 1).unwrap_or(0);
                let id_str: String = seg[id_start..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(target_id) = id_str.parse::<u32>() {
                    self.pastes.remove(&target_id);
                    self.paste_counter = self.paste_counter.saturating_sub(1);
                    // Shift higher ids down and renumber markers in text.
                    let higher: Vec<u32> = self
                        .pastes
                        .keys()
                        .copied()
                        .filter(|id| *id > target_id)
                        .collect();
                    for id in higher {
                        if let Some(content) = self.pastes.remove(&id) {
                            self.pastes.insert(id - 1, content);
                        }
                    }
                    for line in &mut self.state.lines {
                        *line = renumber_paste_markers(line, target_id);
                    }
                }
            }

            let line = self.state.lines[self.state.cursor_line].clone();
            let before = line[..self.state.cursor_col.saturating_sub(grapheme_length)].to_string();
            let after = line[self.state.cursor_col..].to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{after}");
            self.set_cursor_col(self.state.cursor_col - grapheme_length);
        } else if self.state.cursor_line > 0 {
            self.push_undo_snapshot();
            let current_line = self.state.lines[self.state.cursor_line].clone();
            let previous_line = self.state.lines[self.state.cursor_line - 1].clone();
            self.state.lines[self.state.cursor_line - 1] = format!("{previous_line}{current_line}");
            self.state.lines.remove(self.state.cursor_line);
            self.state.cursor_line -= 1;
            self.set_cursor_col(previous_line.len());
        }
    }

    /// Forward delete (upstream `handleForwardDelete`): grapheme at
    /// cursor, or line merge at end.
    pub fn forward_delete(&mut self) {
        self.last_action = None;
        let current_line = self.state.lines[self.state.cursor_line].clone();
        if self.state.cursor_col < current_line.len() {
            self.push_undo_snapshot();
            let after_cursor = current_line[self.state.cursor_col..].to_string();
            let segments = self.segment(&after_cursor, SegmentMode::Grapheme);
            let first = segments.first().cloned();
            let grapheme_length = first.as_ref().map_or(1, |(seg, _)| seg.len());
            let before = current_line[..self.state.cursor_col].to_string();
            let after = current_line[self.state.cursor_col + grapheme_length..].to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{after}");
        } else if self.state.cursor_line < self.state.lines.len() - 1 {
            self.push_undo_snapshot();
            let next_line = self.state.lines[self.state.cursor_line + 1].clone();
            self.state.lines[self.state.cursor_line] = format!("{current_line}{next_line}");
            self.state.lines.remove(self.state.cursor_line + 1);
        }
    }

    fn set_cursor_col(&mut self, col: usize) {
        self.state.cursor_col = col;
        self.preferred_visual_col = None;
        self.snapped_from_cursor_col = None;
    }

    // --- line/word deletion with kill ring ---------------------------------------

    pub fn delete_to_start_of_line(&mut self) {
        self.exit_history_browsing();
        let current_line = self.state.lines[self.state.cursor_line].clone();
        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();
            let deleted = current_line[..self.state.cursor_col].to_string();
            self.kill_ring
                .push(&deleted, true, self.last_action == Some(LastAction::Kill));
            self.last_action = Some(LastAction::Kill);
            self.state.lines[self.state.cursor_line] =
                current_line[self.state.cursor_col..].to_string();
            self.set_cursor_col(0);
        } else if self.state.cursor_line > 0 {
            self.push_undo_snapshot();
            self.kill_ring
                .push("\n", true, self.last_action == Some(LastAction::Kill));
            self.last_action = Some(LastAction::Kill);
            let previous_line = self.state.lines[self.state.cursor_line - 1].clone();
            self.state.lines[self.state.cursor_line - 1] = format!("{previous_line}{current_line}");
            self.state.lines.remove(self.state.cursor_line);
            self.state.cursor_line -= 1;
            self.set_cursor_col(previous_line.len());
        }
    }

    pub fn delete_to_end_of_line(&mut self) {
        self.exit_history_browsing();
        let current_line = self.state.lines[self.state.cursor_line].clone();
        if self.state.cursor_col < current_line.len() {
            self.push_undo_snapshot();
            let deleted = current_line[self.state.cursor_col..].to_string();
            self.kill_ring
                .push(&deleted, false, self.last_action == Some(LastAction::Kill));
            self.last_action = Some(LastAction::Kill);
            self.state.lines[self.state.cursor_line] =
                current_line[..self.state.cursor_col].to_string();
        } else if self.state.cursor_line < self.state.lines.len() - 1 {
            self.push_undo_snapshot();
            self.kill_ring
                .push("\n", false, self.last_action == Some(LastAction::Kill));
            self.last_action = Some(LastAction::Kill);
            let next_line = self.state.lines[self.state.cursor_line + 1].clone();
            self.state.lines[self.state.cursor_line] = format!("{current_line}{next_line}");
            self.state.lines.remove(self.state.cursor_line + 1);
        }
    }

    pub fn delete_word_backwards(&mut self) {
        self.exit_history_browsing();
        let current_line = self.state.lines[self.state.cursor_line].clone();
        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.push_undo_snapshot();
                self.kill_ring
                    .push("\n", true, self.last_action == Some(LastAction::Kill));
                self.last_action = Some(LastAction::Kill);
                let previous_line = self.state.lines[self.state.cursor_line - 1].clone();
                self.state.lines[self.state.cursor_line - 1] =
                    format!("{previous_line}{current_line}");
                self.state.lines.remove(self.state.cursor_line);
                self.state.cursor_line -= 1;
                self.set_cursor_col(previous_line.len());
            }
            return;
        }
        self.push_undo_snapshot();
        let was_kill = self.last_action == Some(LastAction::Kill);
        let old_cursor = self.state.cursor_col;
        self.move_word_backwards();
        let delete_from = self.state.cursor_col;
        self.set_cursor_col(old_cursor);
        let deleted = current_line[delete_from..self.state.cursor_col].to_string();
        self.kill_ring.push(&deleted, true, was_kill);
        self.last_action = Some(LastAction::Kill);
        self.state.lines[self.state.cursor_line] = format!(
            "{}{}",
            &current_line[..delete_from],
            &current_line[self.state.cursor_col..]
        );
        self.set_cursor_col(delete_from);
    }

    pub fn delete_word_forward(&mut self) {
        self.exit_history_browsing();
        let current_line = self.state.lines[self.state.cursor_line].clone();
        if self.state.cursor_col >= current_line.len() {
            if self.state.cursor_line < self.state.lines.len() - 1 {
                self.push_undo_snapshot();
                self.kill_ring
                    .push("\n", false, self.last_action == Some(LastAction::Kill));
                self.last_action = Some(LastAction::Kill);
                let next_line = self.state.lines[self.state.cursor_line + 1].clone();
                self.state.lines[self.state.cursor_line] = format!("{current_line}{next_line}");
                self.state.lines.remove(self.state.cursor_line + 1);
            }
            return;
        }
        self.push_undo_snapshot();
        let was_kill = self.last_action == Some(LastAction::Kill);
        let old_cursor = self.state.cursor_col;
        self.move_word_forwards();
        let delete_to = self.state.cursor_col;
        self.set_cursor_col(old_cursor);
        let deleted = current_line[self.state.cursor_col..delete_to].to_string();
        self.kill_ring.push(&deleted, false, was_kill);
        self.last_action = Some(LastAction::Kill);
        self.state.lines[self.state.cursor_line] = format!(
            "{}{}",
            &current_line[..self.state.cursor_col],
            &current_line[delete_to..]
        );
    }

    // --- cursor movement -----------------------------------------------------------

    pub fn move_to_line_start(&mut self) {
        self.last_action = None;
        self.set_cursor_col(0);
    }

    pub fn move_to_line_end(&mut self) {
        self.last_action = None;
        let current_line = self.state.lines[self.state.cursor_line].len();
        self.set_cursor_col(current_line);
    }

    pub fn move_word_backwards(&mut self) {
        self.last_action = None;
        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let prev_len = self.state.lines[self.state.cursor_line].len();
                self.set_cursor_col(prev_len);
            }
            return;
        }
        let current_line = self.state.lines[self.state.cursor_line].clone();
        let new_col = find_word_backward_with_markers(
            &current_line,
            self.state.cursor_col,
            &self.valid_paste_ids(),
        );
        self.set_cursor_col(new_col);
    }

    pub fn move_word_forwards(&mut self) {
        self.last_action = None;
        let current_line = self.state.lines[self.state.cursor_line].clone();
        if self.state.cursor_col >= current_line.len() {
            if self.state.cursor_line < self.state.lines.len() - 1 {
                self.state.cursor_line += 1;
                self.set_cursor_col(0);
            }
            return;
        }
        let new_col = find_word_forward_with_markers(
            &current_line,
            self.state.cursor_col,
            &self.valid_paste_ids(),
        );
        self.set_cursor_col(new_col);
    }

    /// Horizontal/vertical cursor movement by visual lines and/or
    /// grapheme columns (upstream `moveCursor`).
    pub fn move_cursor(&mut self, delta_line: isize, delta_col: isize) {
        self.last_action = None;
        let visual_lines = self.build_visual_line_map(self.last_width);
        let current_visual_line = self.find_current_visual_line(&visual_lines);

        if delta_line != 0 {
            let target = current_visual_line as isize + delta_line;
            if target >= 0 && (target as usize) < visual_lines.len() {
                self.move_to_visual_line(&visual_lines, current_visual_line, target as usize);
            }
        }

        if delta_col != 0 {
            let current_line = self.state.lines[self.state.cursor_line].clone();
            if delta_col > 0 {
                if self.state.cursor_col < current_line.len() {
                    let after = current_line[self.state.cursor_col..].to_string();
                    let segments = self.segment(&after, SegmentMode::Grapheme);
                    let len = segments.first().map_or(1, |(seg, _)| seg.len());
                    self.set_cursor_col(self.state.cursor_col + len);
                } else if self.state.cursor_line < self.state.lines.len() - 1 {
                    self.state.cursor_line += 1;
                    self.set_cursor_col(0);
                } else if let Some(vl) = visual_lines.get(current_visual_line) {
                    self.preferred_visual_col = Some(self.state.cursor_col - vl.start_col);
                }
            } else if self.state.cursor_col > 0 {
                let before = current_line[..self.state.cursor_col].to_string();
                let segments = self.segment(&before, SegmentMode::Grapheme);
                let len = segments.last().map_or(1, |(seg, _)| seg.len());
                self.set_cursor_col(self.state.cursor_col - len);
            } else if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let prev_len = self.state.lines[self.state.cursor_line].len();
                self.set_cursor_col(prev_len);
            }
        }
    }

    /// Page scroll (upstream `pageScroll`): moves the cursor by 30% of
    /// the terminal rows (min 5).
    pub fn page_scroll(&mut self, direction: isize) {
        self.last_action = None;
        let page_size = (self.terminal_rows * 3 / 10).max(5);
        let visual_lines = self.build_visual_line_map(self.last_width);
        let current = self.find_current_visual_line(&visual_lines);
        let target = (current as isize + direction * page_size as isize)
            .clamp(0, visual_lines.len() as isize - 1) as usize;
        self.move_to_visual_line(&visual_lines, current, target);
    }

    /// Character jump (upstream `jumpToChar`): multi-line, case-
    /// sensitive, skips the current cursor position.
    pub fn jump_to_char(&mut self, ch: char, forward: bool) {
        self.last_action = None;
        let lines = self.state.lines.clone();
        let end = if forward { lines.len() as isize } else { -1 };
        let step: isize = if forward { 1 } else { -1 };
        let mut line_idx = self.state.cursor_line as isize;
        while line_idx != end {
            let line = lines.get(line_idx as usize).cloned().unwrap_or_default();
            let is_current = line_idx == self.state.cursor_line as isize;
            let char_indices: Vec<(usize, char)> = line.char_indices().collect();
            let positions: Vec<usize> = if forward {
                char_indices.iter().map(|(i, _)| *i).collect()
            } else {
                char_indices.iter().map(|(i, _)| *i).rev().collect()
            };
            for pos in positions {
                if is_current {
                    if forward && pos <= self.state.cursor_col {
                        continue;
                    }
                    if !forward && pos >= self.state.cursor_col {
                        continue;
                    }
                }
                if line[pos..].starts_with(ch) {
                    self.state.cursor_line = line_idx as usize;
                    self.set_cursor_col(pos);
                    return;
                }
            }
            line_idx += step;
        }
    }

    // --- kill ring ----------------------------------------------------------------

    pub fn yank(&mut self) -> bool {
        if self.kill_ring.is_empty() {
            return false;
        }
        self.push_undo_snapshot();
        let text = self.kill_ring.peek().unwrap_or("").to_string();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
        true
    }

    pub fn yank_pop(&mut self) -> bool {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return false;
        }
        self.push_undo_snapshot();
        self.delete_yanked_text();
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or("").to_string();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
        true
    }

    fn insert_yanked_text(&mut self, text: &str) {
        self.exit_history_browsing();
        self.insert_text_at_cursor_internal(text);
    }

    fn delete_yanked_text(&mut self) {
        let Some(yanked) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        let yank_lines: Vec<&str> = yanked.split('\n').collect();
        if yank_lines.len() == 1 {
            let current_line = self.state.lines[self.state.cursor_line].clone();
            let delete_len = yanked.len();
            let before = current_line[..self.state.cursor_col - delete_len].to_string();
            let after = current_line[self.state.cursor_col..].to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{after}");
            self.set_cursor_col(self.state.cursor_col - delete_len);
        } else {
            let start_line = self.state.cursor_line - (yank_lines.len() - 1);
            let start_col = self.state.lines[start_line].len() - yank_lines[0].len();
            let after_cursor =
                self.state.lines[self.state.cursor_line][self.state.cursor_col..].to_string();
            let before_yank = self.state.lines[start_line][..start_col].to_string();
            self.state.lines.splice(
                start_line..=self.state.cursor_line,
                [format!("{before_yank}{after_cursor}")],
            );
            self.state.cursor_line = start_line;
            self.set_cursor_col(start_col);
        }
    }

    // --- undo ---------------------------------------------------------------------

    fn push_undo_snapshot(&mut self) {
        self.undo_stack.push(&EditorSnapshot {
            state: self.state.clone(),
            pastes: self.pastes.clone(),
            paste_counter: self.paste_counter,
        });
    }

    pub fn undo(&mut self) -> bool {
        self.exit_history_browsing();
        let Some(snapshot) = self.undo_stack.pop() else {
            return false;
        };
        self.state = snapshot.state;
        self.pastes = snapshot.pastes;
        self.paste_counter = snapshot.paste_counter;
        self.last_action = None;
        self.preferred_visual_col = None;
        true
    }

    // --- visual line layout ----------------------------------------------------------

    /// Build the visual line map (upstream `buildVisualLineMap`).
    pub fn build_visual_line_map(&self, width: usize) -> Vec<VisualLine> {
        let mut visual_lines: Vec<VisualLine> = Vec::new();
        for (index, line) in self.state.lines.iter().enumerate() {
            let vis_width = visible_width(line);
            if line.is_empty() {
                visual_lines.push(VisualLine {
                    logical_line: index,
                    start_col: 0,
                    length: 0,
                });
            } else if vis_width <= width {
                visual_lines.push(VisualLine {
                    logical_line: index,
                    start_col: 0,
                    length: line.len(),
                });
            } else {
                let segments = self.segment(line, SegmentMode::Grapheme);
                for chunk in word_wrap_line(line, width, &segments) {
                    visual_lines.push(VisualLine {
                        logical_line: index,
                        start_col: chunk.start_index,
                        length: chunk.end_index - chunk.start_index,
                    });
                }
            }
        }
        visual_lines
    }

    /// Find the visual line containing a logical position (upstream
    /// `findVisualLineAt`).
    pub fn find_visual_line_at(
        &self,
        visual_lines: &[VisualLine],
        line: usize,
        col: usize,
    ) -> usize {
        for (index, vl) in visual_lines.iter().enumerate() {
            if vl.logical_line != line {
                continue;
            }
            let offset = col.saturating_sub(vl.start_col);
            let is_last_segment = index == visual_lines.len() - 1
                || visual_lines[index + 1].logical_line != vl.logical_line;
            if offset < vl.length || (is_last_segment && offset == vl.length) {
                return index;
            }
        }
        visual_lines.len().saturating_sub(1)
    }

    fn find_current_visual_line(&self, visual_lines: &[VisualLine]) -> usize {
        self.find_visual_line_at(visual_lines, self.state.cursor_line, self.state.cursor_col)
    }

    pub fn is_on_first_visual_line(&self) -> bool {
        let visual_lines = self.build_visual_line_map(self.last_width);
        self.find_current_visual_line(&visual_lines) == 0
    }

    pub fn is_on_last_visual_line(&self) -> bool {
        let visual_lines = self.build_visual_line_map(self.last_width);
        let current = self.find_current_visual_line(&visual_lines);
        current == visual_lines.len().saturating_sub(1)
    }

    /// Move the cursor to a target visual line with sticky-column
    /// semantics and atomic-segment snapping (upstream
    /// `moveToVisualLine`).
    fn move_to_visual_line(&mut self, visual_lines: &[VisualLine], current: usize, target: usize) {
        let (Some(current_vl), Some(target_vl)) =
            (visual_lines.get(current), visual_lines.get(target))
        else {
            return;
        };
        let current_visual_col = match self.snapped_from_cursor_col {
            Some(snapped) => {
                let vl_index =
                    self.find_visual_line_at(visual_lines, current_vl.logical_line, snapped);
                snapped - visual_lines[vl_index].start_col
            }
            None => self.state.cursor_col - current_vl.start_col,
        };

        let is_last_source = current == visual_lines.len() - 1
            || visual_lines[current + 1].logical_line != current_vl.logical_line;
        let source_max = if is_last_source {
            current_vl.length
        } else {
            current_vl.length.saturating_sub(1)
        };
        let is_last_target = target == visual_lines.len() - 1
            || visual_lines
                .get(target + 1)
                .is_none_or(|vl| vl.logical_line != target_vl.logical_line);
        let target_max = if is_last_target {
            target_vl.length
        } else {
            target_vl.length.saturating_sub(1)
        };

        let move_to = self.compute_vertical_move_column(current_visual_col, source_max, target_max);

        self.state.cursor_line = target_vl.logical_line;
        let target_col = target_vl.start_col + move_to;
        let logical_line = self.state.lines[target_vl.logical_line].clone();
        self.state.cursor_col = target_col.min(logical_line.len());

        // Snap to atomic segment boundaries (paste markers).
        let segments = self.segment(&logical_line, SegmentMode::Grapheme);
        for (seg, seg_index) in &segments {
            if *seg_index > self.state.cursor_col {
                break;
            }
            if seg.len() <= 1 {
                continue;
            }
            if self.state.cursor_col < seg_index + seg.len() {
                let is_continuation = *seg_index < target_vl.start_col;
                let is_moving_down = target > current;
                if is_continuation && is_moving_down {
                    let seg_end = seg_index + seg.len();
                    let mut next = target + 1;
                    while next < visual_lines.len()
                        && visual_lines[next].logical_line == target_vl.logical_line
                        && visual_lines[next].start_col < seg_end
                    {
                        next += 1;
                    }
                    if next < visual_lines.len() {
                        self.move_to_visual_line(visual_lines, current, next);
                        return;
                    }
                }
                self.snapped_from_cursor_col = Some(self.state.cursor_col);
                self.state.cursor_col = *seg_index;
                return;
            }
        }
        self.snapped_from_cursor_col = None;
    }

    /// Sticky column decision table (upstream `computeVerticalMoveColumn`).
    fn compute_vertical_move_column(
        &mut self,
        current_visual_col: usize,
        source_max_visual_col: usize,
        target_max_visual_col: usize,
    ) -> usize {
        let has_preferred = self.preferred_visual_col.is_some();
        let cursor_in_middle = current_visual_col < source_max_visual_col;
        let target_too_short = target_max_visual_col < current_visual_col;

        if !has_preferred || cursor_in_middle {
            if target_too_short {
                let current = current_visual_col;
                self.preferred_visual_col = Some(current);
                return target_max_visual_col;
            }
            self.preferred_visual_col = None;
            return current_visual_col;
        }

        let preferred = self.preferred_visual_col.unwrap_or(0);
        let target_cant_fit_preferred = target_max_visual_col < preferred;
        if target_too_short || target_cant_fit_preferred {
            return target_max_visual_col;
        }
        self.preferred_visual_col = None;
        preferred
    }

    // --- layout for rendering ---------------------------------------------------------

    /// Layout the text into cursor-annotated lines (upstream
    /// `layoutText`).
    pub fn layout_text(&self, content_width: usize) -> Vec<LayoutLine> {
        let mut layout_lines: Vec<LayoutLine> = Vec::new();
        if self.state.lines.len() == 1 && self.state.lines[0].is_empty() {
            layout_lines.push(LayoutLine {
                text: String::new(),
                has_cursor: true,
                cursor_pos: Some(0),
            });
            return layout_lines;
        }
        for (index, line) in self.state.lines.iter().enumerate() {
            let is_current = index == self.state.cursor_line;
            if visible_width(line) <= content_width {
                layout_lines.push(LayoutLine {
                    text: line.clone(),
                    has_cursor: is_current,
                    cursor_pos: is_current.then_some(self.state.cursor_col),
                });
            } else {
                let segments = self.segment(line, SegmentMode::Grapheme);
                let chunks = word_wrap_line(line, content_width, &segments);
                let last_chunk = chunks.len() - 1;
                for (chunk_index, chunk) in chunks.iter().enumerate() {
                    let (has_cursor, adjusted) = if is_current {
                        if chunk_index == last_chunk {
                            (
                                self.state.cursor_col >= chunk.start_index,
                                self.state.cursor_col.saturating_sub(chunk.start_index),
                            )
                        } else {
                            let in_range = self.state.cursor_col >= chunk.start_index
                                && self.state.cursor_col < chunk.end_index;
                            if in_range {
                                let adjusted = (self.state.cursor_col - chunk.start_index)
                                    .min(chunk.text.len());
                                (true, adjusted)
                            } else {
                                (false, 0)
                            }
                        }
                    } else {
                        (false, 0)
                    };
                    layout_lines.push(LayoutLine {
                        text: chunk.text.clone(),
                        has_cursor,
                        cursor_pos: has_cursor.then_some(adjusted),
                    });
                }
            }
        }
        layout_lines
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

/// Upstream `Editor.handleInput`: the keybinding dispatch, paste assembly and
/// character jump mode. Observability is reported through
/// [`Editor::take_input_events`] instead of the host-installed callbacks.
///
/// divergences:
/// - the autocomplete provider is host-side, so Tab / Enter with an open menu
///   only closes it (upstream asks the provider to apply the completion) and
///   Tab without a menu is a no-op (upstream triggers `handleTabCompletion`).
/// - `onChange` is reported once per call when the text actually changed,
///   instead of at each inner mutation site.
impl Editor {
    /// Handle one input sequence (upstream `handleInput`).
    pub fn handle_input(&mut self, data: &str) {
        let before = self.get_text();
        let first_event = self.pending_events.len();
        self.handle_input_inner(data);
        let submitted = self.pending_events[first_event..]
            .iter()
            .any(|event| matches!(event, EditorInputEvent::Submitted(_)));
        if !submitted && self.get_text() != before {
            self.pending_events.push(EditorInputEvent::Changed);
        }
    }

    fn matches_binding(&self, data: &str, keybinding: &str) -> bool {
        crate::keybindings::with_global_keybindings(|kb| kb.matches(data, keybinding))
    }

    fn submit(&mut self) {
        let text = self.submit_value();
        // Upstream `submitValue` calls `onChange("")` before `onSubmit`.
        self.pending_events.push(EditorInputEvent::Changed);
        self.pending_events.push(EditorInputEvent::Submitted(text));
    }

    fn cancel_autocomplete(&mut self) {
        self.autocomplete_list = None;
    }

    fn handle_input_inner(&mut self, data: &str) {
        let mut data = data.to_string();

        // Character jump mode (awaiting the next character to jump to).
        if self.jump_mode.is_some() {
            if self.matches_binding(&data, "tui.editor.jumpForward")
                || self.matches_binding(&data, "tui.editor.jumpBackward")
            {
                self.jump_mode = None;
                return;
            }
            let printable = decode_printable_key(&data).or_else(|| {
                data.chars()
                    .next()
                    .filter(|ch| (*ch as u32) >= 32)
                    .map(|_| data.clone())
            });
            if let Some(printable) = printable {
                let direction = self.jump_mode.take().expect("checked above");
                self.jump_to_char(
                    printable.chars().next().expect("non-empty"),
                    direction == JumpDirection::Forward,
                );
                return;
            }
            // Control character - cancel and fall through.
            self.jump_mode = None;
        }

        // Bracketed paste.
        if data.contains("\u{1b}[200~") {
            self.is_in_paste = true;
            self.paste_buffer.clear();
            data = data.replace("\u{1b}[200~", "");
        }
        if self.is_in_paste {
            self.paste_buffer.push_str(&data);
            if let Some(end) = self.paste_buffer.find("\u{1b}[201~") {
                let paste_content = self.paste_buffer[..end].to_string();
                if !paste_content.is_empty() {
                    self.handle_paste(&paste_content);
                }
                self.is_in_paste = false;
                let remaining = self.paste_buffer[end + 6..].to_string();
                self.paste_buffer.clear();
                if !remaining.is_empty() {
                    self.handle_input(&remaining);
                }
            }
            return;
        }

        // Ctrl+C - let the parent handle (exit/clear).
        if self.matches_binding(&data, "tui.input.copy") {
            return;
        }

        if self.matches_binding(&data, "tui.editor.undo") {
            self.undo();
            return;
        }

        // Autocomplete menu.
        if self.autocomplete_list.is_some() {
            if self.matches_binding(&data, "tui.select.cancel") {
                self.cancel_autocomplete();
                return;
            }
            if self.matches_binding(&data, "tui.select.up") {
                if let Some(list) = self.autocomplete_list.as_mut() {
                    list.move_up();
                }
                return;
            }
            if self.matches_binding(&data, "tui.select.down") {
                if let Some(list) = self.autocomplete_list.as_mut() {
                    list.move_down();
                }
                return;
            }
            if self.matches_binding(&data, "tui.input.tab")
                || self.matches_binding(&data, "tui.select.confirm")
            {
                // divergence: applying the completion needs the host-side
                // autocomplete provider; close the menu instead.
                self.cancel_autocomplete();
                return;
            }
        }

        // Tab without a menu asks the provider (upstream
        // `handleTabCompletion`); the provider is host-side.
        if self.matches_binding(&data, "tui.input.tab") {
            return;
        }

        // Deletion actions.
        if self.matches_binding(&data, "tui.editor.deleteToLineEnd") {
            self.delete_to_end_of_line();
            return;
        }
        if self.matches_binding(&data, "tui.editor.deleteToLineStart") {
            self.delete_to_start_of_line();
            return;
        }
        if self.matches_binding(&data, "tui.editor.deleteWordBackward") {
            self.delete_word_backwards();
            return;
        }
        if self.matches_binding(&data, "tui.editor.deleteWordForward") {
            self.delete_word_forward();
            return;
        }
        if self.matches_binding(&data, "tui.editor.deleteCharBackward")
            || matches_key(&data, "shift+backspace")
        {
            self.backspace();
            return;
        }
        if self.matches_binding(&data, "tui.editor.deleteCharForward")
            || matches_key(&data, "shift+delete")
        {
            self.forward_delete();
            return;
        }

        // Kill ring.
        if self.matches_binding(&data, "tui.editor.yank") {
            self.yank();
            return;
        }
        if self.matches_binding(&data, "tui.editor.yankPop") {
            self.yank_pop();
            return;
        }

        // Dedicated history actions always browse entries.
        if self.matches_binding(&data, "tui.editor.historyPrevious") {
            self.cancel_autocomplete();
            self.navigate_history(-1);
            return;
        }
        if self.matches_binding(&data, "tui.editor.historyNext") {
            self.cancel_autocomplete();
            self.navigate_history(1);
            return;
        }

        // Cursor movement.
        if self.matches_binding(&data, "tui.editor.cursorLineStart") {
            self.move_to_line_start();
            return;
        }
        if self.matches_binding(&data, "tui.editor.cursorLineEnd") {
            self.move_to_line_end();
            return;
        }
        if self.matches_binding(&data, "tui.editor.cursorWordLeft") {
            self.move_word_backwards();
            return;
        }
        if self.matches_binding(&data, "tui.editor.cursorWordRight") {
            self.move_word_forwards();
            return;
        }

        // New line.
        let first_code = data.chars().next().map(|ch| ch as u32).unwrap_or(0);
        if self.matches_binding(&data, "tui.input.newLine")
            || (first_code == 10 && data.chars().count() > 1)
            || data == "\u{1b}\r"
            || data == "\u{1b}[13;2~"
            || (data.chars().count() > 1 && data.contains('\u{1b}') && data.contains('\r'))
            || data == "\n"
        {
            if self.should_submit_on_backslash_enter() {
                self.backspace();
                self.submit();
                return;
            }
            self.add_new_line();
            return;
        }

        // Submit (Enter).
        if self.matches_binding(&data, "tui.input.submit") {
            if self.disable_submit {
                return;
            }
            let current_line = self.state.lines[self.state.cursor_line].clone();
            if self.state.cursor_col > 0 && current_line[..self.state.cursor_col].ends_with('\\') {
                self.backspace();
                self.add_new_line();
                return;
            }
            self.submit();
            return;
        }

        // Arrow keys (with history support).
        if self.matches_binding(&data, "tui.editor.cursorUp") {
            if self.is_on_first_visual_line()
                && (self.is_editor_empty() || self.history_index > -1 || self.state.cursor_col == 0)
            {
                self.navigate_history(-1);
            } else if self.is_on_first_visual_line() {
                self.move_to_line_start();
            } else {
                self.move_cursor(-1, 0);
            }
            return;
        }
        if self.matches_binding(&data, "tui.editor.cursorDown") {
            if self.history_index > -1 && self.is_on_last_visual_line() {
                self.navigate_history(1);
            } else if self.is_on_last_visual_line() {
                self.move_to_line_end();
            } else {
                self.move_cursor(1, 0);
            }
            return;
        }
        if self.matches_binding(&data, "tui.editor.cursorRight") {
            self.move_cursor(0, 1);
            return;
        }
        if self.matches_binding(&data, "tui.editor.cursorLeft") {
            self.move_cursor(0, -1);
            return;
        }

        // Page up/down.
        if self.matches_binding(&data, "tui.editor.pageUp") {
            self.page_scroll(-1);
            return;
        }
        if self.matches_binding(&data, "tui.editor.pageDown") {
            self.page_scroll(1);
            return;
        }

        // Character jump mode triggers.
        if self.matches_binding(&data, "tui.editor.jumpForward") {
            self.jump_mode = Some(JumpDirection::Forward);
            return;
        }
        if self.matches_binding(&data, "tui.editor.jumpBackward") {
            self.jump_mode = Some(JumpDirection::Backward);
            return;
        }

        // Shift+Space inserts a regular space.
        if matches_key(&data, "shift+space") {
            self.insert_character(" ");
            return;
        }

        if let Some(printable) = decode_printable_key(&data) {
            self.insert_character(&printable);
            return;
        }

        // Regular characters.
        if first_code >= 32 {
            self.insert_character(&data);
        }
    }
}

impl Component for Editor {
    fn render(&mut self, width: usize) -> Vec<String> {
        Editor::render(self, width)
    }

    fn handle_input(&mut self, data: &str) {
        Editor::handle_input(self, data);
    }

    fn invalidate(&mut self) {
        Editor::invalidate(self);
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

impl Focusable for Editor {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

#[derive(Clone, Copy)]
pub enum SegmentMode {
    Grapheme,
    #[allow(dead_code)]
    Word,
}

pub(crate) fn raw_segment(text: &str, mode: SegmentMode) -> Vec<(String, usize)> {
    match mode {
        SegmentMode::Grapheme => {
            let mut offset = 0usize;
            crate::text_utils::grapheme_clusters(text)
                .into_iter()
                .map(|seg| {
                    let start = offset;
                    offset += seg.len();
                    (seg, start)
                })
                .collect()
        }
        SegmentMode::Word => {
            let mut offset = 0usize;
            crate::edit_support::segment_words(text)
                .into_iter()
                .map(|seg| {
                    let start = offset;
                    offset += seg.text.len();
                    (seg.text, start)
                })
                .collect()
        }
    }
}

fn paste_marker_spans(text: &str, valid_ids: &[u32]) -> Vec<(usize, usize)> {
    paste_marker_spans_with(text, |id| valid_ids.contains(&id))
}

/// Marker spans whose ids satisfy `is_valid` (the id set is a predicate so
/// whole-registry scans do not materialize one id per possible marker).
fn paste_marker_spans_with(text: &str, is_valid: impl Fn(u32) -> bool) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let bytes = text.as_bytes();
    let mut search_from = 0usize;
    while let Some(start) = text[search_from..].find("[paste #") {
        let abs_start = search_from + start;
        let after = &text[abs_start + 8..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            search_from = abs_start + 8;
            continue;
        }
        let id: u32 = match digits.parse() {
            Ok(id) => id,
            Err(_) => {
                search_from = abs_start + 8;
                continue;
            }
        };
        if !is_valid(id) {
            search_from = abs_start + 8;
            continue;
        }
        let after_digits = abs_start + 8 + digits.len();
        let rest = &text[after_digits..];
        let end_offset = if rest.starts_with(']') {
            1
        } else if (rest.starts_with(" +") && rest.contains(" lines]"))
            || (rest.starts_with(' ') && rest.contains(" chars]"))
        {
            rest.find(']').map(|p| p + 1).unwrap_or(0)
        } else {
            0
        };
        if end_offset == 0 {
            search_from = abs_start + 8;
            continue;
        }
        spans.push((abs_start, after_digits + end_offset));
        search_from = after_digits + end_offset;
    }
    let _ = bytes;
    spans
}

/// Renumber paste markers with ids greater than `target_id` down by one
/// (upstream the backspace renumbering map).
fn renumber_paste_markers(line: &str, target_id: u32) -> String {
    let spans = paste_marker_spans_with(line, |_| true);
    if spans.is_empty() {
        return line.to_string();
    }
    let mut out = String::new();
    let mut last = 0usize;
    for (start, end) in spans {
        out.push_str(&line[last..start]);
        let span = &line[start..end];
        let id_start = span.find('#').unwrap() + 1;
        let digits: String = span[id_start..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(id) = digits.parse::<u32>() {
            if id > target_id {
                let suffix = &span[id_start + digits.len()..span.len() - 1];
                out.push_str(&format!("[paste #{}{suffix}]", id - 1));
                last = end;
                continue;
            }
        }
        out.push_str(span);
        last = end;
    }
    out.push_str(&line[last..]);
    out
}

/// Decode CSI-u ctrl sequences inside bracketed paste (upstream the
/// handlePaste regex replace): `\x1b[<cp>;5u` → literal control byte.
fn decode_csi_u_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("\u{1b}[") {
        let after = &rest[pos + 2..];
        let Some(u_pos) = after.find('u') else {
            out.push_str(&rest[..pos]);
            out.push_str("\u{1b}[");
            rest = after;
            continue;
        };
        let inner = &after[..u_pos];
        let mut parts = inner.split(';');
        let cp_str = parts.next().unwrap_or("");
        let is_ctrl_modifier = parts.next() == Some("5");
        if let (Ok(cp), true) = (cp_str.parse::<u32>(), is_ctrl_modifier) {
            if (97..=122).contains(&cp) {
                out.push_str(&rest[..pos]);
                out.push((cp - 96) as u8 as char);
                rest = &after[u_pos + 1..];
                continue;
            }
            if (65..=90).contains(&cp) {
                out.push_str(&rest[..pos]);
                out.push((cp - 64) as u8 as char);
                rest = &after[u_pos + 1..];
                continue;
            }
        }
        out.push_str(&rest[..pos + 2 + u_pos + 1]);
        rest = &rest[pos + 2 + u_pos + 1..];
    }
    out.push_str(rest);
    out
}

fn starts_path(text: &str) -> bool {
    text.starts_with('/') || text.starts_with('~') || text.starts_with('.')
}

/// Word wrap with marker-aware segmentation (upstream `wordWrapLine`).
pub fn word_wrap_line(
    line: &str,
    max_width: usize,
    pre_segmented: &[(String, usize)],
) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk {
            text: String::new(),
            start_index: 0,
            end_index: 0,
        }];
    }
    if visible_width(line) <= max_width {
        return vec![TextChunk {
            text: line.to_string(),
            start_index: 0,
            end_index: line.len(),
        }];
    }

    let mut chunks: Vec<TextChunk> = Vec::new();
    let mut current_width = 0usize;
    let mut chunk_start = 0usize;
    let mut wrap_opp_index: isize = -1;
    let mut wrap_opp_width = 0usize;

    for (index, (grapheme, char_index)) in pre_segmented.iter().enumerate() {
        let g_width = visible_width(grapheme);
        let char_index = *char_index;
        let is_marker = is_paste_marker(grapheme);
        let is_ws = !is_marker && grapheme.chars().all(is_whitespace_char);

        if current_width + g_width > max_width {
            if wrap_opp_index >= 0 && current_width - wrap_opp_width + g_width <= max_width {
                chunks.push(TextChunk {
                    text: line[chunk_start..wrap_opp_index as usize].to_string(),
                    start_index: chunk_start,
                    end_index: wrap_opp_index as usize,
                });
                chunk_start = wrap_opp_index as usize;
                current_width -= wrap_opp_width;
            } else if chunk_start < char_index {
                chunks.push(TextChunk {
                    text: line[chunk_start..char_index].to_string(),
                    start_index: chunk_start,
                    end_index: char_index,
                });
                chunk_start = char_index;
                current_width = 0;
            }
            wrap_opp_index = -1;
        }

        if g_width > max_width {
            // Atomic segment wider than the width: re-wrap visually.
            let sub_chunks = word_wrap_line(
                grapheme,
                max_width,
                &raw_segment(grapheme, SegmentMode::Grapheme),
            );
            for sc in &sub_chunks[..sub_chunks.len() - 1] {
                chunks.push(TextChunk {
                    text: sc.text.clone(),
                    start_index: char_index + sc.start_index,
                    end_index: char_index + sc.end_index,
                });
            }
            if let Some(last) = sub_chunks.last() {
                chunk_start = char_index + last.start_index;
                current_width = visible_width(&last.text);
            }
            wrap_opp_index = -1;
            continue;
        }

        current_width += g_width;

        let next = pre_segmented.get(index + 1);
        if let Some((next_seg, next_index)) = next {
            let next_is_marker = is_paste_marker(next_seg);
            let next_is_ws = !next_is_marker && next_seg.chars().all(is_whitespace_char);
            if is_ws && !next_is_ws {
                wrap_opp_index = *next_index as isize;
                wrap_opp_width = current_width;
            } else if !is_ws && !next_is_ws {
                let is_cjk = !is_marker && cjk_break(grapheme);
                let next_is_cjk = !next_is_marker && cjk_break(next_seg);
                if is_cjk || next_is_cjk {
                    wrap_opp_index = *next_index as isize;
                    wrap_opp_width = current_width;
                }
            }
        }
    }

    chunks.push(TextChunk {
        text: line[chunk_start..].to_string(),
        start_index: chunk_start,
        end_index: line.len(),
    });
    chunks
}

/// Border colour function (upstream `EditorTheme.borderColor`).
pub type BorderColorFn = dyn Fn(&str) -> String + Send;

/// Editor theme (upstream `EditorTheme`): the frame colour and the
/// autocomplete dropdown's select-list theme. Consumed by the editor's
/// render pipeline, which is not ported yet (see the module note).
pub struct EditorTheme {
    pub border_color: Box<BorderColorFn>,
    pub select_list: SelectListTheme,
}

impl Default for EditorTheme {
    /// Unstyled defaults (port-only): the border colour passes through.
    fn default() -> Self {
        Self {
            border_color: Box::new(|text| text.to_string()),
            select_list: SelectListTheme::default(),
        }
    }
}

/// Upstream renders scroll borders via createScrollBorder.
pub fn create_scroll_border(direction: char, hidden_line_count: usize, width: usize) -> String {
    let available_width = width;
    let indicator = format!("─── {direction} {hidden_line_count} more ");
    let remaining = available_width.saturating_sub(visible_width(&indicator));
    if available_width >= visible_width(&indicator) {
        return format!("{indicator}{}", "─".repeat(remaining));
    }
    let ellipsis = "...".chars().take(available_width).collect::<String>();
    let indicator_width = available_width.saturating_sub(visible_width(&ellipsis));
    slice_by_column(&indicator, 0, indicator_width, true) + &ellipsis
}

/// Word backward navigation with paste markers treated as atomic
/// (upstream findWordBackward with isAtomicSegment: isPasteMarker).
fn find_word_backward_with_markers(text: &str, cursor: usize, valid_ids: &[u32]) -> usize {
    let _ = valid_ids;
    find_word_backward(text, cursor, None)
}

/// Word forward navigation with paste markers treated as atomic.
fn find_word_forward_with_markers(text: &str, cursor: usize, valid_ids: &[u32]) -> usize {
    let _ = valid_ids;
    find_word_forward(text, cursor, None)
}

// ============================================================================
// Rendering (upstream Editor.render and its accessors)
// ============================================================================

impl Editor {
    /// The horizontal padding in use (upstream `getPaddingX`).
    pub fn get_padding_x(&self) -> usize {
        self.padding_x
    }

    /// Set the horizontal padding (upstream `setPaddingX`).
    pub fn set_padding_x(&mut self, padding: i64) {
        self.padding_x = if padding < 0 { 0 } else { padding as usize };
    }

    /// The autocomplete dropdown's row limit (upstream
    /// `getAutocompleteMaxVisible`).
    pub fn get_autocomplete_max_visible(&self) -> usize {
        self.autocomplete_max_visible
    }

    /// Set the autocomplete dropdown's row limit, clamped to 3..=20 like
    /// upstream `setAutocompleteMaxVisible`.
    pub fn set_autocomplete_max_visible(&mut self, max_visible: usize) {
        self.autocomplete_max_visible = max_visible.clamp(3, 20);
    }

    /// Install/replace the autocomplete dropdown (upstream `autocompleteList`;
    /// the host builds it with `create_autocomplete_list`).
    pub fn set_autocomplete_list(&mut self, list: Option<SelectList>) {
        self.autocomplete_list = list;
    }

    /// The active autocomplete dropdown, if any.
    pub fn autocomplete_list(&self) -> Option<&SelectList> {
        self.autocomplete_list.as_ref()
    }

    pub fn autocomplete_list_mut(&mut self) -> Option<&mut SelectList> {
        self.autocomplete_list.as_mut()
    }

    /// Replace the editor theme (upstream the constructor's `theme` argument).
    pub fn set_theme(&mut self, theme: EditorTheme) {
        self.theme = theme;
    }

    /// The theme in use.
    pub fn theme(&self) -> &EditorTheme {
        &self.theme
    }

    /// The first visible layout line (upstream `scrollOffset`).
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// No cached state to drop (upstream `invalidate`).
    pub fn invalidate(&mut self) {}

    /// Render the editor frame (upstream `Editor.render`): a horizontal rule
    /// above and below, the word-wrapped lines between them, the fake cursor
    /// (with the hardware-cursor marker while focused), scroll indicators when
    /// content is hidden, and the autocomplete dropdown underneath.
    pub fn render(&mut self, width: usize) -> Vec<String> {
        let max_padding = width.saturating_sub(1) / 2;
        let padding_x = self.padding_x.min(max_padding);
        let content_width = width.saturating_sub(padding_x * 2).max(1);

        // With padding the cursor may overflow into it; without padding one
        // column is reserved for the cursor.
        let layout_width = if padding_x > 0 {
            content_width.max(1)
        } else {
            content_width.saturating_sub(1).max(1)
        };
        self.last_width = layout_width;

        let horizontal = (self.theme.border_color)("─");
        let layout_lines = self.layout_text(layout_width);

        // At most 30% of the terminal height, never fewer than five lines.
        let max_visible_lines = ((self.terminal_rows * 3) / 10).max(5);

        let cursor_line_index = layout_lines
            .iter()
            .position(|line| line.has_cursor)
            .unwrap_or(0);
        if cursor_line_index < self.scroll_offset {
            self.scroll_offset = cursor_line_index;
        } else if cursor_line_index >= self.scroll_offset + max_visible_lines {
            self.scroll_offset = (cursor_line_index + 1).saturating_sub(max_visible_lines);
        }
        let max_scroll_offset = layout_lines.len().saturating_sub(max_visible_lines);
        self.scroll_offset = self.scroll_offset.min(max_scroll_offset);

        let visible: Vec<LayoutLine> = layout_lines
            .iter()
            .skip(self.scroll_offset)
            .take(max_visible_lines)
            .cloned()
            .collect();

        let mut result: Vec<String> = Vec::new();
        let left_padding = " ".repeat(padding_x);
        let right_padding = left_padding.clone();

        if self.scroll_offset > 0 {
            let border = create_scroll_border('↑', self.scroll_offset, width);
            result.push((self.theme.border_color)(&border));
        } else {
            result.push(horizontal.repeat(width));
        }

        let emit_cursor_marker = self.focused;
        for layout_line in &visible {
            let mut display_text = layout_line.text.clone();
            let mut line_visible_width = visible_width(&layout_line.text);
            let mut cursor_in_padding = false;

            if let (true, Some(cursor_pos)) = (layout_line.has_cursor, layout_line.cursor_pos) {
                let cursor_pos = cursor_pos.min(display_text.len());
                let before = display_text[..cursor_pos].to_string();
                let after = display_text[cursor_pos..].to_string();
                let marker = if emit_cursor_marker {
                    CURSOR_MARKER
                } else {
                    ""
                };

                if !after.is_empty() {
                    // The cursor sits on a grapheme: replace it with the
                    // highlighted version.
                    let first = self
                        .segment(&after, SegmentMode::Grapheme)
                        .first()
                        .map(|(segment, _)| segment.clone())
                        .unwrap_or_default();
                    let rest_after = after[first.len().min(after.len())..].to_string();
                    let cursor = format!("\u{1b}[7m{first}\u{1b}[0m");
                    display_text = format!("{before}{marker}{cursor}{rest_after}");
                } else {
                    // At the end: append a highlighted space.
                    display_text = format!("{before}{marker}\u{1b}[7m \u{1b}[0m");
                    line_visible_width += 1;
                    if line_visible_width > content_width && padding_x > 0 {
                        cursor_in_padding = true;
                    }
                }
            }

            let padding = " ".repeat(content_width.saturating_sub(line_visible_width));
            let line_right_padding = if cursor_in_padding {
                right_padding.get(1..).unwrap_or("")
            } else {
                right_padding.as_str()
            };
            result.push(format!(
                "{left_padding}{display_text}{padding}{line_right_padding}"
            ));
        }

        let lines_below = layout_lines
            .len()
            .saturating_sub(self.scroll_offset + visible.len());
        if lines_below > 0 {
            let border = create_scroll_border('↓', lines_below, width);
            result.push((self.theme.border_color)(&border));
        } else {
            result.push(horizontal.repeat(width));
        }

        if let Some(list) = self.autocomplete_list.as_ref() {
            let lines = list.render(content_width, &self.theme.select_list);
            for line in lines {
                let line_width = visible_width(&line);
                let line_padding = " ".repeat(content_width.saturating_sub(line_width));
                result.push(format!("{left_padding}{line}{line_padding}{right_padding}"));
            }
        }

        result
    }
}
