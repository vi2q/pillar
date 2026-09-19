//! Position encodings: bytes, lines/columns and LSP positions are three
//! different coordinate systems and must not be conflated (design §5.2).
//!
//! rustc reports a byte span (0-based, half-open) and 1-based display
//! line/column. LSP reports a 0-based line and a 0-based character in the
//! encoding the client negotiated — `utf-16` by default. A UTF-8 byte offset,
//! a rustc column and an LSP character are equal only for ASCII; for Japanese,
//! emoji (a surrogate pair in UTF-16) and combining marks they differ.
//!
//! [`LineIndex`] is the shared conversion, and its tests are the design's R2
//! fixture: 日本語・絵文字・結合文字・CRLF.

use serde::{Deserialize, Serialize};

/// The encoding LSP positions are measured in (negotiated at initialize).
///
/// LSP defaults to `utf-16`; a client that negotiated `utf-8` or `utf-32`
/// changes what `character` means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionEncoding {
    Utf8,
    /// LSP's default.
    #[default]
    Utf16,
    Utf32,
}

/// A 0-based LSP position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

/// A 1-based display position, as rustc reports it (and as a human reads it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineColumn {
    pub line: u32,
    pub column: u32,
}

/// The line starts of a document, so a byte offset can be mapped without
/// rescanning from the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    /// Build the index. Line 0 starts at 0; each `\n` starts another line, so a
    /// document ending in a newline has a trailing empty line (what editors
    /// show).
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0usize];
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(index + 1);
            }
        }
        Self { starts }
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// The byte offset where `line` (0-based) starts.
    pub fn line_start(&self, line: usize) -> Option<usize> {
        self.starts.get(line).copied()
    }

    fn line_of(&self, byte: usize) -> Option<usize> {
        let position = self.starts.partition_point(|&start| start <= byte);
        position.checked_sub(1)
    }

    /// The byte offset just past a line's content (before a `\n`).
    fn content_end(&self, text: &str, line: usize) -> usize {
        match self.starts.get(line + 1) {
            // `starts[line + 1]` is the byte after `\n`, so this is the `\n`.
            Some(&next) => next.saturating_sub(1).min(text.len()),
            None => text.len(),
        }
    }

    fn position_to_byte(
        &self,
        text: &str,
        line: usize,
        character: u32,
        encoding: PositionEncoding,
    ) -> Option<usize> {
        let start = self.line_start(line)?;
        let end = self.content_end(text, line);
        let slice = text.get(start..end)?;
        let offset = offset_for_units(slice, character as usize, encoding)?;
        Some(start + offset)
    }

    /// A byte offset to an LSP position. `None` for an out-of-range or
    /// non-UTF-8-boundary byte.
    pub fn byte_to_lsp(
        &self,
        text: &str,
        byte: usize,
        encoding: PositionEncoding,
    ) -> Option<LspPosition> {
        if byte > text.len() || !text.is_char_boundary(byte) {
            return None;
        }
        let line = self.line_of(byte)?;
        let start = self.line_start(line)?;
        // A byte on a later line's start maps to that line; a byte past this
        // line's content (its newline) is still on this line for display.
        let character = units(&text[start..byte], encoding);
        Some(LspPosition {
            line: line as u32,
            character: character as u32,
        })
    }

    pub fn lsp_to_byte(
        &self,
        text: &str,
        position: LspPosition,
        encoding: PositionEncoding,
    ) -> Option<usize> {
        self.position_to_byte(text, position.line as usize, position.character, encoding)
    }

    /// A byte offset to a 1-based display line/column in the given encoding.
    pub fn byte_to_line_column(
        &self,
        text: &str,
        byte: usize,
        encoding: PositionEncoding,
    ) -> Option<LineColumn> {
        if byte > text.len() || !text.is_char_boundary(byte) {
            return None;
        }
        let line = self.line_of(byte)?;
        let start = self.line_start(line)?;
        let column = units(&text[start..byte], encoding);
        Some(LineColumn {
            line: (line + 1) as u32,
            column: (column + 1) as u32,
        })
    }

    pub fn line_column_to_byte(
        &self,
        text: &str,
        line_column: LineColumn,
        encoding: PositionEncoding,
    ) -> Option<usize> {
        if line_column.line == 0 || line_column.column == 0 {
            return None;
        }
        self.position_to_byte(
            text,
            (line_column.line - 1) as usize,
            line_column.column - 1,
            encoding,
        )
    }
}

fn units(text: &str, encoding: PositionEncoding) -> usize {
    match encoding {
        PositionEncoding::Utf8 => text.len(),
        PositionEncoding::Utf16 => text.encode_utf16().count(),
        PositionEncoding::Utf32 => text.chars().count(),
    }
}

/// The byte offset of `target` units into `slice`, or `None` if the slice is
/// shorter (a position past the line's end is invalid, not clamped).
fn offset_for_units(slice: &str, target: usize, encoding: PositionEncoding) -> Option<usize> {
    if target == 0 {
        return Some(0);
    }
    let mut seen = 0usize;
    for (index, character) in slice.char_indices() {
        seen += match encoding {
            PositionEncoding::Utf8 => character.len_utf8(),
            PositionEncoding::Utf16 => character.len_utf16(),
            PositionEncoding::Utf32 => 1,
        };
        if seen == target {
            return Some(index + character.len_utf8());
        }
        if seen > target {
            // The target landed inside a code unit (or inside a surrogate
            // pair): not a valid boundary.
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_positions_agree_across_encodings() {
        let text = "let x = 1;\nlet y = 2;\n";
        let index = LineIndex::new(text);
        assert_eq!(index.line_count(), 3);
        let byte = text.find('y').expect("y");
        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            let lsp = index.byte_to_lsp(text, byte, encoding).expect("lsp");
            assert_eq!(
                lsp,
                LspPosition {
                    line: 1,
                    character: 4
                }
            );
            assert_eq!(index.lsp_to_byte(text, lsp, encoding), Some(byte));
        }
        assert_eq!(
            index.byte_to_line_column(text, byte, PositionEncoding::Utf16),
            Some(LineColumn { line: 2, column: 5 })
        );
    }

    #[test]
    fn japanese_columns_are_not_byte_offsets() {
        // "日本語" is 9 UTF-8 bytes, 3 UTF-16 units, 3 chars.
        let text = "日本語x";
        let index = LineIndex::new(text);
        let byte = text.find('x').expect("x");
        assert_eq!(byte, 9);
        assert_eq!(
            index
                .byte_to_lsp(text, byte, PositionEncoding::Utf8)
                .expect("utf8"),
            LspPosition {
                line: 0,
                character: 9
            }
        );
        assert_eq!(
            index
                .byte_to_lsp(text, byte, PositionEncoding::Utf16)
                .expect("utf16"),
            LspPosition {
                line: 0,
                character: 3
            }
        );
        assert_eq!(
            index.lsp_to_byte(
                text,
                LspPosition {
                    line: 0,
                    character: 3
                },
                PositionEncoding::Utf16
            ),
            Some(9)
        );
        // Utf8 encoding accepts a byte not on a char boundary neither.
        assert_eq!(
            index.lsp_to_byte(
                text,
                LspPosition {
                    line: 0,
                    character: 4
                },
                PositionEncoding::Utf8
            ),
            None,
            "a mid-character byte is not a valid position"
        );
    }

    #[test]
    fn an_emoji_is_two_utf16_units() {
        let text = "a😀b";
        let index = LineIndex::new(text);
        let byte_b = text.find('b').expect("b");
        assert_eq!(byte_b, 5); // 'a' + 4-byte emoji
        assert_eq!(
            index
                .byte_to_lsp(text, byte_b, PositionEncoding::Utf16)
                .expect("utf16"),
            LspPosition {
                line: 0,
                character: 3
            }
        );
        assert_eq!(
            index
                .byte_to_lsp(text, byte_b, PositionEncoding::Utf32)
                .expect("utf32"),
            LspPosition {
                line: 0,
                character: 2
            }
        );
        // A position inside the surrogate pair is not a valid boundary.
        assert_eq!(
            index.lsp_to_byte(
                text,
                LspPosition {
                    line: 0,
                    character: 2
                },
                PositionEncoding::Utf16
            ),
            None
        );
        assert_eq!(
            index.lsp_to_byte(
                text,
                LspPosition {
                    line: 0,
                    character: 3
                },
                PositionEncoding::Utf16
            ),
            Some(byte_b)
        );
    }

    #[test]
    fn a_combining_mark_is_a_separate_unit() {
        // "e" + combining acute; two chars, two UTF-16 units, one grapheme.
        let text = "e\u{301}x";
        let index = LineIndex::new(text);
        let byte_x = text.find('x').expect("x");
        assert_eq!(
            index
                .byte_to_lsp(text, byte_x, PositionEncoding::Utf16)
                .expect("utf16"),
            LspPosition {
                line: 0,
                character: 2
            },
            "LSP counts code units, not grapheme clusters"
        );
    }

    #[test]
    fn crlf_keeps_the_carriage_return_in_the_line_content() {
        let text = "one\r\ntwo\r\n";
        let index = LineIndex::new(text);
        assert_eq!(index.line_count(), 3);
        // The second line starts right after the first '\n'.
        assert_eq!(index.line_start(1), Some(5));
        let byte_two = text.find("two").expect("two");
        assert_eq!(
            index
                .byte_to_line_column(text, byte_two, PositionEncoding::Utf8)
                .expect("lc"),
            LineColumn { line: 2, column: 1 }
        );
        // The '\r' is part of line 0's content.
        let byte_cr = text.find('\r').expect("cr");
        assert_eq!(
            index
                .byte_to_lsp(text, byte_cr, PositionEncoding::Utf16)
                .expect("lsp"),
            LspPosition {
                line: 0,
                character: 3
            }
        );
    }

    #[test]
    fn a_bom_is_a_unit_on_the_first_line() {
        let text = "\u{feff}let";
        let index = LineIndex::new(text);
        let byte_l = text.find('l').expect("l");
        assert_eq!(
            index
                .byte_to_lsp(text, byte_l, PositionEncoding::Utf16)
                .expect("lsp"),
            LspPosition {
                line: 0,
                character: 1
            }
        );
    }

    #[test]
    fn a_position_past_the_line_is_invalid_not_clamped() {
        let text = "abc\ndef\n";
        let index = LineIndex::new(text);
        assert_eq!(
            index.lsp_to_byte(
                text,
                LspPosition {
                    line: 0,
                    character: 9
                },
                PositionEncoding::Utf16
            ),
            None
        );
        assert_eq!(
            index.lsp_to_byte(
                text,
                LspPosition {
                    line: 9,
                    character: 0
                },
                PositionEncoding::Utf16
            ),
            None
        );
        // Exactly the end of a line's content is valid.
        assert_eq!(
            index.lsp_to_byte(
                text,
                LspPosition {
                    line: 0,
                    character: 3
                },
                PositionEncoding::Utf16
            ),
            Some(3)
        );
    }

    #[test]
    fn a_round_trip_holds_for_every_char_boundary() {
        let text = "αβ\n日本語😀\r\ne\u{301}z";
        let index = LineIndex::new(text);
        for byte in 0..=text.len() {
            if !text.is_char_boundary(byte) {
                continue;
            }
            let lsp = index
                .byte_to_lsp(text, byte, PositionEncoding::Utf16)
                .expect("lsp");
            assert_eq!(
                index.lsp_to_byte(text, lsp, PositionEncoding::Utf16),
                Some(byte),
                "round trip at byte {byte}"
            );
        }
    }
}
