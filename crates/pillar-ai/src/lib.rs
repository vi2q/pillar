//! pillar-ai: unified multi-provider LLM API.
//! Port of pi `packages/ai` (pi v0.84.3, commit `56700d4`).
//!
//! divergence: the provider transport layer uses a Rust HTTP client instead
//! of the TS SDKs; provider API modules are ported per-API. The compat and
//! legacy-alias modules have no Rust counterpart.

#![forbid(unsafe_code)]

pub mod diagnostics;
pub mod estimate;
pub mod event_stream;
pub mod faux;
pub mod hash;
pub mod overflow;
pub mod retry;
pub mod simple_options;
pub mod text;
pub mod types;
pub mod uuid;

pub use estimate::{calculate_context_tokens, estimate_context_tokens, ContextUsageEstimate};
pub use event_stream::{
    assistant_message_event_stream, collect_events, AssistantMessageEventStream, EventStream,
};
pub use overflow::{get_overflow_patterns, is_context_overflow, is_recoverable_length};
pub use retry::{is_retryable_assistant_error, retry_assistant_call, RetryCallbacks, RetryPolicy};
pub use text::content_text;
pub use types::{
    Api, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, CacheRetention,
    ConstrainedSamplingConfig, ConstrainedStrictness, Content, Context, DeferredHandle,
    DiagnosticErrorInfo, Message, ModelThinkingLevel, ProviderEnv, ProviderHeaders, StopReason,
    ThinkingBudgets, ThinkingLevel, ThinkingLevelMap, Tool, ToolChoice, ToolResultMessage,
    Transport, Usage, UsageCost, UserContent,
};
pub use uuid::uuidv7;
