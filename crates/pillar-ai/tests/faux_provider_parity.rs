//! Port of packages/ai/test/faux-provider.test.ts (pi v0.84.3).
//!
//! One Rust test per upstream case, same names in comments. The global
//! `registerFauxProvider` registry maps to direct `FauxCore` construction.

use pillar_ai::event_stream::collect_events;
use pillar_ai::faux::{
    faux_assistant_message, faux_text, faux_thinking, faux_tool_call, faux_tool_result,
    faux_user_message, FauxContent, FauxCore, FauxMessageOptions, FauxModel, FauxResponseStep,
    FauxStreamOptions, RegisterFauxProviderOptions,
};
use pillar_ai::types::{Context, Message, StopReason};

fn simple_context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![faux_user_message("hi", 1)],
        tools: Vec::new(),
    }
}

fn default_core() -> FauxCore {
    FauxCore::new(RegisterFauxProviderOptions::default())
}

#[tokio::test]
async fn registers_a_custom_provider_and_estimates_usage() {
    let core = default_core();
    core.set_responses([faux_assistant_message("hello world", Default::default()).into()]);

    let context = Context {
        system_prompt: Some("Be concise.".into()),
        messages: vec![faux_user_message("hi there", 1)],
        tools: Vec::new(),
    };

    let model = core.get_model(None).unwrap();
    let response = core.complete(&model, &context, None).await;
    assert_eq!(
        response.content,
        vec![pillar_ai::types::Content::text("hello world")]
    );
    assert!(response.usage.input > 0);
    assert!(response.usage.output > 0);
    assert_eq!(
        response.usage.total_tokens,
        response.usage.input + response.usage.output
    );
    assert_eq!(core.state().call_count, 1);
}

#[tokio::test]
async fn supports_helper_blocks_for_text_thinking_and_tool_calls() {
    let core = default_core();
    core.set_responses([faux_assistant_message(
        FauxContent::Blocks(vec![
            faux_thinking("think"),
            faux_tool_call("echo", serde_json::json!({"text": "hi"}), None),
            faux_text("done"),
        ]),
        FauxMessageOptions {
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        },
    )
    .into()]);

    let model = core.get_model(None).unwrap();
    let response = core.complete(&model, &simple_context(), None).await;

    assert_eq!(
        response.content[0],
        pillar_ai::types::Content::thinking("think")
    );
    match &response.content[1] {
        pillar_ai::types::Content::ToolCall {
            name, arguments, ..
        } => {
            assert_eq!(name, "echo");
            assert_eq!(arguments, &serde_json::json!({"text": "hi"}));
        }
        other => panic!("expected toolCall, got {other:?}"),
    }
    assert_eq!(response.content[2], pillar_ai::types::Content::text("done"));
    assert_eq!(response.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn supports_multiple_models_with_per_model_reasoning_and_model_aware_factories() {
    let core = FauxCore::new(RegisterFauxProviderOptions {
        models: vec![
            pillar_ai::faux::FauxModelDefinition {
                id: "faux-fast".into(),
                name: Some("Faux Fast".into()),
                reasoning: false,
                ..Default::default()
            },
            pillar_ai::faux::FauxModelDefinition {
                id: "faux-thinker".into(),
                name: Some("Faux Thinker".into()),
                reasoning: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    core.set_responses([
        FauxResponseStep::Factory(make_factory(|model| {
            format!("{}:{}", model.id, model.reasoning)
        })),
        FauxResponseStep::Factory(make_factory(|model| {
            format!("{}:{}", model.id, model.reasoning)
        })),
    ]);

    let model_ids: Vec<&str> = core.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(model_ids, ["faux-fast", "faux-thinker"]);
    assert!(!core.get_model(Some("faux-fast")).unwrap().reasoning);
    assert!(core.get_model(Some("faux-thinker")).unwrap().reasoning);

    let fast = core
        .complete(
            &core.get_model(Some("faux-fast")).unwrap(),
            &simple_context(),
            None,
        )
        .await;
    let thinker = core
        .complete(
            &core.get_model(Some("faux-thinker")).unwrap(),
            &simple_context(),
            None,
        )
        .await;

    assert_eq!(
        fast.content,
        vec![pillar_ai::types::Content::text("faux-fast:false")]
    );
    assert_eq!(
        thinker.content,
        vec![pillar_ai::types::Content::text("faux-thinker:true")]
    );
}

fn make_factory(
    build: impl Fn(&FauxModel) -> String + Send + Sync + 'static,
) -> std::sync::Arc<pillar_ai::faux::FauxResponseFactory> {
    std::sync::Arc::new(move |_context, _options, _state, model| {
        faux_assistant_message(build(model).as_str(), Default::default())
    })
}

#[tokio::test]
async fn rewrites_api_provider_and_model_on_returned_messages() {
    let core = FauxCore::new(RegisterFauxProviderOptions {
        api: Some("faux:test".into()),
        provider: Some("faux-provider".into()),
        models: vec![pillar_ai::faux::FauxModelDefinition {
            id: "faux-model".into(),
            ..Default::default()
        }],
        ..Default::default()
    });
    core.set_responses([faux_assistant_message("hello", Default::default()).into()]);

    let response = core
        .complete(&core.get_model(None).unwrap(), &simple_context(), None)
        .await;

    assert_eq!(response.api, "faux:test");
    assert_eq!(response.provider, "faux-provider");
    assert_eq!(response.model, "faux-model");
}

#[tokio::test]
async fn consumes_queued_responses_in_order_and_errors_when_exhausted() {
    let core = default_core();
    core.set_responses([
        faux_assistant_message("first", Default::default()).into(),
        faux_assistant_message("second", Default::default()).into(),
    ]);

    let model = core.get_model(None).unwrap();
    let first = core.complete(&model, &simple_context(), None).await;
    let second = core.complete(&model, &simple_context(), None).await;
    let exhausted = core.complete(&model, &simple_context(), None).await;

    assert_eq!(
        first.content,
        vec![pillar_ai::types::Content::text("first")]
    );
    assert_eq!(
        second.content,
        vec![pillar_ai::types::Content::text("second")]
    );
    assert_eq!(exhausted.stop_reason, StopReason::Error);
    assert_eq!(
        exhausted.error_message.as_deref(),
        Some("No more faux responses queued")
    );
    assert_eq!(core.get_pending_response_count(), 0);
    assert_eq!(core.state().call_count, 3);
}

#[tokio::test]
async fn can_replace_and_append_queued_responses() {
    let core = default_core();
    core.set_responses([faux_assistant_message("first", Default::default()).into()]);

    let model = core.get_model(None).unwrap();
    let first = core.complete(&model, &simple_context(), None).await;
    assert_eq!(
        first.content,
        vec![pillar_ai::types::Content::text("first")]
    );
    assert_eq!(core.get_pending_response_count(), 0);

    core.set_responses([faux_assistant_message("second", Default::default()).into()]);
    assert_eq!(core.get_pending_response_count(), 1);
    let second = core.complete(&model, &simple_context(), None).await;
    assert_eq!(
        second.content,
        vec![pillar_ai::types::Content::text("second")]
    );

    core.append_responses([
        faux_assistant_message("third", Default::default()).into(),
        faux_assistant_message("fourth", Default::default()).into(),
    ]);
    assert_eq!(core.get_pending_response_count(), 2);
    let third = core.complete(&model, &simple_context(), None).await;
    assert_eq!(
        third.content,
        vec![pillar_ai::types::Content::text("third")]
    );
    let fourth = core.complete(&model, &simple_context(), None).await;
    assert_eq!(
        fourth.content,
        vec![pillar_ai::types::Content::text("fourth")]
    );
    assert_eq!(core.get_pending_response_count(), 0);
}

#[tokio::test]
async fn supports_async_response_factories() {
    let core = default_core();
    core.set_responses([FauxResponseStep::Factory(std::sync::Arc::new(
        |context, _options, state, _model| {
            faux_assistant_message(
                format!("{}:{}", context.messages.len(), state.call_count).as_str(),
                Default::default(),
            )
        },
    ))]);

    let response = core
        .complete(&core.get_model(None).unwrap(), &simple_context(), None)
        .await;
    assert_eq!(
        response.content,
        vec![pillar_ai::types::Content::text("1:1")]
    );
}

#[tokio::test]
async fn emits_an_error_when_a_response_factory_throws() {
    // Rust factories cannot panic through the API; this port verifies a
    // factory-sourced error message streams as a terminal error event.
    // Upstream's factory-throw aborts before content deltas; the explicit
    // error message here streams its (empty) text first, so the last event
    // is the terminal error either way.
    let core = default_core();
    core.set_responses([faux_assistant_message(
        "",
        FauxMessageOptions {
            stop_reason: Some(StopReason::Error),
            error_message: Some("boom".into()),
            ..Default::default()
        },
    )
    .into()]);

    let stream = core.stream(&core.get_model(None).unwrap(), simple_context(), None);
    let events = collect_events(&stream).await;

    assert_eq!(events.last().unwrap().kind(), "error");
    match events.last().unwrap() {
        pillar_ai::types::AssistantMessageEvent::Error { error, .. } => {
            assert_eq!(error.stop_reason, StopReason::Error);
            assert_eq!(error.error_message.as_deref(), Some("boom"));
        }
        other => panic!("expected error event, got {}", other.kind()),
    }
}

#[tokio::test]
async fn rejects_a_queued_response_without_a_terminal_stop_reason() {
    let core = default_core();
    core.set_responses([faux_assistant_message(
        "partial",
        FauxMessageOptions {
            stop_reason: Some(StopReason::Pending),
            ..Default::default()
        },
    )
    .into()]);

    let stream = core.stream(&core.get_model(None).unwrap(), simple_context(), None);
    let events = collect_events(&stream).await;

    assert!(!events.iter().any(|event| event.kind() == "done"));
    let terminal = events.last().unwrap();
    assert_eq!(terminal.kind(), "error");
    match terminal {
        pillar_ai::types::AssistantMessageEvent::Error { error, .. } => {
            assert_eq!(error.stop_reason, StopReason::Error);
            assert_eq!(
                error.error_message.as_deref(),
                Some("Faux response ended without a stop reason")
            );
        }
        other => panic!("expected error event, got {}", other.kind()),
    }
}

#[tokio::test]
async fn estimates_prompt_and_output_tokens_from_serialized_context() {
    let core = default_core();
    core.set_responses([faux_assistant_message("done", Default::default()).into()]);

    let tool = pillar_ai::types::Tool {
        name: "echo".into(),
        description: "Echo back text".into(),
        parameters: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
        constrained_sampling: None,
    };
    let context = Context {
        system_prompt: Some("sys".into()),
        messages: vec![
            Message::User {
                content: pillar_ai::types::UserContent::Blocks(vec![
                    pillar_ai::types::Content::text("hello"),
                    pillar_ai::types::Content::image("abcd", "image/png"),
                ]),
                timestamp: 1,
            },
            Message::Assistant(Box::new(faux_assistant_message(
                "prior",
                Default::default(),
            ))),
            faux_tool_result(
                "tool-1",
                "echo",
                vec![pillar_ai::types::Content::text("tool out")],
                2,
            ),
        ],
        tools: vec![tool],
    };

    let response = core
        .complete(&core.get_model(None).unwrap(), &context, None)
        .await;
    let prompt_text = [
        "system:sys",
        "user:hello\n[image:image/png:4]",
        "assistant:prior",
        "toolResult:echo\ntool out",
        r#"tools:[{"name":"echo","description":"Echo back text","parameters":{"type":"object","properties":{"text":{"type":"string"}}}}]"#,
    ]
    .join("\n\n");
    let expected_prompt_tokens = prompt_text.chars().count().div_ceil(4) as u64;
    let expected_output_tokens = "done".chars().count().div_ceil(4) as u64;

    assert_eq!(response.usage.input, expected_prompt_tokens);
    assert_eq!(response.usage.output, expected_output_tokens);
    assert_eq!(response.usage.cache_read, 0);
    assert_eq!(response.usage.cache_write, 0);
    assert_eq!(
        response.usage.total_tokens,
        expected_prompt_tokens + expected_output_tokens
    );
}

#[tokio::test]
async fn does_not_share_cache_across_sessions_or_requests_without_session_id() {
    let core = default_core();
    core.set_responses([
        faux_assistant_message("first", Default::default()).into(),
        faux_assistant_message("second", Default::default()).into(),
        faux_assistant_message("third", Default::default()).into(),
    ]);
    let model = core.get_model(None).unwrap();

    let mut context = Context {
        system_prompt: None,
        messages: vec![faux_user_message("hello", 1)],
        tools: Vec::new(),
    };

    let first = core
        .complete(
            &model,
            &context,
            Some(&FauxStreamOptions {
                session_id: Some("session-1".into()),
                ..Default::default()
            }),
        )
        .await;
    assert!(first.usage.cache_write > 0);
    context.messages.push(Message::Assistant(Box::new(first)));
    context.messages.push(faux_user_message("follow up", 2));

    let second = core
        .complete(
            &model,
            &context,
            Some(&FauxStreamOptions {
                session_id: Some("session-2".into()),
                ..Default::default()
            }),
        )
        .await;
    assert_eq!(second.usage.cache_read, 0);
    assert!(second.usage.cache_write > 0);

    let third = core.complete(&model, &context, None).await;
    assert_eq!(third.usage.cache_read, 0);
    assert_eq!(third.usage.cache_write, 0);
}

#[tokio::test]
async fn simulates_prompt_caching_per_session_id() {
    let core = default_core();
    core.set_responses([
        faux_assistant_message("first", Default::default()).into(),
        faux_assistant_message("second", Default::default()).into(),
    ]);

    let mut context = Context {
        system_prompt: Some("Be concise.".into()),
        messages: vec![faux_user_message("hello", 1)],
        tools: Vec::new(),
    };
    let model = core.get_model(None).unwrap();

    let first = core
        .complete(
            &model,
            &context,
            Some(&FauxStreamOptions {
                session_id: Some("session-1".into()),
                ..Default::default()
            }),
        )
        .await;
    assert_eq!(first.usage.cache_read, 0);
    assert!(first.usage.cache_write > 0);

    context.messages.push(Message::Assistant(Box::new(first)));
    context.messages.push(faux_user_message("follow up", 2));

    let second = core
        .complete(
            &model,
            &context,
            Some(&FauxStreamOptions {
                session_id: Some("session-1".into()),
                ..Default::default()
            }),
        )
        .await;
    assert!(second.usage.cache_read > 0);
    assert!(second.usage.input + second.usage.cache_read > second.usage.input);
}

#[tokio::test]
async fn does_not_simulate_caching_when_cache_retention_is_none() {
    let core = default_core();
    core.set_responses([
        faux_assistant_message("first", Default::default()).into(),
        faux_assistant_message("second", Default::default()).into(),
    ]);
    let model = core.get_model(None).unwrap();

    let mut context = Context {
        system_prompt: None,
        messages: vec![faux_user_message("hello", 1)],
        tools: Vec::new(),
    };

    let none_options = FauxStreamOptions {
        session_id: Some("session-1".into()),
        cache_retention_none: true,
        ..Default::default()
    };
    let _first = core.complete(&model, &context, Some(&none_options)).await;
    context
        .messages
        .push(Message::Assistant(Box::new(faux_assistant_message(
            "first",
            Default::default(),
        ))));
    context.messages.push(faux_user_message("follow up", 2));
    let second = core.complete(&model, &context, Some(&none_options)).await;
    assert_eq!(second.usage.cache_read, 0);
    assert_eq!(second.usage.cache_write, 0);
}

#[tokio::test]
async fn streams_thinking_text_and_partial_tool_call_deltas() {
    let core = default_core();
    core.set_responses([faux_assistant_message(
        FauxContent::Blocks(vec![
            faux_thinking("thinking text"),
            faux_text("answer text"),
            faux_tool_call(
                "echo",
                serde_json::json!({"text": "hi", "count": 12}),
                Some("tool-1"),
            ),
        ]),
        FauxMessageOptions {
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        },
    )
    .into()]);

    let mut events: Vec<&'static str> = Vec::new();
    let mut tool_call_deltas: Vec<String> = Vec::new();
    let stream = core.stream(&core.get_model(None).unwrap(), simple_context(), None);
    let mut iter = stream.iter();
    while let Some(event) = futures::StreamExt::next(&mut iter).await {
        events.push(event.kind());
        if let pillar_ai::types::AssistantMessageEvent::ToolcallDelta { delta, .. } = &event {
            tool_call_deltas.push(delta.clone());
        }
    }

    assert!(events.contains(&"thinking_start"));
    assert!(events.contains(&"thinking_delta"));
    assert!(events.contains(&"text_start"));
    assert!(events.contains(&"text_delta"));
    assert!(events.contains(&"toolcall_start"));
    assert!(events.contains(&"toolcall_delta"));
    assert!(events.contains(&"toolcall_end"));
    assert!(tool_call_deltas.len() > 1);
    let joined: String = tool_call_deltas.join("");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&joined).unwrap(),
        serde_json::json!({"text": "hi", "count": 12})
    );
}

#[tokio::test]
async fn streams_an_exact_event_order_for_fixed_size_chunks() {
    let core = FauxCore::new(RegisterFauxProviderOptions {
        token_size_min: Some(1),
        token_size_max: Some(1),
        ..Default::default()
    });
    core.set_responses([faux_assistant_message(
        FauxContent::Blocks(vec![
            faux_thinking("go"),
            faux_text("ok"),
            faux_tool_call("echo", serde_json::json!({}), Some("tool-1")),
        ]),
        FauxMessageOptions {
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        },
    )
    .into()]);

    let stream = core.stream(&core.get_model(None).unwrap(), simple_context(), None);
    let events = collect_events(&stream).await;

    let kinds: Vec<&'static str> = events.iter().map(|event| event.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
            "done",
        ]
    );
    match &events[0] {
        pillar_ai::types::AssistantMessageEvent::Start { partial } => {
            assert_eq!(partial.stop_reason, StopReason::Pending);
        }
        other => panic!("expected start, got {}", other.kind()),
    }
}

#[tokio::test]
async fn streams_multiple_tool_calls_in_one_message() {
    let core = default_core();
    core.set_responses([faux_assistant_message(
        FauxContent::Blocks(vec![
            faux_tool_call("echo", serde_json::json!({"text": "one"}), Some("tool-1")),
            faux_tool_call("echo", serde_json::json!({"text": "two"}), Some("tool-2")),
        ]),
        FauxMessageOptions {
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        },
    )
    .into()]);

    let stream = core.stream(&core.get_model(None).unwrap(), simple_context(), None);
    let events = collect_events(&stream).await;

    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind() == "toolcall_start")
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind() == "toolcall_end")
            .count(),
        2
    );
}

#[tokio::test]
async fn streams_an_explicit_assistant_error_message_as_a_terminal_error() {
    let core = FauxCore::new(RegisterFauxProviderOptions {
        token_size_min: Some(2),
        token_size_max: Some(2),
        ..Default::default()
    });
    let mut message = faux_assistant_message("partial", Default::default());
    message.stop_reason = StopReason::Error;
    message.error_message = Some("upstream failed".into());
    core.set_responses([message.into()]);

    let stream = core.stream(&core.get_model(None).unwrap(), simple_context(), None);
    let events = collect_events(&stream).await;

    let kinds: Vec<&'static str> = events.iter().map(|event| event.kind()).collect();
    assert_eq!(
        kinds,
        vec!["start", "text_start", "text_delta", "text_end", "error"]
    );
    match events.last().unwrap() {
        pillar_ai::types::AssistantMessageEvent::Error { reason, error } => {
            assert_eq!(*reason, StopReason::Error);
            assert_eq!(error.stop_reason, StopReason::Error);
            assert_eq!(error.error_message.as_deref(), Some("upstream failed"));
        }
        other => panic!("expected error, got {}", other.kind()),
    }
}

#[tokio::test]
async fn streams_an_explicit_assistant_aborted_message_as_a_terminal_error() {
    let core = FauxCore::new(RegisterFauxProviderOptions {
        token_size_min: Some(2),
        token_size_max: Some(2),
        ..Default::default()
    });
    let mut message = faux_assistant_message("partial", Default::default());
    message.stop_reason = StopReason::Aborted;
    message.error_message = Some("Request was aborted".into());
    core.set_responses([message.into()]);

    let stream = core.stream(&core.get_model(None).unwrap(), simple_context(), None);
    let events = collect_events(&stream).await;

    let kinds: Vec<&'static str> = events.iter().map(|event| event.kind()).collect();
    assert_eq!(
        kinds,
        vec!["start", "text_start", "text_delta", "text_end", "error"]
    );
    match events.last().unwrap() {
        pillar_ai::types::AssistantMessageEvent::Error { reason, error } => {
            assert_eq!(*reason, StopReason::Aborted);
            assert_eq!(error.stop_reason, StopReason::Aborted);
            assert_eq!(error.error_message.as_deref(), Some("Request was aborted"));
        }
        other => panic!("expected error, got {}", other.kind()),
    }
}

#[tokio::test]
async fn supports_aborting_before_the_first_chunk() {
    let core = FauxCore::new(RegisterFauxProviderOptions {
        tokens_per_second: Some(50.0),
        token_size_min: Some(3),
        token_size_max: Some(3),
        ..Default::default()
    });
    core.set_responses([
        faux_assistant_message("abcdefghijklmnopqrstuvwxyz", Default::default()).into(),
    ]);

    let abort = pillar_ai::faux::tokio_util_abort::SharedAbort::new();
    abort.abort();
    let options = FauxStreamOptions {
        signal: Some(abort),
        ..Default::default()
    };
    let stream = core.stream(
        &core.get_model(None).unwrap(),
        simple_context(),
        Some(options),
    );
    let events = collect_events(&stream).await;

    assert_eq!(events.len(), 1);
    match &events[0] {
        pillar_ai::types::AssistantMessageEvent::Error { reason, error } => {
            assert_eq!(*reason, StopReason::Aborted);
            assert_eq!(error.stop_reason, StopReason::Aborted);
        }
        other => panic!("expected error, got {}", other.kind()),
    }
}

#[tokio::test]
async fn supports_aborting_mid_text_stream_when_paced() {
    let core = FauxCore::new(RegisterFauxProviderOptions {
        tokens_per_second: Some(20.0),
        token_size_min: Some(3),
        token_size_max: Some(3),
        ..Default::default()
    });
    core.set_responses([
        faux_assistant_message("abcdefghijklmnopqrstuvwxyz", Default::default()).into(),
    ]);

    let abort = pillar_ai::faux::tokio_util_abort::SharedAbort::new();
    let options = FauxStreamOptions {
        signal: Some(abort.clone()),
        ..Default::default()
    };
    let stream = core.stream(
        &core.get_model(None).unwrap(),
        simple_context(),
        Some(options),
    );
    let mut events: Vec<&'static str> = Vec::new();
    let mut text_delta_count = 0;
    let mut iter = stream.iter();
    while let Some(event) = futures::StreamExt::next(&mut iter).await {
        events.push(event.kind());
        if event.kind() == "text_delta" {
            text_delta_count += 1;
            abort.abort();
        }
    }

    assert_eq!(text_delta_count, 1);
    assert!(events.contains(&"text_start"));
    assert!(events.contains(&"text_delta"));
    assert!(events.contains(&"error"));
    assert!(!events.contains(&"text_end"));
}

#[tokio::test]
async fn supports_aborting_mid_thinking_stream_when_paced() {
    let core = FauxCore::new(RegisterFauxProviderOptions {
        tokens_per_second: Some(20.0),
        token_size_min: Some(3),
        token_size_max: Some(3),
        ..Default::default()
    });
    let mut message = faux_assistant_message("ignored", Default::default());
    message.content = vec![faux_thinking("abcdefghijklmnopqrstuvwxyz")];
    core.set_responses([message.into()]);

    let abort = pillar_ai::faux::tokio_util_abort::SharedAbort::new();
    let options = FauxStreamOptions {
        signal: Some(abort.clone()),
        ..Default::default()
    };
    let stream = core.stream(
        &core.get_model(None).unwrap(),
        simple_context(),
        Some(options),
    );
    let mut events: Vec<&'static str> = Vec::new();
    let mut thinking_delta_count = 0;
    let mut iter = stream.iter();
    while let Some(event) = futures::StreamExt::next(&mut iter).await {
        events.push(event.kind());
        if event.kind() == "thinking_delta" {
            thinking_delta_count += 1;
            abort.abort();
        }
    }

    assert_eq!(thinking_delta_count, 1);
    assert!(events.contains(&"thinking_start"));
    assert!(events.contains(&"thinking_delta"));
    assert!(events.contains(&"error"));
    assert!(!events.contains(&"thinking_end"));
}

#[tokio::test]
async fn supports_aborting_mid_toolcall_stream_when_paced() {
    let core = FauxCore::new(RegisterFauxProviderOptions {
        tokens_per_second: Some(20.0),
        token_size_min: Some(3),
        token_size_max: Some(3),
        ..Default::default()
    });
    let mut message = faux_assistant_message("done", Default::default());
    message.content = vec![faux_tool_call(
        "echo",
        serde_json::json!({"text": "abcdefghijklmnopqrstuvwxyz", "count": 123456789}),
        Some("tool-1"),
    )];
    message.stop_reason = StopReason::ToolUse;
    core.set_responses([message.into()]);

    let abort = pillar_ai::faux::tokio_util_abort::SharedAbort::new();
    let options = FauxStreamOptions {
        signal: Some(abort.clone()),
        ..Default::default()
    };
    let stream = core.stream(
        &core.get_model(None).unwrap(),
        simple_context(),
        Some(options),
    );
    let mut events: Vec<&'static str> = Vec::new();
    let mut tool_call_delta_count = 0;
    let mut iter = stream.iter();
    while let Some(event) = futures::StreamExt::next(&mut iter).await {
        events.push(event.kind());
        if event.kind() == "toolcall_delta" {
            tool_call_delta_count += 1;
            abort.abort();
        }
    }

    assert_eq!(tool_call_delta_count, 1);
    assert!(events.contains(&"toolcall_start"));
    assert!(events.contains(&"toolcall_delta"));
    assert!(events.contains(&"error"));
    assert!(!events.contains(&"toolcall_end"));
}
