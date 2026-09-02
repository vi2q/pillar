//! Runtime support for the generated model catalog
//! (`src/models_generated.rs`, emitted by `src/bin/generate-models.rs`).
//!
//! The generated file embeds per-model JSON; this module turns those JSON
//! blobs into `Model` values and exposes the provider registry used by
//! `builtin_models()`.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::Value;

use crate::types::Model;

/// One generated model entry: the API it speaks plus the raw catalog JSON.
pub struct CatalogModel {
    pub api: &'static str,
    pub id: &'static str,
    pub data: &'static str,
}

/// One generated provider entry.
pub struct ModelCatalog {
    pub provider: &'static str,
    pub models: &'static [CatalogModel],
}

/// Parse a generated catalog model into a runtime `Model`.
pub fn catalog_model_to_model(entry: &CatalogModel) -> Option<Model> {
    let value: Value = serde_json::from_str(entry.data).ok()?;
    let mut model: Model = serde_json::from_value(value).ok()?;
    if model.api.is_empty() {
        model.api = entry.api.to_string();
    }
    Some(model)
}

/// Provider id -> models, from the generated catalog.
pub fn generated_models() -> &'static BTreeMap<&'static str, Vec<Model>> {
    static MODELS: OnceLock<BTreeMap<&'static str, Vec<Model>>> = OnceLock::new();
    MODELS.get_or_init(|| {
        let mut out: BTreeMap<&'static str, Vec<Model>> = BTreeMap::new();
        for catalog in crate::models_generated::MODELS {
            let models = catalog
                .models
                .iter()
                .filter_map(catalog_model_to_model)
                .collect();
            out.insert(catalog.provider, models);
        }
        out
    })
}

/// All builtin provider ids from the generated catalog, sorted.
pub fn generated_provider_ids() -> Vec<&'static str> {
    generated_models().keys().copied().collect()
}

/// Models for one builtin provider (empty when unknown).
pub fn generated_models_for(provider: &str) -> Vec<Model> {
    generated_models()
        .get(provider)
        .cloned()
        .unwrap_or_default()
}
