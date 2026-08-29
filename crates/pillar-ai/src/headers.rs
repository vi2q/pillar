//! Port of packages/ai/src/utils/headers.ts (pi v0.84.3) — header
//! normalization helpers shared by providers.

use std::collections::BTreeMap;

use crate::types::ProviderHeaders;

/// Upstream `providerHeadersToRecord`: drop `None`-suppressed headers and
/// return `None` when nothing remains.
pub fn provider_headers_to_record(
    headers: Option<&ProviderHeaders>,
) -> Option<BTreeMap<String, String>> {
    let headers = headers?;
    let mut result = BTreeMap::new();
    for (name, value) in headers {
        if let Some(value) = value {
            result.insert(name.clone(), value.clone());
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}
