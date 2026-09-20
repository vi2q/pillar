//! Port of packages/ai/src/utils/provider-retry.ts (pi v0.84.3).
//!
//! Reproduces the retry behavior used by the OpenAI and Anthropic SDKs while
//! making the backoff sleep interruptible. Upstream callers invoke the SDK
//! with `maxRetries: 0` and wrap the request with this helper; the Rust
//! providers will do the same around [`crate::transport::FetchFn`] calls.

use crate::abort::AbortSignal;

const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

/// Error shape a retryable provider request can fail with (upstream:
/// an SDK `Error` carrying `status`/`headers`). Transport failures map to
/// `status: None`, which upstream treats as retryable.
#[derive(Debug, Clone)]
pub struct ProviderRequestError {
    pub status: Option<u16>,
    /// Response header name/value pairs (lowercased names).
    pub headers: Vec<(String, String)>,
    pub message: String,
    /// True when the failure is the request being aborted.
    pub aborted: bool,
}

impl ProviderRequestError {
    pub fn http(status: u16, headers: Vec<(String, String)>, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            headers,
            message: message.into(),
            aborted: false,
        }
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            status: None,
            headers: Vec::new(),
            message: message.into(),
            aborted: false,
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl std::fmt::Display for ProviderRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProviderRetryOptions {
    pub max_retries: Option<u32>,
    /// Cap on provider-requested retry delays; `Some(0)` disables the limit.
    pub max_retry_delay_ms: Option<u64>,
    pub signal: Option<AbortSignal>,
}

/// Mirrors the pinned OpenAI/Anthropic SDK retry policy; review when either
/// SDK is upgraded.
fn is_retryable_provider_error(error: &ProviderRequestError) -> bool {
    match error.header("x-should-retry") {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    match error.status {
        None => true,
        Some(status) => status == 408 || status == 409 || status == 429 || status >= 500,
    }
}

fn validate_server_retry_delay_ms(
    delay_ms: u64,
    max_retry_delay_ms: Option<u64>,
    provider_error_message: &str,
) -> Result<u64, ProviderRequestError> {
    let max_delay_ms = max_retry_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max_delay_ms > 0 && delay_ms > max_delay_ms {
        return Err(ProviderRequestError {
            status: None,
            headers: Vec::new(),
            message: format!(
                "Server requested {}s retry delay (max: {}s). {provider_error_message}",
                delay_ms.div_ceil(1000),
                max_delay_ms / 1000
            ),
            aborted: false,
        });
    }
    Ok(delay_ms)
}

/// divergence: upstream falls back to `Date.parse(retry-after)` when the
/// header is an HTTP date; the port only parses numeric values (the OpenAI /
/// Anthropic endpoints return numbers) and falls through to exponential
/// backoff otherwise.
fn get_retry_delay_ms(
    error: &ProviderRequestError,
    retry_index: u32,
    max_retry_delay_ms: Option<u64>,
) -> Result<u64, ProviderRequestError> {
    if let Some(retry_after_ms) = error.header("retry-after-ms")
        && let Ok(value) = retry_after_ms.trim().parse::<f64>()
    {
        return validate_server_retry_delay_ms(
            value.max(0.0) as u64,
            max_retry_delay_ms,
            &error.message,
        );
    }

    if let Some(retry_after) = error.header("retry-after") {
        let trimmed = retry_after.trim();
        if let Ok(seconds) = trimmed.parse::<f64>() {
            let delay_ms = (seconds * 1000.0).max(0.0) as u64;
            return validate_server_retry_delay_ms(delay_ms, max_retry_delay_ms, &error.message);
        }
    }

    let exponential_delay_ms = (0.5 * 2f64.powi(retry_index as i32)).min(8.0) * 1000.0;
    // Upstream: exponentialDelay * (1 - Math.random() * 0.25) — up to 25% jitter.
    let jittered = exponential_delay_ms * (1.0 - fastrand::f64() * 0.25);
    Ok(jittered as u64)
}

fn aborted_error() -> ProviderRequestError {
    ProviderRequestError {
        status: None,
        headers: Vec::new(),
        message: "Request aborted".to_string(),
        aborted: true,
    }
}

async fn abortable_sleep(
    ms: u64,
    signal: Option<&AbortSignal>,
) -> Result<(), ProviderRequestError> {
    if signal.map(|signal| signal.is_aborted()).unwrap_or(false) {
        return Err(aborted_error());
    }
    match signal {
        Some(signal) => {
            tokio::select! {
                biased;
                _ = signal.aborted_or_pending() => Err(aborted_error()),
                _ = crate::clock::sleep(std::time::Duration::from_millis(ms)) => Ok(()),
            }
        }
        None => {
            crate::clock::sleep(std::time::Duration::from_millis(ms)).await;
            Ok(())
        }
    }
}

/// Retry an idempotent-failure provider request with the SDK backoff policy.
/// Each attempt is a fresh request; provider-requested delays above
/// `max_retry_delay_ms` fail immediately (60 seconds by default; set it to
/// zero to disable the limit).
pub async fn retry_provider_request<T, F, Fut>(
    request: F,
    options: ProviderRetryOptions,
) -> Result<T, ProviderRequestError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderRequestError>>,
{
    let max_retries = options.max_retries.unwrap_or(0);
    let mut retries_remaining = max_retries;

    loop {
        match request().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if error.aborted {
                    return Err(error);
                }
                if options
                    .signal
                    .as_ref()
                    .map(|signal| signal.is_aborted())
                    .unwrap_or(false)
                {
                    return Err(aborted_error());
                }
                if retries_remaining == 0 || !is_retryable_provider_error(&error) {
                    return Err(error);
                }

                let retry_index = max_retries - retries_remaining;
                retries_remaining -= 1;
                let delay_ms = get_retry_delay_ms(&error, retry_index, options.max_retry_delay_ms)?;
                abortable_sleep(delay_ms, options.signal.as_ref()).await?;
            }
        }
    }
}
