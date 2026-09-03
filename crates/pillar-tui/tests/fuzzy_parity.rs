//! Parity tests for tui/src/fuzzy.ts (pi v0.84.3): subsequence matching,
//! scoring bonuses/penalties, swapped alpha/digit retry, and tokenized
//! filtering/sorting.

use pillar_tui::fuzzy::{fuzzy_filter_by, fuzzy_match};

#[test]
fn empty_query_matches_everything_with_zero_score() {
    let result = fuzzy_match("", "anything");
    assert!(result.matches);
    assert_eq!(result.score, 0.0);
}

#[test]
fn query_longer_than_text_fails() {
    assert!(!fuzzy_match("toolongquery", "short").matches);
}

#[test]
fn subsequence_in_order_matches() {
    assert!(fuzzy_match("abc", "a1b2c3").matches);
    // Out of order does not.
    assert!(!fuzzy_match("acb", "a1b2c3").matches);
}

#[test]
fn exact_match_gets_big_bonus() {
    let exact = fuzzy_match("hello", "hello");
    let prefix = fuzzy_match("hello", "hello world");
    assert!(exact.matches);
    assert!(prefix.matches);
    assert!(exact.score < prefix.score, "exact should score better");
    assert!(
        exact.score <= -100.0,
        "exact bonus applied: {}",
        exact.score
    );
}

#[test]
fn word_boundary_and_consecutive_bonuses() {
    // Consecutive matches beat spread out ones (same start offset):
    // "ab" in "xab" is consecutive, in "xaxb" has a one-char gap.
    let consecutive = fuzzy_match("ab", "xab");
    let spread = fuzzy_match("ab", "xaxb");
    assert!(consecutive.matches && spread.matches);
    assert!(
        consecutive.score < spread.score,
        "consecutive {consecutive:?} vs spread {spread:?}"
    );

    // Word boundary (after space) beats mid-word.
    let boundary = fuzzy_match("b", "a b");
    let midword = fuzzy_match("b", "abz");
    assert!(
        boundary.score < midword.score,
        "boundary {boundary:?} vs mid {midword:?}"
    );

    // Start-of-text is a boundary.
    let start = fuzzy_match("a", "abc");
    assert!(start.score <= -10.0 + 0.0, "start bonus: {}", start.score);
}

#[test]
fn later_matches_score_worse() {
    let early = fuzzy_match("a", "abc");
    let late = fuzzy_match("a", "zza");
    assert!(early.matches && late.matches);
    assert!(early.score < late.score, "early {early:?} vs late {late:?}");
}

#[test]
fn case_insensitive() {
    assert!(fuzzy_match("ABC", "abc").matches);
    assert!(fuzzy_match("abc", "ABC").matches);
    // Exact match after lowering still gets the bonus.
    assert!(fuzzy_match("ABC", "abc").score <= -100.0);
}

#[test]
fn swapped_alpha_digit_retry() {
    // "abc123" doesn't subsequence-match "123abc" in order...
    let direct = fuzzy_match("abc123", "123abc");
    // ...but the swapped form does, with the +5 penalty.
    assert!(direct.matches, "swapped form should match");
    // Compare against matching the swapped text directly: the swapped
    // path is 5 points worse.
    let equivalent = fuzzy_match("123abc", "123abc");
    assert!((direct.score - (equivalent.score + 5.0)).abs() < 1e-9);
}

#[test]
fn no_swapped_form_falls_back_to_primary() {
    // Neither form matches.
    assert!(!fuzzy_match("xyz", "abc").matches);
    // Mixed alpha/digit but not the swapped pattern.
    assert!(!fuzzy_match("a1b2", "b2a1").matches);
}

// --- fuzzyFilter -------------------------------------------------------------------------------

#[test]
fn filter_empty_query_returns_all() {
    let items = vec!["one", "two", "three"];
    assert_eq!(fuzzy_filter_by(&items, "", |i| i.to_string()).len(), 3);
    assert_eq!(fuzzy_filter_by(&items, "   ", |i| i.to_string()).len(), 3);
}

#[test]
fn filter_all_tokens_must_match() {
    let items = vec!["read-files", "write-file", "read"];
    // Both "read" and "files" tokens must match.
    let result = fuzzy_filter_by(&items, "read files", |i| i.to_string());
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], &"read-files");
}

#[test]
fn filter_splits_on_whitespace_and_slashes() {
    let items = vec!["src/main.rs", "src/fuzzy.rs", "docs/readme.md"];
    let result = fuzzy_filter_by(&items, "src/main", |i| i.to_string());
    assert_eq!(result, vec![&"src/main.rs"]);
    // Slash-separated tokens.
    let result = fuzzy_filter_by(&items, "src/fuzzy", |i| i.to_string());
    assert_eq!(result, vec![&"src/fuzzy.rs"]);
}

#[test]
fn filter_sorts_by_total_score_best_first() {
    let items = vec!["file-editor", "editor", "ed"];
    let result = fuzzy_filter_by(&items, "ed", |i| i.to_string());
    // Exact match "ed" first, then the word-boundary prefix.
    assert_eq!(result[0], &"ed");
    assert_eq!(result.len(), 3);
    // Later entries score progressively worse.
    assert_eq!(result[1], &"editor");
    assert_eq!(result[2], &"file-editor");
}

#[test]
fn filter_case_insensitive() {
    let items = vec!["ReadFile", "writefile"];
    let result = fuzzy_filter_by(&items, "read", |i| i.to_string());
    assert_eq!(result, vec![&"ReadFile"]);
}
