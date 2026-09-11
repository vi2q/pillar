//! Parity tests for built-in tool factories (upstream
//! packages/coding-agent/src/core/tools/read.ts `createReadTool`).

use pillar_agent::types::{AgentTool, AgentToolResult, ToolExecuteError};
use pillar_ai::types::Content;
use pillar_coding_agent::core::tools::edit::edit_tool;
use pillar_coding_agent::core::tools::ls::ls_tool;
use pillar_coding_agent::core::tools::read::read_tool;
use pillar_coding_agent::core::tools::write::write_tool;
use serde_json::{Value, json};

fn temp_dir(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pillar-tool-factory-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn run(tool: &AgentTool, args: Value) -> Result<AgentToolResult, ToolExecuteError> {
    (tool.execute)("call-1".to_string(), args, None, None).await
}

fn text_of(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn read_tool_exposes_name_description_and_parameters() {
    let tool = read_tool(".");
    assert_eq!(tool.tool.name, "read");
    assert_eq!(tool.label, "read");
    assert!(!tool.tool.description.is_empty());
    let parameters = tool.tool.parameters.as_object().expect("object schema");
    assert_eq!(
        parameters.get("type").and_then(Value::as_str),
        Some("object")
    );
    assert!(parameters["properties"].get("path").is_some());
}

#[tokio::test]
async fn read_tool_reads_a_text_file() {
    let dir = temp_dir("read");
    std::fs::write(dir.join("note.txt"), "hello\nworld\n").unwrap();
    let tool = read_tool(&dir.to_string_lossy());

    let result = run(&tool, json!({ "path": "note.txt" }))
        .await
        .expect("read succeeds");
    assert_eq!(text_of(&result), "hello\nworld\n");
    assert_eq!(result.details, Value::Null);
}

#[tokio::test]
async fn read_tool_honors_offset_and_limit_with_continuation_notice() {
    let dir = temp_dir("read-window");
    std::fs::write(dir.join("note.txt"), "a\nb\nc\nd\n").unwrap();
    let tool = read_tool(&dir.to_string_lossy());

    let result = run(
        &tool,
        json!({ "path": "note.txt", "offset": 2, "limit": 2 }),
    )
    .await
    .expect("read succeeds");
    let text = text_of(&result);
    assert!(text.starts_with("b\nc"), "{text}");
    assert!(
        text.contains("more lines in file. Use offset=4 to continue."),
        "{text}"
    );
    // A user limit short of the file end is not head truncation, so there are
    // no truncation details (upstream sets `details` only for truncation).
    assert_eq!(result.details, Value::Null);
}

#[tokio::test]
async fn read_tool_reports_head_truncation_details() {
    let dir = temp_dir("read-truncation");
    let content = (1..=2100)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.join("big.txt"), content).unwrap();
    let tool = read_tool(&dir.to_string_lossy());

    let result = run(&tool, json!({ "path": "big.txt" }))
        .await
        .expect("read succeeds");
    let text = text_of(&result);
    assert!(text.contains("Use offset="), "{text}");
    let truncation = &result.details["truncation"];
    assert_eq!(truncation["truncated"], json!(true));
    assert_eq!(truncation["truncatedBy"], json!("lines"));
    assert_eq!(truncation["totalLines"], json!(2100));
    assert_eq!(truncation["maxLines"], json!(2000));
}

#[tokio::test]
async fn read_tool_returns_base64_image_content() {
    let dir = temp_dir("read-image");
    // Minimal PNG signature; the port reads image bytes and returns them as
    // base64 content (upstream additionally processes/resizes).
    let png = [0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    std::fs::write(dir.join("pixel.png"), png).unwrap();
    let tool = read_tool(&dir.to_string_lossy());

    let result = run(&tool, json!({ "path": "pixel.png" }))
        .await
        .expect("read succeeds");
    assert_eq!(result.content.len(), 2, "{:?}", result.content);
    assert!(matches!(result.content[0], Content::Text { .. }));
    match &result.content[1] {
        Content::Image { data, mime_type } => {
            assert_eq!(mime_type, "image/png");
            assert_eq!(data, "iVBORw0KGgo=");
        }
        other => panic!("expected image content, got {other:?}"),
    }
}

#[tokio::test]
async fn read_tool_rejects_missing_path() {
    let tool = read_tool(".");
    let error = run(&tool, json!({})).await.expect_err("missing path");
    assert!(
        error.0.contains("Missing required parameter: path"),
        "{error:?}"
    );
}

#[tokio::test]
async fn read_tool_reports_missing_file() {
    let dir = temp_dir("read-missing");
    let tool = read_tool(&dir.to_string_lossy());
    let error = run(&tool, json!({ "path": "nope.txt" }))
        .await
        .expect_err("missing file");
    assert!(error.0.contains("File not found"), "{error:?}");
}

#[tokio::test]
async fn ls_tool_lists_directories_with_suffixes() {
    let dir = temp_dir("ls");
    std::fs::write(dir.join("b.txt"), "b").unwrap();
    std::fs::create_dir_all(dir.join("a-dir")).unwrap();
    let tool = ls_tool(&dir.to_string_lossy());

    let result = run(&tool, json!({ "path": "." }))
        .await
        .expect("ls succeeds");
    let text = text_of(&result);
    assert_eq!(text, "a-dir/\nb.txt");
    assert_eq!(result.details, json!({}));
}

#[tokio::test]
async fn ls_tool_reports_entry_limit() {
    let dir = temp_dir("ls-limit");
    for index in 0..4 {
        std::fs::write(dir.join(format!("f{index}.txt")), "x").unwrap();
    }
    let tool = ls_tool(&dir.to_string_lossy());

    let result = run(&tool, json!({ "path": ".", "limit": 2 }))
        .await
        .expect("ls succeeds");
    assert_eq!(result.details["entryLimitReached"], json!(2));
    assert!(text_of(&result).contains("2 entries limit reached. Use limit=4 for more"));
}

#[tokio::test]
async fn write_tool_creates_a_file() {
    let dir = temp_dir("write");
    let tool = write_tool(&dir.to_string_lossy());

    let result = run(&tool, json!({ "path": "sub/out.txt", "content": "hello" }))
        .await
        .expect("write succeeds");
    assert_eq!(
        text_of(&result),
        "Successfully wrote 5 bytes to sub/out.txt"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("sub/out.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn write_tool_rejects_missing_content() {
    let dir = temp_dir("write-missing");
    let tool = write_tool(&dir.to_string_lossy());
    let error = run(&tool, json!({ "path": "out.txt" }))
        .await
        .expect_err("missing content");
    assert!(
        error.0.contains("Missing required parameter: content"),
        "{error:?}"
    );
}

#[tokio::test]
async fn edit_tool_replaces_text_and_reports_diff_details() {
    let dir = temp_dir("edit");
    std::fs::write(dir.join("note.txt"), "alpha\nbeta\n").unwrap();
    let tool = edit_tool(&dir.to_string_lossy());

    let result = run(
        &tool,
        json!({
            "path": "note.txt",
            "edits": [{ "oldText": "beta", "newText": "gamma" }],
        }),
    )
    .await
    .expect("edit succeeds");
    assert_eq!(
        text_of(&result),
        "Successfully replaced 1 block(s) in note.txt."
    );
    assert!(
        result.details["diff"]
            .as_str()
            .unwrap_or_default()
            .contains("gamma")
    );
    assert!(
        result.details["patch"]
            .as_str()
            .unwrap_or_default()
            .contains("---")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("note.txt")).unwrap(),
        "alpha\ngamma\n"
    );
}

#[tokio::test]
async fn edit_tool_rejects_empty_edits() {
    let dir = temp_dir("edit-empty");
    std::fs::write(dir.join("note.txt"), "alpha\n").unwrap();
    let tool = edit_tool(&dir.to_string_lossy());
    let error = run(&tool, json!({ "path": "note.txt", "edits": [] }))
        .await
        .expect_err("empty edits");
    assert!(error.0.contains("at least one replacement"), "{error:?}");
}
