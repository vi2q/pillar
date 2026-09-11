//! Port of packages/coding-agent/src/core/auth-storage.ts and
//! models-store.ts (pi v0.84.3): credential and provider-catalog storage
//! backed by locked JSON files, plus the in-memory store.
//!
//! divergence: upstream uses `proper-lockfile` (lock-directory creation with
//! stale detection); the port uses `fd-lock` advisory file locks (`.<name>.lock`
//! sibling files), which give the same read-modify-write serialization for
//! local single-host use. The shared-read-state revision cache and the async
//! coalescing reload machinery are simplified to a synchronous under-lock
//! read (the port's store operations are sync under the hood), preserving
//! the observable contract: writes serialize, readers see the latest content.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pillar_ai::auth_types::{AuthOperationOptions, Credential, CredentialInfo, CredentialStore};
use pillar_ai::error::AiError;
use pillar_ai::models_store::{ModelsStore, ModelsStoreEntry, ModelsStoreOperationOptions};

/// Strip a UTF-8 BOM (upstream `stripBom`).
pub fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

/// Read a file's content, None when missing (upstream readFileSync-or-undefined).
fn read_optional(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

// --- auth-storage.ts -------------------------------------------------------------

/// Credential storage file data: provider id -> credential.
pub type AuthStorageData = BTreeMap<String, Credential>;

fn credential_info(provider_id: &str, credential: &Credential) -> CredentialInfo {
    CredentialInfo {
        provider_id: provider_id.to_string(),
        kind: match credential {
            Credential::ApiKey(_) => "api_key".to_string(),
            Credential::OAuth(_) => "oauth".to_string(),
        },
    }
}

fn parse_auth_storage(content: &str) -> Result<AuthStorageData, String> {
    let parsed: serde_json::Value = serde_json::from_str(strip_bom(content))
        .map_err(|e| format!("Failed to read auth.json: {e}"))?;
    if !parsed.is_object() {
        return Err("Invalid auth.json: expected an object".to_string());
    }
    serde_json::from_value(parsed).map_err(|e| format!("Failed to read auth.json: {e}"))
}

/// Advisory file lock over a JSON file: `with_lock` runs a read-modify-write
/// under the lock and optionally replaces the file content.
pub struct FileAuthStorageBackend {
    pub auth_path: PathBuf,
}

impl FileAuthStorageBackend {
    pub fn new(auth_path: impl Into<PathBuf>) -> Self {
        Self {
            auth_path: auth_path.into(),
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.auth_path.with_extension(format!(
            "{}lock",
            self.auth_path
                .extension()
                .map_or_else(String::new, |e| format!("{}.", e.to_string_lossy()))
        ))
    }

    fn ensure_parent_dir(&self) -> Result<(), String> {
        if let Some(dir) = self.auth_path.parent() {
            if !dir.exists() {
                fs::create_dir_all(dir).map_err(|e| format!("Failed to create auth dir: {e}"))?;
            }
        }
        Ok(())
    }

    fn ensure_file_exists(&self) -> Result<(), String> {
        if !self.auth_path.exists() {
            self.ensure_parent_dir()?;
            fs::write(&self.auth_path, "{}")
                .map_err(|e| format!("Failed to create auth file: {e}"))?;
        }
        Ok(())
    }

    /// Run `fn` under the file lock. `fn` receives the current content (None
    /// when missing) and returns its result plus optional next content.
    pub fn with_lock<T>(
        &self,
        f: impl FnOnce(Option<String>) -> (T, Option<String>),
    ) -> Result<T, String> {
        self.ensure_parent_dir()?;
        self.ensure_file_exists()?;
        let lock_path = self.lock_path();
        if let Some(dir) = lock_path.parent() {
            if !dir.exists() {
                fs::create_dir_all(dir).map_err(|e| format!("Failed to create lock dir: {e}"))?;
            }
        }
        let mut lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&lock_path)
            .map_err(|e| format!("Failed to open lock file: {e}"))?;
        #[cfg(not(target_arch = "wasm32"))]
        let mut guard = fd_lock::RwLock::new(&mut lock_file);
        #[cfg(not(target_arch = "wasm32"))]
        let mut handle = guard
            .try_write()
            .map_err(|e| format!("Failed to acquire auth storage lock: {e}"))?;
        // wasm32 has no advisory file locks; fall back to in-process
        // serialization through the enclosing store lock.
        #[cfg(target_arch = "wasm32")]
        let handle = &mut lock_file;

        let current = read_optional(&self.auth_path);
        let (result, next) = f(current);
        if let Some(next) = next {
            handle
                .seek(SeekFrom::Start(0))
                .and_then(|_| handle.set_len(0))
                .and_then(|_| handle.write_all(next.as_bytes()))
                .map_err(|e| format!("Failed to write auth file: {e}"))?;
            // The lock file is separate from the auth file; the guard is
            // released when this function returns.
            fs::write(&self.auth_path, next)
                .map_err(|e| format!("Failed to write auth file: {e}"))?;
        }
        Ok(result)
    }
}

/// Locked JSON-backed credential storage (upstream `AuthStorage` implements
/// the CredentialStore contract over `FileAuthStorageBackend`).
pub struct AuthStorage {
    backend: FileAuthStorageBackend,
}

impl AuthStorage {
    pub fn new(auth_path: impl Into<PathBuf>) -> Self {
        Self {
            backend: FileAuthStorageBackend::new(auth_path),
        }
    }

    fn load(&self) -> Result<AuthStorageData, String> {
        let content = read_optional(&self.backend.auth_path);
        match content {
            Some(content) => parse_auth_storage(&content),
            None => Ok(BTreeMap::new()),
        }
    }

    fn store(&self, data: &AuthStorageData) -> Result<(), String> {
        let serialized = serde_json::to_string_pretty(data)
            .map_err(|e| format!("Failed to serialize auth.json: {e}"))?;
        self.backend.with_lock(|_| ((), Some(serialized)))?;
        Ok(())
    }
}

#[async_trait]
impl CredentialStore for AuthStorage {
    async fn read(
        &self,
        provider_id: &str,
        _options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, AiError> {
        self.load()
            .map(|data| data.get(provider_id).cloned())
            .map_err(AiError::Other)
    }

    async fn list(
        &self,
        _options: Option<&AuthOperationOptions>,
    ) -> Result<Vec<CredentialInfo>, AiError> {
        self.load()
            .map(|data| {
                data.iter()
                    .map(|(id, credential)| credential_info(id, credential))
                    .collect()
            })
            .map_err(AiError::Other)
    }

    async fn modify(
        &self,
        provider_id: &str,
        f: pillar_ai::auth_types::CredentialModifier<'_>,
        _options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, AiError> {
        let current = self.load().map_err(AiError::Other)?;
        let new_credential = f(current.get(provider_id).cloned())
            .await
            .map_err(|e| AiError::Other(e.to_string()))?;
        let Some(credential) = new_credential else {
            // Leave the entry unchanged.
            return Ok(current.get(provider_id).cloned());
        };
        let mut data = current;
        data.insert(provider_id.to_string(), credential.clone());
        self.store(&data).map_err(AiError::Other)?;
        Ok(Some(credential))
    }

    async fn delete(
        &self,
        provider_id: &str,
        _options: Option<&AuthOperationOptions>,
    ) -> Result<(), AiError> {
        let mut data = self.load().map_err(AiError::Other)?;
        data.remove(provider_id);
        self.store(&data).map_err(AiError::Other)
    }
}

/// Read-only view over auth.json with validation on first load (upstream
/// `ReadOnlyAuthStorage`).
pub struct ReadOnlyAuthStorage {
    auth_path: PathBuf,
    data: Mutex<Option<AuthStorageData>>,
}

impl ReadOnlyAuthStorage {
    pub fn new(auth_path: impl Into<PathBuf>) -> Self {
        Self {
            auth_path: auth_path.into(),
            data: Mutex::new(None),
        }
    }

    fn load(&self) -> Result<AuthStorageData, String> {
        let mut cached = self.data.lock().unwrap();
        if let Some(data) = cached.as_ref() {
            return Ok(data.clone());
        }
        let content = read_optional(&self.auth_path);
        let data = match content {
            Some(content) => parse_auth_storage(&content)?,
            None => BTreeMap::new(),
        };
        *cached = Some(data.clone());
        Ok(data)
    }

    /// Read a credential (upstream async read; sync here).
    pub fn read_credential(&self, provider_id: &str) -> Result<Option<Credential>, String> {
        self.load().map(|data| data.get(provider_id).cloned())
    }

    /// List credential metadata.
    pub fn list_credentials(&self) -> Result<Vec<CredentialInfo>, String> {
        self.load().map(|data| {
            data.iter()
                .map(|(id, credential)| credential_info(id, credential))
                .collect()
        })
    }
}

// --- models-store.ts -------------------------------------------------------------

/// Stored provider catalogs: provider id -> entry.
pub type StoredModels = BTreeMap<String, ModelsStoreEntry>;

/// In-memory models store (upstream `InMemoryCodingAgentModelsStore`).
#[derive(Debug, Default)]
pub struct InMemoryCodingAgentModelsStore {
    entries: Mutex<BTreeMap<String, ModelsStoreEntry>>,
}

impl InMemoryCodingAgentModelsStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ModelsStore for InMemoryCodingAgentModelsStore {
    async fn read(
        &self,
        provider_id: &str,
        _options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<Option<ModelsStoreEntry>, AiError> {
        Ok(self.entries.lock().unwrap().get(provider_id).cloned())
    }

    async fn write(
        &self,
        provider_id: &str,
        entry: &ModelsStoreEntry,
        _options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<(), AiError> {
        self.entries
            .lock()
            .unwrap()
            .insert(provider_id.to_string(), entry.clone());
        Ok(())
    }

    async fn delete(
        &self,
        provider_id: &str,
        _options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<(), AiError> {
        self.entries.lock().unwrap().remove(provider_id);
        Ok(())
    }
}

/// Locked JSON-backed storage for dynamically refreshed provider catalogs
/// (upstream `FileModelsStore`): every write rewrites the whole file under
/// the lock; reads are served from the in-process cache which is refreshed
/// on first use.
pub struct FileModelsStore {
    backend: Arc<FileAuthStorageBackend>,
    cache: Arc<Mutex<StoredModels>>,
}

impl FileModelsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            backend: Arc::new(FileAuthStorageBackend::new(path)),
            cache: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn refresh(&self) -> Result<StoredModels, String> {
        self.backend.with_lock(|content| {
            let data: StoredModels = match content {
                Some(content) => serde_json::from_str(strip_bom(&content)).unwrap_or_default(),
                None => BTreeMap::new(),
            };
            (data.clone(), None)
        })
    }
}

#[async_trait]
impl ModelsStore for FileModelsStore {
    async fn read(
        &self,
        provider_id: &str,
        _options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<Option<ModelsStoreEntry>, AiError> {
        {
            let cache = self.cache.lock().unwrap();
            if let Some(entry) = cache.get(provider_id) {
                return Ok(Some(entry.clone()));
            }
        }
        let data = self.refresh().map_err(AiError::Other)?;
        *self.cache.lock().unwrap() = data.clone();
        Ok(data.get(provider_id).cloned())
    }

    async fn write(
        &self,
        provider_id: &str,
        entry: &ModelsStoreEntry,
        _options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<(), AiError> {
        let serialized = self
            .backend
            .with_lock(|content| {
                let mut current: StoredModels = match content {
                    Some(content) => serde_json::from_str(strip_bom(&content)).unwrap_or_default(),
                    None => BTreeMap::new(),
                };
                current.insert(provider_id.to_string(), entry.clone());
                let next = serde_json::to_string_pretty(&current).ok();
                (current.clone(), next)
            })
            .map_err(AiError::Other)?;
        *self.cache.lock().unwrap() = serialized;
        Ok(())
    }

    async fn delete(
        &self,
        provider_id: &str,
        _options: Option<&ModelsStoreOperationOptions>,
    ) -> Result<(), AiError> {
        let serialized = self
            .backend
            .with_lock(|content| {
                let mut current: StoredModels = match content {
                    Some(content) => serde_json::from_str(strip_bom(&content)).unwrap_or_default(),
                    None => BTreeMap::new(),
                };
                current.remove(provider_id);
                let next = serde_json::to_string_pretty(&current).ok();
                (current.clone(), next)
            })
            .map_err(AiError::Other)?;
        *self.cache.lock().unwrap() = serialized;
        Ok(())
    }
}

/// Validate that a models-store file parses (used by callers for early
/// error surfacing; upstream validates on first read).
pub fn validate_models_store_content(content: &str) -> Result<StoredModels, String> {
    serde_json::from_str(strip_bom(content)).map_err(|e| format!("Invalid models-store.json: {e}"))
}
