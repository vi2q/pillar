//! Port of packages/ai/src/api/google-shared.ts (pi v0.84.3).
//!
//! Shared utilities for Google Generative AI and Google Vertex providers.
//!
//! divergence: upstream delegates HTTP to @google/genai SDK; the Rust port
//! builds raw requests via `FetchFn` and decodes SSE directly. SDK types
//! (Content / Part / FinishReason) are replaced by wire-format structs and
//! string enums defined here.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::AiError;
use crate::text::sanitize_surrogates;
use crate::transform_messages::transform_messages;
use crate::types::{
    AssistantMessage, Content, Context, Model, ModelThinkingLevel, StopReason, Tool,
};

// --- Wire-format types ----------------------------------------------------

/// Gemini API Content (upstream: @google/genai `Content`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub parts: Vec<GeminiPart>,
}

/// Gemini API Part (upstream: @google/genai `Part`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<GeminiBlob>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<GeminiFunctionResponse>,
}

/// Gemini API inline data blob (upstream: @google/genai `Blob`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiBlob {
    pub mime_type: String,
    pub data: String,
}

/// Gemini API function call (upstream: @google/genai `FunctionCall`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiFunctionCall {
    pub name: String,
    #[serde(default)]
    pub args: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Gemini API function response (upstream: @google/genai `FunctionResponse`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiFunctionResponse {
    pub name: String,
    /// `{"output": "..."}` on success, `{"error": "..."}` on failure.
    pub response: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<GeminiPart>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Gemini API streaming response chunk (subset of fields the port uses).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiStreamChunk {
    #[serde(default)]
    pub response_id: Option<String>,
    #[serde(default)]
    pub candidates: Option<Vec<GeminiCandidate>>,
    #[serde(default)]
    pub usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiCandidate {
    #[serde(default)]
    pub content: Option<GeminiContent>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiUsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u64,
    #[serde(default)]
    pub candidates_token_count: u64,
    #[serde(default)]
    pub total_token_count: u64,
    #[serde(default)]
    pub cached_content_token_count: u64,
    #[serde(default)]
    pub thoughts_token_count: u64,
}

/// Gemini FunctionCallingConfigMode (upstream: @google/genai enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FunctionCallingConfigMode {
    #[serde(rename = "AUTO")]
    Auto,
    #[serde(rename = "NONE")]
    None,
    #[serde(rename = "ANY")]
    Any,
    #[serde(rename = "VALIDATED")]
    Validated,
}

impl FunctionCallingConfigMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::None => "NONE",
            Self::Any => "ANY",
            Self::Validated => "VALIDATED",
        }
    }
}

/// Resolved Google thinking level after model mapping.
/// Upstream: `Exclude<ThinkingLevel, "xhigh" | "max">`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedGoogleThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}

// --- Thinking level resolution ---------------------------------------------

/// Upstream `resolveGoogleThinkingLevel`: resolve a pi level or model-specific
/// Google mapping to a standard Google level. Returns error on unsupported
/// mapping (upstream throws).
pub fn resolve_google_thinking_level(
    model: &Model,
    level: ModelThinkingLevel,
) -> Result<ResolvedGoogleThinkingLevel, AiError> {
    if level == ModelThinkingLevel::Off {
        return Ok(ResolvedGoogleThinkingLevel::High);
    }

    let mapped: Option<String> = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&level))
        .and_then(|v| v.clone());

    let resolved = match &mapped {
        Some(s) => s.to_lowercase(),
        None => match level {
            ModelThinkingLevel::Minimal => "minimal".to_string(),
            ModelThinkingLevel::Low => "low".to_string(),
            ModelThinkingLevel::Medium => "medium".to_string(),
            ModelThinkingLevel::High => "high".to_string(),
            _ => {
                return Err(AiError::Other(format!(
                    "Unsupported Google thinking level mapping for {}/{}: {} -> {}",
                    model.provider,
                    model.id,
                    model_thinking_level_str(level),
                    mapped.as_deref().unwrap_or("undefined"),
                )));
            }
        },
    };

    match resolved.as_str() {
        "minimal" => Ok(ResolvedGoogleThinkingLevel::Minimal),
        "low" => Ok(ResolvedGoogleThinkingLevel::Low),
        "medium" => Ok(ResolvedGoogleThinkingLevel::Medium),
        "high" => Ok(ResolvedGoogleThinkingLevel::High),
        _ => Err(AiError::Other(format!(
            "Unsupported Google thinking level mapping for {}/{}: {} -> {}",
            model.provider,
            model.id,
            model_thinking_level_str(level),
            mapped.as_deref().unwrap_or("undefined"),
        ))),
    }
}

fn model_thinking_level_str(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    }
}

// --- Thought signature helpers ----------------------------------------------

/// Upstream `isThinkingPart`: `thought === true` is the definitive marker.
pub fn is_thinking_part(part: &GeminiPart) -> bool {
    part.thought == Some(true)
}

/// Upstream `retainThoughtSignature`: preserve last non-empty signature for
/// the current block.
pub fn retain_thought_signature(existing: Option<&str>, incoming: Option<&str>) -> Option<String> {
    match incoming {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => existing.map(|s| s.to_string()),
    }
}

const BASE64_SIGNATURE_CHARS: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn is_valid_thought_signature(signature: &str) -> bool {
    if signature.is_empty() {
        return false;
    }
    if !signature.len().is_multiple_of(4) {
        return false;
    }
    // Check pattern: /^[A-Za-z0-9+/]+={0,2}$/
    let core = signature.trim_end_matches('=');
    let trailing_equals = signature.len() - core.len();
    if trailing_equals > 2 {
        return false;
    }
    core.bytes()
        .all(|b| BASE64_SIGNATURE_CHARS.contains(&b) || b == b'+' || b == b'/')
}

/// Only keep signatures from the same provider/model and with valid base64.
fn resolve_thought_signature(
    is_same_provider_and_model: bool,
    signature: Option<&str>,
) -> Option<String> {
    match signature {
        Some(sig) if is_same_provider_and_model && is_valid_thought_signature(sig) => {
            Some(sig.to_string())
        }
        _ => None,
    }
}

// --- Tool call ID -----------------------------------------------------------

/// Upstream `requiresToolCallId`: models that require explicit tool call IDs.
pub fn requires_tool_call_id(model_id: &str) -> bool {
    let gemini_major = get_gemini_major_version(model_id);
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || gemini_major.map(|v| v >= 3).unwrap_or(false)
}

/// `^gemini(?:-live)?-(\d+)` → major version.
fn get_gemini_major_version(model_id: &str) -> Option<u64> {
    let lower = model_id.to_lowercase();
    let rest = if let Some(r) = lower.strip_prefix("gemini-live-") {
        r
    } else {
        lower.strip_prefix("gemini-")?
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn supports_multimodal_function_response(model_id: &str) -> bool {
    match get_gemini_major_version(model_id) {
        Some(v) => v >= 3,
        None => true,
    }
}

// --- Message conversion ------------------------------------------------------

/// Upstream `convertMessages`: convert internal messages to Gemini `Content[]`.
pub fn convert_messages(model: &Model, context: &Context) -> Vec<GeminiContent> {
    let mut contents: Vec<GeminiContent> = Vec::new();

    let normalize_fn = |id: &str, _m: &Model, _s: &AssistantMessage| -> String {
        if !requires_tool_call_id(&model.id) {
            return id.to_string();
        }
        let replaced: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        replaced.chars().take(64).collect()
    };

    let transformed = transform_messages(context.messages.clone(), model, Some(&normalize_fn));

    for msg in &transformed {
        match msg {
            crate::types::Message::User { content, .. } => {
                let parts: Vec<GeminiPart> = match content {
                    crate::types::UserContent::Text(text) => {
                        vec![GeminiPart {
                            text: Some(sanitize_surrogates(text)),
                            ..Default::default()
                        }]
                    }
                    crate::types::UserContent::Blocks(blocks) => {
                        let mut parts = Vec::new();
                        for item in blocks {
                            match item {
                                Content::Text { text, .. } => {
                                    parts.push(GeminiPart {
                                        text: Some(sanitize_surrogates(text)),
                                        ..Default::default()
                                    });
                                }
                                Content::Image {
                                    data, mime_type, ..
                                } => {
                                    parts.push(GeminiPart {
                                        inline_data: Some(GeminiBlob {
                                            mime_type: mime_type.clone(),
                                            data: data.clone(),
                                        }),
                                        ..Default::default()
                                    });
                                }
                                _ => {}
                            }
                        }
                        parts
                    }
                };
                if parts.is_empty() {
                    continue;
                }
                contents.push(GeminiContent {
                    role: Some("user".to_string()),
                    parts,
                });
            }
            crate::types::Message::Assistant(assistant) => {
                let mut parts: Vec<GeminiPart> = Vec::new();
                let is_same = assistant.provider == model.provider && assistant.model == model.id;

                for block in &assistant.content {
                    match block {
                        Content::Text {
                            text,
                            text_signature,
                            ..
                        } => {
                            let thought_signature =
                                resolve_thought_signature(is_same, text_signature.as_deref());
                            // Skip empty text blocks unless they carry a signature.
                            if (text.is_empty() || text.trim().is_empty())
                                && thought_signature.is_none()
                            {
                                continue;
                            }
                            parts.push(GeminiPart {
                                text: Some(sanitize_surrogates(text)),
                                thought_signature,
                                ..Default::default()
                            });
                        }
                        Content::Thinking {
                            thinking,
                            thinking_signature,
                            ..
                        } => {
                            if is_same {
                                let thought_signature = resolve_thought_signature(
                                    is_same,
                                    thinking_signature.as_deref(),
                                );
                                if (thinking.is_empty() || thinking.trim().is_empty())
                                    && thought_signature.is_none()
                                {
                                    continue;
                                }
                                parts.push(GeminiPart {
                                    thought: Some(true),
                                    text: Some(sanitize_surrogates(thinking)),
                                    thought_signature,
                                    ..Default::default()
                                });
                            } else {
                                if thinking.is_empty() || thinking.trim().is_empty() {
                                    continue;
                                }
                                parts.push(GeminiPart {
                                    text: Some(sanitize_surrogates(thinking)),
                                    ..Default::default()
                                });
                            }
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            thought_signature,
                            ..
                        } => {
                            let thought_signature =
                                resolve_thought_signature(is_same, thought_signature.as_deref());
                            let function_call = GeminiFunctionCall {
                                name: name.clone(),
                                args: arguments.clone(),
                                id: if requires_tool_call_id(&model.id) {
                                    Some(id.clone())
                                } else {
                                    None
                                },
                            };
                            parts.push(GeminiPart {
                                function_call: Some(function_call),
                                thought_signature,
                                ..Default::default()
                            });
                        }
                        _ => {}
                    }
                }

                if parts.is_empty() {
                    continue;
                }
                contents.push(GeminiContent {
                    role: Some("model".to_string()),
                    parts,
                });
            }
            crate::types::Message::ToolResult(tool_result) => {
                let text_content: Vec<&str> = tool_result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text())
                    .collect();
                let text_result = text_content.join("\n");

                let image_content: Vec<&Content> = if model.input.iter().any(|i| i == "image") {
                    tool_result
                        .content
                        .iter()
                        .filter(|c| matches!(c, Content::Image { .. }))
                        .collect()
                } else {
                    Vec::new()
                };

                let has_text = !text_result.is_empty();
                let has_images = !image_content.is_empty();
                let model_supports_multimodal = supports_multimodal_function_response(&model.id);

                let response_value = if has_text {
                    sanitize_surrogates(&text_result)
                } else if has_images {
                    "(see attached image)".to_string()
                } else {
                    String::new()
                };

                let image_parts: Vec<GeminiPart> = image_content
                    .iter()
                    .filter_map(|block| {
                        if let Content::Image { data, mime_type } = block {
                            Some(GeminiPart {
                                inline_data: Some(GeminiBlob {
                                    mime_type: mime_type.clone(),
                                    data: data.clone(),
                                }),
                                ..Default::default()
                            })
                        } else {
                            None
                        }
                    })
                    .collect();

                let include_id = requires_tool_call_id(&model.id);
                let response_obj = if tool_result.is_error {
                    json!({ "error": response_value })
                } else {
                    json!({ "output": response_value })
                };

                let function_response_part = GeminiPart {
                    function_response: Some(GeminiFunctionResponse {
                        name: tool_result.tool_name.clone(),
                        response: response_obj,
                        parts: if has_images && model_supports_multimodal {
                            Some(image_parts.clone())
                        } else {
                            None
                        },
                        id: if include_id {
                            Some(tool_result.tool_call_id.clone())
                        } else {
                            None
                        },
                    }),
                    ..Default::default()
                };

                // Merge into previous user turn with functionResponse parts.
                let should_merge = contents
                    .last()
                    .map(|c| {
                        c.role.as_deref() == Some("user")
                            && c.parts.iter().any(|p| p.function_response.is_some())
                    })
                    .unwrap_or(false);

                if should_merge {
                    contents
                        .last_mut()
                        .unwrap()
                        .parts
                        .push(function_response_part);
                } else {
                    contents.push(GeminiContent {
                        role: Some("user".to_string()),
                        parts: vec![function_response_part],
                    });
                }

                // For Gemini < 3, add images in a separate user message.
                if has_images && !model_supports_multimodal {
                    let mut img_turn_parts = vec![GeminiPart {
                        text: Some("Tool result image:".to_string()),
                        ..Default::default()
                    }];
                    img_turn_parts.extend(image_parts);
                    contents.push(GeminiContent {
                        role: Some("user".to_string()),
                        parts: img_turn_parts,
                    });
                }
            }
        }
    }

    contents
}

// --- Tool conversion ----------------------------------------------------------

const JSON_SCHEMA_META_DECLARATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "$dynamicAnchor",
    "$vocabulary",
    "$comment",
    "$defs",
    "definitions",
];

/// Strip meta-declarations from a schema object recursively.
/// Arrays are returned as-is (upstream behavior).
fn sanitize_for_open_api(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut result = Map::new();
            for (key, value) in map {
                if JSON_SCHEMA_META_DECLARATIONS.contains(&key.as_str()) {
                    continue;
                }
                result.insert(key.clone(), sanitize_for_open_api(value));
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

/// Upstream `convertTools`: convert tools to Gemini function declarations.
/// `use_parameters`: use legacy `parameters` field (OpenAPI 3.03 Schema)
/// instead of `parametersJsonSchema`.
pub fn convert_tools(
    tools: &[Tool],
    use_parameters: bool,
    supports_strict_mode: bool,
) -> Result<Option<Value>, AiError> {
    if tools.is_empty() {
        return Ok(None);
    }
    let mut declarations: Vec<Value> = Vec::new();
    for tool in tools {
        let strict = crate::constrained_sampling::resolve_json_schema_strict_sampling(
            tool,
            supports_strict_mode,
        )?;
        let parameters = crate::constrained_sampling::get_json_schema_tool_parameters(tool, strict)
            .map_err(|e| AiError::Other(e.0))?;

        let mut decl = Map::new();
        decl.insert("name".to_string(), json!(tool.name));
        decl.insert("description".to_string(), json!(tool.description));
        if use_parameters {
            decl.insert("parameters".to_string(), sanitize_for_open_api(&parameters));
        } else {
            decl.insert("parametersJsonSchema".to_string(), parameters);
        }
        declarations.push(Value::Object(decl));
    }
    Ok(Some(json!({ "functionDeclarations": declarations })))
}

// --- Function calling mode ------------------------------------------------------

/// Gemini 3+ enforces required function parameters in validated tool-calling modes.
pub fn supports_google_strict_tool_sampling(model_id: &str) -> bool {
    get_gemini_major_version(model_id)
        .map(|v| v >= 3)
        .unwrap_or(false)
}

/// Map tool choice string to Gemini FunctionCallingConfigMode.
pub fn map_tool_choice(choice: &str) -> FunctionCallingConfigMode {
    match choice {
        "none" => FunctionCallingConfigMode::None,
        "any" => FunctionCallingConfigMode::Any,
        _ => FunctionCallingConfigMode::Auto,
    }
}

/// Upstream `resolveGoogleFunctionCallingMode`.
pub fn resolve_google_function_calling_mode(
    tools: &[Tool],
    tool_choice: Option<&str>,
    supports_strict_mode: bool,
) -> Result<Option<FunctionCallingConfigMode>, AiError> {
    let mut use_strict_mode = false;
    for tool in tools {
        let strict = crate::constrained_sampling::resolve_json_schema_strict_sampling(
            tool,
            supports_strict_mode,
        )?;
        if strict == Some(true) {
            use_strict_mode = true;
            break;
        }
    }

    if let Some(choice) = tool_choice
        && (choice == "none" || choice == "any")
    {
        return Ok(Some(map_tool_choice(choice)));
    }
    if use_strict_mode {
        return Ok(Some(FunctionCallingConfigMode::Validated));
    }
    Ok(tool_choice.map(map_tool_choice))
}

// --- Stop reason -----------------------------------------------------------------

/// Map string finish reason to StopReason (for raw API responses).
pub fn map_stop_reason_string(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::Error,
    }
}
