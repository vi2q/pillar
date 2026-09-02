//! Port of packages/ai/src/providers/all.ts (pi v0.84.3).
//!
//! Built-in provider factories: auth definitions + generated catalog models
//! wired to the ported API adapters, plus `builtin_models` aggregating them.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::auth_resolve::ProviderRef;
use crate::auth_types::{
    ApiKeyAuth, ApiKeyAuthInput, ApiKeyCredential, AuthResult, ModelAuth, ProviderAuth,
};
use crate::error::AiError;
use crate::models::{CreateProviderOptions, Models, Provider, ProviderApi, ProviderStreams};
use crate::models_catalog::generated_models_for;

// ---------------------------------------------------------------------------
// Shared auth implementations (upstream auth/helpers.ts envApiKeyAuth)
// ---------------------------------------------------------------------------

/// Upstream `envApiKeyAuth`: stored credential first, then env vars in order.
pub struct EnvApiKeyAuth {
    pub name: &'static str,
    pub env_vars: &'static [&'static str],
}

#[async_trait::async_trait]
impl ApiKeyAuth for EnvApiKeyAuth {
    fn name(&self) -> &str {
        self.name
    }

    async fn resolve(&self, input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthResult>, AiError> {
        if let Some(key) = input
            .credential
            .and_then(|credential| credential.key.clone())
        {
            return Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some(key),
                    ..Default::default()
                },
                env: input
                    .credential
                    .and_then(|credential| credential.env.clone()),
                source: Some("stored credential".to_string()),
            }));
        }
        for env_var in self.env_vars {
            if let Some(value) = input.ctx.env(env_var).await {
                return Ok(Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(value),
                        ..Default::default()
                    },
                    env: None,
                    source: Some(env_var.to_string()),
                }));
            }
        }
        Ok(None)
    }
}

fn env_api_key_auth(name: &'static str, env_vars: &'static [&'static str]) -> ProviderAuth {
    ProviderAuth {
        api_key: Some(Arc::new(EnvApiKeyAuth { name, env_vars })),
        oauth: None,
    }
}

/// Cloudflare auth (upstream providers/cloudflare-auth.ts): API key plus
/// account/gateway ids from credential env or ambient context.
struct CloudflareAuth {
    name: &'static str,
    kind: CloudflareAuthKind,
}

#[derive(Clone, Copy, PartialEq)]
enum CloudflareAuthKind {
    WorkersAi,
    AiGateway,
}

const CLOUDFLARE_API_KEY: &str = "CLOUDFLARE_API_KEY";
const CLOUDFLARE_ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
const CLOUDFLARE_GATEWAY_ID: &str = "CLOUDFLARE_GATEWAY_ID";

impl CloudflareAuth {
    async fn resolve_value(&self, name: &str, input: &ApiKeyAuthInput<'_>) -> Option<String> {
        // Per-field merge: prefer the credential value, fall back to env.
        if let Some(credential) = input.credential {
            let from_credential = if name == CLOUDFLARE_API_KEY {
                credential.key.clone()
            } else {
                credential
                    .env
                    .as_ref()
                    .and_then(|env| env.get(name).cloned())
            };
            if from_credential.is_some() {
                return from_credential;
            }
        }
        input.ctx.env(name).await
    }
}

#[async_trait::async_trait]
impl ApiKeyAuth for CloudflareAuth {
    fn name(&self) -> &str {
        self.name
    }

    async fn resolve(&self, input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthResult>, AiError> {
        let api_key = self.resolve_value(CLOUDFLARE_API_KEY, input).await;
        let account_id = self.resolve_value(CLOUDFLARE_ACCOUNT_ID, input).await;
        let gateway_id = if self.kind == CloudflareAuthKind::AiGateway {
            self.resolve_value(CLOUDFLARE_GATEWAY_ID, input).await
        } else {
            None
        };

        let (Some(api_key), Some(account_id)) = (api_key, account_id) else {
            return Ok(None);
        };
        if self.kind == CloudflareAuthKind::AiGateway && gateway_id.is_none() {
            return Ok(None);
        }

        let mut env = BTreeMap::new();
        env.insert(CLOUDFLARE_ACCOUNT_ID.to_string(), account_id);
        if let Some(gateway_id) = gateway_id {
            env.insert(CLOUDFLARE_GATEWAY_ID.to_string(), gateway_id);
        }

        let auth = match self.kind {
            CloudflareAuthKind::WorkersAi => ModelAuth {
                api_key: Some(api_key),
                ..Default::default()
            },
            CloudflareAuthKind::AiGateway => ModelAuth {
                headers: Some(
                    [
                        (
                            "cf-aig-authorization".to_string(),
                            Some(format!("Bearer {api_key}")),
                        ),
                        ("Authorization".to_string(), None),
                        ("x-api-key".to_string(), None),
                    ]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            },
        };

        Ok(Some(AuthResult {
            auth,
            env: Some(env),
            source: Some(
                if input.credential.is_some() {
                    "stored credential"
                } else {
                    CLOUDFLARE_API_KEY
                }
                .to_string(),
            ),
        }))
    }
}

// ---------------------------------------------------------------------------
// Adapter dispatch helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Adapter dispatch helpers
// ---------------------------------------------------------------------------

/// A placeholder streams bundle: the collection-level stream dispatch requires
/// per-API option plumbing that the builtin registry wires lazily. Providers
/// currently dispatch through `Models.stream` with these error stubs until
/// per-API option plumbing lands; catalog/auth behavior is fully live.
fn pending_streams() -> &'static ProviderStreams {
    static STREAMS: std::sync::OnceLock<ProviderStreams> = std::sync::OnceLock::new();
    STREAMS.get_or_init(|| ProviderStreams {
        stream: Arc::new(|model, _context, _options| {
            crate::models::error_stream_for(
                model,
                AiError::Other("provider stream dispatch not configured".to_string()),
            )
        }),
        stream_simple: Arc::new(|model, _context, _options| {
            crate::models::error_stream_for(
                model,
                AiError::Other("provider stream dispatch not configured".to_string()),
            )
        }),
    })
}

fn clone_pending() -> ProviderStreams {
    ProviderStreams {
        stream: pending_streams().stream.clone(),
        stream_simple: pending_streams().stream_simple.clone(),
    }
}

fn single_api(api_name: &str) -> ProviderApi {
    ProviderApi::Map(BTreeMap::from([(
        api_name.to_string(),
        Arc::new(ProviderStreams {
            stream: pending_streams().stream.clone(),
            stream_simple: pending_streams().stream_simple.clone(),
        }),
    )]))
}

fn simple_provider(
    id: &str,
    name: &str,
    base_url: Option<&str>,
    auth: ProviderAuth,
    api: ProviderApi,
) -> Arc<Provider> {
    crate::models::create_provider(CreateProviderOptions {
        id: id.to_string(),
        name: Some(name.to_string()),
        base_url: base_url.map(str::to_string),
        headers: None,
        auth,
        models: generated_models_for(id),
        fetch_models: None,
        filter_models: None,
        api,
    })
}

// ---------------------------------------------------------------------------
// Builtin providers (upstream providers/*.ts)
// ---------------------------------------------------------------------------

/// Amazon Bedrock: bearer token / AWS credential chain ambient auth.
struct BedrockAuth;

#[async_trait::async_trait]
impl ApiKeyAuth for BedrockAuth {
    fn name(&self) -> &str {
        "AWS credentials or bearer token"
    }

    async fn resolve(&self, input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthResult>, AiError> {
        if let Some(key) = input
            .credential
            .and_then(|credential| credential.key.clone())
        {
            return Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some(key),
                    ..Default::default()
                },
                env: input
                    .credential
                    .and_then(|credential| credential.env.clone()),
                source: Some("stored credential".to_string()),
            }));
        }
        if input.ctx.env("AWS_BEARER_TOKEN_BEDROCK").await.is_some() {
            return Ok(Some(AuthResult {
                auth: ModelAuth::default(),
                env: None,
                source: Some("AWS_BEARER_TOKEN_BEDROCK".to_string()),
            }));
        }
        let profile = match input
            .credential
            .and_then(|credential| credential.env.clone())
        {
            Some(env) => env
                .get("AWS_PROFILE")
                .cloned()
                .or(input.ctx.env("AWS_PROFILE").await),
            None => input.ctx.env("AWS_PROFILE").await,
        };
        if profile.is_some() {
            return Ok(Some(AuthResult {
                auth: ModelAuth::default(),
                env: input
                    .credential
                    .and_then(|credential| credential.env.clone()),
                source: Some(
                    if input
                        .credential
                        .and_then(|credential| credential.env.clone())
                        .and_then(|env| env.get("AWS_PROFILE").cloned())
                        .is_some()
                    {
                        "stored credential"
                    } else {
                        "AWS_PROFILE"
                    }
                    .to_string(),
                ),
            }));
        }
        let access_key = input.ctx.env("AWS_ACCESS_KEY_ID").await.is_some();
        let secret_key = input.ctx.env("AWS_SECRET_ACCESS_KEY").await.is_some();
        if access_key && secret_key {
            return Ok(Some(AuthResult {
                auth: ModelAuth::default(),
                env: None,
                source: Some("AWS access keys".to_string()),
            }));
        }
        for (name, source) in [
            ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "ECS task role"),
            ("AWS_CONTAINER_CREDENTIALS_FULL_URI", "ECS task role"),
            ("AWS_WEB_IDENTITY_TOKEN_FILE", "web identity token"),
        ] {
            if input.ctx.env(name).await.is_some() {
                return Ok(Some(AuthResult {
                    auth: ModelAuth::default(),
                    env: None,
                    source: Some(source.to_string()),
                }));
            }
        }
        Ok(None)
    }
}

/// Google Vertex: API key or ADC (ambient gcloud credentials + project/location).
struct VertexAuth;

const VERTEX_ADC_PATH: &str = "~/.config/gcloud/application_default_credentials.json";

#[async_trait::async_trait]
impl ApiKeyAuth for VertexAuth {
    fn name(&self) -> &str {
        "Google Cloud credentials"
    }

    async fn resolve(&self, input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthResult>, AiError> {
        if let Some(key) = input
            .credential
            .and_then(|credential| credential.key.clone())
        {
            return Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some(key),
                    ..Default::default()
                },
                env: None,
                source: Some("stored credential".to_string()),
            }));
        }
        if let Some(key) = input.ctx.env("GOOGLE_CLOUD_API_KEY").await {
            return Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some(key),
                    ..Default::default()
                },
                env: None,
                source: Some("GOOGLE_CLOUD_API_KEY".to_string()),
            }));
        }
        let adc_path = input
            .credential
            .and_then(|credential| credential.env.as_ref())
            .and_then(|env| env.get("GOOGLE_APPLICATION_CREDENTIALS").cloned())
            .or(input.ctx.env("GOOGLE_APPLICATION_CREDENTIALS").await);
        let has_credentials = input
            .ctx
            .file_exists(adc_path.as_deref().unwrap_or(VERTEX_ADC_PATH))
            .await;
        let project = input
            .credential
            .and_then(|credential| credential.env.as_ref())
            .and_then(|env| env.get("GOOGLE_CLOUD_PROJECT").cloned())
            .or(input.ctx.env("GOOGLE_CLOUD_PROJECT").await)
            .or(input.ctx.env("GCLOUD_PROJECT").await);
        let location = input
            .credential
            .and_then(|credential| credential.env.as_ref())
            .and_then(|env| env.get("GOOGLE_CLOUD_LOCATION").cloned())
            .or(input.ctx.env("GOOGLE_CLOUD_LOCATION").await);
        if has_credentials && project.is_some() && location.is_some() {
            return Ok(Some(AuthResult {
                auth: ModelAuth::default(),
                env: input
                    .credential
                    .and_then(|credential| credential.env.clone()),
                source: Some(
                    if input.credential.is_some() {
                        "stored credential"
                    } else {
                        "gcloud application default credentials"
                    }
                    .to_string(),
                ),
            }));
        }
        Ok(None)
    }
}

/// Radius: dynamic gateway provider with persisted catalog (upstream
/// providers/radius.ts). Static port: ambient key auth; models come from the
/// provider's own refresh path.
fn radius_provider() -> Arc<Provider> {
    env_api_key_auth("Radius API key", &["RADIUS_API_KEY"]);
    crate::models::create_provider(CreateProviderOptions {
        id: "radius".to_string(),
        name: Some("Radius".to_string()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(Arc::new(EnvApiKeyAuth {
                name: "Radius API key",
                env_vars: &["RADIUS_API_KEY"],
            })),
            oauth: None,
        },
        models: Vec::new(),
        fetch_models: None,
        filter_models: None,
        api: single_api("pi-messages"),
    })
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// All built-in providers, freshly constructed (upstream `builtinProviders`).
pub fn builtin_providers() -> Vec<Arc<Provider>> {
    vec![
        simple_provider(
            "amazon-bedrock",
            "Amazon Bedrock",
            None,
            ProviderAuth {
                api_key: Some(Arc::new(BedrockAuth)),
                oauth: None,
            },
            single_api("bedrock-converse-stream"),
        ),
        simple_provider(
            "anthropic",
            "Anthropic",
            Some("https://api.anthropic.com"),
            env_api_key_auth(
                "Anthropic API key",
                &[
                    "ANTHROPIC_OAUTH_TOKEN_ENV",
                    "ANTHROPIC_API_KEY_ENV",
                    "ANTHROPIC_API_KEY",
                ],
            ),
            single_api("anthropic-messages"),
        ),
        simple_provider(
            "azure-openai-responses",
            "Azure OpenAI",
            None,
            env_api_key_auth("Azure OpenAI API key", &["AZURE_OPENAI_API_KEY"]),
            single_api("azure-openai-responses"),
        ),
        simple_provider(
            "cloudflare-ai-gateway",
            "Cloudflare AI Gateway",
            None,
            ProviderAuth {
                api_key: Some(Arc::new(CloudflareAuth {
                    name: "Cloudflare API key",
                    kind: CloudflareAuthKind::AiGateway,
                })),
                oauth: None,
            },
            ProviderApi::Map(BTreeMap::from([
                ("anthropic-messages".to_string(), Arc::new(clone_pending())),
                ("openai-completions".to_string(), Arc::new(clone_pending())),
                ("openai-responses".to_string(), Arc::new(clone_pending())),
            ])),
        ),
        simple_provider(
            "cloudflare-workers-ai",
            "Cloudflare Workers AI",
            None,
            ProviderAuth {
                api_key: Some(Arc::new(CloudflareAuth {
                    name: "Cloudflare API key",
                    kind: CloudflareAuthKind::WorkersAi,
                })),
                oauth: None,
            },
            single_api("openai-completions"),
        ),
        simple_provider(
            "github-copilot",
            "GitHub Copilot",
            None,
            env_api_key_auth("GitHub Copilot", &["GITHUB_COPILOT_TOKEN"]),
            ProviderApi::Map(BTreeMap::from([
                ("anthropic-messages".to_string(), Arc::new(clone_pending())),
                ("openai-completions".to_string(), Arc::new(clone_pending())),
                ("openai-responses".to_string(), Arc::new(clone_pending())),
            ])),
        ),
        simple_provider(
            "google",
            "Google",
            Some("https://generativelanguage.googleapis.com/v1beta"),
            env_api_key_auth("Gemini API key", &["GEMINI_API_KEY"]),
            single_api("google-generative-ai"),
        ),
        simple_provider(
            "google-vertex",
            "Google Vertex AI",
            None,
            ProviderAuth {
                api_key: Some(Arc::new(VertexAuth)),
                oauth: None,
            },
            single_api("google-vertex"),
        ),
        simple_provider(
            "kimi-coding",
            "Kimi For Coding",
            None,
            env_api_key_auth("Kimi For Coding API key", &["KIMI_CODING_API_KEY"]),
            single_api("anthropic-messages"),
        ),
        simple_provider(
            "mistral",
            "Mistral",
            Some("https://api.mistral.ai"),
            env_api_key_auth("Mistral API key", &["MISTRAL_API_KEY"]),
            single_api("mistral-conversations"),
        ),
        simple_provider(
            "openai",
            "OpenAI",
            Some("https://api.openai.com/v1"),
            env_api_key_auth("OpenAI API key", &["OPENAI_API_KEY"]),
            single_api("openai-responses"),
        ),
        simple_provider(
            "openai-codex",
            "OpenAI Codex",
            Some("https://chatgpt.com/backend-api"),
            env_api_key_auth("OpenAI Codex (OAuth)", &["OPENAI_CODEX_OAUTH_TOKEN"]),
            single_api("openai-codex-responses"),
        ),
        simple_provider(
            "openrouter",
            "OpenRouter",
            Some("https://openrouter.ai/api/v1"),
            env_api_key_auth("OpenRouter API key", &["OPENROUTER_API_KEY"]),
            single_api("openai-completions"),
        ),
        simple_provider(
            "xai",
            "xAI",
            Some("https://api.x.ai/v1"),
            env_api_key_auth("xAI API key", &["XAI_API_KEY"]),
            single_api("openai-responses"),
        ),
        radius_provider(),
    ]
}

/// A `Models` collection with every built-in provider registered (upstream
/// `builtinModels`).
pub fn builtin_models() -> Models {
    let models = crate::models::create_models(Default::default());
    for provider in builtin_providers() {
        models.set_provider(provider);
    }
    models
}

/// Resolve ambient auth for a provider via its registered auth methods
/// (test helper parity with `Models.get_auth`).
pub async fn provider_auth_result(
    provider: &Provider,
    credentials: &dyn crate::auth_types::CredentialStore,
    auth_context: &dyn crate::auth_types::AuthContext,
) -> Result<Option<AuthResult>, AiError> {
    crate::auth_resolve::resolve_provider_auth(
        &ProviderRef {
            id: &provider.id,
            auth: &provider.auth,
        },
        credentials,
        auth_context,
        None,
    )
    .await
}

// Keep ApiKeyCredential referenced (used by future provider auth impls).
const _: Option<fn(&ApiKeyCredential) -> Option<String>> = None;
