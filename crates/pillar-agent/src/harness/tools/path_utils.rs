//! Port of packages/agent/src/harness/tools/path-utils.ts (pi v0.84.3).

use crate::harness::types::{ExecutionEnv, FileError};

const NARROW_NO_BREAK_SPACE: char = '\u{202F}';

fn replace_unicode_spaces(path: &str) -> String {
    // Upstream regex covers \u00A0, \u2000-\u200A, \u202F, \u205F, \u3000.
    path.chars()
        .map(|c| {
            if matches!(c, '\u{00A0}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
                || ('\u{2000}'..='\u{200A}').contains(&c)
            {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Upstream `normalizeToolPath`: normalize Unicode spaces and strip a
/// leading `@`.
pub fn normalize_tool_path(path: &str) -> String {
    let normalized = replace_unicode_spaces(path);
    normalized
        .strip_prefix('@')
        .map(str::to_owned)
        .unwrap_or(normalized)
}

/// Upstream `resolveToolPath`.
pub async fn resolve_tool_path<E: ExecutionEnv + ?Sized>(
    env: &E,
    path: &str,
) -> Result<String, FileError> {
    env.absolute_path(&normalize_tool_path(path)).await
}

/// Upstream `resolveReadToolPath`: try fuzzy variants for ASR-mangled paths
/// (narrow no-break space before AM/PM, NFD forms, curly quotes).
pub async fn resolve_read_tool_path<E: ExecutionEnv + ?Sized>(
    env: &E,
    path: &str,
) -> Result<String, FileError> {
    let resolved = resolve_tool_path(env, path).await?;
    let am_pm_replaced = replace_am_pm(&resolved);
    let variants = [
        resolved.clone(),
        am_pm_replaced,
        nfd(&resolved),
        replace_apostrophes(&resolved),
        replace_apostrophes(&nfd(&resolved)),
    ];
    let mut seen: Vec<&str> = Vec::new();
    for variant in &variants {
        if seen.contains(&variant.as_str()) {
            continue;
        }
        seen.push(variant);
        if env.exists(variant).await? {
            return Ok(variant.clone());
        }
    }
    Ok(resolved)
}

/// Upstream `/ (AM|PM)\./gi` replacement inserting a narrow no-break space
/// before a case-insensitive "am." / "pm.".
fn replace_am_pm(path: &str) -> String {
    let lower = path.to_lowercase();
    let mut output = String::with_capacity(path.len());
    let mut index = 0;
    while index < path.len() {
        if lower[index..].starts_with("am.") || lower[index..].starts_with("pm.") {
            output.push_str(&path[..index]);
            output.push(NARROW_NO_BREAK_SPACE);
            output.push_str(&path[index..index + 2]);
            output.push('.');
            index += 3;
            continue;
        }
        let ch = path[index..].chars().next().expect("non-empty suffix");
        output.push(ch);
        index += ch.len_utf8();
    }
    output
}

fn replace_apostrophes(path: &str) -> String {
    path.replace('\'', "\u{2019}")
}

/// Minimal NFD-equivalent decomposition for the Latin range (upstream
/// `String.prototype.normalize("NFD")`). The port decomposes the accented
/// Latin letters the fuzzy variants target; full NFD is unnecessary for
/// the existence-probe fallback chain.
fn nfd(path: &str) -> String {
    path.chars()
        .flat_map(|c| match c {
            'à'..='ö' | 'ø'..='þ' => {
                let base = c as u32;
                // Decompose into ASCII letter + combining grave/acute/etc. by
                // lookup over the common Latin-1 letters.
                match c {
                    'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => vec!['a', combining_for(base)],
                    'è' | 'é' | 'ê' | 'ë' => vec!['e', combining_for(base)],
                    'ì' | 'í' | 'î' | 'ï' => vec!['i', combining_for(base)],
                    'ò' | 'ó' | 'ô' | 'õ' | 'ö' => vec!['o', combining_for(base)],
                    'ù' | 'ú' | 'û' | 'ü' => vec!['u', combining_for(base)],
                    'ý' | 'ÿ' => vec!['y', combining_for(base)],
                    'ñ' => vec!['n', '\u{0303}'],
                    'ç' => vec!['c', '\u{0327}'],
                    _ => vec![c],
                }
            }
            c => vec![c],
        })
        .collect()
}

fn combining_for(code: u32) -> char {
    match code {
        0xE0 | 0xE8 | 0xEC | 0xF2 | 0xF9 => '\u{0300}', // grave
        0xE1 | 0xE9 | 0xED | 0xF3 | 0xFA | 0xFD => '\u{0301}', // acute
        0xE2 | 0xEA | 0xEE | 0xF4 | 0xFB => '\u{0302}', // circumflex
        0xE3 | 0xF5 => '\u{0303}',                      // tilde
        _ => '\u{0308}',                                // diaeresis
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_unicode_spaces_and_strips_at() {
        assert_eq!(normalize_tool_path("\u{00A0}doc.md"), " doc.md");
        assert_eq!(normalize_tool_path("a\u{2003}b"), "a b");
        assert_eq!(normalize_tool_path("a\u{3000}b"), "a b");
        assert_eq!(normalize_tool_path("@file"), "file");
        assert_eq!(normalize_tool_path("plain"), "plain");
    }
}
