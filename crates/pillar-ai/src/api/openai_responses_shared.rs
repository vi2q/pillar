//! Port of packages/ai/src/api/openai-responses-shared.ts (pi v0.84.3).
//!
//! Message/tool conversion and stream-event processing shared by the
//! `openai-responses`, `azure-openai-responses`, and `openai-codex-responses`
//! adapters. Upstream types the items with the OpenAI SDK; the Rust port
//! models items as `serde_json::Value` shaped exactly like the wire format.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{Map, Value, json};

use crate::constrained_sampling::{
    append_grammar_tool_input_json_delta, get_grammar_tool_input, get_json_schema_tool_parameters,
    resolve_grammar_constrained_sampling, resolve_json_schema_strict_sampling,
};
use crate::hash::short_hash;
use crate::json_parse::parse_streaming_json;
use crate::models::calculate_cost;
use crate::text::sanitize_surrogates;
use crate::transform_messages::transform_messages;
use crate::types::{AssistantMessage, Content, Context, Model, StopReason, Tool, Usage};

// =============================================================================
// Utilities
// =============================================================================

/// Upstream `TextSignatureV1` encoding for text blocks.
pub fn encode_text_signature_v1(id: &str, phase: Option<&str>) -> String {
    let mut payload = Map::new();
    payload.insert("v".to_string(), json!(1));
    payload.insert("id".to_string(), Value::String(id.to_string()));
    if let Some(phase) = phase {
        payload.insert("phase".to_string(), Value::String(phase.to_string()));
    }
    Value::Object(payload).to_string()
}

/// Parse a text signature: `TextSignatureV1` JSON or a legacy plain id.
pub fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<String>)> {
    let signature = signature?;
    if signature.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(signature) {
            if parsed.get("v") == Some(&json!(1)) {
                if let Some(id) = parsed.get("id").and_then(Value::as_str) {
                    let phase = match parsed.get("phase").and_then(Value::as_str) {
                        Some(phase @ ("commentary" | "final_answer")) => Some(phase.to_string()),
                        _ => None,
                    };
                    return Some((id.to_string(), phase));
                }
            }
        }
    }
    Some((signature.to_string(), None))
}

type ToolResultOutputContent = Vec<Value>;

fn convert_tool_result_output(model: &Model, content: &[Content]) -> Value {
    let text_result = content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<&Content> = content
        .iter()
        .filter(|block| matches!(block, Content::Image { .. }))
        .collect();
    let has_text = !text_result.is_empty();

    if images.is_empty() || !model.input.iter().any(|input| input == "image") {
        let fallback = if has_text {
            text_result
        } else if !images.is_empty() {
            "(see attached image)".to_string()
        } else {
            "(no tool output)".to_string()
        };
        return Value::String(sanitize_surrogates(&fallback));
    }

    let mut output: ToolResultOutputContent = Vec::new();
    if has_text {
        output.push(json!({ "type": "input_text", "text": sanitize_surrogates(&text_result) }));
    }
    for image in images {
        if let Content::Image { data, mime_type } = image {
            output.push(json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": format!("data:{mime_type};base64,{data}")
            }));
        }
    }
    Value::Array(output)
}

// =============================================================================
// Message conversion
// =============================================================================

/// Upstream `ConvertResponsesMessagesOptions`.
#[derive(Default)]
pub struct ConvertResponsesMessagesOptions<'a> {
    pub include_system_prompt: Option<bool>,
    pub grammar_tool_input_properties: Option<&'a BTreeMap<String, String>>,
    /// Deferred tools keyed by normalized tool name.
    pub deferred_tools: Option<&'a BTreeMap<String, Tool>>,
    /// "additional-tools" or "tool-search".
    pub deferred_tools_mode: Option<&'a str>,
    pub tool_options: Option<ConvertResponsesToolsOptions>,
}

/// Convert context messages into the Responses `input` array.
pub fn convert_responses_messages(
    model: &Model,
    context: &Context,
    allowed_tool_call_providers: &BTreeSet<String>,
    options: Option<ConvertResponsesMessagesOptions<'_>>,
) -> Vec<Value> {
    let options = &options;
    let mut messages: Vec<Value> = Vec::new();
    let mut loaded_tool_names: BTreeSet<String> = BTreeSet::new();

    let normalize_id_part = |part: &str| -> String {
        let sanitized: String = part
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let normalized: String = sanitized.chars().take(64).collect();
        let trimmed = normalized.trim_end_matches('_');
        trimmed.to_string()
    };

    let build_foreign_responses_item_id = |item_id: &str| -> String {
        let normalized = format!("fc_{}", short_hash(item_id));
        normalized.chars().take(64).collect()
    };

    let normalize_tool_call_id = |id: &str, source: &AssistantMessage| -> String {
        if !allowed_tool_call_providers.contains(&model.provider) {
            return normalize_id_part(id);
        }
        if !id.contains('|') {
            return normalize_id_part(id);
        }
        let (call_id, item_id) = id.split_once('|').expect("contains |");
        let normalized_call_id = normalize_id_part(call_id);
        let is_foreign_tool_call = source.provider != model.provider || source.api != model.api;
        let mut normalized_item_id = if is_foreign_tool_call {
            build_foreign_responses_item_id(item_id)
        } else {
            normalize_id_part(item_id)
        };
        // OpenAI Responses API requires item id to start with "fc".
        if !normalized_item_id.starts_with("fc_") {
            normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
        }
        format!("{normalized_call_id}|{normalized_item_id}")
    };

    let transformed_messages = transform_messages(
        context.messages.clone(),
        model,
        Some(&|id, _target_model, source| normalize_tool_call_id(id, source)),
    );

    let include_system_prompt = options
        .as_ref()
        .and_then(|o| o.include_system_prompt)
        .unwrap_or(true);
    if include_system_prompt {
        if let Some(system_prompt) = &context.system_prompt {
            let role = if model.reasoning {
                "developer"
            } else {
                "system"
            };
            messages.push(json!({ "role": role, "content": sanitize_surrogates(system_prompt) }));
        }
    }

    let mut msg_index = 0usize;
    for message in &transformed_messages {
        match message {
            crate::types::Message::User { content, .. } => match content {
                crate::types::UserContent::Text(text) => {
                    messages.push(json!({
                        "role": "user",
                        "content": [{ "type": "input_text", "text": sanitize_surrogates(text) }]
                    }));
                }
                crate::types::UserContent::Blocks(blocks) => {
                    let parts: Vec<Value> = blocks
                        .iter()
                        .map(|block| match block {
                            Content::Text { text, .. } => {
                                json!({ "type": "input_text", "text": sanitize_surrogates(text) })
                            }
                            Content::Image { data, mime_type } => json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{mime_type};base64,{data}")
                            }),
                            _ => Value::Null,
                        })
                        .filter(|part| !part.is_null())
                        .collect();
                    if parts.is_empty() {
                        msg_index += 1;
                        continue;
                    }
                    messages.push(json!({ "role": "user", "content": parts }));
                }
            },
            crate::types::Message::Assistant(assistant) => {
                let mut output: Vec<Value> = Vec::new();
                let is_same_provider_and_api =
                    assistant.provider == model.provider && assistant.api == model.api;
                let is_same_model = is_same_provider_and_api && assistant.model == model.id;
                let is_different_model = is_same_provider_and_api && assistant.model != model.id;
                let mut text_block_index = 0usize;

                for block in &assistant.content {
                    match block {
                        Content::Thinking {
                            thinking_signature: Some(signature),
                            ..
                        } => {
                            // The signature slot carries the raw reasoning
                            // item; push it verbatim.
                            if let Ok(item) = serde_json::from_str::<Value>(signature) {
                                output.push(item);
                            }
                        }
                        Content::Thinking { .. } => {}
                        Content::Text {
                            text,
                            text_signature,
                        } => {
                            let parsed_signature = parse_text_signature(text_signature.as_deref());
                            let fallback_message_id = if text_block_index == 0 {
                                format!("msg_pi_{msg_index}")
                            } else {
                                format!("msg_pi_{msg_index}_{text_block_index}")
                            };
                            text_block_index += 1;
                            // OpenAI requires id to be max 64 characters.
                            let msg_id = match parsed_signature.as_ref().map(|(id, _)| id.clone()) {
                                Some(id) if id.chars().count() > 64 => {
                                    format!("msg_{}", short_hash(&id))
                                }
                                Some(id) => id,
                                None => fallback_message_id,
                            };
                            let mut item = Map::new();
                            item.insert("type".to_string(), Value::String("message".to_string()));
                            item.insert("role".to_string(), Value::String("assistant".to_string()));
                            item.insert(
                                "content".to_string(),
                                json!([{
                                    "type": "output_text",
                                    "text": sanitize_surrogates(text),
                                    "annotations": []
                                }]),
                            );
                            item.insert(
                                "status".to_string(),
                                Value::String("completed".to_string()),
                            );
                            item.insert("id".to_string(), Value::String(msg_id));
                            if let Some((_, Some(phase))) = &parsed_signature {
                                item.insert("phase".to_string(), Value::String(phase.clone()));
                            }
                            output.push(Value::Object(item));
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            namespace,
                            ..
                        } => {
                            let (call_id, item_id_raw) = match id.split_once('|') {
                                Some((call_id, item_id)) => (call_id, Some(item_id)),
                                None => (id.as_str(), None),
                            };
                            let custom_input_property = options
                                .as_ref()
                                .and_then(|o| o.grammar_tool_input_properties)
                                .and_then(|properties| properties.get(name).cloned());
                            let mut item_id: Option<String> = item_id_raw.map(str::to_string);

                            // For different-model messages, drop the id to
                            // avoid pairing validation. When replaying custom
                            // tool calls as function calls, also drop
                            // non-fc_* ids.
                            if (is_different_model
                                && item_id
                                    .as_deref()
                                    .is_some_and(|item| item.starts_with("fc_")))
                                || (custom_input_property.is_none()
                                    && !item_id
                                        .as_deref()
                                        .is_some_and(|item| item.starts_with("fc_")))
                            {
                                item_id = None;
                            }

                            let can_replay_namespace = is_same_model
                                || options
                                    .as_ref()
                                    .and_then(|o| o.deferred_tools)
                                    .is_some_and(|deferred| deferred.contains_key(name));

                            if let Some(custom_input_property) = custom_input_property {
                                let input =
                                    get_grammar_tool_input(name, arguments, &custom_input_property)
                                        .unwrap_or_default();
                                let mut item = Map::new();
                                item.insert(
                                    "type".to_string(),
                                    Value::String("custom_tool_call".to_string()),
                                );
                                if let Some(item_id) = &item_id {
                                    item.insert("id".to_string(), Value::String(item_id.clone()));
                                }
                                item.insert(
                                    "call_id".to_string(),
                                    Value::String(call_id.to_string()),
                                );
                                item.insert("name".to_string(), Value::String(name.clone()));
                                item.insert(
                                    "input".to_string(),
                                    Value::String(sanitize_surrogates(&input)),
                                );
                                if can_replay_namespace {
                                    if let Some(namespace) = namespace {
                                        item.insert(
                                            "namespace".to_string(),
                                            Value::String(namespace.clone()),
                                        );
                                    }
                                }
                                output.push(Value::Object(item));
                            } else {
                                let mut item = Map::new();
                                item.insert(
                                    "type".to_string(),
                                    Value::String("function_call".to_string()),
                                );
                                if let Some(item_id) = &item_id {
                                    item.insert("id".to_string(), Value::String(item_id.clone()));
                                }
                                item.insert(
                                    "call_id".to_string(),
                                    Value::String(call_id.to_string()),
                                );
                                item.insert("name".to_string(), Value::String(name.clone()));
                                item.insert(
                                    "arguments".to_string(),
                                    Value::String(arguments.to_string()),
                                );
                                if can_replay_namespace {
                                    if let Some(namespace) = namespace {
                                        item.insert(
                                            "namespace".to_string(),
                                            Value::String(namespace.clone()),
                                        );
                                    }
                                }
                                output.push(Value::Object(item));
                            }
                        }
                        Content::Image { .. } => {}
                    }
                }
                if output.is_empty() {
                    msg_index += 1;
                    continue;
                }
                messages.extend(output);
            }
            crate::types::Message::ToolResult(tool_result) => {
                let call_id = tool_result
                    .tool_call_id
                    .split_once('|')
                    .map(|(call_id, _)| call_id)
                    .unwrap_or(&tool_result.tool_call_id);
                let output = convert_tool_result_output(model, &tool_result.content);

                let is_custom = options
                    .as_ref()
                    .and_then(|o| o.grammar_tool_input_properties)
                    .is_some_and(|properties| properties.contains_key(&tool_result.tool_name));
                let mut item = Map::new();
                item.insert(
                    "type".to_string(),
                    Value::String(
                        if is_custom {
                            "custom_tool_call_output"
                        } else {
                            "function_call_output"
                        }
                        .to_string(),
                    ),
                );
                item.insert("call_id".to_string(), Value::String(call_id.to_string()));
                item.insert("output".to_string(), output);
                messages.push(Value::Object(item));

                // Deferred tools loaded via the transcript.
                let mut deferred_tools: Vec<Tool> = Vec::new();
                for name in tool_result.added_tool_names.iter().flatten() {
                    let tool = options
                        .as_ref()
                        .and_then(|o| o.deferred_tools)
                        .and_then(|deferred| deferred.get(name))
                        .cloned();
                    let Some(tool) = tool else { continue };
                    if loaded_tool_names.contains(name) {
                        continue;
                    }
                    loaded_tool_names.insert(name.clone());
                    deferred_tools.push(tool);
                }
                let deferred_tools_mode = options.as_ref().and_then(|o| o.deferred_tools_mode);
                if !deferred_tools.is_empty() && deferred_tools_mode == Some("additional-tools") {
                    let converted = convert_responses_tools(
                        &deferred_tools,
                        options.as_ref().and_then(|o| o.tool_options.as_ref()),
                    );
                    messages.push(json!({
                        "type": "additional_tools",
                        "role": "developer",
                        "tools": converted
                    }));
                } else if !deferred_tools.is_empty() && deferred_tools_mode == Some("tool-search") {
                    let names: Vec<&str> = deferred_tools
                        .iter()
                        .map(|tool| tool.name.as_str())
                        .collect();
                    let search_call_id = format!(
                        "pi_tool_load_{}",
                        short_hash(&format!("{}:{}", tool_result.tool_call_id, names.join(",")))
                    );
                    messages.push(json!({
                        "type": "tool_search_call",
                        "call_id": search_call_id,
                        "execution": "client",
                        "status": "completed",
                        "arguments": { "query": names.join(" "), "limit": names.len() }
                    }));
                    let mut tool_options = options
                        .as_ref()
                        .and_then(|o| o.tool_options.clone())
                        .unwrap_or_default();
                    tool_options.defer_loading = Some(true);
                    let converted = convert_responses_tools(&deferred_tools, Some(&tool_options));
                    messages.push(json!({
                        "type": "tool_search_output",
                        "call_id": search_call_id,
                        "execution": "client",
                        "status": "completed",
                        "tools": converted
                    }));
                }
            }
        }
        msg_index += 1;
    }

    messages
}

// =============================================================================
// Tool conversion
// =============================================================================

/// Upstream `ConvertResponsesToolsOptions`.
#[derive(Debug, Clone, Default)]
pub struct ConvertResponsesToolsOptions {
    pub strict: Option<bool>,
    pub supports_strict_mode: Option<bool>,
    pub supports_openai_grammar_tools: Option<bool>,
    pub defer_loading: Option<bool>,
}

/// Convert tools into OpenAI Responses tool definitions.
pub fn convert_responses_tools(
    tools: &[Tool],
    options: Option<&ConvertResponsesToolsOptions>,
) -> Vec<Value> {
    let default_strict = options.and_then(|o| o.strict).unwrap_or(false);
    let supports_strict_mode = options.and_then(|o| o.supports_strict_mode).unwrap_or(true);
    let supports_openai_grammar_tools = options
        .and_then(|o| o.supports_openai_grammar_tools)
        .unwrap_or(false);
    let defer_loading = options.and_then(|o| o.defer_loading).unwrap_or(false);

    tools
        .iter()
        .map(|tool| {
            if let Ok(Some(grammar)) =
                resolve_grammar_constrained_sampling(tool, supports_openai_grammar_tools)
            {
                let mut item = Map::new();
                item.insert("type".to_string(), Value::String("custom".to_string()));
                item.insert("name".to_string(), Value::String(tool.name.clone()));
                item.insert("description".to_string(), Value::String(tool.description.clone()));
                item.insert(
                    "format".to_string(),
                    json!({ "type": "grammar", "syntax": grammar.format, "definition": grammar.definition }),
                );
                if defer_loading {
                    item.insert("defer_loading".to_string(), Value::Bool(true));
                }
                return Value::Object(item);
            }

            let constrained_strict =
                resolve_json_schema_strict_sampling(tool, supports_strict_mode).unwrap_or_else(|error| {
                    panic!("tool \"{}\" strict sampling: {error}", tool.name)
                });
            let strict = constrained_strict.unwrap_or(default_strict);
            let parameters = get_json_schema_tool_parameters(tool, Some(strict))
                .unwrap_or_else(|error| panic!("tool \"{}\" parameters: {error}", tool.name));

            let mut item = Map::new();
            item.insert("type".to_string(), Value::String("function".to_string()));
            item.insert("name".to_string(), Value::String(tool.name.clone()));
            item.insert("description".to_string(), Value::String(tool.description.clone()));
            item.insert("parameters".to_string(), parameters);
            if defer_loading {
                item.insert("defer_loading".to_string(), Value::Bool(true));
            }
            if supports_strict_mode {
                item.insert("strict".to_string(), Value::Bool(strict));
            }
            Value::Object(item)
        })
        .collect()
}

// =============================================================================
// Stream processing
// =============================================================================

#[derive(Debug, Clone)]
struct StreamingToolCall {
    id: String,
    name: String,
    arguments: Value,
    namespace: Option<String>,
    partial_json: Option<String>,
    custom_input: Option<(
        String,
        crate::constrained_sampling::GrammarToolInputJsonBuffer,
    )>,
}

#[derive(Debug, Clone)]
enum OutputSlot {
    Thinking {
        content_index: usize,
        item_id: Option<String>,
    },
    Text {
        content_index: usize,
    },
    ToolCall {
        content_index: usize,
        call: StreamingToolCall,
    },
}

type ServiceTierPricingFn<'a> = Box<dyn FnMut(&mut Usage, Option<&str>) + Send + 'a>;

/// Options for [`process_responses_stream`] (upstream
/// `OpenAIResponsesStreamOptions`; service-tier pricing hooks are handled by
/// the caller-facing adapter).
pub struct ProcessResponsesStreamOptions<'a> {
    pub grammar_tool_input_properties: &'a BTreeMap<String, String>,
    /// Receives (usage, resolved service tier) at terminal events.
    pub apply_service_tier_pricing: Option<ServiceTierPricingFn<'a>>,
}

/// Process a Responses API event stream into `output` / `stream`
/// (upstream `processResponsesStream`). Events are wire-shaped JSON values.
pub async fn process_responses_stream<E>(
    mut events: E,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    model: &Model,
    options: Option<ProcessResponsesStreamOptions<'_>>,
) -> Result<(), crate::error::AiError>
where
    E: futures::Stream<Item = Result<Value, crate::error::AiError>> + Unpin,
{
    process_responses_stream_inner(&mut events, output, stream, model, options).await
}

async fn process_responses_stream_inner<E>(
    events: &mut E,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    model: &Model,
    mut options: Option<ProcessResponsesStreamOptions<'_>>,
) -> Result<(), crate::error::AiError>
where
    E: futures::Stream<Item = Result<Value, crate::error::AiError>> + Unpin,
{
    use futures::StreamExt;

    let mut saw_terminal_response_event = false;
    let mut output_slots: HashMap<i64, OutputSlot> = HashMap::new();
    let mut reasoning_blocks_by_id: HashMap<String, usize> = HashMap::new();
    let grammar_properties = options
        .as_ref()
        .map(|o| o.grammar_tool_input_properties.clone())
        .unwrap_or_default();

    // Rebuild output.content indices: slots own content entries.
    let mut content_blocks: Vec<Content> = Vec::new();

    macro_rules! sync_content {
        () => {
            output.content = content_blocks.clone();
        };
    }

    let apply_message_phase_stop_reason = |item: &Value, output: &mut AssistantMessage| {
        if item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("phase").and_then(Value::as_str) == Some("final_answer")
        {
            output.stop_reason = StopReason::Stop;
        }
    };

    while let Some(event_result) = events.next().await {
        let event = event_result?;
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        match event_type.as_str() {
            "response.created" => {
                if let Some(id) = event.pointer("/response/id").and_then(Value::as_str) {
                    output.response_id = Some(id.to_string());
                }
            }
            "response.output_item.added" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                apply_message_phase_stop_reason(&item, output);
                create_slot(
                    output_index,
                    &item,
                    &mut output_slots,
                    &mut content_blocks,
                    output,
                    stream,
                    &grammar_properties,
                );
            }
            "response.reasoning_summary_text.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let Some(OutputSlot::Thinking { content_index, .. }) =
                    get_slot(&mut output_slots, output_index, "thinking")
                else {
                    continue;
                };
                let content_index = *content_index;
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if let Some(Content::Thinking { thinking, .. }) =
                        content_blocks.get_mut(content_index)
                    {
                        thinking.push_str(delta);
                    }
                    sync_content!();
                    stream.push(crate::types::AssistantMessageEvent::ThinkingDelta {
                        content_index,
                        delta: delta.to_string(),
                        partial: output.clone(),
                    });
                }
            }
            "response.reasoning_summary_part.done" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let Some(OutputSlot::Thinking { content_index, .. }) =
                    get_slot(&mut output_slots, output_index, "thinking")
                else {
                    continue;
                };
                let content_index = *content_index;
                if let Some(Content::Thinking { thinking, .. }) =
                    content_blocks.get_mut(content_index)
                {
                    thinking.push_str("\n\n");
                }
                sync_content!();
                stream.push(crate::types::AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta: "\n\n".to_string(),
                    partial: output.clone(),
                });
            }
            "response.reasoning_text.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let Some(OutputSlot::Thinking { content_index, .. }) =
                    get_slot(&mut output_slots, output_index, "thinking")
                else {
                    continue;
                };
                let content_index = *content_index;
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if let Some(Content::Thinking { thinking, .. }) =
                        content_blocks.get_mut(content_index)
                    {
                        thinking.push_str(delta);
                    }
                    sync_content!();
                    stream.push(crate::types::AssistantMessageEvent::ThinkingDelta {
                        content_index,
                        delta: delta.to_string(),
                        partial: output.clone(),
                    });
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let Some(OutputSlot::Text { content_index, .. }) =
                    get_slot(&mut output_slots, output_index, "text")
                else {
                    continue;
                };
                let content_index = *content_index;
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if let Some(Content::Text { text, .. }) = content_blocks.get_mut(content_index)
                    {
                        text.push_str(delta);
                    }
                    sync_content!();
                    stream.push(crate::types::AssistantMessageEvent::TextDelta {
                        content_index,
                        delta: delta.to_string(),
                        partial: output.clone(),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let Some(OutputSlot::ToolCall {
                    content_index,
                    call,
                    ..
                }) = get_slot(&mut output_slots, output_index, "toolCall")
                else {
                    continue;
                };
                let content_index = *content_index;
                let call = &mut *call;
                let Some(delta) = event.get("delta").and_then(Value::as_str) else {
                    continue;
                };
                let Some(partial_json) = &mut call.partial_json else {
                    continue;
                };
                partial_json.push_str(delta);
                call.arguments = parse_streaming_json(Some(partial_json));
                if let Some(Content::ToolCall { arguments, .. }) =
                    content_blocks.get_mut(content_index)
                {
                    *arguments = call.arguments.clone();
                }
                sync_content!();
                stream.push(crate::types::AssistantMessageEvent::ToolcallDelta {
                    content_index,
                    delta: delta.to_string(),
                    partial: output.clone(),
                });
            }
            "response.function_call_arguments.done" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let Some(OutputSlot::ToolCall {
                    content_index,
                    call,
                    ..
                }) = get_slot(&mut output_slots, output_index, "toolCall")
                else {
                    continue;
                };
                let content_index = *content_index;
                let call = &mut *call;
                let Some(partial_json) = &mut call.partial_json else {
                    continue;
                };
                let previous_partial_json = partial_json.clone();
                let arguments = event.get("arguments").and_then(Value::as_str).unwrap_or("");
                *partial_json = arguments.to_string();
                call.arguments = parse_streaming_json(Some(partial_json));
                if let Some(Content::ToolCall { arguments, .. }) =
                    content_blocks.get_mut(content_index)
                {
                    *arguments = call.arguments.clone();
                }

                if arguments.starts_with(&previous_partial_json) {
                    let delta = &arguments[previous_partial_json.len()..];
                    if !delta.is_empty() {
                        sync_content!();
                        stream.push(crate::types::AssistantMessageEvent::ToolcallDelta {
                            content_index,
                            delta: delta.to_string(),
                            partial: output.clone(),
                        });
                    }
                }
            }
            "response.custom_tool_call_input.delta" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let Some(OutputSlot::ToolCall {
                    content_index,
                    call,
                    ..
                }) = get_slot(&mut output_slots, output_index, "toolCall")
                else {
                    continue;
                };
                let content_index = *content_index;
                let call = &mut *call;
                let Some((property, buffer)) = &mut call.custom_input else {
                    continue;
                };
                let Some(delta) = event.get("delta").and_then(Value::as_str) else {
                    continue;
                };
                let current = call
                    .arguments
                    .get(property.as_str())
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let next_input = format!("{current}{delta}");
                let emitted =
                    append_grammar_tool_input_json_delta(buffer, property, &next_input, false)
                        .ok()
                        .flatten();
                call.arguments = json!({ property.clone(): next_input });
                sync_content!();
                if let Some(emitted) = emitted {
                    stream.push(crate::types::AssistantMessageEvent::ToolcallDelta {
                        content_index,
                        delta: emitted,
                        partial: output.clone(),
                    });
                }
            }
            "response.custom_tool_call_input.done" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let Some(OutputSlot::ToolCall {
                    content_index,
                    call,
                    ..
                }) = get_slot(&mut output_slots, output_index, "toolCall")
                else {
                    continue;
                };
                let content_index = *content_index;
                let call = &mut *call;
                let Some((property, buffer)) = &mut call.custom_input else {
                    continue;
                };
                let input = event.get("input").and_then(Value::as_str);
                let next_input = match input {
                    Some(input) => input.to_string(),
                    None => call
                        .arguments
                        .get(property.as_str())
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                };
                let emitted =
                    append_grammar_tool_input_json_delta(buffer, property, &next_input, true)
                        .ok()
                        .flatten();
                call.arguments = json!({ property.clone(): next_input });
                sync_content!();
                if let Some(emitted) = emitted {
                    stream.push(crate::types::AssistantMessageEvent::ToolcallDelta {
                        content_index,
                        delta: emitted,
                        partial: output.clone(),
                    });
                }
            }
            "response.output_item.done" => {
                let output_index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                apply_message_phase_stop_reason(&item, output);
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");

                if item_type == "reasoning" {
                    let Some(OutputSlot::Thinking {
                        content_index,
                        item_id,
                        ..
                    }) = get_slot(&mut output_slots, output_index, "thinking")
                    else {
                        continue;
                    };
                    let content_index = *content_index;
                    let summary_text = item
                        .get("summary")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default();
                    let content_text = item
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default();
                    let final_text = if !summary_text.is_empty() {
                        summary_text
                    } else if !content_text.is_empty() {
                        content_text
                    } else {
                        match content_blocks.get(content_index) {
                            Some(Content::Thinking { thinking, .. }) => thinking.clone(),
                            _ => String::new(),
                        }
                    };
                    if let Some(Content::Thinking {
                        thinking,
                        thinking_signature,
                        ..
                    }) = content_blocks.get_mut(content_index)
                    {
                        *thinking = final_text.clone();
                        *thinking_signature = Some(item.to_string());
                    }
                    if let Some(item_id) = item_id.clone() {
                        reasoning_blocks_by_id.insert(item_id, content_index);
                    }
                    sync_content!();
                    stream.push(crate::types::AssistantMessageEvent::ThinkingEnd {
                        content_index,
                        content: final_text,
                        partial: output.clone(),
                    });
                    output_slots.remove(&output_index);
                } else if item_type == "message" {
                    let Some(OutputSlot::Text { content_index, .. }) =
                        get_slot(&mut output_slots, output_index, "text")
                    else {
                        continue;
                    };
                    let content_index = *content_index;
                    let text = item
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                                    Some("output_text") => part.get("text").and_then(Value::as_str),
                                    _ => part.get("refusal").and_then(Value::as_str),
                                })
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                    let phase = item.get("phase").and_then(Value::as_str);
                    let signature = encode_text_signature_v1(item_id, phase);
                    if let Some(Content::Text {
                        text: block_text,
                        text_signature: block_signature,
                    }) = content_blocks.get_mut(content_index)
                    {
                        *block_text = text.clone();
                        *block_signature = Some(signature);
                    }
                    sync_content!();
                    stream.push(crate::types::AssistantMessageEvent::TextEnd {
                        content_index,
                        content: text,
                        partial: output.clone(),
                    });
                    output_slots.remove(&output_index);
                } else if item_type == "function_call" {
                    let Some(OutputSlot::ToolCall {
                        content_index,
                        call,
                        ..
                    }) = get_slot(&mut output_slots, output_index, "toolCall")
                    else {
                        continue;
                    };
                    let content_index = *content_index;
                    let call = &mut *call;
                    if call.partial_json.is_none() {
                        continue;
                    }
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    call.arguments = parse_streaming_json(Some(arguments));
                    if let Some(namespace) = item.get("namespace").and_then(Value::as_str) {
                        call.namespace = Some(namespace.to_string());
                    }
                    if let Some(Content::ToolCall { arguments, .. }) =
                        content_blocks.get_mut(content_index)
                    {
                        *arguments = call.arguments.clone();
                    }
                    call.partial_json = None;
                    sync_content!();
                    stream.push(crate::types::AssistantMessageEvent::ToolcallEnd {
                        content_index,
                        tool_call: tool_call_content(call),
                        partial: output.clone(),
                    });
                    output_slots.remove(&output_index);
                } else if item_type == "custom_tool_call" {
                    let Some(OutputSlot::ToolCall {
                        content_index,
                        call,
                        ..
                    }) = get_slot(&mut output_slots, output_index, "toolCall")
                    else {
                        continue;
                    };
                    let content_index = *content_index;
                    let input = match item.get("input").and_then(Value::as_str) {
                        Some(input) => input.to_string(),
                        None => call
                            .arguments
                            .get(
                                call.custom_input
                                    .as_ref()
                                    .map(|(property, _)| property.as_str())
                                    .unwrap_or("input"),
                            )
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    };
                    if let Some((property, buffer)) = &mut call.custom_input {
                        let emitted =
                            append_grammar_tool_input_json_delta(buffer, property, &input, true)
                                .ok()
                                .flatten();
                        call.arguments = json!({ property.clone(): input });
                        sync_content!();
                        if let Some(emitted) = emitted {
                            stream.push(crate::types::AssistantMessageEvent::ToolcallDelta {
                                content_index,
                                delta: emitted,
                                partial: output.clone(),
                            });
                        }
                    }
                    call.custom_input = None;
                    if let Some(namespace) = item.get("namespace").and_then(Value::as_str) {
                        call.namespace = Some(namespace.to_string());
                    }
                    sync_content!();
                    stream.push(crate::types::AssistantMessageEvent::ToolcallEnd {
                        content_index,
                        tool_call: tool_call_content(&call.clone()),
                        partial: output.clone(),
                    });
                    output_slots.remove(&output_index);
                }
            }
            "response.completed" | "response.incomplete" => {
                saw_terminal_response_event = true;
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                finalize_response(
                    &response,
                    output,
                    &mut content_blocks,
                    &reasoning_blocks_by_id,
                    model,
                    options.as_mut(),
                );
                sync_content!();
            }
            "error" => {
                let code = event
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let message = event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown error");
                return Err(crate::error::AiError::Other(format!(
                    "Error Code {code}: {message}"
                )));
            }
            "response.failed" => {
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                output.raw_stop_reason = response
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let error = response.get("error");
                let details = response.get("incomplete_details");
                let message = match error.filter(|error| !error.is_null()) {
                    Some(error) => format!(
                        "{}: {}",
                        error
                            .get("code")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("no message")
                    ),
                    None => match details
                        .and_then(|details| details.get("reason"))
                        .and_then(Value::as_str)
                    {
                        Some(reason) => format!("incomplete: {reason}"),
                        None => "Unknown error (no error details in response)".to_string(),
                    },
                };
                return Err(crate::error::AiError::Other(message));
            }
            _ => {}
        }
    }

    if !saw_terminal_response_event {
        return Err(crate::error::AiError::Other(
            "OpenAI Responses stream ended before a terminal response event".to_string(),
        ));
    }
    Ok(())
}

fn tool_call_content(call: &StreamingToolCall) -> Content {
    Content::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
        thought_signature: None,
        namespace: call.namespace.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
fn create_slot(
    output_index: i64,
    item: &Value,
    output_slots: &mut HashMap<i64, OutputSlot>,
    content_blocks: &mut Vec<Content>,
    output: &AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    grammar_properties: &BTreeMap<String, String>,
) {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    let content_index = content_blocks.len();
    match item_type {
        "reasoning" => {
            content_blocks.push(Content::thinking(""));
            let slot = OutputSlot::Thinking {
                content_index,
                item_id: item.get("id").and_then(Value::as_str).map(str::to_string),
            };
            output_slots.insert(output_index, slot);
            stream.push(crate::types::AssistantMessageEvent::ThinkingStart {
                content_index,
                partial: output.clone(),
            });
        }
        "message" => {
            content_blocks.push(Content::text(""));
            let slot = OutputSlot::Text { content_index };
            output_slots.insert(output_index, slot);
            stream.push(crate::types::AssistantMessageEvent::TextStart {
                content_index,
                partial: output.clone(),
            });
        }
        "function_call" => {
            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let call = StreamingToolCall {
                id: format!("{call_id}|{item_id}"),
                name: name.to_string(),
                arguments: Value::Object(Map::new()),
                namespace: item
                    .get("namespace")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                partial_json: Some(
                    item.get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ),
                custom_input: None,
            };
            content_blocks.push(tool_call_content(&call));
            let slot = OutputSlot::ToolCall {
                content_index,
                call,
            };
            output_slots.insert(output_index, slot);
            stream.push(crate::types::AssistantMessageEvent::ToolcallStart {
                content_index,
                partial: output.clone(),
            });
        }
        "custom_tool_call" => {
            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let input_property = grammar_properties
                .get(name)
                .cloned()
                .unwrap_or_else(|| "input".to_string());
            let input = item.get("input").and_then(Value::as_str).unwrap_or("");
            let call = StreamingToolCall {
                id: format!("{call_id}|{item_id}"),
                name: name.to_string(),
                arguments: json!({ input_property.clone(): input }),
                namespace: item
                    .get("namespace")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                partial_json: None,
                custom_input: Some((
                    input_property,
                    crate::constrained_sampling::GrammarToolInputJsonBuffer::default(),
                )),
            };
            content_blocks.push(tool_call_content(&call));
            let slot = OutputSlot::ToolCall {
                content_index,
                call,
            };
            output_slots.insert(output_index, slot);
            stream.push(crate::types::AssistantMessageEvent::ToolcallStart {
                content_index,
                partial: output.clone(),
            });
        }
        _ => {}
    }
}

fn get_slot<'slots>(
    output_slots: &'slots mut HashMap<i64, OutputSlot>,
    output_index: i64,
    expected: &str,
) -> Option<&'slots mut OutputSlot> {
    let slot = output_slots.get_mut(&output_index)?;
    matches!(
        (expected, &*slot),
        ("thinking", OutputSlot::Thinking { .. })
            | ("text", OutputSlot::Text { .. })
            | ("toolCall", OutputSlot::ToolCall { .. })
    )
    .then_some(slot)
}

fn finalize_response(
    response: &Value,
    output: &mut AssistantMessage,
    content_blocks: &mut [Content],
    reasoning_blocks_by_id: &HashMap<String, usize>,
    model: &Model,
    options: Option<&mut ProcessResponsesStreamOptions<'_>>,
) {
    // Backfill reasoning signatures from the terminal response (Azure omits
    // encrypted_content from output_item.done).
    if let Some(response_output) = response.get("output").and_then(Value::as_array) {
        for item in response_output {
            if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                continue;
            }
            let Some(encrypted_content) = item.get("encrypted_content").and_then(Value::as_str)
            else {
                continue;
            };
            let Some(item_id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(content_index) = reasoning_blocks_by_id.get(item_id) else {
                continue;
            };
            let Some(Content::Thinking {
                thinking_signature, ..
            }) = content_blocks.get_mut(*content_index)
            else {
                continue;
            };
            let has_encrypted = thinking_signature
                .as_deref()
                .and_then(|signature| serde_json::from_str::<Value>(signature).ok())
                .and_then(|stored| stored.get("encrypted_content").cloned())
                .is_some_and(|value| !value.is_null());
            if has_encrypted {
                continue;
            }
            let mut stored = thinking_signature
                .as_deref()
                .and_then(|signature| serde_json::from_str::<Value>(signature).ok())
                .unwrap_or_else(|| item.clone());
            if let Some(stored_map) = stored.as_object_mut() {
                stored_map.insert(
                    "encrypted_content".to_string(),
                    Value::String(encrypted_content.to_string()),
                );
            }
            *thinking_signature = Some(stored.to_string());
        }
    }

    if let Some(id) = response.get("id").and_then(Value::as_str) {
        output.response_id = Some(id.to_string());
    }
    if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
        let input_details = usage.get("input_tokens_details");
        let cached_tokens = input_details
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_write_tokens = input_details
            .and_then(|details| details.get("cache_write_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let input_tokens = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output_tokens = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let reasoning = usage
            .get("output_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // OpenAI includes cached and cache-write tokens in input_tokens.
        output.usage = Usage {
            input: input_tokens.saturating_sub(cached_tokens + cache_write_tokens),
            output: output_tokens,
            cache_read: cached_tokens,
            cache_write: cache_write_tokens,
            cache_write_1h: None,
            reasoning: Some(reasoning).filter(|reasoning| *reasoning > 0),
            total_tokens: usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cost: Default::default(),
        };
    }
    calculate_cost(model, &mut output.usage);
    let service_tier = response.get("service_tier").and_then(Value::as_str);
    if let Some(options) = options {
        if let Some(apply) = &mut options.apply_service_tier_pricing {
            apply(&mut output.usage, service_tier);
        }
    }

    // Map status to stop reason.
    let status = response.get("status").and_then(Value::as_str);
    let incomplete_reason = response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str);
    output.raw_stop_reason = Some(match (status, incomplete_reason) {
        (Some(status), Some(reason)) => format!("{status}.{reason}"),
        (Some(status), None) => status.to_string(),
        (None, _) => String::new(),
    });
    let (stop_reason, error_message) = map_responses_stop_reason(status, incomplete_reason);
    output.stop_reason = stop_reason;
    output.error_message = error_message;
    if output
        .content
        .iter()
        .any(|block| matches!(block, Content::ToolCall { .. }))
        && output.stop_reason == StopReason::Stop
    {
        output.stop_reason = StopReason::ToolUse;
    }
}

/// Port of the shared `mapStopReason`.
pub fn map_responses_stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
) -> (StopReason, Option<String>) {
    let Some(status) = status else {
        return (StopReason::Stop, None);
    };
    match status {
        "completed" => (StopReason::Stop, None),
        "incomplete" => {
            if incomplete_reason == Some("max_output_tokens") {
                (StopReason::Length, None)
            } else {
                (
                    StopReason::Error,
                    Some(
                        incomplete_reason
                            .map(|reason| format!("Response incomplete: {reason}"))
                            .unwrap_or_else(|| {
                                "Response incomplete without a provider reason".to_string()
                            }),
                    ),
                )
            }
        }
        "failed" | "cancelled" => (StopReason::Error, None),
        // These two are wonky ...
        "in_progress" | "queued" => (StopReason::Stop, None),
        other => (
            StopReason::Error,
            Some(format!("Unhandled stop reason: {other}")),
        ),
    }
}
