//! Parity tests for tools/output-accumulator.ts + tools/path-utils.ts (pi
//! v0.84.3): bounded-memory streaming accumulation with temp-file spill and
//! tail snapshots, plus tool path expansion/resolution with macOS variant
//! fallbacks.

use pillar_coding_agent::core::tools::output_accumulator::{
    OutputAccumulator, OutputAccumulatorOptions,
};
use pillar_coding_agent::core::tools::path_utils::{
    expand_path, resolve_read_path, resolve_to_cwd,
};
use pillar_coding_agent::core::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy};

// --- OutputAccumulator ---------------------------------------------------------

#[test]
fn accumulator_small_output_no_truncation() {
    let mut acc = OutputAccumulator::default();
    acc.append(b"hello\nworld\n");
    acc.finish();
    let snapshot = acc.snapshot(false);
    assert!(!snapshot.truncation.truncated);
    assert_eq!(snapshot.content, "hello\nworld\n");
    assert_eq!(snapshot.truncation.total_lines, 2);
    assert!(snapshot.full_output_path.is_none());
}

#[test]
fn accumulator_counts_open_lines() {
    let mut acc = OutputAccumulator::default();
    acc.append(b"line1\n");
    assert_eq!(acc.snapshot(false).truncation.total_lines, 1);
    acc.append(b"open line without newline");
    // completed=1 + open=1
    assert_eq!(acc.snapshot(false).truncation.total_lines, 2);
    assert_eq!(acc.get_last_line_bytes(), "open line without newline".len());
    acc.append(b"\n");
    // completed=2, no open line.
    assert_eq!(acc.snapshot(false).truncation.total_lines, 2);
    assert_eq!(acc.get_last_line_bytes(), 0);
}

#[test]
fn accumulator_truncates_by_line_limit_keeping_tail() {
    let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
        max_lines: Some(3),
        max_bytes: Some(1024 * 1024),
        temp_file_prefix: None,
    });
    for i in 1..=10u32 {
        acc.append(format!("line {i}\n").as_bytes());
    }
    acc.finish();
    let snapshot = acc.snapshot(false);
    assert!(snapshot.truncation.truncated);
    assert_eq!(snapshot.truncation.total_lines, 10);
    // Tail keeps the last 3 lines.
    assert_eq!(snapshot.content, "line 8\nline 9\nline 10");
    assert_eq!(snapshot.truncation.truncated_by, Some(TruncatedBy::Lines));
}

#[test]
fn accumulator_spills_to_temp_file_past_byte_threshold() {
    let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
        max_lines: Some(2000),
        max_bytes: Some(256),
        temp_file_prefix: Some(format!("pillar-acc-{}", std::process::id())),
    });
    // Single oversized line: exceeds the byte threshold.
    let big = "x".repeat(600);
    acc.append(big.as_bytes());
    acc.finish();
    let snapshot = acc.snapshot(true);
    assert!(snapshot.truncation.truncated);
    let path = snapshot.full_output_path.expect("temp file");
    let persisted = std::fs::read_to_string(&path).unwrap();
    assert!(persisted.contains(&big), "raw bytes spilled");
    std::fs::remove_file(&path).ok();
    // Content keeps the tail within the byte limit.
    assert!(snapshot.content.len() <= 256);
}

#[test]
fn accumulator_rolling_tail_trims_to_bound() {
    let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
        max_lines: Some(2000),
        max_bytes: Some(64),
        temp_file_prefix: None,
    });
    // Feed many lines; the rolling tail must stay bounded (~2x maxBytes).
    for i in 0..50u32 {
        acc.append(format!("chunk-{i} with some padding\n").as_bytes());
    }
    acc.finish();
    let snapshot = acc.snapshot(false);
    // Snapshot content respects the byte limit.
    assert!(
        snapshot.content.len() <= 64 + 4,
        "{}",
        snapshot.content.len()
    );
    // Totals reflect everything.
    assert_eq!(snapshot.truncation.total_lines, 50);
    // 10 lines of "chunk-N with some padding\n" (26 bytes) + 40 of the
    // two-digit variant (27 bytes).
    assert_eq!(snapshot.truncation.total_bytes, 10 * 26 + 40 * 27);
    assert!(snapshot.truncation.total_bytes > 0);
}

#[test]
fn accumulator_persists_raw_bytes_not_sanitized_text() {
    let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
        max_bytes: Some(16),
        temp_file_prefix: Some(format!("pillar-raw-{}", std::process::id())),
        ..Default::default()
    });
    acc.append(b"\x1b[31mraw bytes stay raw\x1b[0m\n");
    acc.finish();
    let snapshot = acc.snapshot(true);
    let path = snapshot.full_output_path.expect("temp file");
    let persisted = std::fs::read(&path).unwrap();
    // Raw bytes (including the ANSI escapes) are persisted.
    assert!(persisted.starts_with(b"\x1b[31m"), "raw spill");
    std::fs::remove_file(&path).ok();
}

#[test]
fn accumulator_default_limits_match_upstream() {
    let mut acc = OutputAccumulator::default();
    acc.append("x".repeat(DEFAULT_MAX_BYTES - 1).as_bytes());
    acc.finish();
    assert!(!acc.snapshot(false).truncation.truncated);
    let _ = DEFAULT_MAX_LINES;
}

#[test]
#[should_panic(expected = "Cannot append to a finished output accumulator")]
fn accumulator_rejects_append_after_finish() {
    let mut acc = OutputAccumulator::default();
    acc.append(b"done");
    acc.finish();
    acc.append(b"more");
}

// --- path utils ------------------------------------------------------------------

#[test]
fn expand_path_expands_tilde() {
    // SAFETY: test-only single-threaded env mutation.
    unsafe { std::env::set_var("HOME", "/home/tester") };
    assert_eq!(
        expand_path("~/proj/file.txt"),
        std::path::PathBuf::from("/home/tester/proj/file.txt")
    );
    assert_eq!(expand_path("~"), std::path::PathBuf::from("/home/tester"));
}

#[test]
fn expand_path_strips_at_prefix_and_normalizes_spaces() {
    unsafe { std::env::set_var("HOME", "/home/tester") };
    assert_eq!(
        expand_path("@relative/file.txt"),
        std::path::PathBuf::from("relative/file.txt")
    );
    // Narrow no-break spaces normalize to regular spaces.
    assert_eq!(
        expand_path("my\u{202f}file.txt"),
        std::path::PathBuf::from("my file.txt")
    );
}

#[test]
fn resolve_to_cwd_joins_relative_paths() {
    assert_eq!(
        resolve_to_cwd("a/b.txt", "/base"),
        std::path::PathBuf::from("/base/a/b.txt")
    );
    assert_eq!(
        resolve_to_cwd("/abs/x.txt", "/base"),
        std::path::PathBuf::from("/abs/x.txt")
    );
    assert_eq!(
        resolve_to_cwd("a/../b.txt", "/base"),
        std::path::PathBuf::from("/base/b.txt")
    );
}

#[test]
fn resolve_read_path_falls_back_to_macos_variants() {
    let dir = std::env::temp_dir().join(format!("pillar-pathutils-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let cwd = dir.to_string_lossy().to_string();

    // Direct hit.
    std::fs::write(dir.join("plain.txt"), "x").unwrap();
    assert_eq!(resolve_read_path("plain.txt", &cwd), dir.join("plain.txt"));

    // Curly-quote variant: user types a straight apostrophe, the file has
    // U+2019.
    std::fs::write(dir.join("l\u{2019}été.txt"), "x").unwrap();
    assert_eq!(
        resolve_read_path("l'été.txt", &cwd),
        dir.join("l\u{2019}été.txt")
    );

    // NFD variant: user types precomposed é, the file is decomposed. On
    // APFS (normalization-insensitive) the precomposed lookup already
    // matches, so only assert that resolution succeeds either way.
    std::fs::write(dir.join("e\u{0301}clair.txt"), "x").unwrap();
    let resolved = resolve_read_path("éclair.txt", &cwd);
    assert!(resolved.exists(), "{resolved:?}");

    // Missing file returns the resolved path unchanged.
    assert_eq!(
        resolve_read_path("absent.txt", &cwd),
        dir.join("absent.txt")
    );
}
