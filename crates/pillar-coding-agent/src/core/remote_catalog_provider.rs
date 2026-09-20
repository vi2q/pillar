//! Port of packages/coding-agent/src/core/remote-catalog-provider.ts (pi
//! v0.84.3), the pure decision core of the pi.dev remote catalog overlay:
//! model merging, catalog parsing, stale-overlay gating, and the refresh
//! decision tree (restore / freshness window / 304 / 404+501 / transient
//! failure / full refresh).
//!
//! divergences: the HTTP fetch (fetchWithRetry) and the provider trait
//! wiring are host-side; the port exposes the decision function over a
//! host-provided fetch outcome.

use pillar_ai::models_store::ModelsStoreEntry;
use pillar_ai::types::Model;

/// Default catalog base URL (upstream `DEFAULT_CATALOG_BASE_URL`).
pub const DEFAULT_CATALOG_BASE_URL: &str = "https://pi.dev";

/// Per-attempt fetch timeout (upstream
/// `REMOTE_CATALOG_ATTEMPT_TIMEOUT_MS`).
pub const REMOTE_CATALOG_ATTEMPT_TIMEOUT_MS: u64 = 4_000;

/// Minimum interval between remote catalog checks (upstream
/// `REMOTE_CATALOG_REFRESH_INTERVAL_MS`): 4 hours.
pub const REMOTE_CATALOG_REFRESH_INTERVAL_MS: u64 = 4 * 60 * 60 * 1000;

/// Merge dynamic models over a baseline, keyed by model id (upstream
/// `mergeModels`): dynamic entries replace same-id entries in place and
/// append otherwise; baseline order is preserved.
pub fn merge_models(baseline: &[Model], dynamic: &[Model]) -> Vec<Model> {
    let mut merged = baseline.to_vec();
    for model in dynamic {
        match merged.iter().position(|entry| entry.id == model.id) {
            Some(index) => merged[index] = model.clone(),
            None => merged.push(model.clone()),
        }
    }
    merged
}

/// Parse a remote catalog payload (upstream `parseCatalog`): accepts a
/// bare array, an object with a `models` array, or a generic object of
/// values; entries must be objects with an `id`; each model's provider is
/// forced to the provider id.
pub fn parse_catalog(provider_id: &str, value: &serde_json::Value) -> Result<Vec<Model>, String> {
    let entries: Vec<&serde_json::Value> = if let Some(array) = value.as_array() {
        array.iter().collect()
    } else if let Some(object) = value.as_object() {
        if let Some(models) = object.get("models").and_then(|m| m.as_array()) {
            models.iter().collect()
        } else {
            object.values().collect()
        }
    } else {
        return Err(format!(
            "Invalid model catalog for provider \"{provider_id}\""
        ));
    };
    Ok(entries
        .into_iter()
        .filter(|entry| entry.is_object() && entry.get("id").is_some())
        .filter_map(|entry| {
            // Upstream spreads the raw entry into a loosely typed Model;
            // the port fills missing structural fields with defaults so
            // partial catalog entries still load.
            let mut value = entry.clone();
            if let Some(object) = value.as_object_mut() {
                object
                    .entry("api")
                    .or_insert(serde_json::Value::String("openai-completions".to_string()));
                object
                    .entry("baseUrl")
                    .or_insert(serde_json::Value::String(String::new()));
                object.entry("input").or_insert(serde_json::json!([]));
                object
                    .entry("name")
                    .or_insert(serde_json::Value::String(String::new()));
                object
                    .entry("contextWindow")
                    .or_insert(serde_json::Value::Number(serde_json::Number::from(0u64)));
                object
                    .entry("maxTokens")
                    .or_insert(serde_json::Value::Number(serde_json::Number::from(0u64)));
                object
                    .entry("reasoning")
                    .or_insert(serde_json::Value::Bool(false));
                object
                    .entry("provider")
                    .or_insert(serde_json::Value::String(provider_id.to_string()));
                object.entry("cost").or_insert(serde_json::json!({
                    "input": 0.0,
                    "output": 0.0,
                    "cacheRead": 0.0,
                    "cacheWrite": 0.0
                }));
            }
            let mut model: Model = serde_json::from_value(value).ok()?;
            model.provider = provider_id.to_string();
            Some(model)
        })
        .collect())
}

/// The stored overlay models eligible for restoration (upstream
/// `remoteModels`): none without an entry; suppressed when the local
/// generated catalog is at least as new as the remote one.
pub fn remote_models(
    entry: Option<&ModelsStoreEntry>,
    local_generated_at: Option<u64>,
) -> Vec<Model> {
    let Some(entry) = entry else {
        return Vec::new();
    };
    if let Some(local) = local_generated_at
        && entry
            .last_modified
            .is_none_or(|last_modified| last_modified <= local)
    {
        return Vec::new();
    }
    entry.models.clone()
}

/// The outcome of the host fetch attempt (upstream the response fields
/// the provider touches).
#[derive(Debug, Clone, Default)]
pub struct FetchOutcome {
    pub status: u16,
    pub body: Option<serde_json::Value>,
    pub last_modified_header: Option<String>,
    pub etag_header: Option<String>,
}

/// The next persistence + update action for a refresh (upstream the
/// refreshModels body). `persist` updates the store entry; `update`
/// replaces the dynamic overlay; `error` aborts with the upstream
/// message.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefreshAction {
    pub persist: Option<ModelsStoreEntry>,
    pub update_overlay: Option<Vec<Model>>,
    pub error: Option<String>,
}

/// Decide the refresh action for a fetch outcome (upstream the decision
/// tree inside `refreshModels`).
pub fn decide_refresh_action(
    provider_id: &str,
    stored: Option<&ModelsStoreEntry>,
    outcome: &FetchOutcome,
    now: u64,
) -> Result<RefreshAction, String> {
    let base_entry = || ModelsStoreEntry {
        models: stored.map(|s| s.models.clone()).unwrap_or_default(),
        last_modified: stored.and_then(|s| s.last_modified),
        checked_at: Some(now),
        etag: stored.and_then(|s| s.etag.clone()),
    };

    // Unchanged: the dynamic overlay already holds the stored models, so
    // only the freshness window moves.
    if outcome.status == 304 {
        return Ok(RefreshAction {
            persist: Some(base_entry()),
            update_overlay: None,
            error: None,
        });
    }
    // Provider has no remote catalog: clear the validator so the next
    // refresh re-downloads instead of revalidating.
    if outcome.status == 404 || outcome.status == 501 {
        let mut entry = base_entry();
        entry.last_modified = Some(0);
        entry.etag = None;
        return Ok(RefreshAction {
            persist: Some(entry),
            update_overlay: None,
            error: None,
        });
    }
    if !(200..300).contains(&outcome.status) {
        // Transient failure: the cached body and its validator stay valid,
        // so keep the etag and let the next refresh revalidate.
        return Err(format!(
            "Model catalog request failed for {provider_id}: {}",
            outcome.status
        ));
    }
    let refreshed = parse_catalog(
        provider_id,
        outcome.body.as_ref().unwrap_or(&serde_json::Value::Null),
    )?;
    let last_modified = outcome
        .last_modified_header
        .as_deref()
        .and_then(parse_http_date);
    let entry = ModelsStoreEntry {
        models: refreshed.clone(),
        checked_at: Some(now),
        last_modified: Some(last_modified.unwrap_or(0)),
        etag: outcome.etag_header.clone(),
    };
    Ok(RefreshAction {
        persist: Some(entry.clone()),
        update_overlay: Some(remote_models(Some(&entry), None)),
        error: None,
    })
}

/// Whether a remote check should run at all (upstream the freshness gate
/// before the fetch): skipped when a stored check happened within the
/// refresh interval.
pub fn should_check_remote(stored: Option<&ModelsStoreEntry>, force: bool, now: u64) -> bool {
    if force {
        return true;
    }
    match stored {
        Some(stored) => match (stored.checked_at, stored.last_modified) {
            (Some(checked_at), Some(_)) => {
                now.saturating_sub(checked_at) >= REMOTE_CATALOG_REFRESH_INTERVAL_MS
            }
            _ => true,
        },
        None => true,
    }
}

/// Parse an HTTP date header; the upstream uses Date.parse, so the port
/// accepts only a unix-millisecond number for tests (a divergence: full
/// RFC 1123 parsing is host-side).
fn parse_http_date(value: &str) -> Option<u64> {
    value.parse::<u64>().ok()
}
