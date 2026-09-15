//! Port of packages/coding-agent/src/core/provider-attribution.ts (pi
//! v0.84.3): default attribution headers per provider plus the OpenCode
//! session headers, merged under explicit header sources.
//!
//! divergence: the telemetry-enabled check is passed in as a resolved bool
//! (the caller applies `isInstallTelemetryEnabled`, which also reads
//! `PI_TELEMETRY`).

use pillar_ai::types::{Model, ProviderHeaders};

const OPENROUTER_HOST: &str = "openrouter.ai";
const NVIDIA_NIM_HOST: &str = "integrate.api.nvidia.com";
const CLOUDFLARE_API_HOST: &str = "api.cloudflare.com";
const CLOUDFLARE_AI_GATEWAY_HOST: &str = "gateway.ai.cloudflare.com";
const OPENCODE_HOST: &str = "opencode.ai";

fn matches_host(base_url: &str, expected_host: &str) -> bool {
    url_host(base_url).is_some_and(|host| host == expected_host)
}

/// Extract the hostname from a URL without pulling in a URL crate; the
/// upstream uses `new URL(...).hostname` and catches parse failures.
fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let end = rest.find(['/', ':', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    (!host.is_empty()).then_some(host)
}

fn is_open_router_model(model: &Model) -> bool {
    model.provider == "openrouter" || model.base_url.contains(OPENROUTER_HOST)
}

fn is_nvidia_nim_model(model: &Model) -> bool {
    model.provider == "nvidia" || matches_host(&model.base_url, NVIDIA_NIM_HOST)
}

fn is_cloudflare_model(model: &Model) -> bool {
    model.provider == "cloudflare-workers-ai"
        || model.provider == "cloudflare-ai-gateway"
        || matches_host(&model.base_url, CLOUDFLARE_API_HOST)
        || matches_host(&model.base_url, CLOUDFLARE_AI_GATEWAY_HOST)
}

fn default_attribution_headers(
    model: &Model,
    telemetry_enabled: bool,
) -> Option<Vec<(String, String)>> {
    if !telemetry_enabled {
        return None;
    }

    if is_open_router_model(model) {
        return Some(vec![
            ("HTTP-Referer".to_string(), "https://pi.dev".to_string()),
            ("X-OpenRouter-Title".to_string(), "pi".to_string()),
            (
                "X-OpenRouter-Categories".to_string(),
                "cli-agent".to_string(),
            ),
        ]);
    }

    if is_nvidia_nim_model(model) {
        return Some(vec![(
            "X-BILLING-INVOKE-ORIGIN".to_string(),
            "Pi".to_string(),
        )]);
    }

    if is_cloudflare_model(model) {
        return Some(vec![(
            "User-Agent".to_string(),
            "pi-coding-agent".to_string(),
        )]);
    }

    None
}

fn session_headers(model: &Model, session_id: Option<&str>) -> Option<Vec<(String, String)>> {
    let session_id = session_id?;
    if model.provider != "opencode"
        && model.provider != "opencode-go"
        && !matches_host(&model.base_url, OPENCODE_HOST)
    {
        return None;
    }
    Some(vec![
        ("x-opencode-session".to_string(), session_id.to_string()),
        ("x-opencode-client".to_string(), "pi".to_string()),
    ])
}

/// Merge provider attribution headers (OpenCode session headers, default
/// attribution headers) under explicit header sources. Later sources win.
pub fn merge_provider_attribution_headers(
    model: &Model,
    telemetry_enabled: bool,
    session_id: Option<&str>,
    header_sources: &[&ProviderHeaders],
) -> Option<ProviderHeaders> {
    let mut merged: ProviderHeaders = ProviderHeaders::new();
    for (key, value) in session_headers(model, session_id).into_iter().flatten() {
        merged.insert(key, Some(value));
    }
    for (key, value) in default_attribution_headers(model, telemetry_enabled)
        .into_iter()
        .flatten()
    {
        merged.insert(key, Some(value));
    }

    for headers in header_sources {
        for (key, value) in headers.iter() {
            merged.insert(key.clone(), value.clone());
        }
    }

    (!merged.is_empty()).then_some(merged)
}
