//! Port of packages/ai/src/api/anthropic-messages.ts (pi v0.84.3).
//!
//! divergence: upstream talks to the Anthropic SDK client; the Rust port
//! performs requests via `FetchFn` and decodes raw SSE itself.

use crate::api::impl_from_request_options;
use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::event_stream::assistant_message_event_stream;
use crate::types::{
    AssistantMessage, CacheRetention, Content, Context, Message, Model, StopReason, Tool,
    ToolResultMessage, Usage,
};

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

fn force_adaptive_thinking(model: &Model) -> bool {
    anthropic_compat_of(model).force_adaptive_thinking == Some(true)
}

// --- Stream entry --------------------------------------------------------

pub type AnthropicEffort = String;

pub const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
pub const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
pub const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
pub const CLAUDE_CODE_SYSTEM_PROMPT: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

/// Upstream `AnthropicOptions` (stream options for anthropic-messages).
#[derive(Default)]
pub struct AnthropicOptions {
    pub signal: Option<crate::AbortSignal>,
    pub api_key: Option<String>,
    pub fetch: Option<crate::transport::SharedFetchFn>,
    pub env: Option<crate::types::ProviderEnv>,
    pub on_payload: Option<crate::api::OnPayloadFn>,
    pub on_response: Option<crate::api::OnResponseFn>,
    pub headers: Option<crate::types::ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub tool_choice: Option<Value>,
    /// Enable extended thinking (adaptive or budget-based).
    pub thinking_enabled: Option<bool>,
    /// Token budget for extended thinking (older models only).
    pub thinking_budget_tokens: Option<u64>,
    /// Effort level for adaptive thinking models ("low"/"medium"/"high"/
    /// "xhigh"/"max").
    pub effort: Option<AnthropicEffort>,
    /// "summarized" | "omitted"; default "summarized".
    pub thinking_display: Option<String>,
    /// Request the interleaved thinking beta for non-adaptive models
    /// (default true).
    pub interleaved_thinking: Option<bool>,
}

impl_from_request_options!(AnthropicOptions);
impl_from_request_options!(AnthropicSimpleStreamOptions);

/// Upstream `SimpleStreamOptions` for anthropic-messages.
#[derive(Default)]
pub struct AnthropicSimpleStreamOptions {
    pub signal: Option<crate::AbortSignal>,
    pub api_key: Option<String>,
    pub fetch: Option<crate::transport::SharedFetchFn>,
    pub env: Option<crate::types::ProviderEnv>,
    pub on_payload: Option<crate::api::OnPayloadFn>,
    pub on_response: Option<crate::api::OnResponseFn>,
    pub headers: Option<crate::types::ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub tool_choice: Option<Value>,
    pub reasoning: Option<crate::types::ThinkingLevel>,
    pub thinking_budgets: Option<crate::types::ThinkingBudgets>,
}

/// Upstream `mapThinkingLevelToEffort`: adaptive thinking effort mapping.
pub fn map_thinking_level_to_effort(
    model: &Model,
    level: Option<crate::types::ThinkingLevel>,
) -> AnthropicEffort {
    let mapped = level.and_then(|level| {
        let key = match level {
            crate::types::ThinkingLevel::Minimal => crate::types::ModelThinkingLevel::Minimal,
            crate::types::ThinkingLevel::Low => crate::types::ModelThinkingLevel::Low,
            crate::types::ThinkingLevel::Medium => crate::types::ModelThinkingLevel::Medium,
            crate::types::ThinkingLevel::High => crate::types::ModelThinkingLevel::High,
            crate::types::ThinkingLevel::Xhigh => crate::types::ModelThinkingLevel::Xhigh,
            crate::types::ThinkingLevel::Max => crate::types::ModelThinkingLevel::Max,
        };
        model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&key))
            .and_then(|mapped| mapped.clone())
    });
    if let Some(mapped) = mapped {
        return mapped;
    }
    match level {
        Some(crate::types::ThinkingLevel::Minimal) | Some(crate::types::ThinkingLevel::Low) => {
            "low".to_string()
        }
        Some(crate::types::ThinkingLevel::Medium) => "medium".to_string(),
        _ => "high".to_string(),
    }
}

// --- Beta headers --------------------------------------------------------

pub fn should_use_server_side_fallback_beta(model: &Model) -> bool {
    anthropic_compat_of(model)
        .allowed_fallback_models
        .map(|fallbacks| !fallbacks.is_empty())
        .unwrap_or(false)
}

pub fn should_use_fine_grained_tool_streaming_beta(model: &Model, context: &Context) -> bool {
    !context.tools.is_empty() && !get_anthropic_compat(model).supports_eager_tool_input_streaming
}

pub struct ClientIdentity {
    pub is_oauth_token: bool,
    pub beta_features: Vec<String>,
    pub default_headers: Vec<(String, String)>,
}

/// Upstream `createClient`: resolve Bearer vs API-key auth, beta features,
/// and default headers. The Rust port performs requests via `FetchFn`, so
/// this returns identity data rather than an SDK client.
pub struct CreateClientIdentityOptions<'a> {
    pub api_key: Option<&'a str>,
    pub interleaved_thinking: bool,
    pub use_fine_grained_tool_streaming_beta: bool,
    pub use_server_side_fallback_beta: bool,
    pub options_headers: Option<&'a crate::types::ProviderHeaders>,
    pub dynamic_headers: Option<&'a [(String, String)]>,
    pub session_id: Option<&'a str>,
}

pub fn create_client_identity(
    model: &Model,
    options: CreateClientIdentityOptions<'_>,
) -> ClientIdentity {
    let CreateClientIdentityOptions {
        api_key,
        interleaved_thinking,
        use_fine_grained_tool_streaming_beta,
        use_server_side_fallback_beta,
        options_headers,
        dynamic_headers,
        session_id,
    } = options;
    // Adaptive thinking models have interleaved thinking built in, so skip
    // the beta header.
    let needs_interleaved_beta = interleaved_thinking && !force_adaptive_thinking(model);
    let mut beta_features: Vec<String> = Vec::new();
    if use_fine_grained_tool_streaming_beta {
        beta_features.push(FINE_GRAINED_TOOL_STREAMING_BETA.to_string());
    }
    if needs_interleaved_beta {
        beta_features.push(INTERLEAVED_THINKING_BETA.to_string());
    }
    if use_server_side_fallback_beta {
        beta_features.push(SERVER_SIDE_FALLBACK_BETA.to_string());
    }

    let mut base = vec![
        ("accept".to_string(), "application/json".to_string()),
        (
            "anthropic-dangerous-direct-browser-access".to_string(),
            "true".to_string(),
        ),
    ];
    if !beta_features.is_empty() {
        base.push(("anthropic-beta".to_string(), beta_features.join(",")));
    }

    let is_oauth = api_key.is_some_and(is_oauth_token);
    if model.provider == "github-copilot" {
        // Copilot: Bearer auth, selective betas.
        base.push((
            "authorization".to_string(),
            format!("Bearer {}", api_key.unwrap_or("")),
        ));
        let defaults = crate::api::merge_request_headers(
            {
                let mut with_ua = vec![("User-Agent".to_string(), crate::api::get_pi_user_agent())];
                with_ua.extend(base);
                with_ua
            },
            model.headers.as_ref(),
            options_headers,
        );
        let defaults = match dynamic_headers {
            Some(dynamic) => {
                let mut defaults = defaults;
                for (name, value) in dynamic {
                    if let Some(slot) = defaults.iter_mut().find(|(existing, _)| existing == name) {
                        slot.1 = value.clone();
                    } else {
                        defaults.push((name.clone(), value.clone()));
                    }
                }
                defaults
            }
            None => defaults,
        };
        return ClientIdentity {
            is_oauth_token: false,
            beta_features,
            default_headers: defaults,
        };
    }

    if is_oauth {
        // OAuth: Bearer auth, Claude Code identity headers.
        base.push((
            "anthropic-beta".to_string(),
            ["claude-code-20250219", "oauth-2025-04-20"]
                .into_iter()
                .map(str::to_string)
                .chain(beta_features.iter().cloned())
                .collect::<Vec<_>>()
                .join(","),
        ));
        base.push((
            "user-agent".to_string(),
            format!("claude-cli/{}", CLAUDE_CODE_VERSION),
        ));
        base.push(("x-app".to_string(), "cli".to_string()));
        base.push((
            "authorization".to_string(),
            format!("Bearer {}", api_key.unwrap_or("")),
        ));
        let defaults = crate::api::merge_request_headers(
            {
                let mut with_ua = vec![("User-Agent".to_string(), crate::api::get_pi_user_agent())];
                with_ua.extend(base);
                with_ua
            },
            model.headers.as_ref(),
            options_headers,
        );
        return ClientIdentity {
            is_oauth_token: true,
            beta_features,
            default_headers: defaults,
        };
    }

    // API key or header-owned auth.
    let session_affinity_headers: Vec<(String, String)> = match session_id {
        Some(session_id) if get_anthropic_compat(model).send_session_affinity_headers => {
            vec![("x-session-affinity".to_string(), session_id.to_string())]
        }
        _ => Vec::new(),
    };
    if let Some(api_key) = api_key {
        base.push(("x-api-key".to_string(), api_key.to_string()));
    }
    let defaults = crate::api::merge_request_headers(
        {
            let mut with_ua = vec![("User-Agent".to_string(), crate::api::get_pi_user_agent())];
            with_ua.extend(base);
            with_ua.extend(session_affinity_headers);
            with_ua
        },
        model.headers.as_ref(),
        options_headers,
    );
    ClientIdentity {
        is_oauth_token: false,
        beta_features,
        default_headers: defaults,
    }
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

// --- Params --------------------------------------------------------------

pub struct BuildParamsContext<'a> {
    pub model: &'a Model,
    pub context: &'a Context,
    pub is_oauth_token: bool,
    pub options: &'a AnthropicOptions,
}

/// Upstream `buildParams`: assemble the MessageCreateParams payload.
pub fn build_params(
    model: &Model,
    context: &Context,
    is_oauth_token: bool,
    options: &AnthropicOptions,
) -> Value {
    let (_retention, cache_control) =
        get_cache_control(model, options.cache_retention, options.env.as_ref());
    let compat = get_anthropic_compat(model);
    let transformed_messages = crate::transform_messages::transform_messages(
        context.messages.clone(),
        model,
        Some(&|id, _model, _source| normalize_tool_call_id(id)),
    );
    let normalize_tool_name = |name: &str| -> String {
        if is_oauth_token {
            to_claude_code_name(name)
        } else {
            name.to_string()
        }
    };
    let split = crate::deferred_tools::split_deferred_tools_with(
        context,
        compat.supports_tool_references,
        normalize_tool_name,
    );
    let mut immediate_tools = split.immediate;
    let mut deferred_tools: Vec<Tool> = split.deferred.into_values().collect();
    if immediate_tools.is_empty() && !deferred_tools.is_empty() {
        immediate_tools = std::mem::take(&mut deferred_tools);
    }
    let deferred_tool_names: BTreeSet<String> = deferred_tools
        .iter()
        .map(|tool| normalize_tool_name(&tool.name))
        .collect();
    let mut params = serde_json::Map::new();
    params.insert("model".to_string(), Value::String(model.id.clone()));
    params.insert(
        "messages".to_string(),
        Value::Array(convert_messages(
            &transformed_messages,
            ConvertMessagesOptions {
                is_oauth_token,
                cache_control: cache_control.clone(),
                allow_empty_signature: compat.allow_empty_signature,
                deferred_tool_names: &deferred_tool_names,
                normalize_tool_name: &normalize_tool_name,
            },
        )),
    );
    params.insert(
        "max_tokens".to_string(),
        Value::Number(options.max_tokens.unwrap_or(model.max_tokens).into()),
    );
    params.insert("stream".to_string(), Value::Bool(true));

    // For OAuth tokens, we MUST include Claude Code identity
    if is_oauth_token {
        let mut system = vec![json!({
            "type": "text",
            "text": CLAUDE_CODE_SYSTEM_PROMPT,
        })];
        if let Some(cache_control) = &cache_control {
            system[0]["cache_control"] = cache_control.clone();
        }
        if let Some(system_prompt) = &context.system_prompt {
            let mut block =
                json!({ "type": "text", "text": crate::text::sanitize_surrogates(system_prompt) });
            if let Some(cache_control) = &cache_control {
                block["cache_control"] = cache_control.clone();
            }
            system.push(block);
        }
        params.insert("system".to_string(), Value::Array(system));
    } else if let Some(system_prompt) = &context.system_prompt {
        // Add cache control to system prompt for non-OAuth tokens
        let mut block =
            json!({ "type": "text", "text": crate::text::sanitize_surrogates(system_prompt) });
        if let Some(cache_control) = &cache_control {
            block["cache_control"] = cache_control.clone();
        }
        params.insert("system".to_string(), Value::Array(vec![block]));
    }

    // Temperature is incompatible with extended thinking and unsupported on
    // Claude Opus 4.7+.
    if let Some(temperature) = options.temperature {
        if options.thinking_enabled != Some(true) && compat.supports_temperature {
            params.insert(
                "temperature".to_string(),
                serde_json::Number::from_f64(temperature)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            );
        }
    }

    if !immediate_tools.is_empty() || !deferred_tools.is_empty() {
        let tools_cache_control = if compat.supports_cache_control_on_tools {
            cache_control.as_ref()
        } else {
            None
        };
        let mut tools = convert_tools(
            &immediate_tools,
            is_oauth_token,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            tools_cache_control,
            false,
        );
        tools.extend(convert_tools(
            &deferred_tools,
            is_oauth_token,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            None,
            true,
        ));
        params.insert("tools".to_string(), Value::Array(tools));
    }

    // Configure thinking mode: adaptive, budget-based, or explicitly disabled.
    if model.reasoning {
        if options.thinking_enabled == Some(true) {
            let display = options
                .thinking_display
                .clone()
                .unwrap_or_else(|| "summarized".to_string());
            if force_adaptive_thinking(model) {
                // Adaptive thinking: Claude decides when and how much to think.
                params.insert(
                    "thinking".to_string(),
                    json!({ "type": "adaptive", "display": display }),
                );
                if let Some(effort) = &options.effort {
                    params.insert("output_config".to_string(), json!({ "effort": effort }));
                }
            } else {
                // Budget-based thinking for older models
                params.insert(
                    "thinking".to_string(),
                    json!({
                        "type": "enabled",
                        "budget_tokens": options.thinking_budget_tokens.unwrap_or(1024),
                        "display": display,
                    }),
                );
            }
        } else if options.thinking_enabled == Some(false)
            && model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&crate::types::ModelThinkingLevel::Off))
                != Some(&None)
        {
            params.insert("thinking".to_string(), json!({ "type": "disabled" }));
        }
    }

    if let Some(metadata) = &options.metadata {
        if let Some(user_id) = metadata.get("user_id").and_then(Value::as_str) {
            params.insert("metadata".to_string(), json!({ "user_id": user_id }));
        }
    }

    if let Some(tool_choice) = &options.tool_choice {
        if let Some(choice) = tool_choice.as_str() {
            params.insert("tool_choice".to_string(), json!({ "type": choice }));
        } else {
            params.insert("tool_choice".to_string(), tool_choice.clone());
        }
    }

    let allowed_fallback_models = anthropic_compat_of(model)
        .allowed_fallback_models
        .unwrap_or_default();
    if !allowed_fallback_models.is_empty() {
        params.insert(
            "fallbacks".to_string(),
            Value::Array(
                allowed_fallback_models
                    .iter()
                    .map(|fallback| json!({ "model": fallback.model }))
                    .collect(),
            ),
        );
    }

    Value::Object(params)
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

// --- Stream processing ---------------------------------------------------

/// Upstream `processAnthropicStream` block scratch state. `partial_json` is
/// only a streaming buffer; it never persists.
struct StreamBlock {
    index: i64,
    content_index: usize,
    partial_json: Option<String>,
}

struct StreamState {
    blocks: Vec<StreamBlock>,
}

impl StreamState {
    fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    fn find_by_index(&self, index: i64) -> Option<usize> {
        self.blocks.iter().position(|block| block.index == index)
    }
}

/// Port of upstream processAnthropicStream's event loop body: apply one
/// Anthropic stream event to `output` and emit events on `stream`.
struct ProcessEventContext<'a> {
    model: &'a Model,
    is_oauth: bool,
    context_tools: Option<&'a [Tool]>,
}

fn process_anthropic_event(
    event: &Value,
    output: &mut AssistantMessage,
    state: &mut StreamState,
    stream: &crate::event_stream::AssistantMessageEventStream,
    usage_model: &mut Model,
    ctx: &ProcessEventContext<'_>,
) -> Result<(), String> {
    let ProcessEventContext {
        model,
        is_oauth,
        context_tools,
    } = *ctx;
    let event_type = event["type"].as_str().unwrap_or("");
    match event_type {
        "message_start" => {
            let message = &event["message"];
            output.response_id = message["id"].as_str().map(str::to_string);
            output.model = message["model"].as_str().unwrap_or(&model.id).to_string();
            // Fallback cost for server-side fallback responses.
            let fallback_cost = if output.model == model.id {
                None
            } else {
                anthropic_compat_of(model)
                    .allowed_fallback_models
                    .unwrap_or_default()
                    .iter()
                    .find(|fallback| {
                        fallback.provider == model.provider && fallback.model == output.model
                    })
                    .and_then(|fallback| fallback.cost.clone())
            };
            *usage_model = match &fallback_cost {
                Some(cost) => {
                    let mut usage_model = model.clone();
                    usage_model.id = output.model.clone();
                    usage_model.cost = cost.clone();
                    usage_model
                }
                None => model.clone(),
            };
            // Capture initial token usage from message_start event
            let usage = &message["usage"];
            output.usage.input = usage["input_tokens"].as_u64().unwrap_or(0);
            output.usage.output = usage["output_tokens"].as_u64().unwrap_or(0);
            output.usage.cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            output.usage.cache_write = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            output.usage.cache_write_1h =
                usage["cache_creation"]["ephemeral_1h_input_tokens"].as_u64();
            // Anthropic doesn't provide total_tokens, compute from components
            output.usage.total_tokens = output.usage.input
                + output.usage.output
                + output.usage.cache_read
                + output.usage.cache_write;
            crate::models::calculate_cost(usage_model, &mut output.usage);
        }
        "content_block_start" => {
            let index = event["index"].as_i64().unwrap_or(0);
            let content_block = &event["content_block"];
            let block_type = content_block["type"].as_str().unwrap_or("");
            if block_type == "text" {
                let block = Content::Text {
                    text: content_block["text"].as_str().unwrap_or("").to_string(),
                    text_signature: None,
                };
                output.content.push(block);
                let content_index = output.content.len() - 1;
                state.blocks.push(StreamBlock {
                    index,
                    content_index,
                    partial_json: None,
                });
                stream.push(crate::types::AssistantMessageEvent::TextStart {
                    content_index,
                    partial: output.clone(),
                });
            } else if block_type == "thinking" {
                let block = Content::Thinking {
                    thinking: content_block["thinking"].as_str().unwrap_or("").to_string(),
                    thinking_signature: Some(
                        content_block["signature"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                    ),
                    redacted: None,
                };
                output.content.push(block);
                let content_index = output.content.len() - 1;
                state.blocks.push(StreamBlock {
                    index,
                    content_index,
                    partial_json: None,
                });
                stream.push(crate::types::AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: output.clone(),
                });
            } else if block_type == "redacted_thinking" {
                let block = Content::Thinking {
                    thinking: "[Reasoning redacted]".to_string(),
                    thinking_signature: content_block["data"].as_str().map(str::to_string),
                    redacted: Some(true),
                };
                output.content.push(block);
                let content_index = output.content.len() - 1;
                state.blocks.push(StreamBlock {
                    index,
                    content_index,
                    partial_json: None,
                });
                stream.push(crate::types::AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: output.clone(),
                });
            } else if block_type == "tool_use" {
                let name = content_block["name"].as_str().unwrap_or("");
                let display_name = if is_oauth {
                    from_claude_code_name(name, context_tools)
                } else {
                    name.to_string()
                };
                let block = Content::ToolCall {
                    id: content_block["id"].as_str().unwrap_or("").to_string(),
                    name: display_name,
                    arguments: if content_block["input"].is_null() {
                        json!({})
                    } else {
                        content_block["input"].clone()
                    },
                    thought_signature: None,
                    namespace: None,
                };
                output.content.push(block);
                let content_index = output.content.len() - 1;
                state.blocks.push(StreamBlock {
                    index,
                    content_index,
                    partial_json: Some(String::new()),
                });
                stream.push(crate::types::AssistantMessageEvent::ToolcallStart {
                    content_index,
                    partial: output.clone(),
                });
            }
        }
        "content_block_delta" => {
            let index = event["index"].as_i64().unwrap_or(0);
            let delta = &event["delta"];
            let delta_type = delta["type"].as_str().unwrap_or("");
            let Some(position) = state.find_by_index(index) else {
                return Ok(());
            };
            let content_index = state.blocks[position].content_index;
            if delta_type == "text_delta" {
                let Some(delta_text) = delta["text"].as_str() else {
                    return Ok(());
                };
                if let Some(Content::Text { text, .. }) = output.content.get_mut(content_index) {
                    text.push_str(delta_text);
                    stream.push(crate::types::AssistantMessageEvent::TextDelta {
                        content_index,
                        delta: delta_text.to_string(),
                        partial: output.clone(),
                    });
                }
            } else if delta_type == "thinking_delta" {
                let Some(delta_thinking) = delta["thinking"].as_str() else {
                    return Ok(());
                };
                if let Some(Content::Thinking { thinking, .. }) =
                    output.content.get_mut(content_index)
                {
                    thinking.push_str(delta_thinking);
                    stream.push(crate::types::AssistantMessageEvent::ThinkingDelta {
                        content_index,
                        delta: delta_thinking.to_string(),
                        partial: output.clone(),
                    });
                }
            } else if delta_type == "input_json_delta" {
                let Some(partial_json_delta) = delta["partial_json"].as_str() else {
                    return Ok(());
                };
                if let (Some(Content::ToolCall { arguments, .. }), Some(block)) = (
                    output.content.get_mut(content_index),
                    state.blocks.get_mut(position),
                ) {
                    let buffer = block.partial_json.get_or_insert_with(String::new);
                    buffer.push_str(partial_json_delta);
                    let parsed = crate::json_parse::parse_streaming_json(Some(buffer));
                    *arguments = parsed;
                    stream.push(crate::types::AssistantMessageEvent::ToolcallDelta {
                        content_index,
                        delta: partial_json_delta.to_string(),
                        partial: output.clone(),
                    });
                }
            } else if delta_type == "signature_delta" {
                let Some(signature) = delta["signature"].as_str() else {
                    return Ok(());
                };
                if let Some(Content::Thinking {
                    thinking_signature, ..
                }) = output.content.get_mut(content_index)
                {
                    let buffer = thinking_signature.get_or_insert_with(String::new);
                    buffer.push_str(signature);
                }
            }
        }
        "content_block_stop" => {
            let index = event["index"].as_i64().unwrap_or(0);
            let Some(position) = state.find_by_index(index) else {
                return Ok(());
            };
            let content_index = state.blocks[position].content_index;
            let is_tool_call = matches!(
                output.content.get(content_index),
                Some(Content::ToolCall { .. })
            );
            if is_tool_call {
                if let Some(block) = state.blocks.get_mut(position) {
                    if let Some(Content::ToolCall { arguments, .. }) =
                        output.content.get_mut(content_index)
                    {
                        // Finalize in-place and strip the scratch buffer.
                        let parsed =
                            crate::json_parse::parse_streaming_json(block.partial_json.as_deref());
                        *arguments = parsed;
                    }
                    block.partial_json = None;
                }
            }
            match output.content.get(content_index) {
                Some(Content::Text { text, .. }) => {
                    stream.push(crate::types::AssistantMessageEvent::TextEnd {
                        content_index,
                        content: text.clone(),
                        partial: output.clone(),
                    });
                }
                Some(Content::Thinking { thinking, .. }) => {
                    stream.push(crate::types::AssistantMessageEvent::ThinkingEnd {
                        content_index,
                        content: thinking.clone(),
                        partial: output.clone(),
                    });
                }
                Some(Content::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                }) => {
                    stream.push(crate::types::AssistantMessageEvent::ToolcallEnd {
                        content_index,
                        tool_call: Content::tool_call(id.clone(), name.clone(), arguments.clone()),
                        partial: output.clone(),
                    });
                }
                _ => {}
            }
            state.blocks.remove(position);
        }
        "message_delta" => {
            let delta = &event["delta"];
            if let Some(raw_stop_reason) = delta["stop_reason"].as_str() {
                output.raw_stop_reason = Some(raw_stop_reason.to_string());
                let (stop_reason, error_message) =
                    map_stop_reason(raw_stop_reason, delta.get("stop_details"))?;
                output.stop_reason = stop_reason;
                if error_message.is_some() {
                    output.error_message = error_message;
                }
            }
            // Only update usage fields if present (not null). Preserves
            // input_tokens from message_start when proxies omit it in
            // message_delta.
            let usage = &event["usage"];
            if !usage.is_null() {
                if let Some(value) = usage["input_tokens"].as_u64() {
                    output.usage.input = value;
                }
                if let Some(value) = usage["output_tokens"].as_u64() {
                    output.usage.output = value;
                }
                if let Some(value) = usage["cache_read_input_tokens"].as_u64() {
                    output.usage.cache_read = value;
                }
                if let Some(value) = usage["cache_creation_input_tokens"].as_u64() {
                    output.usage.cache_write = value;
                }
                // Anthropic reports reasoning tokens in
                // output_tokens_details.thinking_tokens on the final
                // message_delta usage (a subset of output_tokens).
                if let Some(thinking_tokens) =
                    usage["output_tokens_details"]["thinking_tokens"].as_u64()
                {
                    output.usage.reasoning = Some(thinking_tokens);
                }
            }
            // Anthropic doesn't provide total_tokens, compute from components
            output.usage.total_tokens = output.usage.input
                + output.usage.output
                + output.usage.cache_read
                + output.usage.cache_write;
            crate::models::calculate_cost(usage_model, &mut output.usage);
        }
        _ => {}
    }
    Ok(())
}

/// Port of the upstream `stream()` async body minus the SDK client: drives
/// decoded Anthropic events into `output`/`stream`.
pub fn process_anthropic_events(
    events: Vec<Value>,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    model: &Model,
    is_oauth: bool,
    context: &Context,
) -> Result<(), String> {
    let mut state = StreamState::new();
    let mut usage_model = model.clone();
    let ctx = ProcessEventContext {
        model,
        is_oauth,
        context_tools: Some(&context.tools),
    };
    for event in &events {
        process_anthropic_event(event, output, &mut state, stream, &mut usage_model, &ctx)?;
    }
    Ok(())
}

// --- Stream entry points -------------------------------------------------

/// Resolves the auth surface for a request: explicit API key, header-owned
/// auth, or OAuth. Upstream asserts before streaming (streamSimple) or in
/// the stream task (stream).
fn resolve_auth(
    model: &Model,
    api_key: Option<&str>,
    headers: Option<&crate::types::ProviderHeaders>,
) -> Result<(), String> {
    assert_request_auth(&model.provider, api_key, headers)
}

/// Upstream `stream()` — returns an event stream; the request runs on a
/// spawned task against the default or injected transport.
pub fn stream(
    model: Model,
    context: Context,
    options: Option<AnthropicOptions>,
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

fn fresh_output(model: &Model) -> AssistantMessage {
    AssistantMessage {
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
        timestamp: now_ms(),
    }
}

fn fail_stream(
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    error: String,
    aborted: bool,
) {
    output.stop_reason = if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    output.error_message = Some(error);
    stream.push(crate::types::AssistantMessageEvent::Error {
        reason: output.stop_reason,
        error: output.clone(),
    });
    stream.end(Some(output.clone()));
}

async fn run_stream(
    model: Model,
    context: Context,
    options: AnthropicOptions,
    stream: crate::event_stream::AssistantMessageEventStream,
) {
    let mut output = fresh_output(&model);

    let result = run_stream_inner(&model, &context, &options, &mut output, &stream).await;
    if let Err(error) = result {
        let aborted = options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted());
        fail_stream(&mut output, &stream, error, aborted);
    }
}

type RunResult = Result<(), String>;

async fn run_stream_inner(
    model: &Model,
    context: &Context,
    options: &AnthropicOptions,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
) -> RunResult {
    resolve_auth(model, options.api_key.as_deref(), options.headers.as_ref())?;

    let is_oauth = options.api_key.as_deref().is_some_and(is_oauth_token);
    let has_images =
        crate::api::github_copilot_headers::has_copilot_vision_input(&context.messages);
    let dynamic_headers = (model.provider == "github-copilot").then(|| {
        crate::api::github_copilot_headers::build_copilot_dynamic_headers(
            &context.messages,
            has_images,
        )
    });
    let identity = create_client_identity(
        model,
        CreateClientIdentityOptions {
            api_key: options.api_key.as_deref(),
            interleaved_thinking: options.interleaved_thinking.unwrap_or(true),
            use_fine_grained_tool_streaming_beta: should_use_fine_grained_tool_streaming_beta(
                model, context,
            ),
            use_server_side_fallback_beta: should_use_server_side_fallback_beta(model),
            options_headers: options.headers.as_ref(),
            dynamic_headers: dynamic_headers.as_deref(),
            session_id: options.session_id.as_deref(),
        },
    );

    let mut params = build_params(model, context, is_oauth, options);
    if let Some(on_payload) = &options.on_payload {
        if let Some(next_params) = on_payload(model, params.clone()).await {
            params = next_params;
        }
    }

    let fetch = options.fetch.clone().unwrap_or_else(default_fetch);
    let request = crate::transport::FetchRequest {
        method: "POST".to_string(),
        url: format!("{}/v1/messages", model.base_url.trim_end_matches('/')),
        headers: {
            let mut headers = identity.default_headers;
            if !headers.iter().any(|(name, _)| name == "Content-Type") {
                headers.push(("Content-Type".to_string(), "application/json".to_string()));
            }
            headers
        },
        body: Some(
            serde_json::to_vec(&params)
                .map_err(|error| format!("failed to serialize request body: {error}"))?,
        ),
    };

    let fetch_for_retry = std::sync::Arc::clone(&fetch);
    let request_for_retry = request;
    let signal = options.signal.clone();
    let timeout_ms = options.timeout_ms;
    let response = crate::provider_retry::retry_provider_request(
        || {
            let fetch = std::sync::Arc::clone(&fetch_for_retry);
            let request = request_for_retry.clone();
            let signal = signal.clone();
            async move {
                crate::api::fetch_json_stream(&fetch, request, signal.as_ref(), timeout_ms).await
            }
        },
        crate::provider_retry::ProviderRetryOptions {
            max_retries: options.max_retries,
            max_retry_delay_ms: options.max_retry_delay_ms,
            signal: options.signal.clone(),
        },
    )
    .await
    .map_err(|error| crate::api::format_stream_error(&error))?;

    if let Some(on_response) = &options.on_response {
        on_response(
            crate::api::ProviderResponseInfo {
                status: response.status,
                headers: response.headers.clone(),
            },
            model,
        )
        .await;
    }

    stream.push(crate::types::AssistantMessageEvent::Start {
        partial: output.clone(),
    });

    let sse_events = decode_fetch_response(response, options.signal.as_ref()).await?;
    let events = iterate_anthropic_events(&sse_events)?;

    let mut state = StreamState::new();
    let mut usage_model = model.clone();
    let ctx = ProcessEventContext {
        model,
        is_oauth,
        context_tools: Some(&context.tools),
    };
    for event in &events {
        process_anthropic_event(event, output, &mut state, stream, &mut usage_model, &ctx)?;
    }

    if options
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err("Request was aborted".to_string());
    }
    if output.stop_reason == StopReason::Pending {
        return Err("Anthropic stream ended without a stop reason".to_string());
    }
    if output.stop_reason == StopReason::Aborted || output.stop_reason == StopReason::Error {
        return Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string()));
    }

    stream.push(crate::types::AssistantMessageEvent::Done {
        reason: output.stop_reason,
        message: output.clone(),
    });
    stream.end(Some(output.clone()));
    Ok(())
}

/// Reads the fetch response body and decodes it into raw SSE events,
/// surfacing an `error` SSE event's data as the failure (upstream
/// `iterateSseMessages` + `iterateAnthropicEvents` streaming behavior).
async fn decode_fetch_response(
    response: crate::transport::FetchResponse,
    signal: Option<&crate::AbortSignal>,
) -> Result<Vec<ServerSentEvent>, String> {
    use futures::StreamExt;

    let mut state = SseDecoderState::default();
    let mut buffer = String::new();
    let mut events: Vec<ServerSentEvent> = Vec::new();
    let byte_stream = response.body;
    tokio::pin!(byte_stream);

    loop {
        if signal.is_some_and(|signal| signal.is_aborted()) {
            return Err("Request was aborted".to_string());
        }
        let chunk = match byte_stream.next().await {
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => return Err(error.to_string()),
            None => break,
        };
        let text = String::from_utf8_lossy(&chunk);
        events.extend(decode_sse_chunk(&text, &mut state, &mut buffer));
    }
    events.extend(finish_sse_body(&mut state, &mut buffer));

    // An `error` SSE event fails the stream with its data as the message.
    if let Some(error_event) = events
        .iter()
        .find(|event| event.event.as_deref() == Some("error"))
    {
        return Err(error_event.data.clone());
    }
    Ok(events)
}

/// Upstream `streamSimple()`: maps reasoning levels onto the full options
/// shape, then delegates to [`stream`].
pub fn stream_simple(
    model: Model,
    context: Context,
    options: Option<AnthropicSimpleStreamOptions>,
) -> crate::event_stream::AssistantMessageEventStream {
    let options = options.unwrap_or_default();
    if let Err(error) = resolve_auth(&model, options.api_key.as_deref(), options.headers.as_ref()) {
        let stream = assistant_message_event_stream();
        let mut message = fresh_output(&model);
        message.stop_reason = StopReason::Error;
        message.error_message = Some(error);
        stream.push(crate::types::AssistantMessageEvent::Error {
            reason: StopReason::Error,
            error: message.clone(),
        });
        stream.end(Some(message));
        return stream;
    }

    let base_max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    let base_max_tokens = crate::simple_options::clamp_max_tokens_to_context(
        model.context_window,
        &context,
        base_max_tokens,
    );
    let mut anthropic_options = AnthropicOptions {
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
        max_tokens: Some(base_max_tokens),
        cache_retention: options.cache_retention,
        session_id: options.session_id,
        metadata: options.metadata,
        tool_choice: options.tool_choice,
        ..Default::default()
    };

    match options.reasoning {
        None => {
            anthropic_options.thinking_enabled = Some(false);
        }
        Some(reasoning) => {
            let clamped =
                crate::models::clamp_thinking_level(&model, to_model_thinking_level(reasoning));
            if clamped == crate::types::ModelThinkingLevel::Off {
                anthropic_options.thinking_enabled = Some(false);
            } else if force_adaptive_thinking(&model) {
                anthropic_options.thinking_enabled = Some(true);
                anthropic_options.effort =
                    Some(map_thinking_level_to_effort(&model, options.reasoning));
            } else {
                // Undefined means the caller did not request an output cap;
                // let the helper use the model cap.
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
                anthropic_options.max_tokens = Some(max_tokens);
                anthropic_options.thinking_enabled = Some(true);
                anthropic_options.thinking_budget_tokens = Some(std::cmp::min(
                    thinking_budget,
                    max_tokens.saturating_sub(crate::simple_options::MIN_ANSWER_TOKENS),
                ));
            }
        }
    }

    stream(model, context, Some(anthropic_options))
}

fn to_model_thinking_level(level: crate::types::ThinkingLevel) -> crate::types::ModelThinkingLevel {
    match level {
        crate::types::ThinkingLevel::Minimal => crate::types::ModelThinkingLevel::Minimal,
        crate::types::ThinkingLevel::Low => crate::types::ModelThinkingLevel::Low,
        crate::types::ThinkingLevel::Medium => crate::types::ModelThinkingLevel::Medium,
        crate::types::ThinkingLevel::High => crate::types::ModelThinkingLevel::High,
        crate::types::ThinkingLevel::Xhigh => crate::types::ModelThinkingLevel::Xhigh,
        crate::types::ThinkingLevel::Max => crate::types::ModelThinkingLevel::Max,
    }
}
