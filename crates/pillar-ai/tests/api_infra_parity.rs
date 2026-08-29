//! Port of the upstream unit tests for the shared provider-request
//! infrastructure (pi v0.84.3): provider-retry.test.ts, error-body.test.ts,
//! transform-messages-copilot-openai-to-anthropic.test.ts, and the
//! module-level cases of constrained-sampling.test.ts. One Rust test per
//! upstream test case, same names in comments.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pillar_ai::constrained_sampling::{
    GrammarToolInputJsonBuffer, append_grammar_tool_input_json_delta,
    get_json_schema_tool_parameters, make_strict_json_schema, resolve_json_schema_strict_sampling,
};
use pillar_ai::error_body::{
    MAX_PROVIDER_ERROR_BODY_CHARS, format_provider_error, normalize_provider_error,
};
use pillar_ai::provider_env::get_provider_env_value;
use pillar_ai::provider_retry::{
    ProviderRequestError, ProviderRetryOptions, retry_provider_request,
};
use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse, headers_to_record};
use pillar_ai::types::{
    AssistantMessage, ConstrainedSamplingConfig, ConstrainedStrictness, Model, StopReason, Tool,
};
use pillar_ai::{AbortSignal, Message, ProviderHeaders, transform_messages};

// --- Helpers -------------------------------------------------------------

fn provider_error(status: Option<u16>, headers: Vec<(&str, &str)>) -> ProviderRequestError {
    ProviderRequestError::http(
        status.expect("test errors always carry a status"),
        headers
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        format!("Provider error: {status:?}"),
    )
}

fn make_model() -> Model {
    Model {
        id: "claude-sonnet-4.6".to_string(),
        name: "Claude Sonnet 4.6".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "github-copilot".to_string(),
        base_url: "https://api.individual.githubcopilot.com".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string(), "image".to_string()],
        cost: Default::default(),
        context_window: 128_000,
        max_tokens: 16_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn make_assistant_message(
    api: &str,
    model: &str,
    content: Vec<pillar_ai::types::Content>,
) -> AssistantMessage {
    AssistantMessage {
        content,
        api: api.to_string(),
        provider: "github-copilot".to_string(),
        model: model.to_string(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Default::default(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

/// Normalize function matching what anthropic.ts uses.
fn anthropic_normalize_tool_call_id(
    id: &str,
    _model: &Model,
    _source: &AssistantMessage,
) -> String {
    let normalized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    normalized.chars().take(64).collect()
}

fn make_tool(
    parameters: serde_json::Value,
    constrained_sampling: Option<ConstrainedSamplingConfig>,
) -> Tool {
    Tool {
        name: "sample_tool".to_string(),
        description: "Sample tool".to_string(),
        parameters,
        constrained_sampling,
    }
}

// --- provider-retry.test.ts ----------------------------------------------

// "retries retryable provider errors"
#[tokio::test]
async fn retries_retryable_provider_errors() {
    let calls = Arc::new(Mutex::new(0u32));
    let calls_for_request = Arc::clone(&calls);
    let result: Result<&str, ProviderRequestError> = retry_provider_request(
        move || {
            let calls = Arc::clone(&calls_for_request);
            async move {
                let mut guard = calls.lock().unwrap();
                *guard += 1;
                if *guard == 1 {
                    Err(provider_error(Some(429), vec![("retry-after-ms", "5")]))
                } else {
                    Ok("ok")
                }
            }
        },
        ProviderRetryOptions {
            max_retries: Some(1),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(result.unwrap(), "ok");
    assert_eq!(*calls.lock().unwrap(), 2);
}

// "does not retry errors the provider marks as non-retryable"
#[tokio::test]
async fn does_not_retry_errors_the_provider_marks_as_non_retryable() {
    let calls = Arc::new(Mutex::new(0u32));
    let calls_for_request = Arc::clone(&calls);
    let error = provider_error(Some(429), vec![("x-should-retry", "false")]);
    let error_for_assert = error.clone();
    let result: Result<&str, ProviderRequestError> = retry_provider_request(
        move || {
            let calls = Arc::clone(&calls_for_request);
            let error = error.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Err(error)
            }
        },
        ProviderRetryOptions {
            max_retries: Some(2),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(result.unwrap_err().message, error_for_assert.message);
    assert_eq!(*calls.lock().unwrap(), 1);
}

// "rejects a provider-requested retry delay above the limit"
#[tokio::test]
async fn rejects_a_provider_requested_retry_delay_above_the_limit() {
    let calls = Arc::new(Mutex::new(0u32));
    let calls_for_request = Arc::clone(&calls);
    let result: Result<&str, ProviderRequestError> = retry_provider_request(
        move || {
            let calls = Arc::clone(&calls_for_request);
            async move {
                *calls.lock().unwrap() += 1;
                Err(provider_error(Some(429), vec![("retry-after", "277403")]))
            }
        },
        ProviderRetryOptions {
            max_retries: Some(1),
            max_retry_delay_ms: Some(1000),
            ..Default::default()
        },
    )
    .await;

    let error = result.unwrap_err();
    assert!(
        error
            .message
            .starts_with("Server requested 277403s retry delay (max: 1s)."),
        "{error}"
    );
    assert_eq!(*calls.lock().unwrap(), 1);
}

// "allows disabling the provider-requested retry delay cap"
#[tokio::test]
async fn allows_disabling_the_provider_requested_retry_delay_cap() {
    let calls = Arc::new(Mutex::new(0u32));
    let calls_for_request = Arc::clone(&calls);
    let result: Result<&str, ProviderRequestError> = retry_provider_request(
        move || {
            let calls = Arc::clone(&calls_for_request);
            async move {
                let mut guard = calls.lock().unwrap();
                *guard += 1;
                if *guard == 1 {
                    Err(provider_error(Some(429), vec![("retry-after", "0.01")]))
                } else {
                    Ok("ok")
                }
            }
        },
        ProviderRetryOptions {
            max_retries: Some(1),
            max_retry_delay_ms: Some(0),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(result.unwrap(), "ok");
    assert_eq!(*calls.lock().unwrap(), 2);
}

// "aborts a provider-requested retry delay"
#[tokio::test]
async fn aborts_a_provider_requested_retry_delay() {
    let controller = AbortSignal::new();
    let controller_for_task = controller.clone();
    let calls = Arc::new(Mutex::new(0u32));
    let calls_for_request = Arc::clone(&calls);
    let task = tokio::spawn(async move {
        retry_provider_request(
            move || {
                let calls = Arc::clone(&calls_for_request);
                async move {
                    *calls.lock().unwrap() += 1;
                    Err(provider_error(Some(429), vec![("retry-after", "277403")]))
                }
            },
            ProviderRetryOptions {
                max_retries: Some(2),
                max_retry_delay_ms: Some(0),
                signal: Some(controller_for_task),
            },
        )
        .await
    });

    // Let the first request fail and the backoff sleep start, then abort.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    controller.abort(None);

    let result: Result<&str, ProviderRequestError> = task.await.unwrap();
    assert!(result.unwrap_err().aborted);
    assert_eq!(*calls.lock().unwrap(), 1);
}

// --- error-body.test.ts --------------------------------------------------

// "extracts status and body from a Mistral-shaped error"
#[test]
fn extracts_status_and_body() {
    let norm = normalize_provider_error(
        "Mistral request failed",
        Some(403),
        Some(r#"{"error":"blocked by gateway WAF"}"#),
    );

    assert_eq!(norm.status, Some(403));
    assert_eq!(
        norm.body.as_deref(),
        Some(r#"{"error":"blocked by gateway WAF"}"#)
    );
    assert!(!norm.message_carries_body);
}

// "reads the parsed body off an openai APIError when the message is opaque"
// (Rust: the transport already extracted the body; only the observable
// normalization is ported.)
#[test]
fn reads_the_parsed_body_when_the_message_is_opaque() {
    let norm = normalize_provider_error(
        "403 status code (no body)",
        Some(403),
        Some(r#"{"error":"blocked by gateway WAF"}"#),
    );

    assert_eq!(norm.status, Some(403));
    assert_eq!(
        norm.body.as_deref(),
        Some(r#"{"error":"blocked by gateway WAF"}"#)
    );
    assert!(!norm.message_carries_body);
}

// "preserves the message when @google/genai already folds the body into it"
#[test]
fn preserves_the_message_when_it_already_carries_the_body() {
    let body =
        serde_json::json!({ "error": { "code": 403, "message": "Permission denied" } }).to_string();
    let norm = normalize_provider_error(body.clone(), Some(403), Some(&body));

    assert_eq!(norm.status, Some(403));
    assert!(norm.message_carries_body);
    assert_eq!(norm.message, body);
}

// "treats an empty parsed body object as no body"
#[test]
fn treats_an_empty_body_as_no_body() {
    let norm = normalize_provider_error("403 status code (no body)", Some(403), Some(""));

    assert_eq!(norm.body, None);
    assert!(norm.message_carries_body);
}

// "truncates the body at the cap"
#[test]
fn truncates_the_body_at_the_cap() {
    let long_body = "x".repeat(MAX_PROVIDER_ERROR_BODY_CHARS + 50);
    let norm = normalize_provider_error("failed", Some(500), Some(&long_body));

    let body = norm.body.expect("body present");
    assert!(body.contains("... [truncated 50 chars]"), "{body}");
    assert!(body.len() < long_body.len());
}

// "sets messageCarriesBody when the message already contains the extracted body"
#[test]
fn sets_message_carries_body_when_the_message_contains_the_body() {
    let norm = normalize_provider_error(
        "500: upstream exploded",
        Some(500),
        Some("upstream exploded"),
    );

    assert!(norm.message_carries_body);
}

// "surfaces status and body without a prefix" + "applies a provider prefix
// with status and body"
#[test]
fn formats_status_and_body() {
    let norm = normalize_provider_error(
        "403 status code (no body)",
        Some(403),
        Some(r#"{"error":"blocked by gateway WAF"}"#),
    );

    let formatted = format_provider_error(&norm, None);
    assert!(formatted.contains("403"), "{formatted}");
    assert!(formatted.contains("blocked by gateway WAF"), "{formatted}");
    assert_ne!(formatted, "403 status code (no body)");

    assert_eq!(
        format_provider_error(&norm, Some("OpenAI API error")),
        r#"OpenAI API error (403): {"error":"blocked by gateway WAF"}"#
    );
}

// "preserves the message (with prefix + status) when it already carries the body"
#[test]
fn formats_with_prefix_when_message_carries_body() {
    let body = serde_json::json!({ "error": { "message": "Permission denied" } }).to_string();
    let norm = normalize_provider_error(body.clone(), Some(403), Some(&body));

    assert_eq!(
        format_provider_error(&norm, Some("OpenAI API error")),
        format!("OpenAI API error (403): {body}")
    );
}

// "returns the bare message for a non-Error value" (Rust: a non-Error throw
// is just the message the caller already stringified.)
#[test]
fn returns_the_bare_message() {
    let norm = normalize_provider_error(r#"{"reason":"boom"}"#, None, None);

    assert_eq!(norm.status, None);
    assert_eq!(norm.body, None);
    assert_eq!(format_provider_error(&norm, None), r#"{"reason":"boom"}"#);
}

// --- transform-messages-copilot-openai-to-anthropic.test.ts --------------

// "converts thinking blocks to plain text when source model differs"
#[test]
fn converts_thinking_blocks_to_plain_text_when_source_model_differs() {
    let model = make_model();
    let messages = vec![
        Message::User {
            content: pillar_ai::types::UserContent::Text("hello".to_string()),
            timestamp: 1,
        },
        Message::Assistant(Box::new(make_assistant_message(
            "openai-completions",
            "gpt-4o",
            vec![
                pillar_ai::types::Content::Thinking {
                    thinking: "Let me think about this...".to_string(),
                    thinking_signature: Some("reasoning_content".to_string()),
                    redacted: None,
                },
                pillar_ai::types::Content::text("Hi there!"),
            ],
        ))),
    ];

    let result = transform_messages(messages, &model, Some(&anthropic_normalize_tool_call_id));
    let assistant = result
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant.as_ref()),
            _ => None,
        })
        .next()
        .expect("assistant present");

    let thinking_count = assistant
        .content
        .iter()
        .filter(|block| matches!(block, pillar_ai::types::Content::Thinking { .. }))
        .count();
    let text_count = assistant
        .content
        .iter()
        .filter(|block| matches!(block, pillar_ai::types::Content::Text { .. }))
        .count();
    assert_eq!(thinking_count, 0);
    assert!(text_count >= 2);
}

// "removes thoughtSignature from tool calls when migrating between models"
#[test]
fn removes_thought_signature_from_tool_calls_when_migrating_between_models() {
    let model = make_model();
    let messages = vec![
        Message::User {
            content: pillar_ai::types::UserContent::Text("run a command".to_string()),
            timestamp: 1,
        },
        Message::Assistant(Box::new(AssistantMessage {
            stop_reason: StopReason::ToolUse,
            ..make_assistant_message(
                "openai-responses",
                "gpt-5",
                vec![pillar_ai::types::Content::ToolCall {
                    id: "call_123".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({ "command": "ls" }),
                    thought_signature: Some(
                        r#"{"type":"reasoning.encrypted","id":"call_123","data":"encrypted"}"#
                            .to_string(),
                    ),
                    namespace: None,
                }],
            )
        })),
        Message::ToolResult(Box::new(pillar_ai::types::ToolResultMessage {
            tool_call_id: "call_123".to_string(),
            tool_name: "bash".to_string(),
            content: vec![pillar_ai::types::Content::text("output")],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 1,
        })),
    ];

    let result = transform_messages(messages, &model, Some(&anthropic_normalize_tool_call_id));
    let assistant = result
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant.as_ref()),
            _ => None,
        })
        .next()
        .expect("assistant present");
    let tool_call = assistant
        .content
        .iter()
        .find_map(|block| match block {
            pillar_ai::types::Content::ToolCall {
                thought_signature, ..
            } => Some(thought_signature.clone()),
            _ => None,
        })
        .expect("tool call present");

    assert_eq!(tool_call, None);
}

// "adds synthetic tool results for trailing orphaned tool calls"
#[test]
fn adds_synthetic_tool_results_for_trailing_orphaned_tool_calls() {
    let model = make_model();
    let messages = vec![
        Message::User {
            content: pillar_ai::types::UserContent::Text("read the file".to_string()),
            timestamp: 1,
        },
        Message::Assistant(Box::new(AssistantMessage {
            stop_reason: StopReason::ToolUse,
            ..make_assistant_message(
                "openai-responses",
                "gpt-5",
                vec![pillar_ai::types::Content::ToolCall {
                    id: "call_123|fc_123".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({ "path": "README.md" }),
                    thought_signature: None,
                    namespace: None,
                }],
            )
        })),
    ];

    let result = transform_messages(messages, &model, Some(&anthropic_normalize_tool_call_id));
    let last = result.last().expect("messages present");
    let Message::ToolResult(tool_result) = last else {
        panic!("expected a trailing tool result, got {last:?}");
    };

    assert_eq!(tool_result.tool_call_id, "call_123_fc_123");
    assert_eq!(tool_result.tool_name, "read");
    assert!(tool_result.is_error);
    assert_eq!(
        tool_result.content,
        vec![pillar_ai::types::Content::text("No result provided")]
    );
}

// "adds synthetic results only for trailing tool calls that are still missing results"
#[test]
fn adds_synthetic_results_only_for_trailing_tool_calls_missing_results() {
    let model = make_model();
    let messages = vec![
        Message::User {
            content: pillar_ai::types::UserContent::Text("run commands".to_string()),
            timestamp: 1,
        },
        Message::Assistant(Box::new(AssistantMessage {
            stop_reason: StopReason::ToolUse,
            ..make_assistant_message(
                "openai-responses",
                "gpt-5",
                vec![
                    pillar_ai::types::Content::ToolCall {
                        id: "call_1|fc_1".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::json!({ "path": "README.md" }),
                        thought_signature: None,
                        namespace: None,
                    },
                    pillar_ai::types::Content::ToolCall {
                        id: "call_2|fc_2".to_string(),
                        name: "bash".to_string(),
                        arguments: serde_json::json!({ "command": "pwd" }),
                        thought_signature: None,
                        namespace: None,
                    },
                ],
            )
        })),
        Message::ToolResult(Box::new(pillar_ai::types::ToolResultMessage {
            tool_call_id: "call_1|fc_1".to_string(),
            tool_name: "read".to_string(),
            content: vec![pillar_ai::types::Content::text("done")],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 1,
        })),
    ];

    let result = transform_messages(messages, &model, Some(&anthropic_normalize_tool_call_id));
    let synthetic: Vec<_> = result
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(tool_result) if tool_result.is_error => Some(tool_result.as_ref()),
            _ => None,
        })
        .collect();

    assert_eq!(synthetic.len(), 1);
    assert_eq!(synthetic[0].tool_call_id, "call_2_fc_2");
    assert_eq!(synthetic[0].tool_name, "bash");
    assert_eq!(
        synthetic[0].content,
        vec![pillar_ai::types::Content::text("No result provided")]
    );
}

// --- constrained-sampling.test.ts (module-level cases) -------------------

// "derives strict provider schemas without changing tool definitions"
// divergence: serde_json maps are ordered, so `required` is asserted in key
// order rather than upstream's insertion order (the order is not observable
// to providers).
#[test]
fn derives_strict_provider_schemas_without_changing_tool_definitions() {
    let parameters = serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "offset": { "type": "number" },
            "metadata": {
                "type": "object",
                "properties": { "enabled": { "type": "boolean" } }
            },
            "nullable": { "anyOf": [ { "type": "string" }, { "type": "null" } ] }
        },
        "required": ["path", "metadata"]
    });
    let original = parameters.clone();

    let strict = make_strict_json_schema(&parameters).unwrap();

    assert_eq!(parameters, original);
    let mut required: Vec<String> = strict["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap().to_string())
        .collect();
    required.sort();
    assert_eq!(required, vec!["metadata", "nullable", "offset", "path"]);
    assert_eq!(strict["additionalProperties"], serde_json::json!(false));
    assert_eq!(
        strict["properties"]["offset"],
        serde_json::json!({ "anyOf": [ { "type": "number" }, { "type": "null" } ] })
    );
    assert_eq!(
        strict["properties"]["metadata"],
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["enabled"],
            "properties": { "enabled": { "anyOf": [ { "type": "boolean" }, { "type": "null" } ] } }
        })
    );
    assert_eq!(
        strict["properties"]["nullable"],
        serde_json::json!({ "anyOf": [ { "type": "string" }, { "type": "null" } ] })
    );
}

// "falls back or rejects schemas that cannot be safely converted"
#[test]
fn falls_back_or_rejects_schemas_that_cannot_be_safely_converted() {
    let cases: Vec<(serde_json::Value, &str)> = vec![
        (
            serde_json::json!({
                "type": "object",
                "properties": { "metadata": { "type": "object", "additionalProperties": { "type": "string" } } }
            }),
            "additionalProperties is unsupported",
        ),
        (
            serde_json::json!({
                "allOf": [
                    { "type": "object", "properties": { "a": { "type": "string" } } },
                    { "type": "object", "properties": { "b": { "type": "number" } } }
                ]
            }),
            "allOf schemas are unsupported",
        ),
        (
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "anyOf": [ { "type": "object", "properties": { "nested": { "type": "string" } } }, { "type": "null" } ] }
                }
            }),
            "object and array unions are unsupported",
        ),
        (
            serde_json::json!({
                "type": "object",
                "properties": { "child": { "$ref": "https://example.com/child.json" } },
                "required": ["child"]
            }),
            "$ref schemas are unsupported",
        ),
    ];

    for (parameters, error) in cases {
        assert!(
            make_strict_json_schema(&parameters).is_err_and(|e| e.0.contains(error)),
            "expected {error} for {parameters}"
        );

        let mut tool = make_tool(
            parameters,
            Some(ConstrainedSamplingConfig::JsonSchema {
                strict: ConstrainedStrictness::Prefer,
            }),
        );
        assert_eq!(
            resolve_json_schema_strict_sampling(&tool, true).unwrap(),
            None
        );

        tool.constrained_sampling = Some(ConstrainedSamplingConfig::JsonSchema {
            strict: ConstrainedStrictness::Require,
        });
        let failure = resolve_json_schema_strict_sampling(&tool, true).unwrap_err();
        assert!(failure.to_string().contains(error), "{failure}");
    }
}

// "keeps grammar input JSON deltas append-only"
#[test]
fn keeps_grammar_input_json_deltas_append_only() {
    let mut buffer = GrammarToolInputJsonBuffer::default();
    let first = append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"", false).unwrap();
    let second =
        append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"\nb", true).unwrap();

    let combined = format!("{}{}", first.unwrap(), second.unwrap());
    let parsed: serde_json::Value = serde_json::from_str(&combined).unwrap();
    assert_eq!(parsed, serde_json::json!({ "payload": "a\"\nb" }));
    assert_eq!(
        append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"\nb", true).unwrap(),
        None
    );
    let changed = append_grammar_tool_input_json_delta(&mut buffer, "payload", "changed", true);
    assert!(changed.is_err_and(|e| {
        e.to_string()
            .contains("grammar tool input for property \"payload\" changed after it was closed")
    }));
}

// Non-upstream unit coverage for the remaining constrained-sampling helpers.
#[test]
fn grammar_sampling_resolution_and_tool_parameters() {
    // strict conversion on request when the provider supports it
    let tool = make_tool(
        serde_json::json!({
            "type": "object",
            "properties": { "payload": { "type": "string" } },
            "required": ["payload"],
            "additionalProperties": false
        }),
        Some(ConstrainedSamplingConfig::JsonSchema {
            strict: ConstrainedStrictness::Prefer,
        }),
    );
    assert_eq!(
        resolve_json_schema_strict_sampling(&tool, true).unwrap(),
        Some(true)
    );

    let strict_parameters = get_json_schema_tool_parameters(&tool, Some(true)).unwrap();
    assert_eq!(
        strict_parameters["additionalProperties"],
        serde_json::json!(false)
    );

    // without opt-in, parameters pass through untouched
    let plain = make_tool(
        serde_json::json!({ "type": "object", "properties": {} }),
        None,
    );
    assert_eq!(
        get_json_schema_tool_parameters(&plain, None).unwrap(),
        plain.parameters
    );
}

// --- provider-env --------------------------------------------------------

#[test]
fn provider_env_prefers_scoped_overrides_and_falls_back_to_process_env() {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("PILLAR_TEST_PROVIDER_VAR".to_string(), "scoped".to_string());
    env.insert("PILLAR_TEST_EMPTY_VAR".to_string(), String::new());

    assert_eq!(
        get_provider_env_value("PILLAR_TEST_PROVIDER_VAR", Some(&env)).as_deref(),
        Some("scoped")
    );
    // An empty override counts as unset (upstream `||` semantics)...
    assert_eq!(
        get_provider_env_value("PILLAR_TEST_EMPTY_VAR", Some(&env)),
        None
    );
    // ...and falls through to the process environment.
    assert!(get_provider_env_value("PATH", Some(&env)).is_some());
    assert_eq!(
        get_provider_env_value("PILLAR_TEST_UNSET_VAR", Some(&env)),
        None
    );
}

// --- transport -----------------------------------------------------------

#[tokio::test]
async fn fetch_responses_stream_through_the_trait() {
    struct MockFetch;
    #[async_trait::async_trait]
    impl FetchFn for MockFetch {
        async fn fetch(
            &self,
            request: FetchRequest,
        ) -> Result<FetchResponse, pillar_ai::error::AiError> {
            assert_eq!(request.method, "POST");
            assert_eq!(request.url, "https://upstream.test/v1/chat");
            assert_eq!(
                request.headers,
                vec![("authorization".to_string(), "Bearer test".to_string())]
            );
            let body = request.body.expect("POST body");
            assert!(body.starts_with(b"{"));

            let chunks: Vec<Result<Vec<u8>, pillar_ai::error::AiError>> =
                vec![Ok(b"he".to_vec()), Ok(b"llo".to_vec())];
            Ok(FetchResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "text/plain".to_string())],
                body: Box::pin(futures::stream::iter(chunks)),
            })
        }
    }

    let fetch: pillar_ai::SharedFetchFn = Arc::new(MockFetch);
    let request = FetchRequest::post("https://upstream.test/v1/chat", b"{}".to_vec())
        .with_header("authorization", "Bearer test");
    let response = fetch.fetch(request).await.unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(response.header("CONTENT-TYPE"), Some("text/plain"));
    assert_eq!(response.text().await.unwrap(), "hello");
}

#[test]
fn headers_to_record_collapses_duplicates() {
    let headers = vec![
        ("x-a".to_string(), "1".to_string()),
        ("x-b".to_string(), "2".to_string()),
        ("x-a".to_string(), "3".to_string()),
    ];
    let record = headers_to_record(&headers);
    assert_eq!(record.get("x-a"), Some(&"3".to_string()));
    assert_eq!(record.get("x-b"), Some(&"2".to_string()));
}

#[test]
fn provider_headers_to_record_drops_suppressed_and_empty() {
    let mut headers = ProviderHeaders::new();
    headers.insert("x-keep".to_string(), Some("yes".to_string()));
    headers.insert("x-suppressed".to_string(), None);

    let record = pillar_ai::headers::provider_headers_to_record(Some(&headers)).unwrap();
    assert_eq!(record.get("x-keep"), Some(&"yes".to_string()));
    assert!(!record.contains_key("x-suppressed"));

    let mut empty = ProviderHeaders::new();
    empty.insert("x-suppressed".to_string(), None);
    assert_eq!(
        pillar_ai::headers::provider_headers_to_record(Some(&empty)),
        None
    );
    assert_eq!(pillar_ai::headers::provider_headers_to_record(None), None);
}

#[test]
fn reqwest_fetch_constructs() {
    pillar_ai::transport::ReqwestFetch::new().expect("default transport builds");
}
