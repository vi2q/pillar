//! Port of packages/agent/test/proxy.test.ts (pi v0.84.3).

use std::sync::Arc;

use pillar_agent::proxy::{
    ProxyAssistantMessageEvent, ProxySerializableStreamOptions, ProxyStreamOptions, stream_proxy,
};
use pillar_ai::error::AiError;
use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse};
use pillar_ai::types::{AssistantMessageEvent, Context, Model};

/// SSE fixture transport returning a prepared body (upstream
/// `vi.stubGlobal("fetch", ...)`).
struct StubFetch {
    body: String,
}

#[async_trait::async_trait]
impl FetchFn for StubFetch {
    async fn fetch(&self, _request: FetchRequest) -> Result<FetchResponse, AiError> {
        let chunks: Vec<Result<Vec<u8>, AiError>> =
            vec![Ok(self.body.clone().into_bytes()), Ok(Vec::new())];
        Ok(FetchResponse {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
            body: Box::pin(futures::stream::iter(chunks)),
        })
    }
}

fn base_model() -> Model {
    Model {
        id: "gpt-5.4".to_owned(),
        name: "GPT-5.4".to_owned(),
        api: "openai-responses".to_owned(),
        provider: "openai".to_owned(),
        base_url: "https://api.openai.com/v1".to_owned(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_owned()],
        cost: Default::default(),
        context_window: 400_000,
        max_tokens: 128_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn proxy_options(fetch: Arc<dyn FetchFn>) -> ProxyStreamOptions {
    ProxyStreamOptions {
        auth_token: "test-token".to_owned(),
        proxy_url: "https://proxy.example.com".to_owned(),
        signal: None,
        options: ProxySerializableStreamOptions::default(),
        fetch: Some(fetch),
    }
}

fn proxy_event_body(events: &[ProxyAssistantMessageEvent]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "data: {}\n\n",
                serde_json::to_string(event).expect("event json")
            )
        })
        .collect()
}

/// conformance: "preserves tool-call metadata received only on toolcall_end"
#[tokio::test]
async fn preserves_tool_call_metadata_received_only_on_toolcall_end() {
    let usage = serde_json::json!({
        "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}
    });
    let proxy_events = vec![
        ProxyAssistantMessageEvent::Start,
        ProxyAssistantMessageEvent::ToolcallStart {
            content_index: 0,
            id: "call_test|fc_test".to_owned(),
            tool_name: "lookup".to_owned(),
        },
        ProxyAssistantMessageEvent::ToolcallDelta {
            content_index: 0,
            delta: "{\"value\":\"hello\"}".to_owned(),
        },
        ProxyAssistantMessageEvent::ToolcallEnd {
            content_index: 0,
            tool_call: serde_json::json!({
                "type": "toolCall",
                "id": "call_test|fc_test",
                "name": "lookup",
                "arguments": {"value": "hello"},
                "namespace": "dynamic_tools"
            }),
        },
        ProxyAssistantMessageEvent::Done {
            reason: "toolUse".to_owned(),
            usage: serde_json::from_value(usage).expect("usage"),
        },
    ];

    let fetch = Arc::new(StubFetch {
        body: proxy_event_body(&proxy_events),
    });

    let model = base_model();
    let context = Context {
        system_prompt: Some(String::new()),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let stream = stream_proxy(&model, &context, &proxy_options(fetch));

    let mut events = Vec::new();
    use futures::StreamExt;
    let mut iter = stream.iter();
    while let Some(event) = iter.next().await {
        events.push(event);
    }
    let result = stream.result().await;

    let end_event = events.iter().find_map(|event| match event {
        AssistantMessageEvent::ToolcallEnd { tool_call, .. } => Some(tool_call.clone()),
        _ => None,
    });
    let Some(end_event) = end_event else {
        panic!("Expected toolcall_end event");
    };
    match &end_event {
        pillar_ai::types::Content::ToolCall {
            id,
            name,
            arguments,
            namespace,
            ..
        } => {
            assert_eq!(id, "call_test|fc_test");
            assert_eq!(name, "lookup");
            assert_eq!(arguments.get("value"), Some(&serde_json::json!("hello")));
            assert_eq!(namespace.as_deref(), Some("dynamic_tools"));
        }
        other => panic!("Expected toolCall content, got {other:?}"),
    }

    // The final message carries the same metadata (upstream asserts
    // result.content[0]).
    match result.content.first() {
        Some(pillar_ai::types::Content::ToolCall {
            arguments,
            namespace,
            ..
        }) => {
            assert_eq!(arguments.get("value"), Some(&serde_json::json!("hello")));
            assert_eq!(namespace.as_deref(), Some("dynamic_tools"));
        }
        other => panic!("Expected toolCall content in result, got {other:?}"),
    }
    assert_eq!(result.stop_reason, pillar_ai::types::StopReason::ToolUse);
}
