//! Port of packages/tui/src/alt-screen-search.ts (pi v0.84.3), the
//! search-match core: building a position-mapped search corpus from
//! screen lines (terminal sequences stripped, whitespace runs collapsed
//! to single separators), case-insensitive query matching, and segment
//! coalescing back to row/column spans.
//!
//! divergences: the port covers the pure match functions plus the
//! `AltScreenSearchComponent` overlay (single-line `Input` + result status
//! line); keybinding dispatch into the input is explicit
//! (`input::dispatch_input_keybinding`). Grapheme segmentation maps to char
//! iteration (the corpus is width-mapped the same way).

use crate::input::Input;
use crate::text_utils::{truncate_to_width, visible_width};
use crate::tui::{Component, Focusable};

/// A mapped source span (upstream `SearchSourceSpan` / segment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchSegment {
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
}

/// A search match spanning one or more contiguous segments (upstream
/// `AltScreenSearchMatch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltScreenSearchMatch {
    pub segments: Vec<SearchSegment>,
}

/// Strip ANSI terminal escape sequences (CSI / OSC / simple ESC pairs).
pub fn strip_terminal_sequences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == '\u{1b}' {
            // ESC [ ... final byte (CSI) or ESC ] ... BEL/ST (OSC) or 2-char escapes.
            if i + 1 < bytes.len() {
                match bytes[i + 1] {
                    '[' => {
                        i += 2;
                        while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
                            i += 1;
                        }
                        i += 1; // consume final byte
                        continue;
                    }
                    ']' => {
                        i += 2;
                        while i < bytes.len() {
                            if bytes[i] == '\u{7}' {
                                i += 1;
                                break;
                            }
                            if bytes[i] == '\u{1b}' && i + 1 < bytes.len() && bytes[i + 1] == '\\' {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                        continue;
                    }
                    _ => {
                        i += 2;
                        continue;
                    }
                }
            } else {
                break;
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Corpus width stand-in for grapheme width (single-cell approximation).
fn corpus_width(text: &str) -> usize {
    text.chars().count()
}

fn is_whitespace_text(text: &str) -> bool {
    !text.is_empty() && text.chars().all(char::is_whitespace)
}

/// Build the search corpus with per-character source spans (upstream
/// `buildSearchCorpus`): terminal sequences stripped; whitespace runs
/// become single separators that carry no span; line boundaries act like
/// whitespace.
fn build_search_corpus(lines: &[&str]) -> (String, Vec<Option<SearchSegment>>) {
    let mut corpus_text = String::new();
    let mut source: Vec<Option<SearchSegment>> = Vec::new();
    let mut pending_separator = false;

    for (row, line) in lines.iter().enumerate() {
        let stripped = strip_terminal_sequences(line);
        let mut column = 0usize;
        for grapheme in stripped.split("").filter(|s| !s.is_empty()) {
            // char-level iteration stands in for grapheme segmentation.
            let text = grapheme.to_string();
            let width = corpus_width(&text);
            if is_whitespace_text(&text) {
                if !corpus_text.is_empty() {
                    pending_separator = true;
                }
                column += width;
                continue;
            }
            if pending_separator {
                corpus_text.push(' ');
                source.push(None);
                pending_separator = false;
            }
            let span = SearchSegment {
                row,
                start_col: column,
                end_col: column + width,
            };
            for _ in 0..width {
                corpus_text.push_str(&text);
                source.push(Some(span));
            }
            column += width;
        }
        if !corpus_text.is_empty() {
            pending_separator = true;
        }
    }

    (corpus_text, source)
}

/// Normalize a query: whitespace runs collapse to single spaces, then
/// trim (upstream `normalizeQuery`).
pub fn normalize_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Find all matches of the query across screen lines (upstream
/// `findAltScreenSearchMatches`): case-insensitive, over the
/// position-mapped corpus, coalescing matched characters back into
/// contiguous row/col segments.
pub fn find_alt_screen_search_matches(lines: &[&str], query: &str) -> Vec<AltScreenSearchMatch> {
    let normalized = normalize_query(query);
    if normalized.is_empty() {
        return Vec::new();
    }
    let (corpus_text, source) = build_search_corpus(lines);

    let needle = normalized.to_lowercase();
    let haystack = corpus_text.to_lowercase();
    let mut matches = Vec::new();
    let mut search_from = 0usize;
    while let Some(relative) = haystack[search_from..].find(&needle) {
        let start = search_from + relative;
        let end = start + needle.len();
        let mut segments: Vec<SearchSegment> = Vec::new();
        for index in start..end {
            let Some(span) = source.get(index).copied().flatten() else {
                continue;
            };
            match segments.last_mut() {
                Some(previous)
                    if previous.row == span.row && span.start_col <= previous.end_col =>
                {
                    previous.end_col = previous.end_col.max(span.end_col);
                }
                _ => segments.push(span),
            }
        }
        if !segments.is_empty() {
            matches.push(AltScreenSearchMatch { segments });
        }
        search_from = end;
        if needle.is_empty() {
            break;
        }
    }
    matches
}

/// Stable match identity (upstream `getAltScreenSearchMatchKey`):
/// `firstRow:firstStartCol:lastRow:lastEndCol`.
pub fn get_alt_screen_search_match_key(matched: &AltScreenSearchMatch) -> String {
    match (matched.segments.first(), matched.segments.last()) {
        (Some(first), Some(last)) => {
            format!(
                "{}:{}:{}:{}",
                first.row, first.start_col, last.row, last.end_col
            )
        }
        _ => String::new(),
    }
}

/// The transcript-search overlay (upstream `AltScreenSearchComponent`): a
/// single-line input plus a reversed status line showing the match count.
pub struct AltScreenSearchComponent {
    input: Input,
    result_count: usize,
    result_index: i64,
    focused: bool,
}

impl AltScreenSearchComponent {
    pub fn new() -> Self {
        Self {
            input: Input::new(),
            result_count: 0,
            result_index: -1,
            focused: false,
        }
    }

    /// The current query (upstream `input.getValue()`).
    pub fn query(&self) -> &str {
        self.input.get_value()
    }

    pub fn input(&self) -> &Input {
        &self.input
    }

    pub fn input_mut(&mut self) -> &mut Input {
        &mut self.input
    }

    /// Report the selected match index and the match count (upstream
    /// `setResult`).
    pub fn set_result(&mut self, index: i64, count: usize) {
        self.result_index = index;
        self.result_count = count;
    }

    pub fn result_index(&self) -> i64 {
        self.result_index
    }

    pub fn result_count(&self) -> usize {
        self.result_count
    }
}

impl Default for AltScreenSearchComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for AltScreenSearchComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let safe_width = width.max(1);
        let label = " Find transcript";
        let query = self.input.get_value();
        let status = if query.is_empty() {
            String::new()
        } else if self.result_count == 0 {
            "No matches ".to_string()
        } else {
            format!("{}/{} ", self.result_index + 1, self.result_count)
        };
        let label_width = visible_width(label);
        let status_width = visible_width(&status);
        let gap = " ".repeat(safe_width.saturating_sub(label_width + status_width).max(1));
        let title = truncate_to_width(&format!("{label}{gap}{status}"), safe_width, "", false);
        let padding = " ".repeat(safe_width.saturating_sub(visible_width(&title)));
        let mut lines = vec![format!("\u{1b}[7m{title}{padding}\u{1b}[27m")];
        lines.extend(self.input.render(safe_width));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        if crate::input::dispatch_input_keybinding(&mut self.input, data) {
            return;
        }
        self.input.handle_input(data);
    }

    fn invalidate(&mut self) {}

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

impl Focusable for AltScreenSearchComponent {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}
