//! Parity tests for tools/read.ts (pi v0.84.3): offset/limit windows,
//! truncation notices with continuation offsets, user-limit notices, the
//! oversized-first-line bash fallback, and image detection.

use std::fs;
use std::path::PathBuf;

use pillar_coding_agent::core::tools::read::{read, read_description, read_parameters_json};
use pillar_coding_agent::core::truncate::DEFAULT_MAX_BYTES;

fn setup_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-read-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn description_matches_upstream_wording() {
    let description = read_description();
    assert!(description.starts_with("Read the contents of a file."));
    assert!(description.contains("jpg, png, gif, webp, bmp"));
    assert!(description.contains("2000 lines"));
    assert!(description.contains("50KB"));
    assert!(description.contains("Use offset/limit for large files."));
    assert!(
        description.contains("When you need the full file, continue with offset until complete.")
    );
}

#[test]
fn parameters_schema_matches_upstream_shape() {
    let schema = read_parameters_json();
    assert_eq!(schema["properties"]["path"]["type"], "string");
    assert_eq!(
        schema["properties"]["offset"]["description"],
        "Line number to start reading from (1-indexed)"
    );
    assert_eq!(
        schema["properties"]["limit"]["description"],
        "Maximum number of lines to read"
    );
    assert_eq!(schema["required"], serde_json::json!(["path"]));
}

#[test]
fn read_whole_file() {
    let dir = setup_dir("whole");
    let path = dir.join("f.txt");
    fs::write(&path, "line1\nline2\nline3").unwrap();
    let result = read(path.to_str().unwrap(), None, None, "/tmp").unwrap();
    assert_eq!(result.text, "line1\nline2\nline3");
    assert!(!result.truncated);
    assert_eq!(result.total_file_lines, 3);
    assert!(result.image.is_none());
}

#[test]
fn read_offset_is_one_indexed() {
    let dir = setup_dir("offset");
    let path = dir.join("f.txt");
    fs::write(&path, "one\ntwo\nthree\nfour").unwrap();
    // offset=2 starts at "two".
    let result = read(path.to_str().unwrap(), Some(2), None, "/tmp").unwrap();
    assert_eq!(result.text, "two\nthree\nfour");
}

#[test]
fn read_limit_stops_early_with_continuation_notice() {
    let dir = setup_dir("limit");
    let path = dir.join("f.txt");
    fs::write(&path, "one\ntwo\nthree\nfour").unwrap();
    let result = read(path.to_str().unwrap(), None, Some(2), "/tmp").unwrap();
    assert_eq!(
        result.text,
        "one\ntwo\n\n[2 more lines in file. Use offset=3 to continue.]"
    );
}

#[test]
fn read_offset_plus_limit_window() {
    let dir = setup_dir("window");
    let path = dir.join("f.txt");
    fs::write(&path, "one\ntwo\nthree\nfour\nfive").unwrap();
    let result = read(path.to_str().unwrap(), Some(2), Some(2), "/tmp").unwrap();
    assert_eq!(
        result.text,
        "two\nthree\n\n[2 more lines in file. Use offset=4 to continue.]"
    );
}

#[test]
fn read_offset_beyond_end_errors() {
    let dir = setup_dir("beyond");
    let path = dir.join("f.txt");
    fs::write(&path, "one\ntwo").unwrap();
    let error = read(path.to_str().unwrap(), Some(10), None, "/tmp").unwrap_err();
    assert_eq!(error, "Offset 10 is beyond end of file (2 lines total)");
}

#[test]
fn read_line_truncation_adds_continuation_notice() {
    let dir = setup_dir("line-trunc");
    // Exceeds the 2000-line default limit.
    let mut content = String::new();
    for i in 0..2500 {
        content.push_str(&format!("line {i}\n"));
    }
    let path = dir.join("f.txt");
    fs::write(&path, &content).unwrap();
    let result = read(path.to_str().unwrap(), None, None, "/tmp").unwrap();
    assert!(result.truncated);
    // The trailing newline yields an extra empty final split entry, exactly
    // like upstream's textContent.split("\n").
    assert_eq!(result.total_file_lines, 2501);
    // Notice: [Showing lines 1-2000 of 2501. Use offset=2001 to continue.]
    assert!(
        result
            .text
            .contains("[Showing lines 1-2000 of 2501. Use offset=2001 to continue.]"),
        "{}",
        result.text
    );
    // Content keeps the first 2000 lines.
    assert!(result.text.starts_with("line 0\n"));
    assert!(result.text.contains("line 1999\n"));
    assert!(!result.text.contains("line 2000\n"));
}

#[test]
fn read_byte_truncation_notice_format() {
    let dir = setup_dir("byte-trunc");
    // Lines sized so the byte limit hits before the line limit.
    let line = "x".repeat(200);
    let content = vec![line.as_str(); 400].join("\n"); // 80KB > 50KB
    let path = dir.join("f.txt");
    fs::write(&path, &content).unwrap();
    let result = read(path.to_str().unwrap(), None, None, "/tmp").unwrap();
    assert!(result.truncated);
    assert!(
        result.text.contains(&format!(
            "[Showing lines 1-{} of 400 (50.0KB limit). Use offset={} to continue.]",
            result.text.lines().count().saturating_sub(2),
            result.text.lines().count().saturating_sub(1)
        )),
        "{}",
        result.text
    );
}

#[test]
fn read_oversized_first_line_suggests_bash() {
    let dir = setup_dir("big-line");
    // One line exceeding the 50KB byte limit.
    let line = "x".repeat(DEFAULT_MAX_BYTES + 100);
    let path = dir.join("f.txt");
    fs::write(&path, &line).unwrap();
    let result = read(path.to_str().unwrap(), None, None, "/tmp").unwrap();
    assert!(result.truncated);
    let first_line_size = line.len();
    assert!(
        result.text.starts_with(&format!(
            "[Line 1 is {}, exceeds 50.0KB limit. Use bash: sed -n '1p'",
            pillar_coding_agent::core::truncate::format_size(first_line_size)
        )),
        "{}",
        result.text
    );
    assert!(result.text.contains("| head -c 51200]"));
}

#[test]
fn read_image_files_return_attachment_notes() {
    let dir = setup_dir("image");
    // A minimal PNG header.
    let png = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    fs::write(dir.join("img.png"), png).unwrap();
    let result = read(dir.join("img.png").to_str().unwrap(), None, None, "/tmp").unwrap();
    assert_eq!(result.text, "Read image file [image/png]");
    assert!(result.image.is_some());
    assert_eq!(result.image.as_ref().unwrap().mime_type, "image/png");
    assert_eq!(result.image.as_ref().unwrap().data, png);

    // JPEG extension.
    fs::write(dir.join("img.jpg"), b"jpegdata").unwrap();
    let result = read(dir.join("img.jpg").to_str().unwrap(), None, None, "/tmp").unwrap();
    assert_eq!(result.image.as_ref().unwrap().mime_type, "image/jpeg");
}

#[test]
fn read_missing_file_errors() {
    let error = read("/definitely/not/real.txt", None, None, "/tmp").unwrap_err();
    assert!(error.contains("File not found"), "{error}");
}

#[test]
fn read_macos_variant_fallback_finds_curly_quote_names() {
    let dir = setup_dir("curly");
    // File stored with U+2019; user types a straight apostrophe.
    fs::write(dir.join("l\u{2019}été.txt"), "content").unwrap();
    let result = read(dir.join("l'été.txt").to_str().unwrap(), None, None, "/tmp").unwrap();
    assert_eq!(result.text, "content");
}
