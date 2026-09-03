//! Port of packages/coding-agent/src/core/model-registry.ts (pi v0.84.3):
//! the synchronous compatibility facade exposed to extensions over the
//! model runtime. The runtime itself is host-injected through the
//! [`ModelRuntimeFacade`] trait, so the port covers the facade semantics
//! (error mapping, auth-status shaping, provider display names) without
//! the ModelRuntime implementation.
//!
//! divergences: ModelRuntime (models.json refresh, auth resolution,
//! provider completion) is not ported; hosts plug it in via the trait.

use pillar_ai::types::Model;

/// Resolved request auth (upstream `ResolvedRequestAuth`).
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedRequestAuth {
    Ok {
        api_key: Option<String>,
        headers: Option<std::collections::BTreeMap<String, String>>,
        base_url: Option<String>,
        env: Option<std::collections::BTreeMap<String, String>>,
    },
    Failed {
        error: String,
    },
}

impl ResolvedRequestAuth {
    pub fn is_ok(&self) -> bool {
        matches!(self, ResolvedRequestAuth::Ok { .. })
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            ResolvedRequestAuth::Ok { .. } => None,
            ResolvedRequestAuth::Failed { error } => Some(error),
        }
    }
}

/// Provider auth status (upstream `AuthStatus`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthStatus {
    pub configured: bool,
    /// "stored" | "runtime" | "environment" | "fallback" |
    /// "models_json_key" | "models_json_command".
    pub source: Option<String>,
    pub label: Option<String>,
}

/// The runtime operations the registry facade needs (upstream the subset
/// of `ModelRuntime` the facade touches).
pub trait ModelRuntimeFacade {
    fn find(&self, provider: &str, model_id: &str) -> Option<Model>;
    fn all(&self) -> Vec<Model>;
    fn available(&self) -> Vec<Model>;
    fn has_configured_auth(&self, provider: &str) -> bool;
    fn auth_status(&self, provider: &str) -> AuthStatus;
    fn get_auth(&self, model: &Model) -> Result<Option<ResolvedRuntimeAuth>, String>;
    /// The compatibility request config for a model (authHeader flag +
    /// default headers).
    fn compatibility_request_config(
        &self,
        model: &Model,
    ) -> (bool, Option<std::collections::BTreeMap<String, String>>);
    fn provider_display_name(&self, provider: &str) -> Option<String>;
    fn is_using_oauth(&self, provider: &str) -> bool;
}

/// The auth resolution shape returned by the runtime (upstream
/// `AuthResult` subset).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRuntimeAuth {
    pub api_key: Option<String>,
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    pub base_url: Option<String>,
    pub env: Option<std::collections::BTreeMap<String, String>>,
}

/// The synchronous compatibility facade (upstream `ModelRegistry`).
pub struct ModelRegistry<'r> {
    runtime: &'r dyn ModelRuntimeFacade,
}

impl<'r> ModelRegistry<'r> {
    pub fn new(runtime: &'r dyn ModelRuntimeFacade) -> Self {
        Self { runtime }
    }

    pub fn find(&self, provider: &str, model_id: &str) -> Option<Model> {
        self.runtime.find(provider, model_id)
    }

    pub fn all(&self) -> Vec<Model> {
        self.runtime.all()
    }

    pub fn available(&self) -> Vec<Model> {
        self.runtime.available()
    }

    pub fn has_configured_auth(&self, provider: &str) -> bool {
        self.runtime.has_configured_auth(provider)
    }

    /// Resolve API key + headers for a model (upstream
    /// `getApiKeyAndHeaders`): no resolution falls back to the
    /// compatibility request config; an `authHeader` requirement without a
    /// resolved key maps to the "No API key found" error, as does the
    /// upstream cause-message special case.
    pub fn get_api_key_and_headers(&self, model: &Model) -> ResolvedRequestAuth {
        match self.runtime.get_auth(model) {
            Ok(Some(resolution)) => ResolvedRequestAuth::Ok {
                api_key: resolution.api_key,
                headers: resolution.headers,
                base_url: resolution.base_url,
                env: resolution.env,
            },
            Ok(None) => {
                let (auth_header, headers) = self.runtime.compatibility_request_config(model);
                if auth_header {
                    return ResolvedRequestAuth::Failed {
                        error: format!("No API key found for \"{}\"", model.provider),
                    };
                }
                ResolvedRequestAuth::Ok {
                    api_key: None,
                    headers,
                    base_url: None,
                    env: None,
                }
            }
            Err(error) => {
                let mapped = if error == "authHeader requires a resolved API key" {
                    format!("No API key found for \"{}\"", model.provider)
                } else {
                    error
                };
                ResolvedRequestAuth::Failed { error: mapped }
            }
        }
    }

    pub fn auth_status(&self, provider: &str) -> AuthStatus {
        self.runtime.auth_status(provider)
    }

    /// Display name for a provider, falling back to the raw id (upstream
    /// `getProviderDisplayName`).
    pub fn provider_display_name(&self, provider: &str) -> String {
        self.runtime
            .provider_display_name(provider)
            .unwrap_or_else(|| provider.to_string())
    }

    pub fn is_using_oauth(&self, provider: &str) -> bool {
        self.runtime.is_using_oauth(provider)
    }
}
