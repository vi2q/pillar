//! The read tool's windowed-read corpus (docs/DEVELOPMENT-STRATEGY.md §6): a
//! generated large text file read with `offset`/`limit`.
//!
//! The printed numbers feed the baseline record
//! (`cargo test --release -p pillar-coding-agent --test read_corpus --
//! --nocapture`). The assertions are structural, not SLOs: the windowed read
//! must stay bounded in memory — it used to materialize the whole file plus a
//! slice per line — while reporting the whole-file counts.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pillar_coding_agent::core::tools::read::read;

/// ~16 MB of lines: the file a windowed read must not materialize.
const FILE_LINES: usize = 200_000;
const LINE_LEN: usize = 80;
/// The window asked for, and the memory the read may add on top of the peak.
const WINDOW_LIMIT: usize = 50;
const ALLOWED_RSS_GROWTH_KB: i64 = 8 * 1024;

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

/// Write the corpus with a bounded buffer: building it must not raise the peak
/// the measurement below reads.
fn build_corpus(dir: &Path, lines: usize, line_len: usize) -> PathBuf {
    fs::create_dir_all(dir).expect("corpus dir");
    let path = dir.join("big.txt");
    let filler = "x".repeat(line_len.saturating_sub(17));
    let mut file = fs::File::create(&path).expect("create corpus");
    let mut buffer = String::with_capacity(1 << 16);
    for index in 0..lines {
        buffer.clear();
        buffer.push_str(&format!("L{index:07} {filler}\n"));
        file.write_all(buffer.as_bytes()).expect("write corpus");
    }
    file.flush().expect("flush corpus");
    path
}

#[test]
fn windowed_read_of_a_large_file_stays_bounded() {
    let dir = std::env::temp_dir().join(format!("pillar-read-corpus-{}", std::process::id()));
    let path = build_corpus(&dir, FILE_LINES, LINE_LEN);
    let bytes = fs::metadata(&path).expect("stat corpus").len();
    let cwd = dir.to_string_lossy().into_owned();
    let start_line = FILE_LINES / 2;
    let offset = start_line + 1; // 1-indexed

    let rss_before = peak_rss_kb();
    let started = Instant::now();
    let result = read("big.txt", Some(offset), Some(WINDOW_LIMIT), &cwd).expect("windowed read");
    let elapsed = started.elapsed();
    let rss_after = peak_rss_kb();

    println!(
        "read corpus: lines={FILE_LINES} line_len={LINE_LEN} bytes={bytes} offset={offset} \
         limit={WINDOW_LIMIT} time={elapsed:?} rss={rss_before:?}kB -> {rss_after:?}kB"
    );

    // The window is the requested one; the counts are the whole file's.
    assert_eq!(result.total_file_lines, FILE_LINES + 1);
    assert!(!result.truncated, "a 50-line window is not truncated");
    assert!(
        result.text.starts_with(&format!("L{start_line:07} ")),
        "{}",
        &result.text[..result.text.len().min(80)]
    );
    let remaining = (FILE_LINES + 1) - (start_line + WINDOW_LIMIT);
    let next_offset = start_line + WINDOW_LIMIT + 1;
    assert!(
        result.text.ends_with(&format!(
            "[{remaining} more lines in file. Use offset={next_offset} to continue.]"
        )),
        "{}",
        &result.text[result.text.len().saturating_sub(120)..]
    );

    // Structural guard: the read may not keep the file (16 MB) nor a slice per
    // line (200k × 16 B) resident. A debug build is slow, so the time bound is
    // only a hang guard.
    assert!(elapsed < Duration::from_secs(60), "{elapsed:?}");
    if let (Some(before), Some(after)) = (rss_before, rss_after) {
        let growth = after - before;
        assert!(
            growth < ALLOWED_RSS_GROWTH_KB,
            "windowed read grew the peak RSS by {growth} kB (allowed {ALLOWED_RSS_GROWTH_KB} kB, \
             file is {bytes} bytes)"
        );
    }
}
