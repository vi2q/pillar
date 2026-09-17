//! pillar-ai: unified multi-provider LLM API.
//! Port of pi `packages/ai` (pi v0.84.3, commit `56700d4`).
//!
//! divergence: the provider transport layer uses a Rust HTTP client instead
//! of the TS SDKs; provider API modules are ported per-API. The compat and
//! legacy-alias modules have no Rust counterpart.

#![forbid(unsafe_code)]

pub mod abort;
pub mod api;
pub mod api_dispatch;
pub mod auth_context;
pub mod auth_resolve;
pub mod auth_types;
pub mod clock;
pub mod constrained_sampling;
pub mod credential_store;
pub mod deferred_tools;
pub mod diagnostics;
pub mod error;
pub mod error_body;
pub mod estimate;
pub mod event_stream;
pub mod faux;
pub mod hash;
pub mod headers;
pub mod images_models;
pub mod json_parse;
pub mod models;
pub mod models_catalog;
pub mod models_generated;
pub mod models_store;
pub mod overflow;
pub mod provider_env;
pub mod provider_retry;
pub mod providers_all;
pub mod retry;
pub mod simple_options;
pub mod text;
pub mod transform_messages;
pub mod transport;
pub mod types;
pub mod uuid;

pub use abort::{AbortReason, AbortSignal, operation_signal};
pub use clock::{Elapsed, SleepFn, get_default_sleep, set_default_sleep, sleep, timeout};
pub use auth_resolve::{ModelsError, resolve_provider_auth};
pub use auth_types::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthEvent, AuthOperationOptions,
    AuthPrompt, AuthResult, AuthType, Credential, CredentialInfo, CredentialStore, ModelAuth,
    OAuthAuth, OAuthCredential, ProviderAuth,
};
pub use constrained_sampling::{
    GrammarConstrainedSampling, GrammarToolInputJsonBuffer, UnsupportedStrictJsonSchemaError,
    append_grammar_tool_input_json_delta, create_grammar_tool_input_properties,
    get_grammar_tool_input, get_json_schema_tool_parameters, make_strict_json_schema,
    resolve_grammar_constrained_sampling, resolve_json_schema_strict_sampling,
};
pub use error_body::{
    MAX_PROVIDER_ERROR_BODY_CHARS, NormalizedProviderError, format_provider_error,
    normalize_provider_error, safe_json_stringify, truncate_error_text,
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
pub use provider_env::get_provider_env_value;
pub use provider_retry::{ProviderRequestError, ProviderRetryOptions, retry_provider_request};
pub use retry::{RetryCallbacks, RetryPolicy, is_retryable_assistant_error, retry_assistant_call};
pub use text::{content_text, sanitize_surrogates};
pub use transform_messages::{NormalizeToolCallId, transform_messages};
pub use transport::{
    FetchFn, FetchRequest, FetchResponse, ReqwestFetch, SharedFetchFn, headers_to_record,
};
pub use types::{
    Api, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, CacheRetention,
    ConstrainedSamplingConfig, ConstrainedStrictness, Content, Context, DeferredHandle,
    DiagnosticErrorInfo, Message, Model, ModelCost, ModelCostRates, ModelCostTier,
    ModelThinkingLevel, ProviderEnv, ProviderHeaders, SimpleStreamOptionsLike, StopReason,
    ThinkingBudgets, ThinkingLevel, ThinkingLevelMap, Tool, ToolChoice, ToolResultMessage,
    Transport, Usage, UsageCost, UserContent,
};
pub use uuid::uuidv7;
