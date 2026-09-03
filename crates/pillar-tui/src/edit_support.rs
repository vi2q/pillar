//! Ports of small pi v0.84.3 tui modules:
//! - undo-stack.ts (generic clone-on-push undo stack)
//! - kill-ring.ts (Emacs-style kill/yank ring)
//! - word-navigation.ts (word-forward/backward cursor movement)
//!
//! divergence: upstream uses Intl.Segmenter for word segmentation; the
//! port implements an equivalent segmenter over Unicode alphanumeric
//! runs (words), punctuation runs, and whitespace, with ASCII punctuation
//! boundaries preserved inside word-like segments exactly as upstream's
//! PUNCTUATION_REGEX pass does.

// ============================================================================
// undo-stack.ts
// ============================================================================}

/// Generic undo stack with clone-on-push semantics (upstream `UndoStack`):
/// pushed states are deep-cloned; popped snapshots are returned
/// directly since they are already detached.
#[derive(Debug)]
pub struct UndoStack<S> {
    stack: Vec<S>,
}

impl<S> Default for UndoStack<S> {
    fn default() -> Self {
        Self { stack: Vec::new() }
    }
}

impl<S: Clone> UndoStack<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a deep clone of the given state onto the stack (upstream
    /// `push` via structuredClone).
    pub fn push(&mut self, state: &S) {
        // structuredClone semantics. divergence: the port clones with
        // `Clone` instead of a serde round trip; editor state snapshots
        // are value types so the semantics match.
        self.stack.push(state.clone());
    }

    /// Pop and return the most recent snapshot, or None if empty.
    pub fn pop(&mut self) -> Option<S> {
        self.stack.pop()
    }

    /// Remove all snapshots.
    pub fn clear(&mut self) {
        self.stack.clear();
    }

    pub fn len(&self) -> usize {
        self.stack.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

// ============================================================================
// kill-ring.ts
// ============================================================================}

/// Ring buffer for Emacs-style kill/yank operations (upstream
/// `KillRing`): consecutive kills accumulate into one entry; yank reads
/// the most recent entry; yank-pop cycles through older entries.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KillRing {
    ring: Vec<String>,
}

impl KillRing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add killed text (upstream `push`): empty text is ignored; with
    /// `accumulate` the text merges into the most recent entry, prepended
    /// for backward deletion or appended for forward deletion.
    pub fn push(&mut self, text: &str, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        if accumulate && !self.ring.is_empty() {
            let last = self.ring.pop().expect("non-empty");
            self.ring.push(if prepend {
                format!("{text}{last}")
            } else {
                format!("{last}{text}")
            });
        } else {
            self.ring.push(text.to_string());
        }
    }

    /// Most recent entry without modifying the ring (upstream `peek`).
    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(String::as_str)
    }

    /// Move the last entry to the front for yank-pop cycling (upstream
    /// `rotate`).
    pub fn rotate(&mut self) {
        if self.ring.len() > 1 {
            let last = self.ring.pop().expect("non-empty");
            self.ring.insert(0, last);
        }
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

// ============================================================================
// word-navigation.ts
// ============================================================================}

/// Characters treated as punctuation boundaries inside word-like
/// segments (upstream `PUNCTUATION_REGEX`).
const PUNCTUATION_CHARS: [char; 29] = [
    '(', ')', '{', '}', '[', ']', '<', '>', '.', ',', ';', ':', '\'', '"', '!', '?', '+', '-', '=',
    '*', '/', '\\', '|', '&', '%', '^', '$', '#', '@',
];

fn is_punctuation_char(ch: char) -> bool {
    PUNCTUATION_CHARS.contains(&ch) || ch == '~' || ch == '`'
}

fn is_whitespace_char(ch: char) -> bool {
    ch.is_whitespace()
}

/// One segmentation unit (upstream `Intl.SegmentData` subset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordSegment {
    pub text: String,
    pub word_like: bool,
}

/// Segment text into word/punctuation/whitespace runs (upstream
/// `Intl.Segmenter` word granularity). Word-like runs are contiguous
/// alphanumeric characters; punctuation runs group consecutive
/// punctuation characters; whitespace runs group whitespace.
pub fn segment_words(text: &str) -> Vec<WordSegment> {
    let mut segments: Vec<WordSegment> = Vec::new();
    let mut current = String::new();
    let mut current_kind: u8 = 0; // 0 none, 1 word, 2 punct, 3 space

    let kind_of = |ch: char| -> u8 {
        if ch.is_alphanumeric() {
            1
        } else if is_punctuation_char(ch) {
            2
        } else if ch.is_whitespace() {
            3
        } else {
            1 // treat other symbols as part of word runs (segmenter keeps
            // them with surrounding word material)
        }
    };

    for ch in text.chars() {
        let kind = kind_of(ch);
        if kind != current_kind && !current.is_empty() {
            segments.push(WordSegment {
                word_like: current_kind == 1,
                text: std::mem::take(&mut current),
            });
        }
        current_kind = kind;
        current.push(ch);
    }
    if !current.is_empty() {
        segments.push(WordSegment {
            word_like: current_kind == 1,
            text: current,
        });
    }
    segments
}

/// Find the cursor position after moving one word backward (upstream
/// `findWordBackward`): skip trailing whitespace, then stop at the next
/// word/punctuation boundary; atomic segments (e.g. paste markers) are
/// skipped as single units; word-like segments preserve ASCII punctuation
/// boundaries.
pub fn find_word_backward(
    text: &str,
    cursor: usize,
    is_atomic: Option<&dyn Fn(&str) -> bool>,
) -> usize {
    if cursor == 0 || cursor > text.len() {
        return 0;
    }

    let text_before: String = text.chars().take(cursor).collect();
    let mut segments = segment_words(&text_before);
    let mut new_cursor = cursor;

    let last_is_whitespace = |segments: &Vec<WordSegment>| {
        segments
            .last()
            .map(|s| s.text.chars().all(is_whitespace_char))
            .unwrap_or(false)
    };
    let last_is_atomic = |segments: &Vec<WordSegment>| {
        segments
            .last()
            .map(|s| is_atomic.map(|f| f(&s.text)).unwrap_or(false))
            .unwrap_or(false)
    };

    // Skip trailing whitespace.
    while !segments.is_empty() && !last_is_atomic(&segments) && last_is_whitespace(&segments) {
        if let Some(segment) = segments.pop() {
            new_cursor = new_cursor.saturating_sub(segment.text.chars().count());
        }
    }

    if segments.is_empty() {
        return new_cursor;
    }

    let last_kind = segments.last().map(|s| s.word_like).unwrap_or(false);
    if last_is_atomic(&segments) {
        // Skip one atomic segment.
        if let Some(segment) = segments.last() {
            new_cursor = new_cursor.saturating_sub(segment.text.chars().count());
        }
    } else if last_kind {
        // Skip inside one word-like segment, preserving ASCII punctuation
        // boundaries.
        if let Some(segment) = segments.last() {
            let punctuation_positions: Vec<usize> = segment
                .text
                .char_indices()
                .filter(|(_, ch)| is_punctuation_char(*ch))
                .map(|(index, _)| index)
                .collect();
            if punctuation_positions.is_empty() {
                new_cursor = new_cursor.saturating_sub(segment.text.chars().count());
            } else {
                // Upstream keeps the text after the last punctuation match.
                let last_match_end = punctuation_positions
                    .last()
                    .map(|start| {
                        let byte_start = *start;
                        segment.text[byte_start..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1)
                            + byte_start
                    })
                    .unwrap_or(0);
                let chars_after = segment.text[last_match_end..].chars().count();
                new_cursor = new_cursor.saturating_sub(chars_after);
            }
        }
    } else {
        // Skip non-word non-whitespace run (punctuation).
        while let Some(segment) = segments.last() {
            let is_ws = segment.text.chars().all(is_whitespace_char);
            let atomic = is_atomic.map(|f| f(&segment.text)).unwrap_or(false);
            if atomic || segment.word_like || is_ws {
                break;
            }
            new_cursor = new_cursor.saturating_sub(segment.text.chars().count());
            segments.pop();
        }
    }

    new_cursor
}

/// Find the cursor position after moving one word forward (upstream
/// `findWordForward`): skip leading whitespace, then stop at the next
/// word/punctuation boundary.
pub fn find_word_forward(
    text: &str,
    cursor: usize,
    is_atomic: Option<&dyn Fn(&str) -> bool>,
) -> usize {
    let total: usize = text.chars().count();
    if cursor >= total {
        return total;
    }

    let text_after: String = text.chars().skip(cursor).collect();
    let segments = segment_words(&text_after);
    let mut iter = segments.into_iter().peekable();
    let mut new_cursor = cursor;

    let current_is_whitespace =
        |segment: &WordSegment| segment.text.chars().all(is_whitespace_char);
    let current_is_atomic =
        |segment: &WordSegment| is_atomic.map(|f| f(&segment.text)).unwrap_or(false);

    // Skip leading whitespace.
    while let Some(segment) = iter.peek() {
        if current_is_atomic(segment) || !current_is_whitespace(segment) {
            break;
        }
        new_cursor += segment.text.chars().count();
        iter.next();
    }

    let Some(next) = iter.peek() else {
        return new_cursor;
    };

    if current_is_atomic(next) {
        // Skip one atomic segment.
        new_cursor += next.text.chars().count();
    } else if next.word_like {
        // Skip inside one word-like segment, preserving ASCII punctuation
        // boundaries.
        let index = next
            .text
            .char_indices()
            .find(|(_, ch)| is_punctuation_char(*ch))
            .map(|(index, _)| index)
            .unwrap_or(next.text.len());
        new_cursor += next.text[..index].chars().count();
    } else {
        // Skip non-word non-whitespace run (punctuation).
        while let Some(segment) = iter.peek() {
            if current_is_atomic(segment) || segment.word_like || current_is_whitespace(segment) {
                break;
            }
            new_cursor += segment.text.chars().count();
            iter.next();
        }
    }

    new_cursor
}
