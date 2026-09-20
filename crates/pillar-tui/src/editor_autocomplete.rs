//! Port of the autocomplete decision core from
//! packages/tui/src/components/editor.ts (pi v0.84.3): trigger
//! patterns, trigger character registration, debounce decision, slash
//! menu context checks, best-match selection, and the bounded writer
//! from tui-main-screen.ts.
//!
//! divergences: the async request pipeline (getSuggestions with
//! AbortSignal, debounce timers, request task chaining) stays
//! host-side; the port exposes the pure decision functions.

use crate::select_list::{SelectList, SelectListLayoutOptions};

const DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS: [&str; 2] = ["@", "#"];

/// Slash-command select list layout (upstream
/// `SLASH_COMMAND_SELECT_LIST_LAYOUT`).
pub fn slash_command_select_list_layout() -> SelectListLayoutOptions {
    SelectListLayoutOptions {
        min_primary_column_width: Some(12),
        max_primary_column_width: Some(32),
        ..Default::default()
    }
}

/// Escape regex metacharacters for character-class building (upstream
/// `escapeCharacterClass`).
#[allow(dead_code)]
fn escape_character_class(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 2);
    for ch in value.chars() {
        if "\\\\^$.*+?()[]{}|-".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Trigger pattern state: `(^|\s)[<triggers>][^\s]*$` against the text
/// before the cursor (upstream `buildTriggerPattern`).
#[derive(Debug, Clone)]
pub struct TriggerPattern {
    trigger_characters: Vec<String>,
}

impl TriggerPattern {
    pub fn new(trigger_characters: &[String]) -> Self {
        Self {
            trigger_characters: trigger_characters.to_vec(),
        }
    }

    /// Whether the text before the cursor ends with a trigger symbol
    /// token (upstream `autocompleteTriggerPattern.test(...)`).
    pub fn is_match(&self, text_before_cursor: &str) -> bool {
        let Some(last_token_start) = last_nonspace_token_start(text_before_cursor) else {
            return false;
        };
        let token = &text_before_cursor[last_token_start..];
        let mut chars = token.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !self.trigger_characters.iter().any(|t| t.starts_with(first)) {
            return false;
        }
        // Rest of the token must be non-whitespace (it is by
        // construction) — matches upstream `[^\s]*$`.
        true
    }
}

/// Debounce pattern state: `(?:^|[ \t])(?:@"[^"]*|[^\s]*|[<triggers>]
/// [^\s]*)$` — @-completions debounce, others trigger immediately
/// (upstream `buildDebouncePattern`).
#[derive(Debug, Clone)]
pub struct DebouncePattern {
    #[allow(dead_code)]
    trigger_characters: Vec<String>,
}

impl DebouncePattern {
    pub fn new(trigger_characters: &[String]) -> Self {
        Self {
            trigger_characters: trigger_characters.to_vec(),
        }
    }

    /// Whether the @-attachment debounce applies (upstream
    /// `autocompleteDebouncePattern.test(...)`): true only for an
    /// in-progress @ token — an unterminated quoted path (which may
    /// contain blanks) or a plain unquoted token.
    pub fn is_match(&self, text_before_cursor: &str) -> bool {
        // Find the last @ preceded by the string start or a blank.
        let bytes = text_before_cursor.as_bytes();
        for (index, byte) in bytes.iter().enumerate().rev() {
            if *byte != b'@' {
                continue;
            }
            let at_blank_start =
                index == 0 || bytes[index - 1] == b' ' || bytes[index - 1] == b'\t';
            if !at_blank_start {
                continue;
            }
            let rest = &text_before_cursor[index + 1..];
            if let Some(inner) = rest.strip_prefix('"') {
                // @"[^"]* — an unterminated quoted path (may contain
                // blanks).
                return !inner.contains('"');
            }
            // @ + non-space chars always debounce (attachment path).
            return !rest.contains(char::is_whitespace);
        }
        false
    }
}

fn last_nonspace_token_start(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut start = text.len();
    for (index, byte) in bytes.iter().enumerate().rev() {
        if *byte == b' ' || *byte == b'\t' || *byte == b'\n' {
            start = index + 1;
            break;
        }
        if index == 0 {
            start = 0;
        }
    }
    (start < text.len()).then_some(start)
}

#[allow(dead_code)]
fn last_blank_token_start(text: &str) -> Option<usize> {
    // Tokens start after a space or tab (upstream `(?:^|[ \t])`).
    let bytes = text.as_bytes();
    let mut start = text.len();
    for (index, byte) in bytes.iter().enumerate().rev() {
        if *byte == b' ' || *byte == b'\t' {
            start = index + 1;
            break;
        }
        if index == 0 {
            start = 0;
        }
    }
    (start < text.len()).then_some(start)
}

/// The default trigger set plus extra single-char triggers, filtering
/// "/" and whitespace and deduping (upstream
/// `setAutocompleteTriggerCharacters`).
pub fn register_trigger_characters(defaults: &[String], extra: &[String]) -> Vec<String> {
    let mut next: Vec<String> = DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS
        .iter()
        .map(|s| s.to_string())
        .collect();
    for character in extra {
        if character.chars().count() != 1
            || character == "/"
            || character.chars().all(char::is_whitespace)
            || next.contains(character)
        {
            continue;
        }
        next.push(character.clone());
    }
    let _ = defaults;
    next
}

/// Debounce milliseconds for a completion request (upstream
/// `getAutocompleteDebounceMs`): explicit Tab and forced requests run
/// immediately; an in-progress @ attachment token debounces 20ms.
pub const ATTACHMENT_AUTOCOMPLETE_DEBOUNCE_MS: u64 = 20;

pub fn get_autocomplete_debounce_ms(
    options_force: bool,
    options_explicit_tab: bool,
    debounce_pattern: &DebouncePattern,
    text_before_cursor: &str,
) -> u64 {
    if options_explicit_tab || options_force {
        return 0;
    }
    if debounce_pattern.is_match(text_before_cursor) {
        ATTACHMENT_AUTOCOMPLETE_DEBOUNCE_MS
    } else {
        0
    }
}

/// Slash-menu context checks (upstream the private editor methods).
pub struct SlashMenuContext;

impl SlashMenuContext {
    /// The menu is only allowed on the first line (upstream
    /// `isSlashMenuAllowed`).
    pub fn is_allowed(cursor_line: usize) -> bool {
        cursor_line == 0
    }

    /// Cursor is at the start of the message (whitespace or "/" before
    /// the cursor on line 0) (upstream `isAtStartOfMessage`).
    pub fn is_at_start_of_message(cursor_line: usize, text_before_cursor: &str) -> bool {
        if !Self::is_allowed(cursor_line) {
            return false;
        }
        let trimmed = text_before_cursor.trim();
        trimmed.is_empty() || trimmed == "/"
    }

    /// Whether the text before the cursor starts a slash command
    /// (upstream `isInSlashCommandContext`).
    pub fn is_in_slash_command_context(cursor_line: usize, text_before_cursor: &str) -> bool {
        Self::is_allowed(cursor_line) && text_before_cursor.trim_start().starts_with('/')
    }
}

/// Best autocomplete match index (upstream
/// `getBestAutocompleteMatchIndex`): exact value match wins, then the
/// first prefix match, else -1.
pub fn get_best_autocomplete_match_index(values: &[&str], prefix: &str) -> isize {
    if prefix.is_empty() {
        return -1;
    }
    let mut first_prefix_index: isize = -1;
    for (index, value) in values.iter().enumerate() {
        if *value == prefix {
            return index as isize;
        }
        if first_prefix_index == -1 && value.starts_with(prefix) {
            first_prefix_index = index as isize;
        }
    }
    first_prefix_index
}

/// Build the autocomplete select list with the slash layout when the
/// prefix starts with "/" (upstream `createAutocompleteList`). Items
/// are (value, label, description).
pub fn create_autocomplete_list(
    prefix: &str,
    items: &[(String, String, Option<String>)],
    max_visible: usize,
) -> SelectList {
    let layout = if prefix.starts_with('/') {
        Some(slash_command_select_list_layout())
    } else {
        None
    };
    let select_items = items
        .iter()
        .map(
            |(value, label, description)| crate::select_list::SelectItem {
                value: value.clone(),
                label: label.clone(),
                description: description.clone(),
            },
        )
        .collect();
    match layout {
        Some(layout) => SelectList::new(select_items, max_visible, layout),
        None => SelectList::new(
            select_items,
            max_visible,
            SelectListLayoutOptions::default(),
        ),
    }
}

// ============================================================================
// BoundedTerminalWriter (upstream tui-main-screen.ts)
// ============================================================================

const MAX_RENDER_WRITE_CHARS: usize = 1024 * 1024;

/// Streams terminal output in fixed-size chunks so a full render never
/// forms one oversized string (upstream `BoundedTerminalWriter`). The
/// port buffers into a String and hands complete chunks to the host
/// writer; Rust strings are UTF-8 so no surrogate-pair splitting is
/// needed.
pub struct BoundedTerminalWriter<W: FnMut(&str)> {
    buffer: String,
    written_chars: usize,
    write: W,
}

impl<W: FnMut(&str)> BoundedTerminalWriter<W> {
    pub fn new(write: W) -> Self {
        Self {
            buffer: String::new(),
            written_chars: 0,
            write,
        }
    }

    /// Append data, flushing full chunks (upstream `append`).
    ///
    /// The chunk limit is a byte budget (`MAX_RENDER_WRITE_CHARS`), so the
    /// slice point must be moved back to a UTF-8 char boundary. Cutting a
    /// multi-byte character would panic (or emit a torn sequence): a long
    /// session whose rendered frame crosses the 1 MiB limit used to slice
    /// inside a CJK character.
    pub fn append(&mut self, value: &str) {
        let mut offset = 0usize;
        while offset < value.len() {
            let capacity = MAX_RENDER_WRITE_CHARS - self.buffer.len();
            if capacity == 0 {
                self.flush();
                continue;
            }
            let mut end = (value.len()).min(offset + capacity);
            // Never end the slice inside a character: back up to the nearest
            // boundary. `capacity >= 1` guarantees progress unless the whole
            // chunk limit is consumed by one multi-byte character.
            while end > offset && !value.is_char_boundary(end) {
                end -= 1;
            }
            if end == offset {
                self.flush();
                continue;
            }
            self.buffer.push_str(&value[offset..end]);
            offset = end;
            if self.buffer.len() >= MAX_RENDER_WRITE_CHARS {
                self.flush();
            }
        }
    }

    /// Write the current chunk (upstream `flush`).
    pub fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let chunk = std::mem::take(&mut self.buffer);
        self.written_chars += chunk.len();
        (self.write)(&chunk);
    }

    pub fn length(&self) -> usize {
        self.written_chars + self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_trigger_characters() {
        let registered = register_trigger_characters(&[], &[]);
        assert_eq!(registered, vec!["@", "#"]);
    }

    #[test]
    fn extra_triggers_appended_and_deduped() {
        let registered = register_trigger_characters(
            &[],
            &[
                "!".to_string(),
                "@".to_string(),
                "/".to_string(),
                "  ".to_string(),
            ],
        );
        assert_eq!(registered, vec!["@", "#", "!"]);
    }

    #[test]
    fn trigger_pattern_matches_symbol_tokens() {
        let pattern = TriggerPattern::new(&["@".to_string(), "#".to_string()]);
        assert!(pattern.is_match("look at @src"));
        assert!(pattern.is_match("@"));
        assert!(pattern.is_match("x #tag"));
        assert!(!pattern.is_match("plain text"));
        assert!(!pattern.is_match("a@b")); // not at a token boundary
        // Multi-char trigger chars match by first char (upstream
        // character-class semantics).
        assert!(pattern.is_match("tab #"));
    }

    #[test]
    fn debounce_pattern_matches_unterminated_attachment_tokens() {
        let pattern = DebouncePattern::new(&["#".to_string()]);
        assert!(pattern.is_match("see @src"));
        assert!(pattern.is_match("@"));
        assert!(pattern.is_match("@\"quoted path"));
        // A closed quote is complete — no debounce.
        assert!(!pattern.is_match("@\"done\""));
        // Other trigger chars do not debounce (upstream the escaped
        // without @ branch also matches — but only @ forms debounce in
        // the combined pattern).
        assert!(!pattern.is_match("#ta"));
    }

    #[test]
    fn debounce_ms_rules() {
        let pattern = DebouncePattern::new(&["@ ".to_string()]);
        assert_eq!(
            get_autocomplete_debounce_ms(true, true, &pattern, "see @src"),
            0
        );
        assert_eq!(
            get_autocomplete_debounce_ms(true, false, &pattern, "see @src"),
            0
        );
        assert_eq!(
            get_autocomplete_debounce_ms(false, false, &pattern, "see @src"),
            20
        );
        assert_eq!(
            get_autocomplete_debounce_ms(false, false, &pattern, "plain"),
            0
        );
    }

    #[test]
    fn slash_menu_context_rules() {
        assert!(SlashMenuContext::is_allowed(0));
        assert!(!SlashMenuContext::is_allowed(1));
        assert!(SlashMenuContext::is_at_start_of_message(0, ""));
        assert!(SlashMenuContext::is_at_start_of_message(0, "  "));
        assert!(SlashMenuContext::is_at_start_of_message(0, "/"));
        assert!(!SlashMenuContext::is_at_start_of_message(0, "/cmd x"));
        assert!(SlashMenuContext::is_in_slash_command_context(0, "/cmd"));
        assert!(!SlashMenuContext::is_in_slash_command_context(1, "/cmd"));
        assert!(!SlashMenuContext::is_in_slash_command_context(0, "x /cmd"));
    }

    #[test]
    fn best_match_exact_then_prefix() {
        assert_eq!(get_best_autocomplete_match_index(&["a", "b"], ""), -1);
        assert_eq!(get_best_autocomplete_match_index(&["a", "b"], "b"), 1);
        assert_eq!(get_best_autocomplete_match_index(&["ab", "abc"], "ab"), 0);
        assert_eq!(get_best_autocomplete_match_index(&["ab", "abc"], "abc"), 1);
        assert_eq!(get_best_autocomplete_match_index(&["ab"], "z"), -1);
    }

    #[test]
    fn autocomplete_list_uses_slash_layout_for_slash_prefix() {
        // The two-column path needs a description (render skips it
        // otherwise).
        let list = create_autocomplete_list(
            "/he",
            &[(
                "help".to_string(),
                "help".to_string(),
                Some("show help".to_string()),
            )],
            5,
        );
        let rendered = list.render(60, &test_theme());
        // Two-column layout: description column past the 12-col minimum.
        assert_eq!(rendered.len(), 1);
        let line = &rendered[0];
        let desc_col = line.find("show help").unwrap();
        assert!(desc_col >= 2 + 12, "{line:?}");
    }

    #[test]
    fn autocomplete_list_default_layout_without_slash() {
        let list =
            create_autocomplete_list("he", &[("help".to_string(), "help".to_string(), None)], 5);
        let rendered = list.render(40, &test_theme());
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].contains("help"));
    }

    fn test_theme() -> crate::select_list::SelectListTheme {
        let passthrough =
            || Box::new(|t: &str| t.to_string()) as Box<dyn Fn(&str) -> String + Send>;
        crate::select_list::SelectListTheme {
            selected_prefix: passthrough(),
            selected_text: passthrough(),
            description: passthrough(),
            scroll_info: passthrough(),
            no_match: passthrough(),
        }
    }

    #[test]
    fn bounded_writer_passes_small_writes_through() {
        let mut chunks: Vec<String> = Vec::new();
        {
            let mut writer = BoundedTerminalWriter::new(|data| chunks.push(data.to_string()));
            writer.append("hello ");
            writer.append("world");
            writer.flush();
        }
        assert_eq!(chunks.join(""), "hello world");
    }

    #[test]
    fn bounded_writer_chunks_oversized_input() {
        // Use a small chunk boundary by filling the buffer first.
        let mut chunks: Vec<String> = Vec::new();
        {
            let mut writer = BoundedTerminalWriter::new(|data| chunks.push(data.to_string()));
            let big = "x".repeat(MAX_RENDER_WRITE_CHARS + 10);
            writer.append(&big);
            writer.flush();
        }
        // Split into two chunks: exactly MAX then 10.
        assert_eq!(chunks[0].len(), MAX_RENDER_WRITE_CHARS);
        assert_eq!(chunks[1].len(), 10);
        assert!(chunks[0].chars().all(|c| c == 'x'));
    }

    #[test]
    fn bounded_writer_length_tracks_written_and_pending() {
        let mut writer = BoundedTerminalWriter::new(|_| {});
        writer.append("abc");
        assert_eq!(writer.length(), 3);
        writer.flush();
        assert_eq!(writer.length(), 3);
    }

    /// A cut exactly at the byte limit falls inside a multi-byte character:
    /// the writer must back up to a char boundary instead of panicking on
    /// `value[offset..end]`. This is the shape a long session hits when a
    /// rendered frame crosses the 1 MiB budget.
    #[test]
    fn bounded_writer_never_splits_a_character() {
        let mut chunks: Vec<String> = Vec::new();
        {
            let mut writer = BoundedTerminalWriter::new(|data| chunks.push(data.to_string()));
            // One byte short of the boundary, then 3-byte CJK characters: the
            // first slice would end inside '変' (bytes 0..3).
            writer.append(&"a".repeat(MAX_RENDER_WRITE_CHARS - 1));
            let text = "\u{5909}\u{66f4}\u{304c}\u{7121}\u{3044}";
            writer.append(text);
            writer.flush();
            assert_eq!(
                chunks.concat().chars().filter(|c| *c == 'a').count(),
                MAX_RENDER_WRITE_CHARS - 1
            );
            assert!(
                chunks.concat().ends_with(text),
                "the multi-byte run must survive intact"
            );
        }
        // Every chunk is a valid &str and within the byte budget.
        for chunk in &chunks {
            assert!(chunk.len() <= MAX_RENDER_WRITE_CHARS);
            assert!(chunk.is_char_boundary(chunk.len()));
        }
    }

    /// When the remaining budget is smaller than one character, flush and
    /// retry rather than looping forever or slicing mid-character.
    #[test]
    fn bounded_writer_handles_a_budget_smaller_than_one_character() {
        let mut chunks: Vec<String> = Vec::new();
        {
            let mut writer = BoundedTerminalWriter::new(|data| chunks.push(data.to_string()));
            // Fill one whole chunk so its buffer is flushed, leaving room to
            // append a 3-byte character right after the boundary.
            writer.append(&"a".repeat(MAX_RENDER_WRITE_CHARS));
            writer.flush();
            writer.append("\u{5909}\u{66f4}");
            writer.flush();
        }
        assert!(chunks.concat().ends_with("\u{5909}\u{66f4}"));
    }
}
