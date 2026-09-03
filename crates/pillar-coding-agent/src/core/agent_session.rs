//! Port of packages/coding-agent/src/core/agent-session.ts (pi v0.84.3),
//! the session-state core: skill block parsing, steering/follow-up queue
//! tracking with queue_update events, retry policy state machine,
//! custom-message handling, tool registry with prompt rebuild, and
//! session stats.
//!
//! divergences: model switching/cycling needs the (unported)
//! ModelRuntime and is host-injected via `ModelMutations`; the streaming
//! agent-run loop and compaction orchestration live with the host; the
//! LLM-facing queueing calls into the pillar-agent Agent directly.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use pillar_ai::types::AssistantMessage;

use crate::core::messages::{CodingAgentMessage, CustomMessage};
use crate::core::prompt_templates::{PromptTemplate, expand_prompt_template};
use crate::core::system_prompt::{BuildSystemPromptOptions, Skill, build_system_prompt};

// ============================================================================
// Skill Block Parsing
// ============================================================================

/// Parsed skill block from a user message (upstream `ParsedSkillBlock`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSkillBlock {
    pub name: String,
    pub location: String,
    pub content: String,
    pub user_message: Option<String>,
}

/// Parse a skill block from message text (upstream `parseSkillBlock`):
/// `<skill name="..." location="...">\n...\n</skill>` with an optional
/// trailing user message.
pub fn parse_skill_block(text: &str) -> Option<ParsedSkillBlock> {
    let trimmed = text.trim_start();
    // Match: ^<skill name="([^"]+)" location="([^"]+)">\n([\s\S]*?)\n<\/skill>(?:\n\n([\s\S]+))?$
    let after = trimmed.strip_prefix("<skill name=\"")?;
    let name_end = after.find("\"")?;
    let name = &after[..name_end];
    let after_name = &after[name_end + 1..];
    let after_loc_prefix = after_name.strip_prefix(" location=\"")?;
    let loc_end = after_loc_prefix.find("\"")?;
    let location = &after_loc_prefix[..loc_end];
    let after_loc = &after_loc_prefix[loc_end + 1..];
    let body_start = after_loc.strip_prefix(">\n")?;
    let close = body_start.find("\n</skill>")?;
    let content = &body_start[..close];
    let tail = &body_start[close + "\n</skill>".len()..];
    let user_message = tail
        .strip_prefix("\n\n")
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    Some(ParsedSkillBlock {
        name: name.to_string(),
        location: location.to_string(),
        content: content.to_string(),
        user_message,
    })
}

// ============================================================================
// Queue tracking
// ============================================================================

/// A queued user message event (upstream `queue_update`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueueState {
    pub steering: Vec<String>,
    pub follow_up: Vec<String>,
}

// ============================================================================
// Retry state machine
// ============================================================================

/// Retry decision (upstream `_prepareRetry` result + emitted events).
#[derive(Debug, Clone, PartialEq)]
pub enum RetryStep {
    /// Not retryable: settings disabled or attempt budget exhausted.
    Continue,
    /// Retry scheduled after the given delay; the last assistant message
    /// must be removed from agent state.
    Wait {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
    /// Aborted while waiting.
    Cancelled,
}

/// Compute whether a retry is warranted after agent_end (upstream
/// `_willRetryAfterAgentEnd`): the last assistant message must be a
/// retryable error; context overflow is handled by compaction instead.
pub fn will_retry_after_agent_end(
    messages: &[CodingAgentMessage],
    retry_attempt: u32,
    retry_enabled: bool,
    max_retries: u32,
    context_window: u64,
) -> bool {
    if !retry_enabled || retry_attempt >= max_retries {
        return false;
    }
    for message in messages.iter().rev() {
        if let CodingAgentMessage::Base(pillar_ai::types::Message::Assistant(assistant)) = message {
            return is_retryable_error(assistant, context_window);
        }
    }
    false
}

/// Check if an error is retryable (upstream `_isRetryableError`): context
/// overflow is NOT retryable (compaction handles it).
pub fn is_retryable_error(message: &AssistantMessage, context_window: u64) -> bool {
    if pillar_ai::is_context_overflow(message, Some(context_window)) {
        return false;
    }
    pillar_ai::is_retryable_assistant_error(message)
}

/// Next retry step (upstream `_prepareRetry`, without sleeping): returns
/// the backoff delay computed from attempt count, or Continue/Cancelled.
/// The caller performs the wait and removes the error assistant message
/// from agent state on `Wait`.
pub fn prepare_retry(
    retry_attempt: u32,
    retry_enabled: bool,
    max_retries: u32,
    base_delay_ms: u64,
) -> RetryStep {
    if !retry_enabled {
        return RetryStep::Continue;
    }
    let attempt = retry_attempt + 1;
    if attempt > max_retries {
        return RetryStep::Continue;
    }
    RetryStep::Wait {
        attempt,
        max_attempts: max_retries,
        delay_ms: base_delay_ms * 2u64.pow(attempt - 1),
    }
}

// ============================================================================
// Custom message handling
// ============================================================================

/// Where a custom message should be delivered (upstream `deliverAs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomDelivery {
    Steer,
    FollowUp,
    NextTurn,
}

/// The decision for an incoming custom message (upstream
/// `sendCustomMessage` case analysis).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomMessagePlan {
    /// Queue for the next user prompt (deliverAs "nextTurn").
    PendingNextTurn,
    /// Steer into the running agent.
    Steer,
    /// Follow up after the running agent finishes.
    FollowUp,
    /// Trigger a new turn now (not streaming + triggerTurn).
    RunPrompt,
    /// Defer until the current turn's tool results land.
    PendingTurnEnd,
    /// Append immediately to state + session.
    AppendNow,
}

/// Decide how to handle a custom message (upstream `sendCustomMessage`).
pub fn plan_custom_message(
    is_streaming: bool,
    trigger_turn: Option<bool>,
    deliver_as: Option<CustomDelivery>,
) -> CustomMessagePlan {
    if deliver_as == Some(CustomDelivery::NextTurn) {
        return CustomMessagePlan::PendingNextTurn;
    }
    if is_streaming && trigger_turn != Some(false) {
        return match deliver_as {
            Some(CustomDelivery::FollowUp) => CustomMessagePlan::FollowUp,
            _ => CustomMessagePlan::Steer,
        };
    }
    if trigger_turn == Some(true) {
        return CustomMessagePlan::RunPrompt;
    }
    if is_streaming {
        return CustomMessagePlan::PendingTurnEnd;
    }
    CustomMessagePlan::AppendNow
}

/// Normalize a custom message at ingestion (upstream content `?? []`):
/// missing content becomes an empty list.
pub fn normalize_custom_message(
    custom_type: &str,
    content: Vec<crate::core::messages::CustomContent>,
    display: bool,
    details: Option<serde_json::Value>,
    timestamp: u64,
) -> CustomMessage {
    CustomMessage {
        custom_type: custom_type.to_string(),
        content,
        display,
        details,
        timestamp,
    }
}

// ============================================================================
// Tool registry + system prompt rebuild
// ============================================================================

/// A registered tool entry (upstream `ToolDefinitionEntry` +
/// prompt snippet/guideline maps).
#[derive(Debug, Clone, Default)]
pub struct ToolRegistryEntry {
    pub name: String,
    pub description: String,
    pub prompt_snippet: Option<String>,
    pub prompt_guidelines: Vec<String>,
}

/// Normalize a tool snippet to one line (upstream
/// `_normalizePromptSnippet`).
pub fn normalize_prompt_snippet(text: Option<&str>) -> Option<String> {
    let text = text?;
    let mut one_line = String::new();
    let mut last_was_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                one_line.push(' ');
                last_was_space = true;
            }
        } else {
            one_line.push(ch);
            last_was_space = false;
        }
    }
    let trimmed = one_line.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Dedupe + trim guidelines (upstream `_normalizePromptGuidelines`).
pub fn normalize_prompt_guidelines(guidelines: &[String]) -> Vec<String> {
    let mut unique: Vec<String> = Vec::new();
    for guideline in guidelines {
        let normalized = guideline.trim();
        if !normalized.is_empty() && !unique.iter().any(|u| u == normalized) {
            unique.push(normalized.to_string());
        }
    }
    unique
}

/// Resolve the active tool names (upstream `setActiveToolsByName`):
/// unknown names are ignored.
pub fn resolve_active_tool_names(
    requested: &[String],
    registry: &BTreeMap<String, ToolRegistryEntry>,
) -> Vec<String> {
    requested
        .iter()
        .filter(|name| registry.contains_key(*name))
        .cloned()
        .collect()
}

/// Build the system-prompt options for the current tool set (upstream
/// `_rebuildSystemPrompt`).
pub fn rebuild_system_prompt_options(
    tool_names: &[String],
    registry: &BTreeMap<String, ToolRegistryEntry>,
    loader_system_prompt: Option<String>,
    loader_append_system_prompt: &[String],
    loaded_skills: &[Skill],
    loaded_context_files: Vec<(PathBuf, String)>,
    cwd: &str,
) -> BuildSystemPromptOptions {
    let valid: Vec<String> = tool_names
        .iter()
        .filter(|name| registry.contains_key(*name))
        .cloned()
        .collect();
    let mut tool_snippets = BTreeMap::new();
    let mut prompt_guidelines: Vec<String> = Vec::new();
    for name in &valid {
        if let Some(entry) = registry.get(name) {
            if let Some(snippet) = &entry.prompt_snippet {
                tool_snippets.insert(name.clone(), snippet.clone());
            }
            prompt_guidelines.extend(entry.prompt_guidelines.iter().cloned());
        }
    }
    let append_system_prompt = if loader_append_system_prompt.is_empty() {
        None
    } else {
        Some(loader_append_system_prompt.join("\n\n"))
    };
    let context_files: Vec<crate::core::system_prompt::ContextFile> = loaded_context_files
        .into_iter()
        .map(|(path, content)| crate::core::system_prompt::ContextFile {
            path: path.to_string_lossy().to_string(),
            content,
        })
        .collect();
    BuildSystemPromptOptions {
        custom_prompt: loader_system_prompt,
        selected_tools: Some(valid),
        tool_snippets: Some(tool_snippets),
        prompt_guidelines: Some(prompt_guidelines),
        append_system_prompt,
        cwd: cwd.to_string(),
        context_files: Some(context_files),
        skills: Some(loaded_skills.to_vec()),
        paths: Default::default(),
    }
}

/// Build the system prompt (upstream `_rebuildSystemPrompt` result).
pub fn build_session_system_prompt(options: &BuildSystemPromptOptions) -> String {
    build_system_prompt(options)
}

// ============================================================================
// Session stats
// ============================================================================

/// Session statistics for the /session command (upstream `SessionStats`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStats {
    pub session_file: Option<String>,
    pub session_id: String,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub total_messages: usize,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_write: u64,
    pub tokens_total: u64,
    pub cost: f64,
}

/// Extract the text of a skill block and re-emit it as the expanded
/// command (upstream `_expandSkillCommand`).
pub fn expand_skill_command(text: &str, templates: &[PromptTemplate]) -> String {
    if let Some(block) = parse_skill_block(text) {
        let mut expanded = format!("/{}", block.name);
        if let Some(user_message) = &block.user_message {
            expanded.push(' ');
            expanded.push_str(user_message);
        }
        return expand_prompt_template(&expanded, templates);
    }
    text.to_string()
}

// ============================================================================
// Session helpers
// ============================================================================

/// Count message kinds for stats (upstream the /session command counts).
pub fn count_messages(messages: &[CodingAgentMessage]) -> (usize, usize, usize, usize) {
    let mut user = 0;
    let mut assistant = 0;
    let tool_calls = 0;
    let mut tool_results = 0;
    for message in messages {
        match message {
            CodingAgentMessage::Base(pillar_ai::types::Message::User { .. }) => user += 1,
            CodingAgentMessage::Base(pillar_ai::types::Message::Assistant(_)) => assistant += 1,
            CodingAgentMessage::Base(pillar_ai::types::Message::ToolResult(_)) => tool_results += 1,
            _ => {}
        }
    }
    (user, assistant, tool_calls, tool_results)
}

/// Remove a queued message matching the given text, first-hit wins
/// between steering and follow-up queues (upstream the message_start
/// handler).
pub fn remove_queued_message(queues: &mut QueueState, text: &str) -> bool {
    if let Some(index) = queues.steering.iter().position(|t| t == text) {
        queues.steering.remove(index);
        return true;
    }
    if let Some(index) = queues.follow_up.iter().position(|t| t == text) {
        queues.follow_up.remove(index);
        return true;
    }
    false
}

/// Validate that a set of tool names is usable for the prompt: dedupes
/// while preserving order.
pub fn unique_tool_names(names: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    names
        .iter()
        .filter(|n| seen.insert((*n).clone()))
        .cloned()
        .collect()
}
