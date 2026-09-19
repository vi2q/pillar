//! The `exp_read` / `exp_edit` tool definitions and the glue onto the core
//! (design §4.1, §9).
//!
//! Two properties of this glue matter:
//!
//! - the reference the model needs is in the tool result's `content` (design
//!   §9: `ref` is information the model must be able to repeat), rendered
//!   compactly — the response's structured form goes to `details`, which the
//!   session and the TUI read and the provider never sees;
//! - registration is opt-in. The tools are named `exp_read` / `exp_edit` and
//!   are only created by [`ExpToolkit`]; nothing here touches the existing
//!   `read` / `edit` / `write` set (design §9, §11 stage A).

use std::sync::Arc;

use serde_json::{Value, json};

use super::edit::{ExpEditRequest, ExpEditResponse};
use super::error::ExpError;
use super::ledger::OperationLedger;
use super::read::{ExpReadRequest, ExpReadResponse};
use super::refs::{Clock, OwnerId, RefStore};
use super::store::ConditionalStore;
use super::{ExpLimits, exp_edit, exp_read};
use crate::types::{AgentTool, AgentToolResult, ToolExecuteError};
use pillar_ai::types::{Content, Tool};

/// Per-session state behind the experimental tools.
///
/// One toolkit per session owner: references are bound to that owner and to
/// the host generation, so two sessions cannot use each other's references
/// (design §10.1).
pub struct ExpToolkit {
    host: Arc<dyn ConditionalStore>,
    refs: Arc<RefStore>,
    ledger: Arc<OperationLedger>,
    limits: ExpLimits,
    owner: OwnerId,
}

impl ExpToolkit {
    /// A toolkit over a host, with opaque UUID references (an id a caller
    /// cannot guess) and the host's clock.
    ///
    /// The clock is passed in rather than read from the platform: `SystemTime`
    /// is not available on every host the core compiles for (the Wasm
    /// profiles), and expiry must be controllable in tests (design §3, §8.2).
    /// A host without a wall clock supplies its own monotonic source.
    pub fn new(
        host: Arc<dyn ConditionalStore>,
        clock: Clock,
        limits: ExpLimits,
        owner: OwnerId,
    ) -> Self {
        let refs = Arc::new(RefStore::new(clock, limits.max_live_refs));
        let ledger = Arc::new(OperationLedger::new(limits.ledger_capacity));
        Self {
            host,
            refs,
            ledger,
            limits,
            owner,
        }
    }

    /// A toolkit with an injected reference store and ledger, for tests and
    /// hosts that own their own clock.
    pub fn with_state(
        host: Arc<dyn ConditionalStore>,
        refs: Arc<RefStore>,
        ledger: Arc<OperationLedger>,
        limits: ExpLimits,
        owner: OwnerId,
    ) -> Self {
        Self {
            host,
            refs,
            ledger,
            limits,
            owner,
        }
    }

    /// Start a new host generation: every outstanding reference becomes
    /// expired (design §7.2).
    pub fn restart_generation(&self) -> u64 {
        self.refs.restart()
    }

    /// Every experimental tool, in registration order.
    pub fn tools(&self) -> Vec<AgentTool> {
        vec![self.read_tool(), self.edit_tool()]
    }

    /// The tool the model calls to obtain a reference.
    pub fn read_tool(&self) -> AgentTool {
        let (host, refs, limits, owner) = (
            Arc::clone(&self.host),
            Arc::clone(&self.refs),
            self.limits.clone(),
            self.owner.clone(),
        );
        AgentTool {
            tool: Tool {
                name: "exp_read".to_string(),
                description: exp_read_description(),
                parameters: exp_read_parameters_json(),
                constrained_sampling: None,
            },
            label: "exp_read".to_string(),
            prepare_arguments: None,
            execute: Arc::new(move |_id, args, signal, _on_update| {
                let (host, refs, limits, owner) = (
                    Arc::clone(&host),
                    Arc::clone(&refs),
                    limits.clone(),
                    owner.clone(),
                );
                Box::pin(async move {
                    if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                        return Err(ToolExecuteError("Operation aborted".to_string()));
                    }
                    let request: ExpReadRequest = serde_json::from_value(args)
                        .map_err(|error| ToolExecuteError(format!("exp_read input: {error}")))?;
                    let response =
                        exp_read(host.as_ref(), refs.as_ref(), &limits, &owner, &request)
                            .await
                            .map_err(tool_error)?;
                    Ok(read_result(&response))
                })
            }),
            execution_mode: None,
        }
    }

    /// The tool the model calls to apply referenced replacements.
    pub fn edit_tool(&self) -> AgentTool {
        let (host, refs, ledger, limits, owner) = (
            Arc::clone(&self.host),
            Arc::clone(&self.refs),
            Arc::clone(&self.ledger),
            self.limits.clone(),
            self.owner.clone(),
        );
        AgentTool {
            tool: Tool {
                name: "exp_edit".to_string(),
                description: exp_edit_description(),
                parameters: exp_edit_parameters_json(),
                constrained_sampling: None,
            },
            label: "exp_edit".to_string(),
            prepare_arguments: None,
            execute: Arc::new(move |_id, args, signal, _on_update| {
                let (host, refs, ledger, limits, owner) = (
                    Arc::clone(&host),
                    Arc::clone(&refs),
                    Arc::clone(&ledger),
                    limits.clone(),
                    owner.clone(),
                );
                Box::pin(async move {
                    if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                        // Nothing is reserved: a cancellation before the
                        // operation starts is not an operation (design §4.4).
                        return Err(ToolExecuteError("Operation aborted".to_string()));
                    }
                    let request: ExpEditRequest = serde_json::from_value(args)
                        .map_err(|error| ToolExecuteError(format!("exp_edit input: {error}")))?;
                    let response = exp_edit(
                        host.as_ref(),
                        refs.as_ref(),
                        &ledger,
                        &limits,
                        &owner,
                        &request,
                    )
                    .await
                    .map_err(tool_error)?;
                    Ok(edit_result(&response))
                })
            }),
            execution_mode: None,
        }
    }
}

/// The tool result for a read: a one-line header the model can act on, then
/// the text.
fn read_result(response: &ExpReadResponse) -> AgentToolResult {
    let mut header = format!(
        "[exp_read {} {}-{}/{}",
        response.path,
        response.start_line,
        response.start_line + response.line_count.saturating_sub(1),
        response.total_lines
    );
    match &response.reference {
        Some(reference) => {
            header.push_str(&format!(" ref={reference}"));
            if !response.editable {
                header.push_str(" editable=false");
            }
        }
        None => header.push_str(" ref=-"),
    }
    header.push_str(&format!(" delivery={}", delivery_label(response)));
    if let Some(reason) = response.withheld {
        header.push_str(&format!(" withheld={reason:?}"));
    }
    if let Some(repair) = &response.repair {
        header.push_str(&format!(" hint={repair:?}"));
    }
    header.push(']');

    AgentToolResult {
        content: vec![Content::text(if response.text.is_empty() {
            header
        } else {
            format!("{header}\n{}", response.text)
        })],
        details: serde_json::to_value(response).unwrap_or(Value::Null),
        ..Default::default()
    }
}

fn delivery_label(response: &ExpReadResponse) -> &'static str {
    match response.delivery_state {
        super::read::DeliveryState::Complete => "complete",
        super::read::DeliveryState::Partial => "partial",
    }
}

/// The tool result for an edit: the new revision and a short receipt, no diff
/// (design §4.2: "適用後は新revisionと短いreceiptを返す").
fn edit_result(response: &ExpEditResponse) -> AgentToolResult {
    let receipt = &response.receipt;
    let content = format!(
        "[exp_edit {} applied {} replacement(s), revision {}/{:#x}, {} bytes]",
        receipt.path,
        receipt.edits_applied,
        receipt.revision.generation,
        receipt.revision.digest,
        receipt.bytes_written,
    );
    AgentToolResult {
        content: vec![Content::text(content)],
        details: serde_json::to_value(response).unwrap_or(Value::Null),
        ..Default::default()
    }
}

/// The experiment's errors become tool errors with the code, the message and
/// the one-line repair the model should follow.
fn tool_error(error: ExpError) -> ToolExecuteError {
    let mut message = format!("exp tool failed ({}) : {}", error.code, error.message);
    if let Some(repair) = error.repair {
        message.push_str(&format!(" — {repair}"));
    }
    ToolExecuteError(message)
}

pub fn exp_read_description() -> String {
    "Read a line range of one file and get a reference (`ref`) for the exact bytes shown. \
     Use the reference with exp_edit instead of copying the old text back. Only text that was \
     fully delivered is editable, and only on a host that can publish conditionally."
        .to_string()
}

pub fn exp_edit_description() -> String {
    "Replace the bytes behind one or more references, all from the same file and the same \
     revision. Every reference must come from exp_read in this session. The edit is rejected \
     with a conflict if the file changed since it was read; repeat the same operationId only \
     when retrying the same call."
        .to_string()
}

/// `exp_read` parameters (design §4.1: line-addressed display, byte-identity
/// references).
pub fn exp_read_parameters_json() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Path of the file to read (relative or absolute)"
            },
            "range": {
                "type": "object",
                "description": "1-indexed line range to show and make editable",
                "properties": {
                    "startLine": {"type": "integer", "description": "First line to show (1-indexed)"},
                    "lineCount": {"type": "integer", "description": "Number of lines to show"}
                },
                "required": ["startLine", "lineCount"]
            }
        },
        "required": ["path", "range"]
    })
}

/// `exp_edit` parameters (design §4.1: `ref` plus the new text, no `oldText`).
pub fn exp_edit_parameters_json() -> Value {
    json!({
        "type": "object",
        "properties": {
            "operationId": {
                "type": "string",
                "description": "Your id for this edit. Repeat the same id only when retrying this exact call."
            },
            "edits": {
                "type": "array",
                "description": "Non-overlapping replacements in one file, all from references of the same revision.",
                "items": {
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string", "description": "A ref returned by exp_read"},
                        "replacement": {"type": "string", "description": "Text to put in place of the referenced bytes, verbatim"}
                    },
                    "required": ["ref", "replacement"]
                }
            }
        },
        "required": ["operationId", "edits"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schemas_require_the_fields_the_core_needs() {
        let read = exp_read_parameters_json();
        assert_eq!(read["required"], json!(["path", "range"]));
        assert_eq!(
            read["properties"]["range"]["required"],
            json!(["startLine", "lineCount"])
        );

        let edit = exp_edit_parameters_json();
        assert_eq!(edit["required"], json!(["operationId", "edits"]));
        assert_eq!(
            edit["properties"]["edits"]["items"]["required"],
            json!(["ref", "replacement"])
        );
    }

    #[test]
    fn the_experimental_tool_names_do_not_shadow_the_existing_ones() {
        assert!(!exp_read_description().is_empty());
        assert!(
            exp_read_parameters_json()["properties"]
                .get("offset")
                .is_none()
        );
        assert!(
            exp_edit_parameters_json()["properties"]
                .get("edits")
                .and_then(|edits| edits["items"]["properties"].get("oldText"))
                .is_none()
        );
    }
}
