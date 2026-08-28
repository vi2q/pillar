//! Port of packages/ai/src/utils/text.ts (pi v0.84.3).

use crate::types::Content;

/// Extract and join text from message content.
pub fn content_text(content: &[Content], separator: &str) -> String {
    content
        .iter()
        .filter_map(|block| block.as_text())
        .collect::<Vec<_>>()
        .join(separator)
}
