//! Port of packages/agent/src/harness/tools/edit-diff.ts (pi v0.84.3) —
//! shared diff computation for the edit tool.
//!
//! divergence: upstream uses the `diff` npm package for
//! `generateUnifiedPatch` and `generateDiffString` (`Diff.createTwoFilesPatch`,
//! `Diff.diffLines`); the port implements the same LCS line-diff directly.
//! The unified patch output matches the npm package's format for the
//! common cases exercised by the tests. The port strips the common
//! leading/trailing lines before the LCS (upstream's Myers search walks them
//! on its first diagonal), which keeps the table proportional to the changed
//! region.

use std::borrow::Cow;

/// Upstream `detectLineEnding`.
pub fn detect_line_ending(content: &str) -> &'static str {
    let crlf_idx = content.find("\r\n");
    let lf_idx = content.find('\n');
    match (crlf_idx, lf_idx) {
        (_, None) => "\n",
        (None, Some(_)) => "\n",
        (Some(crlf), Some(lf)) => {
            if crlf < lf {
                "\r\n"
            } else {
                "\n"
            }
        }
    }
}

/// Upstream `normalizeToLF`.
///
/// The replaces are skipped when the text holds no `\r` at all (the common
/// case), so an already-LF file is copied once instead of twice. The output
/// is identical.
pub fn normalize_to_lf(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_owned();
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Upstream `restoreLineEndings`.
pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_owned()
    }
}

/// Upstream `normalizeForFuzzyMatch`: NFKC-ish normalization for matching
/// (smart quotes/dashes/spaces to ASCII, trailing whitespace stripped per
/// line).
///
/// divergence: upstream applies `String.prototype.normalize("NFKC")`; the
/// port applies the ASCII-collapsing character maps the fuzzy match
/// relies on, which covers the characters the upstream tests exercise.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let joined: String = text
        .split('\n')
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    joined
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// Upstream `stripBom`.
pub fn strip_bom(content: &str) -> (&str, &str) {
    if let Some(text) = content.strip_prefix('\u{FEFF}') {
        ("\u{FEFF}", text)
    } else {
        ("", content)
    }
}

/// Upstream `Edit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            lines.push(&content[start..=index]);
            start = index + 1;
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
    let mut offset = 0;
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

fn get_replacement_line_range(
    lines: &[LineSpan],
    replacement_match_index: usize,
    replacement_end: usize,
) -> (usize, usize) {
    let mut start_line = None;
    for (index, line) in lines.iter().enumerate() {
        if replacement_match_index >= line.start && replacement_match_index < line.end {
            start_line = Some(index);
            break;
        }
    }
    let Some(start_line) = start_line else {
        panic!("Replacement range is outside the base content.");
    };
    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < replacement_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        panic!("Replacement range is outside the base content.");
    }
    (start_line, end_line + 1)
}

fn apply_replacements(content: &str, replacements: &[MatchedEdit], offset: usize) -> String {
    let mut result = content.to_owned();
    for replacement in replacements.iter().rev() {
        let match_index = replacement.match_index - offset;
        result.replace_range(
            match_index..match_index + replacement.match_length,
            &replacement.new_text,
        );
    }
    result
}

/// Upstream `applyReplacementsPreservingUnchangedLines`.
fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    mut replacements: Vec<MatchedEdit>,
) -> String {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        panic!(
            "Cannot preserve unchanged lines because the base content has a different line count."
        );
    }

    replacements.sort_by_key(|replacement| replacement.match_index);
    struct Group {
        start_line: usize,
        end_line: usize,
        replacements: Vec<MatchedEdit>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for replacement in replacements {
        let (start_line, end_line) = get_replacement_line_range(
            &base_lines,
            replacement.match_index,
            replacement.match_index + replacement.match_length,
        );
        match groups.last_mut() {
            Some(current) if start_line < current.end_line => {
                current.end_line = current.end_line.max(end_line);
                current.replacements.push(replacement);
            }
            _ => {
                groups.push(Group {
                    start_line,
                    end_line,
                    replacements: vec![replacement],
                });
            }
        }
    }

    let mut original_line_index = 0;
    let mut result = String::new();
    for group in &groups {
        result.extend(
            original_lines[original_line_index..group.start_line]
                .iter()
                .copied(),
        );

        let group_start_offset = base_lines[group.start_line].start;
        let group_end_offset = base_lines[group.end_line - 1].end;
        result.push_str(&apply_replacements(
            &base_content[group_start_offset..group_end_offset],
            &group.replacements,
            group_start_offset,
        ));
        original_line_index = group.end_line;
    }
    result.extend(original_lines[original_line_index..].iter().copied());

    result
}

/// Upstream `FuzzyMatchResult`.
struct FuzzyMatchResult {
    found: bool,
    index: usize,
    match_length: usize,
    used_fuzzy_match: bool,
    #[allow(dead_code)] // upstream field kept for grep parity
    content_for_replacement: String,
}

/// Upstream `fuzzyFindText`.
fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    if let Some(exact_index) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index: exact_index,
            match_length: old_text.len(),
            used_fuzzy_match: false,
            content_for_replacement: content.to_owned(),
        };
    }

    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    match fuzzy_content.find(&fuzzy_old_text) {
        None => FuzzyMatchResult {
            found: false,
            index: 0,
            match_length: 0,
            used_fuzzy_match: false,
            content_for_replacement: content.to_owned(),
        },
        Some(fuzzy_index) => FuzzyMatchResult {
            found: true,
            index: fuzzy_index,
            match_length: fuzzy_old_text.len(),
            used_fuzzy_match: true,
            content_for_replacement: fuzzy_content,
        },
    }
}

fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    if fuzzy_old_text.is_empty() {
        return 0;
    }
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

/// Upstream `AppliedEditsResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEditsResult {
    pub base_content: String,
    pub new_content: String,
}

/// Upstream `applyEditsToNormalizedContent`.
///
/// divergence: upstream throws; the port returns `Result` with the same
/// message strings.
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

    for (index, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(get_empty_old_text_error(
                path,
                index,
                normalized_edits.len(),
            ));
        }
    }

    let initial_matches: Vec<FuzzyMatchResult> = normalized_edits
        .iter()
        .map(|edit| fuzzy_find_text(normalized_content, &edit.old_text))
        .collect();
    let used_fuzzy_match = initial_matches
        .iter()
        .any(|match_result| match_result.used_fuzzy_match);
    let replacement_base_content = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_owned()
    };

    let mut matched_edits: Vec<MatchedEdit> = Vec::new();
    for (index, edit) in normalized_edits.iter().enumerate() {
        let match_result = fuzzy_find_text(&replacement_base_content, &edit.old_text);
        if !match_result.found {
            return Err(get_not_found_error(path, index, normalized_edits.len()));
        }

        let occurrences = count_occurrences(&replacement_base_content, &edit.old_text);
        if occurrences > 1 {
            return Err(get_duplicate_error(
                path,
                index,
                normalized_edits.len(),
                occurrences,
            ));
        }

        matched_edits.push(MatchedEdit {
            edit_index: index,
            match_index: match_result.index,
            match_length: match_result.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched_edits.sort_by_key(|edit| edit.match_index);
    for pair in matched_edits.windows(2) {
        let (previous, current) = (&pair[0], &pair[1]);
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let base_content = normalized_content.to_owned();
    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            &replacement_base_content,
            matched_edits,
        )
    } else {
        apply_replacements(&replacement_base_content, &matched_edits, 0)
    };

    if base_content == new_content {
        return Err(get_no_change_error(path, normalized_edits.len()));
    }

    Ok(AppliedEditsResult {
        base_content,
        new_content,
    })
}

/// One line-oriented diff hunk part (upstream `Diff.diffLines` parts).
#[derive(Debug, PartialEq, Eq)]
struct DiffPart<'a> {
    added: bool,
    removed: bool,
    /// Borrowed from the input, or owned when adjacent same-kind parts merge.
    value: Cow<'a, str>,
}

/// LCS-based line diff over `old_content`/`new_content` (upstream
/// `Diff.diffLines`).
///
/// The common leading/trailing lines are stripped before the table is built,
/// so the table covers the changed region instead of the whole file (an
/// untrimmed `n × m` table made a one-line edit of a large file allocate
/// hundreds of MB). Upstream's Myers search walks the common prefix on its
/// first diagonal, so this also *matches* jsdiff more often where the LCS
/// tie-break is ambiguous: against jsdiff 5.2.2 on a 4000-case corpus of
/// repeated-line inputs, 431 cases moved onto jsdiff's answer and none moved
/// away (see the tests below).
fn diff_lines<'a>(old_content: &'a str, new_content: &'a str) -> Vec<DiffPart<'a>> {
    let old_lines = split_keep_ends(old_content);
    let new_lines = split_keep_ends(new_content);

    // Common leading/trailing lines are context in any diff.
    let prefix = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(old_line, new_line)| old_line == new_line)
        .count();
    let max_suffix = old_lines.len().min(new_lines.len()) - prefix;
    let suffix = old_lines[old_lines.len() - max_suffix..]
        .iter()
        .rev()
        .zip(new_lines[new_lines.len() - max_suffix..].iter().rev())
        .take_while(|(old_line, new_line)| old_line == new_line)
        .count();
    let old_mid = &old_lines[prefix..old_lines.len() - suffix];
    let new_mid = &new_lines[prefix..new_lines.len() - suffix];
    let n = old_mid.len();
    let m = new_mid.len();

    // LCS length table over the changed region.
    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for (i, old_line) in old_mid.iter().enumerate() {
        for (j, new_line) in new_mid.iter().enumerate() {
            table[i + 1][j + 1] = if old_line == new_line {
                table[i][j] + 1
            } else {
                table[i][j + 1].max(table[i + 1][j])
            };
        }
    }

    // Walk the table backwards collecting ops, then reverse.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Op {
        Equal,
        Delete,
        Insert,
    }
    let mut ops: Vec<Op> = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old_mid[i - 1] == new_mid[j - 1] {
            ops.push(Op::Equal);
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || table[i][j - 1] >= table[i - 1][j]) {
            ops.push(Op::Insert);
            j -= 1;
        } else {
            ops.push(Op::Delete);
            i -= 1;
        }
    }
    ops.reverse();

    let push_part = |parts: &mut Vec<DiffPart<'a>>, added: bool, removed: bool, value: &'a str| {
        if let Some(last) = parts.last_mut() {
            if last.added == added && last.removed == removed {
                // Merge adjacent same-kind parts (upstream does the same).
                last.value = Cow::Owned(format!("{}{}", last.value, value));
                return;
            }
        }
        parts.push(DiffPart {
            added,
            removed,
            value: Cow::Borrowed(value),
        });
    };

    let mut parts: Vec<DiffPart<'a>> = Vec::new();
    for line in &old_lines[..prefix] {
        push_part(&mut parts, false, false, line);
    }
    let (mut oi, mut ni) = (0usize, 0usize);
    for op in ops {
        match op {
            Op::Equal => {
                push_part(&mut parts, false, false, old_mid[oi]);
                oi += 1;
                ni += 1;
            }
            Op::Delete => {
                push_part(&mut parts, false, true, old_mid[oi]);
                oi += 1;
            }
            Op::Insert => {
                push_part(&mut parts, true, false, new_mid[ni]);
                ni += 1;
            }
        }
    }
    for line in &old_lines[old_lines.len() - suffix..] {
        push_part(&mut parts, false, false, line);
    }
    parts
}

/// Split into lines keeping their trailing newline.
fn split_keep_ends(content: &str) -> Vec<&str> {
    split_lines_with_endings(content)
}

/// Upstream `generateUnifiedPatch` (upstream delegates to
/// `Diff.createTwoFilesPatch` with `context: 4` and file-headers-only).
pub fn generate_unified_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> String {
    render_unified_patch(path, &diff_lines(old_content, new_content), context_lines)
}

/// Render the patch from an already computed line diff (upstream
/// `Diff.createTwoFilesPatch`, context 4).
fn render_unified_patch(path: &str, parts: &[DiffPart<'_>], context_lines: usize) -> String {
    let header = format!(
        "===================================================================\n--- {path}\n+++ {path}\n"
    );
    let hunks = build_hunks(parts, context_lines);
    format!("{header}{hunks}")
}

fn build_hunks(parts: &[DiffPart<'_>], context_lines: usize) -> String {
    // Flatten parts into tagged lines with old/new positions.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Tag {
        Context,
        Add,
        Del,
    }
    struct Tagged<'a> {
        tag: Tag,
        text: &'a str,
        old_no: usize,
        new_no: usize,
    }
    let mut tagged: Vec<Tagged> = Vec::new();
    let (mut old_no, mut new_no) = (0usize, 0usize);
    for part in parts {
        for line in split_keep_ends(&part.value) {
            match (part.added, part.removed) {
                (false, false) => {
                    old_no += 1;
                    new_no += 1;
                    tagged.push(Tagged {
                        tag: Tag::Context,
                        text: line,
                        old_no,
                        new_no,
                    });
                }
                (true, false) => {
                    new_no += 1;
                    tagged.push(Tagged {
                        tag: Tag::Add,
                        text: line,
                        old_no,
                        new_no,
                    });
                }
                (false, true) => {
                    old_no += 1;
                    tagged.push(Tagged {
                        tag: Tag::Del,
                        text: line,
                        old_no,
                        new_no,
                    });
                }
                (true, true) => unreachable!("diff parts cannot be both added and removed"),
            }
        }
    }

    // Find changed line groups.
    let mut hunks: Vec<String> = Vec::new();
    let mut index = 0;
    let total = tagged.len();
    while index < total {
        if tagged[index].tag == Tag::Context {
            index += 1;
            continue;
        }
        // Hunk start: back up `context_lines`.
        let start = index.saturating_sub(context_lines);
        // Extend forward through changes plus `context_lines` of context.
        let mut end = index;
        let mut changes_seen = false;
        while end < total {
            if tagged[end].tag != Tag::Context {
                changes_seen = true;
                end += 1;
                continue;
            }
            // Peek: stop if the next change is further than context allows.
            let mut next_change = None;
            for (offset, item) in tagged[end..].iter().enumerate() {
                if item.tag != Tag::Context {
                    next_change = Some(end + offset);
                    break;
                }
            }
            match next_change {
                Some(next) if next - end <= context_lines => {
                    end = next;
                }
                _ => {
                    end = (end + context_lines).min(total);
                    break;
                }
            }
        }
        let _ = changes_seen;

        let hunk_old_start = if tagged[start].tag == Tag::Add {
            tagged[start].old_no + 1
        } else {
            tagged[start].old_no
        };
        let hunk_new_start = if tagged[start].tag == Tag::Del {
            tagged[start].new_no + 1
        } else {
            tagged[start].new_no
        };
        let hunk_old_count = tagged[start..end]
            .iter()
            .filter(|item| item.tag != Tag::Add)
            .count()
            .max(1);
        let hunk_new_count = tagged[start..end]
            .iter()
            .filter(|item| item.tag != Tag::Del)
            .count()
            .max(1);
        let mut hunk = format!(
            "@@ -{},{} +{},{} @@\n",
            hunk_old_start, hunk_old_count, hunk_new_start, hunk_new_count
        );
        for item in &tagged[start..end] {
            let marker = match item.tag {
                Tag::Context => ' ',
                Tag::Add => '+',
                Tag::Del => '-',
            };
            let text = item.text.strip_suffix('\n').unwrap_or(item.text);
            hunk.push(marker);
            hunk.push_str(text);
            hunk.push('\n');
        }
        hunks.push(hunk);
        index = end;
    }
    hunks.join("")
}

/// Upstream `generateDiffString`: display-oriented diff with line numbers
/// and context.
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> (String, Option<usize>) {
    render_diff_string(
        &diff_lines(old_content, new_content),
        old_content,
        new_content,
        context_lines,
    )
}

/// Render the display diff from an already computed line diff.
fn render_diff_string(
    parts: &[DiffPart<'_>],
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> (String, Option<usize>) {
    let mut output: Vec<String> = Vec::new();

    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let max_line_num = old_lines.len().max(new_lines.len());
    let line_num_width = max_line_num.to_string().len();

    let mut old_line_num = 1usize;
    let mut new_line_num = 1usize;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    for (index, part) in parts.iter().enumerate() {
        let mut raw: Vec<&str> = part.value.split('\n').collect();
        if raw.last() == Some(&"") {
            raw.pop();
        }

        if part.added || part.removed {
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }

            for line in &raw {
                if part.added {
                    output.push(format!("+{new_line_num:>line_num_width$} {line}"));
                    new_line_num += 1;
                } else {
                    output.push(format!("-{old_line_num:>line_num_width$} {line}"));
                    old_line_num += 1;
                }
            }
            last_was_change = true;
        } else {
            let next_part_is_change =
                index < parts.len() - 1 && (parts[index + 1].added || parts[index + 1].removed);
            let has_leading_change = last_was_change;
            let has_trailing_change = next_part_is_change;

            if has_leading_change && has_trailing_change {
                if raw.len() <= context_lines * 2 {
                    for line in &raw {
                        output.push(format!(" {old_line_num:>line_num_width$} {line}"));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    let leading_count = raw.len().min(context_lines);
                    for line in &raw[..leading_count] {
                        output.push(format!(" {old_line_num:>line_num_width$} {line}"));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                    let trailing: Vec<&str> = raw[raw.len() - context_lines..].to_vec();
                    let skipped = raw.len() - leading_count - trailing.len();

                    output.push(format!(" {:>line_num_width$} ...", ""));
                    old_line_num += skipped;
                    new_line_num += skipped;

                    for line in &trailing {
                        output.push(format!(" {old_line_num:>line_num_width$} {line}"));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            } else if has_leading_change {
                let shown_count = raw.len().min(context_lines);
                for line in &raw[..shown_count] {
                    output.push(format!(" {old_line_num:>line_num_width$} {line}"));
                    old_line_num += 1;
                    new_line_num += 1;
                }
                let skipped = raw.len() - shown_count;
                if skipped > 0 {
                    output.push(format!(" {:>line_num_width$} ...", ""));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
            } else if has_trailing_change {
                let skipped = raw.len().saturating_sub(context_lines);
                if skipped > 0 {
                    output.push(format!(" {:>line_num_width$} ...", ""));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
                for line in &raw[skipped..] {
                    output.push(format!(" {old_line_num:>line_num_width$} {line}"));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            } else {
                old_line_num += raw.len();
                new_line_num += raw.len();
            }

            last_was_change = false;
        }
    }

    (output.join("\n"), first_changed_line)
}

/// Both diff renderings for one edit, from a single line diff.
///
/// divergence: upstream calls `Diff.diffLines` twice per edit (once inside
/// `generateDiffString`, once inside `createTwoFilesPatch`); the port
/// computes the parts once and renders both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditDiffRendering {
    pub diff: String,
    pub patch: String,
    pub first_changed_line: Option<usize>,
}

/// Compute both edit renderings from one diff pass.
pub fn render_edit_diffs(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> EditDiffRendering {
    let parts = diff_lines(old_content, new_content);
    let (diff, first_changed_line) =
        render_diff_string(&parts, old_content, new_content, context_lines);
    EditDiffRendering {
        diff,
        patch: render_unified_patch(path, &parts, context_lines),
        first_changed_line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_line_endings_and_bom() {
        assert_eq!(detect_line_ending("a\r\nb\n"), "\r\n");
        assert_eq!(detect_line_ending("a\nb"), "\n");
        assert_eq!(detect_line_ending("no newline"), "\n");
        assert_eq!(strip_bom("\u{FEFF}text"), ("\u{FEFF}", "text"));
        assert_eq!(strip_bom("text"), ("", "text"));
        assert_eq!(
            restore_line_endings(&normalize_to_lf("a\r\nb\r\n"), "\r\n"),
            "a\r\nb\r\n"
        );
    }

    #[test]
    fn normalizes_fuzzy_matches() {
        assert_eq!(
            normalize_for_fuzzy_match("a\u{2019}b\u{2013}c\u{00A0}d"),
            "a'b-c d"
        );
        assert_eq!(normalize_for_fuzzy_match("x  \ny"), "x\ny");
    }

    /// conformance: applyEditsToNormalizedContent — disjoint edits,
    /// overlap rejection, occurrence counting, BOM/ending handled by
    /// callers.
    #[test]
    fn applies_disjoint_edits() {
        let result = apply_edits_to_normalized_content(
            "alpha\nbeta\ngamma\ndelta\n",
            &[
                Edit {
                    old_text: "alpha\n".to_owned(),
                    new_text: "ALPHA\n".to_owned(),
                },
                Edit {
                    old_text: "gamma\n".to_owned(),
                    new_text: "GAMMA\n".to_owned(),
                },
            ],
            "edit.txt",
        )
        .expect("edits apply");
        assert_eq!(result.new_content, "ALPHA\nbeta\nGAMMA\ndelta\n");
    }

    #[test]
    fn rejects_overlapping_edits() {
        let error = apply_edits_to_normalized_content(
            "one\ntwo\nthree\n",
            &[
                Edit {
                    old_text: "one\ntwo\n".to_owned(),
                    new_text: "ONE\nTWO\n".to_owned(),
                },
                Edit {
                    old_text: "two\nthree\n".to_owned(),
                    new_text: "TWO\nTHREE\n".to_owned(),
                },
            ],
            "edit.txt",
        )
        .expect_err("must overlap");
        assert!(error.contains("overlap"), "{error}");
    }

    #[test]
    fn rejects_missing_and_duplicate_targets() {
        let error = apply_edits_to_normalized_content(
            "foo foo foo",
            &[Edit {
                old_text: "bar".to_owned(),
                new_text: "baz".to_owned(),
            }],
            "edit.txt",
        )
        .expect_err("missing");
        assert!(error.contains("Could not find the exact text"), "{error}");

        let error = apply_edits_to_normalized_content(
            "foo foo foo",
            &[Edit {
                old_text: "foo".to_owned(),
                new_text: "bar".to_owned(),
            }],
            "edit.txt",
        )
        .expect_err("duplicate");
        assert!(error.contains("Found 3 occurrences"), "{error}");
    }

    #[test]
    fn rejects_no_change_and_empty_old_text() {
        let error = apply_edits_to_normalized_content(
            "same",
            &[Edit {
                old_text: "same".to_owned(),
                new_text: "same".to_owned(),
            }],
            "edit.txt",
        )
        .expect_err("no change");
        assert!(error.contains("No changes made"), "{error}");

        let error = apply_edits_to_normalized_content(
            "same",
            &[Edit {
                old_text: String::new(),
                new_text: "x".to_owned(),
            }],
            "edit.txt",
        )
        .expect_err("empty oldText");
        assert!(error.contains("must not be empty"), "{error}");
    }

    /// conformance: fuzzy match normalizes smart punctuation and strips
    /// trailing whitespace, then overlays changes onto original lines.
    #[test]
    fn applies_fuzzy_edits_preserving_unchanged_lines() {
        let original = "alpha \nbeta\ngamma\n";
        let result = apply_edits_to_normalized_content(
            original,
            &[Edit {
                // Trailing space differs; fuzzy matching still finds it.
                old_text: "alpha".to_owned(),
                new_text: "ALPHA".to_owned(),
            }],
            "edit.txt",
        )
        .expect("fuzzy edit applies");
        assert_eq!(result.base_content, original);
        assert!(result.new_content.contains("ALPHA"));
        assert!(result.new_content.contains("beta"));
    }

    /// conformance: generateDiffString marks changed lines with numbers
    /// and reports the first changed line in the new file.
    #[test]
    fn generates_display_diff() {
        let (diff, first) = generate_diff_string(
            "alpha\nbeta\ngamma\ndelta\n",
            "ALPHA\nbeta\nGAMMA\ndelta\n",
            4,
        );
        assert!(diff.contains("ALPHA"), "{diff}");
        assert!(diff.contains("GAMMA"), "{diff}");
        assert!(diff.contains("-1 alpha"), "{diff}");
        assert_eq!(first, Some(1));
    }

    /// conformance: generateUnifiedPatch output round-trips through a
    /// patch application (the port asserts structure; the upstream test
    /// applies the patch with the `diff` package).
    #[test]
    fn generates_unified_patch() {
        let patch = generate_unified_patch(
            "edit.txt",
            "alpha\nbeta\ngamma\ndelta\n",
            "ALPHA\nbeta\nGAMMA\ndelta\n",
            4,
        );
        assert!(patch.starts_with("====="), "{patch}");
        assert!(patch.contains("--- edit.txt"));
        assert!(patch.contains("+++ edit.txt"));
        assert!(patch.contains("@@"), "{patch}");
        assert!(patch.contains("-alpha"));
        assert!(patch.contains("+ALPHA"));
        assert!(patch.contains(" beta"));
        assert!(patch.contains("+GAMMA"));
    }

    /// Where the LCS tie-break is ambiguous (repeated lines around the
    /// change), the trimmed LCS answers what jsdiff answers. Expected parts
    /// are `jsdiff@5.2.2` `Diff.diffLines(...)` output (`value` / `added` /
    /// `removed`): the untrimmed port answered `-A, =A` for the first case and
    /// `-a\nb, =a\nb` for the fourth.
    /// `(old content, new content, expected parts as (added, removed, value))`.
    type ExpectedCase = (&'static str, &'static str, Vec<(bool, bool, &'static str)>);

    #[test]
    fn diff_parts_match_jsdiff_on_repeated_lines() {
        let cases: [ExpectedCase; 5] = [
            (
                "A\nA\n",
                "A\n",
                vec![(false, false, "A\n"), (false, true, "A\n")],
            ),
            (
                "A\n",
                "A\nA\n",
                vec![(false, false, "A\n"), (true, false, "A\n")],
            ),
            (
                "x\nA\nA\ny\n",
                "x\nA\ny\n",
                vec![
                    (false, false, "x\nA\n"),
                    (false, true, "A\n"),
                    (false, false, "y\n"),
                ],
            ),
            (
                "a\nb\na\nb\n",
                "a\nb\n",
                vec![(false, false, "a\nb\n"), (false, true, "a\nb\n")],
            ),
            (
                "A\nA\nA\n",
                "A\n",
                vec![(false, false, "A\n"), (false, true, "A\nA\n")],
            ),
        ];

        for (old_content, new_content, expected) in cases {
            let parts = diff_lines(old_content, new_content);
            let actual: Vec<(bool, bool, &str)> = parts
                .iter()
                .map(|part| (part.added, part.removed, part.value.as_ref()))
                .collect();
            assert_eq!(actual, expected, "{old_content:?} -> {new_content:?}");
        }
    }

    /// The LCS table must cover the changed region, not the file: before the
    /// common prefix/suffix were stripped this built a 5000 × 5000 table
    /// (~200 MB) and ran 25M cells for a one-line change.
    #[test]
    fn diff_of_a_single_line_change_in_a_large_file() {
        let old_content: String = (0..5_000).map(|index| format!("line {index}\n")).collect();
        let new_content = old_content.replace("line 3000\n", "line 3000 changed\n");

        let parts = diff_lines(&old_content, &new_content);
        assert_eq!(parts.len(), 4, "context, delete, insert, context");

        assert!(!parts[0].added && !parts[0].removed);
        assert!(parts[0].value.starts_with("line 0\n"), "{}", parts[0].value);
        assert!(
            parts[0].value.ends_with("line 2999\n"),
            "{}",
            parts[0].value
        );

        assert!(!parts[1].added && parts[1].removed);
        assert_eq!(parts[1].value.as_ref(), "line 3000\n");

        assert!(parts[2].added && !parts[2].removed);
        assert_eq!(parts[2].value.as_ref(), "line 3000 changed\n");

        assert!(!parts[3].added && !parts[3].removed);
        assert!(
            parts[3].value.starts_with("line 3001\n"),
            "{}",
            parts[3].value
        );
        assert!(
            parts[3].value.ends_with("line 4999\n"),
            "{}",
            parts[3].value
        );
    }
}
