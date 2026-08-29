//! pillar-ai: unified multi-provider LLM API.
//! Port of pi `packages/ai` (pi v0.84.3, commit `56700d4`).
//!
//! divergence: the provider transport layer uses a Rust HTTP client instead
//! of the TS SDKs; provider API modules are ported per-API. The compat and
//! legacy-alias modules have no Rust counterpart.

#![forbid(unsafe_code)]

pub mod abort;
pub mod auth_resolve;
pub mod auth_types;
pub mod credential_store;
pub mod diagnostics;
pub mod error;
pub mod estimate;
pub mod event_stream;
pub mod faux;
pub mod hash;
pub mod models_store;
pub mod overflow;
pub mod retry;
pub mod simple_options;
pub mod text;
pub mod types;
pub mod uuid;

pub use abort::{operation_signal, AbortReason, AbortSignal};
pub use auth_resolve::{resolve_provider_auth, ModelsError};
pub use auth_types::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthEvent, AuthOperationOptions,
    AuthPrompt, AuthResult, AuthType, Credential, CredentialInfo, CredentialStore, ModelAuth,
    OAuthAuth, OAuthCredential, ProviderAuth,
};
pub use estimate::{calculate_context_tokens, estimate_context_tokens, ContextUsageEstimate};
pub use event_stream::{
    assistant_message_event_stream, collect_events, AssistantMessageEventStream, EventStream,
};
pub use models_store::{
    InMemoryModelsStore, ModelsStore, ModelsStoreEntry, ModelsStoreOperationOptions,
};
pub use overflow::{get_overflow_patterns, is_context_overflow, is_recoverable_length};
pub use retry::{is_retryable_assistant_error, retry_assistant_call, RetryCallbacks, RetryPolicy};
pub use text::content_text;
pub use types::{
    Api, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, CacheRetention,
    ConstrainedSamplingConfig, ConstrainedStrictness, Content, Context, DeferredHandle,
    DiagnosticErrorInfo, Message, ModelThinkingLevel, ProviderEnv, ProviderHeaders,
    SimpleStreamOptionsLike, StopReason, ThinkingBudgets, ThinkingLevel, ThinkingLevelMap, Tool,
    ToolChoice, ToolResultMessage, Transport, Usage, UsageCost, UserContent,
};
pub use uuid::uuidv7;
