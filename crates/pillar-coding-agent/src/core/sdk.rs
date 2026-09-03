//! Port of packages/coding-agent/src/core/sdk.ts (pi v0.84.3), the
//! session-assembly decision core: initial model/thinking-level/tool
//! selection from settings and session state, and the
//! convertToLlm block-images filter.
//!
//! divergences: model lookup (ModelRuntime) and the streaming fn wiring
//! are host-injected; the port covers the pure resolution logic over a
//! host-provided model lookup.

use pillar_ai::models::clamp_thinking_level;
use pillar_ai::types::{Message, Model};

use crate::core::session_support::{DEFAULT_THINKING_LEVEL, THINKING_LEVEL_OPTIONS};
use crate::core::settings_manager::SettingsManager;

// ============================================================================
// Model resolution (upstream createAgentSession model restoration)
// ============================================================================}

/// The resolved initial model + fallback message (upstream the model
/// restoration block of createAgentSession).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedInitialModel {
    pub model: Option<Model>,
    pub model_fallback_message: Option<String>,
}

/// Resolve the initial model (upstream the createAgentSession model
/// block): a session with saved model data tries
/// `provider/modelId` through the host lookup with configured auth; the
/// fallback message records what could not be restored, then records the
/// actually selected model.
pub fn resolve_initial_model(
    existing_session_model: Option<(&str, &str)>,
    explicit_model: Option<Model>,
    has_configured_auth: impl Fn(&str) -> bool,
    lookup: impl Fn(&str, &str) -> Option<Model>,
) -> ResolvedInitialModel {
    let mut fallback: Option<String> = None;
    if let Some(model) = explicit_model {
        return ResolvedInitialModel {
            model: Some(model),
            model_fallback_message: None,
        };
    }
    if let Some((provider, model_id)) = existing_session_model {
        let restored = lookup(provider, model_id).filter(|m| has_configured_auth(&m.provider));
        if restored.is_none() {
            fallback = Some(format!("Could not restore model {provider}/{model_id}"));
        } else {
            return ResolvedInitialModel {
                model: restored,
                model_fallback_message: None,
            };
        }
    }
    // findInitialModel (settings default / provider defaults) is
    // host-driven; the caller supplies the outcome via a second call if
    // needed. Here we surface the fallback message shape.
    ResolvedInitialModel {
        model: None,
        model_fallback_message: fallback,
    }
}

/// Compose the fallback message with the eventually selected model
/// (upstream: "... . Using provider/id").
pub fn fallback_message_using(fallback: &str, model: &Model) -> String {
    format!("{fallback}. Using {}/{}", model.provider, model.id)
}

// ============================================================================
// Thinking level resolution (upstream createAgentSession thinking block)
// ============================================================================}

/// Inputs for thinking-level resolution (upstream the settings lookups).
#[derive(Debug, Clone, Default)]
pub struct ThinkingLevelInputs<'a> {
    pub explicit_level: Option<String>,
    pub has_existing_session: bool,
    pub has_thinking_entry: bool,
    pub session_thinking_level: Option<String>,
    pub per_model_override: Option<&'a str>,
    pub default_level: Option<String>,
}

/// Parse a thinking-level string (upstream the ThinkingLevel union).
fn parse_thinking_level(level: &str) -> Option<pillar_ai::types::ModelThinkingLevel> {
    use pillar_ai::types::ModelThinkingLevel;
    let level = level.to_lowercase();
    match level.as_str() {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    }
}

/// Format a thinking level back to its pi string.
fn thinking_level_to_string(level: pillar_ai::types::ModelThinkingLevel) -> String {
    use pillar_ai::types::ModelThinkingLevel;
    match level {
        ModelThinkingLevel::Off => "off".to_string(),
        ModelThinkingLevel::Minimal => "minimal".to_string(),
        ModelThinkingLevel::Low => "low".to_string(),
        ModelThinkingLevel::Medium => "medium".to_string(),
        ModelThinkingLevel::High => "high".to_string(),
        ModelThinkingLevel::Xhigh => "xhigh".to_string(),
        ModelThinkingLevel::Max => "max".to_string(),
    }
}

/// Resolve the initial thinking level (upstream the thinking-level chain):
/// explicit → session entry (or settings default when the session has no
/// entry) → per-model override → global default → DEFAULT_THINKING_LEVEL;
/// no model clamps to "off", otherwise the level is clamped to the model's
/// capabilities.
pub fn resolve_thinking_level(inputs: ThinkingLevelInputs<'_>, model: Option<&Model>) -> String {
    let mut level = if inputs.has_existing_session {
        if inputs.has_thinking_entry {
            inputs.session_thinking_level.clone()
        } else {
            inputs.default_level.clone()
        }
    } else {
        None
    };
    if level.is_none() {
        if let Some(per_model) = inputs.per_model_override {
            if model.is_some() {
                level = Some(per_model.to_string());
            }
        }
    }
    if level.is_none() {
        level = Some(
            inputs
                .default_level
                .clone()
                .unwrap_or_else(|| DEFAULT_THINKING_LEVEL.to_string()),
        );
    }
    let level = level.unwrap_or_else(|| DEFAULT_THINKING_LEVEL.to_string());
    match model {
        None => "off".to_string(),
        // Unknown levels keep the default; THINKING_LEVEL_OPTIONS defines
        // the valid set (upstream ThinkingLevel union).
        Some(model) => {
            if !THINKING_LEVEL_OPTIONS.contains(&level.as_str()) {
                return DEFAULT_THINKING_LEVEL.to_string();
            }
            let parsed = parse_thinking_level(&level).expect("validated level");
            thinking_level_to_string(clamp_thinking_level(model, parsed))
        }
    }
}

// ============================================================================
// Tool selection (upstream createAgentSession tool block)
// ============================================================================}

/// The default built-in tool names (upstream `defaultActiveToolNames`).
pub const DEFAULT_ACTIVE_TOOL_NAMES: [&str; 4] = ["read", "bash", "edit", "write"];

/// Tool-selection inputs (upstream `tools` / `noTools` /
/// `excludeTools` / the `defaultTools` setting).
#[derive(Debug, Clone, Default)]
pub struct ToolSelectionInputs<'a> {
    pub tools: Option<&'a [String]>,
    pub no_tools: Option<&'a str>,
    pub exclude_tools: &'a [String],
    pub configured_default_tools: Option<&'a [String]>,
}

/// Resolve initial active tool names (upstream `initialActiveToolNames`):
/// an explicit allowlist wins; `noTools` starts with none; otherwise the
/// configured defaultTools setting or the built-in defaults apply; the
/// exclude list filters afterward.
pub fn resolve_initial_active_tool_names(inputs: ToolSelectionInputs<'_>) -> Vec<String> {
    let base: Vec<String> = if let Some(tools) = inputs.tools {
        tools.to_vec()
    } else if let Some(mode) = inputs.no_tools {
        match mode {
            "all" | "builtin" => Vec::new(),
            _ => inputs
                .configured_default_tools
                .map(|t| t.to_vec())
                .unwrap_or_else(|| {
                    DEFAULT_ACTIVE_TOOL_NAMES
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                }),
        }
    } else {
        inputs
            .configured_default_tools
            .map(|t| t.to_vec())
            .unwrap_or_else(|| {
                DEFAULT_ACTIVE_TOOL_NAMES
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            })
    };
    base.into_iter()
        .filter(|name| !inputs.exclude_tools.contains(name))
        .collect()
}

// ============================================================================
// convertToLlm block-images filter (upstream convertToLlmWithBlockImages)
// ============================================================================}

/// Filter images out of converted LLM messages when blockImages is set
/// (upstream `convertToLlmWithBlockImages`): image content is replaced by
/// a text placeholder, with consecutive placeholders deduped. Messages
/// with mixed content (user Blocks / tool result arrays) have image items
/// replaced in place.
pub fn filter_blocked_images(messages: Vec<Message>, block_images: bool) -> Vec<Message> {
    if !block_images {
        return messages;
    }
    const PLACEHOLDER: &str = "Image reading is disabled.";
    messages
        .into_iter()
        .map(|message| match message {
            Message::User { content, timestamp } => {
                let content = match content {
                    pillar_ai::types::UserContent::Blocks(items) => {
                        let mut replaced: Vec<pillar_ai::types::Content> = Vec::new();
                        for item in items {
                            match item {
                                pillar_ai::types::Content::Image { .. } => {
                                    let is_duplicate = replaced.last().is_some_and(|previous| {
                                        matches!(previous, pillar_ai::types::Content::Text {
                                            text,
                                            ..
                                        } if text == PLACEHOLDER)
                                    });
                                    if !is_duplicate {
                                        replaced.push(pillar_ai::types::Content::text(PLACEHOLDER));
                                    }
                                }
                                other => replaced.push(other),
                            }
                        }
                        pillar_ai::types::UserContent::Blocks(replaced)
                    }
                    other => other,
                };
                Message::User { content, timestamp }
            }
            Message::ToolResult(mut tool_result) => {
                let mut replaced: Vec<pillar_ai::types::Content> = Vec::new();
                for item in std::mem::take(&mut tool_result.content) {
                    match item {
                        pillar_ai::types::Content::Image { .. } => {
                            let is_duplicate = replaced.last().is_some_and(|previous| {
                                matches!(previous, pillar_ai::types::Content::Text {
                                    text,
                                    ..
                                } if text == PLACEHOLDER)
                            });
                            if !is_duplicate {
                                replaced.push(pillar_ai::types::Content::text(PLACEHOLDER));
                            }
                        }
                        other => replaced.push(other),
                    }
                }
                tool_result.content = replaced;
                Message::ToolResult(tool_result)
            }
            other => other,
        })
        .collect()
}

// ============================================================================
// Settings accessors used by the SDK (upstream getters)
// ============================================================================}

/// The settings lookups needed for session assembly, taken from a
/// SettingsManager (upstream getters: getDefaultTools /
/// getDefaultThinkingLevel / getModelThinkingLevel / getBlockImages).
pub struct SdkSettings {
    pub default_tools: Option<Vec<String>>,
    pub default_thinking_level: Option<String>,
    pub block_images: bool,
}

/// Extract the SDK-relevant settings snapshot.
pub fn sdk_settings(settings: &SettingsManager) -> SdkSettings {
    let default_tools: Option<Vec<String>> = settings
        .get_global_setting("defaultTools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
    let default_thinking_level = settings
        .get_global_setting("defaultThinkingLevel")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let block_images = settings
        .get_global_setting("images")
        .and_then(|v| v.get("blockImages"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    SdkSettings {
        default_tools,
        default_thinking_level,
        block_images,
    }
}
