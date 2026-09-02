//! Port of packages/coding-agent/src/core/tools/path-utils.ts (pi v0.84.3):
//! path expansion/resolution for tool inputs, including the macOS filename
//! variant fallbacks (AM/PM narrow spaces, NFD, curly quotes).

use std::path::{Path, PathBuf};

/// Expand `~` and normalize unicode-space artifacts in user-typed paths
/// (upstream `expandPath` with `normalizeUnicodeSpaces`/`stripAtPrefix`).
pub fn expand_path(file_path: &str) -> PathBuf {
    PathBuf::from(strip_at_prefix(&normalize_unicode_spaces(&expand_tilde(
        file_path,
    ))))
}

/// Resolve a path relative to the given cwd; handles `~` expansion and
/// absolute paths (upstream `resolveToCwd`).
pub fn resolve_to_cwd(file_path: &str, cwd: &str) -> PathBuf {
    let expanded = strip_at_prefix(&normalize_unicode_spaces(&expand_tilde(file_path)));
    resolve_with_base(&expanded, cwd)
}

/// Resolve a read path with macOS variant fallbacks (upstream
/// `resolveReadPath`).
pub fn resolve_read_path(file_path: &str, cwd: &str) -> PathBuf {
    let resolved = resolve_to_cwd(file_path, cwd);

    if resolved.exists() {
        return resolved;
    }

    // macOS AM/PM narrow no-break space variant.
    let am_pm_variant = try_macos_screenshot_path(&resolved);
    if am_pm_variant != resolved && am_pm_variant.exists() {
        return am_pm_variant;
    }

    // NFD variant (macOS stores filenames in decomposed form).
    let nfd_variant = try_nfd_variant(&resolved);
    if nfd_variant != resolved && nfd_variant.exists() {
        return nfd_variant;
    }

    // Curly-quote variant (macOS screenshot names use U+2019).
    let curly_variant = try_curly_quote_variant(&resolved);
    if curly_variant != resolved && curly_variant.exists() {
        return curly_variant;
    }

    // Combined NFD + curly quotes (French macOS screenshots).
    let nfd_curly_variant = try_curly_quote_variant(&nfd_variant);
    if nfd_curly_variant != resolved && nfd_curly_variant.exists() {
        return nfd_curly_variant;
    }

    resolved
}

fn expand_tilde(file_path: &str) -> String {
    if file_path == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| file_path.to_string());
    }
    if let Some(rest) = file_path.strip_prefix("~/") {
        return std::env::var("HOME")
            .map(|home| format!("{home}/{rest}"))
            .unwrap_or_else(|_| file_path.to_string());
    }
    file_path.to_string()
}

fn strip_at_prefix(file_path: &str) -> String {
    // Editor-diff prefixes like "@path" are stripped (upstream stripAtPrefix).
    file_path.strip_prefix('@').unwrap_or(file_path).to_string()
}

fn normalize_unicode_spaces(file_path: &str) -> String {
    file_path.replace(['\u{00a0}', '\u{202f}'], " ")
}

fn resolve_with_base(file_path: &str, cwd: &str) -> PathBuf {
    let path = Path::new(file_path);
    if path.is_absolute() {
        return clean_path(path);
    }
    clean_path(&Path::new(cwd).join(path))
}

/// Lexically clean a path: resolve `.` and `..` without touching the
/// filesystem (upstream resolvePath normalization).
fn clean_path(path: &Path) -> PathBuf {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir => {}
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    let mut cleaned = PathBuf::new();
    for part in parts {
        cleaned.push(part);
    }
    cleaned
}

const NARROW_NO_BREAK_SPACE: char = '\u{202f}';

fn try_macos_screenshot_path(file_path: &Path) -> PathBuf {
    // " 10.30.00 AM." -> narrow no-break space before AM/PM.
    let text = file_path.to_string_lossy();
    let mut out = String::with_capacity(text.len());
    let lower = text.to_lowercase();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Match " AM." / " PM." case-insensitively.
        if bytes[i] == b' '
            && i + 4 <= bytes.len()
            && (lower[i + 1..].starts_with("am.") || lower[i + 1..].starts_with("pm."))
        {
            out.push(NARROW_NO_BREAK_SPACE);
            out.push_str(&text[i + 1..i + 4]);
            i += 4;
            continue;
        }
        let ch_len = utf8_len(bytes[i]);
        let end = (i + ch_len).min(bytes.len());
        out.push_str(&text[i..end]);
        i = end;
    }
    PathBuf::from(out)
}

fn try_nfd_variant(file_path: &Path) -> PathBuf {
    // Compatibility normalization: decompose accented characters.
    PathBuf::from(decompose_utf8(&file_path.to_string_lossy()))
}

fn try_curly_quote_variant(file_path: &Path) -> PathBuf {
    PathBuf::from(file_path.to_string_lossy().replace('\'', "\u{2019}"))
}

fn utf8_len(first_byte: u8) -> usize {
    match first_byte {
        b if b < 0x80 => 1,
        b if b >> 5 == 0b110 => 2,
        b if b >> 4 == 0b1110 => 3,
        _ => 4,
    }
}

/// NFD-style decomposition for the Latin-1/accent range (sufficient for the
/// macOS filename variants this targets); other characters pass through.
fn decompose_utf8(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for ch in input.chars() {
        if let Some((base, combining)) = decompose_char(ch) {
            out.push(base);
            out.push(combining);
        } else {
            out.push(ch);
        }
    }
    out
}

fn decompose_char(ch: char) -> Option<(char, char)> {
    let combining: char = match ch {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => '\u{0301}',
        'è' | 'é' | 'ê' | 'ë' => '\u{0301}',
        'ì' | 'í' | 'î' | 'ï' => '\u{0301}',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' => '\u{0301}',
        'ù' | 'ú' | 'û' | 'ü' => '\u{0301}',
        'ç' => '\u{0327}',
        'ñ' => '\u{0303}',
        _ => return None,
    };
    let base = match ch {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        _ => return None,
    };
    Some((base, combining))
}
