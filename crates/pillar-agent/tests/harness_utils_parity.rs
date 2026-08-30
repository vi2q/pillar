//! Port of packages/agent/test/harness/truncate.test.ts and the
//! shell-output case from nodejs-env.test.ts (pi v0.84.3).

use pillar_agent::harness::env::StdFsExecutionEnv;
use pillar_agent::harness::types::FileSystem;
use pillar_agent::harness::utils::shell_output::{ShellCaptureOptions, execute_shell_with_capture};
use pillar_agent::harness::utils::truncate::{TruncationOptions, truncate_head, truncate_tail};

fn temp_root(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "pillar-agent-trunc-{tag}-{}",
        pillar_agent::harness::env::test_suffix()
    ));
    std::fs::create_dir_all(&dir).expect("create temp root");
    dir.to_string_lossy().into_owned()
}

fn byte_length(content: &str) -> usize {
    content.len()
}

/// Upstream `bufferTail`: byte-tail with continuation-byte alignment.
fn buffer_tail(content: &str, max_bytes: usize) -> String {
    let bytes = content.as_bytes();
    if bytes.len() <= max_bytes {
        return content.to_owned();
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn assert_matches_buffer_tail(input: &str, max_byte_values: &[usize]) {
    let total_bytes = byte_length(input);
    let values: Vec<usize> = if max_byte_values.is_empty() {
        (0..=total_bytes + 4).collect()
    } else {
        max_byte_values.to_vec()
    };
    for &max_bytes in &values {
        let result = truncate_tail(
            input,
            TruncationOptions {
                max_bytes: Some(max_bytes),
                max_lines: Some(10),
            },
        );
        let expected = buffer_tail(input, max_bytes);
        assert_eq!(
            result.content, expected,
            "tail mismatch input={input:?} maxBytes={max_bytes}"
        );
        assert!(
            result.output_bytes <= max_bytes,
            "tail output exceeded byte limit input={input:?} maxBytes={max_bytes} outputBytes={}",
            result.output_bytes
        );
    }
}

fn sampled_byte_limits(input: &str) -> Vec<usize> {
    let total_bytes = byte_length(input);
    let candidates = [
        0,
        1,
        2,
        3,
        4,
        5,
        8,
        total_bytes.saturating_sub(1).max(1) / 2,
        total_bytes / 2,
        total_bytes / 2 + 1,
        total_bytes.saturating_sub(8),
        total_bytes.saturating_sub(5),
        total_bytes.saturating_sub(4),
        total_bytes.saturating_sub(3),
        total_bytes.saturating_sub(2),
        total_bytes.saturating_sub(1),
        total_bytes,
        total_bytes + 1,
        total_bytes + 4,
    ];
    let mut values: Vec<usize> = candidates.into_iter().collect();
    values.sort_unstable();
    values.dedup();
    values
}

/// upstream test: "counts UTF-8 bytes without Node Buffer"
#[test]
fn counts_utf8_bytes() {
    let content = "aé🙂\nb";
    let result = truncate_head(
        content,
        TruncationOptions {
            max_bytes: Some(100),
            max_lines: Some(10),
        },
    );
    assert!(!result.truncated);
    assert_eq!(result.total_bytes, byte_length(content));
    assert_eq!(result.output_bytes, byte_length(content));
    assert_eq!(result.total_bytes, 9);
}

/// upstream test: "does not count a trailing newline as an extra line"
#[test]
fn does_not_count_a_trailing_newline_as_an_extra_line() {
    let content = "line\nline\nline\n";
    let head = truncate_head(content, TruncationOptions::default());
    let tail = truncate_tail(content, TruncationOptions::default());
    assert!(!head.truncated);
    assert_eq!(head.total_lines, 3);
    assert_eq!(head.output_lines, 3);
    assert!(!tail.truncated);
    assert_eq!(tail.total_lines, 3);
    assert_eq!(tail.output_lines, 3);
}

/// upstream test: "truncates head on UTF-8 byte limits without partial lines"
#[test]
fn truncates_head_on_utf8_byte_limits_without_partial_lines() {
    let result = truncate_head(
        "éé\nabc",
        TruncationOptions {
            max_bytes: Some(4),
            max_lines: Some(10),
        },
    );
    assert_eq!(result.content, "éé");
    assert!(result.truncated);
    assert!(matches!(
        result.truncated_by,
        Some(pillar_agent::harness::utils::truncate::TruncatedBy::Bytes)
    ));
    assert_eq!(result.output_bytes, 4);
    assert!(!result.first_line_exceeds_limit);
}

/// upstream test: "reports head truncation when the first line exceeds the byte limit"
#[test]
fn reports_head_truncation_when_the_first_line_exceeds_the_byte_limit() {
    let result = truncate_head(
        "éé\nabc",
        TruncationOptions {
            max_bytes: Some(3),
            max_lines: Some(10),
        },
    );
    assert_eq!(result.content, "");
    assert!(result.truncated);
    assert!(matches!(
        result.truncated_by,
        Some(pillar_agent::harness::utils::truncate::TruncatedBy::Bytes)
    ));
    assert!(result.first_line_exceeds_limit);
}

/// upstream test: "truncates tail on UTF-8 boundaries when only a partial last line fits"
#[test]
fn truncates_tail_on_utf8_boundaries_when_only_a_partial_last_line_fits() {
    let result = truncate_tail(
        "aé🙂b",
        TruncationOptions {
            max_bytes: Some(5),
            max_lines: Some(10),
        },
    );
    assert_eq!(result.content, "🙂b");
    assert!(result.truncated);
    assert!(matches!(
        result.truncated_by,
        Some(pillar_agent::harness::utils::truncate::TruncatedBy::Bytes)
    ));
    assert!(result.last_line_partial);
    assert_eq!(result.output_bytes, 5);
}

/// upstream test: "truncates an oversized single line with a trailing newline"
#[test]
fn truncates_an_oversized_single_line_with_a_trailing_newline() {
    let input = format!("{}\n", "X".repeat(300_000));
    let result = truncate_tail(
        &input,
        TruncationOptions {
            max_bytes: Some(1024),
            max_lines: Some(100),
        },
    );
    assert_eq!(result.content, "X".repeat(1024));
    assert_eq!(result.output_bytes, 1024);
    assert_eq!(result.output_lines, 1);
    assert!(result.last_line_partial);
    assert!(matches!(
        result.truncated_by,
        Some(pillar_agent::harness::utils::truncate::TruncatedBy::Bytes)
    ));
}

/// upstream test: "drops an oversized trailing character when it cannot fit in tail byte limit"
#[test]
fn drops_an_oversized_trailing_character_when_it_cannot_fit_in_tail_byte_limit() {
    let result = truncate_tail(
        "abc🙂",
        TruncationOptions {
            max_bytes: Some(3),
            max_lines: Some(10),
        },
    );
    assert_eq!(result.content, "");
    assert!(result.truncated);
    assert!(matches!(
        result.truncated_by,
        Some(pillar_agent::harness::utils::truncate::TruncatedBy::Bytes)
    ));
    assert!(result.last_line_partial);
    assert_eq!(result.output_bytes, 0);
}

/// upstream test: "matches Buffer tail truncation semantics for surrogate edge cases"
/// divergence: Rust strings cannot hold unpaired surrogates, so the port
/// verifies the equivalent valid-UTF-8 boundary inputs.
#[test]
fn matches_buffer_tail_truncation_semantics_for_boundary_cases() {
    let inputs = [
        "a\u{1f42d}",  // 4-byte emoji tail
        "\u{1f642}b",  // leading 4-byte char
        "a\u{1f642}b", // middle
        "\u{1f42d}\u{1f42d}\u{1f642}",
        "\u{1f42d}\u{1f642}\u{1f642}",
        "👩‍💻", // ZWJ sequence
    ];
    for input in inputs {
        assert_matches_buffer_tail(input, &[]);
    }
}

/// upstream test: "matches Buffer tail truncation semantics across deterministic fuzz cases"
/// divergence: the alphabet is restricted to valid UTF-8 (no unpaired
/// surrogates), matching what Rust strings can represent.
#[test]
fn matches_buffer_tail_truncation_semantics_across_deterministic_fuzz_cases() {
    let alphabet = [
        "a", "\u{007f}", "\u{0080}", "é", "\u{07ff}", "\u{0800}", "中", "\u{d7ff}", "\u{e000}",
        "\u{ffff}", "🙂",
    ];

    fn check_exhaustive(prefix: &str, depth: usize, alphabet: &[&str]) {
        assert_matches_buffer_tail(prefix, &sampled_byte_limits(prefix));
        if depth == 0 {
            return;
        }
        for character in alphabet {
            check_exhaustive(&format!("{prefix}{character}"), depth - 1, alphabet);
        }
    }
    check_exhaustive("", 3, &alphabet);

    let mut seed: u32 = 0x1234_5678;
    let mut random = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        seed as f64 / u32::MAX as f64
    };
    for _ in 0..1_000 {
        let length = (random() * 80.0) as usize;
        let mut input = String::new();
        for _ in 0..length {
            let index = (random() * alphabet.len() as f64) as usize;
            input.push_str(alphabet[index.min(alphabet.len() - 1)]);
        }
        assert_matches_buffer_tail(&input, &sampled_byte_limits(&input));
    }
}

/// upstream test (nodejs-env.test.ts): "captures large shell output to a
/// full output file through the execution env"
#[tokio::test]
async fn captures_large_shell_output_to_a_full_output_file_through_the_execution_env() {
    let root = temp_root("spill");
    let env = StdFsExecutionEnv::new(&root);
    let result = execute_shell_with_capture(
        &env,
        "yes line | head -n 15000",
        Some(ShellCaptureOptions::default()),
    )
    .await
    .expect("capture");
    assert!(result.truncated);
    let full_output_path = result
        .full_output_path
        .clone()
        .expect("full output path created");
    let full_output = env
        .read_text_file(&full_output_path)
        .await
        .expect("read spill file");
    assert!(full_output.lines().count() > 10_000);
    assert!(result.output.len() < full_output.len());
}
