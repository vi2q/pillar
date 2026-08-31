//! Port of packages/agent/src/proxy.ts (pi v0.84.3) — proxy stream function
//! for apps that route LLM calls through a server. The server manages auth
//! and proxies requests to LLM providers; the client reconstructs the
//! partial message from bandwidth-optimized (partial-stripped) SSE events.

use std::sync::Arc;

use pillar_ai::event_stream::{AssistantMessageEventStream, assistant_message_event_stream};
use pillar_ai::json_parse::parse_streaming_json;
use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse};
use pillar_ai::types::{
    AssistantMessage, AssistantMessageEvent, Content, Context, Model, StopReason, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// --- Proxy event protocol (upstream `ProxyAssistantMessageEvent`) --------

/// Server-sent proxy events. The server strips the `partial` field from
/// delta events to reduce bandwidth; we reconstruct the partial message
/// client-side (upstream `ProxyAssistantMessageEvent` union).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProxyAssistantMessageEvent {
    Start,
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(
            rename = "contentSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        content_signature: Option<String>,
    },
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(
            rename = "thinkingSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thinking_signature: Option<String>,
    },
    ToolcallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    ToolcallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    ToolcallEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "toolCall")]
        tool_call: Value,
    },
    Done {
        reason: String,
        usage: Usage,
    },
    Error {
        reason: String,
        #[serde(
            rename = "errorMessage",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        error_message: Option<String>,
        usage: Usage,
    },
}

/// Serializable subset of stream options sent to the proxy server (upstream
/// `ProxySerializableStreamOptions`). Non-serializable handles (signal,
/// fetch, callbacks) are deliberately excluded.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxySerializableStreamOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
}

/// Options for [`stream_proxy`] (upstream `ProxyStreamOptions`).
#[derive(Clone)]
pub struct ProxyStreamOptions {
    /// Bearer token forwarded as `Authorization` to the proxy server.
    pub auth_token: String,
    /// Proxy server base URL, e.g. `https://genai.example.com`.
    pub proxy_url: String,
    /// Local abort signal; aborting cancels the in-flight request.
    pub signal: Option<pillar_ai::abort::AbortSignal>,
    /// Serializable stream options forwarded to the server.
    pub options: ProxySerializableStreamOptions,
    /// Injectable HTTP transport; defaults to the reqwest-backed transport.
    pub fetch: Option<Arc<dyn FetchFn>>,
}

fn stop_reason_from_str(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::Stop,
        "length" => StopReason::Length,
        "toolUse" | "tool_use" => StopReason::ToolUse,
        "aborted" => StopReason::Aborted,
        "deferred" => StopReason::Deferred,
        _ => StopReason::Error,
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Stream function that proxies through a server instead of calling LLM
/// providers directly (upstream `streamProxy`). Use as the `streamFn` for an
/// `Agent` that needs to go through a proxy.
pub fn stream_proxy(
    model: &Model,
    context: &Context,
    options: &ProxyStreamOptions,
) -> AssistantMessageEventStream {
    let stream = assistant_message_event_stream();
    let task_stream = stream.clone_stream();

    let model = model.clone();
    let context = context.clone();
    let options = options.clone();

    // Drive the proxy request on a background task, pushing events into the
    // stream (upstream runs the same logic in an async IIFE).
    tokio::spawn(async move {
        // Initialize the partial message built up from events.
        let mut partial = AssistantMessage {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_millis(),
        };

        let fetch: Arc<dyn FetchFn> = match &options.fetch {
            Some(f) => f.clone(),
            None => {
                Arc::new(pillar_ai::transport::ReqwestFetch::new().expect("reqwest fetch init"))
            }
        };

        let body = serde_json::json!({
            "model": serde_json::to_value(&model).unwrap_or(Value::Null),
            "context": serde_json::to_value(&context).unwrap_or(Value::Null),
            "options": serde_json::to_value(&options.options).unwrap_or(Value::Null),
        });

        let request = FetchRequest::post(
            format!("{}/api/stream", options.proxy_url.trim_end_matches('/')),
            serde_json::to_vec(&body).unwrap_or_default(),
        )
        .with_header("Authorization", format!("Bearer {}", options.auth_token))
        .with_header("Content-Type", "application/json");

        let result = run_proxy_request(fetch, request, &options, &task_stream, &mut partial).await;

        if let Err(error_message) = result {
            let reason = if options.signal.as_ref().is_some_and(|s| s.is_aborted()) {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            partial.stop_reason = reason;
            partial.error_message = Some(error_message);
            task_stream.push(AssistantMessageEvent::Error {
                reason,
                error: partial.clone(),
            });
            task_stream.end(None);
        }
    });

    stream
}

/// Performs the POST, parses the SSE lines, and feeds each proxy event
/// through [`process_proxy_event`]. Returns `Err(message)` on transport /
/// HTTP / parse failure (upstream's try/catch + error event).
async fn run_proxy_request(
    fetch: Arc<dyn FetchFn>,
    request: FetchRequest,
    options: &ProxyStreamOptions,
    stream: &AssistantMessageEventStream,
    partial: &mut AssistantMessage,
) -> Result<(), String> {
    let response: FetchResponse = fetch
        .fetch(request)
        .await
        .map_err(|error| format!("Proxy error: {error}"))?;

    if response.status >= 400 {
        let mut message = format!("Proxy error: {}", response.status);
        if let Ok(body) = response.json().await {
            if let Some(error_text) = body.get("error").and_then(Value::as_str) {
                message = format!("Proxy error: {error_text}");
            }
        }
        return Err(message);
    }

    // Consume the streamed body as SSE: lines beginning with "data: " carry
    // one JSON proxy event each (upstream reads the WHATWG body reader).
    use futures::StreamExt;
    let mut body = response.body;
    let mut buffer = String::new();

    while let Some(chunk) = body.next().await {
        if options.signal.as_ref().is_some_and(|s| s.is_aborted()) {
            return Err("Request aborted by user".to_owned());
        }
        let bytes = chunk.map_err(|error| format!("Proxy error: {error}"))?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        let lines: Vec<String> = buffer.split('\n').map(str::to_owned).collect();
        let remainder = lines.last().cloned().unwrap_or_default();
        for line in &lines[..lines.len().saturating_sub(1)] {
            if let Some(data) = line.strip_prefix("data: ") {
                let proxy_event: ProxyAssistantMessageEvent = serde_json::from_str(data)
                    .map_err(|error| format!("Proxy error: invalid event JSON: {error}"))?;
                if let Some(event) = process_proxy_event(proxy_event, partial) {
                    stream.push(event);
                }
            }
        }
        buffer = remainder;
    }

    // Flush any final complete line left in the buffer.
    if let Some(data) = buffer.strip_prefix("data: ") {
        if let Ok(proxy_event) = serde_json::from_str::<ProxyAssistantMessageEvent>(data) {
            if let Some(event) = process_proxy_event(proxy_event, partial) {
                stream.push(event);
            }
        }
    }

    if options.signal.as_ref().is_some_and(|s| s.is_aborted()) {
        return Err("Request aborted by user".to_owned());
    }

    Ok(())
}

/// Process a proxy event and update the partial message (upstream
/// `processProxyEvent`). Returns the client-side assistant event to push, or
/// `None` when the event is ignored.
fn process_proxy_event(
    proxy_event: ProxyAssistantMessageEvent,
    partial: &mut AssistantMessage,
) -> Option<AssistantMessageEvent> {
    match proxy_event {
        ProxyAssistantMessageEvent::Start => Some(AssistantMessageEvent::Start {
            partial: partial.clone(),
        }),

        ProxyAssistantMessageEvent::TextStart { content_index } => {
            ensure_content_slot(partial, content_index, Content::text(String::new()));
            Some(AssistantMessageEvent::TextStart {
                content_index,
                partial: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::TextDelta {
            content_index,
            delta,
        } => match partial.content.get_mut(content_index) {
            Some(Content::Text { text, .. }) => {
                text.push_str(&delta);
                Some(AssistantMessageEvent::TextDelta {
                    content_index,
                    delta,
                    partial: partial.clone(),
                })
            }
            _ => {
                partial.stop_reason = StopReason::Error;
                partial.error_message = Some("Received text_delta for non-text content".to_owned());
                Some(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: partial.clone(),
                })
            }
        },

        ProxyAssistantMessageEvent::TextEnd {
            content_index,
            content_signature,
        } => match partial.content.get_mut(content_index) {
            Some(Content::Text {
                text,
                text_signature,
            }) => {
                *text_signature = content_signature;
                Some(AssistantMessageEvent::TextEnd {
                    content_index,
                    content: text.clone(),
                    partial: partial.clone(),
                })
            }
            _ => {
                partial.stop_reason = StopReason::Error;
                partial.error_message = Some("Received text_end for non-text content".to_owned());
                Some(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: partial.clone(),
                })
            }
        },

        ProxyAssistantMessageEvent::ThinkingStart { content_index } => {
            ensure_content_slot(
                partial,
                content_index,
                Content::Thinking {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                },
            );
            Some(AssistantMessageEvent::ThinkingStart {
                content_index,
                partial: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
        } => match partial.content.get_mut(content_index) {
            Some(Content::Thinking { thinking, .. }) => {
                thinking.push_str(&delta);
                Some(AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta,
                    partial: partial.clone(),
                })
            }
            _ => {
                partial.stop_reason = StopReason::Error;
                partial.error_message =
                    Some("Received thinking_delta for non-thinking content".to_owned());
                Some(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: partial.clone(),
                })
            }
        },

        ProxyAssistantMessageEvent::ThinkingEnd {
            content_index,
            thinking_signature,
        } => match partial.content.get_mut(content_index) {
            Some(Content::Thinking {
                thinking,
                thinking_signature: sig,
                ..
            }) => {
                *sig = thinking_signature;
                Some(AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    content: thinking.clone(),
                    partial: partial.clone(),
                })
            }
            _ => {
                partial.stop_reason = StopReason::Error;
                partial.error_message =
                    Some("Received thinking_end for non-thinking content".to_owned());
                Some(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: partial.clone(),
                })
            }
        },

        ProxyAssistantMessageEvent::ToolcallStart {
            content_index,
            id,
            tool_name,
        } => {
            ensure_content_slot(
                partial,
                content_index,
                Content::ToolCall {
                    id,
                    name: tool_name,
                    arguments: Value::Object(Default::default()),
                    thought_signature: None,
                    namespace: None,
                },
            );
            Some(AssistantMessageEvent::ToolcallStart {
                content_index,
                partial: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::ToolcallDelta {
            content_index,
            delta,
        } => {
            match partial.content.get_mut(content_index) {
                Some(Content::ToolCall { arguments, .. }) => {
                    // Accumulate the partial JSON and re-parse (upstream keeps
                    // a `partialJson` field; the port re-parses from a
                    // side buffer with the same parse_streaming_json).
                    accumulated_partial_json(arguments, &delta);
                    Some(AssistantMessageEvent::ToolcallDelta {
                        content_index,
                        delta,
                        partial: partial.clone(),
                    })
                }
                _ => {
                    partial.stop_reason = StopReason::Error;
                    partial.error_message =
                        Some("Received toolcall_delta for non-toolCall content".to_owned());
                    Some(AssistantMessageEvent::Error {
                        reason: StopReason::Error,
                        error: partial.clone(),
                    })
                }
            }
        }

        ProxyAssistantMessageEvent::ToolcallEnd {
            content_index,
            tool_call,
        } => {
            match partial.content.get_mut(content_index) {
                Some(Content::ToolCall {
                    id,
                    name,
                    arguments,
                    namespace,
                    ..
                }) => {
                    // Upstream Object.assigns the full toolCall (which may carry
                    // fields only present on toolcall_end, like namespace), then
                    // keeps the parsed arguments when the payload omits them.
                    let incoming_id = tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let incoming_name = tool_call
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    if let Some(incoming) = tool_call.get("arguments") {
                        if incoming.is_object() {
                            *arguments = incoming.clone();
                        }
                    }
                    if let Some(incoming) = tool_call.get("namespace").and_then(Value::as_str) {
                        *namespace = Some(incoming.to_owned());
                    }
                    if let Some(incoming) = incoming_id {
                        *id = incoming;
                    }
                    if let Some(incoming) = incoming_name {
                        *name = incoming;
                    }
                    Some(AssistantMessageEvent::ToolcallEnd {
                        content_index,
                        tool_call: partial.content[content_index].clone(),
                        partial: partial.clone(),
                    })
                }
                _ => None,
            }
        }

        ProxyAssistantMessageEvent::Done { reason, usage } => {
            partial.stop_reason = stop_reason_from_str(&reason);
            partial.usage = usage;
            Some(AssistantMessageEvent::Done {
                reason: partial.stop_reason,
                message: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::Error {
            reason,
            error_message,
            usage,
        } => {
            partial.stop_reason = stop_reason_from_str(&reason);
            partial.error_message = error_message;
            partial.usage = usage;
            Some(AssistantMessageEvent::Error {
                reason: partial.stop_reason,
                error: partial.clone(),
            })
        }
    }
}

/// Grow the content vec with defaults so the index exists (upstream indexes
/// assign directly into the sparse array).
fn ensure_content_slot(partial: &mut AssistantMessage, index: usize, content: Content) {
    while partial.content.len() <= index {
        partial.content.push(Content::text(String::new()));
    }
    partial.content[index] = content;
}

/// Upstream accumulates a `partialJson` string on the tool call, then parses
/// it with `parseStreamingJson` after each delta. Rust's `Content::ToolCall`
/// has no partialJson field, so the buffer rides in a side map keyed by
/// content index inside the arguments value's `_partialJson` marker — but to
/// keep the durable shape clean, we parse incrementally here instead.
fn accumulated_partial_json(arguments: &mut Value, delta: &str) {
    // Simple accumulation strategy: append to a hidden buffer stored beside
    // the parsed value via parse_streaming_json's tolerant semantics.
    let mut buffer = arguments
        .get("__partialJson")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    buffer.push_str(delta);
    let parsed = parse_streaming_json(Some(&buffer));
    if let Value::Object(map) = arguments {
        map.insert("__partialJson".to_owned(), Value::String(buffer));
        if let Value::Object(parsed_map) = parsed {
            for (key, value) in parsed_map {
                if key != "__partialJson" {
                    map.insert(key, value);
                }
            }
        }
    }
}
