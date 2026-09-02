//! Port of the upstream provider-composer behavior (pi v0.84.3,
//! provider-composer.test.ts cases reachable without the JS extension
//! bridge): models.json layering, custom model upserts, modelOverrides,
//! compat merging, header resolution, auth status, and composed provider
//! model lists.

use std::collections::BTreeMap;

use pillar_ai::types::{Model, ModelCost, ModelCostRates};
use pillar_coding_agent::core::model_config::{
    ModelConfig, ModelCostJson, ModelsJsonModel, ModelsJsonModelOverride, ModelsJsonProvider,
    PartialCost, ProviderCompatJson,
};
use pillar_coding_agent::core::provider_composer::{
    AuthStatusSource, ProviderConfigInput, apply_extension, apply_models_json, clear_api_key_cache,
    compose_model_provider, configured_request_auth_status, resolve_compatibility_request_config,
    resolve_configured_model_headers, validate_extension_provider,
};

fn model(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "test-api".to_string(),
        provider: provider.to_string(),
        base_url: "https://base.test/v1".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 1.25,
            },
            tiers: None,
        },
        context_window: 100_000,
        max_tokens: 8_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn json_model(id: &str) -> ModelsJsonModel {
    ModelsJsonModel {
        id: id.to_string(),
        ..Default::default()
    }
}

fn headers(items: &[(&str, &str)]) -> BTreeMap<String, String> {
    items
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// --- clearApiKeyCache ---------------------------------------------------------

#[test]
fn clear_api_key_cache_is_exported() {
    clear_api_key_cache();
}

// --- applyModelsJson ------------------------------------------------------------

#[test]
fn no_config_returns_base_models() {
    let base = vec![model("p", "m1")];
    let result = apply_models_json("p", &base, None).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, "m1");
}

#[test]
fn config_requires_some_content() {
    let base = vec![model("p", "m1")];
    let config = ModelsJsonProvider::default();
    let error = apply_models_json("p", &base, Some(&config)).unwrap_err();
    assert_eq!(
        error.0,
        "Provider p: must specify \"baseUrl\", \"headers\", \"compat\", \"modelOverrides\", or \"models\"."
    );
}

#[test]
fn oauth_requires_base_url() {
    let base = vec![model("p", "m1")];
    let config = ModelsJsonProvider {
        oauth: Some("x".to_string()),
        ..Default::default()
    };
    let error = apply_models_json("p", &base, Some(&config)).unwrap_err();
    assert_eq!(
        error.0,
        "Provider p: \"baseUrl\" is required when \"oauth\" is set."
    );
}

#[test]
fn base_url_overrides_all_models() {
    let base = vec![model("p", "m1"), model("p", "m2")];
    let config = ModelsJsonProvider {
        base_url: Some("https://proxy.test/v1".to_string()),
        ..Default::default()
    };
    let result = apply_models_json("p", &base, Some(&config)).unwrap();
    assert!(result.iter().all(|m| m.base_url == "https://proxy.test/v1"));
}

#[test]
fn custom_models_upsert_by_id() {
    let base = vec![model("p", "m1")];
    let mut definition = json_model("m2");
    definition.api = Some("test-api".to_string());
    definition.base_url = Some("https://custom.test/v1".to_string());
    definition.context_window = Some(50_000.0);
    let config = ModelsJsonProvider {
        base_url: Some("https://proxy.test/v1".to_string()),
        models: Some(vec![definition]),
        ..Default::default()
    };
    let result = apply_models_json("p", &base, Some(&config)).unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[1].id, "m2");
    assert_eq!(result[1].context_window, 50_000);
    assert_eq!(result[1].name, "m2", "name defaults to id");
    assert!(!result[1].reasoning, "reasoning defaults to false");
    // Existing model replaced via definition with the same id.
    let mut replacement = json_model("m1");
    replacement.api = Some("test-api".to_string());
    replacement.base_url = Some("https://other.test".to_string());
    let config = ModelsJsonProvider {
        models: Some(vec![replacement]),
        ..Default::default()
    };
    let result = apply_models_json("p", &base, Some(&config)).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].base_url, "https://other.test");
}

#[test]
fn custom_model_defaults_inherit_provider_and_existing_model() {
    let base = vec![model("p", "m1")];
    let mut definition = json_model("m2");
    definition.context_window = Some(50_000.0);
    // No api/baseUrl on the definition or provider: falls back to the
    // existing-model defaults.
    let config = ModelsJsonProvider {
        models: Some(vec![definition]),
        ..Default::default()
    };
    let result = apply_models_json("p", &base, Some(&config)).unwrap();
    assert_eq!(result[1].api, "test-api");
    assert_eq!(result[1].base_url, "https://base.test/v1");
}

#[test]
fn custom_model_missing_api_everywhere_is_an_error() {
    let base = vec![];
    let mut definition = json_model("m2");
    definition.base_url = Some("https://custom.test".to_string());
    let config = ModelsJsonProvider {
        models: Some(vec![definition]),
        ..Default::default()
    };
    let error = apply_models_json("p", &base, Some(&config)).unwrap_err();
    assert_eq!(
        error.0,
        "Provider p, model m2: no \"api\" specified. Set at provider or model level."
    );
}

#[test]
fn custom_model_missing_base_url_is_an_error() {
    let base = vec![];
    let mut definition = json_model("m2");
    definition.api = Some("test-api".to_string());
    let config = ModelsJsonProvider {
        models: Some(vec![definition]),
        ..Default::default()
    };
    let error = apply_models_json("p", &base, Some(&config)).unwrap_err();
    assert_eq!(
        error.0,
        "Provider p: \"baseUrl\" is required when defining custom models."
    );
}

#[test]
fn invalid_context_window_and_max_tokens() {
    let base = vec![];
    let mut definition = json_model("m2");
    definition.api = Some("test-api".to_string());
    definition.base_url = Some("https://x.test".to_string());
    definition.context_window = Some(0.0);
    let config = ModelsJsonProvider {
        models: Some(vec![definition]),
        ..Default::default()
    };
    assert_eq!(
        apply_models_json("p", &base, Some(&config)).unwrap_err().0,
        "Provider p, model m2: invalid contextWindow"
    );

    let mut definition = json_model("m2");
    definition.api = Some("test-api".to_string());
    definition.base_url = Some("https://x.test".to_string());
    definition.max_tokens = Some(-1.0);
    let config = ModelsJsonProvider {
        models: Some(vec![definition]),
        ..Default::default()
    };
    assert_eq!(
        apply_models_json("p", &base, Some(&config)).unwrap_err().0,
        "Provider p, model m2: invalid maxTokens"
    );
}

#[test]
fn custom_model_default_cost_and_limits() {
    let base = vec![];
    let mut definition = json_model("m2");
    definition.api = Some("test-api".to_string());
    definition.base_url = Some("https://x.test".to_string());
    let config = ModelsJsonProvider {
        models: Some(vec![definition]),
        ..Default::default()
    };
    let result = apply_models_json("p", &base, Some(&config)).unwrap();
    assert_eq!(result[0].cost.rates.input, 0.0);
    assert_eq!(result[0].context_window, 128_000);
    assert_eq!(result[0].max_tokens, 16_384);
    assert_eq!(result[0].input, vec!["text".to_string()]);
}

// --- compat merging --------------------------------------------------------------

#[test]
fn compat_merges_with_deep_keys() {
    let base = vec![model("p", "m1")];
    let config = ModelsJsonProvider {
        base_url: Some("https://proxy.test/v1".to_string()),
        compat: Some(ProviderCompatJson::OpenaiCompletions(Box::new(
            serde_json::from_str(
                r#"{"supportsStore": true, "openRouterRouting": {"order": ["a"], "only": ["x"]}}"#,
            )
            .unwrap(),
        ))),
        ..Default::default()
    };
    let result = apply_models_json("p", &base, Some(&config)).unwrap();
    let compat = result[0].compat.as_ref().unwrap();
    let value = serde_json::to_value(compat).unwrap();
    assert_eq!(value["supportsStore"], serde_json::json!(true));
    assert_eq!(
        value["openRouterRouting"]["order"],
        serde_json::json!(["a"])
    );
}

#[test]
fn model_level_compat_overrides_provider_level() {
    let base = vec![];
    let mut definition = json_model("m2");
    definition.api = Some("test-api".to_string());
    definition.base_url = Some("https://x.test".to_string());
    definition.compat = Some(ProviderCompatJson::OpenaiCompletions(Box::new(
        serde_json::from_str(r#"{"supportsStore": true}"#).unwrap(),
    )));
    let config = ModelsJsonProvider {
        models: Some(vec![definition]),
        compat: Some(ProviderCompatJson::OpenaiCompletions(Box::new(
            serde_json::from_str(r#"{"supportsDeveloperRole": true}"#).unwrap(),
        ))),
        ..Default::default()
    };
    let result = apply_models_json("p", &base, Some(&config)).unwrap();
    let value = serde_json::to_value(result[0].compat.as_ref().unwrap()).unwrap();
    // Provider-level compat merges into model-level.
    assert_eq!(value["supportsStore"], serde_json::json!(true));
    assert_eq!(value["supportsDeveloperRole"], serde_json::json!(true));
}

// --- applyExtension -----------------------------------------------------------------

#[test]
fn extension_base_url_only_rewrites_urls() {
    let base = vec![model("p", "m1")];
    let extension = ProviderConfigInput {
        base_url: Some("https://ext.test/v1".to_string()),
        ..Default::default()
    };
    let result = apply_extension("p", &base, Some(&extension)).unwrap();
    assert_eq!(result[0].base_url, "https://ext.test/v1");
    assert_eq!(result[0].id, "m1");
}

#[test]
fn extension_models_replace_the_list() {
    let base = vec![model("p", "m1")];
    let extension = ProviderConfigInput {
        api: Some("test-api".to_string()),
        base_url: Some("https://ext.test/v1".to_string()),
        models: Some(vec![super_extension_model("custom")]),
        ..Default::default()
    };
    let result = apply_extension("p", &base, Some(&extension)).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, "custom");
    assert_eq!(result[0].base_url, "https://ext.test/v1");
    assert_eq!(result[0].provider, "p");
}

fn super_extension_model(id: &str) -> pillar_coding_agent::core::provider_composer::ExtensionModel {
    pillar_coding_agent::core::provider_composer::ExtensionModel {
        id: id.to_string(),
        name: id.to_string(),
        api: None,
        base_url: None,
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        context_window: 100_000,
        max_tokens: 8_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

#[test]
fn validate_extension_provider_accepts_valid_input() {
    let base = vec![model("p", "m1")];
    let extension = ProviderConfigInput {
        base_url: Some("https://ext.test/v1".to_string()),
        ..Default::default()
    };
    assert!(validate_extension_provider("p", &base, None, &extension).is_ok());
}

// --- modelOverrides -------------------------------------------------------------------

#[test]
fn model_overrides_apply_as_the_topmost_layer() {
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "m1".to_string(),
        ModelsJsonModelOverride {
            name: Some("Renamed".to_string()),
            reasoning: Some(true),
            context_window: Some(200_000.0),
            max_tokens: Some(16_000.0),
            cost: Some(PartialCost {
                input: Some(3.0),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let config = ModelsJsonProvider {
        base_url: Some("https://proxy.test/v1".to_string()),
        model_overrides: Some(overrides),
        ..Default::default()
    };
    // Upstream applies modelOverrides inside composeModelProvider.getModels,
    // after the models.json layer — the port mirrors that in compose.
    let model_config = ModelConfig::from_providers(BTreeMap::from([("p".to_string(), config)]));
    let provider =
        compose_model_provider("p", Some(&test_base_provider()), &model_config, None).unwrap();
    let result = (provider.get_models)();
    let m = &result[0];
    assert_eq!(m.name, "Renamed");
    assert!(m.reasoning);
    assert_eq!(m.context_window, 200_000);
    assert_eq!(m.max_tokens, 16_000);
    assert_eq!(m.cost.rates.input, 3.0);
    // Partial cost: other rates keep base values.
    assert_eq!(m.cost.rates.output, 2.0);
}

#[test]
fn overrides_survive_custom_model_upserts() {
    // modelOverrides apply after custom-model upserts in the composed chain
    // (composeModelProvider.getModels); here verify the override map lookup
    // by id works for a custom model too.
    let base = vec![];
    let mut definition = json_model("custom");
    definition.api = Some("test-api".to_string());
    definition.base_url = Some("https://x.test".to_string());
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "custom".to_string(),
        ModelsJsonModelOverride {
            name: Some("Override".to_string()),
            ..Default::default()
        },
    );
    let config = ModelsJsonProvider {
        models: Some(vec![definition]),
        model_overrides: Some(overrides),
        ..Default::default()
    };
    let applied = apply_models_json("p", &base, Some(&config)).unwrap();
    assert_eq!(
        applied[0].name, "custom",
        "apply_models_json itself does not apply overrides"
    );

    // Through compose_model_provider the override layer applies on top.
    let model_config = ModelConfig::from_providers(BTreeMap::from([("p".to_string(), config)]));
    let provider = compose_model_provider("p", None, &model_config, None).unwrap();
    let models = (provider.get_models)();
    assert_eq!(models[0].name, "Override");
}

// --- configuredRequestAuthStatus ---------------------------------------------------------

#[test]
fn no_api_key_gives_no_status() {
    assert!(configured_request_auth_status(None, None).is_none());
}

#[test]
fn literal_api_key_status_sources() {
    let config = ModelsJsonProvider {
        api_key: Some("sk-literal".to_string()),
        ..Default::default()
    };
    let status = configured_request_auth_status(Some(&config), None).unwrap();
    assert!(status.configured);
    assert_eq!(status.source, Some(AuthStatusSource::ModelsJsonKey));

    let extension = ProviderConfigInput {
        api_key: Some("sk-ext".to_string()),
        ..Default::default()
    };
    let status = configured_request_auth_status(None, Some(&extension)).unwrap();
    assert_eq!(status.source, Some(AuthStatusSource::Fallback));
}

#[test]
fn command_api_key_reports_models_json_command() {
    let config = ModelsJsonProvider {
        api_key: Some("!echo secret".to_string()),
        ..Default::default()
    };
    let status = configured_request_auth_status(Some(&config), None).unwrap();
    assert!(status.configured);
    assert_eq!(status.source, Some(AuthStatusSource::ModelsJsonCommand));
}

#[test]
fn env_var_api_key_reports_environment_with_labels() {
    // SAFETY: test-only, single-threaded env mutation.
    unsafe { std::env::set_var("PCP_TEST_KEY", "value") };
    let config = ModelsJsonProvider {
        api_key: Some("$PCP_TEST_KEY".to_string()),
        ..Default::default()
    };
    let status = configured_request_auth_status(Some(&config), None).unwrap();
    assert!(status.configured);
    assert_eq!(status.source, Some(AuthStatusSource::Environment));
    assert_eq!(status.label.as_deref(), Some("PCP_TEST_KEY"));
    unsafe { std::env::remove_var("PCP_TEST_KEY") };

    // Missing env var -> not configured.
    let config = ModelsJsonProvider {
        api_key: Some("$PCP_TEST_MISSING".to_string()),
        ..Default::default()
    };
    let status = configured_request_auth_status(Some(&config), None).unwrap();
    assert!(!status.configured);
}

// --- header resolution ----------------------------------------------------------------------

#[test]
fn model_headers_merge_config_and_extension_layers() {
    let m = model("p", "m1");
    let config = ModelsJsonProvider {
        headers: Some(headers(&[("X-Provider", "p"), ("X-Shared", "config")])),
        ..Default::default()
    };
    let mut definition = json_model("m1");
    definition.headers = Some(headers(&[("X-Model", "model")]));
    let config_with_models = ModelsJsonProvider {
        headers: config.headers.clone(),
        models: Some(vec![definition]),
        ..Default::default()
    };
    // Upstream rawModelHeaders reads config.modelOverrides, per-model
    // definitions, and extension model headers — provider/extension-level
    // headers are request-layer, not raw model headers.
    let mut ext_model = super_extension_model("m1");
    ext_model.headers = Some(headers(&[("X-Ext", "ext")]));
    let extension = ProviderConfigInput {
        models: Some(vec![ext_model]),
        ..Default::default()
    };
    let resolved =
        resolve_configured_model_headers(&m, Some(&config_with_models), Some(&extension), None)
            .unwrap()
            .unwrap();
    assert_eq!(resolved.get("X-Model").map(String::as_str), Some("model"));
    assert_eq!(resolved.get("X-Ext").map(String::as_str), Some("ext"));
    assert!(!resolved.contains_key("X-Provider"));
}

#[test]
fn model_override_headers_apply_per_model() {
    let m = model("p", "m1");
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "m1".to_string(),
        ModelsJsonModelOverride {
            headers: Some(headers(&[("X-Override", "yes")])),
            ..Default::default()
        },
    );
    let config = ModelsJsonProvider {
        model_overrides: Some(overrides),
        ..Default::default()
    };
    let resolved = resolve_configured_model_headers(&m, Some(&config), None, None)
        .unwrap()
        .unwrap();
    assert_eq!(resolved.get("X-Override").map(String::as_str), Some("yes"));
}

#[test]
fn no_configured_headers_resolves_none() {
    let m = model("p", "m1");
    assert!(
        resolve_configured_model_headers(&m, None, None, None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn compatibility_request_config_merges_model_and_configured_headers() {
    let mut m = model("p", "m1");
    m.headers = Some(
        headers(&[("X-Model", "m"), ("X-Shared", "model")])
            .into_iter()
            .map(|(k, v)| (k, Some(v)))
            .collect(),
    );
    let config = ModelsJsonProvider {
        headers: Some(headers(&[("X-Config", "c"), ("X-Shared", "config")])),
        ..Default::default()
    };
    let result = resolve_compatibility_request_config(&m, Some(&config), None).unwrap();
    let headers_map = result.headers.unwrap();
    assert_eq!(headers_map.get("X-Model").unwrap(), &Some("m".to_string()));
    assert_eq!(headers_map.get("X-Config").unwrap(), &Some("c".to_string()));
    // Configured headers win over model headers on shared keys.
    assert_eq!(
        headers_map.get("X-Shared").unwrap(),
        &Some("config".to_string())
    );
    assert!(!result.auth_header);
}

#[test]
fn auth_header_flag_propagates() {
    let config = ModelsJsonProvider {
        auth_header: Some(true),
        api_key: Some("sk".to_string()),
        ..Default::default()
    };
    let m = model("p", "m1");
    let result = resolve_compatibility_request_config(&m, Some(&config), None).unwrap();
    assert!(result.auth_header);
}

// --- composeModelProvider ----------------------------------------------------------------------

#[test]
fn compose_fabricates_an_api_key_auth_when_only_configured_key_exists() {
    // Upstream composeApiKeyAuth always returns a composed method (with a
    // prompt login) unless the provider is OAuth-only; the port mirrors this
    // by always composing api-key auth. The no-auth error is therefore
    // unreachable through the public composition path; verify composition
    // succeeds with a bare baseUrl config instead.
    let model_config = ModelConfig::from_providers(BTreeMap::from([(
        "p".to_string(),
        ModelsJsonProvider {
            base_url: Some("https://x.test".to_string()),
            ..Default::default()
        },
    )]));
    let provider = compose_model_provider("p", None, &model_config, None).unwrap();
    assert!(provider.auth.api_key.is_some());
}

#[test]
fn compose_with_models_json_key_composes_a_working_provider() {
    let model_config = ModelConfig::from_providers(BTreeMap::from([(
        "p".to_string(),
        ModelsJsonProvider {
            name: Some("Configured Name".to_string()),
            base_url: Some("https://x.test/v1".to_string()),
            api_key: Some("sk-test".to_string()),
            ..Default::default()
        },
    )]));
    let provider = compose_model_provider("p", None, &model_config, None).unwrap();
    assert_eq!(provider.id, "p");
    assert_eq!(provider.name, "Configured Name");
    assert_eq!(provider.base_url.as_deref(), Some("https://x.test/v1"));
    assert!(provider.auth.api_key.is_some());
    // Empty catalog: no models.
    assert!((provider.get_models)().is_empty());
}

#[test]
fn compose_applies_the_extension_layer_last() {
    let model_config = ModelConfig::from_providers(BTreeMap::from([(
        "p".to_string(),
        ModelsJsonProvider {
            api_key: Some("sk-test".to_string()),
            base_url: Some("https://config.test/v1".to_string()),
            ..Default::default()
        },
    )]));
    let extension = ProviderConfigInput {
        name: Some("Extension Name".to_string()),
        base_url: Some("https://ext.test/v1".to_string()),
        ..Default::default()
    };
    let provider = compose_model_provider("p", None, &model_config, Some(&extension)).unwrap();
    assert_eq!(provider.name, "Extension Name");
    assert_eq!(provider.base_url.as_deref(), Some("https://ext.test/v1"));
}

#[test]
fn compose_inherits_base_provider_models_and_dispatch() {
    let base_model = model("p", "m1");
    let base = pillar_ai::models::Provider {
        id: "p".to_string(),
        name: "Base".to_string(),
        base_url: Some("https://base.test".to_string()),
        headers: None,
        auth: pillar_ai::auth_types::ProviderAuth::default(),
        get_models: {
            let m = base_model.clone();
            Box::new(move || vec![m.clone()])
        },
        refresh_models: None,
        filter_models: None,
        api: pillar_ai::api_dispatch::single_api("openai-completions"),
    };
    let model_config = ModelConfig::from_providers(BTreeMap::from([(
        "p".to_string(),
        ModelsJsonProvider {
            api_key: Some("sk-test".to_string()),
            ..Default::default()
        },
    )]));
    let provider = compose_model_provider("p", Some(&base), &model_config, None).unwrap();
    let models = (provider.get_models)();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "m1");
    // Dispatch inherited from the base provider.
    assert!(matches!(
        provider.api,
        pillar_ai::models::ProviderApi::Map(_)
    ));
}

#[test]
fn cost_json_round_trip_through_override() {
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "m1".to_string(),
        ModelsJsonModelOverride {
            cost: Some(PartialCost {
                input: Some(5.0),
                output: Some(6.0),
                cache_read: Some(0.5),
                cache_write: Some(6.5),
                tiers: None,
            }),
            ..Default::default()
        },
    );
    let config = ModelsJsonProvider {
        api_key: Some("sk".to_string()),
        base_url: Some("https://x.test".to_string()),
        model_overrides: Some(overrides),
        ..Default::default()
    };
    let model_config = ModelConfig::from_providers(BTreeMap::from([("p".to_string(), config)]));
    let provider =
        compose_model_provider("p", Some(&test_base_provider()), &model_config, None).unwrap();
    let models = (provider.get_models)();
    assert_eq!(models[0].cost.rates.input, 5.0);
    assert_eq!(models[0].cost.rates.output, 6.0);
    assert_eq!(models[0].cost.rates.cache_read, 0.5);
    assert_eq!(models[0].cost.rates.cache_write, 6.5);
}

fn test_base_provider() -> pillar_ai::models::Provider {
    let m = model("p", "m1");
    pillar_ai::models::Provider {
        id: "p".to_string(),
        name: "Base".to_string(),
        base_url: Some("https://base.test".to_string()),
        headers: None,
        auth: pillar_ai::auth_types::ProviderAuth::default(),
        get_models: Box::new(move || vec![m.clone()]),
        refresh_models: None,
        filter_models: None,
        api: pillar_ai::models::ProviderApi::None,
    }
}

// Suppress unused-import warnings for types used only in doc comments.
#[allow(unused)]
fn _type_witness(_: ModelCostJson) {}
