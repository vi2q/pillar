//! Parity tests for edit-diff.ts (pi v0.84.3): line ending detection,
//! fuzzy matching normalization, multi-edit application (disjoint regions,
//! overlap errors, duplicate errors), fuzzy overlay preserving original
//! bytes, and diff generation.

use pillar_coding_agent::core::tools::edit_diff::{
    Edit, apply_edits_to_normalized_content, compute_edits_diff, detect_line_ending,
    fuzzy_find_text, generate_diff_string, generate_unified_patch, normalize_for_fuzzy_match,
    normalize_to_lf, restore_line_endings,
};

fn edit(old_text: &str, new_text: &str) -> Edit {
    Edit {
        old_text: old_text.to_string(),
        new_text: new_text.to_string(),
    }
}

// --- line endings ---------------------------------------------------------------

#[test]
fn detect_line_ending_prefers_crlf_when_first() {
    assert_eq!(detect_line_ending("a\r\nb\nc"), "\r\n");
    assert_eq!(detect_line_ending("a\nb\rc"), "\n");
    assert_eq!(detect_line_ending("no newlines"), "\n");
}

#[test]
fn normalize_and_restore_line_endings() {
    assert_eq!(normalize_to_lf("a\r\nb\rc"), "a\nb\nc");
    assert_eq!(restore_line_endings("a\nb", "\r\n"), "a\r\nb");
    assert_eq!(restore_line_endings("a\nb", "\n"), "a\nb");
}

// --- fuzzy normalization ------------------------------------------------------------

#[test]
fn normalize_for_fuzzy_match_transformations() {
    // Trailing whitespace stripped per line (the trailing newline keeps an
    // empty final segment, matching upstream's split/join round trip).
    assert_eq!(normalize_for_fuzzy_match("a  \nb \t\n"), "a\nb\n");
    // Smart quotes -> ASCII.
    assert_eq!(
        normalize_for_fuzzy_match("\u{2018}x\u{2019} \u{201c}y\u{201d}"),
        "'x' \"y\""
    );
    // Unicode dashes -> hyphen (em-dash, en-dash, minus).
    assert_eq!(
        normalize_for_fuzzy_match("a\u{2014}b\u{2013}c\u{2212}d"),
        "a-b-c-d"
    );
    // Special spaces -> regular space (NBSP, narrow NBSP, ideographic).
    assert_eq!(
        normalize_for_fuzzy_match("a\u{00a0}b\u{202f}c\u{3000}d"),
        "a b c d"
    );
}

// --- fuzzyFindText ----------------------------------------------------------------------

#[test]
fn fuzzy_find_exact_match_first() {
    let result = fuzzy_find_text("hello world", "world");
    assert!(result.found);
    assert_eq!(result.index, 6);
    assert_eq!(result.match_length, 5);
    assert!(!result.used_fuzzy_match);
    assert_eq!(result.content_for_replacement, "hello world");
}

#[test]
fn fuzzy_find_falls_back_to_normalized_content() {
    // Content has trailing whitespace that the pattern omits.
    let content = "function foo() {\n  return 1;  \n}";
    let result = fuzzy_find_text(content, "return 1;\n}");
    assert!(result.found);
    assert!(result.used_fuzzy_match);
    // The replacement content is the normalized version.
    assert_eq!(
        result.content_for_replacement,
        "function foo() {\n  return 1;\n}"
    );
}

#[test]
fn fuzzy_find_not_found() {
    let result = fuzzy_find_text("abc", "xyz");
    assert!(!result.found);
    assert_eq!(result.index, 0);
    assert_eq!(result.match_length, 0);
    assert!(!result.used_fuzzy_match);
}

// --- applyEditsToNormalizedContent ------------------------------------------------------------

#[test]
fn apply_single_edit() {
    let applied = apply_edits_to_normalized_content(
        "const a = 1;\nconst b = 2;\n",
        &[edit("const b = 2;", "const b = 3;")],
        "f.ts",
    )
    .unwrap();
    assert_eq!(applied.base_content, "const a = 1;\nconst b = 2;\n");
    assert_eq!(applied.new_content, "const a = 1;\nconst b = 3;\n");
}

#[test]
fn apply_multiple_disjoint_edits_match_same_original() {
    let applied = apply_edits_to_normalized_content(
        "a\nb\nc\nd\ne",
        &[edit("a", "A"), edit("c", "C"), edit("e", "E")],
        "f.ts",
    )
    .unwrap();
    // All edits match the same original content; reverse-order application
    // keeps offsets stable.
    assert_eq!(applied.new_content, "A\nb\nC\nd\nE");
}

#[test]
fn apply_edits_not_found_error() {
    let single = apply_edits_to_normalized_content("abc", &[edit("zzz", "y")], "f.ts").unwrap_err();
    assert_eq!(
        single,
        "Could not find the exact text in f.ts. The old text must match exactly including all whitespace and newlines."
    );
    let multi =
        apply_edits_to_normalized_content("abc", &[edit("a", "x"), edit("zzz", "y")], "f.ts")
            .unwrap_err();
    assert_eq!(
        multi,
        "Could not find edits[1] in f.ts. The oldText must match exactly including all whitespace and newlines."
    );
}

#[test]
fn apply_edits_duplicate_error() {
    let content = "dup\nmid\ndup";
    let single =
        apply_edits_to_normalized_content(content, &[edit("dup", "x")], "f.ts").unwrap_err();
    assert_eq!(
        single,
        "Found 2 occurrences of the text in f.ts. The text must be unique. Please provide more context to make it unique."
    );
    let multi =
        apply_edits_to_normalized_content(content, &[edit("mid", "m"), edit("dup", "x")], "f.ts")
            .unwrap_err();
    assert_eq!(
        multi,
        "Found 2 occurrences of edits[1] in f.ts. Each oldText must be unique. Please provide more context to make it unique."
    );
}

#[test]
fn apply_edits_empty_old_text_error() {
    let single = apply_edits_to_normalized_content("abc", &[edit("", "y")], "f.ts").unwrap_err();
    assert_eq!(single, "oldText must not be empty in f.ts.");
    let multi = apply_edits_to_normalized_content("abc", &[edit("a", "x"), edit("", "y")], "f.ts")
        .unwrap_err();
    assert_eq!(multi, "edits[1].oldText must not be empty in f.ts.");
}

#[test]
fn apply_edits_overlap_error() {
    let error = apply_edits_to_normalized_content(
        "abcdefgh",
        &[edit("abc", "x"), edit("cdef", "y")],
        "f.ts",
    )
    .unwrap_err();
    assert_eq!(
        error,
        "edits[0] and edits[1] overlap in f.ts. Merge them into one edit or target disjoint regions."
    );
}

#[test]
fn apply_edits_no_change_error() {
    let single =
        apply_edits_to_normalized_content("abc", &[edit("abc", "abc")], "f.ts").unwrap_err();
    assert_eq!(
        single,
        "No changes made to f.ts. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
    );
    let multi = apply_edits_to_normalized_content("abc", &[edit("a", "a"), edit("b", "b")], "f.ts")
        .unwrap_err();
    assert_eq!(
        multi,
        "No changes made to f.ts. The replacements produced identical content."
    );
}

#[test]
fn apply_fuzzy_edits_preserve_unchanged_lines() {
    // Original content has trailing whitespace; the edit pattern omits it.
    // Unchanged lines must keep their original bytes.
    let original = "keep me   \nchange me  \nalso keep \t";
    let applied = apply_edits_to_normalized_content(
        &normalize_to_lf(original),
        &[edit("change me", "CHANGED")],
        "f.ts",
    )
    .unwrap();
    // The touched line keeps its trailing whitespace: only the matched
    // segment is replaced (upstream rewrites the line from the normalized
    // base with the replacement applied in place).
    assert_eq!(applied.new_content, "keep me   \nCHANGED  \nalso keep \t");
}

// --- diff generation -----------------------------------------------------------------------

#[test]
fn generate_diff_string_marks_added_and_removed_lines() {
    let (diff, first_changed) = generate_diff_string("a\nb\nc", "a\nB\nc", 4);
    // Context lines print the OLD line number (upstream prints oldLineNum).
    assert!(diff.contains(" 1 a"), "{diff}");
    assert!(diff.contains("-2 b"), "{diff}");
    assert!(diff.contains("+2 B"), "{diff}");
    assert!(diff.contains(" 3 c"), "{diff}");
    assert_eq!(first_changed, Some(2));
}

#[test]
fn generate_diff_string_first_changed_line_in_new_file() {
    // Removed lines shift the new-file numbering.
    let (diff, first_changed) = generate_diff_string("one\ntwo\nthree", "two\nthree", 4);
    // "one" removed; the first change is the removal (new-file line 1).
    assert_eq!(first_changed, Some(1));
    assert!(diff.contains("-1 one"), "{diff}");
    // Context lines print the old numbering: "two" is old line 2.
    assert!(diff.contains(" 2 two"), "{diff}");
}

#[test]
fn generate_diff_string_skips_distant_context() {
    let old = (0..50)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
    lines[25] = "CHANGED".to_string();
    let new = lines.join("\n");
    let (diff, _) = generate_diff_string(&old, &new, 4);
    // The distant context between the changes collapses with a "..." marker.
    assert!(diff.contains("..."), "{diff}");
    assert!(diff.contains("+26 CHANGED"), "{diff}");
    assert!(!diff.contains("line 0"), "{diff}");
    assert!(!diff.contains("line 49"), "{diff}");
}

#[test]
fn generate_unified_patch_has_headers_and_context() {
    let patch = generate_unified_patch("f.ts", "a\nb\nc", "a\nB\nc");
    assert!(patch.starts_with("--- f.ts\n"), "{patch}");
    assert!(patch.contains("+++ f.ts"), "{patch}");
    assert!(patch.contains("-b") && patch.contains("+B"), "{patch}");
    // Context lines surround the change.
    assert!(patch.contains(" a"), "{patch}");
    assert!(patch.contains(" c"), "{patch}");
}

// --- computeEditsDiff ---------------------------------------------------------------------------

#[test]
fn compute_edits_diff_round_trips_through_disk() {
    let dir = std::env::temp_dir().join(format!("pillar-editdiff-{}", std::process::id()));
    fs_extra_setup(&dir);
    let path = dir.join("f.txt");
    std::fs::write(&path, "a\nb\nc\n").unwrap();
    let result = compute_edits_diff(path.to_str().unwrap(), &[edit("b", "B")], "/tmp").unwrap();
    assert!(result.diff.contains("+2 B"), "{}", result.diff);
    assert_eq!(result.first_changed_line, Some(2));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn compute_edits_diff_missing_file_is_an_error() {
    let error =
        compute_edits_diff("/definitely/not/real.ts", &[edit("a", "b")], "/tmp").unwrap_err();
    assert!(error.contains("Could not edit file"), "{error}");
}

fn fs_extra_setup(dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
}
