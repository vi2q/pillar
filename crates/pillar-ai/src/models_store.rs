//! Port of packages/ai/src/models-store.ts (pi v0.84.3) — persistent model
//! catalogs keyed by provider ID, plus the in-memory implementation.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::types::Model;

/// An entry in the models store: a persisted catalog snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsStoreEntry {
    pub models: Vec<Model>,
    /// Unix timestamp (ms) from the remote catalog's Last-Modified header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<u64>,
    /// Unix timestamp (ms) of the last completed remote check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    /// Opaque validator from the remote catalog's ETag header, stored
    /// verbatim (quotes included) and echoed back as If-None-Match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// Options for models-store operations (abort plumbing).
#[derive(Debug, Clone, Default)]
pub struct ModelsStoreOperationOptions {
    pub signal: Option<crate::abort::AbortSignal>,
}

/// Persistent model catalogs keyed by provider ID.
#[async_trait]
pub trait ModelsStore: Send + Sync {
    async fn read(
        &self,
        provider_id: &str,
        options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<Option<ModelsStoreEntry>, crate::error::AiError>;
    async fn write(
        &self,
        provider_id: &str,
        entry: &ModelsStoreEntry,
        options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<(), crate::error::AiError>;
    async fn delete(
        &self,
        provider_id: &str,
        options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<(), crate::error::AiError>;
}

/// In-memory models store.
#[derive(Debug, Default)]
pub struct InMemoryModelsStore {
    entries: std::sync::Mutex<HashMap<String, ModelsStoreEntry>>,
}

impl InMemoryModelsStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ModelsStore for InMemoryModelsStore {
    async fn read(
        &self,
        provider_id: &str,
        options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<Option<ModelsStoreEntry>, crate::error::AiError> {
        if let Some(signal) = options.and_then(|o| o.signal.as_ref()) {
            signal.throw_if_aborted()?;
        }
        let guard = self.entries.lock().expect("models store lock");
        let entry = guard.get(provider_id).cloned();
        drop(guard);
        Ok(entry)
    }

    async fn write(
        &self,
        provider_id: &str,
        entry: &ModelsStoreEntry,
        options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<(), crate::error::AiError> {
        if let Some(signal) = options.and_then(|o| o.signal.as_ref()) {
            signal.throw_if_aborted()?;
        }
        let mut guard = self.entries.lock().expect("models store lock");
        guard.insert(provider_id.to_string(), entry.clone());
        Ok(())
    }

    async fn delete(
        &self,
        provider_id: &str,
        options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<(), crate::error::AiError> {
        if let Some(signal) = options.and_then(|o| o.signal.as_ref()) {
            signal.throw_if_aborted()?;
        }
        let mut guard = self.entries.lock().expect("models store lock");
        guard.remove(provider_id);
        Ok(())
    }
}
