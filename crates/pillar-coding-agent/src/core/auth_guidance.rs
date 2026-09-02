//! Port of packages/coding-agent/src/core/auth-guidance.ts (pi v0.84.3):
//! user-facing messages for missing provider auth and model selection.
//!
//! divergence: upstream builds docs paths from the installed package
//! directory; the port reports the documentation filenames without an
//! absolute package path.

const UNKNOWN_PROVIDER: &str = "unknown";

/// Provider login help block (upstream `getProviderLoginHelp`).
pub fn get_provider_login_help() -> String {
    [
        "Use /login to log into a provider via OAuth or API key. See:",
        "  docs/providers.md",
        "  docs/models.md",
    ]
    .join("\n")
}

/// Upstream `formatNoModelsAvailableMessage`.
pub fn format_no_models_available_message() -> String {
    format!("No models available. {}", get_provider_login_help())
}

/// Upstream `formatNoModelSelectedMessage`.
pub fn format_no_model_selected_message() -> String {
    format!(
        "No model selected.\n\n{}\n\nThen use /model to select a model.",
        get_provider_login_help()
    )
}

/// Upstream `formatNoApiKeyFoundMessage`.
pub fn format_no_api_key_found_message(provider: &str) -> String {
    let provider_display = if provider == UNKNOWN_PROVIDER {
        "the selected model"
    } else {
        provider
    };
    format!(
        "No API key found for {}.\n\n{}",
        provider_display,
        get_provider_login_help()
    )
}
