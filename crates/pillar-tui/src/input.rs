//! Port of packages/tui/src/components/input.ts (pi v0.84.3): single-line
//! text input with grapheme-aware cursor movement, Emacs-style kill ring,
//! undo support, bracketed paste, horizontal scrolling, and inverse-video
//! fake cursor rendering.
//!
//! divergences: keybinding dispatch (handleInput over the keybindings
//! table) stays host-side; the port exposes the editing primitives as
//! plain methods so the host maps key sequences onto them. Kitty CSI-u /
//! modifyOtherKeys printable decoding is exposed by keys.rs for the host
//! dispatch loop.

use crate::edit_support::{KillRing, UndoStack, find_word_backward, find_word_forward};
use crate::keybindings::with_global_keybindings;
use crate::keys::decode_printable_key;
use crate::stack_layout::slice_by_column;
use crate::text_utils::{grapheme_clusters, visible_width};

/// Hardware cursor marker (upstream `CURSOR_MARKER`): zero-width escape
/// emitted before the fake cursor for IME positioning.
pub const CURSOR_MARKER: &str = "\u{1b}_pi:c\u{7}";

fn is_control_char(ch: char) -> bool {
    let code = ch as u32;
    code < 32 || code == 0x7f || (0x80..=0x9f).contains(&code)
}

fn clean_pasted_text(text: &str) -> String {
    text.replace("\r\n", "")
        .replace(['\r', '\n'], "")
        .replace('\t', "    ")
}

#[derive(Clone)]
struct InputState {
    #[allow(dead_code)]
    value: String,
    #[allow(dead_code)]
    cursor: usize,
}

/// Single-line text input (upstream `Input`). `cursor` is a byte offset
/// into `value` that is always on a grapheme boundary.
pub struct Input {
    value: String,
    cursor: usize,
    pub focused: bool,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    undo_stack: UndoStack<InputState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

impl Input {
    pub fn new() -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            focused: false,
            kill_ring: KillRing::new(),
            last_action: None,
            undo_stack: UndoStack::new(),
        }
    }

    pub fn get_value(&self) -> &str {
        &self.value
    }

    pub fn set_value(&mut self, value: &str) {
        self.value = value.to_string();
        self.cursor = self.cursor.min(self.value.len());
    }

    pub fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.value.len());
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    // --- editing primitives ------------------------------------------------

    fn is_whitespace_char(ch: char) -> bool {
        ch.is_whitespace()
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(&InputState {
            value: self.value.clone(),
            cursor: self.cursor,
        });
    }

    pub fn undo(&mut self) {
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.value = snapshot.value;
        self.cursor = snapshot.cursor;
        self.last_action = None;
    }

    pub fn insert_character(&mut self, ch: &str) {
        // Undo coalescing (upstream `insertCharacter`): consecutive
        // non-whitespace word chars share one undo unit; whitespace or a
        // different action starts a new one.
        let starts_whitespace = ch.chars().any(Self::is_whitespace_char);
        let coalesce = self.last_action == Some(LastAction::TypeWord)
            && !starts_whitespace
            && !self.value.is_empty();
        if !coalesce {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);
        self.value.insert_str(self.cursor, ch);
        self.cursor += ch.len();
    }

    fn last_grapheme_len(&self, text: &str) -> usize {
        grapheme_clusters(text).last().map_or(1, |g| g.len())
    }

    fn first_grapheme_len(&self, text: &str) -> usize {
        grapheme_clusters(text).first().map_or(1, |g| g.len())
    }

    pub fn backspace(&mut self) {
        self.last_action = None;
        if self.cursor > 0 {
            self.push_undo();
            let before = self.value[..self.cursor].to_string();
            let grapheme_length = self.last_grapheme_len(&before);
            self.value
                .replace_range(self.cursor - grapheme_length..self.cursor, "");
            self.cursor -= grapheme_length;
        }
    }

    pub fn forward_delete(&mut self) {
        self.last_action = None;
        if self.cursor < self.value.len() {
            self.push_undo();
            let after = self.value[self.cursor..].to_string();
            let grapheme_length = self.first_grapheme_len(&after);
            self.value
                .replace_range(self.cursor..self.cursor + grapheme_length, "");
        }
    }

    pub fn delete_to_line_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let deleted = self.value[..self.cursor].to_string();
        self.kill_ring
            .push(&deleted, true, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        self.value = self.value[self.cursor..].to_string();
        self.cursor = 0;
    }

    pub fn delete_to_line_end(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.push_undo();
        let deleted = self.value[self.cursor..].to_string();
        self.kill_ring
            .push(&deleted, false, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        self.value.truncate(self.cursor);
    }

    pub fn delete_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let old_cursor = self.cursor;
        self.move_word_backwards();
        let delete_from = self.cursor;
        self.cursor = old_cursor;
        let deleted = self.value[delete_from..self.cursor].to_string();
        self.kill_ring.push(&deleted, true, was_kill);
        self.last_action = Some(LastAction::Kill);
        self.value.replace_range(delete_from..self.cursor, "");
        self.cursor = delete_from;
    }

    pub fn delete_word_forward(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let old_cursor = self.cursor;
        self.move_word_forwards();
        let delete_to = self.cursor;
        self.cursor = old_cursor;
        let deleted = self.value[self.cursor..delete_to].to_string();
        self.kill_ring.push(&deleted, false, was_kill);
        self.last_action = Some(LastAction::Kill);
        self.value.replace_range(self.cursor..delete_to, "");
    }

    pub fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        self.push_undo();
        self.value.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.last_action = Some(LastAction::Yank);
    }

    pub fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo();
        let prev_text = self.kill_ring.peek().unwrap_or("").to_string();
        self.value
            .replace_range(self.cursor - prev_text.len()..self.cursor, "");
        self.cursor -= prev_text.len();
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or("").to_string();
        self.value.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.last_action = Some(LastAction::Yank);
    }

    pub fn handle_paste(&mut self, pasted_text: &str) {
        self.last_action = None;
        self.push_undo();
        let clean = clean_pasted_text(pasted_text);
        self.value.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    // --- cursor movement ---------------------------------------------------

    pub fn cursor_left(&mut self) {
        self.last_action = None;
        if self.cursor > 0 {
            let before = self.value[..self.cursor].to_string();
            self.cursor -= self.last_grapheme_len(&before);
        }
    }

    pub fn cursor_right(&mut self) {
        self.last_action = None;
        if self.cursor < self.value.len() {
            let after = self.value[self.cursor..].to_string();
            self.cursor += self.first_grapheme_len(&after);
        }
    }

    pub fn cursor_line_start(&mut self) {
        self.last_action = None;
        self.cursor = 0;
    }

    pub fn cursor_line_end(&mut self) {
        self.last_action = None;
        self.cursor = self.value.len();
    }

    pub fn move_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.last_action = None;
        self.cursor = find_word_backward(&self.value, self.cursor, None);
    }

    pub fn move_word_forwards(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.last_action = None;
        self.cursor = find_word_forward(&self.value, self.cursor, None);
    }

    // --- input dispatch ------------------------------------------------------

    /// Dispatch raw terminal data (upstream `handleInput` minus
    /// keybinding-table actions, which stay host-side): bracketed paste
    /// buffering, escape/submit callbacks, printable decoding.
    pub fn handle_input(&mut self, data: &str) {
        // Bracketed paste start/end markers.
        if data.contains("\u{1b}[200~") {
            let rest = data.replacen("\u{1b}[200~", "", 1);
            self.process_paste_chunk(&rest);
            return;
        }
        if data.contains("\u{1b}[201~") {
            let rest = data.replacen("\u{1b}[201~", "", 1);
            if !rest.is_empty() {
                self.handle_input(&rest);
            }
            return;
        }
        // Regular printable input; reject control characters.
        if let Some(printable) = decode_printable_key(data) {
            self.insert_character(&printable);
            return;
        }
        if !data.chars().any(is_control_char) {
            self.insert_character(data);
        }
    }

    fn process_paste_chunk(&mut self, data: &str) {
        // Accumulate the chunk; if it contains the end marker, process up
        // to it and re-dispatch the remainder.
        if let Some(end_index) = data.find("\u{1b}[201~") {
            let content = &data[..end_index];
            self.handle_paste(content);
            let remaining = &data[end_index + "\u{1b}[201~".len()..];
            if !remaining.is_empty() {
                self.handle_input(remaining);
            }
        } else {
            self.handle_paste(data);
        }
    }

    // --- rendering -----------------------------------------------------------

    /// Render a single line with prompt and inverse-video fake cursor
    /// (upstream `render`).
    pub fn render(&self, width: usize) -> Vec<String> {
        let prompt = "> ";
        let Some(available_width) = (width as isize - prompt.len() as isize)
            .checked_rem(1)
            .map(|_| width - prompt.len())
        else {
            return vec![prompt.to_string()];
        };
        if available_width == 0 {
            return vec![prompt.to_string()];
        }

        let mut visible_text = String::new();
        let mut cursor_display = self.cursor;
        let total_width = visible_width(&self.value);

        if (total_width as isize) < available_width as isize {
            visible_text = self.value.clone();
        } else {
            // Horizontal scrolling; reserve one column for the cursor at
            // the end.
            let scroll_width = if self.cursor == self.value.len() {
                available_width - 1
            } else {
                available_width
            };
            let cursor_col = visible_width(&self.value[..self.cursor]);
            if scroll_width > 0 {
                let half_width = scroll_width / 2;
                let start_col = if cursor_col < half_width {
                    0
                } else if cursor_col > total_width.saturating_sub(half_width) {
                    total_width.saturating_sub(scroll_width)
                } else {
                    cursor_col.saturating_sub(half_width)
                };
                visible_text = slice_by_column(&self.value, start_col, scroll_width, true);
                let before = slice_by_column(
                    &self.value,
                    start_col,
                    cursor_col.saturating_sub(start_col),
                    true,
                );
                cursor_display = before.len();
            } else {
                cursor_display = 0;
            }
        }

        // Build the line with the fake cursor: first grapheme at the
        // cursor position, inverse video.
        let after = &visible_text[cursor_display.min(visible_text.len())..];
        let cursor_grapheme = grapheme_clusters(after).first().cloned();
        let at_cursor: String = cursor_grapheme.clone().unwrap_or_else(|| " ".to_string());
        let before_text = visible_text[..cursor_display.min(visible_text.len())].to_string();
        let after_text = {
            let consumed = cursor_grapheme.map_or(0, |g| g.len());
            after.chars().skip(consumed).collect::<String>()
        };
        let marker = if self.focused { CURSOR_MARKER } else { "" };
        let cursor_char = format!("\u{1b}[7m{at_cursor}\u{1b}[27m");
        let text_with_cursor = format!("{before_text}{marker}{cursor_char}{after_text}");

        let visual_length = visible_width(&text_with_cursor);
        let padding = " ".repeat(available_width.saturating_sub(visual_length));
        vec![format!("{prompt}{text_with_cursor}{padding}")]
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

/// Dispatch the keybinding-driven editing keys of `Input.handleInput`
/// (upstream the keybinding section of `Input.handleInput`), returning
/// whether the sequence was consumed. Printable text and bracketed paste
/// fall through to [`Input::handle_input`].
///
/// divergence: upstream dispatches these inside `Input.handleInput`; the port
/// keeps keybinding dispatch host-side (see the module note), so components
/// that own an `Input` (the alt-screen search overlay) call this explicitly.
pub fn dispatch_input_keybinding(input: &mut Input, data: &str) -> bool {
    let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));
    // Escape / submit only notify upstream callbacks; without them the key is
    // still swallowed.
    if matches("tui.select.cancel") {
        return true;
    }
    if matches("tui.editor.undo") {
        input.undo();
        return true;
    }
    if matches("tui.input.submit") || data == "\n" {
        return true;
    }
    if matches("tui.editor.deleteCharBackward") {
        input.backspace();
        return true;
    }
    if matches("tui.editor.deleteCharForward") {
        input.forward_delete();
        return true;
    }
    if matches("tui.editor.deleteWordBackward") {
        input.delete_word_backwards();
        return true;
    }
    if matches("tui.editor.deleteWordForward") {
        input.delete_word_forward();
        return true;
    }
    if matches("tui.editor.deleteToLineStart") {
        input.delete_to_line_start();
        return true;
    }
    if matches("tui.editor.deleteToLineEnd") {
        input.delete_to_line_end();
        return true;
    }
    if matches("tui.editor.yank") {
        input.yank();
        return true;
    }
    if matches("tui.editor.yankPop") {
        input.yank_pop();
        return true;
    }
    if matches("tui.editor.cursorLeft") {
        input.cursor_left();
        return true;
    }
    if matches("tui.editor.cursorRight") {
        input.cursor_right();
        return true;
    }
    if matches("tui.editor.cursorLineStart") {
        input.cursor_line_start();
        return true;
    }
    if matches("tui.editor.cursorLineEnd") {
        input.cursor_line_end();
        return true;
    }
    if matches("tui.editor.cursorWordLeft") {
        input.move_word_backwards();
        return true;
    }
    if matches("tui.editor.cursorWordRight") {
        input.move_word_forwards();
        return true;
    }
    false
}

impl crate::tui::Component for Input {
    fn render(&mut self, width: usize) -> Vec<String> {
        Input::render(self, width)
    }

    fn as_focusable(&mut self) -> Option<&mut dyn crate::tui::Focusable> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

impl crate::tui::Focusable for Input {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}
