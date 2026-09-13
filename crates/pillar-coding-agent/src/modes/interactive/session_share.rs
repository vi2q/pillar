//! Port of packages/coding-agent/src/modes/interactive/session-share.ts
//! (pi v0.84.3), export half: attach presentation metadata to a session
//! export so a share link can render tools and the system prompt.
//!
//! divergence: the interactive share flows (`shareSession`, `tryShareViaRadius`,
//! `shareViaGist`) drive TUI components (BorderedLoader, editor container,
//! focus) and the Radius/gist HTTP calls; they land with the interactive mode
//! port. This module provides the export they build on.

use std::path::Path;

use serde_json::{Value, json};

use crate::core::agent_session_class::AgentSession;
use crate::core::session_support::export_session_to_jsonl;

/// Tool definitions embedded into the share metadata (upstream
/// `session.state.tools.map((tool) => ({ name, description, parameters }))`).
pub fn share_tool_definitions(session: &AgentSession) -> Vec<Value> {
    session
        .state()
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.tool.name,
                "description": tool.tool.description,
                "parameters": tool.tool.parameters,
            })
        })
        .collect()
}

/// Export the current branch with presentation metadata for a share link
/// (upstream `exportSessionForShare`): a trailing `pi.share` custom entry
/// carrying the system prompt and tool definitions.
pub fn export_session_for_share(file_path: &Path, session: &AgentSession) -> Result<(), String> {
    let system_prompt = session.system_prompt();
    let tools = share_tool_definitions(session);
    let cwd = session.cwd().to_string();
    let output_path = file_path.to_string_lossy().to_string();

    let trailing = |parent_id: &str, timestamp: &str| -> Vec<Value> {
        vec![json!({
            "type": "custom",
            "customType": "pi.share",
            // Upstream slices a UUID to 8 characters.
            "id": short_id(),
            "parentId": parent_id,
            "timestamp": timestamp,
            "data": {
                "systemPrompt": system_prompt,
                "tools": tools,
            },
        })]
    };

    let session_manager = session.session_manager().lock().expect("session lock");
    export_session_to_jsonl(&session_manager, Some(&output_path), &cwd, Some(&trailing))?;
    Ok(())
}

/// An 8-character entry id (upstream `crypto.randomUUID().slice(0, 8)`).
fn short_id() -> String {
    let id = pillar_ai::uuid::uuidv7();
    id.chars().take(8).collect()
}
