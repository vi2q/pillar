//! Port of packages/ai/src/providers/faux.ts (pi v0.84.3).
//!
//! Faux provider for tests: scripted responses, usage estimation from a
//! serialized context, per-session prompt-cache simulation, streaming with
//! deltas, and deferred-response handling.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::event_stream::{AssistantMessageEventStream, assistant_message_event_stream};
use crate::types::{
    AssistantMessage, Content, Context, DeferredHandle, Message, StopReason, ToolResultMessage,
    Usage, UsageCost, UserContent,
};

pub const DEFAULT_API: &str = "faux";
pub const DEFAULT_PROVIDER: &str = "faux";
pub const DEFAULT_MODEL_ID: &str = "faux-1";
pub const DEFAULT_MODEL_NAME: &str = "Faux Model";
pub const DEFAULT_BASE_URL: &str = "http://localhost:0";
pub const DEFAULT_MIN_TOKEN_SIZE: usize = 3;
pub const DEFAULT_MAX_TOKEN_SIZE: usize = 5;

pub fn default_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: UsageCost::default(),
    }
}

#[derive(Debug, Clone)]
pub struct FauxModelDefinition {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: bool,
    pub input: Vec<String>,
    pub cost: Option<UsageCost>,
    pub context_window: u64,
    pub max_tokens: u64,
}

impl Default for FauxModelDefinition {
    fn default() -> Self {
        Self {
            id: DEFAULT_MODEL_ID.to_owned(),
            name: None,
            reasoning: false,
            input: vec!["text".into(), "image".into()],
            cost: None,
            context_window: 128_000,
            max_tokens: 16_384,
        }
    }
}

/// A faux model — the subset of upstream `Model` the faux provider needs.
#[derive(Debug, Clone, PartialEq)]
pub struct FauxModel {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<String>,
    pub cost: UsageCost,
    pub context_window: u64,
    pub max_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct FauxProviderState {
    pub call_count: u64,
    pub deferred_fetch_count: u64,
    pub cancelled_deferred: Vec<DeferredHandle>,
}

/// A scripted response step: a ready message or a factory over the request.
#[derive(Clone)]
pub enum FauxResponseStep {
    Message(Box<AssistantMessage>),
    Factory(Arc<FauxResponseFactory>),
}

pub type FauxResponseFactory = dyn Fn(&Context, &FauxStreamOptions, &FauxProviderState, &FauxModel) -> AssistantMessage
    + Send
    + Sync;

impl From<AssistantMessage> for FauxResponseStep {
    fn from(message: AssistantMessage) -> Self {
        FauxResponseStep::Message(Box::new(message))
    }
}

/// Request options the faux provider observes.
#[derive(Debug, Clone, Default)]
pub struct FauxStreamOptions {
    pub session_id: Option<String>,
    pub cache_retention_none: bool,
    pub deferred: Option<DeferredConfig>,
    pub signal: Option<tokio_util_abort::SharedAbort>,
}

#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct DeferredConfig {
    pub pending_fetches: usize,
    pub poll_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RegisterFauxProviderOptions {
    pub api: Option<String>,
    pub provider: Option<String>,
    pub models: Vec<FauxModelDefinition>,
    pub deferred: Option<DeferredConfig>,
    pub tokens_per_second: Option<f64>,
    pub token_size_min: Option<usize>,
    pub token_size_max: Option<usize>,
}

// --- helpers -----------------------------------------------------------

pub fn faux_text(text: impl Into<String>) -> Content {
    Content::text(text)
}

pub fn faux_thinking(thinking: impl Into<String>) -> Content {
    Content::thinking(thinking)
}

pub fn faux_tool_call(name: &str, arguments: Value, id: Option<&str>) -> Content {
    Content::tool_call(
        id.map(String::from).unwrap_or_else(|| random_id("tool")),
        name,
        arguments,
    )
}

pub fn random_id(prefix: &str) -> String {
    format!(
        "{prefix}:{}:{}",
        now_millis(),
        crate::hash::short_hash(&uuid_v4ish())
    )
}

fn uuid_v4ish() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        crate::hash::short_hash(&now_millis().to_string()),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn faux_assistant_message(
    content: impl Into<FauxContent>,
    options: FauxMessageOptions,
) -> AssistantMessage {
    let content = match content.into() {
        FauxContent::Text(text) => vec![Content::text(text)],
        FauxContent::Blocks(blocks) => blocks,
    };
    AssistantMessage {
        content,
        api: DEFAULT_API.to_owned(),
        provider: DEFAULT_PROVIDER.to_owned(),
        model: DEFAULT_MODEL_ID.to_owned(),
        response_model: None,
        usage: default_usage(),
        stop_reason: options.stop_reason.unwrap_or(StopReason::Stop),
        deferred: None,
        error_message: options.error_message,
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: options.timestamp.unwrap_or_else(now_millis),
    }
}

#[derive(Debug, Default)]
pub struct FauxMessageOptions {
    pub stop_reason: Option<StopReason>,
    pub error_message: Option<String>,
    pub timestamp: Option<u64>,
}

pub enum FauxContent {
    Text(String),
    Blocks(Vec<Content>),
}

impl From<&str> for FauxContent {
    fn from(value: &str) -> Self {
        FauxContent::Text(value.to_owned())
    }
}

impl From<String> for FauxContent {
    fn from(value: String) -> Self {
        FauxContent::Text(value)
    }
}

impl From<Vec<Content>> for FauxContent {
    fn from(value: Vec<Content>) -> Self {
        FauxContent::Blocks(value)
    }
}

pub fn faux_user_message(content: impl Into<UserContent>, timestamp: u64) -> Message {
    Message::User {
        content: content.into(),
        timestamp,
    }
}

impl From<&str> for UserContent {
    fn from(value: &str) -> Self {
        UserContent::Text(value.to_owned())
    }
}

impl From<String> for UserContent {
    fn from(value: String) -> Self {
        UserContent::Text(value)
    }
}

impl From<Vec<Content>> for UserContent {
    fn from(value: Vec<Content>) -> Self {
        UserContent::Blocks(value)
    }
}

pub fn faux_tool_result(
    tool_call_id: &str,
    tool_name: &str,
    content: Vec<Content>,
    timestamp: u64,
) -> Message {
    Message::ToolResult(Box::new(ToolResultMessage {
        tool_call_id: tool_call_id.to_owned(),
        tool_name: tool_name.to_owned(),
        content,
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp,
    }))
}

fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

fn content_to_text(content: &[Content]) -> String {
    content
        .iter()
        .map(|block| match block {
            Content::Text { text, .. } => text.clone(),
            Content::Image {
                mime_type, data, ..
            } => format!("[image:{mime_type}:{}]", data.len()),
            Content::Thinking { thinking, .. } => thinking.clone(),
            Content::ToolCall {
                name, arguments, ..
            } => format!("{name}:{arguments}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_to_text(message: &Message) -> String {
    match message {
        Message::User { content, .. } => match content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => content_to_text(blocks),
        },
        Message::Assistant(assistant) => content_to_text(&assistant.content),
        Message::ToolResult(tool_result) => {
            let mut parts = vec![tool_result.tool_name.clone()];
            parts.push(content_to_text(&tool_result.content));
            parts.join("\n")
        }
    }
}

fn serialize_context(context: &Context) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(system_prompt) = &context.system_prompt {
        parts.push(format!("system:{system_prompt}"));
    }
    for message in &context.messages {
        parts.push(format!(
            "{}:{}",
            role_name(message),
            message_to_text(message)
        ));
    }
    if !context.tools.is_empty() {
        let tools_json = serde_json::to_string(&context.tools).unwrap_or_default();
        parts.push(format!("tools:{tools_json}"));
    }
    parts.join("\n\n")
}

fn role_name(message: &Message) -> &'static str {
    match message {
        Message::User { .. } => "user",
        Message::Assistant(_) => "assistant",
        Message::ToolResult(_) => "toolResult",
    }
}

fn common_prefix_length(a: &str, b: &str) -> usize {
    let len = a.chars().count().min(b.chars().count());
    let mut index = 0;
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca != cb {
            break;
        }
        index += ca.len_utf8().max(cb.len_utf8());
        if index >= len {
            break;
        }
    }
    index
}

fn with_usage_estimate(
    mut message: AssistantMessage,
    context: &Context,
    options: Option<&FauxStreamOptions>,
    prompt_cache: &mut HashMap<String, String>,
) -> AssistantMessage {
    let prompt_text = serialize_context(context);
    let prompt_tokens = estimate_tokens(&prompt_text);
    let output_tokens = estimate_tokens(&content_to_text(&message.content));
    let mut input = prompt_tokens;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;

    let session_id = options.as_ref().and_then(|o| o.session_id.clone());
    if let Some(session_id) = session_id {
        let use_cache = !options.map(|o| o.cache_retention_none).unwrap_or(false);
        if use_cache {
            match prompt_cache.get(session_id.as_str()) {
                Some(previous_prompt) => {
                    let cached_chars = common_prefix_length(previous_prompt, &prompt_text);
                    cache_read = estimate_tokens(
                        &previous_prompt[..cached_chars.min(previous_prompt.len())],
                    );
                    cache_write =
                        estimate_tokens(&prompt_text[cached_chars.min(prompt_text.len())..]);
                    input = prompt_tokens.saturating_sub(cache_read);
                }
                None => {
                    cache_write = prompt_tokens;
                }
            }
            prompt_cache.insert(session_id, prompt_text);
        }
    }

    message.usage = Usage {
        input,
        output: output_tokens,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output_tokens + cache_read + cache_write,
        cost: UsageCost::default(),
    };
    message
}

fn split_string_by_token_size(
    text: &str,
    min_token_size: usize,
    max_token_size: usize,
) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        let token_size = min_token_size
            + (faux_rand_range((max_token_size - min_token_size + 1) as u64) as usize);
        let char_size = (token_size * 4).max(1);
        let end = (index + char_size).min(chars.len());
        chunks.push(chars[index..end].iter().collect());
        index = end;
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Deterministic-enough PRNG (xorshift) shared by chunk splitting.
fn faux_rand_range(range: u64) -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(0x9e3779b97f4a7c15);
    let mut x = SEED.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    SEED.store(x, Ordering::Relaxed);
    x % range.max(1)
}

fn clone_message(
    mut message: AssistantMessage,
    api: &str,
    provider: &str,
    model_id: &str,
) -> AssistantMessage {
    message.api = api.to_owned();
    message.provider = provider.to_owned();
    message.model = model_id.to_owned();
    if message.timestamp == 0 {
        message.timestamp = now_millis();
    }
    message
}

fn create_deferred_message(model: &FauxModel, handle: DeferredHandle) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        usage: default_usage(),
        stop_reason: StopReason::Deferred,
        deferred: Some(handle),
        error_message: None,
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_millis(),
    }
}

fn create_error_message(
    message: &str,
    api: &str,
    provider: &str,
    model_id: &str,
) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: api.to_owned(),
        provider: provider.to_owned(),
        model: model_id.to_owned(),
        response_model: None,
        usage: default_usage(),
        stop_reason: StopReason::Error,
        deferred: None,
        error_message: Some(message.to_owned()),
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_millis(),
    }
}

fn create_aborted_message(mut partial: AssistantMessage) -> AssistantMessage {
    partial.stop_reason = StopReason::Aborted;
    partial.error_message = Some("Request was aborted".to_owned());
    partial.timestamp = now_millis();
    partial
}

async fn schedule_chunk(chunk: &str, tokens_per_second: Option<f64>) {
    match tokens_per_second {
        Some(rate) if rate > 0.0 => {
            let delay_ms = (estimate_tokens(chunk) as f64 / rate) * 1000.0;
            crate::clock::sleep(std::time::Duration::from_millis(delay_ms as u64)).await;
        }
        _ => {
            // queueMicrotask parity: yield once.
            tokio::task::yield_now().await;
        }
    }
}

// --- core --------------------------------------------------------------

#[allow(dead_code)]
struct DeferredEntry {
    handle: DeferredHandle,
    step: FauxResponseStep,
    context: Context,
    options: Option<FauxStreamOptions>,
    model: FauxModel,
    pending_fetches: usize,
    cancelled: bool,
    final_message: Option<AssistantMessage>,
}

struct FauxCoreInner {
    api: String,
    provider: String,
    min_token_size: usize,
    max_token_size: usize,
    tokens_per_second: Option<f64>,
    state: FauxProviderState,
    prompt_cache: HashMap<String, String>,
    deferred_responses: HashMap<String, DeferredEntry>,
    deferred_config: Option<DeferredConfig>,
}

/// The faux provider core: scripted streaming with usage estimation.
pub struct FauxCore {
    pub api: String,
    pub provider: String,
    pub models: Vec<FauxModel>,
    inner: Arc<Mutex<FauxCoreInner>>,
    pending: Arc<Mutex<Vec<FauxResponseStep>>>,
}

impl FauxCore {
    pub fn new(options: RegisterFauxProviderOptions) -> Self {
        let api = options
            .api
            .clone()
            .unwrap_or_else(|| format!("{DEFAULT_API}:{}", random_id("api")));
        let provider = options
            .provider
            .clone()
            .unwrap_or_else(|| DEFAULT_PROVIDER.to_owned());
        let min_token_size = options
            .token_size_min
            .unwrap_or(DEFAULT_MIN_TOKEN_SIZE)
            .max(1)
            .min(options.token_size_max.unwrap_or(DEFAULT_MAX_TOKEN_SIZE));
        let max_token_size = options
            .token_size_max
            .unwrap_or(DEFAULT_MAX_TOKEN_SIZE)
            .max(min_token_size);

        let definitions = if options.models.is_empty() {
            vec![FauxModelDefinition::default()]
        } else {
            options.models.clone()
        };
        let models = definitions
            .into_iter()
            .map(|definition| FauxModel {
                id: definition.id.clone(),
                name: definition.name.unwrap_or_else(|| definition.id.clone()),
                api: api.clone(),
                provider: provider.clone(),
                base_url: DEFAULT_BASE_URL.to_owned(),
                reasoning: definition.reasoning,
                input: definition.input,
                cost: definition.cost.unwrap_or_default(),
                context_window: definition.context_window,
                max_tokens: definition.max_tokens,
            })
            .collect();

        Self {
            api: api.clone(),
            provider: provider.clone(),
            models,
            inner: Arc::new(Mutex::new(FauxCoreInner {
                api,
                provider,
                min_token_size,
                max_token_size,
                tokens_per_second: options.tokens_per_second,
                state: FauxProviderState::default(),
                prompt_cache: HashMap::new(),
                deferred_responses: HashMap::new(),
                deferred_config: options.deferred,
            })),
            pending: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn state(&self) -> FauxProviderState {
        self.inner.lock().expect("faux lock").state.clone()
    }

    pub fn get_model(&self, model_id: Option<&str>) -> Option<FauxModel> {
        match model_id {
            None => self.models.first().cloned(),
            Some(id) => self.models.iter().find(|m| m.id == id).cloned(),
        }
    }

    pub fn set_responses<I: IntoIterator<Item = FauxResponseStep>>(&self, responses: I) {
        let mut pending = self.pending.lock().expect("faux lock");
        *pending = responses.into_iter().collect();
    }

    pub fn append_responses<I: IntoIterator<Item = FauxResponseStep>>(&self, responses: I) {
        self.pending.lock().expect("faux lock").extend(responses);
    }

    pub fn get_pending_response_count(&self) -> usize {
        self.pending.lock().expect("faux lock").len()
    }

    pub async fn complete(
        &self,
        model: &FauxModel,
        context: &Context,
        options: Option<&FauxStreamOptions>,
    ) -> AssistantMessage {
        let stream = self.stream(model, context.clone(), options.cloned());
        stream.result().await
    }

    /// Mirrors upstream `stream`: shifts one pending response and streams it.
    pub fn stream(
        &self,
        request_model: &FauxModel,
        context: Context,
        options: Option<FauxStreamOptions>,
    ) -> AssistantMessageEventStream {
        let outer = assistant_message_event_stream();
        {
            let mut inner = self.inner.lock().expect("faux lock");
            inner.state.call_count += 1;
        }

        let inner = Arc::clone(&self.inner);
        let pending = Arc::clone(&self.pending);
        let request_model = request_model.clone();
        let spawned = outer.clone_stream();

        tokio::spawn(async move {
            let _ = run_stream(inner, pending, spawned, request_model, context, options).await;
        });

        outer
    }
}

// Alias to satisfy the local stream closure typing.
use crate::types::AssistantMessageEvent as AssistantMessageEvent2;

type StreamOutcome = Result<(), AssistantMessage>;

async fn run_stream(
    inner: Arc<Mutex<FauxCoreInner>>,
    pending: Arc<Mutex<Vec<FauxResponseStep>>>,
    outer: AssistantMessageEventStream,
    request_model: FauxModel,
    context: Context,
    options: Option<FauxStreamOptions>,
) -> StreamOutcome {
    let (step, api, provider, min_token, max_token, tps, _deferred_config) = {
        let guard = inner.lock().expect("faux lock");
        let step = {
            let mut p = pending.lock().expect("faux lock");
            if p.is_empty() {
                None
            } else {
                Some(p.remove(0))
            }
        };
        (
            step,
            guard.api.clone(),
            guard.provider.clone(),
            guard.min_token_size,
            guard.max_token_size,
            guard.tokens_per_second,
            guard.deferred_config,
        )
    };

    if step.is_none() {
        let mut message = create_error_message(
            "No more faux responses queued",
            &api,
            &provider,
            &request_model.id,
        );
        {
            let mut guard = inner.lock().expect("faux lock");
            message =
                with_usage_estimate(message, &context, options.as_ref(), &mut guard.prompt_cache);
        }
        outer.push(AssistantMessageEvent2::Error {
            reason: StopReason::Error,
            error: message.clone(),
        });
        outer.end(Some(message));
        return Ok(());
    }
    let step = step.expect("checked some");

    // Deferred path.
    if let Some(deferred) = options.as_ref().and_then(|o| o.deferred) {
        let handle = DeferredHandle {
            provider: request_model.provider.clone(),
            model_id: request_model.id.clone(),
            api: request_model.api.clone(),
            id: random_id("deferred"),
            expires_at: None,
            poll_after_ms: deferred.poll_after_ms,
            data: None,
        };
        let deferred_message = create_deferred_message(&request_model, handle.clone());
        {
            let mut guard = inner.lock().expect("faux lock");
            guard.deferred_responses.insert(
                handle.id.clone(),
                DeferredEntry {
                    handle,
                    step,
                    context,
                    options,
                    model: request_model.clone(),
                    pending_fetches: deferred.pending_fetches,
                    cancelled: false,
                    final_message: None,
                },
            );
        }
        stream_with_deltas(&outer, deferred_message, min_token, max_token, tps, None).await;
        return Ok(());
    }

    let message = resolve_response(
        &inner,
        &pending,
        step,
        &context,
        options.as_ref(),
        &request_model,
    )
    .await;
    let signal = options.as_ref().and_then(|o| o.signal.clone());
    if signal.as_ref().map(|s| s.is_aborted()).unwrap_or(false) {
        let partial = AssistantMessage {
            content: Vec::new(),
            stop_reason: StopReason::Pending,
            ..message.clone()
        };
        let aborted_message = create_aborted_message(partial);
        outer.push(AssistantMessageEvent2::Error {
            reason: StopReason::Aborted,
            error: aborted_message.clone(),
        });
        outer.end(Some(aborted_message));
        return Ok(());
    }
    stream_with_deltas(&outer, message, min_token, max_token, tps, signal).await;
    Ok(())
}

async fn resolve_response(
    inner: &Arc<Mutex<FauxCoreInner>>,
    _pending: &Arc<Mutex<Vec<FauxResponseStep>>>,
    step: FauxResponseStep,
    context: &Context,
    options: Option<&FauxStreamOptions>,
    request_model: &FauxModel,
) -> AssistantMessage {
    let resolved = match &step {
        FauxResponseStep::Message(message) => *message.clone(),
        FauxResponseStep::Factory(factory) => {
            let state = inner.lock().expect("faux lock").state.clone();
            factory(
                context,
                options.unwrap_or(&FauxStreamOptions::default()),
                &state,
                request_model,
            )
        }
    };
    // One guard covers every field access; nested or repeated lock() calls
    // in one statement deadlock on the non-reentrant mutex.
    let mut guard = inner.lock().expect("faux lock");
    let cloned = clone_message(resolved, &guard.api, &guard.provider, &request_model.id);
    let message = with_usage_estimate(cloned, context, options, &mut guard.prompt_cache);
    drop(guard);
    message
}

async fn stream_with_deltas(
    stream: &AssistantMessageEventStream,
    message: AssistantMessage,
    min_token_size: usize,
    max_token_size: usize,
    tokens_per_second: Option<f64>,
    signal: Option<tokio_util_abort::SharedAbort>,
) {
    let mut partial = AssistantMessage {
        content: Vec::new(),
        stop_reason: StopReason::Pending,
        ..message.clone()
    };
    if signal.as_ref().map(|s| s.is_aborted()).unwrap_or(false) {
        let aborted = create_aborted_message(partial);
        stream.push(AssistantMessageEvent2::Error {
            reason: StopReason::Aborted,
            error: aborted.clone(),
        });
        stream.end(Some(aborted));
        return;
    }

    stream.push(AssistantMessageEvent2::Start {
        partial: partial.clone(),
    });

    for index in 0..message.content.len() {
        if signal.as_ref().map(|s| s.is_aborted()).unwrap_or(false) {
            let aborted = create_aborted_message(partial);
            stream.push(AssistantMessageEvent2::Error {
                reason: StopReason::Aborted,
                error: aborted.clone(),
            });
            stream.end(Some(aborted));
            return;
        }
        let block = &message.content[index];

        match block {
            Content::Thinking { thinking, .. } => {
                partial.content.push(Content::thinking(""));
                stream.push(AssistantMessageEvent2::ThinkingStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                for chunk in split_string_by_token_size(thinking, min_token_size, max_token_size) {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if signal.as_ref().map(|s| s.is_aborted()).unwrap_or(false) {
                        let aborted = create_aborted_message(partial);
                        stream.push(AssistantMessageEvent2::Error {
                            reason: StopReason::Aborted,
                            error: aborted.clone(),
                        });
                        stream.end(Some(aborted));
                        return;
                    }
                    if let Content::Thinking { thinking, .. } = &mut partial.content[index] {
                        thinking.push_str(&chunk);
                    }
                    stream.push(AssistantMessageEvent2::ThinkingDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                stream.push(AssistantMessageEvent2::ThinkingEnd {
                    content_index: index,
                    content: thinking.clone(),
                    partial: partial.clone(),
                });
            }
            Content::Text { text, .. } => {
                partial.content.push(Content::text(""));
                stream.push(AssistantMessageEvent2::TextStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                for chunk in split_string_by_token_size(text, min_token_size, max_token_size) {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if signal.as_ref().map(|s| s.is_aborted()).unwrap_or(false) {
                        let aborted = create_aborted_message(partial);
                        stream.push(AssistantMessageEvent2::Error {
                            reason: StopReason::Aborted,
                            error: aborted.clone(),
                        });
                        stream.end(Some(aborted));
                        return;
                    }
                    if let Content::Text { text, .. } = &mut partial.content[index] {
                        text.push_str(&chunk);
                    }
                    stream.push(AssistantMessageEvent2::TextDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                stream.push(AssistantMessageEvent2::TextEnd {
                    content_index: index,
                    content: text.clone(),
                    partial: partial.clone(),
                });
            }
            Content::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                partial.content.push(Content::tool_call(
                    id.clone(),
                    name.clone(),
                    Value::Object(Default::default()),
                ));
                stream.push(AssistantMessageEvent2::ToolcallStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                let serialized = serde_json::to_string(arguments).unwrap_or_default();
                for chunk in split_string_by_token_size(&serialized, min_token_size, max_token_size)
                {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if signal.as_ref().map(|s| s.is_aborted()).unwrap_or(false) {
                        let aborted = create_aborted_message(partial);
                        stream.push(AssistantMessageEvent2::Error {
                            reason: StopReason::Aborted,
                            error: aborted.clone(),
                        });
                        stream.end(Some(aborted));
                        return;
                    }
                    stream.push(AssistantMessageEvent2::ToolcallDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                partial.content[index] = block.clone();
                stream.push(AssistantMessageEvent2::ToolcallEnd {
                    content_index: index,
                    tool_call: block.clone(),
                    partial: partial.clone(),
                });
            }
            Content::Image { .. } => {
                // Images pass through without delta streaming.
                partial.content.push(block.clone());
            }
        }
    }

    if message.stop_reason == StopReason::Pending {
        let error = create_error_message(
            "Faux response ended without a stop reason",
            &message.api,
            &message.provider,
            &message.model,
        );
        stream.push(AssistantMessageEvent2::Error {
            reason: StopReason::Error,
            error: error.clone(),
        });
        stream.end(Some(error));
        return;
    }
    if message.stop_reason == StopReason::Error || message.stop_reason == StopReason::Aborted {
        stream.push(AssistantMessageEvent2::Error {
            reason: message.stop_reason,
            error: message.clone(),
        });
        stream.end(Some(message));
        return;
    }

    stream.push(AssistantMessageEvent2::Done {
        reason: message.stop_reason,
        message: message.clone(),
    });
    stream.end(Some(message));
}

/// A minimal shared abort handle mirroring the AbortSignal surface the faux
/// provider consumes (`is_aborted`).
pub mod tokio_util_abort {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, Default, Clone)]
    pub struct SharedAbort(Arc<AtomicBool>);

    impl SharedAbort {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn abort(&self) {
            self.0.store(true, Ordering::SeqCst);
        }

        pub fn is_aborted(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }
}
