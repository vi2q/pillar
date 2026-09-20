//! Temporary reproduction: the bounded render writer slices a &str by byte
//! offset that is not a UTF-8 char boundary when a multi-byte character
//! straddles the chunk limit.

use pillar_tui::editor_autocomplete::BoundedTerminalWriter;

const MAX_RENDER_WRITE_CHARS: usize = 1024 * 1024;

#[test]
fn multibyte_char_at_chunk_boundary() {
    // Fill the buffer so the next append must cut across the limit, then
    // append text whose multi-byte chars straddle that boundary.
    let mut chunks: Vec<String> = Vec::new();
    {
        let mut writer = BoundedTerminalWriter::new(|data| chunks.push(data.to_string()));
        // One byte short of the boundary, then a run of 3-byte CJK chars.
        writer.append(&"a".repeat(MAX_RENDER_WRITE_CHARS - 1));
        writer.append("\u{5909}\u{66f4}\u{304c}\u{7121}\u{3044}");
        writer.flush();
    }
    let joined: String = chunks.concat();
    assert!(
        joined.contains("\u{5909}\u{66f4}\u{304c}\u{7121}\u{3044}"),
        "text must survive chunking intact; got {} chunks",
        chunks.len()
    );
    for chunk in &chunks {
        assert!(chunk.is_char_boundary(chunk.len()), "chunk splits a char");
    }
}
