//! Parity tests for auth-storage.ts + models-store.ts (pi v0.84.3):
//! locked JSON-backed credential and provider-catalog storage, in-memory
//! stores, read-only validation, and BOM handling.

use pillar_ai::auth_types::{Credential, CredentialStore};
use pillar_ai::models_store::{ModelsStore, ModelsStoreEntry};
use pillar_ai::types::{Model, ModelCost, ModelCostRates};
use pillar_coding_agent::core::auth_storage::{
    AuthStorage, FileAuthStorageBackend, FileModelsStore, InMemoryCodingAgentModelsStore,
    ReadOnlyAuthStorage, strip_bom,
};

fn temp_path(name: &str) -> std::path::PathBuf {
    // Unique dir per call so parallel tests never share lock files.
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "pillar-auth-store-{}-{n}-{}",
        std::process::id(),
        name
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir.join(name)
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let lock = path.with_extension(format!(
        "{}lock",
        path.extension()
            .map_or_else(String::new, |e| format!("{}.", e.to_string_lossy()))
    ));
    let _ = std::fs::remove_file(lock);
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

fn api_key_credential(key: &str) -> Credential {
    Credential::ApiKey(pillar_ai::auth_types::ApiKeyCredential {
        key: Some(key.to_string()),
        env: None,
    })
}

fn oauth_credential() -> Credential {
    Credential::OAuth(pillar_ai::auth_types::OAuthCredential {
        refresh: "r".to_string(),
        access: "a".to_string(),
        expires: 1_000,
        extra: Default::default(),
    })
}

fn model(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "test-api".to_string(),
        provider: provider.to_string(),
        base_url: "https://x.test".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates::default(),
            tiers: None,
        },
        context_window: 100_000,
        max_tokens: 8_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn entry(provider: &str) -> ModelsStoreEntry {
    ModelsStoreEntry {
        models: vec![model(provider, "m1")],
        last_modified: Some(100),
        checked_at: Some(200),
        etag: Some("\"v1\"".to_string()),
    }
}

// --- stripBom ------------------------------------------------------------------

#[test]
fn strip_bom_removes_only_a_leading_bom() {
    assert_eq!(strip_bom("\u{feff}{}"), "{}");
    assert_eq!(strip_bom("{}"), "{}");
    assert_eq!(strip_bom(""), "");
}

// --- FileAuthStorageBackend ---------------------------------------------------------

#[test]
fn backend_creates_file_and_parent_dirs() {
    let path = temp_path("auth.json");
    cleanup(&path);
    let backend = FileAuthStorageBackend::new(&path);
    let result = backend.with_lock(|content| (content, None)).unwrap();
    assert_eq!(
        result.as_deref(),
        Some("{}"),
        "missing file reads as empty object"
    );
    assert!(path.exists());
    cleanup(&path);
}

#[test]
fn backend_write_and_read_round_trip() {
    let path = temp_path("auth.json");
    cleanup(&path);
    let backend = FileAuthStorageBackend::new(&path);
    backend
        .with_lock(|_| {
            (
                (),
                Some(r#"{"anthropic":{"type":"api_key","key":"sk"}}"#.to_string()),
            )
        })
        .unwrap();
    let content = backend
        .with_lock(|content| (content, None))
        .unwrap()
        .unwrap();
    assert!(content.contains("anthropic"));
    cleanup(&path);
}

// --- AuthStorage (CredentialStore) ------------------------------------------------------

#[tokio::test]
async fn auth_storage_read_write_delete() {
    let path = temp_path("auth.json");
    cleanup(&path);
    let storage = AuthStorage::new(&path);

    assert!(storage.read("anthropic", None).await.unwrap().is_none());

    storage
        .modify(
            "anthropic",
            Box::new(|_| Box::pin(async { Ok(Some(api_key_credential("sk-1"))) })),
            None,
        )
        .await
        .unwrap()
        .expect("credential stored");

    let stored = storage.read("anthropic", None).await.unwrap().unwrap();
    assert_eq!(stored, api_key_credential("sk-1"));

    // List reports provider ids and kinds.
    let listed = storage.list(None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].provider_id, "anthropic");
    assert_eq!(listed[0].kind, "api_key");

    // Modify sees the current credential.
    storage
        .modify(
            "anthropic",
            Box::new(|current| {
                Box::pin(async move {
                    match current {
                        Some(Credential::ApiKey(key)) => Ok(Some(api_key_credential(
                            key.key.as_deref().unwrap_or_default(),
                        ))),
                        _ => panic!("expected api key"),
                    }
                })
            }),
            None,
        )
        .await
        .unwrap();

    // Delete removes the entry.
    storage.delete("anthropic", None).await.unwrap();
    assert!(storage.read("anthropic", None).await.unwrap().is_none());
    cleanup(&path);
}

#[tokio::test]
async fn auth_storage_modify_none_leaves_entry_unchanged() {
    let path = temp_path("auth.json");
    cleanup(&path);
    let storage = AuthStorage::new(&path);
    storage
        .modify(
            "p",
            Box::new(|_| Box::pin(async { Ok(Some(oauth_credential())) })),
            None,
        )
        .await
        .unwrap();
    // Returning None leaves the stored credential in place.
    storage
        .modify("p", Box::new(|_| Box::pin(async { Ok(None) })), None)
        .await
        .unwrap();
    assert_eq!(
        storage.read("p", None).await.unwrap(),
        Some(oauth_credential())
    );
    cleanup(&path);
}

#[tokio::test]
async fn auth_storage_modifier_errors_propagate() {
    let path = temp_path("auth.json");
    cleanup(&path);
    let storage = AuthStorage::new(&path);
    let error = storage
        .modify(
            "p",
            Box::new(|_| {
                Box::pin(async { Err(pillar_ai::error::AiError::Other("boom".to_string())) })
            }),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("boom"));
    cleanup(&path);
}

// --- ReadOnlyAuthStorage -------------------------------------------------------------------

#[test]
fn read_only_storage_reads_and_caches() {
    let path = temp_path("auth.json");
    cleanup(&path);
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    std::fs::write(&path, r#"{"p":{"type":"api_key","key":"sk"}}"#).unwrap();
    let storage = ReadOnlyAuthStorage::new(&path);
    assert_eq!(
        storage.read_credential("p").unwrap(),
        Some(api_key_credential("sk"))
    );
    assert_eq!(storage.list_credentials().unwrap().len(), 1);

    // Missing file -> empty.
    let path2 = temp_path("missing-auth.json");
    cleanup(&path2);
    let storage2 = ReadOnlyAuthStorage::new(&path2);
    assert_eq!(storage2.read_credential("p").unwrap(), None);
}

#[test]
fn read_only_storage_rejects_invalid_content() {
    let path = temp_path("auth.json");
    cleanup(&path);
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    std::fs::write(&path, "not json").unwrap();
    let storage = ReadOnlyAuthStorage::new(&path);
    assert!(storage.read_credential("p").is_err());

    // Array instead of object.
    std::fs::write(&path, "[]").unwrap();
    let storage = ReadOnlyAuthStorage::new(&path);
    assert!(storage.read_credential("p").is_err());
}

// --- in-memory models store --------------------------------------------------------------

#[tokio::test]
async fn in_memory_models_store_round_trip() {
    let store = InMemoryCodingAgentModelsStore::new();
    assert!(store.read("p", None).await.unwrap().is_none());
    let e = entry("p");
    store.write("p", &e, None).await.unwrap();
    assert_eq!(store.read("p", None).await.unwrap().as_ref(), Some(&e));
    store.delete("p", None).await.unwrap();
    assert!(store.read("p", None).await.unwrap().is_none());
}

// --- file models store ---------------------------------------------------------------------

#[tokio::test]
async fn file_models_store_round_trip_and_cross_instance() {
    let path = temp_path("models-store.json");
    cleanup(&path);
    let store = FileModelsStore::new(&path);
    assert!(store.read("p", None).await.unwrap().is_none());

    let e = entry("p");
    store.write("p", &e, None).await.unwrap();
    assert_eq!(store.read("p", None).await.unwrap().as_ref(), Some(&e));

    // A second instance over the same file sees the written entry.
    let store2 = FileModelsStore::new(&path);
    assert_eq!(store2.read("p", None).await.unwrap().as_ref(), Some(&e));

    // The file content is a JSON object of provider id -> entry.
    let content = std::fs::read_to_string(&path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(parsed.get("p").and_then(|p| p.get("models")).is_some());

    store.delete("p", None).await.unwrap();
    assert!(store.read("p", None).await.unwrap().is_none());
    cleanup(&path);
}

#[tokio::test]
async fn file_models_store_write_serializes_multiple_providers() {
    let path = temp_path("models-store.json");
    cleanup(&path);
    let store = FileModelsStore::new(&path);
    let e1 = entry("p1");
    let mut e2 = entry("p2");
    e2.checked_at = None;
    store.write("p1", &e1, None).await.unwrap();
    store.write("p2", &e2, None).await.unwrap();
    assert_eq!(store.read("p1", None).await.unwrap().as_ref(), Some(&e1));
    assert_eq!(store.read("p2", None).await.unwrap().as_ref(), Some(&e2));
    cleanup(&path);
}
