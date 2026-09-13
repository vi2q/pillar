//! Port of packages/coding-agent/src/modes/interactive/interactive-mode.ts
//! (pi v0.84.3), starting with the pure helpers at the top of the module.
//!
//! Progress: the value helpers below are ported. The `InteractiveMode` class
//! (rendering loop, slash-command handling, selectors) is ported
//! incrementally; helpers that need not-yet-ported types (`AuthSelectorProvider`
//! login completions, `ExpandableText`) land with those types.

use std::io::IsTerminal;

use pillar_ai::types::Model;
use pillar_tui::autocomplete::AutocompleteItem;

use crate::cli::args::APP_NAME;
use crate::core::model_resolver::default_model_per_provider;
use crate::core::session_manager::SessionManager;

/// Warning shown when Anthropic subscription auth is in use (upstream
/// `ANTHROPIC_SUBSCRIPTION_AUTH_WARNING`).
pub const ANTHROPIC_SUBSCRIPTION_AUTH_WARNING: &str = "Anthropic subscription auth is active. Third-party harness usage draws from extra usage and is billed per token, not your Claude plan limits. Manage extra usage at https://claude.ai/settings/usage. Disable this warning in /settings.";

/// Terminal error codes that mean the terminal is gone (upstream
/// `DEAD_TERMINAL_ERROR_CODES`).
pub const DEAD_TERMINAL_ERROR_CODES: [&str; 3] = ["EIO", "EPIPE", "ENOTCONN"];

/// Whether an I/O error means the terminal died (upstream
/// `isDeadTerminalError`): EIO, EPIPE, or ENOTCONN.
pub fn is_dead_terminal_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::BrokenPipe | ErrorKind::NotConnected
    ) {
        return true;
    }
    // EIO has no `ErrorKind` variant; macOS/Linux agree on 5.
    error.raw_os_error() == Some(5)
}

/// Whether the API key is an Anthropic subscription (OAuth) token (upstream
/// `isAnthropicSubscriptionAuthKey`).
pub fn is_anthropic_subscription_auth_key(api_key: Option<&str>) -> bool {
    api_key.is_some_and(|key| key.starts_with("sk-ant-oat"))
}

/// Whether the model is the placeholder unknown model (upstream
/// `isUnknownModel`).
pub fn is_unknown_model(model: Option<&Model>) -> bool {
    model.is_some_and(|model| {
        model.provider == "unknown" && model.id == "unknown" && model.api == "unknown"
    })
}

/// Shell-quote a value only when it contains unsafe characters (upstream
/// `quoteIfNeeded`).
pub fn quote_if_needed(value: &str) -> String {
    let safe = !value.is_empty()
        && !value.chars().any(|character| {
            !(character.is_ascii_alphanumeric() || "_\\-./~:@".contains(character))
        });
    if safe {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The `pillar` resume command for a session, or `None` when resuming makes no
/// sense (upstream `formatResumeCommand`).
pub fn format_resume_command(session_manager: &SessionManager) -> Option<String> {
    format_resume_command_with(session_manager, std::io::stdout().is_terminal())
}

/// As [`format_resume_command`] with the TTY check supplied (upstream reads
/// `process.stdout.isTTY`).
pub fn format_resume_command_with(
    session_manager: &SessionManager,
    stdout_is_tty: bool,
) -> Option<String> {
    if !stdout_is_tty {
        return None;
    }
    if !session_manager.is_persisted() {
        return None;
    }
    let session_file = session_manager.session_file()?;
    if !session_file.exists() {
        return None;
    }

    let mut args = vec![APP_NAME.to_string()];
    if !session_manager.uses_default_session_dir() {
        args.push("--session-dir".to_string());
        args.push(quote_if_needed(
            &session_manager.session_dir().to_string_lossy(),
        ));
    }
    args.push("--session".to_string());
    args.push(session_manager.session_id().to_string());
    Some(args.join(" "))
}

/// Whether the provider has a built-in default model (upstream
/// `hasDefaultModelProvider`).
pub fn has_default_model_provider(provider_id: &str) -> bool {
    default_model_per_provider(provider_id).is_some()
}

/// Guidance shown after a llama.cpp login (upstream
/// `llamaCppPostLoginGuidance`).
pub fn llama_cpp_post_login_guidance(action_label: &str, loaded_model_count: usize) -> String {
    if loaded_model_count == 0 {
        format!(
            "{action_label}. No llama.cpp models are loaded. Use /llama to load a model, then /model to select it."
        )
    } else {
        format!(
            "{action_label}. Use /model to select a loaded llama.cpp model, or /llama to manage models."
        )
    }
}

/// Filter items for the autocomplete (upstream
/// `createFuzzyAutocompleteItems`): `None` when nothing matches.
pub fn create_fuzzy_autocomplete_items<T>(
    items: &[T],
    prefix: &str,
    get_search_text: impl Fn(&T) -> String,
    to_autocomplete_item: impl Fn(&T) -> AutocompleteItem,
) -> Option<Vec<AutocompleteItem>> {
    let filtered = pillar_tui::fuzzy::fuzzy_filter_by(items, prefix, &get_search_text);
    if filtered.is_empty() {
        return None;
    }
    Some(filtered.into_iter().map(&to_autocomplete_item).collect())
}
