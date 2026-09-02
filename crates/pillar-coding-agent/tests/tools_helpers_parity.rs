//! Parity tests for tools/render-utils.ts + tools/file-mutation-queue.ts
//! (pi v0.84.3): tool output text shaping and same-file mutation
//! serialization.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use pillar_coding_agent::core::tools::file_mutation_queue::FileMutationQueue;
use pillar_coding_agent::core::tools::render_utils::{
    ToolResultBlock, coerce_str, get_text_output, normalize_display_text, replace_tabs,
    shorten_path,
};

// --- shortenPath -----------------------------------------------------------------

#[test]
fn shorten_path_collapses_home_prefix() {
    unsafe { std::env::set_var("HOME", "/home/tester") };
    assert_eq!(
        shorten_path("/home/tester/proj/file.txt"),
        "~/proj/file.txt"
    );
    // Non-home paths pass through.
    assert_eq!(shorten_path("/var/log/x"), "/var/log/x");
}

// --- str coercion --------------------------------------------------------------------

#[test]
fn coerce_str_follows_upstream_semantics() {
    assert_eq!(
        coerce_str(Some(&serde_json::json!("text"))),
        Some("text".to_string())
    );
    assert_eq!(
        coerce_str(Some(&serde_json::Value::Null)),
        Some(String::new())
    );
    assert_eq!(coerce_str(None), Some(String::new()));
    // Non-string non-null values are invalid args.
    assert_eq!(coerce_str(Some(&serde_json::json!(42))), None);
}

// --- text shaping ----------------------------------------------------------------------

#[test]
fn replace_tabs_with_three_spaces() {
    assert_eq!(replace_tabs("a\tb"), "a   b");
    assert_eq!(replace_tabs("no tabs"), "no tabs");
}

#[test]
fn normalize_display_text_strips_carriage_returns() {
    assert_eq!(normalize_display_text("a\r\nb\rc"), "a\nbc");
}

// --- getTextOutput -----------------------------------------------------------------------

#[test]
fn get_text_output_joins_text_blocks_sanitized() {
    let blocks = vec![
        ToolResultBlock::Text("\x1b[31mred\r\n".to_string()),
        ToolResultBlock::Text("second\tline".to_string()),
    ];
    let output = get_text_output(&blocks, false);
    // Block 1 keeps its trailing newline; the join adds the separator.
    // (Tabs are not replaced by getTextOutput; replaceTabs is separate.)
    assert_eq!(output, "red\n\nsecond\tline");
}

#[test]
fn get_text_output_appends_image_indicators() {
    let blocks = vec![
        ToolResultBlock::Text("output".to_string()),
        ToolResultBlock::Image {
            data: "abc".to_string(),
            mime_type: "image/png".to_string(),
        },
    ];
    let output = get_text_output(&blocks, false);
    assert_eq!(output, "output\n[image: image/png]");

    // Empty text with image only -> just the indicator.
    let blocks = vec![ToolResultBlock::Image {
        data: "abc".to_string(),
        mime_type: "image/jpeg".to_string(),
    }];
    assert_eq!(get_text_output(&blocks, false), "[image: image/jpeg]");
}

#[test]
fn get_text_output_empty_result_is_empty() {
    assert_eq!(get_text_output(&[], false), "");
}

// --- file mutation queue ----------------------------------------------------------------------

#[test]
fn mutation_queue_serializes_same_file_mutations() {
    let queue = Arc::new(FileMutationQueue::new());
    let counter = Arc::new(AtomicU32::new(0));
    let overlap = Arc::new(AtomicU32::new(0));
    let max_overlap = Arc::new(AtomicU32::new(0));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let queue_ref = queue.clone();
        let counter = counter.clone();
        let overlap = overlap.clone();
        let max_overlap = max_overlap.clone();
        handles.push(std::thread::spawn(move || {
            queue_ref
                .with_file_mutation_queue(std::path::Path::new("/tmp/shared.txt"), || {
                    let now = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    overlap.store(now, Ordering::SeqCst);
                    // Track the highest concurrent count inside the critical section.
                    max_overlap.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    counter.fetch_sub(1, Ordering::SeqCst);
                })
                .unwrap();
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(
        max_overlap.load(Ordering::SeqCst),
        1,
        "same-file mutations never overlap"
    );
    let _ = overlap;
}

#[test]
fn mutation_queue_different_files_are_independent() {
    let queue = Arc::new(FileMutationQueue::new());
    let start = std::time::Instant::now();
    let mut handles = Vec::new();
    for i in 0..2 {
        let queue = queue.clone();
        handles.push(std::thread::spawn(move || {
            queue
                .with_file_mutation_queue(
                    std::path::Path::new(&format!("/tmp/independent-{i}.txt")),
                    || {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    },
                )
                .unwrap();
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    // Both 50ms sleeps ran concurrently: well under the serialized 100ms.
    assert!(
        start.elapsed() < std::time::Duration::from_millis(95),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn mutation_queue_equivalent_paths_share_the_queue() {
    // Lexically equivalent paths (with ./ segments) serialize together.
    let queue = Arc::new(FileMutationQueue::new());
    let counter = Arc::new(AtomicU32::new(0));
    let max_overlap = Arc::new(AtomicU32::new(0));
    let mut handles = Vec::new();
    for path in ["/tmp/eq.txt", "/tmp/./eq.txt"] {
        let queue = queue.clone();
        let counter = counter.clone();
        let max_overlap = max_overlap.clone();
        handles.push(std::thread::spawn(move || {
            queue
                .with_file_mutation_queue(std::path::Path::new(path), || {
                    let now = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    max_overlap.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    counter.fetch_sub(1, Ordering::SeqCst);
                })
                .unwrap();
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(max_overlap.load(Ordering::SeqCst), 1);
}
