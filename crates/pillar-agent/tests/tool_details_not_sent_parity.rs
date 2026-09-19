//! The compaction / branch-summary serializer is the second path that turns
//! conversation history into model input. It must read a tool result's
//! `content` only: `details` (the edit tool's diff/patch) are stored for the
//! session and the TUI, not sent anywhere.
//!
//! `docs/TOOL-EFFICIENCY-DESIGN.md` §6.2 asks for this to be established by
//! inspecting what is actually produced, not by trusting the field name.
//! `crates/pillar-ai/tests/tool_details_not_sent_parity.rs` covers the provider
//! request bodies.

use serde_json::json;

use pillar_agent::harness::compaction::utils::serialize_conversation;
use pillar_ai::types::{Content, Message, ToolResultMessage, Usage};

const SENTINEL: &str = "pillar-details-sentinel-9f2c";
const RESULT_TEXT: &str = "Successfully replaced 1 block(s) in f.txt.";

fn tool_result_with_details() -> Message {
    Message::ToolResult(Box::new(ToolResultMessage {
        tool_call_id: "call-1".to_string(),
        tool_name: "edit".to_string(),
        content: vec![Content::text(RESULT_TEXT)],
        details: Some(json!({
            "diff": format!("-old\n+{SENTINEL}"),
            "patch": format!("--- f.txt\n+++ f.txt\n+{SENTINEL}"),
            "firstChangedLine": 1,
        })),
        usage: None::<Usage>,
        added_tool_names: None,
        is_error: false,
        timestamp: 1,
    }))
}

#[test]
fn compaction_serializer_excludes_tool_result_details() {
    let serialized = serialize_conversation(&[tool_result_with_details()]);

    assert!(
        serialized.contains(RESULT_TEXT),
        "the tool result content must be serialized: {serialized}"
    );
    assert!(
        !serialized.contains(SENTINEL),
        "tool result details reached the compaction input: {serialized}"
    );
}
