//! Port of packages/ai/src/api/bedrock-converse-stream.ts (pi v0.84.3).
//!
//! Amazon Bedrock Converse streaming adapter.
//!
//! divergence: upstream drives the AWS SDK's `BedrockRuntimeClient` (SigV4
//! signing, credential chain, HTTP/2 handler, Smithy middleware stack). The
//! Rust port builds the ConverseStream JSON payload directly and sends it
//! through `FetchFn`, decoding the `application/vnd.amazon.eventstream`
//! binary framing locally. Bearer-token auth (`Authorization: Bearer`,
//! upstream `config.token` + `authSchemePreference`) and the unauthenticated
//! `AWS_BEDROCK_SKIP_AUTH=1` path are fully self-sufficient. SigV4 signing
//! for AWS access keys / profiles is NOT ported: the resolved `ClientConfig`
//! (region/endpoint/profile/credentials priority) is computed with the same
//! rules and exposed for callers, but actual signing must come from a custom
//! `fetch` (see docs/INSTRUCTIONS.md #52 for the analogous google-vertex
//! divergence).
//!
//! Other divergences:
//! - CRC32 checks of eventstream frames are not verified (length framing is).
//! - A non-2xx HTTP response cannot be mapped to a modeled AWS exception
//!   name (the SDK extracts it from `x-amzn-errortype`), so the failure
//!   diagnostic omits `errorCode` for transport-level failures and
//!   `errorMessage` carries the raw body instead of a prefixed message.
//! - Response headers reach `onResponse` from the raw HTTP response, which
//!   is a superset of what the SDK's `$metadata` preserves.

use serde_json::{Map, Value, json};

use crate::api::{
    OnPayloadFn, OnResponseFn, ProviderResponseInfo, get_pi_user_agent, impl_from_request_options,
    resolve_cache_retention,
};
use crate::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_json_schema_strict_sampling,
};
use crate::diagnostics::append_assistant_message_diagnostic;
use crate::error_body::normalize_provider_error;
use crate::event_stream::assistant_message_event_stream;
use crate::json_parse::parse_streaming_json;
use crate::provider_env::get_provider_env_value;
use crate::text::sanitize_surrogates;
use crate::transform_messages::transform_messages;
use crate::types::AssistantMessageDiagnostic;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, CacheRetention, Content, Context, Message, Model,
    ProviderEnv, ProviderHeaders, StopReason, ThinkingBudgets, ThinkingLevel, Tool, Usage,
    UserContent,
};

fn default_fetch() -> crate::transport::SharedFetchFn {
    std::sync::Arc::new(
        crate::transport::ReqwestFetch::new()
            .unwrap_or_else(|error| panic!("default transport unavailable: {error}")),
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";

/// Matches the placeholder the Anthropic API path uses for redacted thinking.
const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";

/// Human-readable prefixes for Bedrock SDK exception names. The downstream
/// retry logic in agent-session matches patterns like `server.?error` and
/// `service.?unavailable`, so we preserve the legacy prefix format rather
/// than using the raw SDK exception name.
const BEDROCK_ERROR_PREFIXES: &[(&str, &str)] = &[
    ("InternalServerException", "Internal server error"),
    ("ModelStreamErrorException", "Model stream error"),
    ("ValidationException", "Validation error"),
    ("ThrottlingException", "Throttling error"),
    ("ServiceUnavailableException", "Service unavailable"),
];

/// Some models reject the account/profile's configured Bedrock data retention
/// mode (e.g. "data retention mode 'default' is not available for this
/// model"). Point users at the AWS docs explaining how to configure a
/// supported mode.
const BEDROCK_DATA_RETENTION_DOCS_URL: &str =
    "https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";

/// Over-long header values are dropped rather than truncated: a truncated
/// request id is not a request id.
const MAX_BEDROCK_DIAGNOSTIC_VALUE_CHARS: usize = 200;

// --- Options -------------------------------------------------------------------

/// Upstream `BedrockOptions`.
#[derive(Default)]
pub struct BedrockOptions {
    pub signal: Option<crate::AbortSignal>,
    pub api_key: Option<String>,
    pub fetch: Option<crate::transport::SharedFetchFn>,
    pub env: Option<ProviderEnv>,
    pub on_payload: Option<OnPayloadFn>,
    pub on_response: Option<OnResponseFn>,
    pub headers: Option<ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub region: Option<String>,
    pub profile: Option<String>,
    /// "auto" | "any" | "none" | {"type": "tool", "name": ...}
    pub tool_choice: Option<Value>,
    pub reasoning: Option<ThinkingLevel>,
    /// Custom token budgets per thinking level. Overrides default budgets.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Only supported by Claude 4.x models; enables interleaved tool use
    /// with extended thinking.
    pub interleaved_thinking: Option<bool>,
    /// Controls how Claude's thinking content is returned in responses:
    /// "summarized" (default) or "omitted".
    pub thinking_display: Option<String>,
    /// Key-value pairs attached to the inference request for cost
    /// allocation tagging (upstream `requestMetadata`).
    pub request_metadata: Option<Map<String, Value>>,
    /// Bearer token for Bedrock API key authentication. When set, bypasses
    /// SigV4 signing and sends `Authorization: Bearer <token>` instead.
    pub bearer_token: Option<String>,
}

impl_from_request_options!(BedrockOptions);
impl_from_request_options!(SimpleStreamOptions);

/// Upstream `SimpleStreamOptions` for bedrock-converse-stream.
#[derive(Default)]
pub struct SimpleStreamOptions {
    pub signal: Option<crate::AbortSignal>,
    pub api_key: Option<String>,
    pub fetch: Option<crate::transport::SharedFetchFn>,
    pub env: Option<ProviderEnv>,
    pub on_payload: Option<OnPayloadFn>,
    pub on_response: Option<OnResponseFn>,
    pub headers: Option<ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_choice: Option<Value>,
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub cache_retention: Option<CacheRetention>,
}

/// Resolved Bedrock client configuration (upstream
/// `BedrockRuntimeClientConfig`, the parts pi sets). The Rust port does not
/// perform SigV4 signing; this struct carries the region/endpoint/profile/
/// credentials/bearer resolution so callers (and tests) can observe the
/// exact same priority rules as the upstream SDK client construction.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ClientConfig {
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub profile: Option<String>,
    pub credentials: Option<Credentials>,
    pub token: Option<String>,
    pub auth_scheme_preference: Option<Vec<String>>,
}

/// Upstream `BedrockRuntimeClientConfig["credentials"]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// Streaming scratch state for one content block (upstream `Block`): the
/// persisted content plus the wire block index, the partial-JSON tool-input
/// buffer, and buffered encrypted-reasoning chunks. Scratch fields are
/// stripped before a message is persisted.
#[derive(Debug, Clone)]
struct Block {
    content: Content,
    index: usize,
    partial_json: Option<String>,
    /// Scratch buffer for encrypted reasoning deltas, joined into
    /// `thinking_signature`.
    redacted_chunks: Option<Vec<Vec<u8>>>,
}

/// Mid-stream failure carrying optional SDK-style metadata so the catch path
/// can build the failure diagnostic (upstream: thrown exception with
/// `name`/`$metadata`).
struct BedrockStreamError {
    message: String,
    /// Modeled AWS error code (upstream `error.name` ending in "Exception").
    error_code: Option<String>,
    status: Option<u16>,
    request_id: Option<String>,
}

impl BedrockStreamError {
    fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error_code: None,
            status: None,
            request_id: None,
        }
    }
}

type RunResult = Result<(), BedrockStreamError>;

// --- Stream entry ---------------------------------------------------------------

/// Upstream `stream()` — returns an event stream; the request runs on a
/// spawned task against the default or injected transport.
pub fn stream(
    model: Model,
    context: Context,
    options: Option<BedrockOptions>,
) -> crate::event_stream::AssistantMessageEventStream {
    let stream = assistant_message_event_stream();
    let task_stream = stream.clone_stream();
    tokio::spawn(run_stream(
        model,
        context,
        options.unwrap_or_default(),
        task_stream,
    ));
    stream
}

fn fresh_output(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: crate::api::zeroed_usage(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
    }
}

fn fail_stream(
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    blocks: &mut [Block],
    error: &BedrockStreamError,
    aborted: bool,
    response_request_id: Option<String>,
) {
    // Finalize every block from the terminal path too: a stream can settle
    // without stopping each block (upstream catch block).
    finalize_streaming_blocks(blocks);
    sync_blocks_to_output(blocks, output);
    output.stop_reason = if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    output.error_message = Some(format_bedrock_error_message(error));
    if output.stop_reason == StopReason::Error {
        append_bedrock_failure_diagnostic(
            output,
            error.error_code.as_deref(),
            error.status,
            error.request_id.clone().or(response_request_id),
        );
    }
    stream.push(AssistantMessageEvent::Error {
        reason: output.stop_reason,
        error: output.clone(),
    });
    stream.end(Some(output.clone()));
}

/// Upstream `formatBedrockError` on the error struct: modeled exception
/// names get their human-readable prefix, everything else passes through
/// (the transport message already carries the HTTP body when present).
fn format_bedrock_error_message(error: &BedrockStreamError) -> String {
    if let Some(code) = &error.error_code {
        if let Some((_, prefix)) = BEDROCK_ERROR_PREFIXES.iter().find(|(name, _)| name == code) {
            return format!("{prefix}: {}", error.message);
        }
    }
    error.message.clone()
}

async fn run_stream(
    model: Model,
    context: Context,
    options: BedrockOptions,
    stream: crate::event_stream::AssistantMessageEventStream,
) {
    let mut output = fresh_output(&model);

    // Kept outside the error path so the catch can still correlate a
    // mid-stream failure: exceptions delivered as stream events carry no
    // HTTP metadata of their own (upstream `responseRequestId`).
    let mut response_request_id: Option<String> = None;
    let mut blocks: Vec<Block> = Vec::new();
    let result = run_stream_inner(
        &model,
        &context,
        &options,
        &mut output,
        &stream,
        &mut blocks,
        &mut response_request_id,
    )
    .await;
    if let Err(error) = result {
        let aborted = options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted());
        fail_stream(
            &mut output,
            &stream,
            &mut blocks,
            &error,
            aborted,
            // requestId priority: the exception's own metadata first, then
            // the response header captured before the failure.
            response_request_id,
        );
    }
}

async fn run_stream_inner(
    model: &Model,
    context: &Context,
    options: &BedrockOptions,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    blocks: &mut Vec<Block>,
    response_request_id: &mut Option<String>,
) -> RunResult {
    // Client config resolution (upstream client construction). The Rust port
    // does not SigV4-sign; the resolved config is surfaced for parity/tests
    // and for custom `fetch` implementations that need it.
    let _config = build_client_config(model, options);

    let strict_mode = supports_strict_mode(model);
    let cache_retention = resolve_cache_retention(options.cache_retention, options.env.as_ref());
    let inference_max_tokens = match options.max_tokens {
        Some(max_tokens) => Some(max_tokens),
        None if is_anthropic_claude_model(model) => Some(model.max_tokens),
        None => None,
    };

    let mut command_input = json!({
        "modelId": model.id,
        "messages": convert_messages(context, model, cache_retention, options.env.as_ref())?,
        "system": build_system_prompt(
            context.system_prompt.as_deref(),
            model,
            cache_retention,
            options.env.as_ref(),
        ),
        "inferenceConfig": {
            "maxTokens": inference_max_tokens,
            "temperature": options.temperature,
        },
        "toolConfig": convert_tool_config(
            &context.tools,
            options.tool_choice.as_ref(),
            strict_mode,
        )?,
        "additionalModelRequestFields": build_additional_model_request_fields(model, options),
    });
    if let Some(request_metadata) = &options.request_metadata {
        command_input["requestMetadata"] = Value::Object(request_metadata.clone());
    }
    if let Some(on_payload) = &options.on_payload {
        if let Some(next) = on_payload(model, command_input.clone()).await {
            command_input = next;
        }
    }

    // --- Send request (upstream `client.send(command, {abortSignal})`) ---
    let url = build_request_url(model, options)?;
    let mut headers = vec![
        ("User-Agent".to_string(), get_pi_user_agent()),
        (
            "accept".to_string(),
            "application/vnd.amazon.eventstream".to_string(),
        ),
        ("content-type".to_string(), "application/json".to_string()),
        (
            "x-amz-target".to_string(),
            "BedrockRuntime_20230418.ConverseStream".to_string(),
        ),
    ];
    // Upstream bearer-token auth: config.token + authSchemePreference
    // ["httpBearerAuth"] bypasses SigV4 signing entirely.
    let skip_auth = get_provider_env_value("AWS_BEDROCK_SKIP_AUTH", options.env.as_ref())
        .as_deref()
        == Some("1");
    let bearer_token = options
        .bearer_token
        .clone()
        .or_else(|| options.api_key.clone())
        .or_else(|| get_provider_env_value("AWS_BEARER_TOKEN_BEDROCK", options.env.as_ref()));
    if let Some(token) = bearer_token.filter(|_| !skip_auth) {
        headers.push(("authorization".to_string(), format!("Bearer {token}")));
    }
    // Caller-supplied headers; reserved SigV4/auth headers are silently
    // skipped (upstream build-step middleware).
    if let Some(custom_headers) = options.headers.as_ref() {
        for (key, value) in custom_headers {
            if let Some(value) = value {
                if !is_reserved_header(key) {
                    headers.push((key.clone(), value.clone()));
                }
            }
        }
    }

    let body = serde_json::to_vec(&command_input).map_err(|error| {
        BedrockStreamError::plain(format!("failed to serialize request body: {error}"))
    })?;

    let fetch = options.fetch.clone().unwrap_or_else(default_fetch);
    let request = crate::transport::FetchRequest {
        method: "POST".to_string(),
        url,
        headers,
        body: Some(body),
    };

    // Upstream passes the abort signal to client.send. The onResponse
    // callback fires as soon as the HTTP response arrives, before the event
    // stream is consumed (upstream deserialize-step middleware).
    let response =
        crate::api::fetch_json_stream(&fetch, request, options.signal.as_ref(), options.timeout_ms)
            .await
            .map_err(|error| BedrockStreamError {
                message: format_transport_error(&error),
                error_code: None,
                status: error.status,
                request_id: response_header_request_id(&error),
            })?;

    let header_request_id = response
        .header("x-amzn-requestid")
        .and_then(normalize_diagnostic_value);
    *response_request_id = header_request_id.clone();
    if let Some(on_response) = &options.on_response {
        on_response(
            ProviderResponseInfo {
                status: response.status,
                headers: response.headers.clone(),
            },
            model,
        )
        .await;
    }

    stream.push(AssistantMessageEvent::Start {
        partial: output.clone(),
    });

    // --- Consume event stream ---
    let events = read_bedrock_events(response, options.signal.as_ref()).await?;
    for event in &events {
        handle_bedrock_event(event, model, output, stream, blocks)?;
    }

    if options
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err(BedrockStreamError::plain("Request was aborted"));
    }
    if output.stop_reason == StopReason::Pending {
        return Err(BedrockStreamError::plain(
            "Bedrock stream ended without a stop reason",
        ));
    }
    if output.stop_reason == StopReason::Aborted || output.stop_reason == StopReason::Error {
        return Err(BedrockStreamError::plain(
            output
                .error_message
                .clone()
                .unwrap_or_else(|| "An unknown error occurred".to_string()),
        ));
    }

    // A stream can settle without stopping every block, so finalize here too.
    finalize_streaming_blocks(blocks);
    sync_blocks_to_output(blocks, output);
    stream.push(AssistantMessageEvent::Done {
        reason: output.stop_reason,
        message: output.clone(),
    });
    stream.end(Some(output.clone()));
    Ok(())
}

// --- Wire format: AWS eventstream framing ---------------------------------------
//
// The ConverseStream response body is an `application/vnd.amazon.eventstream`
// binary protocol. Each message is:
//   [4B total length][4B headers length][4B payload length]
//   [headers: (1B name-len)(name)(1B type)(value)...]
//   [4B CRC of headers][payload][4B CRC of message]
// We only need the `:message-type` / `:event-type` headers and the JSON
// payload; CRCs are validated implicitly by the length framing (divergence:
// upstream's SDK verifies CRCs; the port trusts the length framing).

async fn read_bedrock_events(
    mut response: crate::transport::FetchResponse,
    signal: Option<&crate::AbortSignal>,
) -> Result<Vec<Value>, BedrockStreamError> {
    use futures::StreamExt;

    let mut buffer: Vec<u8> = Vec::new();
    let mut events: Vec<Value> = Vec::new();

    loop {
        let chunk = {
            let read_fut = response.body.next();
            tokio::pin!(read_fut);
            match read_guarded(&mut read_fut, signal).await {
                Ok(chunk) => chunk,
                Err(error) => return Err(error),
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        buffer.extend_from_slice(&chunk);
    }

    parse_all_frames(&buffer, &mut events)?;
    Ok(events)
}

/// Read one body chunk with an abort guard (upstream passes the caller
/// signal into `client.send`, so the body read aborts with it).
async fn read_guarded(
    read_fut: &mut (
             impl std::future::Future<Output = Option<Result<Vec<u8>, crate::error::AiError>>> + Unpin
         ),
    signal: Option<&crate::AbortSignal>,
) -> Result<Option<Vec<u8>>, BedrockStreamError> {
    let abort_fut = async {
        match signal {
            Some(s) => {
                let _ = s.aborted_or_pending().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(abort_fut);

    tokio::select! {
        biased;
        _ = &mut abort_fut => Err(BedrockStreamError::plain("Request was aborted")),
        chunk = read_fut => match chunk {
            Some(Ok(c)) => Ok(Some(c)),
            Some(Err(e)) => Err(BedrockStreamError::plain(e.to_string())),
            None => Ok(None),
        },
    }
}

/// Parse every complete eventstream frame in `buffer` (upstream: the SDK's
/// unmarshaller walking the async iterator).
fn parse_all_frames(buffer: &[u8], events: &mut Vec<Value>) -> Result<(), BedrockStreamError> {
    let mut offset = 0usize;
    while offset + 12 <= buffer.len() {
        let total = u32::from_be_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ]) as usize;
        if total < 16 || offset + total > buffer.len() {
            break; // incomplete (or corrupt) tail frame
        }
        let headers_len = u32::from_be_bytes([
            buffer[offset + 4],
            buffer[offset + 5],
            buffer[offset + 6],
            buffer[offset + 7],
        ]) as usize;
        let payload_len = u32::from_be_bytes([
            buffer[offset + 8],
            buffer[offset + 9],
            buffer[offset + 10],
            buffer[offset + 11],
        ]) as usize;
        let headers_start = offset + 12;
        let headers_end = headers_start + headers_len;
        let payload_start = headers_end + 4; // skip headers CRC
        let payload_end = payload_start + payload_len;
        if payload_end > offset + total {
            return Err(BedrockStreamError::plain(
                "Bedrock eventstream frame lengths are inconsistent",
            ));
        }

        let headers = parse_eventstream_headers(&buffer[headers_start..headers_end])?;
        let payload = &buffer[payload_start..payload_end];

        let message_type = headers
            .iter()
            .find(|(name, _)| name == ":message-type")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        let event_type = headers
            .iter()
            .find(|(name, _)| name == ":event-type")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();

        if message_type == "exception" {
            // Exception frames carry the modeled error code in `:error-code`
            // (or the event type) and a JSON/text message payload.
            let code = headers
                .iter()
                .find(|(name, _)| name == ":error-code")
                .map(|(_, value)| value.clone())
                .unwrap_or(event_type);
            let message = String::from_utf8_lossy(payload).trim().to_string();
            return Err(BedrockStreamError {
                message: if message.is_empty() {
                    format!("{code}: stream exception")
                } else {
                    message
                },
                error_code: Some(code),
                status: None,
                request_id: None,
            });
        }

        if message_type == "event" && !payload.is_empty() {
            let parsed: Value = serde_json::from_slice(payload).map_err(|error| {
                BedrockStreamError::plain(format!("Invalid Bedrock event payload: {error}"))
            })?;
            // Tag the parsed payload with its event type so the handler can
            // dispatch the way upstream's union type does.
            let mut tagged = Map::new();
            tagged.insert("eventType".to_string(), Value::String(event_type.clone()));
            if let Value::Object(obj) = parsed {
                for (key, value) in obj {
                    tagged.insert(key, value);
                }
            }
            events.push(Value::Object(tagged));
        }

        offset += total;
    }
    Ok(())
}

/// Parse the eventstream header block: repeated
/// `(1B name-len)(name)(1B type)(value)` records. ConverseStream uses type 7
/// (string) for the headers we read; other value types are skipped with
/// their correct wire sizes.
fn parse_eventstream_headers(bytes: &[u8]) -> Result<Vec<(String, String)>, BedrockStreamError> {
    let mut headers = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let name_len = bytes[offset] as usize;
        offset += 1;
        if offset + name_len + 1 > bytes.len() {
            return Err(BedrockStreamError::plain(
                "Bedrock eventstream headers are truncated",
            ));
        }
        let name = String::from_utf8_lossy(&bytes[offset..offset + name_len]).to_string();
        offset += name_len;
        let value_type = bytes[offset];
        offset += 1;
        // Wire sizes per Smithy eventstream spec: true/false carry no value,
        // byte=1, short=2, int=4, long=8, bytearray/string=2-byte len prefix,
        // timestamp=8, guid=16.
        let (value, value_len): (String, usize) = match value_type {
            7 => {
                if offset + 2 > bytes.len() {
                    return Err(BedrockStreamError::plain(
                        "Bedrock eventstream header value is truncated",
                    ));
                }
                let len = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
                offset += 2;
                if offset + len > bytes.len() {
                    return Err(BedrockStreamError::plain(
                        "Bedrock eventstream header value is truncated",
                    ));
                }
                (
                    String::from_utf8_lossy(&bytes[offset..offset + len]).to_string(),
                    len,
                )
            }
            0 | 1 => (String::new(), 0),
            2 => (String::new(), 1),
            3 => (String::new(), 2),
            4 => (String::new(), 4),
            8 => (String::new(), 8),
            5 => (String::new(), 8),
            6 => {
                if offset + 2 > bytes.len() {
                    return Err(BedrockStreamError::plain(
                        "Bedrock eventstream header value is truncated",
                    ));
                }
                let len = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
                offset += 2;
                (String::new(), len)
            }
            9 => (String::new(), 16),
            _ => {
                return Err(BedrockStreamError::plain(format!(
                    "Unknown Bedrock eventstream header value type: {value_type}"
                )));
            }
        };
        offset += value_len;
        headers.push((name, value));
    }
    Ok(headers)
}

// --- Event handlers ---------------------------------------------------------------

/// Dispatch one tagged event (upstream `for await (const item of
/// response.stream!)` chain). Event payloads arrive as JSON objects keyed by
/// their member name (`messageStart`, `contentBlockStart`, …) with an
/// injected `eventType` tag.
fn handle_bedrock_event(
    event: &Value,
    model: &Model,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    blocks: &mut Vec<Block>,
) -> Result<(), BedrockStreamError> {
    let event_type = event
        .get("eventType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match event_type.as_str() {
        "messageStart" => {
            let role = event
                .pointer("/messageStart/role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if role != "assistant" {
                return Err(BedrockStreamError::plain(
                    "Unexpected assistant message start but got user message start instead",
                ));
            }
            stream.push(AssistantMessageEvent::Start {
                partial: output.clone(),
            });
        }
        "contentBlockStart" => handle_content_block_start(event, blocks, output, stream),
        "contentBlockDelta" => handle_content_block_delta(event, blocks, output, stream),
        "contentBlockStop" => handle_content_block_stop(event, blocks, output, stream),
        "messageStop" => {
            let reason = event
                .pointer("/messageStop/stopReason")
                .and_then(Value::as_str)
                .map(str::to_string);
            output.raw_stop_reason = reason.clone();
            let mapped = map_stop_reason(reason.as_deref());
            output.stop_reason = mapped.stop_reason;
            if let Some(error_message) = mapped.error_message {
                output.error_message = Some(error_message);
            }
        }
        "metadata" => handle_metadata(event, model, output),
        // Modeled mid-stream exceptions arrive as exception frames and are
        // surfaced by the frame parser; unknown event types are ignored
        // (upstream's if-chain falls through for unrecognized members).
        _ => {}
    }
    Ok(())
}

fn handle_content_block_start(
    event: &Value,
    blocks: &mut Vec<Block>,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
) {
    let Some(index) = event
        .pointer("/contentBlockStart/contentBlockIndex")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
    else {
        return;
    };
    let tool_use = event.pointer("/contentBlockStart/start/toolUse");
    if let Some(tool_use) = tool_use {
        let block = Block {
            content: Content::ToolCall {
                id: tool_use
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: tool_use
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: Value::Object(Map::new()),
                thought_signature: None,
                namespace: None,
            },
            index,
            partial_json: Some(String::new()),
            redacted_chunks: None,
        };
        blocks.push(block);
        output.content.push(blocks.last().unwrap().content.clone());
        stream.push(AssistantMessageEvent::ToolcallStart {
            content_index: output.content.len() - 1,
            partial: output.clone(),
        });
    }
}

fn handle_content_block_delta(
    event: &Value,
    blocks: &mut Vec<Block>,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
) {
    let Some(content_block_index) = event
        .pointer("/contentBlockDelta/contentBlockIndex")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
    else {
        return;
    };
    let Some(delta) = event.pointer("/contentBlockDelta/delta") else {
        return;
    };
    let slot = blocks.iter().position(|b| b.index == content_block_index);

    if let Some(text) = delta.get("text").and_then(Value::as_str) {
        // If no text block exists yet, create one, as `contentBlockStart` is
        // not sent for text blocks.
        let slot = match slot {
            Some(slot) => slot,
            None => {
                blocks.push(Block {
                    content: Content::Text {
                        text: String::new(),
                        text_signature: None,
                    },
                    index: content_block_index,
                    partial_json: None,
                    redacted_chunks: None,
                });
                output.content.push(Content::Text {
                    text: String::new(),
                    text_signature: None,
                });
                stream.push(AssistantMessageEvent::TextStart {
                    content_index: output.content.len() - 1,
                    partial: output.clone(),
                });
                blocks.len() - 1
            }
        };
        if let Content::Text {
            text: block_text, ..
        } = &mut blocks[slot].content
        {
            block_text.push_str(text);
        }
        sync_slot(blocks, output, slot);
        stream.push(AssistantMessageEvent::TextDelta {
            content_index: slot,
            delta: text.to_string(),
            partial: output.clone(),
        });
        return;
    }

    if let Some(tool_use) = delta.get("toolUse") {
        if let Some(slot) = slot {
            if matches!(blocks[slot].content, Content::ToolCall { .. }) {
                let chunk = tool_use.get("input").and_then(Value::as_str).unwrap_or("");
                let partial = blocks[slot].partial_json.get_or_insert_with(String::new);
                partial.push_str(chunk);
                let arguments = parse_streaming_json(Some(partial.as_str()));
                if let Content::ToolCall {
                    arguments: args, ..
                } = &mut blocks[slot].content
                {
                    *args = arguments;
                }
                sync_slot(blocks, output, slot);
                stream.push(AssistantMessageEvent::ToolcallDelta {
                    content_index: slot,
                    delta: chunk.to_string(),
                    partial: output.clone(),
                });
            }
        }
        return;
    }

    if let Some(reasoning) = delta.get("reasoningContent") {
        let slot = match slot {
            Some(slot) => slot,
            None => {
                blocks.push(Block {
                    content: Content::Thinking {
                        thinking: String::new(),
                        thinking_signature: Some(String::new()),
                        redacted: None,
                    },
                    index: content_block_index,
                    partial_json: None,
                    redacted_chunks: None,
                });
                output.content.push(Content::Thinking {
                    thinking: String::new(),
                    thinking_signature: Some(String::new()),
                    redacted: None,
                });
                stream.push(AssistantMessageEvent::ThinkingStart {
                    content_index: output.content.len() - 1,
                    partial: output.clone(),
                });
                blocks.len() - 1
            }
        };
        if !matches!(blocks[slot].content, Content::Thinking { .. }) {
            return;
        }

        if let Some(text) = reasoning.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                if let Content::Thinking { thinking, .. } = &mut blocks[slot].content {
                    thinking.push_str(text);
                }
                sync_slot(blocks, output, slot);
                stream.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: slot,
                    delta: text.to_string(),
                    partial: output.clone(),
                });
            }
        }
        // `thinking_signature` holds either an Anthropic signature or an
        // opaque redacted payload, never both: mixing them would corrupt
        // whichever arrived first.
        let redacted = matches!(
            blocks[slot].content,
            Content::Thinking {
                redacted: Some(true),
                ..
            }
        );
        if let Some(signature) = reasoning.get("signature").and_then(Value::as_str) {
            if !signature.is_empty() && !redacted {
                if let Content::Thinking {
                    thinking_signature: Some(existing),
                    ..
                } = &mut blocks[slot].content
                {
                    existing.push_str(signature);
                }
                sync_slot(blocks, output, slot);
            }
        }
        if let Some(redacted_content) = reasoning.get("redactedContent").and_then(Value::as_array) {
            if !redacted_content.is_empty() {
                // Encrypted reasoning from non-Anthropic models on Bedrock
                // (e.g. OpenAI GPT-5.6). The payload is opaque, so keep it
                // verbatim in `thinking_signature` the way the Anthropic path
                // stores redacted thinking, and replay it on the next turn.
                if !redacted {
                    if let Content::Thinking {
                        thinking,
                        thinking_signature,
                        redacted: redacted_flag,
                    } = &mut blocks[slot].content
                    {
                        *redacted_flag = Some(true);
                        *thinking_signature = Some(String::new());
                        thinking.push_str(REDACTED_THINKING_PLACEHOLDER);
                    }
                    sync_slot(blocks, output, slot);
                    stream.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: slot,
                        delta: REDACTED_THINKING_PLACEHOLDER.to_string(),
                        partial: output.clone(),
                    });
                }
                let chunks: Vec<u8> = redacted_content
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|v| v as u8)
                    .collect();
                blocks[slot]
                    .redacted_chunks
                    .get_or_insert_with(Vec::new)
                    .push(chunks);
            }
        }
    }
}

/// Copy one scratch block's content into the persisted message.
fn sync_slot(blocks: &[Block], output: &mut AssistantMessage, slot: usize) {
    if let Some(content) = output.content.get_mut(slot) {
        content.clone_from(&blocks[slot].content);
    }
}

/// Encodes buffered encrypted reasoning into `thinking_signature` and drops
/// the scratch buffer, which must never reach a persisted message: a raw
/// byte buffer serializes to an index-keyed object roughly ten times the
/// size of the base64 payload.
fn flush_redacted_content(block: &mut Block) {
    if !matches!(block.content, Content::Thinking { .. }) {
        return;
    }
    let Some(chunks) = block.redacted_chunks.take() else {
        return;
    };
    let joined: Vec<u8> = chunks.iter().flatten().copied().collect();
    let encoded = encode_base64(&joined);
    if let Content::Thinking {
        thinking_signature, ..
    } = &mut block.content
    {
        *thinking_signature = Some(encoded);
    }
}

/// Strips every streaming scratch field. Runs from the terminal paths as well
/// as `contentBlockStop`, because a stream can settle without stopping each
/// block. Scratch fields live only on the `Block` wrappers; this folds the
/// flushed redacted payload into the persisted content.
fn finalize_streaming_blocks(blocks: &mut [Block]) {
    for block in blocks {
        block.index = usize::MAX;
        block.partial_json = None;
        flush_redacted_content(block);
    }
}

/// Copy finalized scratch content back into the persisted message.
fn sync_blocks_to_output(blocks: &[Block], output: &mut AssistantMessage) {
    for (slot, block) in blocks.iter().enumerate() {
        if let Some(content) = output.content.get_mut(slot) {
            content.clone_from(&block.content);
        }
    }
}

fn handle_metadata(event: &Value, model: &Model, output: &mut AssistantMessage) {
    let Some(usage) = event.pointer("/metadata/usage") else {
        return;
    };
    let input = usage
        .get("inputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("outputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = usage
        .get("cacheReadInputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = usage
        .get("cacheWriteInputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("totalTokens")
        .and_then(Value::as_u64)
        .unwrap_or(input + output_tokens);
    output.usage = Usage {
        input,
        output: output_tokens,
        cache_read,
        cache_write,
        total_tokens,
        ..crate::api::zeroed_usage()
    };
    crate::models::calculate_cost(model, &mut output.usage);
}

fn handle_content_block_stop(
    event: &Value,
    blocks: &mut [Block],
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
) {
    let Some(content_block_index) = event
        .pointer("/contentBlockStop/contentBlockIndex")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
    else {
        return;
    };
    let Some(slot) = blocks.iter().position(|b| b.index == content_block_index) else {
        return;
    };
    blocks[slot].index = usize::MAX;

    match blocks[slot].content.clone() {
        Content::Text { text, .. } => {
            stream.push(AssistantMessageEvent::TextEnd {
                content_index: slot,
                content: text,
                partial: output.clone(),
            });
        }
        Content::Thinking { .. } => {
            flush_redacted_content(&mut blocks[slot]);
            sync_slot(blocks, output, slot);
            let content = match &blocks[slot].content {
                Content::Thinking { thinking, .. } => thinking.clone(),
                _ => unreachable!("checked above"),
            };
            stream.push(AssistantMessageEvent::ThinkingEnd {
                content_index: slot,
                content,
                partial: output.clone(),
            });
        }
        Content::ToolCall { .. } => {
            // Finalize in-place and strip the scratch buffer so replay only
            // carries parsed arguments.
            let partial = blocks[slot].partial_json.take();
            if let Content::ToolCall { arguments, .. } = &mut blocks[slot].content {
                *arguments = parse_streaming_json(partial.as_deref());
            }
            sync_slot(blocks, output, slot);
            let tool_call = blocks[slot].content.clone();
            stream.push(AssistantMessageEvent::ToolcallEnd {
                content_index: slot,
                tool_call,
                partial: output.clone(),
            });
        }
        _ => {}
    }
}

// --- Capability checks ------------------------------------------------------------

/// Check if the model supports adaptive thinking (Opus 4.6+, Sonnet 4.6+).
/// Checks both model ID and model name to support application inference
/// profiles whose ARNs don't contain the model name.
fn get_model_match_candidates(model_id: &str, model_name: Option<&str>) -> Vec<String> {
    let values: Vec<&str> = match model_name {
        Some(name) => vec![model_id, name],
        None => vec![model_id],
    };
    let mut candidates = Vec::new();
    for value in values {
        let lower = value.to_lowercase();
        candidates.push(lower.clone());
        candidates.push(collapse_separators(&lower));
    }
    candidates
}

/// `lower.replace(/[\s_.:]+/g, "-")`
fn collapse_separators(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_was_sep = false;
    for ch in value.chars() {
        if ch.is_whitespace() || ch == '_' || ch == '.' || ch == ':' {
            if !last_was_sep {
                out.push('-');
            }
            last_was_sep = true;
        } else {
            out.push(ch);
            last_was_sep = false;
        }
    }
    out
}

fn candidates_include(candidates: &[String], needles: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| needles.iter().any(|needle| candidate.contains(needle)))
}

fn supports_adaptive_thinking(model_id: &str, model_name: Option<&str>) -> bool {
    let candidates = get_model_match_candidates(model_id, model_name);
    candidates_include(
        &candidates,
        &[
            "opus-4-6",
            "opus-4-7",
            "opus-4-8",
            "opus-5",
            "sonnet-4-6",
            "sonnet-5",
            "fable-5",
        ],
    )
}

fn supports_native_xhigh_effort(model: &Model) -> bool {
    let candidates = get_model_match_candidates(&model.id, Some(&model.name));
    candidates_include(
        &candidates,
        &["opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "fable-5"],
    )
}

fn map_thinking_level_to_effort(model: &Model, level: Option<ThinkingLevel>) -> String {
    if level == Some(ThinkingLevel::Xhigh) && supports_native_xhigh_effort(model) {
        return "xhigh".to_string();
    }

    let mapped = level.and_then(|level| {
        model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&to_model_thinking_level(level)).cloned())
            .flatten()
    });
    if let Some(mapped) = mapped {
        return match mapped.as_str() {
            "low" | "medium" | "high" | "xhigh" | "max" => mapped,
            _ => "high".to_string(),
        };
    }

    match level {
        Some(ThinkingLevel::Minimal) | Some(ThinkingLevel::Low) => "low".to_string(),
        Some(ThinkingLevel::Medium) => "medium".to_string(),
        _ => "high".to_string(),
    }
}

/// Check if the model is an Anthropic Claude model on Bedrock. Checks both
/// model ID and model name to support application inference profiles whose
/// ARNs don't contain the model name.
fn is_anthropic_claude_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

/// Check if the model supports prompt caching. Supported: Claude 3.5 Haiku,
/// Claude 3.7 Sonnet, Claude 4.x, Claude 5. For application inference
/// profiles (whose ARNs don't contain the model name), also checks
/// `model.name`; as a last resort, `AWS_BEDROCK_FORCE_CACHE=1` enables cache
/// points. Amazon Nova models have automatic caching and don't need explicit
/// cache points.
fn supports_prompt_caching(model: &Model, env: Option<&ProviderEnv>) -> bool {
    let candidates = get_model_match_candidates(&model.id, Some(&model.name));

    let has_claude_ref = candidates.iter().any(|s| s.contains("claude"));
    if !has_claude_ref {
        // Application inference profiles don't contain the model name in the
        // ARN. Allow users to force cache points via environment variable.
        return get_provider_env_value("AWS_BEDROCK_FORCE_CACHE", env).as_deref() == Some("1");
    }
    // Claude 5 models (fable-5, opus-5, sonnet-5)
    if candidates_include(&candidates, &["fable-5", "opus-5", "sonnet-5"]) {
        return true;
    }
    // Claude 4.x models (opus-4, sonnet-4, haiku-4)
    if candidates.iter().any(|s| s.contains("-4-")) {
        return true;
    }
    // Claude 3.7 Sonnet
    if candidates.iter().any(|s| s.contains("claude-3-7-sonnet")) {
        return true;
    }
    // Claude 3.5 Haiku
    if candidates.iter().any(|s| s.contains("claude-3-5-haiku")) {
        return true;
    }
    false
}

/// Only Anthropic Claude models support the signature field in
/// reasoningContent. Other models (OpenAI, Qwen, Minimax, Moonshot, etc.)
/// reject it with: "This model doesn't support the
/// reasoningContent.reasoningText.signature field".
fn supports_thinking_signature(model: &Model) -> bool {
    is_anthropic_claude_model(model)
}

fn supports_strict_mode(model: &Model) -> bool {
    model
        .compat
        .as_ref()
        .map(|compat| match compat {
            crate::types::ModelCompat::OpenaiCompletions(compat) => {
                compat.supports_strict_mode.unwrap_or(false)
            }
            crate::types::ModelCompat::OpenaiResponses(compat) => {
                compat.supports_strict_mode.unwrap_or(false)
            }
            crate::types::ModelCompat::AnthropicMessages(_) => false,
        })
        .unwrap_or(false)
}

fn to_model_thinking_level(level: ThinkingLevel) -> crate::types::ModelThinkingLevel {
    match level {
        ThinkingLevel::Minimal => crate::types::ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => crate::types::ModelThinkingLevel::Low,
        ThinkingLevel::Medium => crate::types::ModelThinkingLevel::Medium,
        ThinkingLevel::High => crate::types::ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => crate::types::ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => crate::types::ModelThinkingLevel::Max,
    }
}

// --- Payload builders ------------------------------------------------------------

fn cache_point_block(cache_retention: CacheRetention) -> Value {
    match cache_retention {
        CacheRetention::Long => json!({ "cachePoint": { "type": "default", "ttl": "ONE_HOUR" } }),
        _ => json!({ "cachePoint": { "type": "default" } }),
    }
}

fn build_system_prompt(
    system_prompt: Option<&str>,
    model: &Model,
    cache_retention: CacheRetention,
    env: Option<&ProviderEnv>,
) -> Option<Value> {
    let system_prompt = system_prompt?;
    let mut blocks = vec![json!({ "text": sanitize_surrogates(system_prompt) })];

    // Add cache point for supported Claude models when caching is enabled
    if cache_retention != CacheRetention::None && supports_prompt_caching(model, env) {
        blocks.push(cache_point_block(cache_retention));
    }

    Some(Value::Array(blocks))
}

/// Upstream `normalizeToolCallId`: sanitize to `[a-zA-Z0-9_-]`, cap at 64.
fn normalize_tool_call_id(id: &str) -> String {
    let mut sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.chars().count() > 64 {
        sanitized = sanitized.chars().take(64).collect();
    }
    sanitized
}

/// Adapter for `transform_messages`' 3-argument normalizer signature.
fn bedrock_tool_call_id_normalizer() -> impl Fn(&str, &Model, &AssistantMessage) -> String {
    |id: &str, _model: &Model, _source: &AssistantMessage| normalize_tool_call_id(id)
}

fn create_non_blank_text_block(text: &str) -> Option<Value> {
    let sanitized = sanitize_surrogates(text);
    if sanitized.trim().is_empty() {
        None
    } else {
        Some(json!({ "text": sanitized }))
    }
}

fn create_required_text_block(text: &str) -> Value {
    create_non_blank_text_block(text).unwrap_or_else(|| json!({ "text": EMPTY_TEXT_PLACEHOLDER }))
}

/// Upstream `sanitizeBedrockDocument`: recursively drop empty property names
/// from replayed tool arguments (Bedrock rejects `""` keys; they are an
/// artifact of streamed-argument repair).
fn sanitize_bedrock_document(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_bedrock_document).collect()),
        Value::Object(obj) => Value::Object(
            obj.iter()
                .filter(|(key, _)| !key.is_empty())
                .map(|(key, nested)| (key.clone(), sanitize_bedrock_document(nested)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn convert_tool_result_content(content: &[Content]) -> Result<Vec<Value>, BedrockStreamError> {
    let mut result: Vec<Value> = Vec::new();
    for block in content {
        match block {
            Content::Image { data, mime_type } => {
                result.push(json!({ "image": create_image_block(mime_type, data)? }));
            }
            Content::Text { text, .. } => {
                if let Some(text_block) = create_non_blank_text_block(text) {
                    result.push(text_block);
                }
            }
            _ => {}
        }
    }
    if result.is_empty() {
        result.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
    }
    Ok(result)
}

/// Upstream `convertMessages`: Bedrock Converse message shape, with
/// consecutive toolResult messages folded into a single user message and a
/// trailing cache point on the last user message when caching is enabled.
fn convert_messages(
    context: &Context,
    model: &Model,
    cache_retention: CacheRetention,
    env: Option<&ProviderEnv>,
) -> Result<Vec<Value>, BedrockStreamError> {
    let mut result: Vec<Value> = Vec::new();
    let normalizer = bedrock_tool_call_id_normalizer();
    let transformed = transform_messages(context.messages.clone(), model, Some(&normalizer));

    let mut i = 0usize;
    while i < transformed.len() {
        match &transformed[i] {
            Message::User { content, .. } => {
                let mut content_blocks: Vec<Value> = Vec::new();
                match content {
                    UserContent::Text(text) => {
                        content_blocks.push(create_required_text_block(text));
                    }
                    UserContent::Blocks(blocks) => {
                        for block in blocks {
                            match block {
                                Content::Text { text, .. } => {
                                    if let Some(text_block) = create_non_blank_text_block(text) {
                                        content_blocks.push(text_block);
                                    }
                                }
                                Content::Image { data, mime_type } => {
                                    content_blocks.push(json!({
                                        "image": create_image_block(mime_type, data)?
                                    }));
                                }
                                _ => continue,
                            }
                        }
                        if content_blocks.is_empty() {
                            content_blocks.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
                        }
                    }
                }
                result.push(json!({ "role": "user", "content": content_blocks }));
            }
            Message::Assistant(assistant) => {
                // Skip assistant messages with empty content (e.g., from
                // aborted requests): Bedrock rejects empty content arrays.
                if assistant.content.is_empty() {
                    i += 1;
                    continue;
                }
                let mut content_blocks: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        Content::Text { text, .. } => {
                            // Skip empty text blocks
                            if let Some(text_block) = create_non_blank_text_block(text) {
                                content_blocks.push(text_block);
                            }
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            content_blocks.push(json!({
                                "toolUse": {
                                    "toolUseId": id,
                                    "name": name,
                                    "input": sanitize_bedrock_document(arguments),
                                }
                            }));
                        }
                        Content::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            // Encrypted reasoning is opaque: replay the stored
                            // payload as the `redactedContent` member instead of
                            // lowering it to reasoning text.
                            if *redacted == Some(true) {
                                if let Some(redacted_content) =
                                    decode_redacted_content(thinking_signature.as_deref())
                                {
                                    content_blocks.push(json!({
                                        "reasoningContent": {
                                            "redactedContent": redacted_content
                                        }
                                    }));
                                }
                                continue;
                            }
                            // Skip empty thinking blocks
                            let thinking_text = sanitize_surrogates(thinking);
                            if thinking_text.trim().is_empty() {
                                continue;
                            }
                            // Only Anthropic models support the signature field
                            // in reasoningText; for other models we omit it to
                            // avoid errors like "This model doesn't support the
                            // reasoningContent.reasoningText.signature field".
                            if supports_thinking_signature(model) {
                                // Signatures arrive after thinking deltas. If a
                                // partial or externally persisted message lacks
                                // a signature, Bedrock rejects the replayed
                                // reasoning block. Fall back to plain text,
                                // matching Anthropic.
                                let signature_empty = thinking_signature
                                    .as_deref()
                                    .map(str::trim)
                                    .map(str::is_empty)
                                    .unwrap_or(true);
                                if signature_empty {
                                    content_blocks.push(json!({ "text": thinking_text }));
                                } else {
                                    content_blocks.push(json!({
                                        "reasoningContent": {
                                            "reasoningText": {
                                                "text": thinking_text,
                                                "signature": thinking_signature,
                                            }
                                        }
                                    }));
                                }
                            } else {
                                content_blocks.push(json!({
                                    "reasoningContent": {
                                        "reasoningText": { "text": thinking_text }
                                    }
                                }));
                            }
                        }
                        _ => continue,
                    }
                }
                // Skip if all content blocks were filtered out
                if content_blocks.is_empty() {
                    i += 1;
                    continue;
                }
                result.push(json!({ "role": "assistant", "content": content_blocks }));
            }
            Message::ToolResult(tool_result) => {
                // Collect all consecutive toolResult messages into a single
                // user message: Bedrock requires all tool results in one
                // message.
                let mut tool_results: Vec<Value> = Vec::new();
                tool_results.push(json!({
                    "toolResult": {
                        "toolUseId": tool_result.tool_call_id,
                        "content": convert_tool_result_content(&tool_result.content)?,
                        "status": if tool_result.is_error { "error" } else { "success" },
                    }
                }));

                // Look ahead for consecutive toolResult messages
                let mut j = i + 1;
                while j < transformed.len() {
                    if let Message::ToolResult(next) = &transformed[j] {
                        tool_results.push(json!({
                            "toolResult": {
                                "toolUseId": next.tool_call_id,
                                "content": convert_tool_result_content(&next.content)?,
                                "status": if next.is_error { "error" } else { "success" },
                            }
                        }));
                        j += 1;
                    } else {
                        break;
                    }
                }

                // Skip the messages we've already processed
                i = j;
                result.push(json!({ "role": "user", "content": tool_results }));
                continue;
            }
        }
        i += 1;
    }

    // Add cache point to the last user message for supported Claude models
    // when caching is enabled
    if cache_retention != CacheRetention::None
        && supports_prompt_caching(model, env)
        && let Some(last) = result.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.push(cache_point_block(cache_retention));
    }

    Ok(result)
}

fn convert_tool_config(
    tools: &[Tool],
    tool_choice: Option<&Value>,
    supports_strict_mode: bool,
) -> Result<Option<Value>, BedrockStreamError> {
    if tools.is_empty() {
        return Ok(None);
    }
    if tool_choice == Some(&json!("none")) {
        return Ok(None);
    }

    let mut bedrock_tools: Vec<Value> = Vec::new();
    for tool in tools {
        let strict = resolve_json_schema_strict_sampling(tool, supports_strict_mode)
            .map_err(|error| BedrockStreamError::plain(error.to_string()))?;
        let parameters = get_json_schema_tool_parameters(tool, strict)
            .map_err(|error| BedrockStreamError::plain(error.to_string()))?;
        let mut tool_spec = json!({
            "toolSpec": {
                "name": tool.name,
                "description": tool.description,
                "inputSchema": { "json": parameters },
            }
        });
        if strict == Some(true) {
            tool_spec["toolSpec"]["strict"] = json!(true);
        }
        bedrock_tools.push(tool_spec);
    }

    let bedrock_tool_choice = match tool_choice {
        Some(Value::String(choice)) => match choice.as_str() {
            "auto" => Some(json!({ "auto": {} })),
            "any" => Some(json!({ "any": {} })),
            _ => None,
        },
        Some(choice) if choice.get("type").and_then(Value::as_str) == Some("tool") => {
            let name = choice
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(json!({ "tool": { "name": name } }))
        }
        _ => None,
    };

    Ok(Some(json!({
        "tools": bedrock_tools,
        "toolChoice": bedrock_tool_choice,
    })))
}

/// Upstream `mapStopReason`: raw Bedrock stop reason → pi stop reason, with
/// unknown reasons surfacing as an error stop carrying the raw value.
fn map_stop_reason(reason: Option<&str>) -> MappedStopReason {
    match reason {
        Some("end_turn") | Some("stop_sequence") => MappedStopReason {
            stop_reason: StopReason::Stop,
            error_message: None,
        },
        Some("max_tokens") | Some("model_context_window_exceeded") => MappedStopReason {
            stop_reason: StopReason::Length,
            error_message: None,
        },
        Some("tool_use") => MappedStopReason {
            stop_reason: StopReason::ToolUse,
            error_message: None,
        },
        Some(other) => MappedStopReason {
            stop_reason: StopReason::Error,
            error_message: Some(format!("Provider stopped with: {other}")),
        },
        None => MappedStopReason {
            stop_reason: StopReason::Error,
            error_message: None,
        },
    }
}

struct MappedStopReason {
    stop_reason: StopReason,
    error_message: Option<String>,
}

// --- Request URL -----------------------------------------------------------------

/// Upstream relies on the SDK client to route the request to the resolved
/// region endpoint. The port builds the standard ConverseStream URL from the
/// resolved region; a custom endpoint (VPC/proxy) replaces it verbatim.
fn build_request_url(
    model: &Model,
    options: &BedrockOptions,
) -> Result<String, BedrockStreamError> {
    let config = build_client_config(model, options);
    if let Some(endpoint) = &config.endpoint {
        return Ok(format!(
            "{}/model/{}/converse-stream",
            endpoint.trim_end_matches('/'),
            model.id
        ));
    }
    let region = config
        .region
        .clone()
        .unwrap_or_else(|| "us-east-1".to_string());
    Ok(format!(
        "https://bedrock-runtime.{region}.amazonaws.com/model/{}/converse-stream",
        model.id
    ))
}

// --- Client config resolution ------------------------------------------------------

fn get_configured_bedrock_region(options: &BedrockOptions) -> Option<String> {
    options
        .region
        .clone()
        .or_else(|| get_provider_env_value("AWS_REGION", options.env.as_ref()))
        .or_else(|| get_provider_env_value("AWS_DEFAULT_REGION", options.env.as_ref()))
}

fn get_configured_bedrock_credentials(env: Option<&ProviderEnv>) -> Option<Credentials> {
    let access_key_id = get_provider_env_value("AWS_ACCESS_KEY_ID", env)?;
    let secret_access_key = get_provider_env_value("AWS_SECRET_ACCESS_KEY", env)?;
    Some(Credentials {
        access_key_id,
        secret_access_key,
        session_token: get_provider_env_value("AWS_SESSION_TOKEN", env),
    })
}

/// Upstream `getStandardBedrockEndpointRegion`: extract the region from a
/// standard AWS Bedrock runtime hostname
/// (`bedrock-runtime[-fips].<region>.amazonaws.com[.cn]`).
fn get_standard_bedrock_endpoint_region(base_url: &str) -> Option<String> {
    let hostname = extract_hostname(base_url)?.to_lowercase();
    let rest = hostname.strip_prefix("bedrock-runtime")?;
    let rest = rest.strip_prefix("-fips").unwrap_or(rest);
    let rest = rest.strip_prefix('.')?;
    let region_end = rest.find(".amazonaws.com")?;
    let region = &rest[..region_end];
    if region.is_empty()
        || !region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    Some(region.to_string())
}

/// Minimal hostname extraction (no `url` crate dependency).
fn extract_hostname(base_url: &str) -> Option<String> {
    let rest = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))?;
    let host_end = rest.find('/').unwrap_or(rest.len());
    let host_port = &rest[..host_end];
    let host = host_port
        .rsplit_once(':')
        .map(|(host, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() {
                host
            } else {
                host_port
            }
        })
        .unwrap_or(host_port);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn should_use_explicit_bedrock_endpoint(
    base_url: &str,
    configured_region: Option<&str>,
    has_ambient_configured_profile: bool,
) -> bool {
    let endpoint_region = get_standard_bedrock_endpoint_region(base_url);
    let Some(_endpoint_region) = endpoint_region else {
        return true;
    };
    configured_region.is_none() && !has_ambient_configured_profile
}

/// Resolve the full client config with upstream's priority rules:
/// - profile: explicit option > scoped env AWS_PROFILE > ambient AWS_PROFILE
/// - region: ARN-embedded > explicit option > env vars > endpoint-derived >
///   us-east-1 default
/// - endpoint: standard AWS URLs are pinned only when no region or ambient
///   profile is configured (custom VPC/proxy endpoints always pass through)
/// - credentials: ambient AWS keys are used only when no explicit/scoped
///   profile is configured (the SDK default chain prefers a configured
///   profile over env keys, but only when `credentials` is not set — #6957)
/// - bearer token: options > apiKey > AWS_BEARER_TOKEN_BEDROCK, unless
///   AWS_BEDROCK_SKIP_AUTH=1
pub fn build_client_config(model: &Model, options: &BedrockOptions) -> ClientConfig {
    // A profile explicitly configured through pi's auth flow (the `profile`
    // option or scoped `AWS_PROFILE` on the stored credential's env) must
    // win over ambient AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY.
    let options_profile = options.profile.clone().or_else(|| {
        options
            .env
            .as_ref()
            .and_then(|env| env.get("AWS_PROFILE").cloned())
    });
    let profile = options_profile
        .clone()
        .or_else(|| get_provider_env_value("AWS_PROFILE", options.env.as_ref()));
    let configured_region = get_configured_bedrock_region(options);
    // Upstream `Boolean(getProviderEnvValue("AWS_PROFILE"))` — process env
    // only, NOT the scoped options.env (a scoped profile must not disable
    // endpoint pinning; it wins via optionsProfile instead). Tests that need
    // a deterministic ambient state should set/unset AWS_PROFILE in the
    // process env (edition 2024: unsafe { std::env::set_var(...) }).
    let has_ambient_configured_profile = std::env::var("AWS_PROFILE")
        .ok()
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let endpoint_region = get_standard_bedrock_endpoint_region(&model.base_url);
    let use_explicit_endpoint = should_use_explicit_bedrock_endpoint(
        &model.base_url,
        configured_region.as_deref(),
        has_ambient_configured_profile,
    );

    let mut config = ClientConfig {
        profile,
        endpoint: use_explicit_endpoint.then(|| model.base_url.clone()),
        ..Default::default()
    };

    // Region resolution: ARN-embedded > explicit option > env vars >
    // endpoint-derived > us-east-1 default. When the model ID is an
    // inference profile ARN, extract the region from it. This avoids
    // conflicts with AWS_REGION set for other services.
    if let Some(arn_region) = extract_arn_region(&model.id) {
        config.region = Some(arn_region);
    } else if let Some(region) = configured_region.clone() {
        config.region = Some(region);
    } else if let (Some(endpoint_region), true) = (endpoint_region.clone(), use_explicit_endpoint) {
        config.region = Some(endpoint_region);
    } else if !has_ambient_configured_profile {
        config.region = Some("us-east-1".to_string());
    }

    let skip_auth = get_provider_env_value("AWS_BEDROCK_SKIP_AUTH", options.env.as_ref())
        .as_deref()
        == Some("1");
    // Support proxies that don't need authentication
    if skip_auth {
        config.credentials = Some(Credentials {
            access_key_id: "dummy-access-key".to_string(),
            secret_access_key: "dummy-secret-key".to_string(),
            session_token: None,
        });
    } else if let Some(credentials) = get_configured_bedrock_credentials(options.env.as_ref()) {
        if options_profile.is_none() {
            config.credentials = Some(credentials);
        }
    }

    // Resolve bearer token for Bedrock API key auth.
    let bearer_token = options
        .bearer_token
        .clone()
        .or_else(|| options.api_key.clone())
        .or_else(|| get_provider_env_value("AWS_BEARER_TOKEN_BEDROCK", options.env.as_ref()));
    if let Some(token) = bearer_token.filter(|_| !skip_auth) {
        config.token = Some(token);
        config.auth_scheme_preference = Some(vec!["httpBearerAuth".to_string()]);
    }

    config
}

/// `^arn:aws(?:-[a-z0-9-]+)?:bedrock:([a-z0-9-]+):` — region embedded in an
/// inference profile ARN.
fn extract_arn_region(model_id: &str) -> Option<String> {
    let rest = model_id.strip_prefix("arn:aws")?;
    // Either ":bedrock:..." directly, or a partition suffix such as
    // "-us-gov:bedrock:..." before the region.
    let rest = if let Some(stripped) = rest.strip_prefix('-') {
        let colon = stripped.find(':')?;
        &stripped[colon + 1..]
    } else {
        rest.strip_prefix(':')?
    };
    let rest = rest.strip_prefix("bedrock:")?;
    let region = &rest[..rest.find(':')?];
    if region.is_empty()
        || !region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    Some(region.to_string())
}

fn is_gov_cloud_bedrock_target(model: &Model, options: &BedrockOptions) -> bool {
    if let Some(region) = get_configured_bedrock_region(options) {
        if region.to_lowercase().starts_with("us-gov-") {
            return true;
        }
    }
    let model_id = model.id.to_lowercase();
    model_id.starts_with("us-gov.") || model_id.starts_with("arn:aws-us-gov:")
}

/// Upstream `buildAdditionalModelRequestFields`: the Anthropic-specific
/// thinking payload for Claude models on Bedrock.
fn build_additional_model_request_fields(model: &Model, options: &BedrockOptions) -> Option<Value> {
    let reasoning = options.reasoning?;
    if !model.reasoning {
        return None;
    }
    if !is_anthropic_claude_model(model) {
        return None;
    }

    // GovCloud Bedrock currently rejects the Claude thinking.display field.
    // Omit it there until the GovCloud Converse schema catches up.
    let display = if is_gov_cloud_bedrock_target(model, options) {
        None
    } else {
        Some(
            options
                .thinking_display
                .clone()
                .unwrap_or_else(|| "summarized".to_string()),
        )
    };

    let adaptive = supports_adaptive_thinking(&model.id, Some(&model.name));
    let mut result = if adaptive {
        let mut thinking = Map::new();
        thinking.insert("type".to_string(), json!("adaptive"));
        if let Some(display) = &display {
            thinking.insert("display".to_string(), json!(display));
        }
        json!({
            "thinking": Value::Object(thinking),
            "output_config": {
                "effort": map_thinking_level_to_effort(model, Some(reasoning))
            },
        })
    } else {
        let default_budgets: [(ThinkingLevel, u64); 6] = [
            (ThinkingLevel::Minimal, 1024),
            (ThinkingLevel::Low, 2048),
            (ThinkingLevel::Medium, 8192),
            (ThinkingLevel::High, 16384),
            (ThinkingLevel::Xhigh, 16384), // Budget-based Claude clamps extended levels to high
            (ThinkingLevel::Max, 16384),
        ];
        // Custom budgets only cover token-based levels through high.
        let level = match reasoning {
            ThinkingLevel::Xhigh | ThinkingLevel::Max => ThinkingLevel::High,
            other => other,
        };
        let budget = options
            .thinking_budgets
            .and_then(|budgets| match level {
                ThinkingLevel::Minimal => budgets.minimal,
                ThinkingLevel::Low => budgets.low,
                ThinkingLevel::Medium => budgets.medium,
                ThinkingLevel::High => budgets.high,
                _ => None,
            })
            .or_else(|| {
                default_budgets
                    .iter()
                    .find(|(candidate, _)| *candidate == reasoning)
                    .map(|(_, budget)| *budget)
            });
        let mut thinking = Map::new();
        thinking.insert("type".to_string(), json!("enabled"));
        thinking.insert("budget_tokens".to_string(), json!(budget));
        if let Some(display) = &display {
            thinking.insert("display".to_string(), json!(display));
        }
        json!({ "thinking": Value::Object(thinking) })
    };

    if !adaptive && options.interleaved_thinking.unwrap_or(true) {
        result["anthropic_beta"] = json!(["interleaved-thinking-2025-05-14"]);
    }

    Some(result)
}

fn create_image_block(mime_type: &str, data: &str) -> Result<Value, BedrockStreamError> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => {
            return Err(BedrockStreamError::plain(format!(
                "Unknown image type: {other}"
            )));
        }
    };
    let bytes = decode_base64(data)
        .ok_or_else(|| BedrockStreamError::plain("Invalid base64 image data"))?;
    // Upstream hands a Uint8Array to the SDK, which serializes it to a
    // base64 string on the wire; the port sends the base64 string directly.
    Ok(json!({
        "source": { "bytes": encode_base64(&bytes) },
        "format": format,
    }))
}

/// Upstream `base64ToBytes`. Divergence: upstream's `atob` throws on invalid
/// input; the port returns `None` for the same inputs.
fn decode_base64(data: &str) -> Option<Vec<u8>> {
    base64_decode_std(data)
}

/// Standard-alphabet base64 decode (upstream `atob` semantics: invalid
/// characters rejected).
fn base64_decode_std(data: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(data.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for ch in data.chars() {
        if ch.is_whitespace() || ch == '=' {
            continue;
        }
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(output)
}

/// Standard-alphabet base64 encode (upstream `btoa`).
fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied();
        let third = chunk.get(2).copied();
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output
            .push(ALPHABET[(((first & 0x03) << 4) | (second.unwrap_or(0) >> 4)) as usize] as char);
        output.push(match second {
            None => '=',
            Some(second) => {
                ALPHABET[(((second & 0x0F) << 2) | (third.unwrap_or(0) >> 6)) as usize] as char
            }
        });
        output.push(match third {
            None => '=',
            Some(third) => ALPHABET[(third & 0x3F) as usize] as char,
        });
    }
    output
}

/// Decodes a stored redacted payload. The AWS SDK hands the blob over as
/// bytes, but a persisted session carries it as base64. A hand-edited or
/// externally produced session can hold a signature that is not base64; drop
/// that block instead of failing the whole request.
fn decode_redacted_content(signature: Option<&str>) -> Option<Vec<u8>> {
    let signature = signature?;
    base64_decode_std(signature).filter(|bytes| !bytes.is_empty())
}

// --- Error formatting -------------------------------------------------------------

/// Upstream `formatBedrockError` for transport-level failures: surface the
/// raw HTTP body (with status) when the message does not already carry it,
/// and append the data-retention hint when applicable. divergence: the
/// modeled AWS exception name is not recoverable from a plain HTTP failure,
/// so no legacy prefix is applied here.
fn format_transport_error(error: &crate::provider_retry::ProviderRequestError) -> String {
    let norm = normalize_provider_error(error.message.clone(), error.status, None);
    // The transport folds the HTTP body into `message` for non-2xx, so the
    // normalized body is always None here — the message is the body.
    let core = norm.message.clone();
    if core.to_lowercase().contains("data retention mode") {
        format!(" See {BEDROCK_DATA_RETENTION_DOCS_URL} for supported data retention modes.")
    } else {
        core
    }
}

fn normalize_diagnostic_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_BEDROCK_DIAGNOSTIC_VALUE_CHARS {
        return None;
    }
    Some(trimmed.to_string())
}

/// Header keys that must never be overwritten by caller-supplied headers.
/// `host` and `x-amz-*` participate in the SigV4 canonical request;
/// `authorization` is owned by SigV4 or the bearer-token path. Compared
/// case-insensitively.
fn is_reserved_header(key: &str) -> bool {
    let lower = key.to_lowercase();
    lower.starts_with("x-amz-") || lower == "authorization" || lower == "host"
}

fn response_header_request_id(
    error: &crate::provider_retry::ProviderRequestError,
) -> Option<String> {
    error
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-amzn-requestid"))
        .and_then(|(_, value)| normalize_diagnostic_value(value))
}

/// Structured metadata alongside `errorMessage`, which stays byte-identical
/// because `isRetryableAssistantError` matches against it. Unknown fields
/// are omitted, never guessed.
fn append_bedrock_failure_diagnostic(
    output: &mut AssistantMessage,
    error_code: Option<&str>,
    error_status: Option<u16>,
    fallback_request_id: Option<String>,
) {
    let mut details = Map::new();

    if let Some(status) = error_status {
        details.insert("status".to_string(), json!(status));
    }

    // The SDK puts the modeled code on `error.name` for service exceptions
    // and unmodeled stream errors alike; modeled Bedrock errors all end in
    // "Exception", unlike transport names such as "TimeoutError".
    if let Some(code) = error_code
        && code.ends_with("Exception")
        && let Some(normalized) = normalize_diagnostic_value(code)
    {
        details.insert("errorCode".to_string(), json!(normalized));
    }

    if let Some(request_id) = fallback_request_id {
        details.insert("requestId".to_string(), json!(request_id));
    }

    if details.is_empty() {
        return;
    }

    append_assistant_message_diagnostic(
        output,
        AssistantMessageDiagnostic {
            kind: "bedrock_response_failure".to_string(),
            timestamp: now_ms(),
            error: None,
            details: Some(Value::Object(details)),
        },
    );
}

// --- streamSimple -----------------------------------------------------------------

/// Upstream `streamSimple` for bedrock-converse-stream: base option
/// assembly, adaptive vs fixed-budget thinking for Claude models.
pub fn stream_simple(
    model: Model,
    context: Context,
    options: Option<SimpleStreamOptions>,
) -> crate::event_stream::AssistantMessageEventStream {
    let options = options.unwrap_or_default();

    // buildBaseOptions: clamp maxTokens to the remaining context window.
    let base_max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    let max_tokens = crate::simple_options::clamp_max_tokens_to_context(
        model.context_window,
        &context,
        base_max_tokens,
    );

    let mut bedrock_options = BedrockOptions {
        signal: options.signal,
        api_key: options.api_key,
        fetch: options.fetch,
        env: options.env,
        on_payload: options.on_payload,
        on_response: options.on_response,
        headers: options.headers,
        timeout_ms: options.timeout_ms,
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        temperature: options.temperature,
        max_tokens: Some(max_tokens),
        tool_choice: options.tool_choice,
        cache_retention: options.cache_retention,
        ..Default::default()
    };

    match options.reasoning {
        None => {
            bedrock_options.reasoning = None;
            stream(model, context, Some(bedrock_options))
        }
        Some(reasoning) => {
            if is_anthropic_claude_model(&model) {
                if supports_adaptive_thinking(&model.id, Some(&model.name)) {
                    bedrock_options.reasoning = Some(reasoning);
                    bedrock_options.thinking_budgets = options.thinking_budgets;
                    return stream(model, context, Some(bedrock_options));
                }

                // Undefined means the caller did not request an output cap;
                // let the helper use the model cap. Do not coerce to 0 here,
                // or the thinking budget would become the entire maxTokens
                // value.
                let (adjusted_max_tokens, thinking_budget) =
                    crate::simple_options::adjust_max_tokens_for_thinking(
                        options.max_tokens,
                        model.max_tokens,
                        reasoning,
                        options.thinking_budgets,
                    );
                let max_tokens = crate::simple_options::clamp_max_tokens_to_context(
                    model.context_window,
                    &context,
                    adjusted_max_tokens,
                );
                bedrock_options.max_tokens = Some(max_tokens);
                bedrock_options.reasoning = Some(reasoning);
                bedrock_options.thinking_budgets = Some(merge_budget_with_clamp(
                    options.thinking_budgets,
                    crate::simple_options::clamp_reasoning(Some(reasoning))
                        .expect("reasoning is Some"),
                    std::cmp::min(
                        thinking_budget,
                        max_tokens.saturating_sub(crate::simple_options::MIN_ANSWER_TOKENS),
                    ),
                ));
                stream(model, context, Some(bedrock_options))
            } else {
                bedrock_options.reasoning = Some(reasoning);
                bedrock_options.thinking_budgets = options.thinking_budgets;
                stream(model, context, Some(bedrock_options))
            }
        }
    }
}

/// Upstream `{...(options.thinkingBudgets || {}), [clampReasoning(level)]:
/// Math.min(adjusted.thinkingBudget, Math.max(0, maxTokens - 1024))}`.
fn merge_budget_with_clamp(
    budgets: Option<ThinkingBudgets>,
    level: ThinkingLevel,
    budget: u64,
) -> ThinkingBudgets {
    let mut merged = budgets.unwrap_or_default();
    match level {
        ThinkingLevel::Minimal => merged.minimal = Some(budget),
        ThinkingLevel::Low => merged.low = Some(budget),
        ThinkingLevel::Medium => merged.medium = Some(budget),
        ThinkingLevel::High => merged.high = Some(budget),
        _ => {}
    }
    merged
}
