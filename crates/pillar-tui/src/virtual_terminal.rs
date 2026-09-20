//! Port of packages/tui/test/virtual-terminal.ts (pi v0.84.3) as a
//! Rust test emulator: a terminal screen model that interprets the
//! control sequences the TUI renderer emits (CUP, cursor movement,
//! erase line/screen, clear scrollback, synchronized output, bracketed
//! paste toggles, cursor visibility) and exposes a viewport + scroll
//! buffer for parity assertions.
//!
//! divergences: full VT100/xterm emulation (xterm.js headless) is out
//! of scope — the subset of sequences the renderer actually emits is
//! interpreted, which is what the parity suites need. No scroll region
//! handling, no wide-char overlap wrap (cells past the width are
//! truncated), no SGR state tracking beyond what string comparison
//! needs (content is stored raw including ANSI).

/// Input handler type (raw terminal data callback).
pub type InputHandler = Box<dyn FnMut(&str)>;

/// Byte cap for a short escape form (CSI and two-char escapes): these are
/// fixed-shape and never legitimately long, so a longer run is malformed.
const MAX_ESCAPE_LEN: usize = 32;

/// Byte cap for OSC/APC sequences. Unlike CSI these carry arbitrary payloads
/// (the renderer's window title embeds the session id), so the guard is only a
/// memory bound, not a validity heuristic.
const MAX_OSC_APC_LEN: usize = 4096;

/// A minimal terminal screen model for renderer tests (upstream
/// `VirtualTerminal`).
pub struct VirtualTerminal {
    columns: usize,
    rows: usize,
    /// Scrollback + viewport rows, each row raw content.
    buffer: Vec<String>,
    /// Index into `buffer` of viewport row 0.
    viewport_y: usize,
    cursor_x: usize,
    cursor_y: usize,
    /// Pending escape sequence being accumulated.
    pending_escape: String,
    input_handler: Option<InputHandler>,
    resize_handler: Option<Box<dyn FnMut()>>,
    kitty_protocol_active: bool,
}

impl VirtualTerminal {
    pub fn new(columns: usize, rows: usize) -> Self {
        let rows_total = rows;
        let mut buffer = Vec::with_capacity(rows_total);
        for _ in 0..rows_total {
            buffer.push(String::new());
        }
        Self {
            columns,
            rows,
            buffer,
            viewport_y: 0,
            cursor_x: 0,
            cursor_y: 0,
            pending_escape: String::new(),
            input_handler: None,
            resize_handler: None,
            kitty_protocol_active: true,
        }
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn set_input_handler(&mut self, handler: InputHandler) {
        self.input_handler = Some(handler);
    }

    pub fn send_input(&mut self, data: &str) {
        if let Some(handler) = &mut self.input_handler {
            handler(data);
        }
    }

    pub fn resize(&mut self, columns: usize, rows: usize) {
        self.columns = columns;
        self.rows = rows;
        if let Some(handler) = &mut self.resize_handler {
            handler();
        }
    }

    /// Write terminal data, interpreting the renderer's control
    /// sequences (upstream `write` → xterm).
    pub fn write(&mut self, data: &str) {
        for ch in data.chars() {
            self.write_char(ch);
        }
    }

    fn write_char(&mut self, ch: char) {
        if !self.pending_escape.is_empty() {
            self.pending_escape.push(ch);
            if self.try_interpret_pending() {
                return;
            }
            // A malformed sequence must not buffer forever. OSC/APC can be
            // legitimately long (a title embeds the session id), so they only
            // get a generous memory bound; the fixed-shape escapes keep the
            // tight one.
            let limit = if self.pending_escape.starts_with("\u{1b}]")
                || self.pending_escape.starts_with("\u{1b}_")
            {
                MAX_OSC_APC_LEN
            } else {
                MAX_ESCAPE_LEN
            };
            if self.pending_escape.len() > limit {
                // Give up and flush as text. This must not feed the buffer
                // back through `write_char`: the leading ESC would start a
                // new pending escape and recurse until the stack overflows.
                let text = std::mem::take(&mut self.pending_escape);
                self.write_plain(&text);
            }
            return;
        }
        if ch == '\u{1b}' {
            self.pending_escape.push(ch);
            return;
        }
        self.put_plain_char(ch);
    }

    /// Write one character without treating it as the start of an escape
    /// sequence.
    fn put_plain_char(&mut self, ch: char) {
        match ch {
            '\r' => self.cursor_x = 0,
            '\n' => self.line_feed(),
            // A stray sequence introducer / terminator carries no cell.
            '\u{1b}' | '\u{7}' => {}
            _ => self.put_char(ch),
        }
    }

    fn try_interpret_pending(&mut self) -> bool {
        let seq = std::mem::take(&mut self.pending_escape);
        if self.interpret(&seq) {
            return true;
        }
        self.pending_escape = seq;
        false
    }

    /// Interpret a complete escape sequence; false when more input is
    /// needed.
    fn interpret(&mut self, seq: &str) -> bool {
        // OSC (title): ESC ] ... BEL
        if let Some(body) = seq.strip_prefix("\u{1b}]") {
            if body.ends_with('\u{7}') {
                return true; // Title etc. — ignored.
            }
            return false;
        }
        // APC (cursor marker): ESC _ ... BEL — zero width, ignored.
        if let Some(body) = seq.strip_prefix("\u{1b}_") {
            if body.ends_with('\u{7}') {
                return true;
            }
            return false;
        }
        let Some(body) = seq.strip_prefix("\u{1b}[") else {
            // Two-char escapes: ESC followed by one char.
            return seq.chars().count() >= 2;
        };
        // CSI: ends with a byte 0x40-0x7E.
        let Some(final_char) = body.chars().last() else {
            return false;
        };
        if !('\u{40}'..='\u{7e}').contains(&final_char) {
            return false;
        }
        let params = &body[..body.len() - final_char.len_utf8()];
        let params = params.strip_prefix('?').unwrap_or(params);
        let parse_num = |default: usize| -> usize {
            params
                .split(';')
                .next()
                .and_then(|p| p.parse().ok())
                .unwrap_or(default)
        };
        match final_char {
            'H' | 'f' => {
                // CUP: row;col (1-indexed).
                let mut parts = params.split(';');
                let row: usize = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);
                let col: usize = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);
                self.cursor_y = row.saturating_sub(1);
                self.cursor_x = col.saturating_sub(1);
            }
            'A' => {
                let n = parse_num(1).max(1);
                self.cursor_y = self.cursor_y.saturating_sub(n);
            }
            'B' => {
                let n = parse_num(1).max(1);
                self.cursor_y += n;
                self.ensure_rows(self.cursor_y + 1);
            }
            'G' => {
                let n = parse_num(1).max(1);
                self.cursor_x = n - 1;
            }
            'J' => {
                // 2 = clear screen (and home upstream: "ESC[2J" pairs with H).
                if params == "2" {
                    self.clear_viewport();
                } else if params.is_empty() || params == "0" {
                    // Clear from cursor to end.
                    self.clear_from_cursor();
                }
            }
            'K' => {
                // Erase line.
                if params.is_empty() || params == "0" {
                    self.erase_line_from_cursor();
                } else if params == "2" {
                    self.erase_line_full();
                }
            }
            'h' | 'l' => {
                // Mode set/reset: ?2004 (paste), ?25 (cursor), ?2026
                // (synchronized output), ?2031 — all no-ops for the model.
            }
            'u' => {
                // Kitty flags query/setting — no visual effect.
            }
            'c' => {}
            't' => {}
            'n' => {}
            _ => {}
        }
        true
    }

    fn put_char(&mut self, ch: char) {
        self.ensure_rows(self.cursor_y + 1);
        if self.cursor_x < self.columns {
            let row = &mut self.buffer[self.viewport_y + self.cursor_y];
            while row.chars().count() < self.cursor_x {
                row.push(' ');
            }
            let char_count = row.chars().count();
            if self.cursor_x < char_count {
                let byte_index = row
                    .char_indices()
                    .nth(self.cursor_x)
                    .map(|(i, _)| i)
                    .unwrap_or(row.len());
                row.replace_range(byte_index..byte_index + ch.len_utf8(), &ch.to_string());
            } else {
                row.push(ch);
            }
        }
        self.cursor_x += 1;
    }

    fn line_feed(&mut self) {
        self.cursor_y += 1;
        self.ensure_rows(self.cursor_y + 1);
        // Scrolling when the cursor passes the viewport bottom.
        if self.cursor_y >= self.rows {
            let scroll = self.cursor_y - self.rows + 1;
            for _ in 0..scroll {
                self.buffer.push(String::new());
            }
            self.viewport_y += scroll;
            self.cursor_y = self.rows - 1;
        }
    }

    fn ensure_rows(&mut self, count: usize) {
        while self.viewport_y + count > self.buffer.len() {
            self.buffer.push(String::new());
        }
    }

    fn clear_viewport(&mut self) {
        for row in self.viewport_y..self.viewport_y + self.rows {
            if let Some(line) = self.buffer.get_mut(row) {
                line.clear();
            }
        }
    }

    fn clear_from_cursor(&mut self) {
        self.erase_line_from_cursor();
        for row in (self.viewport_y + self.cursor_y + 1)..(self.viewport_y + self.rows) {
            if let Some(line) = self.buffer.get_mut(row) {
                line.clear();
            }
        }
    }

    fn erase_line_from_cursor(&mut self) {
        let index = self.viewport_y + self.cursor_y;
        if let Some(line) = self.buffer.get_mut(index) {
            let keep: String = line.chars().take(self.cursor_x).collect();
            *line = keep;
        }
    }

    fn erase_line_full(&mut self) {
        let index = self.viewport_y + self.cursor_y;
        if let Some(line) = self.buffer.get_mut(index) {
            line.clear();
        }
    }

    fn write_plain(&mut self, text: &str) {
        for ch in text.chars() {
            self.put_plain_char(ch);
        }
    }

    /// The visible viewport lines (upstream `getViewport`).
    pub fn get_viewport(&self) -> Vec<String> {
        (0..self.rows)
            .map(|row| {
                self.buffer
                    .get(self.viewport_y + row)
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }

    /// The entire buffer including scrollback (upstream
    /// `getScrollBuffer`).
    pub fn get_scroll_buffer(&self) -> Vec<String> {
        self.buffer.clone()
    }

    /// Cursor position (upstream `getCursorPosition`).
    pub fn get_cursor_position(&self) -> (usize, usize) {
        (self.cursor_x, self.cursor_y)
    }

    /// Hardware cursor row in buffer coordinates (upstream the
    /// renderer's hardwareCursorRow tracking).
    pub fn cursor_row(&self) -> usize {
        self.viewport_y + self.cursor_y
    }

    pub fn clear(&mut self) {
        for row in self.viewport_y..self.viewport_y + self.rows {
            if let Some(line) = self.buffer.get_mut(row) {
                line.clear();
            }
        }
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
        for _ in 0..self.rows {
            self.buffer.push(String::new());
        }
        self.viewport_y = 0;
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    pub fn kitty_protocol_active(&self) -> bool {
        self.kitty_protocol_active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string_refs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_text_lands_in_viewport() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("hello");
        assert_eq!(vt.get_viewport()[0], "hello");
    }

    #[test]
    fn crlf_moves_to_next_line_start() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("one\r\ntwo");
        let viewport = vt.get_viewport();
        assert_eq!(viewport[0], "one");
        assert_eq!(viewport[1], "two");
        assert_eq!(vt.get_cursor_position(), (3, 1));
    }

    #[test]
    fn cup_positions_absolutely() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("\u{1b}[5;3HX");
        // Cursor advanced past the written char.
        assert_eq!(vt.get_cursor_position(), (3, 4));
        assert_eq!(vt.get_viewport()[4].trim(), "X");
    }

    #[test]
    fn cursor_up_down_move() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("\u{1b}[1;1H\r\n\r\n\u{1b}[2A");
        assert_eq!(vt.get_cursor_position(), (0, 0));
        vt.write("\u{1b}[3B");
        assert_eq!(vt.get_cursor_position(), (0, 3));
    }

    #[test]
    fn column_absolute() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("\u{1b}[1;1H\u{1b}[10Gabc");
        assert_eq!(vt.get_viewport()[0].trim(), "abc");
    }

    #[test]
    fn erase_line_full_then_write() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("hello world");
        vt.write("\r\u{1b}[2Kgoodbye");
        assert_eq!(vt.get_viewport()[0], "goodbye");
    }

    #[test]
    fn erase_line_from_cursor_keeps_prefix() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("hello world");
        vt.write("\u{1b}[6G\u{1b}[K");
        assert_eq!(vt.get_viewport()[0], "hello");
    }

    #[test]
    fn clear_screen_empties_viewport() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("some content\r\nmore");
        vt.write("\u{1b}[2J");
        assert!(vt.get_viewport().iter().all(|l| l.is_empty()));
    }

    #[test]
    fn cursor_marker_is_zero_width() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("ab\u{1b}_pi:c\u{7}c");
        // The APC marker is ignored; text reads "abc".
        assert_eq!(vt.get_viewport()[0], "abc");
    }

    #[test]
    fn osc_title_ignored() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("\u{1b}]0;my title\u{7}content");
        assert_eq!(vt.get_viewport()[0], "content");
    }

    #[test]
    fn long_osc_title_is_ignored() {
        // The renderer's title embeds the session id and is routinely longer
        // than the short escape cap; it must still be consumed as an OSC, not
        // flushed as text (this used to recurse and overflow the stack).
        let mut vt = VirtualTerminal::new(80, 24);
        let title = "x".repeat(200);
        vt.write(&format!("\u{1b}]0;{title}\u{7}content"));
        assert_eq!(vt.get_viewport()[0], "content");
    }

    #[test]
    fn long_apc_marker_is_ignored() {
        let mut vt = VirtualTerminal::new(80, 24);
        let marker = "y".repeat(200);
        vt.write(&format!("\u{1b}_{marker}\u{7}content"));
        assert_eq!(vt.get_viewport()[0], "content");
    }

    #[test]
    fn oversized_escape_flushes_as_text_without_recursing() {
        // A CSI that never terminates (a digit run) exceeds the short cap: the
        // parser gives up, renders it as text and keeps going. Before the fix
        // the give-up path re-entered the parser and recursed until the stack
        // overflowed.
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("\u{1b}[");
        vt.write(&"1".repeat(100));
        // The malformed run was flushed as text (the stray ESC is dropped).
        assert!(
            vt.get_viewport()[0].starts_with("[111"),
            "{:?}",
            vt.get_viewport()[0]
        );
        // Parsing resumed: a normal escape still works afterwards.
        vt.write("\r\u{1b}[2Kafter");
        assert_eq!(vt.get_viewport()[0], "after");
    }

    #[test]
    fn synchronized_output_markers_are_inert() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("\u{1b}[?2026hrendered\u{1b}[?2026l");
        assert_eq!(vt.get_viewport()[0], "rendered");
    }

    #[test]
    fn bracketed_paste_and_cursor_markers_inert() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("\u{1b}[?2004h\u{1b}[?25ltext\u{1b}[?25h\u{1b}[?2004l");
        assert_eq!(vt.get_viewport()[0], "text");
    }

    #[test]
    fn viewport_scrolls_on_overflow() {
        let mut vt = VirtualTerminal::new(10, 3);
        for i in 0..5 {
            vt.write(&format!("line{i}\r\n"));
        }
        let viewport = vt.get_viewport();
        // After the final LF the cursor sits on a fresh blank line, so
        // the viewport shows the last two content rows plus the blank.
        assert_eq!(viewport, string_refs(&["line3", "line4", ""]));
        // Scrollback retains the earlier lines.
        let scroll = vt.get_scroll_buffer();
        assert!(scroll.len() >= 5);
        assert_eq!(scroll[0], "line0");
    }

    #[test]
    fn wide_chars_occur_two_cells_in_content() {
        // The model stores raw content; width accounting is by chars.
        let mut vt = VirtualTerminal::new(80, 24);
        vt.write("日本");
        assert_eq!(vt.get_viewport()[0], "日本");
    }

    #[test]
    fn row_writes_past_width_truncated() {
        let mut vt = VirtualTerminal::new(5, 1);
        vt.write("abcdefghij");
        assert_eq!(vt.get_viewport()[0], "abcde");
    }

    #[test]
    fn cursor_row_tracks_buffer_position() {
        let mut vt = VirtualTerminal::new(10, 2);
        for i in 0..4 {
            vt.write(&format!("l{i}\r\n"));
        }
        // After 4 line feeds the cursor is at buffer row 4.
        assert_eq!(vt.cursor_row(), 4);
    }

    #[test]
    fn send_input_reaches_handler() {
        let mut vt = VirtualTerminal::new(80, 24);
        let received: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let sink = received.clone();
        vt.set_input_handler(Box::new(move |data| {
            sink.lock().unwrap().push(data.to_string());
        }));
        vt.send_input("\u{1b}[A");
        assert_eq!(received.lock().unwrap().as_slice(), ["\u{1b}[A"]);
    }

    #[test]
    fn reset_restores_blank_state() {
        let mut vt = VirtualTerminal::new(10, 3);
        vt.write("junk\r\njunk");
        vt.reset();
        assert!(vt.get_viewport().iter().all(|l| l.is_empty()));
        assert_eq!(vt.get_cursor_position(), (0, 0));
    }
}
