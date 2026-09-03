//! Port of packages/coding-agent/src/core/tools/edit-diff.ts (pi v0.84.3):
//! line-ending handling, fuzzy matching normalization, multi-edit
//! application, unified patches, and the display diff string.
//!
//! divergence: `generateUnifiedPatch` builds the header manually over a
//! longest-common-subsequence line diff (via the `similar` crate) instead
//! of the jsdiff `createTwoFilesPatch` output; both produce a standard
//! unified patch with the same context rules.

use similar::{ChangeTag, TextDiff};

/// An exact-text replacement (upstream `Edit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

/// Detect the dominant line ending (upstream `detectLineEnding`).
pub fn detect_line_ending(content: &str) -> &'static str {
    let crlf_idx = content.find("\r\n");
    let lf_idx = content.find('\n');
    let Some(lf_idx) = lf_idx else {
        return "\n";
    };
    let Some(crlf_idx) = crlf_idx else {
        return "\n";
    };
    if crlf_idx < lf_idx { "\r\n" } else { "\n" }
}

/// Normalize CRLF and lone CR to LF (upstream `normalizeToLF`).
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Restore LF to the given ending (upstream `restoreLineEndings`).
pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// Normalize text for fuzzy matching (upstream `normalizeForFuzzyMatch`):
/// NFKC, per-line trailing whitespace stripped, smart quotes to ASCII,
/// Unicode dashes to `-`, special spaces to regular space.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc = unicode_normalization_nfkc(text);
    nfkc.split('\n')
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .replace(['\u{2018}', '\u{2019}', '\u{201a}', '\u{201b}'], "'")
        .replace(['\u{201c}', '\u{201d}', '\u{201e}', '\u{201f}'], "\"")
        .replace(
            [
                '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}',
            ],
            "-",
        )
        .replace(
            [
                '\u{00a0}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}',
                '\u{2008}', '\u{2009}', '\u{200a}', '\u{202f}', '\u{205f}', '\u{3000}',
            ],
            " ",
        )
}

/// NFKC normalization without pulling in a full unicode crate: the port
/// maps the character classes the fuzzy matcher actually encounters
/// (fullwidth forms, common compatibility glyphs). Characters outside the
/// mapped classes pass through unchanged.
fn unicode_normalization_nfkc(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            // Fullwidth ASCII variants (U+FF01-U+FF5E) -> ASCII.
            c @ '\u{ff01}'..='\u{ff5e}' => char::from_u32(0x21 + (c as u32 - 0xff01)).unwrap_or(c),
            // Fullwidth space -> space.
            '\u{3000}' => ' ',
            _ => ch,
        })
        .collect()
}

fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = content.as_bytes();
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            lines.push(&content[start..=i]);
            start = i + 1;
        }
    }
    if start < content.len() {
        lines.push(&content[start..]);
    }
    lines
}

struct LineSpan {
    start: usize,
    end: usize,
}

fn get_line_spans(content: &str) -> Vec<LineSpan> {
    let mut offset = 0usize;
    split_lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = LineSpan {
                start: offset,
                end: offset + line.len(),
            };
            offset = span.end;
            span
        })
        .collect()
}

/// Fuzzy match result (upstream `FuzzyMatchResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatchResult {
    pub found: bool,
    pub index: usize,
    pub match_length: usize,
    pub used_fuzzy_match: bool,
    /// Content to use for replacement operations: original content on an
    /// exact match, fuzzy-normalized content on a fuzzy match.
    pub content_for_replacement: String,
}

/// Find oldText in content, exact first, then fuzzy (upstream
/// `fuzzyFindText`).
pub fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    if let Some(exact_index) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index: exact_index,
            match_length: old_text.len(),
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        };
    }

    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    match fuzzy_content.find(&fuzzy_old_text) {
        Some(fuzzy_index) => FuzzyMatchResult {
            found: true,
            index: fuzzy_index,
            match_length: fuzzy_old_text.len(),
            used_fuzzy_match: true,
            content_for_replacement: fuzzy_content,
        },
        None => FuzzyMatchResult {
            found: false,
            index: 0,
            match_length: 0,
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        },
    }
}

fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    fuzzy_content.matches(&fuzzy_old_text).count()
}

fn get_not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        return format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        );
    }
    format!(
        "Could not find edits[{edit_index}] in {path}. The oldText must match exactly including all whitespace and newlines."
    )
}

fn get_duplicate_error(
    path: &str,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        return format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        );
    }
    format!(
        "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
    )
}

fn get_empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        return format!("oldText must not be empty in {path}.");
    }
    format!("edits[{edit_index}].oldText must not be empty in {path}.")
}

fn get_no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        return format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        );
    }
    format!("No changes made to {path}. The replacements produced identical content.")
}

struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

struct TextReplacement {
    match_index: usize,
    match_length: usize,
    new_text: String,
}

fn apply_replacements(content: &str, replacements: &[TextReplacement], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let match_index = replacement.match_index - offset;
        result = format!(
            "{}{}{}",
            &result[..match_index],
            replacement.new_text,
            &result[match_index + replacement.match_length..]
        );
    }
    result
}

/// The result of applying edits (upstream `AppliedEditsResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEditsResult {
    pub base_content: String,
    pub new_content: String,
}

/// Apply replacements matched against `baseContent` to `originalContent`
/// while preserving unchanged line blocks (upstream
/// `applyReplacementsPreservingUnchangedLines`).
fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[TextReplacement],
) -> Result<String, String> {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .to_string(),
        );
    }

    let mut groups: Vec<(usize, usize, Vec<&TextReplacement>)> = Vec::new();
    let mut sorted_replacements: Vec<&TextReplacement> = replacements.iter().collect();
    sorted_replacements.sort_by_key(|r| r.match_index);
    for replacement in &sorted_replacements {
        let range = get_replacement_line_range(&base_lines, replacement)?;
        match groups.last_mut() {
            Some(current) if range.0 < current.1 => {
                current.1 = current.1.max(range.1);
                current.2.push(replacement);
            }
            _ => groups.push((range.0, range.1, vec![replacement])),
        }
    }

    let mut original_line_index = 0usize;
    let mut result = String::new();
    for (start_line, end_line, group_replacements) in &groups {
        result.push_str(&original_lines[original_line_index..*start_line].join(""));

        let group_start_offset = base_lines[*start_line].start;
        let group_end_offset = base_lines[end_line - 1].end;
        let group_refs: Vec<TextReplacement> = group_replacements
            .iter()
            .map(|r| TextReplacement {
                match_index: r.match_index,
                match_length: r.match_length,
                new_text: r.new_text.clone(),
            })
            .collect();
        result.push_str(&apply_replacements(
            &base_content[group_start_offset..group_end_offset],
            &group_refs,
            group_start_offset,
        ));
        original_line_index = *end_line;
    }
    result.push_str(&original_lines[original_line_index..].join(""));

    Ok(result)
}

fn get_replacement_line_range(
    lines: &[LineSpan],
    replacement: &TextReplacement,
) -> Result<(usize, usize), String> {
    let replacement_start = replacement.match_index;
    let replacement_end = replacement.match_index + replacement.match_length;

    let mut start_line = None;
    for (i, line) in lines.iter().enumerate() {
        if replacement_start >= line.start && replacement_start < line.end {
            start_line = Some(i);
            break;
        }
    }
    let Some(start_line) = start_line else {
        return Err("Replacement range is outside the base content.".to_string());
    };

    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < replacement_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        return Err("Replacement range is outside the base content.".to_string());
    }

    Ok((start_line, end_line + 1))
}

/// Apply one or more exact-text replacements to LF-normalized content
/// (upstream `applyEditsToNormalizedContent`): all edits match the same
/// original content; replacements apply in reverse order so offsets stay
/// stable; fuzzy matches overlay line-level changes onto the original so
/// unchanged line blocks keep their original bytes.
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEditsResult, String> {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();

    for (i, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(get_empty_old_text_error(path, i, normalized_edits.len()));
        }
    }

    let used_fuzzy_match = normalized_edits
        .iter()
        .any(|edit| fuzzy_find_text(normalized_content, &edit.old_text).used_fuzzy_match);
    let replacement_base_content = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched_edits: Vec<MatchedEdit> = Vec::new();
    for (i, edit) in normalized_edits.iter().enumerate() {
        let match_result = fuzzy_find_text(&replacement_base_content, &edit.old_text);
        if !match_result.found {
            return Err(get_not_found_error(path, i, normalized_edits.len()));
        }

        let occurrences = count_occurrences(&replacement_base_content, &edit.old_text);
        if occurrences > 1 {
            return Err(get_duplicate_error(
                path,
                i,
                normalized_edits.len(),
                occurrences,
            ));
        }

        matched_edits.push(MatchedEdit {
            edit_index: i,
            match_index: match_result.index,
            match_length: match_result.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched_edits.sort_by_key(|m| m.match_index);
    for i in 1..matched_edits.len() {
        let previous = &matched_edits[i - 1];
        let current = &matched_edits[i];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let base_content = normalized_content.to_string();
    let replacement_refs: Vec<TextReplacement> = matched_edits
        .iter()
        .map(|m| TextReplacement {
            match_index: m.match_index,
            match_length: m.match_length,
            new_text: m.new_text.clone(),
        })
        .collect();
    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            &replacement_base_content,
            &replacement_refs,
        )?
    } else {
        apply_replacements(&replacement_base_content, &replacement_refs, 0)
    };

    if base_content == new_content {
        return Err(get_no_change_error(path, normalized_edits.len()));
    }

    Ok(AppliedEditsResult {
        base_content,
        new_content,
    })
}

// ============================================================================
// Diff generation
// ============================================================================

/// Generate a standard unified patch (upstream `generateUnifiedPatch` via
/// jsdiff createTwoFilesPatch, context 4).
pub fn generate_unified_patch(path: &str, old_content: &str, new_content: &str) -> String {
    let diff = TextDiff::from_lines(old_content, new_content);
    let mut patch = String::new();
    patch.push_str(&format!("--- {path}\n"));
    patch.push_str(&format!("+++ {path}\n"));
    patch.push_str(&diff.unified_diff().context_radius(4).to_string());
    patch
}

/// One diff hunk line with its kind (added/removed/context) and raw text.
enum DiffPartKind {
    Added,
    Removed,
    Context,
}

struct DiffPart {
    kind: DiffPartKind,
    lines: Vec<String>,
}

/// Split a diff into parts grouped by tag (upstream Diff.diffLines parts).
fn diff_line_parts(old_content: &str, new_content: &str) -> Vec<DiffPart> {
    let diff = TextDiff::from_lines(old_content, new_content);
    let mut parts: Vec<DiffPart> = Vec::new();
    for change in diff.iter_all_changes() {
        let (kind, text) = match change.tag() {
            ChangeTag::Delete => (DiffPartKind::Removed, change.value()),
            ChangeTag::Insert => (DiffPartKind::Added, change.value()),
            ChangeTag::Equal => (DiffPartKind::Context, change.value()),
        };
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        // jsdiff part values keep the trailing newline; drop the trailing
        // empty segment the same way.
        if let Some(last) = lines.last() {
            if last.is_empty() {
                lines.pop();
            }
        }
        match parts.last_mut() {
            Some(part) if std::mem::discriminant(&part.kind) == std::mem::discriminant(&kind) => {
                part.lines.extend(lines);
            }
            _ => parts.push(DiffPart { kind, lines }),
        }
    }
    parts
}

/// Generate a display-oriented diff string with line numbers and context
/// (upstream `generateDiffString`).
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> (String, Option<usize>) {
    let parts = diff_line_parts(old_content, new_content);
    let mut output: Vec<String> = Vec::new();

    let old_lines = old_content.split('\n').count();
    let new_lines = new_content.split('\n').count();
    let line_num_width = old_lines.max(new_lines).to_string().len();

    let mut old_line_num = 1usize;
    let mut new_line_num = 1usize;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    for (i, part) in parts.iter().enumerate() {
        let raw = &part.lines;
        match part.kind {
            DiffPartKind::Added | DiffPartKind::Removed => {
                if first_changed_line.is_none() {
                    first_changed_line = Some(new_line_num);
                }
                for line in raw {
                    if matches!(part.kind, DiffPartKind::Added) {
                        let line_num = format!("{new_line_num:>width$}", width = line_num_width);
                        output.push(format!("+{line_num} {line}"));
                        new_line_num += 1;
                    } else {
                        let line_num = format!("{old_line_num:>width$}", width = line_num_width);
                        output.push(format!("-{line_num} {line}"));
                        old_line_num += 1;
                    }
                }
                last_was_change = true;
            }
            DiffPartKind::Context => {
                let next_part_is_change = i < parts.len() - 1
                    && matches!(
                        parts[i + 1].kind,
                        DiffPartKind::Added | DiffPartKind::Removed
                    );
                let has_leading_change = last_was_change;
                let has_trailing_change = next_part_is_change;

                fn push_context(
                    line: &str,
                    output: &mut Vec<String>,
                    old_line_num: usize,
                    line_num_width: usize,
                ) {
                    let line_num = format!("{old_line_num:>width$}", width = line_num_width);
                    output.push(format!(" {line_num} {line}"));
                }

                if has_leading_change && has_trailing_change {
                    if raw.len() <= context_lines * 2 {
                        for line in raw {
                            push_context(line, &mut output, old_line_num, line_num_width);
                            old_line_num += 1;
                            new_line_num += 1;
                        }
                    } else {
                        let leading = &raw[..context_lines];
                        let trailing = &raw[raw.len() - context_lines..];
                        let skipped = raw.len() - leading.len() - trailing.len();

                        for line in leading {
                            push_context(line, &mut output, old_line_num, line_num_width);
                            old_line_num += 1;
                            new_line_num += 1;
                        }
                        output.push(format!(" {:>width$} ...", "", width = line_num_width));
                        old_line_num += skipped;
                        new_line_num += skipped;

                        for line in trailing {
                            push_context(line, &mut output, old_line_num, line_num_width);
                            old_line_num += 1;
                            new_line_num += 1;
                        }
                    }
                } else if has_leading_change {
                    let shown = &raw[..raw.len().min(context_lines)];
                    let skipped = raw.len() - shown.len();
                    for line in shown {
                        push_context(line, &mut output, old_line_num, line_num_width);
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                    if skipped > 0 {
                        output.push(format!(" {:>width$} ...", "", width = line_num_width));
                        old_line_num += skipped;
                        new_line_num += skipped;
                    }
                } else if has_trailing_change {
                    let skipped = raw.len().saturating_sub(context_lines);
                    if skipped > 0 {
                        output.push(format!(" {:>width$} ...", "", width = line_num_width));
                        old_line_num += skipped;
                        new_line_num += skipped;
                    }
                    for line in &raw[skipped..] {
                        push_context(line, &mut output, old_line_num, line_num_width);
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    // Skip these context lines entirely.
                    old_line_num += raw.len();
                    new_line_num += raw.len();
                }
                last_was_change = false;
            }
        }
    }

    (output.join("\n"), first_changed_line)
}

/// Preview diff result (upstream `EditDiffResult`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditDiffResult {
    pub diff: String,
    pub first_changed_line: Option<usize>,
}

/// Compute the diff for one or more edit operations without applying them
/// (upstream `computeEditsDiff`).
pub fn compute_edits_diff(path: &str, edits: &[Edit], cwd: &str) -> Result<EditDiffResult, String> {
    let absolute_path = crate::core::tools::path_utils::resolve_to_cwd(path, cwd);

    if !absolute_path.is_file() {
        return Err(format!("Could not edit file: {path}. Error code: ENOENT."));
    }
    let raw_content = std::fs::read_to_string(&absolute_path).map_err(|e| e.to_string())?;

    // Strip BOM before matching (LLM won't include an invisible BOM).
    let content = raw_content.strip_prefix('\u{feff}').unwrap_or(&raw_content);
    let normalized_content = normalize_to_lf(content);
    let applied = apply_edits_to_normalized_content(&normalized_content, edits, path)?;
    let (diff, first_changed_line) =
        generate_diff_string(&applied.base_content, &applied.new_content, 4);
    Ok(EditDiffResult {
        diff,
        first_changed_line,
    })
}
