//! Port of components/diff.ts: render a `generateDiffString` diff with colours
//! and word-level highlighting of modified lines.
//!
//! divergence: upstream computes the word diff with jsdiff's `diffWords`; the
//! port uses the `similar` crate's word diff, which also groups whitespace with
//! adjacent words but does not ignore whitespace-only changes.

use crate::modes::interactive::theme::theme;
use similar::{ChangeTag, TextDiff};

/// Options (upstream `RenderDiffOptions`; `filePath` is kept for API
/// compatibility and unused).
#[derive(Debug, Clone, Default)]
pub struct RenderDiffOptions {
    pub file_path: Option<String>,
}

struct ParsedDiffLine {
    prefix: char,
    line_num: String,
    content: String,
}

/// Split `"+123 content"` / `"-123 content"` / `" 123 content"` (upstream
/// `parseDiffLine`).
fn parse_diff_line(line: &str) -> Option<ParsedDiffLine> {
    let mut chars = line.chars();
    let prefix = chars.next()?;
    if prefix != '+' && prefix != '-' && !prefix.is_whitespace() {
        return None;
    }
    let rest = chars.as_str();
    let digits_len = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
    let line_num: String = rest.chars().take(digits_len).collect();
    let after = rest[digits_len..].chars();
    // The upstream regex requires a whitespace separator after the number.
    let mut after = after;
    match after.next() {
        Some(separator) if separator.is_whitespace() => {}
        _ => return None,
    }
    Some(ParsedDiffLine {
        prefix,
        line_num,
        content: after.collect(),
    })
}

/// Tabs render inconsistently across terminals (upstream `replaceTabs`).
fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

/// Word-level diff with inverse video on changed runs; leading whitespace is
/// kept out of the highlight (upstream `renderIntraLineDiff`).
fn render_intra_line_diff(old_content: &str, new_content: &str) -> (String, String) {
    let diff = TextDiff::from_words(old_content, new_content);
    let mut removed_line = String::new();
    let mut added_line = String::new();
    let mut is_first_removed = true;
    let mut is_first_added = true;
    let theme = theme();

    for change in diff.iter_all_changes() {
        let value = change.value();
        match change.tag() {
            ChangeTag::Delete => {
                let mut value = value.to_string();
                if is_first_removed {
                    let leading = value.len() - value.trim_start().len();
                    let leading_ws = value[..leading].to_string();
                    value = value[leading..].to_string();
                    removed_line.push_str(&leading_ws);
                    is_first_removed = false;
                }
                if !value.is_empty() {
                    removed_line.push_str(&theme.inverse(&value));
                }
            }
            ChangeTag::Insert => {
                let mut value = value.to_string();
                if is_first_added {
                    let leading = value.len() - value.trim_start().len();
                    let leading_ws = value[..leading].to_string();
                    value = value[leading..].to_string();
                    added_line.push_str(&leading_ws);
                    is_first_added = false;
                }
                if !value.is_empty() {
                    added_line.push_str(&theme.inverse(&value));
                }
            }
            ChangeTag::Equal => {
                removed_line.push_str(value);
                added_line.push_str(value);
            }
        }
    }

    (removed_line, added_line)
}

/// Render a diff: context lines dim, removals red, additions green, with
/// inverse video on the changed tokens of a single-line modification
/// (upstream `renderDiff`).
pub fn render_diff(diff_text: &str, _options: RenderDiffOptions) -> String {
    let lines: Vec<&str> = diff_text.split('\n').collect();
    let theme_handle = theme();
    let mut result: Vec<String> = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];
        let Some(parsed) = parse_diff_line(line) else {
            result.push(theme_handle.fg("toolDiffContext", line));
            index += 1;
            continue;
        };

        if parsed.prefix == '-' {
            let mut removed: Vec<(String, String)> = Vec::new();
            while index < lines.len() {
                match parse_diff_line(lines[index]) {
                    Some(next) if next.prefix == '-' => {
                        removed.push((next.line_num, next.content));
                        index += 1;
                    }
                    _ => break,
                }
            }
            let mut added: Vec<(String, String)> = Vec::new();
            while index < lines.len() {
                match parse_diff_line(lines[index]) {
                    Some(next) if next.prefix == '+' => {
                        added.push((next.line_num, next.content));
                        index += 1;
                    }
                    _ => break,
                }
            }

            if removed.len() == 1 && added.len() == 1 {
                let (removed_line, added_line) = render_intra_line_diff(
                    &replace_tabs(&removed[0].1),
                    &replace_tabs(&added[0].1),
                );
                result.push(theme_handle.fg(
                    "toolDiffRemoved",
                    &format!("-{} {}", removed[0].0, removed_line),
                ));
                result.push(theme_handle.fg(
                    "toolDiffAdded",
                    &format!("+{} {}", added[0].0, added_line),
                ));
            } else {
                for (line_num, content) in &removed {
                    result.push(theme_handle.fg(
                        "toolDiffRemoved",
                        &format!("-{line_num} {}", replace_tabs(content)),
                    ));
                }
                for (line_num, content) in &added {
                    result.push(theme_handle.fg(
                        "toolDiffAdded",
                        &format!("+{line_num} {}", replace_tabs(content)),
                    ));
                }
            }
        } else if parsed.prefix == '+' {
            result.push(theme_handle.fg(
                "toolDiffAdded",
                &format!("+{} {}", parsed.line_num, replace_tabs(&parsed.content)),
            ));
            index += 1;
        } else {
            result.push(theme_handle.fg(
                "toolDiffContext",
                &format!(" {} {}", parsed.line_num, replace_tabs(&parsed.content)),
            ));
            index += 1;
        }
    }

    result.join("\n")
}
