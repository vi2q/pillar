//! Port of the upstream google-* tests (pi v0.84.3) that run without live
//! API keys: google-shared-convert-tools, google-shared-gemini3-unsigned-
//! tool-call, google-shared-image-tool-result-routing, google-shared-signed-
//! empty-blocks, google-thinking-signature, google-thinking-level-map
//! (unit part) and google-raw-stop-reason (mocked-transport part).
//! One Rust test per upstream test case, same names in comments.

use pillar_ai::api::google_shared::{
    GeminiContent, GeminiPart, ResolvedGoogleThinkingLevel, convert_messages, convert_tools,
    is_thinking_part, map_stop_reason_string, requires_tool_call_id,
    resolve_google_function_calling_mode, resolve_google_thinking_level, retain_thought_signature,
    supports_google_strict_tool_sampling,
};
use pillar_ai::error::AiError;
use pillar_ai::types::{
    AssistantMessage, Content, Context, Message, Model, ModelThinkingLevel, StopReason, Tool,
    ToolResultMessage, UserContent,
};
use serde_json::{Value, json};

// --- Helpers ---------------------------------------------------------------

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn make_tool(parameters: Value) -> Tool {
    Tool {
        name: "test_tool".to_string(),
        description: "A test tool".to_string(),
        parameters,
        constrained_sampling: None,
    }
}

fn make_model(api: &str, provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: api.to_string(),
        provider: provider.to_string(),
        base_url: "https://example.com".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: Default::default(),
        context_window: 128_000,
        max_tokens: 8192,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn gemini3_model(api: &str, provider: &str, id: &str) -> Model {
    make_model(api, provider, id)
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::Text(text.to_string()),
        timestamp: now_ms(),
    }
}

fn assistant_with_tool_calls(
    api: &str,
    provider: &str,
    model_id: &str,
    calls: Vec<Content>,
) -> Message {
    Message::Assistant(Box::new(AssistantMessage {
        content: calls,
        api: api.to_string(),
        provider: provider.to_string(),
        model: model_id.to_string(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Default::default(),
        stop_reason: StopReason::ToolUse,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
    }))
}

fn tool_result(id: &str, name: &str, content: Vec<Content>) -> Message {
    Message::ToolResult(Box::new(ToolResultMessage {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        content,
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: now_ms(),
    }))
}

fn function_call_ids(contents: &[GeminiContent]) -> Vec<String> {
    contents
        .iter()
        .flat_map(|c| c.parts.iter())
        .filter_map(|p| p.function_call.as_ref().and_then(|fc| fc.id.clone()))
        .collect()
}

fn function_response_ids(contents: &[GeminiContent]) -> Vec<String> {
    contents
        .iter()
        .flat_map(|c| c.parts.iter())
        .filter_map(|p| p.function_response.as_ref().and_then(|fr| fr.id.clone()))
        .collect()
}

// --- google-shared-convert-tools.test.ts -----------------------------------

#[test]
fn strips_json_schema_meta_keys_from_parameters_when_use_parameters_true() {
    let tools = [make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$id": "urn:bash-tool",
        "$comment": "A bash tool for demonstration",
        "$defs": { "commandDef": { "type": "string" } },
        "definitions": { "legacyDef": { "type": "number" } },
        "type": "object",
        "properties": { "command": { "type": "string" } },
        "required": ["command"],
    }))];

    let binding = convert_tools(&tools, true, true).expect("convert_tools");
    let binding = binding.expect("non-empty");
    let decl = binding
        .get("functionDeclarations")
        .and_then(|d| d.as_array())
        .and_then(|d| d.first())
        .and_then(|d| d.as_object())
        .expect("declaration");

    let parameters = decl.get("parameters").expect("parameters");
    assert_eq!(
        parameters,
        &json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"],
        })
    );
    for meta in ["$schema", "$id", "$comment", "$defs", "definitions"] {
        assert!(decl.get("parameters").unwrap().get(meta).is_none());
    }
}

#[test]
fn recursively_strips_nested_json_schema_meta_keys() {
    let tools = [make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "deep": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "$id": "urn:nested",
                "type": "string",
            },
        },
    }))];

    let result = convert_tools(&tools, true, true).expect("convert_tools");
    let decl = result.expect("non-empty")["functionDeclarations"][0].clone();

    assert_eq!(
        decl["parameters"],
        json!({
            "type": "object",
            "properties": { "deep": { "type": "string" } },
        })
    );
}

#[test]
fn preserves_ref_while_stripping_meta_keys() {
    let tools = [make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "refProp": {
                "$ref": "#/$defs/someDef",
                "type": "string",
            },
        },
    }))];

    let result = convert_tools(&tools, true, true).expect("convert_tools");
    let decl = result.expect("non-empty")["functionDeclarations"][0].clone();

    assert_eq!(
        decl["parameters"],
        json!({
            "type": "object",
            "properties": {
                "refProp": { "$ref": "#/$defs/someDef", "type": "string" },
            },
        })
    );
}

#[test]
fn does_not_mutate_the_original_tool_parameters_object() {
    let original_parameters = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { "command": { "type": "string" } },
        "required": ["command"],
    });
    let original_clone = original_parameters.clone();
    let tools = [make_tool(original_parameters)];

    let _ = convert_tools(&tools, true, true).expect("convert_tools");

    assert_eq!(
        tools[0].parameters, original_clone,
        "original parameters must not be mutated"
    );
}

#[test]
fn preserves_schema_in_parameters_json_schema_when_use_parameters_false() {
    let tools = [make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { "command": { "type": "string" } },
        "required": ["command"],
    }))];

    let result = convert_tools(&tools, false, true).expect("convert_tools");
    let decl = result.expect("non-empty")["functionDeclarations"][0].clone();

    assert_eq!(
        decl["parametersJsonSchema"],
        json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"],
        })
    );
}

#[test]
fn handles_tools_without_schema_gracefully() {
    let tools = [make_tool(json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"],
    }))];

    let result = convert_tools(&tools, true, true).expect("convert_tools");
    let decl = result.expect("non-empty")["functionDeclarations"][0].clone();

    assert_eq!(
        decl["parameters"],
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        })
    );
}

#[test]
fn uses_validated_function_calling_for_strict_tools_on_gemini_3() {
    let mut tool = make_tool(json!({ "type": "object", "properties": {} }));
    tool.constrained_sampling = Some(pillar_ai::types::ConstrainedSamplingConfig::JsonSchema {
        strict: pillar_ai::types::ConstrainedStrictness::Require,
    });

    assert!(supports_google_strict_tool_sampling(
        "gemini-3.1-pro-preview"
    ));
    assert!(!supports_google_strict_tool_sampling("gemini-2.5-pro"));

    let mode =
        resolve_google_function_calling_mode(&[tool.clone()], None, true).expect("resolve mode");
    assert_eq!(
        mode,
        Some(pillar_ai::api::google_shared::FunctionCallingConfigMode::Validated)
    );

    // With strict tools unsupported, resolve must throw (upstream throws).
    let result = resolve_google_function_calling_mode(&[tool], None, false);
    match result {
        Err(AiError::Other(message)) => {
            assert!(
                message.contains("requires JSON-schema constrained sampling"),
                "{message}"
            );
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[test]
fn returns_undefined_for_empty_tool_list() {
    let empty: [Tool; 0] = [];
    assert!(
        convert_tools(&empty, false, true)
            .expect("convert_tools")
            .is_none()
    );
    assert!(
        convert_tools(&empty, true, true)
            .expect("convert_tools")
            .is_none()
    );
}

// --- google-shared-gemini3-unsigned-tool-call.test.ts ------------------------

fn unsigned_tool_call_context(
    api: &str,
    provider: &str,
    model_id: &str,
    sig: Option<&str>,
) -> Context {
    Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![
            user_message("Hi"),
            assistant_with_tool_calls(
                api,
                provider,
                model_id,
                vec![
                    Content::ToolCall {
                        id: "call_1".to_string(),
                        name: "bash".to_string(),
                        arguments: json!({ "command": "echo hi" }),
                        thought_signature: sig.map(|s| s.to_string()),
                        namespace: None,
                    },
                    Content::ToolCall {
                        id: "call_2".to_string(),
                        name: "bash".to_string(),
                        arguments: json!({ "command": "ls -la" }),
                        thought_signature: None,
                        namespace: None,
                    },
                ],
            ),
            tool_result("call_1", "bash", vec![Content::text("hi")]),
            tool_result("call_2", "bash", vec![Content::text("files")]),
        ],
    }
}

#[test]
fn preserves_tool_call_ids_for_gemini3_history() {
    for model in [
        gemini3_model("google-generative-ai", "google", "gemini-3-pro-preview"),
        gemini3_model("google-generative-ai", "google", "gemini-3.6-flash"),
        gemini3_model("google-vertex", "google-vertex", "gemini-3-pro-preview"),
    ] {
        let context = unsigned_tool_call_context(&model.api, &model.provider, &model.id, None);
        let contents = convert_messages(&model, &context);

        assert_eq!(
            function_call_ids(&contents),
            vec!["call_1".to_string(), "call_2".to_string()],
            "model {}",
            model.id
        );
        assert_eq!(
            function_response_ids(&contents),
            vec!["call_1".to_string(), "call_2".to_string()],
            "model {}",
            model.id
        );
    }
}

#[test]
fn does_not_add_skip_thought_signature_validator_for_unsigned_google_gen_ai_tool_calls() {
    let model = gemini3_model("google-generative-ai", "google", "gemini-3-pro-preview");
    let context = unsigned_tool_call_context(&model.api, &model.provider, "other-model", None);

    let contents = convert_messages(&model, &context);
    let model_turn = contents
        .iter()
        .find(|c| c.role.as_deref() == Some("model"))
        .expect("model turn");

    let function_call_parts: Vec<&GeminiPart> = model_turn
        .parts
        .iter()
        .filter(|p| p.function_call.is_some())
        .collect();
    assert_eq!(function_call_parts.len(), 2);
    assert!(function_call_parts[0].thought_signature.is_none());
    assert!(function_call_parts[1].thought_signature.is_none());

    let serialized = serde_json::to_string(model_turn).unwrap();
    assert!(!serialized.contains("skip_thought_signature_validator"));

    // No historical context text from the other model (converted without tags).
    let historical: Vec<_> = model_turn
        .parts
        .iter()
        .filter(|p| {
            p.text
                .as_deref()
                .map(|t| t.contains("Historical context"))
                .unwrap_or(false)
        })
        .collect();
    assert!(historical.is_empty());
}

#[test]
fn does_not_add_skip_thought_signature_validator_for_unsigned_vertex_tool_calls() {
    let model = gemini3_model("google-vertex", "google-vertex", "gemini-3-pro-preview");
    let context = unsigned_tool_call_context(&model.api, &model.provider, &model.id, None);

    let contents = convert_messages(&model, &context);
    let model_turn = contents
        .iter()
        .find(|c| c.role.as_deref() == Some("model"))
        .expect("model turn");
    let function_call_parts: Vec<&GeminiPart> = model_turn
        .parts
        .iter()
        .filter(|p| p.function_call.is_some())
        .collect();

    assert_eq!(function_call_parts.len(), 2);
    assert!(function_call_parts[0].thought_signature.is_none());
    assert!(function_call_parts[1].thought_signature.is_none());
    let serialized = serde_json::to_string(model_turn).unwrap();
    assert!(!serialized.contains("skip_thought_signature_validator"));
}

#[test]
fn preserves_valid_thought_signature_when_present_for_the_same_provider_and_model() {
    let model = gemini3_model("google-generative-ai", "google", "gemini-3-pro-preview");
    let valid_sig = "AAAAAAAAAAAAAAAAAAAAAA==";
    let context =
        unsigned_tool_call_context(&model.api, &model.provider, &model.id, Some(valid_sig));

    let contents = convert_messages(&model, &context);
    let model_turn = contents
        .iter()
        .find(|c| c.role.as_deref() == Some("model"))
        .expect("model turn");
    let function_call_parts: Vec<&GeminiPart> = model_turn
        .parts
        .iter()
        .filter(|p| p.function_call.is_some())
        .collect();

    assert_eq!(function_call_parts.len(), 2);
    assert_eq!(
        function_call_parts[0].thought_signature.as_deref(),
        Some(valid_sig)
    );
    assert!(function_call_parts[1].thought_signature.is_none());
}

#[test]
fn does_not_add_a_thought_signature_for_non_gemini_3_models() {
    let model = gemini3_model("google-generative-ai", "google", "gemini-2.5-flash");
    let context = unsigned_tool_call_context(&model.api, &model.provider, "other-model", None);

    let contents = convert_messages(&model, &context);
    let model_turn = contents
        .iter()
        .find(|c| c.role.as_deref() == Some("model"))
        .expect("model turn");
    let function_call_parts: Vec<&GeminiPart> = model_turn
        .parts
        .iter()
        .filter(|p| p.function_call.is_some())
        .collect();
    let function_response_parts: Vec<&GeminiPart> = contents
        .iter()
        .flat_map(|c| c.parts.iter())
        .filter(|p| p.function_response.is_some())
        .collect();

    assert_eq!(function_call_parts.len(), 2);
    assert!(
        function_call_parts
            .iter()
            .all(|p| p.function_call.as_ref().unwrap().id.is_none())
    );
    assert!(
        function_call_parts
            .iter()
            .all(|p| p.thought_signature.is_none())
    );
    assert_eq!(function_response_parts.len(), 2);
    assert!(
        function_response_parts
            .iter()
            .all(|p| p.function_response.as_ref().unwrap().id.is_none())
    );
}

#[test]
fn requires_tool_call_id_table() {
    // "returns false for gemini-2.5-flash" / true for the others
    assert!(!requires_tool_call_id("gemini-2.5-flash"));
    assert!(requires_tool_call_id("gemini-3.6-flash"));
    assert!(requires_tool_call_id("claude-sonnet-4-5"));
    assert!(requires_tool_call_id("gpt-oss-120b"));
}

// --- google-shared-image-tool-result-routing.test.ts -------------------------

fn image_routing_context(api: &str, provider: &str, model_id: &str) -> Context {
    Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![
            user_message("read the files"),
            assistant_with_tool_calls(
                api,
                provider,
                model_id,
                vec![
                    Content::ToolCall {
                        id: "call_a".to_string(),
                        name: "read".to_string(),
                        arguments: json!({ "path": "a.txt" }),
                        thought_signature: None,
                        namespace: None,
                    },
                    Content::ToolCall {
                        id: "call_img".to_string(),
                        name: "read".to_string(),
                        arguments: json!({ "path": "image.png" }),
                        thought_signature: None,
                        namespace: None,
                    },
                    Content::ToolCall {
                        id: "call_b".to_string(),
                        name: "read".to_string(),
                        arguments: json!({ "path": "b.txt" }),
                        thought_signature: None,
                        namespace: None,
                    },
                ],
            ),
            tool_result("call_a", "read", vec![Content::text("alpha text")]),
            tool_result(
                "call_img",
                "read",
                vec![Content::Image {
                    data: "abc".to_string(),
                    mime_type: "image/png".to_string(),
                }],
            ),
            tool_result("call_b", "read", vec![Content::text("beta text")]),
        ],
    }
}

fn image_routing_model(id: &str) -> Model {
    let mut m = make_model("google-generative-ai", "google", id);
    m.input = vec!["text".to_string(), "image".to_string()];
    m
}

#[test]
fn keeps_separate_synthetic_image_turn_for_gemini_2_x_google_api_models() {
    let model = image_routing_model("gemini-2.5-flash");
    let contents = convert_messages(
        &model,
        &image_routing_context(&model.api, &model.provider, &model.id),
    );

    assert_eq!(contents.len(), 5);
    assert!(
        contents[2]
            .parts
            .iter()
            .all(|p| p.function_response.is_some())
    );
    assert_eq!(
        contents[3].parts[0].text.as_deref(),
        Some("Tool result image:")
    );
    assert!(contents[3].parts[1].inline_data.is_some());
    assert!(contents[4].parts[0].function_response.is_some());
}

#[test]
fn nests_image_tool_results_for_gemini_3_google_api_models() {
    let model = image_routing_model("gemini-3-pro-preview");
    let contents = convert_messages(
        &model,
        &image_routing_context(&model.api, &model.provider, &model.id),
    );

    assert_eq!(contents.len(), 3);
    let tool_result_turn = &contents[2];
    assert_eq!(tool_result_turn.parts.len(), 3);
    let image_response = tool_result_turn.parts[1]
        .function_response
        .as_ref()
        .expect("image response");
    let parts = image_response.parts.as_ref().expect("nested parts");
    assert_eq!(parts.len(), 1);
    assert!(parts[0].inline_data.is_some());
}

// --- google-shared-signed-empty-blocks.test.ts -------------------------------

const VALID_SIG: &str = "AAAAAAAAAAAAAAAAAAAAAA==";

fn signed_empty_context(model_id: &str, content: Vec<Content>) -> Context {
    Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![
            user_message("Hi"),
            assistant_with_tool_calls("google-generative-ai", "google", model_id, content),
        ],
    }
}

#[test]
fn keeps_a_signed_empty_thinking_block_so_its_signature_is_echoed_back() {
    let model = make_model("google-generative-ai", "google", "gemini-3-pro-preview");
    let contents = convert_messages(
        &model,
        &signed_empty_context(
            &model.id,
            vec![
                Content::Thinking {
                    thinking: String::new(),
                    thinking_signature: Some(VALID_SIG.to_string()),
                    redacted: None,
                },
                Content::ToolCall {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({ "command": "ls" }),
                    thought_signature: None,
                    namespace: None,
                },
            ],
        ),
    );
    let model_turn = contents
        .iter()
        .find(|c| c.role.as_deref() == Some("model"))
        .expect("model turn");
    let signed: Vec<&GeminiPart> = model_turn
        .parts
        .iter()
        .filter(|p| p.thought_signature.as_deref() == Some(VALID_SIG))
        .collect();
    assert_eq!(signed.len(), 1);
    assert_eq!(signed[0].thought, Some(true));
}

#[test]
fn keeps_a_signed_empty_text_block_the_same_way() {
    let model = make_model("google-generative-ai", "google", "gemini-3-pro-preview");
    let contents = convert_messages(
        &model,
        &signed_empty_context(
            &model.id,
            vec![
                Content::Text {
                    text: String::new(),
                    text_signature: Some(VALID_SIG.to_string()),
                },
                Content::ToolCall {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({ "command": "ls" }),
                    thought_signature: None,
                    namespace: None,
                },
            ],
        ),
    );
    let model_turn = contents
        .iter()
        .find(|c| c.role.as_deref() == Some("model"))
        .expect("model turn");
    let signed: Vec<&GeminiPart> = model_turn
        .parts
        .iter()
        .filter(|p| p.thought_signature.as_deref() == Some(VALID_SIG))
        .collect();
    assert_eq!(signed.len(), 1);
}

#[test]
fn still_drops_unsigned_empty_blocks() {
    let model = make_model("google-generative-ai", "google", "gemini-3-pro-preview");
    let contents = convert_messages(
        &model,
        &signed_empty_context(
            &model.id,
            vec![
                Content::Thinking {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                },
                Content::Text {
                    text: "   ".to_string(),
                    text_signature: None,
                },
                Content::ToolCall {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({ "command": "ls" }),
                    thought_signature: None,
                    namespace: None,
                },
            ],
        ),
    );
    let model_turn = contents
        .iter()
        .find(|c| c.role.as_deref() == Some("model"))
        .expect("model turn");
    assert_eq!(model_turn.parts.len(), 1);
    assert!(model_turn.parts[0].function_call.is_some());
}

#[test]
fn still_drops_signed_empty_blocks_from_a_different_provider_model() {
    let model = make_model("google-generative-ai", "google", "gemini-3-pro-preview");
    let contents = convert_messages(
        &model,
        &signed_empty_context(
            "other-model",
            vec![
                Content::Thinking {
                    thinking: String::new(),
                    thinking_signature: Some(VALID_SIG.to_string()),
                    redacted: None,
                },
                Content::Text {
                    text: String::new(),
                    text_signature: Some(VALID_SIG.to_string()),
                },
                Content::ToolCall {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({ "command": "ls" }),
                    thought_signature: None,
                    namespace: None,
                },
            ],
        ),
    );
    let model_turn = contents
        .iter()
        .find(|c| c.role.as_deref() == Some("model"))
        .expect("model turn");
    assert_eq!(model_turn.parts.len(), 1);
    assert!(model_turn.parts[0].function_call.is_some());
    let serialized = serde_json::to_string(model_turn).unwrap();
    assert!(!serialized.contains(VALID_SIG));
}

// --- google-thinking-signature.test.ts ---------------------------------------

#[test]
fn treats_part_thought_true_as_thinking() {
    let part = GeminiPart {
        thought: Some(true),
        thought_signature: Some("opaque-signature".to_string()),
        ..Default::default()
    };
    assert!(is_thinking_part(&part));
    let plain = GeminiPart {
        thought: Some(true),
        ..Default::default()
    };
    assert!(is_thinking_part(&plain));
}

#[test]
fn does_not_treat_thought_signature_alone_as_thinking() {
    // Per Google docs, thoughtSignature is for context replay and can appear on any part type.
    // Only thought === true indicates thinking content.
    // See: https://ai.google.dev/gemini-api/docs/thought-signatures
    let part = GeminiPart {
        thought_signature: Some("opaque-signature".to_string()),
        ..Default::default()
    };
    assert!(!is_thinking_part(&part));
    let part_false = GeminiPart {
        thought: Some(false),
        thought_signature: Some("opaque-signature".to_string()),
        ..Default::default()
    };
    assert!(!is_thinking_part(&part_false));
}

#[test]
fn does_not_treat_empty_or_missing_signatures_as_thinking_if_thought_is_not_set() {
    let part = GeminiPart::default();
    assert!(!is_thinking_part(&part));
    let part2 = GeminiPart {
        thought: Some(false),
        thought_signature: Some(String::new()),
        ..Default::default()
    };
    assert!(!is_thinking_part(&part2));
}

#[test]
fn preserves_the_existing_signature_when_subsequent_deltas_omit_thought_signature() {
    let first = retain_thought_signature(None, Some("sig-1"));
    assert_eq!(first.as_deref(), Some("sig-1"));

    let second = retain_thought_signature(first.as_deref(), None);
    assert_eq!(second.as_deref(), Some("sig-1"));

    let third = retain_thought_signature(second.as_deref(), Some(""));
    assert_eq!(third.as_deref(), Some("sig-1"));
}

#[test]
fn updates_the_signature_when_a_new_non_empty_signature_arrives() {
    let updated = retain_thought_signature(Some("sig-1"), Some("sig-2"));
    assert_eq!(updated.as_deref(), Some("sig-2"));
}

// --- google-thinking-level-map.test.ts (unit part) ----------------------------

#[test]
fn exhaustively_resolves_supported_logical_levels_and_mapping_values() {
    // Default expectations (no map): off -> high, identity otherwise.
    assert_eq!(
        resolve_google_thinking_level(
            &make_model("google-generative-ai", "test-google", "gemini-3.7-flash"),
            ModelThinkingLevel::Off
        )
        .unwrap(),
        ResolvedGoogleThinkingLevel::High
    );
    for (level, expected) in [
        (
            ModelThinkingLevel::Minimal,
            ResolvedGoogleThinkingLevel::Minimal,
        ),
        (ModelThinkingLevel::Low, ResolvedGoogleThinkingLevel::Low),
        (
            ModelThinkingLevel::Medium,
            ResolvedGoogleThinkingLevel::Medium,
        ),
        (ModelThinkingLevel::High, ResolvedGoogleThinkingLevel::High),
    ] {
        assert_eq!(
            resolve_google_thinking_level(
                &make_model("google-generative-ai", "test-google", "gemini-3.7-flash"),
                level
            )
            .unwrap(),
            expected
        );
    }

    // Mapped expectations: uppercase provider values resolve case-insensitively.
    for (mapped, expected) in [
        ("minimal", ResolvedGoogleThinkingLevel::Minimal),
        ("low", ResolvedGoogleThinkingLevel::Low),
        ("medium", ResolvedGoogleThinkingLevel::Medium),
        ("high", ResolvedGoogleThinkingLevel::High),
        ("MINIMAL", ResolvedGoogleThinkingLevel::Minimal),
        ("LOW", ResolvedGoogleThinkingLevel::Low),
        ("MEDIUM", ResolvedGoogleThinkingLevel::Medium),
        ("HIGH", ResolvedGoogleThinkingLevel::High),
    ] {
        let mut model = make_model("google-generative-ai", "test-google", "gemini-3.7-flash");
        model.thinking_level_map = Some(
            [
                (ModelThinkingLevel::High, Some(mapped.to_string())),
                (ModelThinkingLevel::Xhigh, Some(mapped.to_string())),
                (ModelThinkingLevel::Max, Some(mapped.to_string())),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            resolve_google_thinking_level(&model, ModelThinkingLevel::High).unwrap(),
            expected,
            "map high -> {mapped}"
        );
        assert_eq!(
            resolve_google_thinking_level(&model, ModelThinkingLevel::Xhigh).unwrap(),
            expected,
            "map xhigh -> {mapped}"
        );
        assert_eq!(
            resolve_google_thinking_level(&model, ModelThinkingLevel::Max).unwrap(),
            expected,
            "map max -> {mapped}"
        );
    }

    // Invalid mappings throw with the exact upstream message.
    let mut invalid_model = make_model("google-generative-ai", "test-google", "gemini-3.7-flash");
    invalid_model.thinking_level_map = Some(
        [(ModelThinkingLevel::Xhigh, Some("extreme".to_string()))]
            .into_iter()
            .collect(),
    );
    let err = resolve_google_thinking_level(&invalid_model, ModelThinkingLevel::Xhigh)
        .expect_err("must fail");
    assert_eq!(
        err.to_string(),
        "Unsupported Google thinking level mapping for test-google/gemini-3.7-flash: xhigh -> extreme"
    );

    let err2 = resolve_google_thinking_level(
        &make_model("google-generative-ai", "test-google", "gemini-3.7-flash"),
        ModelThinkingLevel::Max,
    )
    .expect_err("must fail");
    assert_eq!(
        err2.to_string(),
        "Unsupported Google thinking level mapping for test-google/gemini-3.7-flash: max -> undefined"
    );
}

// --- google-raw-stop-reason.test.ts (unit part) --------------------------------

#[test]
fn map_stop_reason_string_table() {
    assert_eq!(map_stop_reason_string("STOP"), StopReason::Stop);
    assert_eq!(map_stop_reason_string("MAX_TOKENS"), StopReason::Length);
    assert_eq!(map_stop_reason_string("SAFETY"), StopReason::Error);
    assert_eq!(
        map_stop_reason_string("MALFORMED_FUNCTION_CALL"),
        StopReason::Error
    );
}

// --- helpers used above ---------------------------------------------------------
