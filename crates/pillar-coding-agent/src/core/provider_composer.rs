//! Port of packages/coding-agent/src/core/provider-composer.ts (pi v0.84.3).
//!
//! Composes built-in provider, models.json, and extension layers into a
//! runtime `Provider`: model list merging (baseUrl/compat overrides, custom
//! model upserts, modelOverrides as the topmost layer), composed api-key/OAuth
//! auth, and header resolution helpers.
//!
//! divergence: upstream extension providers carry JS callbacks
//! (`streamSimple`, `login`, `refreshModels`, `modifyModels`); the port keeps
//! the data-only extension shape (models/baseUrl/headers/authHeader/apiKey)
//! and stream dispatch delegates to the builtin API adapters via
//! `pillar_ai::api_dispatch::streams_for`. The legacy extension OAuth
//! adapter (`adaptOAuth`) is a JS-bridge concern and is not ported.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use pillar_ai::auth_types::{
    ApiKeyAuth, ApiKeyAuthInput, AuthContext, AuthResult, ModelAuth, OAuthAuth, ProviderAuth,
    ProviderAuthInteraction,
};
use pillar_ai::error::AiError;
use pillar_ai::models::{Provider, ProviderApi};
use pillar_ai::types::{Model, ModelCompat, ProviderHeaders};

use crate::core::model_config::{
    ModelsJsonModel, ModelsJsonModelOverride, ModelsJsonProvider, ProviderCompatJson,
};
use crate::core::resolve_config_value::{
    Env, get_config_value_env_var_names, is_command_config_value, is_config_value_configured,
    resolve_config_value_or_throw, resolve_headers_or_throw,
};

/// Re-exported alias (upstream `clearApiKeyCache = clearConfigValueCache`).
pub fn clear_api_key_cache() {
    crate::core::resolve_config_value::clear_config_value_cache();
}

/// Data-only extension provider input (upstream `ProviderConfigInput` minus
/// the JS callbacks).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderConfigInput {
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api: Option<String>,
    pub auth_header: Option<bool>,
    pub headers: Option<BTreeMap<String, String>>,
    pub models: Option<Vec<ExtensionModel>>,
}

/// Extension model definition (upstream inline models shape).
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionModel {
    pub id: String,
    pub name: String,
    pub api: Option<String>,
    pub base_url: Option<String>,
    pub reasoning: bool,
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    pub input: Vec<String>,
    pub context_window: u64,
    pub max_tokens: u64,
    pub sampling_params: Option<BTreeMap<String, serde_json::Value>>,
    pub headers: Option<BTreeMap<String, String>>,
    pub compat: Option<ProviderCompatJson>,
}

/// Auth status of a configured API key (upstream `AuthStatus`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthStatus {
    pub configured: bool,
    pub source: Option<AuthStatusSource>,
    pub label: Option<String>,
}

/// Where a configured key comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatusSource {
    Stored,
    Runtime,
    ModelsJsonKey,
    ModelsJsonCommand,
    Environment,
    Fallback,
}

impl AuthStatusSource {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthStatusSource::Stored => "stored",
            AuthStatusSource::Runtime => "runtime",
            AuthStatusSource::ModelsJsonKey => "models_json_key",
            AuthStatusSource::ModelsJsonCommand => "models_json_command",
            AuthStatusSource::Environment => "environment",
            AuthStatusSource::Fallback => "fallback",
        }
    }
}

// --- compat merging ----------------------------------------------------------

/// Flatten a `ModelCompat` into a variant-tagged field map for merging.
/// Upstream merges the compat object by spread plus deep-merge for the four
/// routing/chat-template keys; the port converts through serde_json values.
fn compat_to_value(compat: Option<&ModelCompat>) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    if let Some(compat) = compat {
        let value = serde_json::to_value(compat).ok();
        if let Some(serde_json::Value::Object(obj)) = value {
            map = obj;
        }
    }
    map
}

fn compat_from_value(map: serde_json::Map<String, serde_json::Value>) -> Option<ModelCompat> {
    serde_json::from_value(serde_json::Value::Object(map)).ok()
}

const DEEP_MERGE_KEYS: [&str; 4] = [
    "openRouterRouting",
    "vercelGatewayRouting",
    "chatTemplateKwargs",
    "chatTemplateArgs",
];

/// Convert a models.json compat union into a runtime `ModelCompat`
/// (the field structs share the serde shape; untagged on both sides).
fn compat_json_to_model(compat: &ProviderCompatJson) -> Option<ModelCompat> {
    match compat {
        ProviderCompatJson::OpenaiCompletions(c) => serde_json::to_value(c.as_ref())
            .ok()
            .and_then(|v| serde_json::from_value(v).ok()),
        ProviderCompatJson::OpenaiResponses(c) => serde_json::to_value(c.as_ref())
            .ok()
            .and_then(|v| serde_json::from_value(v).ok()),
        ProviderCompatJson::AnthropicMessages(c) => serde_json::to_value(c.as_ref())
            .ok()
            .and_then(|v| serde_json::from_value(v).ok()),
    }
}

fn merge_compat(
    base: Option<&ModelCompat>,
    override_compat: Option<&ProviderCompatJson>,
) -> Option<ModelCompat> {
    let Some(override_compat) = override_compat else {
        return base.cloned();
    };
    let Some(override_model) = compat_json_to_model(override_compat) else {
        return base.cloned();
    };
    let Ok(override_value) = serde_json::to_value(&override_model) else {
        return base.cloned();
    };
    let serde_json::Value::Object(override_map) = override_value else {
        return base.cloned();
    };
    let mut merged = compat_to_value(base);
    for (key, value) in override_map {
        if DEEP_MERGE_KEYS.contains(&key.as_str()) {
            let base_value = merged.get(&key).cloned();
            let deep = match (base_value, &value) {
                (Some(serde_json::Value::Object(b)), serde_json::Value::Object(o)) => {
                    let mut m = b.clone();
                    for (k, v) in o {
                        m.insert(k.clone(), v.clone());
                    }
                    serde_json::Value::Object(m)
                }
                _ => value.clone(),
            };
            merged.insert(key, deep);
        } else {
            merged.insert(key, value);
        }
    }
    compat_from_value(merged).or(Some(override_model))
}

// --- model overrides -----------------------------------------------------------

fn thinking_level_map_json_to_model(
    map: &BTreeMap<String, Option<String>>,
) -> Option<pillar_ai::types::ThinkingLevelMap> {
    let mut out = BTreeMap::new();
    for (key, value) in map {
        let level = serde_json::from_value::<pillar_ai::types::ModelThinkingLevel>(
            serde_json::Value::String(key.clone()),
        )
        .ok()?;
        out.insert(level, value.clone());
    }
    Some(out)
}

fn merge_thinking_level_maps(
    base: Option<&pillar_ai::types::ThinkingLevelMap>,
    override_map: Option<&BTreeMap<String, Option<String>>>,
) -> Option<pillar_ai::types::ThinkingLevelMap> {
    let Some(override_map) = override_map else {
        return base.cloned();
    };
    let override_converted = thinking_level_map_json_to_model(override_map)?;
    let mut merged = base.cloned().unwrap_or_default();
    for (level, target) in override_converted {
        merged.insert(level, target);
    }
    Some(merged)
}

fn apply_model_override(model: &Model, override_cfg: &ModelsJsonModelOverride) -> Model {
    let mut out = model.clone();
    if let Some(name) = &override_cfg.name {
        out.name = name.clone();
    }
    if let Some(reasoning) = override_cfg.reasoning {
        out.reasoning = reasoning;
    }
    out.thinking_level_map = merge_thinking_level_maps(
        model.thinking_level_map.as_ref(),
        override_cfg.thinking_level_map.as_ref(),
    );
    if let Some(input) = &override_cfg.input {
        out.input = input.clone();
    }
    if let Some(cost) = &override_cfg.cost {
        out.cost.rates.input = cost.input.unwrap_or(model.cost.rates.input);
        out.cost.rates.output = cost.output.unwrap_or(model.cost.rates.output);
        out.cost.rates.cache_read = cost.cache_read.unwrap_or(model.cost.rates.cache_read);
        out.cost.rates.cache_write = cost.cache_write.unwrap_or(model.cost.rates.cache_write);
        if cost.tiers.is_some() {
            out.cost.tiers = cost.tiers.as_ref().map(|tiers| {
                tiers
                    .iter()
                    .map(|tier| pillar_ai::types::ModelCostTier {
                        input_tokens_above: tier.input_tokens_above as u64,
                        rates: pillar_ai::types::ModelCostRates {
                            input: tier.input,
                            output: tier.output,
                            cache_read: tier.cache_read,
                            cache_write: tier.cache_write,
                        },
                    })
                    .collect()
            });
        }
    }
    if let Some(context_window) = override_cfg.context_window {
        out.context_window = context_window as u64;
    }
    if let Some(max_tokens) = override_cfg.max_tokens {
        out.max_tokens = max_tokens as u64;
    }
    if let Some(sampling_params) = &override_cfg.sampling_params {
        let mut merged = model.sampling_params.clone().unwrap_or_default();
        for (key, value) in sampling_params {
            merged.insert(key.clone(), value.clone());
        }
        out.sampling_params = Some(merged);
    }
    out.compat = merge_compat(model.compat.as_ref(), override_cfg.compat.as_ref());
    out
}

/// Errors from provider composition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeError(pub String);

impl std::fmt::Display for ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ComposeError {}

fn model_from_json(
    provider_id: &str,
    definition: &ModelsJsonModel,
    provider_config: &ModelsJsonProvider,
    defaults: Option<&Model>,
) -> Result<Model, ComposeError> {
    let api = definition
        .api
        .as_ref()
        .or(provider_config.api.as_ref())
        .or_else(|| defaults.map(|d| &d.api))
        .cloned();
    let Some(api) = api else {
        return Err(ComposeError(format!(
            "Provider {}, model {}: no \"api\" specified. Set at provider or model level.",
            provider_id, definition.id
        )));
    };
    let base_url = definition
        .base_url
        .as_ref()
        .or(provider_config.base_url.as_ref())
        .or_else(|| defaults.map(|d| &d.base_url))
        .cloned();
    let Some(base_url) = base_url else {
        return Err(ComposeError(format!(
            "Provider {}: \"baseUrl\" is required when defining custom models.",
            provider_id
        )));
    };
    if let Some(context_window) = definition.context_window {
        if context_window <= 0.0 {
            return Err(ComposeError(format!(
                "Provider {}, model {}: invalid contextWindow",
                provider_id, definition.id
            )));
        }
    }
    if let Some(max_tokens) = definition.max_tokens {
        if max_tokens <= 0.0 {
            return Err(ComposeError(format!(
                "Provider {}, model {}: invalid maxTokens",
                provider_id, definition.id
            )));
        }
    }
    let compat = merge_compat(None, provider_config.compat.as_ref());
    let compat = merge_compat(compat.as_ref(), definition.compat.as_ref());
    Ok(Model {
        id: definition.id.clone(),
        name: definition
            .name
            .clone()
            .unwrap_or_else(|| definition.id.clone()),
        api,
        provider: provider_id.to_string(),
        base_url,
        reasoning: definition.reasoning.unwrap_or(false),
        thinking_level_map: definition
            .thinking_level_map
            .as_ref()
            .and_then(thinking_level_map_json_to_model),
        input: definition
            .input
            .clone()
            .unwrap_or_else(|| vec!["text".to_string()]),
        cost: definition.cost.as_ref().map_or(
            pillar_ai::types::ModelCost {
                rates: pillar_ai::types::ModelCostRates::default(),
                tiers: None,
            },
            |cost| pillar_ai::types::ModelCost {
                rates: pillar_ai::types::ModelCostRates {
                    input: cost.input,
                    output: cost.output,
                    cache_read: cost.cache_read,
                    cache_write: cost.cache_write,
                },
                tiers: cost.tiers.as_ref().map(|tiers| {
                    tiers
                        .iter()
                        .map(|tier| pillar_ai::types::ModelCostTier {
                            input_tokens_above: tier.input_tokens_above as u64,
                            rates: pillar_ai::types::ModelCostRates {
                                input: tier.input,
                                output: tier.output,
                                cache_read: tier.cache_read,
                                cache_write: tier.cache_write,
                            },
                        })
                        .collect()
                }),
            },
        ),
        context_window: definition.context_window.map_or(128_000, |v| v as u64),
        max_tokens: definition.max_tokens.map_or(16_384, |v| v as u64),
        sampling_params: definition.sampling_params.clone(),
        headers: None,
        compat,
    })
}

/// Apply the models.json provider layer over the base models.
pub fn apply_models_json(
    provider_id: &str,
    base_models: &[Model],
    config: Option<&ModelsJsonProvider>,
) -> Result<Vec<Model>, ComposeError> {
    let Some(config) = config else {
        return Ok(base_models.to_vec());
    };
    if config.oauth.is_some() && config.base_url.is_none() {
        return Err(ComposeError(format!(
            "Provider {}: \"baseUrl\" is required when \"oauth\" is set.",
            provider_id
        )));
    }
    let has_overrides = config
        .model_overrides
        .as_ref()
        .is_some_and(|m| !m.is_empty());
    if config.models.as_ref().is_none_or(|m| m.is_empty())
        && config.base_url.is_none()
        && config.headers.is_none()
        && config.compat.is_none()
        && !has_overrides
        && config.api_key.is_none()
        && config.oauth.is_none()
        && config.auth_header.is_none()
    {
        return Err(ComposeError(format!(
            "Provider {}: must specify \"baseUrl\", \"headers\", \"compat\", \"modelOverrides\", or \"models\".",
            provider_id
        )));
    }

    let mut models: Vec<Model> = base_models
        .iter()
        .map(|model| {
            let mut out = model.clone();
            out.base_url = config
                .base_url
                .clone()
                .unwrap_or_else(|| model.base_url.clone());
            out.compat = merge_compat(model.compat.as_ref(), config.compat.as_ref());
            out
        })
        .collect();
    for definition in config.models.iter().flatten() {
        let existing_index = models.iter().position(|model| model.id == definition.id);
        let defaults = existing_index.map_or(models.first(), |idx| Some(&models[idx]));
        let model = model_from_json(provider_id, definition, config, defaults)?;
        if let Some(idx) = existing_index {
            models[idx] = model;
        } else {
            models.push(model);
        }
    }
    Ok(models)
}

/// Apply the extension provider layer over the (models.json-applied) models.
pub fn apply_extension(
    provider_id: &str,
    models: &[Model],
    config: Option<&ProviderConfigInput>,
) -> Result<Vec<Model>, ComposeError> {
    let Some(config) = config else {
        return Ok(models.to_vec());
    };
    let Some(extension_models) = &config.models else {
        if let Some(base_url) = &config.base_url {
            return Ok(models
                .iter()
                .map(|model| {
                    let mut out = model.clone();
                    out.base_url = base_url.clone();
                    out
                })
                .collect());
        }
        return Ok(models.to_vec());
    };
    extension_models
        .iter()
        .map(|definition| {
            let defaults = models
                .iter()
                .find(|model| model.id == definition.id)
                .or_else(|| models.first());
            let api = definition
                .api
                .as_ref()
                .or(config.api.as_ref())
                .or_else(|| defaults.map(|d| &d.api))
                .cloned();
            let Some(api) = api else {
                return Err(ComposeError(format!(
                    "Provider {}, model {}: no \"api\" specified. Set at provider or model level.",
                    provider_id, definition.id
                )));
            };
            let base_url = definition
                .base_url
                .as_ref()
                .or(config.base_url.as_ref())
                .or_else(|| defaults.map(|d| &d.base_url))
                .cloned();
            let Some(base_url) = base_url else {
                return Err(ComposeError(format!(
                    "Provider {}: \"baseUrl\" is required when defining custom models.",
                    provider_id
                )));
            };
            let cost = pillar_ai::types::ModelCost {
                rates: pillar_ai::types::ModelCostRates {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                tiers: None,
            };
            Ok(Model {
                id: definition.id.clone(),
                name: definition.name.clone(),
                api,
                provider: provider_id.to_string(),
                base_url,
                reasoning: definition.reasoning,
                thinking_level_map: definition
                    .thinking_level_map
                    .as_ref()
                    .and_then(thinking_level_map_json_to_model),
                input: definition.input.clone(),
                cost,
                context_window: definition.context_window,
                max_tokens: definition.max_tokens,
                sampling_params: definition.sampling_params.clone(),
                headers: None,
                compat: merge_compat(None, definition.compat.as_ref()),
            })
        })
        .collect()
}

/// Validate an extension provider registration eagerly.
pub fn validate_extension_provider(
    provider_id: &str,
    base_models: &[Model],
    models_config: Option<&ModelsJsonProvider>,
    extension: &ProviderConfigInput,
) -> Result<(), ComposeError> {
    apply_extension(
        provider_id,
        &apply_models_json(provider_id, base_models, models_config)?,
        Some(extension),
    )
    .map(|_| ())
}

// --- header/auth helpers --------------------------------------------------------

fn configured_api_key<'a>(
    config: Option<&'a ModelsJsonProvider>,
    extension: Option<&'a ProviderConfigInput>,
) -> Option<&'a str> {
    extension
        .and_then(|e| e.api_key.as_deref())
        .or_else(|| config.and_then(|c| c.api_key.as_deref()))
}

fn configured_headers(
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
) -> Option<BTreeMap<String, String>> {
    let has_any = config.as_ref().is_some_and(|c| c.headers.is_some())
        || extension.as_ref().is_some_and(|e| e.headers.is_some());
    if !has_any {
        return None;
    }
    let mut merged = BTreeMap::new();
    if let Some(headers) = config.and_then(|c| c.headers.as_ref()) {
        for (key, value) in headers {
            merged.insert(key.clone(), value.clone());
        }
    }
    if let Some(headers) = extension.and_then(|e| e.headers.as_ref()) {
        for (key, value) in headers {
            merged.insert(key.clone(), value.clone());
        }
    }
    Some(merged)
}

fn configured_auth_header(
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
) -> bool {
    extension
        .and_then(|e| e.auth_header)
        .or(config.and_then(|c| c.auth_header))
        .unwrap_or(false)
}

fn with_configured_auth(
    auth: &ModelAuth,
    headers: Option<&ProviderHeaders>,
    auth_header: bool,
) -> Result<ModelAuth, ComposeError> {
    let mut merged_headers: ProviderHeaders = auth.headers.clone().unwrap_or_default();
    if let Some(headers) = headers {
        for (key, value) in headers {
            merged_headers.insert(key.clone(), value.clone());
        }
    }
    let merged = if merged_headers.is_empty() {
        None
    } else {
        Some(merged_headers)
    };
    let mut out = auth.clone();
    out.headers = merged;
    if auth_header {
        let Some(api_key) = &out.api_key else {
            return Err(ComposeError(
                "authHeader requires a resolved API key".to_string(),
            ));
        };
        let mut headers = out.headers.clone().unwrap_or_default();
        headers.insert(
            "Authorization".to_string(),
            Some(format!("Bearer {}", api_key)),
        );
        out.headers = Some(headers);
    }
    Ok(out)
}

async fn config_context_env(
    values: &[&String],
    ctx: &dyn AuthContext,
    explicit: Option<&Env>,
) -> Option<Env> {
    let mut env: Env = explicit.cloned().unwrap_or_default();
    for value in values {
        for name in get_config_value_env_var_names(value) {
            if env.contains_key(&name) {
                continue;
            }
            if let Some(value) = ctx.env(&name).await {
                env.insert(name, value);
            }
        }
    }
    (!env.is_empty()).then_some(env)
}

// --- composed auth ------------------------------------------------------------------

/// Env source for resolving configured values without an `AuthContext`
/// (tests, callers with a fixed environment).
#[derive(Debug, Clone, Copy)]
pub struct FixedEnvAuthContext<'a> {
    pub env: &'a Env,
}

#[async_trait]
impl<'a> AuthContext for FixedEnvAuthContext<'a> {
    async fn env(&self, name: &str) -> Option<String> {
        self.env.get(name).cloned()
    }
    async fn file_exists(&self, _path: &str) -> bool {
        false
    }
}

/// Composed api-key auth (upstream `composeApiKeyAuth`): inherits the base
/// provider's auth when present, otherwise resolves the configured key from
/// models.json/extension (literal, `$VAR`, or `!command`), merging configured
/// headers and honoring `authHeader`.
pub struct ComposedApiKeyAuth {
    pub name: String,
    pub provider_id: String,
    /// The base provider's method, when there is one (upstream `inherited`):
    /// its `login` wins over the fabricated "Enter API key" prompt.
    pub inherited: Option<Arc<dyn ApiKeyAuth>>,
    pub raw_key: Option<String>,
    pub raw_headers: Option<BTreeMap<String, String>>,
    pub auth_header: bool,
}

#[async_trait]
impl pillar_ai::auth_types::ApiKeyAuth for ComposedApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    /// Upstream `inherited?.login ?? (interaction) => prompt("Enter API key")`:
    /// a provider with no base method still gets an interactive login, which
    /// is what `/login` needs for a models.json / extension provider.
    async fn login(
        &self,
        interaction: &ProviderAuthInteraction,
    ) -> Result<Option<pillar_ai::auth_types::ApiKeyCredential>, AiError> {
        if let Some(inherited) = &self.inherited {
            return inherited.login(interaction).await;
        }
        interaction.signal.throw_if_aborted()?;
        let key = interaction
            .interaction
            .prompt(&pillar_ai::auth_types::AuthPrompt::Secret {
                message: "Enter API key".to_string(),
                placeholder: None,
            })
            .await?;
        interaction.signal.throw_if_aborted()?;
        Ok(Some(pillar_ai::auth_types::ApiKeyCredential {
            key: Some(key),
            env: None,
        }))
    }

    fn has_login(&self) -> bool {
        self.inherited
            .as_ref()
            .is_none_or(|auth| auth.has_login())
    }

    async fn check(
        &self,
        input: &ApiKeyAuthInput<'_>,
    ) -> Result<Option<pillar_ai::auth_types::AuthCheck>, AiError> {
        let Some(raw_key) = &self.raw_key else {
            return Ok(None);
        };
        if is_command_config_value(raw_key) {
            return Ok(Some(pillar_ai::auth_types::AuthCheck {
                source: Some("configured API key".to_string()),
                kind: "api_key".to_string(),
            }));
        }
        for name in get_config_value_env_var_names(raw_key) {
            if input.ctx.env(&name).await.is_none() {
                return Ok(None);
            }
        }
        Ok(Some(pillar_ai::auth_types::AuthCheck {
            source: Some("configured API key".to_string()),
            kind: "api_key".to_string(),
        }))
    }

    async fn resolve(&self, input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthResult>, AiError> {
        let result = match input.credential {
            Some(credential) => match &credential.key {
                Some(key) => AuthResult {
                    auth: ModelAuth {
                        api_key: Some(key.clone()),
                        ..Default::default()
                    },
                    env: credential.env.clone(),
                    source: Some("stored credential".to_string()),
                },
                None => return Ok(None),
            },
            None => {
                let Some(raw_key) = &self.raw_key else {
                    return Ok(None);
                };
                let env = config_context_env(&[raw_key], input.ctx, None).await;
                let key = resolve_config_value_or_throw(
                    raw_key,
                    &format!("API key for provider \"{}\"", self.provider_id),
                    env.as_ref(),
                )
                .map_err(AiError::Other)?;
                AuthResult {
                    auth: ModelAuth {
                        api_key: Some(key),
                        ..Default::default()
                    },
                    env: None,
                    source: Some("configured API key".to_string()),
                }
            }
        };
        let explicit_env: Env = result
            .env
            .clone()
            .or_else(|| input.credential.and_then(|c| c.env.clone()))
            .unwrap_or_default();
        let header_env = config_context_env(
            &self
                .raw_headers
                .as_ref()
                .map(|h| h.values().collect::<Vec<_>>())
                .unwrap_or_default(),
            input.ctx,
            Some(&explicit_env),
        )
        .await;
        let headers = resolve_headers_or_throw(
            self.raw_headers.as_ref(),
            &format!("provider \"{}\"", self.provider_id),
            header_env.as_ref(),
        )
        .map_err(AiError::Other)?;
        let provider_headers: Option<ProviderHeaders> =
            headers.map(|h| h.into_iter().map(|(k, v)| (k, Some(v))).collect());
        let auth = with_configured_auth(&result.auth, provider_headers.as_ref(), self.auth_header)
            .map_err(|e| AiError::Other(e.0))?;
        Ok(Some(AuthResult { auth, ..result }))
    }
}

/// Compose the api-key auth for a provider. Returns None for OAuth-only
/// providers with no inherited key source (upstream: no fabricated login).
pub fn compose_api_key_auth(
    provider_id: &str,
    base_auth: Option<&ProviderAuth>,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
) -> Option<Arc<ComposedApiKeyAuth>> {
    let inherited = base_auth.and_then(|auth| auth.api_key.as_ref());
    let raw_key = configured_api_key(config, extension).map(str::to_string);
    let oauth = extension
        .and_then(|e| e.auth_header)
        .map_or(base_auth.and_then(|auth| auth.oauth.clone()), |_| {
            base_auth.and_then(|auth| auth.oauth.clone())
        });
    // OAuth-only providers get no fabricated API-key login method.
    if inherited.is_none() && raw_key.is_none() && oauth.is_some() {
        return None;
    }
    let raw_headers = configured_headers(config, extension);
    let auth_header = configured_auth_header(config, extension);
    Some(Arc::new(ComposedApiKeyAuth {
        // Upstream `inherited?.name ?? "API key"`.
        name: inherited
            .map(|auth| auth.name().to_string())
            .unwrap_or_else(|| "API key".to_string()),
        provider_id: provider_id.to_string(),
        inherited: inherited.cloned(),
        raw_key,
        raw_headers,
        auth_header,
    }))
}

/// Compose the OAuth auth for a provider: the extension layer in upstream
/// adapts JS extension OAuth; the port passes through the base OAuth only.
pub fn compose_oauth_auth(
    _provider_id: &str,
    base_auth: Option<&ProviderAuth>,
    _config: Option<&ModelsJsonProvider>,
    _extension: Option<&ProviderConfigInput>,
) -> Option<Arc<dyn OAuthAuth>> {
    base_auth.and_then(|auth| auth.oauth.clone())
}

fn raw_model_headers(
    model: &Model,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
) -> Option<BTreeMap<String, String>> {
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    if let Some(config) = config {
        if let Some(overrides) = &config.model_overrides {
            if let Some(override_cfg) = overrides.get(&model.id) {
                if let Some(model_headers) = &override_cfg.headers {
                    for (key, value) in model_headers {
                        headers.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        if let Some(definitions) = &config.models {
            if let Some(definition) = definitions.iter().find(|entry| entry.id == model.id) {
                if let Some(model_headers) = &definition.headers {
                    for (key, value) in model_headers {
                        headers.insert(key.clone(), value.clone());
                    }
                }
            }
        }
    }
    if let Some(extension) = extension {
        if let Some(models) = &extension.models {
            if let Some(extension_model) = models.iter().find(|entry| entry.id == model.id) {
                if let Some(model_headers) = &extension_model.headers {
                    for (key, value) in model_headers {
                        headers.insert(key.clone(), value.clone());
                    }
                }
            }
        }
    }
    (!headers.is_empty()).then_some(headers)
}

/// Resolve per-model configured headers (upstream
/// `resolveConfiguredModelHeaders`).
pub fn resolve_configured_model_headers(
    model: &Model,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
    env: Option<&Env>,
) -> Result<Option<BTreeMap<String, String>>, ComposeError> {
    resolve_headers_or_throw(
        raw_model_headers(model, config, extension).as_ref(),
        &format!("model \"{}/{}\"", model.provider, model.id),
        env,
    )
    .map_err(ComposeError)
}

/// Request config for compatibility dispatch (upstream
/// `CompatibilityRequestConfig`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompatibilityRequestConfig {
    pub headers: Option<ProviderHeaders>,
    pub auth_header: bool,
}

/// Merge model headers with configured headers and the authHeader flag.
pub fn resolve_compatibility_request_config(
    model: &Model,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
) -> Result<CompatibilityRequestConfig, ComposeError> {
    let mut configured = configured_headers(config, extension).unwrap_or_default();
    if let Some(raw) = raw_model_headers(model, config, extension) {
        for (key, value) in raw {
            configured.insert(key, value);
        }
    }
    let configured = (!configured.is_empty()).then_some(configured);
    let configured = resolve_headers_or_throw(
        configured.as_ref(),
        &format!("model \"{}/{}\"", model.provider, model.id),
        None,
    )
    .map_err(ComposeError)?;
    let mut headers: ProviderHeaders = model.headers.clone().unwrap_or_default();
    if let Some(configured) = &configured {
        for (key, value) in configured {
            headers.insert(key.clone(), Some(value.clone()));
        }
    }
    Ok(CompatibilityRequestConfig {
        headers: (!headers.is_empty() || model.headers.is_some() || configured.is_some())
            .then_some(headers)
            .filter(|_| model.headers.is_some() || configured.is_some()),
        auth_header: configured_auth_header(config, extension),
    })
}

/// Auth status of the configured API key (upstream
/// `configuredRequestAuthStatus`).
pub fn configured_request_auth_status(
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
) -> Option<AuthStatus> {
    let value = configured_api_key(config, extension)?;
    if is_command_config_value(value) {
        return Some(AuthStatus {
            configured: true,
            source: Some(AuthStatusSource::ModelsJsonCommand),
            label: None,
        });
    }
    let names = get_config_value_env_var_names(value);
    if !names.is_empty() {
        return Some(if is_config_value_configured(value, None) {
            AuthStatus {
                configured: true,
                source: Some(AuthStatusSource::Environment),
                label: Some(names.join(", ")),
            }
        } else {
            AuthStatus {
                configured: false,
                source: None,
                label: None,
            }
        });
    }
    Some(AuthStatus {
        configured: true,
        source: Some(if extension.is_some_and(|e| e.api_key.is_some()) {
            AuthStatusSource::Fallback
        } else {
            AuthStatusSource::ModelsJsonKey
        }),
        label: None,
    })
}

// --- provider composition ---------------------------------------------------------------

/// Compose built-in, models.json, and extension layers into a runtime
/// `Provider` without reading credentials.
///
/// divergence: the port's extension layer is data-only (no `streamSimple`,
/// OAuth adapter, or `refreshModels` callbacks); stream dispatch falls
/// through to the builtin API adapters.
pub fn compose_model_provider(
    provider_id: &str,
    base: Option<&Provider>,
    model_config: &crate::core::model_config::ModelConfig,
    extension: Option<&ProviderConfigInput>,
) -> Result<Provider, ComposeError> {
    let config = model_config.get_provider(provider_id);

    let base_models: Vec<Model> = base.map(|b| (b.get_models)()).unwrap_or_default();
    let get_models = {
        let provider_id = provider_id.to_string();
        let config = config.cloned();
        let extension = extension.cloned();
        let base_models = base_models.clone();
        move || {
            let mut models = apply_extension(
                &provider_id,
                &apply_models_json(&provider_id, &base_models, config.as_ref()).unwrap_or_default(),
                extension.as_ref(),
            )
            .unwrap_or_default();
            if let Some(config) = &config {
                models = models
                    .into_iter()
                    .map(|model| {
                        if let Some(override_cfg) = config
                            .model_overrides
                            .as_ref()
                            .and_then(|m| m.get(&model.id))
                        {
                            apply_model_override(&model, override_cfg)
                        } else {
                            model
                        }
                    })
                    .collect();
            }
            models
        }
    };

    // Validate eagerly so registration/reload reports structural errors now.
    let validated_models = apply_extension(
        provider_id,
        &apply_models_json(provider_id, &base_models, config)?,
        extension,
    )?;

    let base_auth = base.map(|b| b.auth.clone());
    let api_key = compose_api_key_auth(provider_id, base_auth.as_ref(), config, extension);
    let oauth = compose_oauth_auth(provider_id, base_auth.as_ref(), config, extension);
    if api_key.is_none() && oauth.is_none() {
        return Err(ComposeError(format!(
            "Provider {}: no authentication method configured.",
            provider_id
        )));
    }

    let base_api = base.map(|b| b.api.clone()).unwrap_or_default();
    let streams: ProviderApi = match base {
        // Base provider dispatches its own APIs; composition reuses them.
        Some(_) => base_api,
        // Config-only providers resolve the API per model, like upstream's
        // lazy `getApiProvider(model.api)` fallback (previously this was
        // `ProviderApi::None`, so every custom provider failed to stream with
        // "has no API implementation").
        None => {
            let api_names: std::collections::BTreeSet<&str> = validated_models
                .iter()
                .map(|model| model.api.as_str())
                .collect();
            let api_names: Vec<&str> = api_names.into_iter().collect();
            pillar_ai::api_dispatch::api_map(&api_names)
        }
    };

    let name = extension
        .and_then(|e| e.name.clone())
        .or_else(|| config.and_then(|c| c.name.clone()))
        .or_else(|| base.map(|b| b.name.clone()))
        .unwrap_or_else(|| provider_id.to_string());
    let base_url = extension
        .and_then(|e| e.base_url.clone())
        .or_else(|| config.and_then(|c| c.base_url.clone()))
        .or_else(|| base.and_then(|b| b.base_url.clone()));

    Ok(Provider {
        id: provider_id.to_string(),
        name,
        base_url,
        headers: base.and_then(|b| b.headers.clone()),
        auth: ProviderAuth {
            api_key: api_key.map(|auth| auth as Arc<dyn pillar_ai::auth_types::ApiKeyAuth>),
            oauth,
        },
        get_models: Box::new(get_models),
        refresh_models: None,
        filter_models: base.and_then(|b| b.filter_models.clone()),
        api: streams,
    })
}
