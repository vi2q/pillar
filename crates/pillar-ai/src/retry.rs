//! Port of packages/ai/src/utils/retry.ts (pi v0.84.3).
//!
//! Retry classification and the bounded-retry loop for assistant-producing
//! calls. The non-retryable and retryable error-pattern lists are preserved
//! verbatim; classification is case-insensitive substring/regex matching.

use regex::Regex;
use std::sync::OnceLock;

use crate::types::{AssistantMessage, StopReason};

fn non_retryable_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            "(?i){}",
            [
                // OpenCode Go/free-tier subscription/account limits returned as
                // 429 JSON error types; not transient throttles.
                "GoUsageLimitError",
                "FreeUsageLimitError",
                // OpenCode Go subscription-limit text.
                "Monthly usage limit reached",
                "available balance",
                // Generic quota/budget/billing exhaustion. `insufficient_quota`
                // is OpenAI's quota/billing error code; the others cover common
                // gateway wording.
                "insufficient_quota",
                "out of budget",
                "quota exceeded",
                "billing",
            ]
            .join("|")
        ))
        .expect("non-retryable pattern")
    })
}

fn retryable_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            "(?i){}",
            [
                // Generic provider load, HTTP status, server-side transient failures.
                "overloaded",
                "rate.?limit",
                "too many requests",
                "429",
                "500",
                "502",
                "503",
                "504",
                "524",
                "service.?unavailable",
                "server.?error",
                "internal.?error",
                // Wrapper/provider text for transient upstream failures,
                // including OpenRouter "Provider returned error" responses.
                "provider.?returned.?error",
                "exceeded request buffer limit while retrying upstream",
                // Network, proxy, fetch transport failures.
                "network.?error",
                "connection.?error",
                "connection.?refused",
                "connection.?lost",
                "other side closed",
                "fetch failed",
                "getaddrinfo",
                "ENOTFOUND",
                "EAI_AGAIN",
                "upstream.?connect",
                "reset before headers",
                "socket hang up",
                "socket connection was closed",
                "timed? out",
                "timeout",
                "terminated",
                // WebSocket transports can report close/error text.
                "websocket.?closed",
                "websocket.?error",
                // Premature stream endings from SDKs and transports.
                "ended without",
                "stream ended before message_stop",
                "stream ended before a terminal response event",
                "http2 request did not get a response",
                // Provider-requested retry delay cap failures flow through the
                // outer retry policy so callers can surface/abort the backoff.
                "retry delay",
                // Explicit retry guidance emitted mid-stream by OpenAI Responses
                // and Bedrock stream exceptions.
                "you can retry your request",
                "try your request again",
                "please retry your request",
                // gRPC based providers (e.g. NVIDIA NIM)
                "ResourceExhausted",
            ]
            .join("|")
        ))
        .expect("retryable pattern")
    })
}

/// Retry policy: bounded attempts with exponential backoff
/// (`baseDelayMs * 2^(attempt-1)`). Matches `settings.retry` in coding-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Max retry attempts (0 = no retries). The initial call never counts.
    pub max_retries: u32,
    /// Base delay in ms. Per-attempt delay is `baseDelayMs * 2^(attempt-1)`.
    pub base_delay_ms: u64,
}

/// Scheduled-retry callback type.
pub type OnRetryScheduled<'a> = Box<dyn Fn(u32, u32, u64, &str) + Send + 'a>;
/// Finished-retry callback type.
pub type OnRetryFinished<'a> = Box<dyn Fn(bool, u32, Option<&str>) + Send + 'a>;

/// Callbacks emitted by [`retry_assistant_call`] around each retry.
#[derive(Default)]
pub struct RetryCallbacks<'a> {
    /// Emitted before the backoff sleep of each retry attempt (1-indexed).
    pub on_retry_scheduled: Option<OnRetryScheduled<'a>>,
    /// Emitted after the backoff sleep, immediately before the retried call.
    pub on_retry_attempt_start: Option<Box<dyn Fn() + Send + 'a>>,
    /// Emitted once when the loop ends: success if a later call completed normally.
    pub on_retry_finished: Option<OnRetryFinished<'a>>,
}

/// Run a single assistant-producing call with bounded retry on transient errors.
///
/// - Success returns immediately; aborts are terminal and never retried.
/// - Non-retryable errors (quota/billing exhaustion included) return immediately.
/// - Otherwise retries up to `policy.max_retries` with exponential backoff.
///
/// When `policy` is disabled, the first response returns unchanged.
pub async fn retry_assistant_call<F, Fut>(
    mut produce: F,
    policy: Option<RetryPolicy>,
    callbacks: Option<&mut RetryCallbacks<'_>>,
) -> AssistantMessage
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = AssistantMessage>,
{
    let max_attempts = policy
        .map(|p| if p.enabled { p.max_retries } else { 0 })
        .unwrap_or(0);

    let mut attempt: u32 = 0;
    let mut last_retry: Option<(u32, String)> = None;
    loop {
        let response = produce().await;

        // Abort: terminal but not successful. Never retry an aborted message.
        if response.stop_reason == StopReason::Aborted {
            if let (Some(callbacks), Some((attempt, _))) = (callbacks.as_ref(), last_retry.as_ref())
            {
                if let Some(on_finished) = &callbacks.on_retry_finished {
                    on_finished(false, *attempt, None);
                }
            }
            return response;
        }

        // Success: non-error, non-abort responses return as-is.
        if response.stop_reason != StopReason::Error {
            if let (Some(callbacks), Some((attempt, _))) = (callbacks.as_ref(), last_retry.as_ref())
            {
                if let Some(on_finished) = &callbacks.on_retry_finished {
                    on_finished(true, *attempt, None);
                }
            }
            return response;
        }

        // Non-retryable, or budget exhausted: return the final error message.
        if attempt >= max_attempts || !is_retryable_assistant_error(&response) {
            if let (Some(callbacks), Some((attempt, _))) = (callbacks.as_ref(), last_retry.as_ref())
            {
                if let Some(on_finished) = &callbacks.on_retry_finished {
                    on_finished(false, *attempt, response.error_message.as_deref());
                }
            }
            return response;
        }

        attempt += 1;
        let error_message = response
            .error_message
            .clone()
            .unwrap_or_else(|| "Unknown error".into());
        let delay_ms = policy
            .map(|p| p.base_delay_ms * 2u64.pow(attempt - 1))
            .unwrap_or(0);
        if let Some(callbacks) = callbacks.as_ref() {
            if let Some(on_scheduled) = &callbacks.on_retry_scheduled {
                on_scheduled(attempt, max_attempts, delay_ms, &error_message);
            }
        }

        last_retry = Some((attempt, error_message));

        // The upstream abortable backoff sleep maps to a plain sleep here;
        // cancellation surfaces through the produce call in the Rust port.
        if delay_ms > 0 {
            crate::clock::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        if let Some(callbacks) = callbacks.as_ref() {
            if let Some(on_start) = &callbacks.on_retry_attempt_start {
                on_start();
            }
        }
    }
}

/// Classifies whether a failed assistant message looks like a transient
/// provider or transport error.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error_message) = &message.error_message else {
        return false;
    };
    if non_retryable_pattern().is_match(error_message) {
        return false;
    }
    retryable_pattern().is_match(error_message)
}
