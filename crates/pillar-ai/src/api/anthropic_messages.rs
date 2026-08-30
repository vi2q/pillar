//! Port of packages/ai/src/api/anthropic-messages.ts (pi v0.84.3) —
//! conversion & raw-SSE layer. The stream entry points (stream/streamSimple/
//! buildParams/processAnthropicStream) land in a follow-up change.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::types::{CacheRetention, Content, Message, Model, StopReason, Tool, ToolResultMessage};

// --- Compat --------------------------------------------------------------

/// Upstream `AnthropicMessagesCompat` — resolved fields with defaults
/// (upstream `getAnthropicCompat`).
#[derive(Debug, Clone)]
pub struct ResolvedAnthropicCompat {
    pub supports_eager_tool_input_streaming: bool,
    pub supports_long_cache_retention: bool,
    pub send_session_affinity_headers: bool,
    pub supports_cache_control_on_tools: bool,
    pub supports_temperature: bool,
    pub allow_empty_signature: bool,
    pub supports_strict_tools: bool,
    pub supports_tool_references: bool,
}

pub fn get_anthropic_compat(model: &Model) -> ResolvedAnthropicCompat {
    let compat = anthropic_compat_of(model);
    ResolvedAnthropicCompat {
        supports_eager_tool_input_streaming: compat
            .supports_eager_tool_input_streaming
            .unwrap_or(true),
        supports_long_cache_retention: compat.supports_long_cache_retention.unwrap_or(true),
        send_session_affinity_headers: compat.send_session_affinity_headers.unwrap_or(false),
        supports_cache_control_on_tools: compat.supports_cache_control_on_tools.unwrap_or(true),
        supports_temperature: compat.supports_temperature.unwrap_or(true),
        allow_empty_signature: compat.allow_empty_signature.unwrap_or(false),
        supports_strict_tools: compat.supports_strict_tools.unwrap_or(false),
        supports_tool_references: compat
            .supports_tool_references
            .unwrap_or_else(|| default_supports_tool_references(model)),
    }
}

/// Default for `supportsToolReferences`: first-party Anthropic models except
/// Haiku (rejects client-side tool_reference blocks) and models that predate
/// tool search (Claude 3.x, Opus/Sonnet 4.0, Opus 4.1).
pub fn default_supports_tool_references(model: &Model) -> bool {
    if model.provider != "anthropic" || model.id.contains("haiku") {
        return false;
    }
    let Some((major, minor)) = version_captures(&model.id) else {
        return false;
    };
    // Upstream: minor digit-run of length >= 8 is not a version (date suffix).
    let minor = match minor {
        Some(minor) if minor.len() < 8 => minor.parse::<u64>().unwrap_or(0),
        _ => 0,
    };
    major > 4 || (major == 4 && minor >= 5)
}

/// `^claude-(opus|sonnet|fable)-(\d+)(?:-(\d+))?(?:-|$)` as (major, minor).
fn version_captures(id: &str) -> Option<(u64, Option<&str>)> {
    let rest = id.strip_prefix("claude-")?;
    let (family, tail) = rest.split_once('-')?;
    if !matches!(family, "opus" | "sonnet" | "fable") {
        return None;
    }
    // tail = major digits, optionally "-" + minor digits, then end or "-...".
    let (major_str, after_major) = tail.split_once('-').unwrap_or((tail, ""));
    if major_str.is_empty() || !major_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let minor: Option<&str> = if after_major.is_empty() {
        None
    } else if after_major.chars().all(|c| c.is_ascii_digit()) {
        Some(after_major)
    } else {
        let (minor, _) = after_major.split_once('-')?;
        if minor.is_empty() || !minor.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        Some(minor)
    };
    Some((major_str.parse().ok()?, minor))
}

fn anthropic_compat_of(model: &Model) -> crate::types::AnthropicMessagesCompat {
    model
        .compat
        .as_ref()
        .and_then(|compat| match compat {
            crate::types::ModelCompat::AnthropicMessages(compat) => Some((**compat).clone()),
            _ => None,
        })
        .unwrap_or_default()
}

// --- Cache control -------------------------------------------------------

pub fn resolve_cache_retention_anthropic(
    cache_retention: Option<CacheRetention>,
    env: Option<&crate::types::ProviderEnv>,
) -> CacheRetention {
    if let Some(retention) = cache_retention {
        return retention;
    }
    if crate::provider_env::get_provider_env_value("PI_CACHE_RETENTION", env).as_deref()
        == Some("long")
    {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

pub fn get_cache_control(
    model: &Model,
    cache_retention: Option<CacheRetention>,
    env: Option<&crate::types::ProviderEnv>,
) -> (CacheRetention, Option<Value>) {
    let retention = resolve_cache_retention_anthropic(cache_retention, env);
    if retention == CacheRetention::None {
        return (retention, None);
    }
    let ttl = (retention == CacheRetention::Long
        && get_anthropic_compat(model).supports_long_cache_retention)
        .then_some("1h");
    let mut cache_control = Map::new();
    cache_control.insert("type".to_string(), Value::String("ephemeral".to_string()));
    if let Some(ttl) = ttl {
        cache_control.insert("ttl".to_string(), Value::String(ttl.to_string()));
    }
    (retention, Some(Value::Object(cache_control)))
}

// --- Claude Code stealth mode -------------------------------------------

pub const CLAUDE_CODE_VERSION: &str = "2.1.75";

/// Claude Code 2.x tool names (canonical casing).
pub const CLAUDE_CODE_TOOLS: [&str; 17] = [
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

/// Convert tool name to CC canonical casing if it matches (case-insensitive).
pub fn to_claude_code_name(name: &str) -> String {
    let lowered = name.to_lowercase();
    for canonical in CLAUDE_CODE_TOOLS {
        if canonical.to_lowercase() == lowered {
            return canonical.to_string();
        }
    }
    name.to_string()
}

pub fn from_claude_code_name(name: &str, tools: Option<&[Tool]>) -> String {
    if let Some(tools) = tools {
        if !tools.is_empty() {
            let lowered = name.to_lowercase();
            if let Some(matched) = tools
                .iter()
                .find(|tool| tool.name.to_lowercase() == lowered)
            {
                return matched.name.clone();
            }
        }
    }
    name.to_string()
}

pub fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

/// Anthropic IDs must match `^[a-zA-Z0-9_-]+$` (max 64 chars).
pub fn normalize_tool_call_id(id: &str) -> String {
    let filtered: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    filtered.chars().take(64).collect()
}

// --- Content conversion --------------------------------------------------

/// Upstream `convertContentBlocks`: text-only content collapses to a plain
/// string; images produce a content block array (with a placeholder text
/// block when no text accompanies them).
pub fn convert_content_blocks(content: &[Content]) -> Value {
    let has_images = content
        .iter()
        .any(|block| matches!(block, Content::Image { .. }));
    if !has_images {
        let text = content
            .iter()
            .filter_map(|block| match block {
                Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Value::String(crate::text::sanitize_surrogates(&text));
    }

    let mut blocks: Vec<Value> = Vec::new();
    for block in content {
        match block {
            Content::Text { text, .. } => blocks.push(json!({
                "type": "text",
                "text": crate::text::sanitize_surrogates(text),
            })),
            Content::Image {
                mime_type, data, ..
            } => blocks.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime_type,
                    "data": data,
                },
            })),
            _ => {}
        }
    }

    let has_text = blocks.iter().any(|block| block["type"] == json!("text"));
    if !has_text {
        blocks.insert(0, json!({ "type": "text", "text": "(see attached image)" }));
    }
    Value::Array(blocks)
}

/// Upstream `convertToolResult`: builds the tool_result block plus any
/// displaced sibling content for tool_reference-bearing results.
pub fn convert_tool_result(
    msg: &ToolResultMessage,
    is_oauth_token: bool,
    deferred_tool_names: &BTreeSet<String>,
    loaded_tool_names: &mut BTreeSet<String>,
    normalize_tool_name: impl Fn(&str) -> String,
) -> (Value, Vec<Value>) {
    let mut references: Vec<Value> = Vec::new();
    for name in msg.added_tool_names.iter().flatten() {
        let normalized_name = normalize_tool_name(name);
        if !deferred_tool_names.contains(&normalized_name)
            || loaded_tool_names.contains(&normalized_name)
        {
            continue;
        }
        loaded_tool_names.insert(normalized_name);
        references.push(json!({
            "type": "tool_reference",
            "tool_name": if is_oauth_token { to_claude_code_name(name) } else { name.clone() },
        }));
    }
    let converted_content = convert_content_blocks(&msg.content);
    // Anthropic rejects tool references mixed with ordinary tool-result content.
    let tool_result = json!({
        "type": "tool_result",
        "tool_use_id": msg.tool_call_id,
        "content": if references.is_empty() { converted_content.clone() } else { Value::Array(references) },
        "is_error": msg.is_error,
    });
    let sibling_content: Vec<Value> = if msg.added_tool_names.is_none() {
        Vec::new()
    } else {
        match converted_content {
            Value::String(text) => vec![json!({ "type": "text", "text": text })],
            other => other.as_array().cloned().unwrap_or_default(),
        }
    };
    (tool_result, sibling_content)
}

pub struct ConvertMessagesOptions<'a> {
    pub is_oauth_token: bool,
    pub cache_control: Option<Value>,
    pub allow_empty_signature: bool,
    pub deferred_tool_names: &'a BTreeSet<String>,
    pub normalize_tool_name: &'a dyn Fn(&str) -> String,
}

/// Upstream `convertMessages`: user/assistant/toolResult messages to
/// Anthropic `MessageParam`s, with consecutive toolResult batching and
/// trailing cache_control on the last user message.
pub fn convert_messages(
    transformed_messages: &[Message],
    options: ConvertMessagesOptions<'_>,
) -> Vec<Value> {
    let ConvertMessagesOptions {
        is_oauth_token,
        cache_control,
        allow_empty_signature,
        deferred_tool_names,
        normalize_tool_name,
    } = options;
    let mut params: Vec<Value> = Vec::new();
    let mut loaded_tool_names: BTreeSet<String> = BTreeSet::new();

    let mut index = 0usize;
    while index < transformed_messages.len() {
        let msg = &transformed_messages[index];
        match msg {
            Message::User { content, .. } => match content {
                crate::types::UserContent::Text(text) => {
                    if !text.trim().is_empty() {
                        params.push(json!({
                            "role": "user",
                            "content": crate::text::sanitize_surrogates(text),
                        }));
                    }
                }
                crate::types::UserContent::Blocks(blocks) => {
                    let converted: Vec<Value> = blocks
                        .iter()
                        .filter_map(|block| match block {
                            Content::Text { text, .. } => Some(json!({
                                "type": "text",
                                "text": crate::text::sanitize_surrogates(text),
                            })),
                            Content::Image {
                                data, mime_type, ..
                            } => Some(json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": mime_type,
                                    "data": data,
                                },
                            })),
                            _ => None,
                        })
                        .filter(|block| {
                            if block["type"] == json!("text") {
                                !block["text"].as_str().unwrap_or("").trim().is_empty()
                            } else {
                                true
                            }
                        })
                        .collect();
                    if !converted.is_empty() {
                        params.push(json!({ "role": "user", "content": converted }));
                    }
                }
            },
            Message::Assistant(assistant) => {
                let mut blocks: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        Content::Text { text, .. } => {
                            if text.trim().is_empty() {
                                continue;
                            }
                            blocks.push(json!({
                                "type": "text",
                                "text": crate::text::sanitize_surrogates(text),
                            }));
                        }
                        Content::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                            ..
                        } => {
                            // Redacted thinking: pass the opaque payload back as
                            // redacted_thinking (payload rides in thinkingSignature).
                            if *redacted == Some(true) {
                                if let Some(data) = thinking_signature {
                                    blocks
                                        .push(json!({ "type": "redacted_thinking", "data": data }));
                                }
                                continue;
                            }
                            let thinking_signature = thinking_signature.as_deref().unwrap_or("");
                            let has_thinking_signature = !thinking_signature.trim().is_empty();
                            if thinking.trim().is_empty() && !has_thinking_signature {
                                continue;
                            }
                            // If thinking signature is missing/empty (e.g., from aborted
                            // stream), convert to plain text for Anthropic. Some
                            // compatible providers emit and accept empty signatures, so
                            // let marked models preserve the block.
                            if !has_thinking_signature {
                                if allow_empty_signature {
                                    blocks.push(json!({
                                        "type": "thinking",
                                        "thinking": crate::text::sanitize_surrogates(thinking),
                                        "signature": "",
                                    }));
                                } else {
                                    blocks.push(json!({
                                        "type": "text",
                                        "text": crate::text::sanitize_surrogates(thinking),
                                    }));
                                }
                            } else {
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": crate::text::sanitize_surrogates(thinking),
                                    "signature": thinking_signature,
                                }));
                            }
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": if is_oauth_token { to_claude_code_name(name) } else { name.clone() },
                                "input": if arguments.is_null() { json!({}) } else { arguments.clone() },
                            }));
                        }
                        _ => {}
                    }
                }
                if blocks.is_empty() {
                    index += 1;
                    continue;
                }
                params.push(json!({ "role": "assistant", "content": blocks }));
            }
            Message::ToolResult(_) => {
                // Collect all consecutive toolResult messages, needed for z.ai
                // Anthropic endpoint.
                let mut tool_results: Vec<Value> = Vec::new();
                let mut sibling_content: Vec<Value> = Vec::new();
                let mut j = index;
                while j < transformed_messages.len() {
                    let Message::ToolResult(tool_result) = &transformed_messages[j] else {
                        break;
                    };
                    let converted = convert_tool_result(
                        tool_result,
                        is_oauth_token,
                        deferred_tool_names,
                        &mut loaded_tool_names,
                        normalize_tool_name,
                    );
                    tool_results.push(converted.0);
                    sibling_content.extend(converted.1);
                    j += 1;
                }

                // Skip the messages we've already processed.
                index = j - 1;

                // Displaced reference-bearing results must follow every
                // tool_result block.
                params.push(json!({
                    "role": "user",
                    "content": tool_results.into_iter().chain(sibling_content).collect::<Vec<_>>(),
                }));
            }
        }
        index += 1;
    }

    // Add cache_control to the last user message to cache conversation history
    if let Some(cache_control) = cache_control {
        if let Some(last_message) = params.last_mut() {
            if last_message["role"] == json!("user") {
                if let Some(content) = last_message.get_mut("content") {
                    if content.is_array() {
                        if let Some(last_block) = content.as_array_mut().unwrap().last_mut() {
                            let block_type = last_block["type"].as_str().unwrap_or("");
                            if block_type == "text"
                                || block_type == "image"
                                || block_type == "tool_result"
                            {
                                if let Some(block_obj) = last_block.as_object_mut() {
                                    block_obj.insert("cache_control".to_string(), cache_control);
                                }
                            }
                        }
                    } else if let Some(text) = content.as_str().map(str::to_string) {
                        *content = json!([{ "type": "text", "text": text, "cache_control": cache_control }]);
                    }
                }
            }
        }
    }

    params
}

pub fn convert_tools(
    tools: &[Tool],
    is_oauth_token: bool,
    supports_eager_tool_input_streaming: bool,
    supports_strict_tools: bool,
    cache_control: Option<&Value>,
    defer_loading: bool,
) -> Vec<Value> {
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let strict = crate::constrained_sampling::resolve_json_schema_strict_sampling(
                tool,
                supports_strict_tools,
            )
            .unwrap_or_else(|error| panic!("tool \"{}\" strict sampling: {error}", tool.name))
            .unwrap_or(false);
            let parameters =
                crate::constrained_sampling::get_json_schema_tool_parameters(tool, Some(strict))
                    .unwrap_or_else(|error| panic!("tool \"{}\" schema: {error}", tool.name));
            let legacy_input_schema = json!({
                "type": "object",
                "properties": parameters.get("properties").cloned().unwrap_or(json!({})),
                "required": parameters.get("required").cloned().unwrap_or(json!([])),
            });
            // Upstream strict: full parameters overlaid by the legacy shape.
            let input_schema = if strict {
                let full = parameters.as_object().cloned().unwrap_or_default();
                let legacy = legacy_input_schema.as_object().cloned().unwrap_or_default();
                let mut merged = full;
                for (key, value) in legacy {
                    merged.insert(key, value);
                }
                Value::Object(merged)
            } else {
                legacy_input_schema
            };

            let mut tool_json = Map::new();
            tool_json.insert(
                "name".to_string(),
                Value::String(if is_oauth_token {
                    to_claude_code_name(&tool.name)
                } else {
                    tool.name.clone()
                }),
            );
            tool_json.insert(
                "description".to_string(),
                Value::String(tool.description.clone()),
            );
            if supports_eager_tool_input_streaming {
                tool_json.insert("eager_input_streaming".to_string(), Value::Bool(true));
            }
            if strict {
                tool_json.insert("strict".to_string(), Value::Bool(true));
            }
            tool_json.insert("input_schema".to_string(), input_schema);
            if defer_loading {
                tool_json.insert("defer_loading".to_string(), Value::Bool(true));
            }
            if let Some(cache_control) = cache_control {
                if index == tools.len() - 1 {
                    tool_json.insert("cache_control".to_string(), cache_control.clone());
                }
            }
            Value::Object(tool_json)
        })
        .collect()
}

pub fn map_stop_reason(
    reason: &str,
    stop_details: Option<&Value>,
) -> Result<(StopReason, Option<String>), String> {
    match reason {
        "end_turn" => Ok((StopReason::Stop, None)),
        "max_tokens" => Ok((StopReason::Length, None)),
        "tool_use" => Ok((StopReason::ToolUse, None)),
        "refusal" => {
            let explanation = stop_details
                .and_then(|details| details.get("explanation"))
                .and_then(Value::as_str)
                .unwrap_or("The model refused to complete the request");
            Ok((StopReason::Error, Some(explanation.to_string())))
        }
        "pause_turn" => Ok((StopReason::Stop, None)), // Stop is good enough -> resubmit
        "stop_sequence" => Ok((StopReason::Stop, None)), // We don't supply stop sequences
        "sensitive" => Ok((
            StopReason::Error,
            Some("Provider stopped with: sensitive".to_string()),
        )),
        _ => Err(format!("Unhandled stop reason: {reason}")),
    }
}

// --- Raw SSE parsing -----------------------------------------------------

pub const ANTHROPIC_MESSAGE_EVENTS: [&str; 6] = [
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

pub struct ServerSentEvent {
    pub event: Option<String>,
    pub data: String,
    pub raw: Vec<String>,
}

#[derive(Default)]
pub struct SseDecoderState {
    pub event: Option<String>,
    pub data: Vec<String>,
    pub raw: Vec<String>,
}

pub fn flush_sse_event(state: &mut SseDecoderState) -> Option<ServerSentEvent> {
    if state.event.is_none() && state.data.is_empty() {
        return None;
    }
    let event = ServerSentEvent {
        event: state.event.take(),
        data: state.data.join("\n"),
        raw: state.raw.clone(),
    };
    state.data.clear();
    state.raw.clear();
    Some(event)
}

pub fn decode_sse_line(line: &str, state: &mut SseDecoderState) -> Option<ServerSentEvent> {
    if line.is_empty() {
        return flush_sse_event(state);
    }

    state.raw.push(line.to_string());
    if line.starts_with(':') {
        return None;
    }

    let (field_name, value) = match line.split_once(':') {
        Some((field_name, value)) => (field_name, value.strip_prefix(' ').unwrap_or(value)),
        None => (line, ""),
    };

    if field_name == "event" {
        state.event = Some(value.to_string());
    } else if field_name == "data" {
        state.data.push(value.to_string());
    }

    None
}

pub fn next_line_break_index(text: &str) -> Option<usize> {
    let carriage_return_index = text.find('\r');
    let newline_index = text.find('\n');
    match (carriage_return_index, newline_index) {
        (None, None) => None,
        (Some(cr), None) => Some(cr),
        (None, Some(lf)) => Some(lf),
        (Some(cr), Some(lf)) => Some(cr.min(lf)),
    }
}

pub struct ConsumedLine {
    pub line: String,
    pub rest: String,
}

pub fn consume_line(text: &str) -> Option<ConsumedLine> {
    let line_break_index = next_line_break_index(text)?;
    let mut next_index = line_break_index + 1;
    if text.as_bytes()[line_break_index] == b'\r' && text.as_bytes().get(next_index) == Some(&b'\n')
    {
        next_index += 1;
    }

    Some(ConsumedLine {
        line: text[..line_break_index].to_string(),
        rest: text[next_index..].to_string(),
    })
}

/// Feed a chunk of body text through the line decoder (streaming variant of
/// the upstream `iterateSseMessages` loop); returns events completed by it.
pub fn decode_sse_chunk(
    chunk: &str,
    state: &mut SseDecoderState,
    buffer: &mut String,
) -> Vec<ServerSentEvent> {
    let mut events = Vec::new();
    buffer.push_str(chunk);
    while let Some(consumed) = consume_line(buffer) {
        *buffer = consumed.rest;
        if let Some(event) = decode_sse_line(&consumed.line, state) {
            events.push(event);
        }
    }
    events
}

/// Finish a stream: parse a trailing partial line, then flush any pending
/// event (upstream tail handling in `iterateSseMessages`).
pub fn finish_sse_body(state: &mut SseDecoderState, buffer: &mut String) -> Vec<ServerSentEvent> {
    let mut events = Vec::new();
    if !buffer.is_empty() {
        let rest = std::mem::take(buffer);
        if let Some(event) = decode_sse_line(&rest, state) {
            events.push(event);
        }
    }
    if let Some(event) = flush_sse_event(state) {
        events.push(event);
    }
    events
}

/// Parse a complete SSE body in one shot (test helper parity with feeding
/// the whole body through the streaming decoder).
pub fn decode_sse_body(body: &str) -> Vec<ServerSentEvent> {
    let mut state = SseDecoderState::default();
    let mut buffer = String::new();
    let mut events = decode_sse_chunk(body, &mut state, &mut buffer);
    events.extend(finish_sse_body(&mut state, &mut buffer));
    events
}

pub fn is_anthropic_message_event(event: &str) -> bool {
    ANTHROPIC_MESSAGE_EVENTS.contains(&event)
}

pub fn parse_anthropic_event(sse: &ServerSentEvent) -> Result<Value, String> {
    crate::json_parse::parse_json_with_repair(&sse.data).ok_or_else(|| {
        format!(
            "Could not parse Anthropic SSE event {}: parse failure; data={}; raw={}",
            sse.event.as_deref().unwrap_or(""),
            sse.data,
            sse.raw.join("\\n")
        )
    })
}

/// Filter raw SSE events into Anthropic stream events, tracking
/// message_start/message_stop parity (upstream `iterateAnthropicEvents`).
pub fn iterate_anthropic_events(sse_events: &[ServerSentEvent]) -> Result<Vec<Value>, String> {
    let mut events = Vec::new();
    let mut saw_message_start = false;
    let mut saw_message_end = false;

    for sse in sse_events {
        if sse.event.as_deref() == Some("error") {
            return Err(sse.data.clone());
        }

        let Some(event_name) = sse.event.as_deref() else {
            continue;
        };
        if !is_anthropic_message_event(event_name) {
            continue;
        }

        let event = parse_anthropic_event(sse)?;
        match event["type"].as_str() {
            Some("message_start") => saw_message_start = true,
            Some("message_stop") => saw_message_end = true,
            _ => {}
        }
        events.push(event);
    }

    if saw_message_start && !saw_message_end {
        return Err("Anthropic stream ended before message_stop".to_string());
    }
    Ok(events)
}

/// Upstream `assertRequestAuth`: require an API key or an auth-bearing header.
pub fn assert_request_auth(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&crate::types::ProviderHeaders>,
) -> Result<(), String> {
    if api_key.is_some() {
        return Ok(());
    }
    let has_header = |name: &str| -> bool {
        let Some(headers) = headers else { return false };
        let expected = name.to_lowercase();
        headers.iter().any(|(key, value)| {
            key.to_lowercase() == expected
                && value
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
        })
    };
    if has_header("authorization") || has_header("x-api-key") || has_header("cf-aig-authorization")
    {
        return Ok(());
    }
    Err(format!("No API key for provider: {provider}"))
}
