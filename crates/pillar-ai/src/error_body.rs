//! Port of packages/ai/src/utils/error-body.ts (pi v0.84.3).
//!
//! Shared normalization for provider HTTP error objects. Upstream probes
//! SDK-specific error fields (Mistral, `openai`, `@google/genai`, AWS
//! Bedrock) to recover the HTTP status and raw body the SDK failed to fold
//! into `error.message`; those SDK objects do not exist in the Rust port, so
//! `normalize_provider_error` takes the status, raw body, and message the
//! transport already extracted. `format_provider_error` composes the display
//! string providers use, preserving upstream's exact shape.

/// Cap on the surfaced raw body reason.
pub const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedProviderError {
    /// HTTP status code, when one could be extracted.
    pub status: Option<u16>,
    /// Raw HTTP body reason, already trimmed and truncated to the cap.
    pub body: Option<String>,
    /// The error message the SDK/transport produced.
    pub message: String,
    /// True when `message` already contains the body (no separate body to add).
    pub message_carries_body: bool,
}

/// Normalize a provider HTTP failure from its message, status, and raw body.
/// An empty or whitespace-only body counts as no body (upstream treats empty
/// parsed bodies the same way so they never surface as `"{}"`).
pub fn normalize_provider_error(
    message: impl Into<String>,
    status: Option<u16>,
    body: Option<&str>,
) -> NormalizedProviderError {
    let message = message.into();
    let body = body.and_then(|body| {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate_error_text(trimmed, MAX_PROVIDER_ERROR_BODY_CHARS))
        }
    });
    let message_carries_body = match &body {
        Some(body) => message.contains(body.as_str()),
        None => true,
    };
    NormalizedProviderError {
        status,
        body,
        message,
        message_carries_body,
    }
}

/// Compose a display string from a normalized error. When the message already
/// carries the body (Anthropic / `@google/genai` happy path) or no
/// body/status was extracted, the message is returned unchanged. Otherwise
/// the status and body are surfaced, with an optional provider prefix.
///
/// - no prefix: `"<status>: <body>"`
/// - prefix:    `"<prefix> (<status>): <body>"`
pub fn format_provider_error(norm: &NormalizedProviderError, prefix: Option<&str>) -> String {
    if norm.message_carries_body || norm.status.is_none() || norm.body.is_none() {
        return match (prefix, norm.status) {
            (Some(prefix), Some(status)) => format!("{prefix} ({status}): {}", norm.message),
            _ => norm.message.clone(),
        };
    }
    match prefix {
        Some(prefix) => format!(
            "{prefix} ({}): {}",
            norm.status.unwrap(),
            norm.body.clone().unwrap()
        ),
        None => format!("{}: {}", norm.status.unwrap(), norm.body.clone().unwrap()),
    }
}

pub fn truncate_error_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    let dropped = text.chars().count() - max_chars;
    format!("{kept}... [truncated {dropped} chars]")
}

/// Upstream `safeJsonStringify` for a parsed JSON value. serde_json
/// serialization of a `Value` cannot fail, so this is a plain serialization.
pub fn safe_json_stringify(value: &serde_json::Value) -> String {
    value.to_string()
}
