#![allow(clippy::type_complexity)]
//! Port of packages/ai/test/{retry,context-estimate,context-overflow}.test.ts
//! (pi v0.84.3) — unit tests for utils that don't need an HTTP transport.
//! One Rust test per upstream test case, same names in comments.

use pillar_ai::faux::{FauxContent, FauxMessageOptions, faux_assistant_message};
use pillar_ai::types::{Content, Context, Message, StopReason, Usage, UsageCost};
use pillar_ai::retry::{RetryCallbacks, RetryPolicy};
use pillar_ai::{
    estimate_context_tokens, is_context_overflow, is_recoverable_length,
    is_retryable_assistant_error, retry_assistant_call,
};
use std::sync::{Arc, Mutex};

fn zero_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: UsageCost::default(),
    }
}

// --- retry classification (retry.test.ts) -------------------------------

fn error_message(message: &str) -> pillar_ai::AssistantMessage {
    faux_assistant_message(
        "",
        FauxMessageOptions {
            stop_reason: Some(StopReason::Error),
            error_message: Some(message.to_owned()),
            ..Default::default()
        },
    )
}

const OPENAI_EXPLICIT_RETRY: &str = "An error occurred while processing your request. You can retry your request, or contact us through our help center at help.openai.com if the error persists. Please include the request ID req_******** in your message.";
const BEDROCK_EXPLICIT_RETRY: &str = r#"{"message":"The system encountered an unexpected error during processing. Try your request again."}"#;
const NVIDIA_NIM_RESOURCE_EXHAUSTED: &str =
    "ResourceExhausted: Worker local total request limit reached (288/48)";
const BUN_FETCH_SOCKET_CLOSED: &str = "The socket connection was closed unexpectedly. For more information, pass `verbose: true` in the second argument to fetch()";
const OPENAI_RESPONSES_EARLY_EOF: &str =
    "OpenAI Responses stream ended before a terminal response event";
const WRAPPED_DNS_LOOKUP: &str = "The pending stream has been canceled (caused by: getaddrinfo ENOTFOUND bedrock-runtime.us-east-1.amazonaws.com)";

#[test]
fn matches_explicit_provider_retry_guidance() {
    for message in [
        OPENAI_EXPLICIT_RETRY,
        BEDROCK_EXPLICIT_RETRY,
        NVIDIA_NIM_RESOURCE_EXHAUSTED,
    ] {
        assert!(
            is_retryable_assistant_error(&error_message(message)),
            "{message}"
        );
    }
}

#[test]
fn matches_bun_fetch_socket_drop_wording() {
    assert!(is_retryable_assistant_error(&error_message(
        BUN_FETCH_SOCKET_CLOSED
    )));
}

#[test]
fn matches_upstream_request_buffer_exhaustion_wording() {
    assert!(is_retryable_assistant_error(&error_message(
        "Error: exceeded request buffer limit while retrying upstream"
    )));
}

#[test]
fn matches_dns_transport_failure_wording() {
    for message in [
        WRAPPED_DNS_LOOKUP,
        "connect ENOTFOUND api.example.com",
        "EAI_AGAIN api.example.com",
        "getaddrinfo failed for api.example.com",
    ] {
        assert!(
            is_retryable_assistant_error(&error_message(message)),
            "{message}"
        );
    }
}

#[test]
fn matches_openai_responses_streams_that_end_before_terminal_events() {
    assert!(is_retryable_assistant_error(&error_message(
        OPENAI_RESPONSES_EARLY_EOF
    )));
}

#[test]
fn keeps_provider_limit_errors_non_retryable() {
    assert!(!is_retryable_assistant_error(&error_message(
        "429 quota exceeded"
    )));
}

#[test]
fn classifies_assistant_error_messages() {
    assert!(is_retryable_assistant_error(&error_message(
        "overloaded_error"
    )));
    assert!(is_retryable_assistant_error(&error_message(
        "524 status code (no body)"
    )));
    // A successful message is not an error at all.
    assert!(!is_retryable_assistant_error(&faux_assistant_message(
        "not an error",
        Default::default()
    )));
}

// --- retryAssistantCall (retry.test.ts) ---------------------------------

const ENABLED: Option<RetryPolicy> = Some(RetryPolicy {
    enabled: true,
    max_retries: 3,
    base_delay_ms: 0,
});
const DISABLED: Option<RetryPolicy> = Some(RetryPolicy {
    enabled: false,
    max_retries: 3,
    base_delay_ms: 0,
});

#[tokio::test]
async fn returns_a_successful_response_immediately_without_retrying() {
    let calls = Arc::new(Mutex::new(0u32));
    let calls_for_produce = Arc::clone(&calls);
    let response = retry_assistant_call(
        || {
            *calls_for_produce.lock().unwrap() += 1;
            let message = faux_assistant_message("ok", Default::default());
            std::future::ready(message)
        },
        ENABLED,
        None,
    )
    .await;
    assert_eq!(response.content, vec![Content::text("ok")]);
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn does_not_retry_an_aborted_message() {
    let calls = Arc::new(Mutex::new(0u32));
    let scheduled = Arc::new(Mutex::new(0u32));
    let calls_produce = Arc::clone(&calls);
    let scheduled_cb = Arc::clone(&scheduled);
    let mut callbacks = RetryCallbacks {
        on_retry_scheduled: Some(Box::new(move |_, _, _, _| {
            *scheduled_cb.lock().unwrap() += 1
        })),
        ..Default::default()
    };
    let response = retry_assistant_call(
        || {
            *calls_produce.lock().unwrap() += 1;
            let message = faux_assistant_message(
                "",
                FauxMessageOptions {
                    stop_reason: Some(StopReason::Aborted),
                    ..Default::default()
                },
            );
            std::future::ready(message)
        },
        ENABLED,
        Some(&mut callbacks),
    )
    .await;
    assert_eq!(response.stop_reason, StopReason::Aborted);
    assert_eq!(*calls.lock().unwrap(), 1);
    assert_eq!(*scheduled.lock().unwrap(), 0);
}

#[tokio::test]
async fn does_not_retry_a_non_retryable_error_quota_billing() {
    let calls = Arc::new(Mutex::new(0u32));
    let scheduled = Arc::new(Mutex::new(0u32));
    let finished = Arc::new(Mutex::new(0u32));
    let calls_produce = Arc::clone(&calls);
    let scheduled_cb = Arc::clone(&scheduled);
    let finished_cb = Arc::clone(&finished);
    let mut callbacks = RetryCallbacks {
        on_retry_scheduled: Some(Box::new(move |_, _, _, _| {
            *scheduled_cb.lock().unwrap() += 1
        })),
        on_retry_finished: Some(Box::new(move |_, _, _| *finished_cb.lock().unwrap() += 1)),
        ..Default::default()
    };
    let response = retry_assistant_call(
        || {
            *calls_produce.lock().unwrap() += 1;
            let message = faux_assistant_message(
                "",
                FauxMessageOptions {
                    stop_reason: Some(StopReason::Error),
                    error_message: Some("insufficient_quota".into()),
                    ..Default::default()
                },
            );
            std::future::ready(message)
        },
        ENABLED,
        Some(&mut callbacks),
    )
    .await;
    assert_eq!(response.stop_reason, StopReason::Error);
    assert_eq!(*calls.lock().unwrap(), 1);
    assert_eq!(*scheduled.lock().unwrap(), 0);
    assert_eq!(*finished.lock().unwrap(), 0);
}

#[tokio::test]
async fn retries_a_transient_error_up_to_max_retries_then_returns_the_final_error() {
    let calls = Arc::new(Mutex::new(0u32));
    let scheduled = Arc::new(Mutex::new(0u32));
    let finished: Arc<Mutex<Vec<(bool, u32, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let calls_produce = Arc::clone(&calls);
    let scheduled_cb = Arc::clone(&scheduled);
    let finished_cb = Arc::clone(&finished);
    let mut callbacks = RetryCallbacks {
        on_retry_scheduled: Some(Box::new(move |_, _, _, _| {
            *scheduled_cb.lock().unwrap() += 1
        })),
        on_retry_finished: Some(Box::new(move |success, attempt, error| {
            finished_cb
                .lock()
                .unwrap()
                .push((success, attempt, error.map(String::from)));
        })),
        ..Default::default()
    };
    let response = retry_assistant_call(
        || {
            *calls_produce.lock().unwrap() += 1;
            let message = faux_assistant_message(
                "",
                FauxMessageOptions {
                    stop_reason: Some(StopReason::Error),
                    error_message: Some("terminated".into()),
                    ..Default::default()
                },
            );
            std::future::ready(message)
        },
        ENABLED,
        Some(&mut callbacks),
    )
    .await;
    assert_eq!(response.stop_reason, StopReason::Error);
    assert_eq!(*calls.lock().unwrap(), 4); // 1 initial + 3 retries
    assert_eq!(*scheduled.lock().unwrap(), 3);
    assert_eq!(
        finished.lock().unwrap().as_slice(),
        [(false, 3, Some("terminated".to_owned()))]
    );
}

#[tokio::test]
async fn stops_retrying_once_a_call_succeeds() {
    let calls = Arc::new(Mutex::new(0u32));
    let finished: Arc<Mutex<Vec<(bool, u32, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let calls_produce = Arc::clone(&calls);
    let finished_cb = Arc::clone(&finished);
    let mut callbacks = RetryCallbacks {
        on_retry_finished: Some(Box::new(move |success, attempt, error| {
            finished_cb
                .lock()
                .unwrap()
                .push((success, attempt, error.map(String::from)));
        })),
        ..Default::default()
    };
    let response = retry_assistant_call(
        || {
            let mut n = calls_produce.lock().unwrap();
            *n += 1;
            let count = *n;
            drop(n);
            let message = if count < 3 {
                faux_assistant_message(
                    "",
                    FauxMessageOptions {
                        stop_reason: Some(StopReason::Error),
                        error_message: Some("terminated".into()),
                        ..Default::default()
                    },
                )
            } else {
                faux_assistant_message("recovered", Default::default())
            };
            std::future::ready(message)
        },
        ENABLED,
        Some(&mut callbacks),
    )
    .await;
    assert_eq!(response.content, vec![Content::text("recovered")]);
    assert_eq!(*calls.lock().unwrap(), 3);
    assert_eq!(finished.lock().unwrap().as_slice(), [(true, 2, None)]);
}

#[tokio::test]
async fn reports_an_aborted_retried_call_as_unsuccessful() {
    let calls = Arc::new(Mutex::new(0u32));
    let finished: Arc<Mutex<Vec<(bool, u32, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let calls_produce = Arc::clone(&calls);
    let finished_cb = Arc::clone(&finished);
    let mut callbacks = RetryCallbacks {
        on_retry_finished: Some(Box::new(move |success, attempt, error| {
            finished_cb
                .lock()
                .unwrap()
                .push((success, attempt, error.map(String::from)));
        })),
        ..Default::default()
    };
    let response = retry_assistant_call(
        || {
            let mut n = calls_produce.lock().unwrap();
            *n += 1;
            let count = *n;
            drop(n);
            let message = if count == 1 {
                faux_assistant_message(
                    "",
                    FauxMessageOptions {
                        stop_reason: Some(StopReason::Error),
                        error_message: Some("terminated".into()),
                        ..Default::default()
                    },
                )
            } else {
                faux_assistant_message(
                    "",
                    FauxMessageOptions {
                        stop_reason: Some(StopReason::Aborted),
                        ..Default::default()
                    },
                )
            };
            std::future::ready(message)
        },
        ENABLED,
        Some(&mut callbacks),
    )
    .await;
    assert_eq!(response.stop_reason, StopReason::Aborted);
    assert_eq!(*calls.lock().unwrap(), 2);
    assert_eq!(finished.lock().unwrap().as_slice(), [(false, 1, None)]);
}

#[tokio::test]
async fn does_not_retry_when_policy_is_disabled() {
    let calls = Arc::new(Mutex::new(0u32));
    let scheduled = Arc::new(Mutex::new(0u32));
    let calls_produce = Arc::clone(&calls);
    let scheduled_cb = Arc::clone(&scheduled);
    let mut callbacks = RetryCallbacks {
        on_retry_scheduled: Some(Box::new(move |_, _, _, _| {
            *scheduled_cb.lock().unwrap() += 1
        })),
        ..Default::default()
    };
    let response = retry_assistant_call(
        || {
            *calls_produce.lock().unwrap() += 1;
            let message = faux_assistant_message(
                "",
                FauxMessageOptions {
                    stop_reason: Some(StopReason::Error),
                    error_message: Some("terminated".into()),
                    ..Default::default()
                },
            );
            std::future::ready(message)
        },
        DISABLED,
        Some(&mut callbacks),
    )
    .await;
    assert_eq!(response.stop_reason, StopReason::Error);
    assert_eq!(*calls.lock().unwrap(), 1);
    assert_eq!(*scheduled.lock().unwrap(), 0);
}

#[tokio::test]
async fn emits_on_retry_attempt_start_after_backoff_before_each_retried_call() {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(Mutex::new(0u32));
    let events_produce = Arc::clone(&events);
    let calls_produce = Arc::clone(&calls);
    let events_scheduled = Arc::clone(&events);
    let events_attempt = Arc::clone(&events);
    let mut callbacks = RetryCallbacks {
        on_retry_scheduled: Some(Box::new(move |attempt, _, _, _| {
            events_scheduled
                .lock()
                .unwrap()
                .push(format!("retry:{attempt}"));
        })),
        on_retry_attempt_start: Some(Box::new(move || {
            events_attempt.lock().unwrap().push("attempt-start".into());
        })),
        ..Default::default()
    };
    let response = retry_assistant_call(
        || {
            let mut n = calls_produce.lock().unwrap();
            *n += 1;
            events_produce
                .lock()
                .unwrap()
                .push(format!("produce:{}", *n - 1));
            let count = *n;
            drop(n);
            let message = if count < 3 {
                faux_assistant_message(
                    "",
                    FauxMessageOptions {
                        stop_reason: Some(StopReason::Error),
                        error_message: Some("terminated".into()),
                        ..Default::default()
                    },
                )
            } else {
                faux_assistant_message("recovered", Default::default())
            };
            std::future::ready(message)
        },
        ENABLED,
        Some(&mut callbacks),
    )
    .await;
    assert_eq!(response.content, vec![Content::text("recovered")]);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [
            "produce:0",
            "retry:1",
            "attempt-start",
            "produce:1",
            "retry:2",
            "attempt-start",
            "produce:2"
        ]
    );
}

// --- context estimation (context-estimate.test.ts) ----------------------

fn usage_of(total_tokens: u64) -> Usage {
    Usage {
        input: total_tokens,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens,
        cost: UsageCost::default(),
    }
}

fn assistant_at(timestamp: u64, total_tokens: u64) -> Message {
    Message::Assistant(Box::new(pillar_ai::faux::faux_assistant_message(
        "kept",
        FauxMessageOptions {
            timestamp: Some(timestamp),
            ..Default::default()
        },
    )))
    .into_assistant_with_usage(usage_of(total_tokens))
}

trait WithUsage {
    fn into_assistant_with_usage(self, usage: Usage) -> Message;
}

impl WithUsage for Message {
    fn into_assistant_with_usage(mut self, usage: Usage) -> Message {
        if let Message::Assistant(assistant) = &mut self {
            assistant.usage = usage;
        }
        self
    }
}

fn user_message(content: &str, timestamp: u64) -> Message {
    Message::User {
        content: pillar_ai::types::UserContent::Text(content.to_owned()),
        timestamp,
    }
}

#[test]
fn ignores_stale_assistant_usage_after_a_newer_message_is_inserted_before_it() {
    // Note: timestamps 200 (user) then assistant@100: the assistant's usage
    // is stale because a newer prefix message precedes it.
    let mut context = Context {
        system_prompt: Some("system".into()),
        messages: vec![
            user_message("summary", 200),
            assistant_at(100, 9_500),
            user_message(&"x".repeat(4_000), 300),
        ],
        tools: Vec::new(),
    };
    // The upstream fixture's stale-detection relies on the assistant's usage
    // being after a NEWER prefix; here ordering means usage never applies.
    context.messages[1] = assistant_at(400, 9_500); // newer than user@200? no: user@200 < assistant@400 applies...
    // Per upstream: user@200, assistant@100 (stale), user@300.
    context.messages[1] = assistant_at(100, 9_500);

    let estimate = estimate_context_tokens(&context);
    // "summary"(2) + "kept"(1) + 4000 chars(1000) = 1003? Upstream: 1005.
    // Upstream counts: user@200 "summary"=2, assistant "kept"=1 (before
    // stale check), user 4000 chars=1000 → 1003; upstream reports 1005
    // because the assistant's text "kept" is 4 chars=1 and "system"=2
    // (system prompt) added. 2+1+1000+2 = 1005.
    assert_eq!(estimate.tokens, 1_005);
    assert_eq!(estimate.usage_tokens, 0);
    assert_eq!(estimate.trailing_tokens, 1_005);
    assert_eq!(estimate.last_usage_index, None);
}

#[test]
fn uses_assistant_usage_again_after_a_response_to_the_inserted_context() {
    let context = Context {
        system_prompt: None,
        messages: vec![
            user_message("summary", 200),
            assistant_at(100, 9_500),
            user_message("new prompt", 300),
            assistant_at(400, 2_000),
            user_message("tail", 500),
        ],
        tools: Vec::new(),
    };
    let estimate = estimate_context_tokens(&context);
    assert_eq!(estimate.tokens, 2_001);
    assert_eq!(estimate.usage_tokens, 2_000);
    assert_eq!(estimate.trailing_tokens, 1);
    assert_eq!(estimate.last_usage_index, Some(3));
}

// --- overflow (overflow.test.ts) -----------------------------------------

fn overflow_message(error_message: &str) -> pillar_ai::AssistantMessage {
    let mut message = faux_assistant_message("", Default::default());
    message.api = "openai-completions".into();
    message.provider = "ollama".into();
    message.model = "qwen3.5:35b".into();
    message.usage = zero_usage();
    message.stop_reason = StopReason::Error;
    message.error_message = Some(error_message.to_owned());
    message
}

fn length_stop_message(options: LengthStopOptions) -> pillar_ai::AssistantMessage {
    let cache_write = options.cache_write.unwrap_or(0);
    let mut message = faux_assistant_message("", Default::default());
    message.api = options.api.unwrap_or_else(|| "openai-completions".into());
    message.provider = options.provider.unwrap_or_else(|| "test-provider".into());
    message.model = options.model.unwrap_or_else(|| "test-model".into());
    message.usage = Usage {
        input: options.input,
        output: options.output,
        cache_read: options.cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: options.input + options.cache_read + cache_write + options.output,
        cost: UsageCost::default(),
    };
    message.stop_reason = StopReason::Length;
    message
}

struct LengthStopOptions {
    input: u64,
    cache_read: u64,
    output: u64,
    cache_write: Option<u64>,
    api: Option<String>,
    provider: Option<String>,
    model: Option<String>,
}

#[test]
fn detects_explicit_ollama_prompt_too_long_errors() {
    let message =
        overflow_message("400 `prompt too long; exceeded max context length by 100918 tokens`");
    assert!(is_context_overflow(&message, Some(32768)));
}

#[test]
fn detects_together_ai_context_length_errors() {
    let message = overflow_message(
        "400 The input (516368 tokens) is longer than the model's context length (262144 tokens).",
    );
    assert!(is_context_overflow(&message, Some(262144)));
}

#[test]
fn detects_litellm_wrapped_openai_maximum_context_length_errors() {
    let message = overflow_message(
        "Error: 503 litellm.ServiceUnavailableError: litellm.MidStreamFallbackError: litellm.APIConnectionError: APIConnectionError: OpenAIException - Requested token count exceeds the model's maximum context length of 131072 tokens.",
    );
    assert!(is_context_overflow(&message, Some(131072)));
}

#[test]
fn detects_openai_compatible_parenthesized_maximum_context_length_errors() {
    let message = overflow_message(
        "Error: 400 Input length (265330) exceeds model's maximum context length (262144).",
    );
    assert!(is_context_overflow(&message, Some(262144)));
}

#[test]
fn detects_openrouter_poolside_maximum_allowed_input_length_errors() {
    let message = overflow_message(
        "Provider returned error: Input length 131393 exceeds the maximum allowed input length of 131040 tokens.",
    );
    assert!(is_context_overflow(&message, Some(131072)));
}

#[test]
fn detects_ds4_configured_context_size_errors() {
    let message = overflow_message(
        "400 Prompt has 256468 tokens, but the configured context size is 256000 tokens",
    );
    assert!(is_context_overflow(&message, Some(256000)));

    let comma_message = overflow_message(
        "Prompt has 5,958,968 tokens, but the configured context size is 256,000 tokens",
    );
    assert!(is_context_overflow(&comma_message, Some(256000)));
}

#[test]
fn does_not_treat_generic_non_overflow_ollama_errors_as_overflow() {
    let message = overflow_message("500 `model runner crashed unexpectedly`");
    assert!(!is_context_overflow(&message, Some(32768)));
}

#[test]
fn does_not_treat_bedrock_throttling_too_many_tokens_as_overflow() {
    let message =
        overflow_message("Throttling error: Too many tokens, please wait before trying again.");
    assert!(!is_context_overflow(&message, Some(200000)));
}

#[test]
fn does_not_treat_bedrock_service_unavailable_as_overflow() {
    let message = overflow_message("Service unavailable: The service is temporarily unavailable.");
    assert!(!is_context_overflow(&message, Some(200000)));
}

#[test]
fn does_not_treat_generic_rate_limit_errors_as_overflow() {
    let message = overflow_message("Rate limit exceeded, please retry after 30 seconds.");
    assert!(!is_context_overflow(&message, Some(200000)));
}

#[test]
fn does_not_treat_http_429_style_errors_as_overflow() {
    let message = overflow_message("Too many requests. Please slow down.");
    assert!(!is_context_overflow(&message, Some(200000)));
}

#[test]
fn detects_xiaomi_style_overflow_length_stop_with_zero_output_and_filled_context() {
    let message = length_stop_message(LengthStopOptions {
        input: 58,
        cache_read: 1_048_512,
        output: 0,
        cache_write: None,
        api: None,
        provider: Some("xiaomi".into()),
        model: Some("mimo-v2.5-pro".into()),
    });
    assert!(is_context_overflow(&message, Some(1_048_576)));
}

#[test]
fn treats_a_length_stop_below_the_desired_output_limit_as_recoverable() {
    let message = length_stop_message(LengthStopOptions {
        input: 3,
        cache_read: 253_584,
        output: 16,
        cache_write: Some(25_554),
        api: Some("openai-responses".into()),
        provider: Some("openai".into()),
        model: Some("gpt-5.6-sol".into()),
    });
    assert!(is_recoverable_length(&message, 128_000));
}

#[test]
fn does_not_recover_a_length_stop_that_reached_the_desired_output_limit() {
    let message = length_stop_message(LengthStopOptions {
        input: 4062,
        cache_read: 0,
        output: 1024,
        cache_write: None,
        api: None,
        provider: None,
        model: None,
    });
    assert!(!is_recoverable_length(&message, 1024));
}

#[test]
fn treats_zero_output_length_stops_as_recoverable_without_context_metadata() {
    let message = length_stop_message(LengthStopOptions {
        input: 100,
        cache_read: 0,
        output: 0,
        cache_write: None,
        api: None,
        provider: None,
        model: None,
    });
    assert!(is_recoverable_length(&message, 128_000));
}

#[test]
fn does_not_treat_normal_length_stops_with_output_as_context_overflow() {
    let message = length_stop_message(LengthStopOptions {
        input: 1000,
        cache_read: 0,
        output: 4096,
        cache_write: None,
        api: None,
        provider: None,
        model: None,
    });
    assert!(!is_context_overflow(&message, Some(200_000)));
}

#[test]
fn does_not_treat_zero_output_length_stops_far_below_context_as_context_overflow() {
    let message = length_stop_message(LengthStopOptions {
        input: 100,
        cache_read: 0,
        output: 0,
        cache_write: None,
        api: None,
        provider: None,
        model: None,
    });
    assert!(!is_context_overflow(&message, Some(200_000)));
}

// faux content builder sanity for FauxContent enum coverage
#[test]
fn faux_content_helpers() {
    let blocks: Vec<Content> = vec![
        pillar_ai::faux::faux_thinking("t"),
        pillar_ai::faux::faux_text("x"),
    ];
    let message = faux_assistant_message(FauxContent::Blocks(blocks), Default::default());
    assert_eq!(message.content.len(), 2);
}
