//! Port of packages/ai/src/auth/credential-store.ts (pi v0.84.3) — the
//! default in-memory credential store. Apps inject persistent stores.
//! Keyed by `Provider.id`, one credential per provider; writes are
//! serialized per provider through a task queue.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::abort::operation_signal;
use crate::auth_types::{AuthOperationOptions, Credential, CredentialInfo, CredentialStore};

/// Default in-memory credential store. Apps inject persistent stores.
/// Keyed by `Provider.id`, one credential per provider; see `CredentialStore`.
/// Writes are serialized per provider through a task queue.
#[derive(Debug, Default)]
pub struct InMemoryCredentialStore {
    credentials: Mutex<HashMap<String, Credential>>,
    /// Per-provider lock ensuring `modify`/`delete` mutual exclusion
    /// (upstream serializes through a promise chain).
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_for(&self, provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().expect("credential store locks");
        Arc::clone(locks.entry(provider_id.to_string()).or_default())
    }
}

#[async_trait]
impl CredentialStore for InMemoryCredentialStore {
    async fn read(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, crate::error::AiError> {
        if let Some(options) = options {
            if let Some(signal) = &options.signal {
                signal.throw_if_aborted()?;
            }
        }
        let guard = self.credentials.lock().expect("credential store lock");
        Ok(guard.get(provider_id).cloned())
    }

    async fn list(
        &self,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Vec<CredentialInfo>, crate::error::AiError> {
        if let Some(options) = options {
            if let Some(signal) = &options.signal {
                signal.throw_if_aborted()?;
            }
        }
        let guard = self.credentials.lock().expect("credential store lock");
        Ok(guard
            .iter()
            .map(|(provider_id, credential)| CredentialInfo {
                provider_id: provider_id.clone(),
                kind: credential_type_label(credential).to_string(),
            })
            .collect())
    }

    async fn modify(
        &self,
        provider_id: &str,
        f: crate::auth_types::CredentialModifier<'_>,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, crate::error::AiError> {
        let signal = operation_signal(options.and_then(|o| o.signal.as_ref()));
        let lock = self.lock_for(provider_id);
        let _guard = lock.lock().await;
        signal.throw_if_aborted()?;
        let current = {
            let guard = self.credentials.lock().expect("credential store lock");
            guard.get(provider_id).cloned()
        };
        let next = f(current.clone()).await;
        signal.throw_if_aborted()?;
        if let Some(next) = &next {
            let mut guard = self.credentials.lock().expect("credential store lock");
            guard.insert(provider_id.to_string(), next.clone());
        }
        Ok(next.or(current))
    }

    async fn delete(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<(), crate::error::AiError> {
        let signal = operation_signal(options.and_then(|o| o.signal.as_ref()));
        let lock = self.lock_for(provider_id);
        let _guard = lock.lock().await;
        signal.throw_if_aborted()?;
        let mut guard = self.credentials.lock().expect("credential store lock");
        guard.remove(provider_id);
        Ok(())
    }
}

fn credential_type_label(credential: &Credential) -> &'static str {
    match credential {
        Credential::ApiKey(_) => "api_key",
        Credential::OAuth(_) => "oauth",
    }
}
