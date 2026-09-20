//! The search contract's structural corpus (docs/DEVELOPMENT-STRATEGY.md §6):
//! a fixed generated corpus (parameters printed as its identity), searched on
//! every run.
//!
//! The printed timings are the input for the release baseline
//! (`cargo test --release -p pillar-coding-agent --test search_corpus --
//! --nocapture`); the assertions are generous structural guards — a debug
//! build is far slower than release, so no assertion here is a performance SLO.
//! The result counts are assertions: they catch a search that silently drops
//! matches (limits, batching, context) rather than getting slower.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pillar_coding_agent::core::tools::search::{FindOptions, GrepOptions, find_files, grep_files};

const CORPUS_FILES: usize = 1000;
const CORPUS_LINES: usize = 40;
const CORPUS_LINE_LEN: usize = 80;
/// Every n-th file contains the needle (exactly once).
const MATCH_EVERY: usize = 10;
/// The single large file keeps the needle on its last line.
const BIG_FILE_LINES: usize = 200_000;
/// A large file where every n-th line matches, searched *with context*: the
/// case where a per-match whole-file snapshot (or an unbounded context window)
/// turns the search quadratic.
const DENSE_FILE_LINES: usize = 200_000;
const DENSE_EVERY: usize = 40;

/// The corpus identity a baseline record must carry alongside the numbers.
fn corpus_id() -> String {
    format!(
        "files={CORPUS_FILES},lines={CORPUS_LINES},line_len={CORPUS_LINE_LEN},\
         match_every={MATCH_EVERY},big_lines={BIG_FILE_LINES},\
         dense_lines={DENSE_FILE_LINES},dense_every={DENSE_EVERY}"
    )
}

fn build_corpus() -> (PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("pillar-search-corpus-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let filler = "filler text that repeats without matching anything at all. ".repeat(2);
    for index in 0..CORPUS_FILES {
        let sub = dir.join(format!("pkg{}", index % 10));
        fs::create_dir_all(&sub).unwrap();
        let mut content = String::with_capacity(CORPUS_LINES * CORPUS_LINE_LEN);
        for line in 0..CORPUS_LINES {
            if index % MATCH_EVERY == 0 && line == CORPUS_LINES / 2 {
                content.push_str("needle 42 here\n");
            } else {
                content.push_str(&filler[..CORPUS_LINE_LEN.min(filler.len())]);
                content.push('\n');
            }
        }
        let extension = if index % 3 == 0 { "md" } else { "txt" };
        fs::write(sub.join(format!("file{index}.{extension}")), content).unwrap();
    }
    let big = dir.join("big.txt");
    let mut content = String::with_capacity(BIG_FILE_LINES * 24);
    for index in 0..BIG_FILE_LINES - 1 {
        content.push_str(&format!("line {index}\n"));
    }
    content.push_str("needle at the end\n");
    fs::write(big, content).unwrap();
    let dense = dir.join("dense.txt");
    let mut content = String::with_capacity(DENSE_FILE_LINES * 32);
    for index in 0..DENSE_FILE_LINES {
        if index % DENSE_EVERY == 0 {
            content.push_str(&format!("needle dense {index}\n"));
        } else {
            content.push_str(&format!("filler line {index}\n"));
        }
    }
    fs::write(dense, content).unwrap();
    let cwd = dir.to_string_lossy().to_string();
    (dir, cwd)
}

/// Peak resident set (KB) of this process, when the platform reports it.
fn peak_rss_kb() -> Option<i64> {
    #[cfg(unix)]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `usage` is a valid out-parameter.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
            // Linux reports KB, macOS bytes.
            let raw = usage.ru_maxrss;
            return Some(if cfg!(target_os = "macos") {
                raw / 1024
            } else {
                raw
            });
        }
        None
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn timed(label: &str, run: impl FnOnce() -> usize) -> (String, usize, Duration) {
    let started = Instant::now();
    let count = run();
    let elapsed = started.elapsed();
    (label.to_string(), count, elapsed)
}

#[test]
fn measure_search_corpus() {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let (dir, cwd) = build_corpus();
    let root = dir.to_string_lossy().to_string();
    let files = CORPUS_FILES + 2;

    let expect_hits = CORPUS_FILES.div_ceil(MATCH_EVERY);
    // `.txt` files are the ones whose index is not a multiple of three, plus the
    // two standalone `.txt` files (big / dense); the glob-filtered hits are the
    // needle files that are `.md`.
    let expect_txt = (0..CORPUS_FILES).filter(|index| index % 3 != 0).count() + 2;
    let expect_md_hits = (0..CORPUS_FILES)
        .filter(|index| index % MATCH_EVERY == 0 && index % 3 == 0)
        .count();
    let dense_hits = DENSE_FILE_LINES.div_ceil(DENSE_EVERY);
    let rss_before_dense = peak_rss_kb();
    let cases = vec![
        timed("find *.txt", || {
            let result = find_files(
                "*.txt",
                &root,
                &cwd,
                FindOptions {
                    limit: 10_000,
                    ..Default::default()
                },
            )
            .expect("find");
            result.text.lines().count()
        }),
        timed("grep literal needle", || {
            let result = grep_files(
                "needle",
                &root,
                &cwd,
                GrepOptions {
                    literal: true,
                    limit: 10_000,
                    ..Default::default()
                },
            )
            .expect("grep");
            result.text.matches("needle").count()
        }),
        timed("grep regex needle\\s?[0-9]+", || {
            let result = grep_files(
                r"needle\s?[0-9]+",
                &root,
                &cwd,
                GrepOptions {
                    limit: 10_000,
                    ..Default::default()
                },
            )
            .expect("grep");
            result
                .text
                .lines()
                .filter(|line| line.contains("needle"))
                .count()
        }),
        timed("grep no-hit", || {
            let result = grep_files(
                "zzz-no-such-needle-zzz",
                &root,
                &cwd,
                GrepOptions::default(),
            )
            .expect("grep");
            result.text.lines().count()
        }),
        timed("grep glob *.md context 2", || {
            let result = grep_files(
                "needle",
                &root,
                &cwd,
                GrepOptions {
                    glob: Some("*.md".to_string()),
                    context: 2,
                    limit: 10_000,
                    ..Default::default()
                },
            )
            .expect("grep");
            result
                .text
                .lines()
                .filter(|line| line.contains("needle"))
                .count()
        }),
        timed("grep big.txt (200k lines)", || {
            let result = grep_files(
                "needle at the end",
                &dir.join("big.txt").to_string_lossy(),
                &cwd,
                GrepOptions {
                    literal: true,
                    ..Default::default()
                },
            )
            .expect("grep");
            result.text.matches("needle at the end").count()
        }),
        // Many hits *with* context: each match carries a bounded window, so this
        // costs the number of renders, not (lines × hits) copies.
        timed("grep dense.txt 200k lines ctx=2", || {
            let result = grep_files(
                "needle dense",
                &dir.join("dense.txt").to_string_lossy(),
                &cwd,
                GrepOptions {
                    context: 2,
                    limit: dense_hits,
                    ..Default::default()
                },
            )
            .expect("grep");
            result
                .text
                .lines()
                .filter(|line| line.contains("needle"))
                .count()
        }),
    ];
    let dense_rss = peak_rss_kb();
    let dense_elapsed = cases
        .iter()
        .find(|(label, _, _)| label.starts_with("grep dense"))
        .map(|(_, _, elapsed)| *elapsed)
        .unwrap_or_default();

    println!(
        "search corpus: {} profile={profile} files={files} rss={:?}kB",
        corpus_id(),
        peak_rss_kb()
    );
    for (label, count, elapsed) in &cases {
        println!(
            "  {label:32} {count:6} results  {:8.1} ms",
            elapsed.as_secs_f64() * 1000.0
        );
    }

    // Contract guards: the hits are found (not dropped by batching or limits),
    // and the corpus search stays within a generous structural bound.
    assert_eq!(cases[0].1, expect_txt, "find lists every .txt file");
    assert!(
        cases[1].1 >= expect_hits,
        "literal grep found {} matches, expected at least {expect_hits}",
        cases[1].1
    );
    assert_eq!(cases[2].1, expect_hits, "regex grep over the corpus");
    assert_eq!(cases[3].1, 1, "the no-hit message is one line");
    assert_eq!(cases[4].1, expect_md_hits, "glob-filtered context grep");
    assert_eq!(cases[5].1, 1, "the needle at the end of the big file");
    assert!(
        cases[6].1 > 100,
        "the dense context search renders many hits ({}): the output byte \
         budget truncates the render, not the match collection",
        cases[6].1
    );
    // Structural guard on the *retention*: a window per match stays proportional
    // to the render, so this must not look like the file being held per hit.
    assert!(
        dense_elapsed < Duration::from_secs(60),
        "the dense context search took {dense_elapsed:?} (structural guard: a \
         per-match file snapshot makes this quadratic)"
    );
    if let (Some(before), Some(after)) = (rss_before_dense, dense_rss) {
        println!("  dense context search peak RSS {before} -> {after} kB");
    }
    let total: Duration = cases.iter().map(|(_, _, elapsed)| *elapsed).sum();
    assert!(
        total < Duration::from_secs(120),
        "the corpus pass took {total:?} (structural guard, not an SLO)"
    );
}
