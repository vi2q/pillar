//! The experimental tools at their real boundary: JSON arguments in, an
//! `AgentToolResult` out (design §4.1, §9, §10.1 "実経路").
//!
//! These tests drive `AgentTool::execute`, so they cover what the model
//! actually sees: the schema-visible argument shapes, the compact `content`
//! (which must carry the reference the next call needs), the structured
//! `details`, and the error text with its repair hint.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pillar_agent::AbortSignal;
use pillar_agent::exp::{
    ExpLimits, ExpToolkit, MemoryHost, OperationLedger, OwnerId, RefStore, manual_clock,
};
use pillar_agent::types::AgentToolResult;
use serde_json::{Value, json};

fn toolkit(host: Arc<MemoryHost>) -> ExpToolkit {
    let limits = ExpLimits::default();
    let (clock, _now) = manual_clock(0);
    let next = Arc::new(AtomicU64::new(0));
    let ids: Arc<dyn Fn() -> String + Send + Sync> =
        Arc::new(move || format!("ref-{}", next.fetch_add(1, Ordering::SeqCst)));
    ExpToolkit::with_state(
        host,
        Arc::new(RefStore::with_id_source(clock, limits.max_live_refs, ids)),
        Arc::new(OperationLedger::new(limits.ledger_capacity)),
        limits,
        OwnerId::new("session-a"),
    )
}

fn text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|part| match part {
            pillar_ai::types::Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn execute(
    toolkit: &ExpToolkit,
    tool: &str,
    args: Value,
    signal: Option<AbortSignal>,
) -> Result<AgentToolResult, pillar_agent::types::ToolExecuteError> {
    let tools = toolkit.tools();
    let tool = tools
        .into_iter()
        .find(|candidate| candidate.tool.name == tool)
        .expect("tool exists");
    (tool.execute)("call-1".to_string(), args, signal, None).await
}

#[tokio::test]
async fn the_read_tool_hands_back_a_reference_the_edit_tool_accepts() {
    let host = Arc::new(MemoryHost::new());
    host.set_file("f.txt", b"one\ntwo\nthree\n");
    let toolkit = toolkit(Arc::clone(&host));

    let read = execute(
        &toolkit,
        "exp_read",
        json!({"path": "f.txt", "range": {"startLine": 2, "lineCount": 1}}),
        None,
    )
    .await
    .expect("read");
    let read_text = text(&read);
    assert!(read_text.starts_with("[exp_read f.txt 2-2/3 ref=ref-0"), "{read_text}");
    assert!(read_text.contains("\ntwo\n"), "{read_text}");
    let reference = read.details["reference"]
        .as_str()
        .expect("details carry the reference")
        .to_string();
    assert!(read_text.contains(&reference), "the model must see the reference");

    let edit = execute(
        &toolkit,
        "exp_edit",
        json!({
            "operationId": "op-1",
            "edits": [{"ref": reference, "replacement": "two\nand a half\n"}],
        }),
        None,
    )
    .await
    .expect("edit");
    let edit_text = text(&edit);
    assert!(edit_text.starts_with("[exp_edit f.txt applied 1 replacement(s)"), "{edit_text}");
    assert_eq!(
        host.content("f.txt").as_deref(),
        Some(&b"one\ntwo\nand a half\nthree\n"[..])
    );
    assert_eq!(edit.details["receipt"]["editsApplied"], json!(1));
}

#[tokio::test]
async fn the_read_tool_withholds_a_reference_it_cannot_make_editable() {
    let host = Arc::new(MemoryHost::new());
    host.set_file("f.txt", b"one\n\xff\ntwo\n");
    let toolkit = toolkit(Arc::clone(&host));

    let read = execute(
        &toolkit,
        "exp_read",
        json!({"path": "f.txt", "range": {"startLine": 1, "lineCount": 3}}),
        None,
    )
    .await
    .expect("read");
    let read_text = text(&read);
    assert!(read_text.contains("ref=-"), "{read_text}");
    assert!(read_text.contains("withheld=NotUtf8"), "{read_text}");
    assert!(read_text.contains("hint="), "{read_text}");
    assert_eq!(read.details["editable"], json!(false));
}

#[tokio::test]
async fn an_unknown_reference_comes_back_as_a_tool_error_with_a_repair() {
    let host = Arc::new(MemoryHost::new());
    host.set_file("f.txt", b"one\n");
    let toolkit = toolkit(Arc::clone(&host));

    let error = execute(
        &toolkit,
        "exp_edit",
        json!({"operationId": "op-1", "edits": [{"ref": "ref-nope", "replacement": "x"}]}),
        None,
    )
    .await
    .expect_err("unknown reference");
    assert!(error.0.contains("invalid_ref"), "{error}");
    assert!(error.0.contains("read the target again"), "{error}");
    assert_eq!(host.content("f.txt").as_deref(), Some(&b"one\n"[..]));
}

#[tokio::test]
async fn malformed_arguments_are_rejected_as_tool_errors() {
    let host = Arc::new(MemoryHost::new());
    host.set_file("f.txt", b"one\n");
    let toolkit = toolkit(Arc::clone(&host));

    let error = execute(&toolkit, "exp_read", json!({"path": "f.txt"}), None)
        .await
        .expect_err("missing range");
    assert!(error.0.contains("exp_read input"), "{error}");

    let error = execute(
        &toolkit,
        "exp_edit",
        json!({"operationId": "op-1", "edits": []}),
        None,
    )
    .await
    .expect_err("empty edits");
    assert!(error.0.contains("invalid_request"), "{error}");
}

#[tokio::test]
async fn a_cancelled_call_never_reaches_the_ledger() {
    let host = Arc::new(MemoryHost::new());
    host.set_file("f.txt", b"one\n");
    let toolkit = toolkit(Arc::clone(&host));
    let signal = AbortSignal::new();
    signal.abort();

    let error = execute(
        &toolkit,
        "exp_edit",
        json!({"operationId": "op-1", "edits": [{"ref": "ref-0", "replacement": "x"}]}),
        Some(signal),
    )
    .await
    .expect_err("aborted");
    assert_eq!(error.0, "Operation aborted");

    // Nothing was reserved, so the same id is fresh for a later call.
    let error = execute(
        &toolkit,
        "exp_edit",
        json!({"operationId": "op-1", "edits": [{"ref": "ref-0", "replacement": "x"}]}),
        None,
    )
    .await
    .expect_err("the reference was never issued");
    assert!(error.0.contains("invalid_ref"), "{error}");
}

#[tokio::test]
async fn a_retried_call_is_recognized_by_its_operation_id() {
    let host = Arc::new(MemoryHost::new());
    host.set_file("f.txt", b"one\ntwo\n");
    let toolkit = toolkit(Arc::clone(&host));

    let read = execute(
        &toolkit,
        "exp_read",
        json!({"path": "f.txt", "range": {"startLine": 1, "lineCount": 1}}),
        None,
    )
    .await
    .expect("read");
    let reference = read.details["reference"].as_str().expect("reference").to_string();
    let args = json!({
        "operationId": "op-1",
        "edits": [{"ref": reference, "replacement": "ONE\n"}],
    });

    let first = execute(&toolkit, "exp_edit", args.clone(), None)
        .await
        .expect("first");
    let second = execute(&toolkit, "exp_edit", args, None)
        .await
        .expect("retry");
    assert_eq!(text(&first), text(&second));
    assert_eq!(
        host.content("f.txt").as_deref(),
        Some(&b"ONE\ntwo\n"[..]),
        "the retry must not apply the edit twice"
    );
}

#[tokio::test]
async fn a_new_host_generation_makes_the_reference_expired() {
    let host = Arc::new(MemoryHost::new());
    host.set_file("f.txt", b"one\n");
    let toolkit = toolkit(Arc::clone(&host));

    let read = execute(
        &toolkit,
        "exp_read",
        json!({"path": "f.txt", "range": {"startLine": 1, "lineCount": 1}}),
        None,
    )
    .await
    .expect("read");
    let reference = read.details["reference"].as_str().expect("reference").to_string();

    toolkit.restart_generation();
    let error = execute(
        &toolkit,
        "exp_edit",
        json!({"operationId": "op-1", "edits": [{"ref": reference, "replacement": "ONE\n"}]}),
        None,
    )
    .await
    .expect_err("expired by restart");
    assert!(error.0.contains("expired_ref"), "{error}");
}
