//! Port of packages/coding-agent/src/core/runtime-credentials.ts (pi
//! v0.84.3): async credential-store overlay for non-persistent runtime API
//! keys.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use pillar_ai::auth_types::{
    AuthOperationOptions, Credential, CredentialInfo, CredentialModifier, CredentialStore,
};
use pillar_ai::error::AiError;

/// Credential-store overlay: runtime API keys shadow the underlying store's
/// stored credentials without persisting them.
#[derive(Clone)]
pub struct RuntimeCredentials {
    store: Arc<dyn CredentialStore>,
    overrides: Arc<Mutex<BTreeMap<String, String>>>,
}

impl RuntimeCredentials {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self {
            store,
            overrides: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn set_runtime_api_key(&self, provider_id: &str, api_key: &str) {
        self.overrides
            .lock()
            .unwrap()
            .insert(provider_id.to_string(), api_key.to_string());
    }

    pub fn remove_runtime_api_key(&self, provider_id: &str) {
        self.overrides
            .lock()
            .expect("runtime credentials lock")
            .remove(provider_id);
    }

    pub fn has_runtime_api_key(&self, provider_id: &str) -> bool {
        self.overrides
            .lock()
            .expect("runtime credentials lock")
            .contains_key(provider_id)
    }
}

#[async_trait]
impl CredentialStore for RuntimeCredentials {
    async fn read(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, AiError> {
        if let Some(key) = self
            .overrides
            .lock()
            .expect("runtime credentials lock")
            .get(provider_id)
        {
            return Ok(Some(Credential::ApiKey(
                pillar_ai::auth_types::ApiKeyCredential {
                    key: Some(key.clone()),
                    env: None,
                },
            )));
        }
        self.store.read(provider_id, options).await
    }

    async fn list(
        &self,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Vec<CredentialInfo>, AiError> {
        let mut entries: BTreeMap<String, CredentialInfo> = self
            .store
            .list(options)
            .await?
            .into_iter()
            .map(|entry| (entry.provider_id.clone(), entry))
            .collect();
        for provider_id in self
            .overrides
            .lock()
            .expect("runtime credentials lock")
            .keys()
        {
            entries.insert(
                provider_id.clone(),
                CredentialInfo {
                    provider_id: provider_id.clone(),
                    kind: "api_key".to_string(),
                },
            );
        }
        Ok(entries.into_values().collect())
    }

    async fn modify(
        &self,
        provider_id: &str,
        f: CredentialModifier<'_>,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, AiError> {
        self.store.modify(provider_id, f, options).await
    }

    async fn delete(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<(), AiError> {
        self.store.delete(provider_id, options).await?;
        self.overrides
            .lock()
            .expect("runtime credentials lock")
            .remove(provider_id);
        Ok(())
    }
}
