//! Port of the upstream model-config behavior (pi v0.84.3): models.json
//! loading with JSONC stripping, validation errors, provider lookup, and the
//! missing-file -> empty-config contract.

use std::path::PathBuf;

use pillar_coding_agent::core::model_config::{ModelConfig, strip_json_comments};
use serde_json::json;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pillar-coding-agent-model-config-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_models_json(dir: &std::path::Path, content: &str) -> PathBuf {
    let path = dir.join("models.json");
    std::fs::write(&path, content).expect("write models.json");
    path
}

fn valid_provider_json() -> serde_json::Value {
    json!({
        "name": "Test Provider",
        "baseUrl": "https://example.test/v1",
        "apiKey": "test-key",
        "api": "openai-completions",
        "models": [
            {
                "id": "test-model",
                "name": "Test Model",
                "reasoning": true,
                "cost": { "input": 1.0, "output": 2.0, "cacheRead": 0.1, "cacheWrite": 0.2 },
                "contextWindow": 128000,
                "maxTokens": 16384
            }
        ]
    })
}

// --- strip_json_comments ----------------------------------------------------

#[test]
fn strip_json_comments_removes_line_comments_outside_strings() {
    assert_eq!(
        strip_json_comments("{ // comment\n\"a\": 1 }"),
        "{ \n\"a\": 1 }"
    );
}

#[test]
fn strip_json_comments_preserves_comment_like_content_inside_strings() {
    let input = r#"{ "url": "https://example.com//path" }"#;
    assert_eq!(strip_json_comments(input), input);
}

#[test]
fn strip_json_comments_removes_trailing_commas() {
    assert_eq!(
        strip_json_comments("{ \"a\": [1, 2,], \"b\": 1, }"),
        "{ \"a\": [1, 2], \"b\": 1 }"
    );
}

// --- load -------------------------------------------------------------------

#[test]
fn missing_file_yields_empty_config_without_error() {
    let config = ModelConfig::load(Some(std::path::Path::new("/nonexistent/models.json")));
    assert!(config.get_provider_ids().is_empty());
    assert!(config.get_error().is_none());
}

#[test]
fn none_path_yields_empty_config() {
    let config = ModelConfig::load(None);
    assert!(config.get_provider_ids().is_empty());
    assert!(config.get_error().is_none());
}

#[test]
fn loads_valid_config_and_exposes_providers() {
    let dir = temp_dir("valid");
    let path = write_models_json(
        &dir,
        &format!(
            r#"{{ "providers": {{ "test": {} }} }}"#,
            valid_provider_json()
        ),
    );

    let config = ModelConfig::load(Some(&path));
    assert!(config.get_error().is_none());
    assert_eq!(config.get_provider_ids(), vec!["test".to_string()]);
    let provider = config.get_provider("test").expect("provider");
    assert_eq!(provider.name.as_deref(), Some("Test Provider"));
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://example.test/v1")
    );
    assert_eq!(provider.api.as_deref(), Some("openai-completions"));
    let models = provider.models.as_ref().expect("models");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "test-model");
    assert_eq!(models[0].reasoning, Some(true));
    let cost = models[0].cost.as_ref().expect("cost");
    assert_eq!(cost.input, 1.0);
    assert_eq!(models[0].context_window, Some(128000.0));
}

#[test]
fn loads_jsonc_with_comments_and_trailing_commas() {
    let dir = temp_dir("jsonc");
    let path = write_models_json(
        &dir,
        r#"{
            // provider list
            "providers": {
                "test": {
                    "name": "Test",
                    "models": [{ "id": "m1", "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }, }],
                },
            }
        }"#,
    );

    let config = ModelConfig::load(Some(&path));
    assert!(
        config.get_error().is_none(),
        "error: {:?}",
        config.get_error()
    );
    assert_eq!(config.get_provider_ids(), vec!["test".to_string()]);
}

#[test]
fn parse_failure_sets_error() {
    let dir = temp_dir("parse-error");
    let path = write_models_json(&dir, "{ not json");

    let config = ModelConfig::load(Some(&path));
    assert!(config.get_provider_ids().is_empty());
    let error = config.get_error().expect("error set");
    assert!(
        error.contains("Failed to parse models.json"),
        "unexpected: {error}"
    );
    assert!(error.contains("models.json"), "unexpected: {error}");
}

#[test]
fn schema_failure_sets_error() {
    let dir = temp_dir("schema-error");
    // providers must be an object.
    let path = write_models_json(&dir, r#"{ "providers": [] }"#);

    let config = ModelConfig::load(Some(&path));
    assert!(config.get_provider_ids().is_empty());
    let error = config.get_error().expect("error set");
    assert!(
        error.contains("Invalid models.json schema"),
        "unexpected: {error}"
    );
}

#[test]
fn provider_with_bad_model_shape_sets_error() {
    let dir = temp_dir("model-error");
    // cost missing required fields.
    let path = write_models_json(
        &dir,
        r#"{ "providers": { "test": { "models": [{ "id": "m", "cost": { "input": "free" } }] } } }"#,
    );

    let config = ModelConfig::load(Some(&path));
    let error = config.get_error().expect("error set");
    assert!(error.contains("providers.test"), "unexpected: {error}");
}

#[test]
fn compat_variants_parse_per_api() {
    let dir = temp_dir("compat");
    let path = write_models_json(
        &dir,
        r#"{
            "providers": {
                "completions": {
                    "compat": { "thinkingFormat": "openai", "supportsStore": false },
                    "models": []
                },
                "responses": {
                    "compat": { "supportsToolSearch": true },
                    "models": []
                },
                "anthropic": {
                    "compat": { "forceAdaptiveThinking": true },
                    "models": []
                }
            }
        }"#,
    );

    let config = ModelConfig::load(Some(&path));
    assert!(
        config.get_error().is_none(),
        "error: {:?}",
        config.get_error()
    );
    let completions = config
        .get_provider("completions")
        .unwrap()
        .compat
        .as_ref()
        .unwrap();
    assert!(matches!(
        completions,
        pillar_coding_agent::core::model_config::ProviderCompatJson::OpenaiCompletions(_)
    ));
    // Upstream typebox unions validate in declaration order and ignore
    // unknown fields, so a responses-shaped object matches the (first)
    // completions variant; the port mirrors that first-match-wins order.
    let responses = config
        .get_provider("responses")
        .unwrap()
        .compat
        .as_ref()
        .unwrap();
    assert!(matches!(
        responses,
        pillar_coding_agent::core::model_config::ProviderCompatJson::OpenaiCompletions(_)
    ));
    let anthropic = config
        .get_provider("anthropic")
        .unwrap()
        .compat
        .as_ref()
        .unwrap();
    assert!(matches!(
        anthropic,
        pillar_coding_agent::core::model_config::ProviderCompatJson::OpenaiCompletions(_)
    ));
}

#[test]
fn model_overrides_parse() {
    let dir = temp_dir("overrides");
    let path = write_models_json(
        &dir,
        r#"{
            "providers": {
                "test": {
                    "modelOverrides": {
                        "m1": { "reasoning": true, "cost": { "input": 5 } }
                    }
                }
            }
        }"#,
    );

    let config = ModelConfig::load(Some(&path));
    assert!(
        config.get_error().is_none(),
        "error: {:?}",
        config.get_error()
    );
    let provider = config.get_provider("test").expect("provider");
    let overrides = provider.model_overrides.as_ref().expect("overrides");
    let m1 = overrides.get("m1").expect("m1");
    assert_eq!(m1.reasoning, Some(true));
    assert_eq!(m1.cost.as_ref().and_then(|cost| cost.input), Some(5.0));
}
