//! Port of packages/coding-agent/src/core/model-resolver.ts (pi v0.84.3) and
//! the small helpers it needs from `cli/args.ts` (`isValidThinkingLevel`) and
//! `defaults.ts` (`DEFAULT_THINKING_LEVEL`), plus `modelsAreEqual` from pi-ai.
//!
//! Model resolution, scoping, and initial selection.
//!
//! divergence: upstream takes a `ModelRuntime` (async availability scans, CLI
//! exit on error, chalk console output); the port operates on plain model
//! slices and returns results, with an auth-lookup trait replacing
//! `hasConfiguredAuth`. Console output is the caller's concern.

use std::collections::BTreeMap;

use globset::GlobBuilder;
use pillar_ai::types::Model;

pub use pillar_agent::types::thinking::AgentThinkingLevel as ResolverThinkingLevel;

/// Default thinking level (upstream `DEFAULT_THINKING_LEVEL`).
pub const DEFAULT_THINKING_LEVEL: ResolverThinkingLevel = ResolverThinkingLevel::Medium;

const VALID_THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Upstream `isValidThinkingLevel` (cli/args.ts).
pub fn is_valid_thinking_level(level: &str) -> bool {
    VALID_THINKING_LEVELS.contains(&level)
}

/// Upstream `modelsAreEqual` (pi-ai models.ts): equality by id + provider.
pub fn models_are_equal(a: &Model, b: &Model) -> bool {
    a.id == b.id && a.provider == b.provider
}

/// Default model IDs for each known provider (upstream
/// `defaultModelPerProvider`, verbatim).
pub fn default_model_per_provider(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "amazon-bedrock" => "us.anthropic.claude-opus-4-6-v1",
        "ant-ling" => "Ring-2.6-1T",
        "anthropic" => "claude-opus-4-8",
        "openai" => "gpt-5.5",
        "azure-openai-responses" => "gpt-5.4",
        "openai-codex" => "gpt-5.5",
        "radius" => "auto",
        "nvidia" => "nvidia/nemotron-3-super-120b-a12b",
        "deepseek" => "deepseek-v4-pro",
        "google" => "gemini-3.1-pro-preview",
        "google-vertex" => "gemini-3.1-pro-preview",
        "github-copilot" => "gpt-5.4",
        "openrouter" => "moonshotai/kimi-k2.6",
        "vercel-ai-gateway" => "zai/glm-5.1",
        "xai" => "grok-4.6",
        "groq" => "openai/gpt-oss-120b",
        "cerebras" => "gpt-oss-120b",
        "zai" => "glm-5.3",
        "zai-coding-cn" => "glm-5.3",
        "mistral" => "devstral-medium-latest",
        "minimax" => "MiniMax-M2.7",
        "minimax-cn" => "MiniMax-M2.7",
        "moonshotai" => "kimi-k2.6",
        "moonshotai-cn" => "kimi-k2.6",
        "huggingface" => "moonshotai/Kimi-K2.6",
        "fireworks" => "accounts/fireworks/models/kimi-k2p6",
        "together" => "moonshotai/Kimi-K2.6",
        "baseten" => "zai-org/GLM-5.2",
        "opencode" => "kimi-k2.6",
        "opencode-go" => "kimi-k2.6",
        "kimi-coding" => "kimi-for-coding",
        "cloudflare-workers-ai" => "@cf/moonshotai/kimi-k2.6",
        "cloudflare-ai-gateway" => "workers-ai/@cf/moonshotai/kimi-k2.6",
        "qwen-token-plan" => "qwen3.7-max",
        "qwen-token-plan-cn" => "qwen3.7-max",
        "qwen-token-plan-individual" => "qwen3.8-max",
        "xiaomi" => "mimo-v2.5-pro",
        "xiaomi-token-plan-cn" => "mimo-v2.5-pro",
        "xiaomi-token-plan-ams" => "mimo-v2.5-pro",
        "xiaomi-token-plan-sgp" => "mimo-v2.5-pro",
        _ => return None,
    })
}

/// Ordered iteration over known providers (upstream iterates
/// `Object.keys(defaultModelPerProvider)`; the port fixes the order).
pub const KNOWN_PROVIDERS: [&str; 40] = [
    "amazon-bedrock",
    "ant-ling",
    "anthropic",
    "openai",
    "azure-openai-responses",
    "openai-codex",
    "radius",
    "nvidia",
    "deepseek",
    "google",
    "google-vertex",
    "github-copilot",
    "openrouter",
    "vercel-ai-gateway",
    "xai",
    "groq",
    "cerebras",
    "zai",
    "zai-coding-cn",
    "mistral",
    "minimax",
    "minimax-cn",
    "moonshotai",
    "moonshotai-cn",
    "huggingface",
    "fireworks",
    "together",
    "baseten",
    "opencode",
    "opencode-go",
    "kimi-coding",
    "cloudflare-workers-ai",
    "cloudflare-ai-gateway",
    "qwen-token-plan",
    "qwen-token-plan-cn",
    "qwen-token-plan-individual",
    "xiaomi",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-sgp",
];

/// A model matched from a scope pattern, with optional explicit thinking level.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedModel {
    pub model: Model,
    /// Thinking level if explicitly specified in pattern (e.g. "model:high").
    pub thinking_level: Option<ResolverThinkingLevel>,
}

/// Upstream `ThinkingLevel` here is the agent-level vocabulary including
/// "off"; pattern suffixes parse into it.
fn parse_thinking_level(suffix: &str) -> Option<ResolverThinkingLevel> {
    Some(match suffix {
        "off" => ResolverThinkingLevel::Off,
        "minimal" => ResolverThinkingLevel::Minimal,
        "low" => ResolverThinkingLevel::Low,
        "medium" => ResolverThinkingLevel::Medium,
        "high" => ResolverThinkingLevel::High,
        "xhigh" => ResolverThinkingLevel::Xhigh,
        "max" => ResolverThinkingLevel::Max,
        _ => return None,
    })
}

/// Auth lookup used in place of upstream `ModelRuntime.hasConfiguredAuth`.
pub trait AuthConfigured {
    fn has_configured_auth(&self, provider: &str) -> bool;
}

/// Auth source that reports every provider authenticated (no-pricing
/// equivalent for tests and callers without an auth layer).
pub struct AllAuthConfigured;
impl AuthConfigured for AllAuthConfigured {
    fn has_configured_auth(&self, _provider: &str) -> bool {
        true
    }
}

/// Auth source that reports no provider authenticated.
pub struct NoAuthConfigured;
impl AuthConfigured for NoAuthConfigured {
    fn has_configured_auth(&self, _provider: &str) -> bool {
        false
    }
}

/// Auth source backed by a fixed provider set.
pub struct AuthProviders(pub Vec<String>);
impl AuthConfigured for AuthProviders {
    fn has_configured_auth(&self, provider: &str) -> bool {
        self.0.iter().any(|p| p == provider)
    }
}

/// Helper to check if a model ID looks like an alias (no date suffix).
/// Dates are typically in format: -20241022 or -20250929.
fn is_alias(id: &str) -> bool {
    // Check if ID ends with -latest
    if id.ends_with("-latest") {
        return true;
    }
    // Check if ID ends with a date pattern (-YYYYMMDD)
    let bytes = id.as_bytes();
    if bytes.len() >= 9 && bytes[bytes.len() - 9] == b'-' {
        return !bytes[bytes.len() - 8..].iter().all(|b| b.is_ascii_digit());
    }
    true
}

/// Find an exact model reference match. Supports either a bare model id or a
/// canonical provider/modelId reference. When matching by bare id, ambiguous
/// matches across providers are rejected.
pub fn find_exact_model_reference_match(
    model_reference: &str,
    available_models: &[Model],
) -> Option<Model> {
    let trimmed_reference = model_reference.trim();
    if trimmed_reference.is_empty() {
        return None;
    }

    let normalized_reference = trimmed_reference.to_lowercase();

    let canonical_matches: Vec<&Model> = available_models
        .iter()
        .filter(|model| {
            format!("{}/{}", model.provider, model.id).to_lowercase() == normalized_reference
        })
        .collect();
    if canonical_matches.len() == 1 {
        return Some(canonical_matches[0].clone());
    }
    if canonical_matches.len() > 1 {
        return None;
    }

    if let Some(slash_index) = trimmed_reference.find('/') {
        let provider = trimmed_reference[..slash_index].trim();
        let model_id = trimmed_reference[slash_index + 1..].trim();
        if !provider.is_empty() && !model_id.is_empty() {
            let provider_matches: Vec<&Model> = available_models
                .iter()
                .filter(|model| {
                    model.provider.to_lowercase() == provider.to_lowercase()
                        && model.id.to_lowercase() == model_id.to_lowercase()
                })
                .collect();
            if provider_matches.len() == 1 {
                return Some(provider_matches[0].clone());
            }
            if provider_matches.len() > 1 {
                return None;
            }
        }
    }

    let id_matches: Vec<&Model> = available_models
        .iter()
        .filter(|model| model.id.to_lowercase() == normalized_reference)
        .collect();
    if id_matches.len() == 1 {
        Some(id_matches[0].clone())
    } else {
        None
    }
}

/// Try to match a pattern to a model from the available models list. Returns
/// the matched model or None if no match found.
fn try_match_model(model_pattern: &str, available_models: &[Model]) -> Option<Model> {
    if let Some(exact_match) = find_exact_model_reference_match(model_pattern, available_models) {
        return Some(exact_match);
    }

    // No exact match - fall back to partial matching
    let pattern_lower = model_pattern.to_lowercase();
    let matches: Vec<&Model> = available_models
        .iter()
        .filter(|m| {
            m.id.to_lowercase().contains(&pattern_lower)
                || m.name.to_lowercase().contains(&pattern_lower)
        })
        .collect();

    if matches.is_empty() {
        return None;
    }

    // Separate into aliases and dated versions
    let aliases: Vec<&&Model> = matches.iter().filter(|m| is_alias(&m.id)).collect();
    if !aliases.is_empty() {
        // Prefer alias - if multiple aliases, pick the one that sorts highest
        let mut aliases = aliases;
        aliases.sort_by(|a, b| b.id.cmp(&a.id));
        Some((*aliases[0]).clone())
    } else {
        // No alias found, pick latest dated version
        let mut dated: Vec<&&Model> = matches.iter().filter(|m| !is_alias(&m.id)).collect();
        dated.sort_by(|a, b| b.id.cmp(&a.id));
        dated.first().map(|m| (*(*m)).clone())
    }
}

/// Result of parsing a model pattern.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedModelResult {
    pub model: Option<Model>,
    /// Thinking level if explicitly specified in pattern.
    pub thinking_level: Option<ResolverThinkingLevel>,
    pub warning: Option<String>,
}

fn build_fallback_model(
    provider: &str,
    model_id: &str,
    available_models: &[Model],
) -> Option<Model> {
    let provider_models: Vec<&Model> = available_models
        .iter()
        .filter(|m| m.provider == provider)
        .collect();
    if provider_models.is_empty() {
        return None;
    }

    let default_id = default_model_per_provider(provider);
    let base = default_id
        .and_then(|id| provider_models.iter().find(|m| m.id == id))
        .unwrap_or(&provider_models[0]);

    let mut model = (*base).clone();
    model.id = model_id.to_string();
    model.name = model_id.to_string();
    Some(model)
}

/// Parse a pattern to extract model and thinking level. Handles models with
/// colons in their IDs (e.g. OpenRouter's `:exacto` suffix).
///
/// Algorithm:
/// 1. Try to match full pattern as a model
/// 2. If found, return it
/// 3. If not found and has colons, split on last colon:
///    - If suffix is a valid thinking level, use it and recurse on prefix
///    - If suffix is invalid, warn and recurse on prefix
pub fn parse_model_pattern(
    pattern: &str,
    available_models: &[Model],
    options: Option<ParseModelPatternOptions>,
) -> ParsedModelResult {
    // Try exact match first
    if let Some(exact_match) = try_match_model(pattern, available_models) {
        return ParsedModelResult {
            model: Some(exact_match),
            thinking_level: None,
            warning: None,
        };
    }

    // No match - try splitting on last colon if present
    let Some(last_colon_index) = pattern.rfind(':') else {
        // No colons, pattern simply doesn't match any model
        return ParsedModelResult::default();
    };

    let prefix = &pattern[..last_colon_index];
    let suffix = &pattern[last_colon_index + 1..];

    if let Some(level) = parse_thinking_level(suffix) {
        // Valid thinking level - recurse on prefix and use this level
        let result = parse_model_pattern(prefix, available_models, options);
        if result.model.is_some() {
            // Only use this thinking level if no warning from inner recursion
            return ParsedModelResult {
                thinking_level: if result.warning.is_none() {
                    Some(level)
                } else {
                    None
                },
                ..result
            };
        }
        result
    } else {
        // Invalid suffix
        let allow_fallback = options
            .as_ref()
            .is_none_or(|o| o.allow_invalid_thinking_level_fallback);
        if !allow_fallback {
            // In strict mode (CLI --model parsing), treat it as part of the
            // model id and fail. This avoids accidentally resolving to a
            // different model.
            return ParsedModelResult::default();
        }

        // Scope mode: recurse on prefix and warn
        let result = parse_model_pattern(prefix, available_models, options);
        if result.model.is_some() {
            return ParsedModelResult {
                warning: Some(format!(
                    "Invalid thinking level \"{}\" in pattern \"{}\". Using default instead.",
                    suffix, pattern
                )),
                thinking_level: None,
                ..result
            };
        }
        result
    }
}

/// Options for `parse_model_pattern` (upstream second parameter object).
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseModelPatternOptions {
    pub allow_invalid_thinking_level_fallback: bool,
}

/// A diagnostic from model scope resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelScopeDiagnostic {
    pub code: ModelScopeDiagnosticCode,
    pub message: String,
    pub pattern: String,
}

/// Diagnostic codes (upstream type union; all are warnings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelScopeDiagnosticCode {
    NoMatch,
    InvalidThinkingLevel,
}

/// Result of resolving model scope patterns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolveModelScopeResult {
    pub scoped_models: Vec<ScopedModel>,
    pub diagnostics: Vec<ModelScopeDiagnostic>,
}

/// Resolve model patterns to actual models with optional thinking levels.
/// Format: "pattern:level" where ":level" is optional. Glob patterns
/// (`*`, `?`, `[`) match against "provider/modelId" or bare model id
/// (case-insensitive, minimatch semantics via globset).
pub fn resolve_model_scope_from_models(
    patterns: &[String],
    models: &[Model],
) -> ResolveModelScopeResult {
    let available_models: Vec<Model> = models.to_vec();
    let mut scoped_models: Vec<ScopedModel> = Vec::new();
    let mut diagnostics: Vec<ModelScopeDiagnostic> = Vec::new();

    for pattern in patterns {
        // Check if pattern contains glob characters
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            // Extract optional thinking level suffix (e.g. "provider/*:high")
            let mut glob_pattern = pattern.as_str();
            let mut thinking_level: Option<ResolverThinkingLevel> = None;

            if let Some(colon_idx) = pattern.rfind(':') {
                let suffix = &pattern[colon_idx + 1..];
                if let Some(level) = parse_thinking_level(suffix) {
                    thinking_level = Some(level);
                    glob_pattern = &pattern[..colon_idx];
                }
            }

            if let Some(exact_match) =
                find_exact_model_reference_match(glob_pattern, &available_models)
            {
                if !scoped_models
                    .iter()
                    .any(|sm| models_are_equal(&sm.model, &exact_match))
                {
                    scoped_models.push(ScopedModel {
                        model: exact_match,
                        thinking_level,
                    });
                }
                continue;
            }

            // Match against "provider/modelId" format OR just model ID. This
            // allows "*sonnet*" to match without requiring "anthropic/*sonnet*".
            let glob_for = |source: &str| {
                // minimatch `nocase: true`
                let mut builder = GlobBuilder::new(source);
                builder.case_insensitive(true);
                builder.build().ok().map(|g| g.compile_matcher())
            };
            let full_matcher = glob_for(glob_pattern);
            let id_matcher = glob_for(glob_pattern);
            let matching_models: Vec<Model> = available_models
                .iter()
                .filter(|m| {
                    let full_id = format!("{}/{}", m.provider, m.id);
                    full_matcher
                        .as_ref()
                        .is_some_and(|mm| mm.is_match(&full_id))
                        || id_matcher.as_ref().is_some_and(|im| im.is_match(&m.id))
                })
                .cloned()
                .collect();

            if matching_models.is_empty() {
                diagnostics.push(ModelScopeDiagnostic {
                    code: ModelScopeDiagnosticCode::NoMatch,
                    message: format!("No models match pattern \"{}\"", pattern),
                    pattern: pattern.clone(),
                });
                continue;
            }

            for model in matching_models {
                if !scoped_models
                    .iter()
                    .any(|sm| models_are_equal(&sm.model, &model))
                {
                    scoped_models.push(ScopedModel {
                        model,
                        thinking_level,
                    });
                }
            }
            continue;
        }

        let parsed = parse_model_pattern(pattern, &available_models, None);

        if let Some(warning) = &parsed.warning {
            diagnostics.push(ModelScopeDiagnostic {
                code: ModelScopeDiagnosticCode::InvalidThinkingLevel,
                message: warning.clone(),
                pattern: pattern.clone(),
            });
        }

        let Some(model) = parsed.model else {
            diagnostics.push(ModelScopeDiagnostic {
                code: ModelScopeDiagnosticCode::NoMatch,
                message: format!("No models match pattern \"{}\"", pattern),
                pattern: pattern.clone(),
            });
            continue;
        };

        // Avoid duplicates
        if !scoped_models
            .iter()
            .any(|sm| models_are_equal(&sm.model, &model))
        {
            scoped_models.push(ScopedModel {
                model,
                thinking_level: parsed.thinking_level,
            });
        }
    }

    ResolveModelScopeResult {
        scoped_models,
        diagnostics,
    }
}

/// Result of resolving a model from CLI flags.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolveCliModelResult {
    pub model: Option<Model>,
    pub thinking_level: Option<ResolverThinkingLevel>,
    pub warning: Option<String>,
    /// Error message suitable for CLI display. When set, model will be None.
    pub error: Option<String>,
}

/// Resolve a single model from CLI flags.
///
/// Supports:
/// - `--provider <provider> --model <pattern>`
/// - `--model <provider>/<pattern>`
/// - Fuzzy matching (same rules as model scoping: exact id, then partial id/name)
///
/// Note: this does not apply the thinking level by itself, but it may *parse*
/// and return a thinking level from "<pattern>:<thinking>" so the caller can
/// apply it.
pub fn resolve_cli_model(options: ResolveCliModelOptions) -> ResolveCliModelResult {
    let ResolveCliModelOptions {
        cli_provider,
        cli_model,
        cli_thinking,
        models,
        auth,
    } = options;

    let Some(cli_model) = cli_model else {
        return ResolveCliModelResult::default();
    };

    // Important: use *all* models here, not just models with pre-configured
    // auth. This allows "--api-key" to be used for first-time setup.
    let available_models: Vec<Model> = models.to_vec();
    if available_models.is_empty() {
        return ResolveCliModelResult {
            error: Some(
                "No models available. Check your installation or add models to models.json."
                    .to_string(),
            ),
            ..Default::default()
        };
    }

    // Build canonical provider lookup (case-insensitive)
    let provider_map: BTreeMap<String, String> = available_models
        .iter()
        .map(|m| (m.provider.to_lowercase(), m.provider.clone()))
        .collect();

    let mut provider: Option<String> = cli_provider
        .as_deref()
        .and_then(|p| provider_map.get(&p.to_lowercase()).cloned());
    if let Some(cli_provider) = &cli_provider {
        if provider.is_none() {
            return ResolveCliModelResult {
                error: Some(format!(
                    "Unknown provider \"{}\". Use --list-models to see available providers/models.",
                    cli_provider
                )),
                ..Default::default()
            };
        }
    }

    // If no explicit --provider, try to interpret "provider/model" format
    // first. When the prefix before the first slash matches a known provider,
    // prefer that interpretation over matching models whose IDs literally
    // contain slashes (e.g. "zai/glm-5" should resolve to provider=zai,
    // model=glm-5, not to a vercel-ai-gateway model with id "zai/glm-5").
    let mut pattern = cli_model.clone();
    let mut inferred_provider = false;

    if provider.is_none() {
        if let Some(slash_index) = cli_model.find('/') {
            let maybe_provider = &cli_model[..slash_index];
            if let Some(canonical) = provider_map.get(&maybe_provider.to_lowercase()) {
                provider = Some(canonical.clone());
                pattern = cli_model[slash_index + 1..].to_string();
                inferred_provider = true;
            }
        }
    }

    // If no provider was inferred from the slash, try exact matches without
    // provider inference. This handles models whose IDs naturally contain
    // slashes (e.g. OpenRouter-style IDs). Bare exact IDs can exist in
    // multiple providers, so do not choose by catalog order. Prefer the sole
    // authenticated provider when there is one; otherwise require an explicit
    // provider to avoid silently selecting an unusable provider.
    if provider.is_none() {
        let lower = cli_model.to_lowercase();
        let exact_matches: Vec<Model> = available_models
            .iter()
            .filter(|m| {
                m.id.to_lowercase() == lower
                    || format!("{}/{}", m.provider, m.id).to_lowercase() == lower
            })
            .cloned()
            .collect();
        if exact_matches.len() == 1 {
            return ResolveCliModelResult {
                model: Some(exact_matches[0].clone()),
                ..Default::default()
            };
        }
        if exact_matches.len() > 1 {
            let authenticated_exact_matches: Vec<&Model> = exact_matches
                .iter()
                .filter(|m| auth.has_configured_auth(&m.provider))
                .collect();
            if authenticated_exact_matches.len() == 1 {
                return ResolveCliModelResult {
                    model: Some(authenticated_exact_matches[0].clone()),
                    ..Default::default()
                };
            }

            let mut refs: Vec<String> = exact_matches
                .iter()
                .map(|m| format!("{}/{}", m.provider, m.id))
                .collect();
            refs.sort();
            let matches_str = refs.join(", ");
            let auth_hint = if authenticated_exact_matches.is_empty() {
                "No matching provider is authenticated."
            } else {
                "More than one matching provider is authenticated."
            };
            return ResolveCliModelResult {
                error: Some(format!(
                    "Model \"{}\" is ambiguous across providers: {}. {} Use --provider or provider/model.",
                    cli_model, matches_str, auth_hint
                )),
                ..Default::default()
            };
        }
    }

    if cli_provider.is_some() {
        if let Some(provider) = &provider {
            // If both were provided, tolerate --model <provider>/<pattern> by
            // stripping the provider prefix
            let prefix = format!("{}/", provider);
            if cli_model.to_lowercase().starts_with(&prefix.to_lowercase()) {
                pattern = cli_model[prefix.len()..].to_string();
            }
        }
    }

    let candidates: Vec<Model> = provider
        .as_deref()
        .map(|p| {
            available_models
                .iter()
                .filter(|m| m.provider == p)
                .cloned()
                .collect()
        })
        .unwrap_or_else(|| available_models.clone());
    let parsed = parse_model_pattern(
        &pattern,
        &candidates,
        Some(ParseModelPatternOptions {
            allow_invalid_thinking_level_fallback: false,
        }),
    );

    if let Some(model) = &parsed.model {
        // If provider inference matched an unauthenticated provider/model pair,
        // prefer one exact raw model-id match that is authenticated. This
        // keeps "provider/model" syntax preferred when usable, but handles
        // models whose literal id starts with a known provider name.
        if inferred_provider && !auth.has_configured_auth(&model.provider) {
            let raw_exact_matches: Vec<Model> = available_models
                .iter()
                .filter(|m| {
                    m.id.to_lowercase() == cli_model.to_lowercase() && !models_are_equal(m, model)
                })
                .cloned()
                .collect();
            if !raw_exact_matches.is_empty() {
                let authenticated_raw_matches: Vec<&Model> = raw_exact_matches
                    .iter()
                    .filter(|m| auth.has_configured_auth(&m.provider))
                    .collect();
                if authenticated_raw_matches.len() == 1 {
                    return ResolveCliModelResult {
                        model: Some(authenticated_raw_matches[0].clone()),
                        ..Default::default()
                    };
                }
            }
        }
        return ResolveCliModelResult {
            model: parsed.model,
            thinking_level: parsed.thinking_level,
            warning: parsed.warning,
            error: None,
        };
    }

    // If we inferred a provider from the slash but found no match within that
    // provider, fall back to matching the full input as a raw model id across
    // all models. This handles OpenRouter-style IDs like "openai/gpt-4o:extended"
    // where "openai" looks like a provider but the full string is actually a
    // model id on openrouter.
    if inferred_provider {
        let lower = cli_model.to_lowercase();
        if let Some(exact) = available_models.iter().find(|m| {
            m.id.to_lowercase() == lower
                || format!("{}/{}", m.provider, m.id).to_lowercase() == lower
        }) {
            return ResolveCliModelResult {
                model: Some(exact.clone()),
                ..Default::default()
            };
        }
        // Also try parseModelPattern on the full input against all models
        let fallback = parse_model_pattern(
            &cli_model,
            &available_models,
            Some(ParseModelPatternOptions {
                allow_invalid_thinking_level_fallback: false,
            }),
        );
        if let Some(model) = fallback.model {
            return ResolveCliModelResult {
                model: Some(model),
                thinking_level: fallback.thinking_level,
                warning: fallback.warning,
                error: None,
            };
        }
    }

    if let Some(provider) = &provider {
        // Parse thinking level suffix from the pattern before building the
        // fallback model, but only when --thinking is not explicitly provided.
        // e.g. "zai-org/GLM-5.1-FP8:high" → modelId="zai-org/GLM-5.1-FP8",
        // fallbackThinking="high"
        let mut fallback_pattern = pattern.clone();
        let mut fallback_thinking: Option<ResolverThinkingLevel> = None;
        if cli_thinking.is_none() {
            if let Some(last_colon) = pattern.rfind(':') {
                let suffix = &pattern[last_colon + 1..];
                if let Some(level) = parse_thinking_level(suffix) {
                    fallback_pattern = pattern[..last_colon].to_string();
                    fallback_thinking = Some(level);
                }
            }
        }

        if let Some(mut fallback_model) =
            build_fallback_model(provider, &fallback_pattern, &available_models)
        {
            let requested_thinking = cli_thinking.or(fallback_thinking);
            if let Some(level) = requested_thinking {
                if level != ResolverThinkingLevel::Off {
                    fallback_model.reasoning = true;
                }
            }
            let fallback_warning = parsed.warning.as_ref().map_or_else(
                || {
                    format!(
                        "Model \"{}\" not found for provider \"{}\". Using custom model id.",
                        fallback_pattern, provider
                    )
                },
                |warning| {
                    format!(
                        "{} Model \"{}\" not found for provider \"{}\". Using custom model id.",
                        warning, fallback_pattern, provider
                    )
                },
            );
            return ResolveCliModelResult {
                model: Some(fallback_model),
                thinking_level: fallback_thinking,
                warning: Some(fallback_warning),
                error: None,
            };
        }
    }

    let display = provider
        .as_ref()
        .map_or_else(|| cli_model.clone(), |p| format!("{}/{}", p, pattern));
    ResolveCliModelResult {
        model: None,
        thinking_level: None,
        warning: parsed.warning,
        error: Some(format!(
            "Model \"{}\" not found. Use --list-models to see available models.",
            display
        )),
    }
}

/// Inputs for `resolve_cli_model` (upstream takes a ModelRuntime; the port
/// takes the model list and an auth-lookup directly).
pub struct ResolveCliModelOptions<'a> {
    pub cli_provider: Option<String>,
    pub cli_model: Option<String>,
    pub cli_thinking: Option<ResolverThinkingLevel>,
    pub models: &'a [Model],
    pub auth: &'a dyn AuthConfigured,
}

/// Result of finding the initial model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InitialModelResult {
    pub model: Option<Model>,
    pub thinking_level: ResolverThinkingLevel,
    pub fallback_message: Option<String>,
}

/// Snapshot of available models plus an auth lookup, replacing upstream's
/// `ModelRuntime` for the initial-selection path.
pub struct ModelRuntimeView<'a> {
    pub models: &'a [Model],
    pub available: &'a [Model],
    pub auth: &'a dyn AuthConfigured,
}

impl<'a> ModelRuntimeView<'a> {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model> {
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == model_id)
            .cloned()
    }
}

/// Find the initial model to use based on priority:
/// 1. CLI args (provider + model)
/// 2. First model from scoped models (if not continuing/resuming)
/// 3. Saved default from settings
/// 4. First available model with valid API key
///
/// divergence: upstream exits the process when CLI resolution errors; the
/// port returns the error message and no model.
pub fn find_initial_model(options: FindInitialModelOptions) -> InitialModelResult {
    let FindInitialModelOptions {
        cli_provider,
        cli_model,
        scoped_models,
        is_continuing,
        default_provider,
        default_model_id,
        default_thinking_level,
        model_thinking_levels,
        runtime,
    } = options;

    // 1. CLI args take priority
    if let (Some(cli_provider), Some(cli_model)) = (&cli_provider, &cli_model) {
        let resolved = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: Some(cli_provider.clone()),
            cli_model: Some(cli_model.clone()),
            cli_thinking: None,
            models: runtime.models,
            auth: runtime.auth,
        });
        if resolved.error.is_some() || resolved.model.is_some() {
            return InitialModelResult {
                model: resolved.model,
                thinking_level: DEFAULT_THINKING_LEVEL,
                fallback_message: resolved.error,
            };
        }
    }

    // 2. Use first model from scoped models (skip if continuing/resuming)
    if !scoped_models.is_empty() && !is_continuing {
        let scoped_model = &scoped_models[0];
        let per_model = model_thinking_levels
            .as_ref()
            .and_then(|map| {
                map.get(&format!(
                    "{}/{}",
                    scoped_model.model.provider, scoped_model.model.id
                ))
            })
            .copied()
            .flatten();
        return InitialModelResult {
            model: Some(scoped_model.model.clone()),
            thinking_level: scoped_model
                .thinking_level
                .or(per_model)
                .or(default_thinking_level)
                .unwrap_or(DEFAULT_THINKING_LEVEL),
            fallback_message: None,
        };
    }

    // 3. Try saved default from settings if auth is configured.
    if let (Some(default_provider), Some(default_model_id)) = (&default_provider, &default_model_id)
    {
        if let Some(found) = runtime.get_model(default_provider, default_model_id) {
            if runtime.auth.has_configured_auth(&found.provider) {
                let per_model = model_thinking_levels
                    .as_ref()
                    .and_then(|map| map.get(&format!("{}/{}", default_provider, default_model_id)))
                    .copied()
                    .flatten();
                let thinking_level = if let Some(per_model) = per_model {
                    per_model
                } else {
                    default_thinking_level.unwrap_or(DEFAULT_THINKING_LEVEL)
                };
                return InitialModelResult {
                    model: Some(found),
                    thinking_level,
                    fallback_message: None,
                };
            }
        }
    }

    // 4. Try first available model with valid API key
    if !runtime.available.is_empty() {
        // Try to find a default model from known providers
        for provider in KNOWN_PROVIDERS {
            let Some(default_id) = default_model_per_provider(provider) else {
                continue;
            };
            if let Some(matched) = runtime
                .available
                .iter()
                .find(|m| m.provider == provider && m.id == default_id)
            {
                return InitialModelResult {
                    model: Some(matched.clone()),
                    thinking_level: DEFAULT_THINKING_LEVEL,
                    fallback_message: None,
                };
            }
        }

        // If no default found, use first available
        return InitialModelResult {
            model: Some(runtime.available[0].clone()),
            thinking_level: DEFAULT_THINKING_LEVEL,
            fallback_message: None,
        };
    }

    // 5. No model found
    InitialModelResult {
        model: None,
        thinking_level: DEFAULT_THINKING_LEVEL,
        fallback_message: None,
    }
}

/// Inputs for `find_initial_model`.
pub struct FindInitialModelOptions<'a> {
    pub cli_provider: Option<String>,
    pub cli_model: Option<String>,
    pub scoped_models: Vec<ScopedModel>,
    pub is_continuing: bool,
    pub default_provider: Option<String>,
    pub default_model_id: Option<String>,
    pub default_thinking_level: Option<ResolverThinkingLevel>,
    pub model_thinking_levels: Option<BTreeMap<String, Option<ResolverThinkingLevel>>>,
    pub runtime: ModelRuntimeView<'a>,
}

/// Result of restoring a model from a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RestoreModelResult {
    pub model: Option<Model>,
    pub fallback_message: Option<String>,
}

/// Restore model from session, with fallback to available models.
///
/// divergence: upstream prints console messages when `shouldPrintMessages`;
/// the port is pure and returns only the outcome.
pub fn restore_model_from_session(
    saved_provider: &str,
    saved_model_id: &str,
    current_model: Option<&Model>,
    runtime: &ModelRuntimeView,
) -> RestoreModelResult {
    let restored_model = runtime.get_model(saved_provider, saved_model_id);

    // Check if restored model exists and still has auth configured
    let found = restored_model.is_some();
    let has_auth = restored_model
        .as_ref()
        .is_some_and(|m| runtime.auth.has_configured_auth(&m.provider));

    if let Some(model) = restored_model.filter(|_| has_auth) {
        return RestoreModelResult {
            model: Some(model),
            fallback_message: None,
        };
    }

    // Model not found or no API key - fall back
    let reason = if !found {
        "model no longer exists"
    } else {
        "no auth configured"
    };

    // If we already have a model, use it as fallback
    if let Some(current) = current_model {
        return RestoreModelResult {
            model: Some(current.clone()),
            fallback_message: Some(format!(
                "Could not restore model {}/{} ({}). Using {}/{}.",
                saved_provider, saved_model_id, reason, current.provider, current.id
            )),
        };
    }

    // Try to find any available model
    if !runtime.available.is_empty() {
        // Try to find a default model from known providers
        let mut fallback_model: Option<Model> = None;
        for provider in KNOWN_PROVIDERS {
            let Some(default_id) = default_model_per_provider(provider) else {
                continue;
            };
            if let Some(matched) = runtime
                .available
                .iter()
                .find(|m| m.provider == provider && m.id == default_id)
            {
                fallback_model = Some(matched.clone());
                break;
            }
        }

        // If no default found, use first available
        let fallback_model = fallback_model.unwrap_or_else(|| runtime.available[0].clone());

        return RestoreModelResult {
            fallback_message: Some(format!(
                "Could not restore model {}/{} ({}). Using {}/{}.",
                saved_provider, saved_model_id, reason, fallback_model.provider, fallback_model.id
            )),
            model: Some(fallback_model),
        };
    }

    // No models available
    RestoreModelResult::default()
}

/// Convenience constructor for tests: build a `ModelRuntimeView` from an Arc
/// slice pair and an auth provider.
pub fn runtime_view<'a>(
    models: &'a [Model],
    available: &'a [Model],
    auth: &'a dyn AuthConfigured,
) -> ModelRuntimeView<'a> {
    ModelRuntimeView {
        models,
        available,
        auth,
    }
}
