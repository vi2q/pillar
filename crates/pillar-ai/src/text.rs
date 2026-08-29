//! Port of packages/ai/src/utils/text.ts (pi v0.84.3).

use crate::types::Content;

/// Removes unpaired Unicode surrogate characters from a string.
///
/// Unpaired surrogates (high surrogates 0xD800-0xDBFF without matching low
/// surrogates, or vice versa) cause JSON serialization errors in many API
/// providers; valid emoji and other non-BMP characters use properly paired
/// surrogates and are unaffected.
///
/// divergence: Rust `String` values are UTF-8 and cannot contain unpaired
/// surrogates by construction, so this is an identity function. It exists so
/// provider ports call it at the same points as upstream (grep-parity).
pub fn sanitize_surrogates(text: &str) -> String {
    text.to_string()
}

/// Extract and join text from message content.
pub fn content_text(content: &[Content], separator: &str) -> String {
    content
        .iter()
        .filter_map(|block| block.as_text())
        .collect::<Vec<_>>()
        .join(separator)
}
