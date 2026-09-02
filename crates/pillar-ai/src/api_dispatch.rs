//! Per-API stream dispatch wiring (upstream providers/*.ts `api:` fields).
//!
//! Builtin providers dispatch to the ported API adapters by converting the
//! collection-level `StreamRequestOptions` into each adapter's options
//! struct (upstream spreads `StreamOptions` into per-API options at call
//! sites inside provider factories).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::models::{ProviderApi, ProviderStreams, StreamFn, StreamRequestOptions};
use crate::types::{Context, Model};

use crate::api::anthropic_messages::AnthropicOptions;
use crate::api::azure_openai_responses::AzureOpenAIResponsesOptions;
use crate::api::bedrock_converse_stream::BedrockOptions;
use crate::api::google_generative_ai::GoogleOptions;
use crate::api::google_vertex::GoogleVertexOptions;
use crate::api::mistral_conversations::MistralOptions;
use crate::api::openai_codex_responses::OpenaiCodexResponsesOptions;
use crate::api::openai_completions::OpenaiCompletionsOptions;
use crate::api::openai_responses::OpenaiResponsesOptions;
use crate::api::pi_messages::PiMessagesOptions;
use crate::event_stream::AssistantMessageEventStream;

/// Build a `StreamFn` dispatching to one adapter's `stream`.
macro_rules! adapter_stream_fn {
    ($adapter:path, $options_ty:ty) => {
        Arc::new(
            move |model: &Model,
                  context: &Context,
                  options: &StreamRequestOptions|
                  -> AssistantMessageEventStream {
                let adapter_options: $options_ty = options.into();
                $adapter(model.clone(), context.clone(), Some(adapter_options))
            },
        ) as StreamFn
    };
}

fn streams_for(api: &str) -> Option<ProviderStreams> {
    macro_rules! bundle {
        ($stream:path, $stream_simple:path, $options_ty:ty, $simple_ty:ty) => {
            ProviderStreams {
                stream: adapter_stream_fn!($stream, $options_ty),
                stream_simple: adapter_stream_fn!($stream_simple, $simple_ty),
            }
        };
    }
    let streams = match api {
        "openai-completions" => bundle!(
            crate::api::openai_completions::stream,
            crate::api::openai_completions::stream_simple,
            OpenaiCompletionsOptions,
            crate::api::openai_completions::SimpleStreamOptions
        ),
        "openai-responses" => bundle!(
            crate::api::openai_responses::stream,
            crate::api::openai_responses::stream_simple,
            OpenaiResponsesOptions,
            crate::api::openai_responses::SimpleStreamOptions
        ),
        "anthropic-messages" => bundle!(
            crate::api::anthropic_messages::stream,
            crate::api::anthropic_messages::stream_simple,
            AnthropicOptions,
            crate::api::anthropic_messages::AnthropicSimpleStreamOptions
        ),
        "google-generative-ai" => bundle!(
            crate::api::google_generative_ai::stream,
            crate::api::google_generative_ai::stream_simple,
            GoogleOptions,
            crate::api::google_generative_ai::SimpleStreamOptions
        ),
        "google-vertex" => bundle!(
            crate::api::google_vertex::stream,
            crate::api::google_vertex::stream_simple,
            GoogleVertexOptions,
            crate::api::google_vertex::SimpleStreamOptions
        ),
        "mistral-conversations" => bundle!(
            crate::api::mistral_conversations::stream,
            crate::api::mistral_conversations::stream_simple_mistral,
            MistralOptions,
            crate::api::mistral_conversations::SimpleStreamOptions
        ),
        "bedrock-converse-stream" => bundle!(
            crate::api::bedrock_converse_stream::stream,
            crate::api::bedrock_converse_stream::stream_simple,
            BedrockOptions,
            crate::api::bedrock_converse_stream::SimpleStreamOptions
        ),
        "openai-codex-responses" => bundle!(
            crate::api::openai_codex_responses::stream,
            crate::api::openai_codex_responses::stream_simple,
            OpenaiCodexResponsesOptions,
            crate::api::openai_codex_responses::CodexSimpleStreamOptions
        ),
        "azure-openai-responses" => bundle!(
            crate::api::azure_openai_responses::stream,
            crate::api::azure_openai_responses::stream_simple,
            AzureOpenAIResponsesOptions,
            crate::api::azure_openai_responses::SimpleStreamOptions
        ),
        "pi-messages" => bundle!(
            crate::api::pi_messages::stream,
            crate::api::pi_messages::stream_simple,
            PiMessagesOptions,
            crate::api::pi_messages::SimpleStreamOptions
        ),
        _ => return None,
    };
    Some(streams)
}

/// ProviderApi map for the given API names (upstream `api:` provider field).
pub fn api_map(api_names: &[&str]) -> ProviderApi {
    let mut map = BTreeMap::new();
    for api in api_names {
        if let Some(streams) = streams_for(api) {
            map.insert((*api).to_string(), Arc::new(streams));
        }
    }
    if map.is_empty() {
        ProviderApi::None
    } else {
        ProviderApi::Map(map)
    }
}

/// Single-API ProviderApi.
pub fn single_api(api_name: &str) -> ProviderApi {
    api_map(&[api_name])
}
