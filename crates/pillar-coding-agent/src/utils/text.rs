//! Port of packages/coding-agent/src/utils/text.ts (pi v0.84.3).

/// Split a leading UTF-8 BOM off the content (upstream `splitBom`).
pub fn split_bom(content: &str) -> (&str, &str) {
    match content.strip_prefix('\u{feff}') {
        Some(text) => ("\u{feff}", text),
        None => ("", content),
    }
}

/// Strip a leading UTF-8 BOM (upstream `stripBom`).
pub fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}
