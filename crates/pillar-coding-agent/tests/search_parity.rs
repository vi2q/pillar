//! Parity tests for the FFF-inspired native search (upstream find/grep
//! tools): parallel gitignore-aware traversal, glob matching, relativized
//! output, notices, and the grep match/context/limit contract.

use std::fs;
use std::path::{Path, PathBuf};

use pillar_coding_agent::core::tools::search::{
    FindOptions, FindResult, GrepOptions, GrepResult, find_files, grep_files, inside_git_repo,
    relativize_find_result_path,
};

fn setup_repo(name: &str, files: &[(&str, &str)]) -> (PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("pillar-search-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for (path, content) in files {
        let full = dir.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }
    // Mark as a git repo so .gitignore rules apply.
    fs::create_dir_all(dir.join(".git")).unwrap();
    let cwd = dir.to_string_lossy().to_string();
    (dir, cwd)
}

// --- helpers -----------------------------------------------------------------

#[test]
fn relativize_paths_against_search_root() {
    let root = PathBuf::from("/base");
    assert_eq!(
        relativize_find_result_path(Path::new("/base/src/a.ts"), &root),
        "src/a.ts"
    );
    // Outside the root stays absolute.
    assert_eq!(
        relativize_find_result_path(Path::new("/other/x"), &root),
        "/other/x"
    );
    // Trailing separator preserved.
    assert_eq!(
        relativize_find_result_path(Path::new("/base/dir/"), &root),
        "dir/"
    );
}

#[test]
fn inside_git_repo_walks_ancestors() {
    let dir = std::env::temp_dir().join(format!("pillar-gitcheck-{}", std::process::id()));
    let nested = dir.join("a").join("b");
    let _ = fs::create_dir_all(&nested);
    assert!(!inside_git_repo(&nested));
    fs::create_dir_all(dir.join(".git")).unwrap();
    assert!(inside_git_repo(&nested));
    assert!(inside_git_repo(&dir));
    fs::remove_dir_all(&dir).ok();
}

// --- find ----------------------------------------------------------------------

#[test]
fn find_matches_glob_and_relativizes() {
    let (dir, cwd) = setup_repo(
        "find-glob",
        &[
            ("src/a.ts", "x"),
            ("src/deep/b.spec.ts", "x"),
            ("docs/readme.md", "x"),
        ],
    );
    let result = find_files(
        "src/**/*.ts",
        &dir.to_string_lossy(),
        &cwd,
        FindOptions::default(),
    )
    .unwrap();
    assert!(result.text.contains("src/a.ts"), "{}", result.text);
    assert!(result.text.contains("src/deep/b.spec.ts"));
    assert!(!result.text.contains("readme.md"));
    assert!(result.result_limit_reached.is_none());
    assert!(result.truncation_max_bytes.is_none());
}

#[test]
fn find_bare_pattern_matches_basename() {
    let (dir, cwd) = setup_repo(
        "find-bare",
        &[
            ("src/thing.ts", "x"),
            ("lib/thing.ts", "x"),
            ("other.md", "x"),
        ],
    );
    let result = find_files("*.ts", &dir.to_string_lossy(), &cwd, FindOptions::default()).unwrap();
    assert!(result.text.contains("src/thing.ts"));
    assert!(result.text.contains("lib/thing.ts"));
    assert!(!result.text.contains("other.md"));
}

#[test]
fn find_respects_gitignore() {
    let (dir, cwd) = setup_repo(
        "find-ignore",
        &[("keep.ts", "x"), ("build/generated.ts", "x")],
    );
    fs::write(dir.join(".gitignore"), "build/\n").unwrap();
    let result = find_files("*.ts", &dir.to_string_lossy(), &cwd, FindOptions::default()).unwrap();
    assert!(result.text.contains("keep.ts"), "{}", result.text);
    assert!(!result.text.contains("generated.ts"), "{}", result.text);
}

#[test]
fn find_respects_hard_ignore_node_modules() {
    let (dir, cwd) = setup_repo(
        "find-nm",
        &[("app.ts", "x"), ("node_modules/pkg/index.js", "x")],
    );
    let result = find_files(
        "**/*.js",
        &dir.to_string_lossy(),
        &cwd,
        FindOptions::default(),
    )
    .unwrap();
    assert!(!result.text.contains("node_modules"), "{}", result.text);
    let _ = cwd;
}

#[test]
fn find_limit_reached_adds_notice() {
    let files: Vec<(String, &str)> = (0..10).map(|i| (format!("f{i}.ts"), "x")).collect();
    let files_ref: Vec<(&str, &str)> = files.iter().map(|(p, c)| (p.as_str(), *c)).collect();
    let (dir, cwd) = setup_repo("find-limit", &files_ref);
    let result = find_files(
        "*.ts",
        &dir.to_string_lossy(),
        &cwd,
        FindOptions {
            limit: 3,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.result_limit_reached, Some(3));
    assert!(
        result
            .text
            .contains("3 results limit reached. Use limit=6 for more"),
        "{}",
        result.text
    );
    // Output capped to 3 result lines, then an empty line + notice line.
    // (The parallel walk's first-3 selection is scheduling-dependent.)
    let lines: Vec<&str> = result.text.lines().collect();
    assert_eq!(lines.len(), 5);
    for line in &lines[..3] {
        assert!(line.ends_with(".ts"), "{}", line);
    }
    assert!(lines[3].is_empty());
    assert!(lines[4].starts_with("[3 results limit reached"));
}

#[test]
fn find_no_matches_message() {
    let (dir, cwd) = setup_repo("find-none", &[("a.ts", "x")]);
    let result = find_files(
        "*.zzz",
        &dir.to_string_lossy(),
        &cwd,
        FindOptions::default(),
    )
    .unwrap();
    assert_eq!(result.text, "No files found matching pattern");
}

#[test]
fn find_missing_path_errors() {
    let (_dir, cwd) = setup_repo("find-missing", &[]);
    let error =
        find_files("*.ts", "/definitely/not/real", &cwd, FindOptions::default()).unwrap_err();
    assert!(error.starts_with("Path not found:"), "{error}");
}

#[test]
fn find_results_are_sorted_deterministically() {
    let (dir, cwd) = setup_repo("find-sort", &[("z.ts", "x"), ("a.ts", "x"), ("m.ts", "x")]);
    let result = find_files("*.ts", &dir.to_string_lossy(), &cwd, FindOptions::default()).unwrap();
    let lines: Vec<&str> = result.text.lines().collect();
    let mut sorted = lines.clone();
    sorted.sort();
    assert_eq!(lines, sorted);
}

// --- grep ----------------------------------------------------------------------

#[test]
fn grep_matches_contents_with_line_numbers() {
    let (dir, cwd) = setup_repo(
        "grep-basic",
        &[
            ("a.txt", "hello world\nsecond line\nhello again"),
            ("b.txt", "no match here"),
        ],
    );
    let result = grep_files(
        "hello",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions::default(),
    )
    .unwrap();
    assert!(
        result.text.contains("a.txt:1: hello world"),
        "{}",
        result.text
    );
    assert!(result.text.contains("a.txt:3: hello again"));
    assert!(!result.text.contains("b.txt"));
    assert!(result.match_limit_reached.is_none());
}

#[test]
fn grep_no_matches_message() {
    let (dir, cwd) = setup_repo("grep-none", &[("a.txt", "content")]);
    let result = grep_files(
        "zzz-not-there",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions::default(),
    )
    .unwrap();
    assert_eq!(result.text, "No matches found");
}

#[test]
fn grep_ignore_case() {
    let (dir, cwd) = setup_repo("grep-case", &[("a.txt", "HELLO world")]);
    let case_sensitive = grep_files(
        "hello",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions::default(),
    )
    .unwrap();
    assert_eq!(case_sensitive.text, "No matches found");
    let insensitive = grep_files(
        "hello",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions {
            ignore_case: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        insensitive.text.contains("a.txt:1:"),
        "{}",
        insensitive.text
    );
}

#[test]
fn grep_literal_mode_escapes_regex() {
    let (dir, cwd) = setup_repo("grep-literal", &[("a.txt", "a.b (c)")]);
    // Regex mode: "." matches any char, but the literal line still matches.
    let literal = grep_files(
        "a.b",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions {
            literal: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(literal.text.contains("a.txt:1:"), "{}", literal.text);
    // A regex-only pattern like "(c" is invalid as regex but valid literal.
    let parens = grep_files(
        "(c",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions {
            literal: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(parens.text.contains("a.txt:1:"), "{}", parens.text);
}

#[test]
fn grep_glob_filters_files() {
    let (dir, cwd) = setup_repo(
        "grep-glob",
        &[("a.ts", "target"), ("b.md", "target"), ("c.ts", "other")],
    );
    let result = grep_files(
        "target",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions {
            glob: Some("*.ts".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(result.text.contains("a.ts:"), "{}", result.text);
    assert!(!result.text.contains("b.md"));
}

#[test]
fn grep_context_lines() {
    let (dir, cwd) = setup_repo("grep-ctx", &[("a.txt", "one\ntwo\nMATCH\nfour\nfive")]);
    let result = grep_files(
        "MATCH",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions {
            context: 1,
            ..Default::default()
        },
    )
    .unwrap();
    // Context lines use the "-N-" separator; the match line uses ":N:".
    assert!(result.text.contains("a.txt-2- two"), "{}", result.text);
    assert!(result.text.contains("a.txt:3: MATCH"));
    assert!(result.text.contains("a.txt-4- four"));
    assert!(!result.text.contains("one"), "{}", result.text);
    assert!(!result.text.contains("five"));
}

#[test]
fn grep_match_limit_adds_notice() {
    let (dir, cwd) = setup_repo("grep-limit", &[("a.txt", "x\nx\nx\nx\nx")]);
    let result = grep_files(
        "x",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions {
            limit: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.match_limit_reached, Some(2));
    assert!(
        result
            .text
            .contains("2 matches limit reached. Use limit=4 for more"),
        "{}",
        result.text
    );
}

#[test]
fn grep_truncates_long_lines() {
    let (dir, cwd) = setup_repo("grep-long", &[("a.txt", &"y".repeat(600))]);
    let result = grep_files("y", &dir.to_string_lossy(), &cwd, GrepOptions::default()).unwrap();
    assert!(result.lines_truncated, "{}", result.text);
    assert!(
        result.text.contains("Some lines truncated to 500 chars"),
        "{}",
        result.text
    );
    // The match line itself is capped.
    let match_line = result.text.lines().next().unwrap();
    assert!(match_line.len() < 600);
}

#[test]
fn grep_skips_binary_files() {
    let (dir, cwd) = setup_repo("grep-bin", &[]);
    // A file with a NUL byte in the first 1KB is treated as binary.
    fs::write(dir.join("bin.dat"), b"hello\x00world").unwrap();
    let result = grep_files(
        "hello",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions::default(),
    )
    .unwrap();
    assert_eq!(result.text, "No matches found");
}

#[test]
fn grep_rel_path_format_when_directory() {
    let (dir, cwd) = setup_repo("grep-rel", &[("sub/a.txt", "needle")]);
    let result = grep_files(
        "needle",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions::default(),
    )
    .unwrap();
    // Directory search: paths relativized against the search root.
    assert!(result.text.contains("sub/a.txt:1:"), "{}", result.text);
}

#[test]
fn find_result_defaults() {
    let result = FindResult::default();
    assert_eq!(result.text, "");
    assert!(result.result_limit_reached.is_none());
    assert!(result.truncation_max_bytes.is_none());
    let result = GrepResult::default();
    assert!(!result.lines_truncated);
}

// --- cancellation -------------------------------------------------------------

/// An abort observed during a long scan rejects the whole call (upstream
/// `signal.addEventListener("abort", ...)` → `Operation aborted`) instead of
/// answering a partial match list. The file is large enough that the abort
/// lands mid-scan on any machine where a debug scan takes longer than 10 ms.
#[test]
fn grep_aborts_during_a_long_scan() {
    let mut content = String::with_capacity(800_000 * 4);
    for index in 0..800_000 {
        content.push_str(&format!("line {index}\n"));
    }
    let (dir, cwd) = setup_repo("grep-abort", &[("big.txt", &content)]);
    let signal = pillar_agent::abort::AbortSignal::new();
    let killer = signal.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(10));
        killer.abort();
    });

    let error = grep_files(
        "no-such-needle",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions {
            signal: Some(signal),
            ..Default::default()
        },
    )
    .expect_err("the aborted call rejects");
    assert_eq!(error, "Operation aborted");
}

/// An already-aborted signal stops the walk before it starts.
#[test]
fn find_aborts_before_walking() {
    let (dir, cwd) = setup_repo("find-abort", &[("a.ts", "x")]);
    let signal = pillar_agent::abort::AbortSignal::new();
    signal.abort();
    let error = find_files(
        "*.ts",
        &dir.to_string_lossy(),
        &cwd,
        FindOptions {
            signal: Some(signal),
            ..Default::default()
        },
    )
    .expect_err("the aborted call rejects");
    assert_eq!(error, "Operation aborted");
}

/// The `path:line: text` lines a result rendered (context lines use `-line-`,
/// the notices carry no line number, so a match line is the one whose text
/// after the first `:` starts with a digit).
fn match_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| {
            line.split_once(':')
                .is_some_and(|(_, rest)| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
        .map(str::to_string)
        .collect()
}

/// With context, each match renders its own window — overlapping windows repeat
/// the shared lines and the window is clamped at the file's edges, exactly as
/// upstream's `start..=end` rendering does. The scan captures the window while
/// it passes (no whole-file snapshot, no re-read), so this pins the contract
/// the capture has to reproduce.
#[test]
fn grep_context_window_is_per_match_and_clamped_at_the_edges() {
    let (dir, cwd) = setup_repo(
        "grep-ctx-window",
        &[("a.txt", "one\ntwo\nhit\nhit\nfive\nsix\n")],
    );
    let result = grep_files(
        "hit",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions {
            context: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        result.text,
        "a.txt-1- one\na.txt-2- two\na.txt:3: hit\na.txt-4- hit\na.txt-5- five\n\
         a.txt-2- two\na.txt-3- hit\na.txt:4: hit\na.txt-5- five\na.txt-6- six"
    );

    // A match on the first line has no `before` side, and a file shorter than
    // the window is rendered whole.
    let (dir, cwd) = setup_repo("grep-ctx-edge", &[("b.txt", "hit\nb\nc")]);
    let result = grep_files(
        "hit",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions {
            context: 5,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.text, "b.txt:1: hit\nb.txt-2- b\nb.txt-3- c");
}

/// Reaching the match limit must not cut the trailing context of the last match
/// found: the scan stops *collecting* at the limit but reads on until the open
/// window is complete (policy review sb39f R5; before, the search stopped on the
/// second hit and the first one lost its `after` lines).
#[test]
fn grep_keeps_trailing_context_when_the_limit_is_reached() {
    let (dir, cwd) = setup_repo("grep-ctx-limit", &[("a.txt", "hit\nhit\nthree\nfour\n")]);
    let result = grep_files(
        "hit",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions {
            context: 3,
            limit: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.match_limit_reached, Some(1));
    let matches = match_lines(&result.text);
    assert_eq!(matches, ["a.txt:1: hit"], "{}", result.text);
    assert!(result.text.contains("a.txt-2- hit"), "{}", result.text);
    assert!(
        result.text.contains("a.txt-3- three") && result.text.contains("a.txt-4- four"),
        "the trailing context is complete: {}",
        result.text
    );
}

/// The limit is a hard bound however the parallel walk interleaves: each worker
/// appends only what still fits under the lock (before, every worker compared
/// against the count it saw when it started and then appended unconditionally,
/// so concurrent files could push the total past the limit).
#[test]
fn grep_limit_bounds_the_parallel_walk() {
    let files: Vec<(String, String)> = (0..60)
        .map(|index| (format!("f{index}.txt"), "hit here\n".to_string()))
        .collect();
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_str()))
        .collect();
    let (dir, cwd) = setup_repo("grep-hard-limit", &files);
    // Repeated: the old accounting was a race (each worker compared against the
    // count it saw when it started), so one run could pass by luck.
    for _ in 0..10 {
        for limit in [1usize, 2, 7] {
            let result = grep_files(
                "hit",
                &dir.to_string_lossy(),
                &cwd,
                GrepOptions {
                    limit,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                match_lines(&result.text).len(),
                limit,
                "limit {limit}: {}",
                result.text
            );
            assert_eq!(result.match_limit_reached, Some(limit));
        }
    }
}

/// A pathological line inside a window is cut when it is captured, so a match
/// never holds several copies of a huge line, and the output still reports the
/// truncation.
#[test]
fn grep_budgets_a_huge_context_line() {
    let long = "z".repeat(80_000);
    let content = format!("before\n{long}\nhit\nafter\n");
    let (dir, cwd) = setup_repo("grep-huge-context", &[("a.txt", &content)]);
    let result = grep_files(
        "hit",
        &dir.to_string_lossy(),
        &cwd,
        GrepOptions {
            context: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(result.lines_truncated, "{}", result.text);
    assert!(
        result.text.contains("Some lines truncated to 500 chars"),
        "{}",
        result.text
    );
    for line in result.text.lines() {
        assert!(
            line.len() < 1000,
            "every rendered line is within the budget: {} chars",
            line.len()
        );
    }
}
