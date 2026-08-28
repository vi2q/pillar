//! Port of packages/ai/src/utils/diagnostics.ts (pi v0.84.3).
//!
//! divergence: Rust errors carry a Display string rather than JS
//! name/message/stack triples; `DiagnosticErrorInfo` keeps the shape so
//! serialized diagnostics match pi, with `name` derived from the error
//! type when available and `stack` left unset.

use crate::types::{AssistantMessage, AssistantMessageDiagnostic, DiagnosticErrorInfo};

/// Formats an unexpected value for error messages. `std::error::Error`
/// values use their Display; anything else stringifies via `Debug`.
pub fn format_thrown_value(value: &dyn std::error::Error) -> String {
    let display = value.to_string();
    if display.is_empty() {
        "Error".to_owned()
    } else {
        display
    }
}

/// Extracts diagnostic info; the `name` derives from the error's type name.
pub fn extract_diagnostic_error<E: std::error::Error + ?Sized>(error: &E) -> DiagnosticErrorInfo {
    let message = error.to_string();
    DiagnosticErrorInfo {
        name: Some(
            std::any::type_name::<E>()
                .rsplit("::")
                .next()
                .unwrap_or("Error")
                .to_owned(),
        ),
        message: if message.is_empty() {
            "Error".to_owned()
        } else {
            message
        },
        stack: None,
        code: None,
    }
}

pub fn create_assistant_message_diagnostic(
    kind: impl Into<String>,
    error: &dyn std::error::Error,
    details: Option<serde_json::Value>,
) -> AssistantMessageDiagnostic {
    AssistantMessageDiagnostic {
        kind: kind.into(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        error: Some(extract_diagnostic_error(error)),
        details,
    }
}

/// Appends a diagnostic to a message's diagnostics list.
pub fn append_assistant_message_diagnostic(
    message: &mut AssistantMessage,
    diagnostic: AssistantMessageDiagnostic,
) {
    message.diagnostics.push(diagnostic);
}
