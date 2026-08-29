//! pillar-ai: unified multi-provider LLM API.
//! Port of pi `packages/ai` (pi v0.84.3, commit `56700d4`).
//!
//! divergence: the provider transport layer uses a Rust HTTP client instead
//! of the TS SDKs; provider API modules are ported per-API. The compat and
//! legacy-alias modules have no Rust counterpart.

#![forbid(unsafe_code)]

pub mod abort;
pub mod auth_context;
pub mod auth_resolve;
pub mod auth_types;
pub mod credential_store;
pub mod diagnostics;
pub mod error;
pub mod estimate;
pub mod event_stream;
pub mod faux;
pub mod hash;
pub mod models;
pub mod models_store;
pub mod overflow;
pub mod retry;
pub mod simple_options;
pub mod text;
pub mod types;
pub mod uuid;

pub use abort::{AbortReason, AbortSignal, operation_signal};
pub use auth_resolve::{ModelsError, resolve_provider_auth};
pub use auth_types::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthEvent, AuthOperationOptions,
    AuthPrompt, AuthResult, AuthType, Credential, CredentialInfo, CredentialStore, ModelAuth,
    OAuthAuth, OAuthCredential, ProviderAuth,
};
pub use estimate::{ContextUsageEstimate, calculate_context_tokens, estimate_context_tokens};
pub use event_stream::{
    AssistantMessageEventStream, EventStream, assistant_message_event_stream, collect_events,
};
pub use models::{
    AuthTarget, CreateModelsOptions, CreateProviderOptions, Models, ModelsRefreshOptions,
    ModelsRefreshResult, ModelsStreamOptions, Provider, ProviderStreams, RefreshModelsContext,
    calculate_cost, clamp_thinking_level, create_provider, get_supported_thinking_levels, has_api,
    models_are_equal,
};
pub use models_store::{
    InMemoryModelsStore, ModelsStore, ModelsStoreEntry, ModelsStoreOperationOptions,
};
pub use overflow::{get_overflow_patterns, is_context_overflow, is_recoverable_length};
pub use retry::{RetryCallbacks, RetryPolicy, is_retryable_assistant_error, retry_assistant_call};
pub use text::content_text;
pub use types::{
    Api, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, CacheRetention,
    ConstrainedSamplingConfig, ConstrainedStrictness, Content, Context, DeferredHandle,
    DiagnosticErrorInfo, Message, Model, ModelCost, ModelCostRates, ModelCostTier,
    ModelThinkingLevel, ProviderEnv, ProviderHeaders, SimpleStreamOptionsLike, StopReason,
    ThinkingBudgets, ThinkingLevel, ThinkingLevelMap, Tool, ToolChoice, ToolResultMessage,
    Transport, Usage, UsageCost, UserContent,
};
pub use uuid::uuidv7;
