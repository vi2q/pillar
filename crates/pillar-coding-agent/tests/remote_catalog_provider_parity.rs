//! Parity tests for remote-catalog-provider.ts (pi v0.84.3), the pure
//! decision core: model merging, catalog parsing, stale-overlay gating,
//! the freshness window, and the refresh decision tree.

use serde_json::json;

use pillar_ai::models_store::ModelsStoreEntry;
use pillar_ai::types::Model;
use pillar_coding_agent::core::remote_catalog_provider::{
    DEFAULT_CATALOG_BASE_URL, FetchOutcome, REMOTE_CATALOG_ATTEMPT_TIMEOUT_MS,
    REMOTE_CATALOG_REFRESH_INTERVAL_MS, decide_refresh_action, merge_models, parse_catalog,
    remote_models, should_check_remote,
};

fn model(id: &str, provider: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "openai-completions".to_string(),
        provider: provider.to_string(),
        base_url: "https://example.com".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: Default::default(),
        context_window: 100_000,
        max_tokens: 4096,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn store_entry(
    models: Vec<Model>,
    last_modified: Option<u64>,
    checked_at: Option<u64>,
    etag: Option<&str>,
) -> ModelsStoreEntry {
    ModelsStoreEntry {
        models,
        last_modified,
        checked_at,
        etag: etag.map(str::to_string),
    }
}

// --- merge ------------------------------------------------------------------------------------

#[test]
fn merge_models_replaces_by_id_and_appends() {
    let baseline = vec![model("m1", "p"), model("m2", "p")];
    let dynamic = vec![model("m2", "p"), model("m3", "p")];
    let merged = merge_models(&baseline, &dynamic);
    let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["m1", "m2", "m3"]);
    // m2 replaced in place (dynamic provider wins).
    assert_eq!(merged[1].provider, "p");
}

#[test]
fn catalog_constants() {
    assert_eq!(DEFAULT_CATALOG_BASE_URL, "https://pi.dev");
    assert_eq!(REMOTE_CATALOG_ATTEMPT_TIMEOUT_MS, 4_000);
    assert_eq!(REMOTE_CATALOG_REFRESH_INTERVAL_MS, 4 * 60 * 60 * 1000);
}

// --- parsing ----------------------------------------------------------------------------------

#[test]
fn parse_catalog_accepts_array_models_object_and_object_map() {
    // Bare array.
    let array = json!([{"id": "m1", "name": "M1"}, {"id": "m2"}]);
    let parsed = parse_catalog("prov", &array).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].provider, "prov");

    // Object with a models array.
    let wrapped = json!({"models": [{"id": "m1"}]});
    let parsed = parse_catalog("prov", &wrapped).unwrap();
    assert_eq!(parsed.len(), 1);

    // Generic object of values (missing id entries filtered).
    let map = json!({"a": {"id": "m1"}, "b": {"name": "no id"}, "c": "string"});
    let parsed = parse_catalog("prov", &map).unwrap();
    assert_eq!(parsed.len(), 1);

    // Invalid shapes error with the upstream message.
    assert_eq!(
        parse_catalog("prov", &json!("string")).unwrap_err(),
        "Invalid model catalog for provider \"prov\""
    );
    assert_eq!(
        parse_catalog("prov", &json!(42)).unwrap_err(),
        "Invalid model catalog for provider \"prov\""
    );
}

// --- stale overlay gating ------------------------------------------------------------------------

#[test]
fn remote_models_suppressed_by_local_generation() {
    let entry = store_entry(
        vec![model("m1", "p")],
        Some(1000),
        Some(1000),
        Some("\"e\""),
    );

    // No local generation: overlay restored.
    assert_eq!(remote_models(Some(&entry), None).len(), 1);
    // Local generated at 2000 >= remote lastModified 1000: suppressed.
    assert!(remote_models(Some(&entry), Some(2000)).is_empty());
    // Local generated at 500 < 1000: restored.
    assert_eq!(remote_models(Some(&entry), Some(500)).len(), 1);
    // No entry: nothing.
    assert!(remote_models(None, None).is_empty());
}

// --- freshness gate --------------------------------------------------------------------------------

#[test]
fn should_check_remote_honors_interval_and_force() {
    let fresh = store_entry(vec![], Some(1000), Some(1000), Some("\"e\""));
    let stale = store_entry(vec![], Some(1000), Some(0), Some("\"e\""));

    // Within the 4h window: skip.
    assert!(!should_check_remote(
        Some(&fresh),
        false,
        1000 + REMOTE_CATALOG_REFRESH_INTERVAL_MS - 1
    ));
    // Past the window: check.
    assert!(should_check_remote(
        Some(&stale),
        false,
        REMOTE_CATALOG_REFRESH_INTERVAL_MS + 1
    ));
    // Force always checks.
    assert!(should_check_remote(Some(&fresh), true, 1000));
    // No stored entry: check.
    assert!(should_check_remote(None, false, 1000));
    // Missing lastModified: check.
    let no_modified = store_entry(vec![], None, Some(1000), None);
    assert!(should_check_remote(Some(&no_modified), false, 1000));
}

// --- refresh decision tree ---------------------------------------------------------------------------

#[test]
fn refresh_304_only_moves_freshness_window() {
    let stored = store_entry(
        vec![model("m1", "p")],
        Some(1000),
        Some(500),
        Some("\"etag\""),
    );
    let action = decide_refresh_action(
        "p",
        Some(&stored),
        &FetchOutcome {
            status: 304,
            ..Default::default()
        },
        2000,
    )
    .unwrap();
    let persisted = action.persist.unwrap();
    assert_eq!(persisted.checked_at, Some(2000));
    assert_eq!(persisted.last_modified, Some(1000)); // validator kept
    assert_eq!(persisted.etag.as_deref(), Some("\"etag\""));
    assert!(action.update_overlay.is_none());
    assert!(action.error.is_none());
}

#[test]
fn refresh_404_clears_validator() {
    let stored = store_entry(
        vec![model("m1", "p")],
        Some(1000),
        Some(500),
        Some("\"etag\""),
    );
    for status in [404u16, 501] {
        let action = decide_refresh_action(
            "p",
            Some(&stored),
            &FetchOutcome {
                status,
                ..Default::default()
            },
            2000,
        )
        .unwrap();
        let persisted = action.persist.unwrap();
        assert_eq!(persisted.last_modified, Some(0));
        assert_eq!(persisted.etag, None);
        assert!(action.update_overlay.is_none());
    }
}

#[test]
fn refresh_transient_failure_keeps_etag_and_errors() {
    let stored = store_entry(
        vec![model("m1", "p")],
        Some(1000),
        Some(500),
        Some("\"etag\""),
    );
    let error = decide_refresh_action(
        "p",
        Some(&stored),
        &FetchOutcome {
            status: 503,
            ..Default::default()
        },
        2000,
    )
    .unwrap_err();
    assert_eq!(error, "Model catalog request failed for p: 503");
}

#[test]
fn refresh_success_parses_and_publishes() {
    let body = json!([{"id": "m1", "name": "M1"}, {"id": "m2"}]);
    let action = decide_refresh_action(
        "p",
        None,
        &FetchOutcome {
            status: 200,
            body: Some(body),
            last_modified_header: Some("1704067200000".to_string()),
            etag_header: Some("\"new-etag\"".to_string()),
        },
        2000,
    )
    .unwrap();
    let persisted = action.persist.unwrap();
    assert_eq!(persisted.checked_at, Some(2000));
    assert_eq!(persisted.last_modified, Some(1_704_067_200_000));
    assert_eq!(persisted.etag.as_deref(), Some("\"new-etag\""));
    // Overlay published from the refreshed entry (no local generation
    // gating here — the caller applies remote_models with its own
    // localGeneratedAt).
    let overlay = action.update_overlay.unwrap();
    assert_eq!(overlay.len(), 2);
    assert_eq!(overlay[0].provider, "p");
}

#[test]
fn refresh_success_with_bad_date_treats_as_zero() {
    let action = decide_refresh_action(
        "p",
        None,
        &FetchOutcome {
            status: 200,
            body: Some(json!([])),
            last_modified_header: Some("not-a-date".to_string()),
            etag_header: None,
        },
        2000,
    )
    .unwrap();
    assert_eq!(action.persist.unwrap().last_modified, Some(0));
}

#[test]
fn refresh_success_with_invalid_body_errors() {
    let error = decide_refresh_action(
        "p",
        None,
        &FetchOutcome {
            status: 200,
            body: Some(json!("garbage")),
            last_modified_header: None,
            etag_header: None,
        },
        2000,
    )
    .unwrap_err();
    assert!(
        error.contains("Invalid model catalog for provider \"p\""),
        "{error}"
    );
}
