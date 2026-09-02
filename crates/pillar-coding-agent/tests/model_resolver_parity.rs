//! Port of the upstream model-resolver behavior (pi v0.84.3,
//! model-resolver.test.ts + args.ts thinking-level cases): exact reference
//! matching, alias/dated preference, colon-suffix thinking levels, glob
//! scoping, CLI resolution (provider inference, ambiguity, fallback model
//! ids), and initial/restore selection priority.

use pillar_ai::types::{Model, ModelCost};
use pillar_coding_agent::core::model_resolver::{
    AuthProviders, DEFAULT_THINKING_LEVEL, FindInitialModelOptions, KNOWN_PROVIDERS,
    ModelRuntimeView, ParseModelPatternOptions, ResolveCliModelOptions, ResolverThinkingLevel,
    default_model_per_provider, find_exact_model_reference_match, is_valid_thinking_level,
    models_are_equal, parse_model_pattern, resolve_cli_model, resolve_model_scope_from_models,
    restore_model_from_session,
};

fn model(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "test-api".to_string(),
        provider: provider.to_string(),
        base_url: "https://example.test/v1".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost::default(),
        context_window: 10000,
        max_tokens: 1000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn model_named(provider: &str, id: &str, name: &str) -> Model {
    Model {
        name: name.to_string(),
        ..model(provider, id)
    }
}

fn patterns(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// --- isValidThinkingLevel / defaults --------------------------------------

#[test]
fn thinking_level_vocabulary_matches_upstream() {
    for level in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
        assert!(is_valid_thinking_level(level), "{}", level);
    }
    assert!(!is_valid_thinking_level("ultra"));
    assert!(!is_valid_thinking_level(""));
}

#[test]
fn default_thinking_level_is_medium() {
    assert_eq!(DEFAULT_THINKING_LEVEL, ResolverThinkingLevel::Medium);
}

// --- defaultModelPerProvider ----------------------------------------------

#[test]
fn default_models_per_provider_are_unchanged() {
    assert_eq!(
        default_model_per_provider("anthropic"),
        Some("claude-opus-4-8")
    );
    assert_eq!(default_model_per_provider("openai"), Some("gpt-5.5"));
    assert_eq!(
        default_model_per_provider("openrouter"),
        Some("moonshotai/kimi-k2.6")
    );
    assert_eq!(default_model_per_provider("unknown-provider"), None);
    // Every entry in KNOWN_PROVIDERS has a default id.
    for provider in KNOWN_PROVIDERS {
        assert!(
            default_model_per_provider(provider).is_some(),
            "{}",
            provider
        );
    }
}

// --- modelsAreEqual --------------------------------------------------------

#[test]
fn models_are_equal_compares_provider_and_id_only() {
    let a = model("anthropic", "claude-x");
    let mut b = model("anthropic", "claude-x");
    b.base_url = "https://other.test".to_string();
    assert!(models_are_equal(&a, &b));
    assert!(!models_are_equal(&a, &model("openai", "claude-x")));
    assert!(!models_are_equal(&a, &model("anthropic", "claude-y")));
}

// --- findExactModelReferenceMatch ------------------------------------------

#[test]
fn exact_reference_match_by_canonical_form() {
    let models = vec![model("anthropic", "claude-x"), model("openai", "gpt")];
    let found = find_exact_model_reference_match("anthropic/claude-x", &models).unwrap();
    assert_eq!(found.provider, "anthropic");
    // Case-insensitive
    assert!(find_exact_model_reference_match("Anthropic/Claude-X", &models).is_some());
    // Trimmed
    assert!(find_exact_model_reference_match("  anthropic/claude-x  ", &models).is_some());
}

#[test]
fn exact_reference_match_by_bare_id_rejects_ambiguity() {
    let models = vec![
        model("anthropic", "shared"),
        model("openai", "shared"),
        model("openai", "unique"),
    ];
    assert!(find_exact_model_reference_match("unique", &models).is_some());
    // Two providers carry "shared" -> ambiguous -> no match.
    assert!(find_exact_model_reference_match("shared", &models).is_none());
}

#[test]
fn exact_reference_match_empty_and_missing() {
    let models = vec![model("anthropic", "claude-x")];
    assert!(find_exact_model_reference_match("", &models).is_none());
    assert!(find_exact_model_reference_match("  ", &models).is_none());
    assert!(find_exact_model_reference_match("nope", &models).is_none());
}

// --- parseModelPattern ------------------------------------------------------

#[test]
fn parse_exact_pattern_returns_no_thinking_level() {
    let models = vec![model("anthropic", "claude-x")];
    let result = parse_model_pattern("claude-x", &models, None);
    assert_eq!(result.model.as_ref().unwrap().id, "claude-x");
    assert_eq!(result.thinking_level, None);
    assert_eq!(result.warning, None);
}

#[test]
fn parse_pattern_with_thinking_level_suffix() {
    let models = vec![model("anthropic", "claude-x")];
    let result = parse_model_pattern("claude-x:high", &models, None);
    assert_eq!(result.model.as_ref().unwrap().id, "claude-x");
    assert_eq!(result.thinking_level, Some(ResolverThinkingLevel::High));
}

#[test]
fn parse_pattern_with_off_thinking_level() {
    let models = vec![model("anthropic", "claude-x")];
    let result = parse_model_pattern("claude-x:off", &models, None);
    assert_eq!(result.model.as_ref().unwrap().id, "claude-x");
    assert_eq!(result.thinking_level, Some(ResolverThinkingLevel::Off));
}

#[test]
fn parse_pattern_prefers_full_match_with_colon_in_id() {
    // OpenRouter-style ":exacto" suffix that is itself a real model id.
    let models = vec![model("openrouter", "kimi:exacto")];
    let result = parse_model_pattern("kimi:exacto", &models, None);
    assert_eq!(result.model.as_ref().unwrap().id, "kimi:exacto");
    assert_eq!(result.thinking_level, None);
}

#[test]
fn parse_pattern_invalid_suffix_falls_back_with_warning() {
    let models = vec![model("anthropic", "claude-x")];
    // Scope mode (default): invalid suffix -> recurse + warning.
    let result = parse_model_pattern("claude-x:bogus", &models, None);
    assert_eq!(result.model.as_ref().unwrap().id, "claude-x");
    assert_eq!(result.thinking_level, None);
    assert_eq!(
        result.warning.as_deref(),
        Some(
            "Invalid thinking level \"bogus\" in pattern \"claude-x:bogus\". Using default instead."
        )
    );
}

#[test]
fn parse_pattern_invalid_suffix_strict_mode_fails() {
    let models = vec![model("anthropic", "claude-x")];
    let result = parse_model_pattern(
        "claude-x:bogus",
        &models,
        Some(ParseModelPatternOptions {
            allow_invalid_thinking_level_fallback: false,
        }),
    );
    assert!(result.model.is_none());
    assert_eq!(result.warning, None);
}

#[test]
fn parse_pattern_no_match_anywhere() {
    let models = vec![model("anthropic", "claude-x")];
    let result = parse_model_pattern("gpt-9:high", &models, None);
    assert!(result.model.is_none());
    assert_eq!(result.thinking_level, None);
}

// --- partial matching: alias vs dated versions ------------------------------

#[test]
fn partial_match_prefers_alias_over_dated_version() {
    let models = vec![
        model("anthropic", "claude-sonnet-4-5-20250929"),
        model("anthropic", "claude-sonnet-4-5"),
    ];
    let result = parse_model_pattern("sonnet", &models, None);
    assert_eq!(result.model.as_ref().unwrap().id, "claude-sonnet-4-5");
}

#[test]
fn partial_match_prefers_highest_sorting_alias() {
    let models = vec![model("p", "model-2-latest"), model("p", "model-1-latest")];
    let result = parse_model_pattern("model", &models, None);
    assert_eq!(result.model.as_ref().unwrap().id, "model-2-latest");
}

#[test]
fn partial_match_falls_back_to_latest_dated_version() {
    let models = vec![model("p", "model-20241022"), model("p", "model-20250929")];
    let result = parse_model_pattern("model", &models, None);
    assert_eq!(result.model.as_ref().unwrap().id, "model-20250929");
}

#[test]
fn partial_match_matches_by_name_too() {
    let models = vec![model_named("p", "x1", "Sonnet Big")];
    let result = parse_model_pattern("sonnet", &models, None);
    assert_eq!(result.model.as_ref().unwrap().id, "x1");
}

#[test]
fn dated_suffix_detection_ignores_non_dates() {
    // "model-2024" is not a date suffix (4 digits) so it counts as an alias.
    let models = vec![model("p", "model-2024"), model("p", "model-20241022")];
    let result = parse_model_pattern("model", &models, None);
    assert_eq!(result.model.as_ref().unwrap().id, "model-2024");
}

// --- resolveModelScope -------------------------------------------------------

#[test]
fn scope_resolves_simple_patterns_with_dedupe() {
    let models = vec![model("anthropic", "claude-x"), model("openai", "gpt")];
    let result = resolve_model_scope_from_models(
        &patterns(&["claude-x", "claude-x", "openai/gpt"]),
        &models,
    );
    assert_eq!(result.scoped_models.len(), 2);
    assert!(result.diagnostics.is_empty());
}

#[test]
fn scope_glob_matches_provider_and_bare_id() {
    let models = vec![
        model("anthropic", "claude-sonnet"),
        model("anthropic", "claude-opus"),
        model("openai", "gpt-5"),
    ];
    // Bare glob matches without requiring provider prefix.
    let result = resolve_model_scope_from_models(&patterns(&["*sonnet*"]), &models);
    assert_eq!(result.scoped_models.len(), 1);
    assert_eq!(result.scoped_models[0].model.id, "claude-sonnet");

    // Provider-scoped glob.
    let result = resolve_model_scope_from_models(&patterns(&["anthropic/*"]), &models);
    assert_eq!(result.scoped_models.len(), 2);

    // Glob with thinking level suffix.
    let result = resolve_model_scope_from_models(&patterns(&["anthropic/*:high"]), &models);
    assert_eq!(result.scoped_models.len(), 2);
    assert_eq!(
        result.scoped_models[0].thinking_level,
        Some(ResolverThinkingLevel::High)
    );
}

#[test]
fn scope_glob_case_insensitive() {
    let models = vec![model("anthropic", "Claude-Sonnet")];
    let result = resolve_model_scope_from_models(&patterns(&["*claude*"]), &models);
    assert_eq!(result.scoped_models.len(), 1);
}

#[test]
fn scope_no_match_produces_diagnostic() {
    let models = vec![model("anthropic", "claude-x")];
    let result = resolve_model_scope_from_models(&patterns(&["gpt*"]), &models);
    assert!(result.scoped_models.is_empty());
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(
        result.diagnostics[0].message,
        "No models match pattern \"gpt*\""
    );
}

#[test]
fn scope_invalid_thinking_suffix_produces_diagnostic() {
    let models = vec![model("anthropic", "claude-x")];
    let result = resolve_model_scope_from_models(&patterns(&["claude-x:bogus"]), &models);
    assert_eq!(result.scoped_models.len(), 1);
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(
        result.diagnostics[0].message,
        "Invalid thinking level \"bogus\" in pattern \"claude-x:bogus\". Using default instead."
    );
}

// --- resolveCliModel ---------------------------------------------------------

#[test]
fn cli_no_model_flag_is_empty_result() {
    let models = vec![model("anthropic", "claude-x")];
    let auth = AuthProviders(vec!["anthropic".to_string()]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: None,
        cli_model: None,
        cli_thinking: None,
        models: &models,
        auth: &auth,
    });
    assert!(result.model.is_none());
    assert!(result.error.is_none());
}

#[test]
fn cli_no_models_available_is_an_error() {
    let auth = AuthProviders(vec![]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: None,
        cli_model: Some("x".to_string()),
        cli_thinking: None,
        models: &[],
        auth: &auth,
    });
    assert_eq!(
        result.error.as_deref(),
        Some("No models available. Check your installation or add models to models.json.")
    );
}

#[test]
fn cli_unknown_provider_is_an_error() {
    let models = vec![model("anthropic", "claude-x")];
    let auth = AuthProviders(vec!["anthropic".to_string()]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: Some("nope".to_string()),
        cli_model: Some("claude-x".to_string()),
        cli_thinking: None,
        models: &models,
        auth: &auth,
    });
    assert_eq!(
        result.error.as_deref(),
        Some("Unknown provider \"nope\". Use --list-models to see available providers/models.")
    );
}

#[test]
fn cli_provider_and_model_resolves() {
    let models = vec![model("anthropic", "claude-x")];
    let auth = AuthProviders(vec!["anthropic".to_string()]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: Some("anthropic".to_string()),
        cli_model: Some("claude-x:high".to_string()),
        cli_thinking: None,
        models: &models,
        auth: &auth,
    });
    assert_eq!(result.model.as_ref().unwrap().id, "claude-x");
    assert_eq!(result.thinking_level, Some(ResolverThinkingLevel::High));
    assert!(result.error.is_none());
}

#[test]
fn cli_provider_model_slash_form_tolerated() {
    let models = vec![model("anthropic", "claude-x")];
    let auth = AuthProviders(vec!["anthropic".to_string()]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: Some("anthropic".to_string()),
        cli_model: Some("anthropic/claude-x".to_string()),
        cli_thinking: None,
        models: &models,
        auth: &auth,
    });
    assert_eq!(result.model.as_ref().unwrap().id, "claude-x");
}

#[test]
fn cli_slash_prefix_matching_known_provider_wins() {
    // "zai/glm-5" should resolve to provider=zai, model=glm-5, not a
    // vercel-ai-gateway model whose id literally contains "zai/glm-5".
    let models = vec![
        model("vercel-ai-gateway", "zai/glm-5"),
        model("zai", "glm-5"),
    ];
    let auth = AuthProviders(vec!["zai".to_string()]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: None,
        cli_model: Some("zai/glm-5".to_string()),
        cli_thinking: None,
        models: &models,
        auth: &auth,
    });
    assert_eq!(result.model.as_ref().unwrap().provider, "zai");
    assert_eq!(result.model.as_ref().unwrap().id, "glm-5");
}

#[test]
fn cli_bare_exact_id_ambiguous_across_providers_uses_auth() {
    let models = vec![model("anthropic", "shared"), model("openai", "shared")];
    // Both authenticated -> ambiguous error.
    let both = AuthProviders(vec!["anthropic".to_string(), "openai".to_string()]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: None,
        cli_model: Some("shared".to_string()),
        cli_thinking: None,
        models: &models,
        auth: &both,
    });
    let error = result.error.unwrap();
    assert!(
        error.contains("is ambiguous across providers: anthropic/shared, openai/shared"),
        "{}",
        error
    );
    assert!(error.contains("More than one matching provider is authenticated."));

    // One authenticated -> that one wins.
    let one = AuthProviders(vec!["openai".to_string()]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: None,
        cli_model: Some("shared".to_string()),
        cli_thinking: None,
        models: &models,
        auth: &one,
    });
    assert_eq!(result.model.as_ref().unwrap().provider, "openai");

    // None authenticated -> ambiguous with auth hint.
    let none = AuthProviders(vec![]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: None,
        cli_model: Some("shared".to_string()),
        cli_thinking: None,
        models: &models,
        auth: &none,
    });
    let error = result.error.unwrap();
    assert!(error.contains("No matching provider is authenticated."));
}

#[test]
fn cli_fallback_to_custom_model_id_for_known_provider() {
    let models = vec![model("zai", "glm-5.3")];
    let auth = AuthProviders(vec!["zai".to_string()]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: Some("zai".to_string()),
        cli_model: Some("custom-model:high".to_string()),
        cli_thinking: None,
        models: &models,
        auth: &auth,
    });
    let model = result.model.unwrap();
    assert_eq!(model.id, "custom-model");
    assert!(model.reasoning, "thinking level implies reasoning");
    assert!(result.warning.as_deref().unwrap().starts_with(
        "Model \"custom-model\" not found for provider \"zai\". Using custom model id."
    ));
    // Thinking level parsed from suffix is returned.
    assert_eq!(result.thinking_level, Some(ResolverThinkingLevel::High));
}

#[test]
fn cli_fallback_prefers_provider_default_model_base() {
    let models = vec![model("zai", "glm-5.3")];
    let auth = AuthProviders(vec!["zai".to_string()]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: Some("zai".to_string()),
        cli_model: Some("custom".to_string()),
        cli_thinking: None,
        models: &models,
        auth: &auth,
    });
    // Fallback model clones the provider's default model then overrides id.
    let model = result.model.unwrap();
    assert_eq!(model.id, "custom");
    assert_eq!(model.base_url, "https://example.test/v1");
}

#[test]
fn cli_unknown_provider_model_not_found_error() {
    // Provider must be present in the catalog to get past the provider check;
    // the model id is then unknown within it.
    // Without a --provider, a bare unmatched id has no fallback base and
    // surfaces the not-found error.
    let models = vec![model("unknown-p", "m1")];
    let auth = AuthProviders(vec!["unknown-p".to_string()]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: None,
        cli_model: Some("gpt-9".to_string()),
        cli_thinking: None,
        models: &models,
        auth: &auth,
    });
    assert_eq!(
        result.error.as_deref(),
        Some("Model \"gpt-9\" not found. Use --list-models to see available models.")
    );
}

#[test]
fn cli_strict_mode_rejects_invalid_thinking_suffix() {
    let models = vec![model("anthropic", "claude-x")];
    let auth = AuthProviders(vec!["anthropic".to_string()]);
    let result = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: Some("anthropic".to_string()),
        cli_model: Some("claude-x:bogus".to_string()),
        cli_thinking: None,
        models: &models,
        auth: &auth,
    });
    // Strict CLI parsing treats the suffix as part of the model id: no model
    // match, so the known-provider fallback builds a custom id "claude-x:bogus"
    // (upstream does the same via buildFallbackModel with the full pattern).
    let model = result.model.expect("fallback model");
    assert_eq!(model.id, "claude-x:bogus");
    assert!(
        result
            .warning
            .as_deref()
            .unwrap()
            .contains("not found for provider")
    );
}

// --- findInitialModel ---------------------------------------------------------

fn runtime<'a>(
    models: &'a [Model],
    available: &'a [Model],
    auth: &'a AuthProviders,
) -> ModelRuntimeView<'a> {
    ModelRuntimeView {
        models,
        available,
        auth,
    }
}

#[test]
fn initial_cli_args_take_priority() {
    let models = vec![model("anthropic", "claude-x"), model("openai", "gpt")];
    let available = vec![model("openai", "gpt")];
    let auth = AuthProviders(vec!["openai".to_string()]);
    let result =
        pillar_coding_agent::core::model_resolver::find_initial_model(FindInitialModelOptions {
            cli_provider: Some("anthropic".to_string()),
            cli_model: Some("claude-x".to_string()),
            scoped_models: vec![],
            is_continuing: false,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_thinking_levels: None,
            runtime: runtime(&models, &available, &auth),
        });
    assert_eq!(result.model.as_ref().unwrap().id, "claude-x");
    assert_eq!(result.thinking_level, DEFAULT_THINKING_LEVEL);
}

#[test]
fn initial_scoped_model_with_per_model_level() {
    let models = vec![model("anthropic", "claude-x")];
    let available = vec![];
    let auth = AuthProviders(vec![]);
    let mut levels = std::collections::BTreeMap::new();
    levels.insert(
        "anthropic/claude-x".to_string(),
        Some(ResolverThinkingLevel::Low),
    );
    let scoped = resolve_model_scope_from_models(&patterns(&["claude-x"]), &models);
    let result =
        pillar_coding_agent::core::model_resolver::find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: scoped.scoped_models,
            is_continuing: false,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_thinking_levels: Some(levels),
            runtime: runtime(&models, &available, &auth),
        });
    assert_eq!(result.model.as_ref().unwrap().id, "claude-x");
    assert_eq!(result.thinking_level, ResolverThinkingLevel::Low);
}

#[test]
fn initial_is_continuing_skips_scoped_models() {
    let models = vec![model("anthropic", "claude-x"), model("openai", "gpt")];
    let available = vec![model("openai", "gpt")];
    let auth = AuthProviders(vec!["openai".to_string()]);
    let scoped = resolve_model_scope_from_models(&patterns(&["claude-x"]), &models);
    let result =
        pillar_coding_agent::core::model_resolver::find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: scoped.scoped_models,
            is_continuing: true,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_thinking_levels: None,
            runtime: runtime(&models, &available, &auth),
        });
    // Falls through to available models (openai default exists).
    assert_eq!(result.model.as_ref().unwrap().provider, "openai");
}

#[test]
fn initial_saved_default_used_when_auth_configured() {
    let models = vec![model("anthropic", "claude-x")];
    let available = vec![];
    let auth = AuthProviders(vec!["anthropic".to_string()]);
    let result =
        pillar_coding_agent::core::model_resolver::find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: vec![],
            is_continuing: true,
            default_provider: Some("anthropic".to_string()),
            default_model_id: Some("claude-x".to_string()),
            default_thinking_level: Some(ResolverThinkingLevel::High),
            model_thinking_levels: None,
            runtime: runtime(&models, &available, &auth),
        });
    assert_eq!(result.model.as_ref().unwrap().id, "claude-x");
    assert_eq!(result.thinking_level, ResolverThinkingLevel::High);
}

#[test]
fn initial_saved_default_skipped_without_auth() {
    let models = vec![model("anthropic", "claude-x"), model("openai", "gpt")];
    let available = vec![model("openai", "gpt")];
    let auth = AuthProviders(vec!["openai".to_string()]);
    let result =
        pillar_coding_agent::core::model_resolver::find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: vec![],
            is_continuing: true,
            default_provider: Some("anthropic".to_string()),
            default_model_id: Some("claude-x".to_string()),
            default_thinking_level: None,
            model_thinking_levels: None,
            runtime: runtime(&models, &available, &auth),
        });
    assert_eq!(result.model.as_ref().unwrap().provider, "openai");
}

#[test]
fn initial_prefers_known_provider_default_from_available() {
    let models = vec![
        model("zai", "glm-5.3"),
        model("openai", "gpt-5.5"),
        model("openai", "other"),
    ];
    let available = models.clone();
    let auth = AuthProviders(vec![]);
    let result =
        pillar_coding_agent::core::model_resolver::find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: vec![],
            is_continuing: true,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_thinking_levels: None,
            runtime: runtime(&models, &available, &auth),
        });
    // First known-provider default hit in KNOWN_PROVIDERS order wins; openai
    // default "gpt-5.5" is present.
    assert_eq!(result.model.as_ref().unwrap().id, "gpt-5.5");
}

#[test]
fn initial_falls_back_to_first_available() {
    let models = vec![model("unknown-p", "m1"), model("unknown-p", "m2")];
    let available = models.clone();
    let auth = AuthProviders(vec![]);
    let result =
        pillar_coding_agent::core::model_resolver::find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: vec![],
            is_continuing: true,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_thinking_levels: None,
            runtime: runtime(&models, &available, &auth),
        });
    assert_eq!(result.model.as_ref().unwrap().id, "m1");
}

#[test]
fn initial_no_models_at_all() {
    let auth = AuthProviders(vec![]);
    let result =
        pillar_coding_agent::core::model_resolver::find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: vec![],
            is_continuing: true,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_thinking_levels: None,
            runtime: runtime(&[], &[], &auth),
        });
    assert!(result.model.is_none());
    assert_eq!(result.thinking_level, DEFAULT_THINKING_LEVEL);
}

// --- restoreModelFromSession ---------------------------------------------------

#[test]
fn restore_returns_saved_model_when_auth_configured() {
    let models = vec![model("anthropic", "claude-x")];
    let auth = AuthProviders(vec!["anthropic".to_string()]);
    let current = model("openai", "gpt");
    let result = restore_model_from_session(
        "anthropic",
        "claude-x",
        Some(&current),
        &runtime(&models, &models, &auth),
    );
    assert_eq!(result.model.as_ref().unwrap().id, "claude-x");
    assert!(result.fallback_message.is_none());
}

#[test]
fn restore_falls_back_to_current_model_when_no_auth() {
    let models = vec![model("anthropic", "claude-x")];
    let auth = AuthProviders(vec![]);
    let current = model("openai", "gpt");
    let result = restore_model_from_session(
        "anthropic",
        "claude-x",
        Some(&current),
        &runtime(&models, &models, &auth),
    );
    assert_eq!(result.model.as_ref().unwrap().id, "gpt");
    assert_eq!(
        result.fallback_message.as_deref(),
        Some("Could not restore model anthropic/claude-x (no auth configured). Using openai/gpt.")
    );
}

#[test]
fn restore_reports_missing_model_reason() {
    let models = vec![model("openai", "gpt")];
    let auth = AuthProviders(vec!["openai".to_string()]);
    let result =
        restore_model_from_session("anthropic", "gone", None, &runtime(&models, &models, &auth));
    assert_eq!(
        result.fallback_message.as_deref(),
        Some("Could not restore model anthropic/gone (model no longer exists). Using openai/gpt.")
    );
}

#[test]
fn restore_falls_back_to_known_provider_default() {
    let models = vec![model("openai", "gpt-5.5"), model("openai", "other")];
    let auth = AuthProviders(vec!["openai".to_string()]);
    let result =
        restore_model_from_session("anthropic", "gone", None, &runtime(&models, &models, &auth));
    assert_eq!(result.model.as_ref().unwrap().id, "gpt-5.5");
}

#[test]
fn restore_nothing_available_returns_none() {
    let auth = AuthProviders(vec![]);
    let result = restore_model_from_session("a", "b", None, &runtime(&[], &[], &auth));
    assert!(result.model.is_none());
    assert!(result.fallback_message.is_none());
}
