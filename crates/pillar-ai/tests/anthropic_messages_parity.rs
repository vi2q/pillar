//! Port of the upstream anthropic-messages tests (pi v0.84.3) that run
//! without live API keys: anthropic-sse-parsing (SSE repair, initial
//! content, refusal/sensitive stops, usage no-op, unknown events),
//! anthropic-cache-write-1h-cost, and the thinking-disable payload cases
//! (via build_params directly; the SDK client is not ported).

use serde_json::{Value, json};

use pillar_ai::api::anthropic_messages::{
    AnthropicOptions, ConvertMessagesOptions, build_params, convert_messages, convert_tools,
    decode_sse_body, get_anthropic_compat, iterate_anthropic_events, map_stop_reason,
    normalize_tool_call_id, process_anthropic_events, to_claude_code_name,
};
use pillar_ai::event_stream::assistant_message_event_stream;
use pillar_ai::types::{
    AnthropicAllowedFallbackModel, AnthropicMessagesCompat, Content, Context, Message, Model,
    ModelCompat, StopReason, Tool, Usage, UserContent,
};

// --- Helpers -------------------------------------------------------------

fn base_model() -> Model {
    Model {
        id: "claude-haiku-4-5".to_string(),
        name: "Claude Haiku 4.5".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        base_url: "https://api.anthropic.com".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: pillar_ai::types::ModelCost {
            rates: pillar_ai::types::ModelCostRates {
                input: 5.0,
                output: 25.0,
                cache_read: 0.5,
                cache_write: 6.25,
            },
            tiers: None,
        },
        context_window: 200_000,
        max_tokens: 32_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn opus_4_8_model() -> Model {
    Model {
        id: "claude-opus-4-8".to_string(),
        name: "Claude Opus 4.8".to_string(),
        // Generated catalog values (pi.dev/models/anthropic/claude-opus-4-8).
        thinking_level_map: Some(
            [
                (
                    pillar_ai::types::ModelThinkingLevel::Xhigh,
                    Some("xhigh".to_string()),
                ),
                (
                    pillar_ai::types::ModelThinkingLevel::Max,
                    Some("max".to_string()),
                ),
            ]
            .into_iter()
            .collect(),
        ),
        compat: Some(ModelCompat::AnthropicMessages(
            AnthropicMessagesCompat {
                force_adaptive_thinking: Some(true),
                ..Default::default()
            }
            .into(),
        )),
        ..base_model()
    }
}

fn sse_body(events: &[(String, Value)]) -> String {
    let mut body = String::new();
    for (event, data) in events {
        body.push_str(&format!("event: {event}\ndata: {data}\n\n"));
    }
    body
}

fn message_start(id: &str, input: u64, output: u64) -> Value {
    json!({
        "type": "message_start",
        "message": {
            "id": id,
            "usage": {
                "input_tokens": input,
                "output_tokens": output,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
            },
        },
    })
}

fn message_delta(stop_reason: &str, input: u64, output: u64) -> Value {
    json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason },
        "usage": {
            "input_tokens": input,
            "output_tokens": output,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
        },
    })
}

fn text_block_events(text: &str) -> Vec<(String, Value)> {
    vec![
        (
            "message_start".to_string(),
            message_start("msg_test", 12, 0),
        ),
        (
            "content_block_start".to_string(),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            }),
        ),
        (
            "content_block_delta".to_string(),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": text },
            }),
        ),
        (
            "content_block_stop".to_string(),
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta".to_string(),
            message_delta("end_turn", 12, 5),
        ),
        (
            "message_stop".to_string(),
            json!({ "type": "message_stop" }),
        ),
    ]
}

fn hello_context() -> Context {
    Context {
        messages: vec![Message::User {
            content: UserContent::Text("Say hello.".to_string()),
            timestamp: 1,
        }],
        ..Default::default()
    }
}

fn run_events(
    model: &Model,
    context: &Context,
    events: Vec<Value>,
) -> pillar_ai::types::AssistantMessage {
    let mut output = pillar_ai::types::AssistantMessage {
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
        timestamp: 1,
    };
    let stream = assistant_message_event_stream();
    process_anthropic_events(events, &mut output, &stream, model, false, context)
        .expect("stream processes");
    output
}

fn decoded(events: &[(String, Value)]) -> Vec<Value> {
    let body = sse_body(events);
    let sse_events = decode_sse_body(&body);
    iterate_anthropic_events(&sse_events).expect("events iterate")
}

// --- anthropic-sse-parsing.test.ts ---------------------------------------

// "repairs malformed SSE JSON and malformed streamed tool JSON"
#[test]
fn repairs_malformed_sse_json_and_malformed_streamed_tool_json() {
    let model = base_model();
    let context = Context {
        messages: vec![Message::User {
            content: UserContent::Text("Use the edit tool.".to_string()),
            timestamp: 1,
        }],
        tools: vec![Tool {
            name: "edit".to_string(),
            description: "Edit a file.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "text": { "type": "string" },
                },
                "required": ["path", "text"],
            }),
            constrained_sampling: None,
        }],
        ..Default::default()
    };

    // Backslash-escape inside a string plus a literal tab character.
    let malformed_tool_json_delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"A\H\",\"text\":\"col1	col2\"}"}}"#;

    // The malformed delta stays a raw JSON string: the SSE decoder's repair
    // path is what must recover it.
    let body = format!(
        "event: message_start\ndata: {}\n\n\
         event: content_block_start\ndata: {}\n\n\
         event: content_block_delta\ndata: {}\n\n\
         event: content_block_stop\ndata: {}\n\n\
         event: message_delta\ndata: {}\n\n\
         event: message_stop\ndata: {}\n\n",
        message_start("msg_test", 12, 0),
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "tool_use", "id": "toolu_test", "name": "edit", "input": {} },
        }),
        malformed_tool_json_delta,
        json!({ "type": "content_block_stop", "index": 0 }),
        message_delta("tool_use", 12, 5),
        json!({ "type": "message_stop" }),
    );
    let sse_events = decode_sse_body(&body);
    let events = iterate_anthropic_events(&sse_events).expect("events iterate");

    let result = run_events(&model, &context, events);

    assert_eq!(result.stop_reason, StopReason::ToolUse);
    assert_eq!(result.error_message, None);

    let Some(Content::ToolCall { arguments, .. }) = result
        .content
        .iter()
        .find(|block| matches!(block, Content::ToolCall { .. }))
    else {
        panic!("expected toolCall block");
    };
    assert_eq!(arguments, &json!({ "path": "A\\H", "text": "col1\tcol2" }));
}

// "preserves content from content_block_start events"
#[test]
fn preserves_content_from_content_block_start_events() {
    let model = base_model();
    let events = vec![
        (
            "message_start".to_string(),
            message_start("msg_initial_content", 12, 0),
        ),
        (
            "content_block_start".to_string(),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "Initial text" },
            }),
        ),
        (
            "content_block_delta".to_string(),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": " plus delta" },
            }),
        ),
        (
            "content_block_stop".to_string(),
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "content_block_start".to_string(),
            json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": { "type": "thinking", "thinking": "Initial thinking", "signature": "initial signature" },
            }),
        ),
        (
            "content_block_delta".to_string(),
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": { "type": "thinking_delta", "thinking": " plus delta" },
            }),
        ),
        (
            "content_block_delta".to_string(),
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": { "type": "signature_delta", "signature": " plus delta" },
            }),
        ),
        (
            "content_block_stop".to_string(),
            json!({ "type": "content_block_stop", "index": 1 }),
        ),
        (
            "message_delta".to_string(),
            message_delta("end_turn", 12, 5),
        ),
        (
            "message_stop".to_string(),
            json!({ "type": "message_stop" }),
        ),
    ];

    let result = run_events(&model, &hello_context(), decoded(&events));

    assert_eq!(
        result.content,
        vec![
            Content::text("Initial text plus delta"),
            Content::Thinking {
                thinking: "Initial thinking plus delta".to_string(),
                thinking_signature: Some("initial signature plus delta".to_string()),
                redacted: None,
            },
        ]
    );
}

// "preserves refusal stop details from message_delta"
#[test]
fn preserves_refusal_stop_details_from_message_delta() {
    let model = base_model();
    let explanation = "This request triggered restrictions on violative cyber content and was blocked under Anthropic's Usage Policy. To learn more, provide feedback, or request an exemption based on how you use Claude, visit our help center: https://support.claude.com/en/articles/14604842-real-time-cyber-safeguards-on-claude.";
    let events = vec![
        (
            "message_start".to_string(),
            message_start("msg_01XFUDYJgAACzvnptvVoYEL", 412, 0),
        ),
        (
            "message_delta".to_string(),
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": "refusal",
                    "stop_details": { "type": "refusal", "category": "cyber", "explanation": explanation },
                },
                "usage": {
                    "input_tokens": 412,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                },
            }),
        ),
        (
            "message_stop".to_string(),
            json!({ "type": "message_stop" }),
        ),
    ];

    let result = run_events(&model, &hello_context(), decoded(&events));

    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.raw_stop_reason.as_deref(), Some("refusal"));
    assert_eq!(result.error_message.as_deref(), Some(explanation));
}

// "preserves sensitive stop reasons with a descriptive error message"
#[test]
fn preserves_sensitive_stop_reasons_with_a_descriptive_error_message() {
    let model = base_model();
    let events = vec![
        (
            "message_start".to_string(),
            message_start("msg_sensitive", 12, 0),
        ),
        (
            "message_delta".to_string(),
            message_delta("sensitive", 12, 0),
        ),
        (
            "message_stop".to_string(),
            json!({ "type": "message_stop" }),
        ),
    ];

    let result = run_events(&model, &hello_context(), decoded(&events));

    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.raw_stop_reason.as_deref(), Some("sensitive"));
    assert_eq!(
        result.error_message.as_deref(),
        Some("Provider stopped with: sensitive")
    );
}

// "treats message_delta without usage as a no-op for usage accumulation"
#[test]
fn treats_message_delta_without_usage_as_a_no_op_for_usage_accumulation() {
    let model = base_model();
    let events: Vec<(String, Value)> = text_block_events("Hello")
        .into_iter()
        .map(|(event, data)| {
            if event == "message_delta" {
                (
                    event,
                    json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } }),
                )
            } else {
                (event, data)
            }
        })
        .collect();

    let result = run_events(&model, &hello_context(), decoded(&events));

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.error_message, None);
    assert_eq!(result.content, vec![Content::text("Hello")]);
    assert_eq!(result.usage.input, 12);
    assert_eq!(result.usage.total_tokens, 12);
}

// "ignores unknown SSE events after message_stop"
#[test]
fn ignores_unknown_sse_events_after_message_stop() {
    let model = base_model();
    let mut events = text_block_events("Hello");
    events.push(("done".to_string(), json!("[DONE]")));
    events.push(("proxy.stats".to_string(), json!("not json")));

    let result = run_events(&model, &hello_context(), decoded(&events));

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.error_message, None);
    assert_eq!(result.content, vec![Content::text("Hello")]);
}

// --- anthropic-cache-write-1h-cost.test.ts -------------------------------

fn cache_creation_events(cache_creation: Option<Value>) -> Vec<(String, Value)> {
    let mut start_usage = json!({
        "type": "message_start",
        "message": {
            "id": "msg_test",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 0,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 1_000_000u64,
            },
        },
    });
    if let Some(cache_creation) = cache_creation {
        start_usage["message"]["usage"]["cache_creation"] = cache_creation;
    }
    vec![
        ("message_start".to_string(), start_usage),
        (
            "content_block_start".to_string(),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            }),
        ),
        (
            "content_block_delta".to_string(),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "Hi" },
            }),
        ),
        (
            "content_block_stop".to_string(),
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta".to_string(),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 5,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 1_000_000u64,
                },
            }),
        ),
        (
            "message_stop".to_string(),
            json!({ "type": "message_stop" }),
        ),
    ]
}

// claude-opus-4-8: input 5, cacheWrite (5m) 6.25 per Mtok. 1h write = 2x input = 10.
// "prices the 1h portion at 2x input and the rest at the 5m rate"
#[test]
fn prices_the_1h_portion_at_2x_input_and_the_rest_at_the_5m_rate() {
    let model = opus_4_8_model();
    let events = cache_creation_events(Some(json!({
        "ephemeral_5m_input_tokens": 600_000u64,
        "ephemeral_1h_input_tokens": 400_000u64,
    })));
    let result = run_events(&model, &hello_context(), decoded(&events));

    assert_eq!(result.usage.cache_write, 1_000_000);
    assert_eq!(result.usage.cache_write_1h, Some(400_000));
    // 600k * 6.25/Mtok + 400k * 10/Mtok = 3.75 + 4.0 = 7.75
    assert!((result.usage.cost.cache_write - 7.75).abs() < 1e-9);
}

// "falls back to the 5m rate when no breakdown is reported"
#[test]
fn falls_back_to_the_5m_rate_when_no_breakdown_is_reported() {
    let model = opus_4_8_model();
    let events = cache_creation_events(None);
    let result = run_events(&model, &hello_context(), decoded(&events));

    assert_eq!(result.usage.cache_write, 1_000_000);
    assert_eq!(result.usage.cache_write_1h, None);
    // 1M * 6.25/Mtok = 6.25
    assert!((result.usage.cost.cache_write - 6.25).abs() < 1e-9);
}

// --- anthropic-thinking-disable.test.ts (payload cases) ------------------

fn thinking_payload(model: &Model, reasoning: Option<pillar_ai::types::ThinkingLevel>) -> Value {
    // streamSimple maps a reasoning level onto the full options shape before
    // buildParams; replicate that mapping here (upstream streamSimple).
    let mut options = AnthropicOptions {
        api_key: Some("fake-key".to_string()),
        ..Default::default()
    };
    match reasoning {
        None => {
            options.thinking_enabled = Some(false);
        }
        Some(reasoning) => {
            options.thinking_enabled = Some(true);
            let adaptive = matches!(
                model.compat.as_ref(),
                Some(ModelCompat::AnthropicMessages(compat))
                    if compat.force_adaptive_thinking == Some(true)
            );
            if adaptive {
                options.effort = Some(
                    pillar_ai::api::anthropic_messages::map_thinking_level_to_effort(
                        model,
                        Some(reasoning),
                    ),
                );
            }
        }
    }
    let params = build_params(
        model,
        &Context {
            messages: vec![Message::User {
                content: UserContent::Text("Hello".to_string()),
                timestamp: 1,
            }],
            ..Default::default()
        },
        false,
        &options,
    );
    json!({
        "thinking": params.get("thinking").cloned().unwrap_or(Value::Null),
        "output_config": params.get("output_config").cloned().unwrap_or(Value::Null),
    })
}

// "sends thinking.type=disabled for budget-based reasoning models when
// thinking is off"
#[test]
fn sends_thinking_disabled_for_budget_based_reasoning_models() {
    let model = Model {
        id: "claude-sonnet-4-5".to_string(),
        name: "Claude Sonnet 4.5".to_string(),
        ..base_model()
    };
    let payload = thinking_payload(&model, None);
    assert_eq!(payload["thinking"], json!({ "type": "disabled" }));
    assert_eq!(payload["output_config"], Value::Null);
}

// "sends thinking.type=disabled for adaptive reasoning models when thinking
// is off"
#[test]
fn sends_thinking_disabled_for_adaptive_reasoning_models() {
    let model = Model {
        id: "claude-opus-4-6".to_string(),
        name: "Claude Opus 4.6".to_string(),
        compat: Some(ModelCompat::AnthropicMessages(
            AnthropicMessagesCompat {
                force_adaptive_thinking: Some(true),
                ..Default::default()
            }
            .into(),
        )),
        ..base_model()
    };
    let payload = thinking_payload(&model, None);
    assert_eq!(payload["thinking"], json!({ "type": "disabled" }));
    assert_eq!(payload["output_config"], Value::Null);
}

// "sends thinking.type=disabled for Claude Opus 4.8 when thinking is off"
#[test]
fn sends_thinking_disabled_for_claude_opus_4_8() {
    let payload = thinking_payload(&opus_4_8_model(), None);
    assert_eq!(payload["thinking"], json!({ "type": "disabled" }));
    assert_eq!(payload["output_config"], Value::Null);
}

// "omits thinking.type=disabled for Claude Fable 5 when thinking is off"
#[test]
fn omits_thinking_disabled_for_claude_fable_5() {
    // Upstream marks off as unsupported (thinkingLevelMap.off === null).
    let model = Model {
        id: "claude-fable-5".to_string(),
        name: "Claude Fable 5".to_string(),
        thinking_level_map: Some(
            [(pillar_ai::types::ModelThinkingLevel::Off, None)]
                .into_iter()
                .collect(),
        ),
        ..base_model()
    };
    let payload = thinking_payload(&model, None);
    assert_eq!(payload["thinking"], Value::Null);
    assert_eq!(payload["output_config"], Value::Null);
}

// "uses adaptive thinking for Claude Opus 4.8 when reasoning is enabled"
#[test]
fn uses_adaptive_thinking_for_claude_opus_4_8_when_reasoning_is_enabled() {
    let payload = thinking_payload(
        &opus_4_8_model(),
        Some(pillar_ai::types::ThinkingLevel::High),
    );
    assert_eq!(
        payload["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(payload["output_config"], json!({ "effort": "high" }));
}

// "uses adaptive thinking for Claude Sonnet 5 when reasoning is enabled"
#[test]
fn uses_adaptive_thinking_for_claude_sonnet_5_when_reasoning_is_enabled() {
    let model = Model {
        id: "claude-sonnet-5".to_string(),
        name: "Claude Sonnet 5".to_string(),
        compat: Some(ModelCompat::AnthropicMessages(
            AnthropicMessagesCompat {
                force_adaptive_thinking: Some(true),
                ..Default::default()
            }
            .into(),
        )),
        ..base_model()
    };
    let payload = thinking_payload(&model, Some(pillar_ai::types::ThinkingLevel::High));
    assert_eq!(
        payload["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(payload["output_config"], json!({ "effort": "high" }));
}

// "maps xhigh reasoning to effort=xhigh for Claude Opus 4.8"
#[test]
fn maps_xhigh_reasoning_to_effort_xhigh_for_claude_opus_4_8() {
    let payload = thinking_payload(
        &opus_4_8_model(),
        Some(pillar_ai::types::ThinkingLevel::Xhigh),
    );
    assert_eq!(
        payload["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(payload["output_config"], json!({ "effort": "xhigh" }));
}

// --- anthropic-eager-tool-input-compat (payload subset) ------------------

fn lookup_tool() -> Tool {
    Tool {
        name: "lookup".to_string(),
        description: "Look up a value".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
        }),
        constrained_sampling: None,
    }
}

// "sends per-tool eager_input_streaming by default"
#[test]
fn sends_per_tool_eager_input_streaming_by_default() {
    let model = base_model();
    let context = Context {
        messages: vec![Message::User {
            content: UserContent::Text("Use the tool".to_string()),
            timestamp: 1,
        }],
        tools: vec![lookup_tool()],
        ..Default::default()
    };
    let params = build_params(
        &model,
        &context,
        false,
        &AnthropicOptions {
            api_key: Some("test-key".to_string()),
            cache_retention: Some(pillar_ai::types::CacheRetention::None),
            ..Default::default()
        },
    );
    assert_eq!(params["tools"][0]["eager_input_streaming"], json!(true));
}

// "uses the legacy fine-grained tool streaming beta when eager tool input
// streaming is disabled"
#[test]
fn uses_legacy_fine_grained_beta_when_eager_streaming_disabled() {
    let model = Model {
        compat: Some(ModelCompat::AnthropicMessages(
            AnthropicMessagesCompat {
                supports_eager_tool_input_streaming: Some(false),
                ..Default::default()
            }
            .into(),
        )),
        ..base_model()
    };
    let context = Context {
        messages: vec![Message::User {
            content: UserContent::Text("Use the tool".to_string()),
            timestamp: 1,
        }],
        tools: vec![lookup_tool()],
        ..Default::default()
    };
    let params = build_params(
        &model,
        &context,
        false,
        &AnthropicOptions {
            api_key: Some("test-key".to_string()),
            cache_retention: Some(pillar_ai::types::CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(params["tools"][0].get("eager_input_streaming").is_none());
    assert!(
        pillar_ai::api::anthropic_messages::should_use_fine_grained_tool_streaming_beta(
            &model, &context
        )
    );
}

// "does not send the legacy fine-grained tool streaming beta when there are
// no tools"
#[test]
fn omits_legacy_beta_when_there_are_no_tools() {
    let model = Model {
        compat: Some(ModelCompat::AnthropicMessages(
            AnthropicMessagesCompat {
                supports_eager_tool_input_streaming: Some(false),
                ..Default::default()
            }
            .into(),
        )),
        ..base_model()
    };
    let context = hello_context();
    assert!(
        !pillar_ai::api::anthropic_messages::should_use_fine_grained_tool_streaming_beta(
            &model, &context
        )
    );
    let params = build_params(
        &model,
        &context,
        false,
        &AnthropicOptions {
            api_key: Some("test-key".to_string()),
            cache_retention: Some(pillar_ai::types::CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(params.get("tools").is_none());
}

// --- Unit-level parity helpers -------------------------------------------

#[test]
fn normalizes_tool_call_ids_to_anthropic_shape() {
    assert_eq!(
        normalize_tool_call_id("toolu_abc-123_XY"),
        "toolu_abc-123_XY"
    );
    assert_eq!(normalize_tool_call_id("call|weird id"), "call_weird_id");
    let long = "a".repeat(100);
    assert_eq!(normalize_tool_call_id(&long).len(), 64);
}

#[test]
fn maps_stop_reasons_with_details() {
    assert_eq!(
        map_stop_reason("end_turn", None).unwrap(),
        (StopReason::Stop, None)
    );
    assert_eq!(
        map_stop_reason("max_tokens", None).unwrap(),
        (StopReason::Length, None)
    );
    assert_eq!(
        map_stop_reason("tool_use", None).unwrap(),
        (StopReason::ToolUse, None)
    );
    assert_eq!(
        map_stop_reason("refusal", Some(&json!({ "explanation": "no" }))).unwrap(),
        (StopReason::Error, Some("no".to_string()))
    );
    assert_eq!(
        map_stop_reason("refusal", None).unwrap(),
        (
            StopReason::Error,
            Some("The model refused to complete the request".to_string())
        )
    );
    assert_eq!(
        map_stop_reason("pause_turn", None).unwrap(),
        (StopReason::Stop, None)
    );
    assert_eq!(
        map_stop_reason("stop_sequence", None).unwrap(),
        (StopReason::Stop, None)
    );
    assert_eq!(
        map_stop_reason("sensitive", None).unwrap(),
        (
            StopReason::Error,
            Some("Provider stopped with: sensitive".to_string())
        )
    );
    assert!(map_stop_reason("brand_new", None).is_err());
}

#[test]
fn resolves_tool_references_default_by_model() {
    // First-party non-haiku Claude 4.5+ supports tool references.
    let modern = Model {
        id: "claude-sonnet-4-5".to_string(),
        ..base_model()
    };
    assert!(get_anthropic_compat(&modern).supports_tool_references);
    // Haiku rejects client-side tool_reference blocks.
    let haiku = base_model();
    assert!(!get_anthropic_compat(&haiku).supports_tool_references);
    // Third-party providers do not.
    let copilot = Model {
        provider: "github-copilot".to_string(),
        ..modern.clone()
    };
    assert!(!get_anthropic_compat(&copilot).supports_tool_references);
    // Old versions do not.
    let old = Model {
        id: "claude-sonnet-4-0".to_string(),
        ..base_model()
    };
    assert!(!get_anthropic_compat(&old).supports_tool_references);
    // Explicit override wins.
    let forced = Model {
        compat: Some(ModelCompat::AnthropicMessages(
            AnthropicMessagesCompat {
                supports_tool_references: Some(true),
                ..Default::default()
            }
            .into(),
        )),
        ..haiku
    };
    assert!(get_anthropic_compat(&forced).supports_tool_references);
}

#[test]
fn maps_claude_code_tool_names_case_insensitively() {
    assert_eq!(to_claude_code_name("bash"), "Bash");
    assert_eq!(to_claude_code_name("TodoWrite"), "TodoWrite");
    assert_eq!(to_claude_code_name("custom_tool"), "custom_tool");
}

#[test]
fn builds_claude_code_system_prompt_for_oauth() {
    let model = base_model();
    let context = Context {
        system_prompt: Some("Be helpful.".to_string()),
        messages: vec![Message::User {
            content: UserContent::Text("Hello".to_string()),
            timestamp: 1,
        }],
        ..Default::default()
    };
    let cache_control = json!({ "type": "ephemeral" });
    let params = build_params(
        &model,
        &context,
        true,
        &AnthropicOptions {
            api_key: Some("sk-ant-oat-test".to_string()),
            cache_retention: Some(pillar_ai::types::CacheRetention::Short),
            ..Default::default()
        },
    );
    let system = params["system"].as_array().expect("system array");
    assert_eq!(
        system[0]["text"],
        json!(pillar_ai::api::anthropic_messages::CLAUDE_CODE_SYSTEM_PROMPT)
    );
    assert_eq!(system[0]["cache_control"], cache_control);
    assert_eq!(system[1]["text"], json!("Be helpful."));
    assert_eq!(system[1]["cache_control"], cache_control);
}

#[test]
fn adds_fallbacks_only_when_compat_lists_them() {
    let model = Model {
        compat: Some(ModelCompat::AnthropicMessages(
            AnthropicMessagesCompat {
                allowed_fallback_models: Some(vec![AnthropicAllowedFallbackModel {
                    provider: "anthropic".to_string(),
                    model: "claude-opus-4-8".to_string(),
                    cost: None,
                }]),
                ..Default::default()
            }
            .into(),
        )),
        ..base_model()
    };
    let params = build_params(
        &model,
        &hello_context(),
        false,
        &AnthropicOptions {
            api_key: Some("k".to_string()),
            cache_retention: Some(pillar_ai::types::CacheRetention::None),
            ..Default::default()
        },
    );
    assert_eq!(params["fallbacks"], json!([{ "model": "claude-opus-4-8" }]));

    let plain = build_params(
        &base_model(),
        &hello_context(),
        false,
        &AnthropicOptions {
            api_key: Some("k".to_string()),
            cache_retention: Some(pillar_ai::types::CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(plain.get("fallbacks").is_none());
}

#[test]
fn converts_tools_with_strict_overlay_and_defer_loading() {
    let tool = Tool {
        name: "lookup".to_string(),
        description: "Look up a value".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false,
        }),
        constrained_sampling: Some(pillar_ai::types::ConstrainedSamplingConfig::JsonSchema {
            strict: pillar_ai::types::ConstrainedStrictness::Prefer,
        }),
    };
    let tools = convert_tools(std::slice::from_ref(&tool), false, true, true, None, true);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["strict"], json!(true));
    assert_eq!(tools[0]["defer_loading"], json!(true));
    assert_eq!(tools[0]["eager_input_streaming"], json!(true));
    assert_eq!(tools[0]["input_schema"]["required"], json!(["value"]));
}

#[test]
fn batches_consecutive_tool_results_with_reference_displacement() {
    let model = base_model();
    let normalize = |name: &str| name.to_string();
    let deferred: std::collections::BTreeSet<String> =
        ["deferred_tool".to_string()].into_iter().collect();
    let assistant = pillar_ai::types::AssistantMessage {
        content: vec![Content::tool_call("toolu_1", "deferred_tool", json!({}))],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    };
    let tool_result = Message::ToolResult(Box::new(pillar_ai::types::ToolResultMessage {
        tool_call_id: "toolu_1".to_string(),
        tool_name: "deferred_tool".to_string(),
        content: vec![Content::text("loaded")],
        details: None,
        usage: None,
        added_tool_names: Some(vec!["deferred_tool".to_string()]),
        is_error: false,
        timestamp: 1,
    }));
    let params = convert_messages(
        &[Message::Assistant(Box::new(assistant)), tool_result],
        ConvertMessagesOptions {
            is_oauth_token: false,
            cache_control: None,
            allow_empty_signature: false,
            deferred_tool_names: &deferred,
            normalize_tool_name: &normalize,
        },
    );
    // The tool_result block carries references; the ordinary content moves
    // into a sibling text block after it.
    let user = params
        .iter()
        .find(|param| param["role"] == json!("user"))
        .expect("user message");
    let content = user["content"].as_array().expect("content array");
    assert_eq!(content[0]["type"], json!("tool_result"));
    assert_eq!(content[0]["content"][0]["type"], json!("tool_reference"));
    assert_eq!(
        content[0]["content"][0]["tool_name"],
        json!("deferred_tool")
    );
    assert_eq!(content[1]["type"], json!("text"));
    assert_eq!(content[1]["text"], json!("loaded"));
}

// --- stream() e2e over a mock transport ----------------------------------

use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse};

type CapturedRequests = std::sync::Arc<tokio::sync::Mutex<Vec<FetchRequest>>>;

/// Transport answering with a canned Anthropic SSE body and recording the
/// request (headers + JSON body) for assertions.
struct MockAnthropicSse {
    body: String,
    status: u16,
    captured: CapturedRequests,
}

impl MockAnthropicSse {
    fn new(body: String) -> (Self, CapturedRequests) {
        let captured: CapturedRequests = std::sync::Arc::default();
        (
            Self {
                body,
                status: 200,
                captured: std::sync::Arc::clone(&captured),
            },
            captured,
        )
    }
}

#[async_trait::async_trait]
impl FetchFn for MockAnthropicSse {
    async fn fetch(
        &self,
        request: FetchRequest,
    ) -> Result<FetchResponse, pillar_ai::error::AiError> {
        self.captured.lock().await.push(request);
        let chunks: Vec<Result<Vec<u8>, pillar_ai::error::AiError>> =
            vec![Ok(self.body.clone().into_bytes()), Ok(Vec::new())];
        Ok(FetchResponse {
            status: self.status,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: Box::pin(futures::stream::iter(chunks)),
        })
    }
}

fn sse_body_of(events: &[(String, Value)]) -> String {
    let mut body = String::new();
    for (event, data) in events {
        body.push_str(&format!("event: {event}\ndata: {data}\n\n"));
    }
    body
}

#[tokio::test]
async fn streams_over_mock_transport_and_sends_expected_request() {
    let model = base_model();
    let body = sse_body_of(&text_block_events("Hello"));
    let (mock, captured) = MockAnthropicSse::new(body);

    let s = pillar_ai::api::anthropic_messages::stream(
        model.clone(),
        hello_context(),
        Some(AnthropicOptions {
            api_key: Some("test-key".to_string()),
            fetch: Some(std::sync::Arc::new(mock)),
            ..Default::default()
        }),
    );
    let _events = pillar_ai::event_stream::collect_events(&s).await;
    let result = s.result().await;

    assert_eq!(
        result.stop_reason,
        StopReason::Stop,
        "{:?}",
        result.error_message
    );
    assert_eq!(result.content, vec![Content::text("Hello")]);

    let requests = captured.lock().await;
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.url, "https://api.anthropic.com/v1/messages");
    assert!(
        request
            .headers
            .iter()
            .any(|(name, value)| name == "x-api-key" && value == "test-key"),
        "headers: {:?}",
        request.headers
    );
    let sent: Value = serde_json::from_slice(request.body.as_deref().expect("body")).expect("json");
    assert_eq!(sent["model"], json!("claude-haiku-4-5"));
    assert_eq!(sent["stream"], json!(true));
    assert_eq!(sent["max_tokens"], json!(model.max_tokens));
}
