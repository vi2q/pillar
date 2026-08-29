//! Port of packages/ai/src/auth/types.ts + auth/resolve.ts (pi v0.84.3) —
//! credential shapes, credential store contract, auth context, and the
//! shared auth-resolution logic used by `Models` and `ImagesModels`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::abort::AbortSignal;

/// Provider-scoped environment/config values.
pub type ProviderEnv = crate::types::ProviderEnv;

/// Custom headers; `None` value suppresses a provider default header.
pub type ProviderHeaders = crate::types::ProviderHeaders;

/// Request auth for a single model request. If a value cannot be expressed as
/// `apiKey`, `headers`, or `baseUrl`, it is provider config, not auth.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<ProviderHeaders>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// Stored api-key credential. `env` holds provider-scoped environment/config
/// values such as Cloudflare account/gateway ids.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub struct ApiKeyCredential {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
}

/// Stored canonical OAuth credential.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredential {
    pub refresh: String,
    pub access: String,
    pub expires: u64,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// One type-tagged credential per provider — the shape of today's auth.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey(ApiKeyCredential),
    OAuth(OAuthCredential),
}

/// Non-secret credential metadata for account/status enumeration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    pub provider_id: String,
    #[serde(rename = "type")]
    pub kind: String,
}

/// Optional cancellation for public auth and credential operations.
#[derive(Debug, Clone, Default)]
pub struct AuthOperationOptions {
    pub signal: Option<AbortSignal>,
}

/// Environment access for auth resolution. Injectable for tests and browsers.
#[async_trait]
pub trait AuthContext: Send + Sync {
    async fn env(&self, name: &str) -> Option<String>;
    /// Check whether a file exists. Supports a leading `~`.
    async fn file_exists(&self, path: &str) -> bool;
}

/// Result of resolving auth for a model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthResult {
    pub auth: ModelAuth,
    /// Provider-scoped environment/config values resolved from credentials and ambient context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
    /// Human-readable label for status UI: "ANTHROPIC_API_KEY", "OAuth", "~/.aws/credentials".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthCheck {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
}

pub type AuthType = String; // "api_key" | "oauth"

/// App-owned credential storage, keyed by `Provider.id`, one credential per
/// provider. `modify` is the only write path; every mutation is a serialized
/// read-modify-write. Error semantics: `read` resolves `None` for missing
/// entries; methods fail only on storage failure.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    /// Read the stored credential, possibly expired. Display/status use.
    async fn read(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, crate::error::AiError>;

    /// List stored credential metadata without resolving or exposing secrets.
    async fn list(
        &self,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Vec<CredentialInfo>, crate::error::AiError>;

    /// Serialized write — the only write path. `fn` sees the current
    /// credential; return the new credential, or `None` to leave the entry
    /// unchanged. Mutual exclusion per provider id. Resolves with the
    /// post-write credential. Rejections from `fn` propagate.
    async fn modify(
        &self,
        provider_id: &str,
        f: CredentialModifier<'_>,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, crate::error::AiError>;

    /// Remove a credential (logout). Serializes against `modify`.
    async fn delete(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<(), crate::error::AiError>;
}

/// Async modifier closure over the current credential; returns the new
/// credential or `None` to leave the entry unchanged.
pub type CredentialModifier<'a> =
    Box<dyn FnOnce(Option<Credential>) -> BoxFuture<'a, Option<Credential>> + Send + 'a>;

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

// --- Auth method traits -------------------------------------------------

/// Api-key auth: stored key/provider env plus ambient sources (env vars, AWS
/// profiles, ADC files). Ambient-only providers omit `login`.
#[async_trait]
pub trait ApiKeyAuth: Send + Sync {
    /// Display name, e.g. "Anthropic API key".
    fn name(&self) -> &str;

    /// Interactive setup (prompt for key/provider env). `None` = ambient-only.
    async fn login(
        &self,
        _interaction: &ProviderAuthInteraction,
    ) -> Result<Option<ApiKeyCredential>, crate::error::AiError> {
        Ok(None)
    }

    /// Optional side-effect-free availability check. `None` means Models
    /// checks availability by resolving auth.
    async fn check(
        &self,
        _input: &ApiKeyAuthInput<'_>,
    ) -> Result<Option<AuthCheck>, crate::error::AiError> {
        Ok(None)
    }

    /// Resolve auth from the stored credential and/or ambient sources,
    /// merging per field. `None` = not configured.
    async fn resolve(
        &self,
        input: &ApiKeyAuthInput<'_>,
    ) -> Result<Option<AuthResult>, crate::error::AiError>;
}

/// Input bundle for `ApiKeyAuth::check` / `resolve`.
#[derive(Clone)]
pub struct ApiKeyAuthInput<'a> {
    pub ctx: &'a dyn AuthContext,
    pub credential: Option<&'a ApiKeyCredential>,
    pub signal: Option<&'a AbortSignal>,
}

/// OAuth auth. The `refresh`/`to_auth` split lets `Models` own the locked
/// refresh pattern: `refresh` produces a credential, `to_auth` derives
/// request auth from whatever credential ends up stored.
#[async_trait]
pub trait OAuthAuth: Send + Sync {
    /// Display name, e.g. "Anthropic (Claude Pro/Max)".
    fn name(&self) -> &str;

    /// Whether access through this auth method is backed by a subscription.
    fn is_subscription(&self) -> Option<bool> {
        None
    }

    /// Selector label for the OAuth login option.
    fn login_label(&self) -> Option<&str> {
        None
    }

    async fn login(
        &self,
        interaction: &ProviderAuthInteraction,
    ) -> Result<OAuthCredential, crate::error::AiError>;

    /// Exchange the refresh token. Network call; fails on invalid_grant etc.
    /// `Models` runs this under the store lock.
    async fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: &AbortSignal,
    ) -> Result<OAuthCredential, crate::error::AiError>;

    /// Side-effect-free derivation of request auth from a valid credential.
    async fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> Result<ModelAuth, crate::error::AiError>;
}

/// Provider auth. At least one of `api_key`/`oauth` must be present.
#[derive(Clone, Default)]
pub struct ProviderAuth {
    pub api_key: Option<Arc<dyn ApiKeyAuth>>,
    pub oauth: Option<Arc<dyn OAuthAuth>>,
}

// --- Login interaction --------------------------------------------------

/// Prompt shown to the user during login.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthPrompt {
    Text {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<SelectOption>,
    },
    ManualCode {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectOption {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthInfoLink {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Login events delivered to the interaction's `notify` callback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthEvent {
    Info {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        links: Option<Vec<AuthInfoLink>>,
    },
    AuthUrl {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        interval_seconds: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_in_seconds: Option<u64>,
    },
    Progress {
        message: String,
    },
}

/// Login interaction callbacks serving both api-key and OAuth flows.
#[async_trait]
pub trait AuthInteraction: Send + Sync {
    /// Optional flow-abort signal.
    fn signal(&self) -> Option<&AbortSignal>;

    /// Prompt returns the entered/selected string (`select` returns the option id).
    async fn prompt(&self, prompt: &AuthPrompt) -> Result<String, crate::error::AiError>;
    /// Notify of a login event.
    async fn notify(&self, event: &AuthEvent);
}

/// Normalized interaction passed to provider login implementations.
#[derive(Clone, Copy)]
pub struct ProviderAuthInteraction<'a> {
    pub interaction: &'a dyn AuthInteraction,
    /// Always present, normalized from the interaction's optional signal.
    pub signal: &'a AbortSignal,
}
