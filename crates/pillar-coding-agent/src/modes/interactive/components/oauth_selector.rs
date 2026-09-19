//! Port of packages/coding-agent/src/modes/interactive/components/
//! oauth-selector.ts (pi v0.84.3): the provider list `/login` and `/logout`
//! open (upstream `OAuthSelectorComponent`).
//!
//! divergences:
//! - pillar-tui keeps keybinding dispatch host-side, so
//!   [`OAuthSelectorComponent::handle_key`] answers an
//!   [`AuthSelectorOutcome`] instead of invoking `onSelect` / `onCancel`.
//! - upstream stores the `ApiKeyAuth` / `OAuthAuth` method itself and only
//!   ever reads `method.name` (for the fuzzy text); the port stores that
//!   display name ([`AuthSelectorProvider::method_name`]), which keeps the
//!   component free of trait objects.
//! - upstream rebuilds the list into a container on every mutation; the port
//!   renders from the current selection, so the theme is read at render time
//!   (the port's global-theme idiom).

use pillar_ai::auth_types::AuthCheck;
use pillar_tui::components::TruncatedText;
use pillar_tui::fuzzy::fuzzy_filter_by;
use pillar_tui::input::{Input, dispatch_input_keybinding};
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::tui::{Component, Focusable};

use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::theme::theme;

/// Rows the viewport shows at once (upstream `maxVisible`).
pub const AUTH_SELECTOR_MAX_VISIBLE: usize = 8;

/// One selectable auth entry (upstream `AuthSelectorProvider`).
#[derive(Debug, Clone, PartialEq)]
pub struct AuthSelectorProvider {
    pub id: String,
    pub name: String,
    /// `"oauth"` or `"api_key"`.
    pub auth_type: String,
    /// The auth method's display name (upstream `method?.name`).
    pub method_name: Option<String>,
    /// Resolved status for this provider, when known.
    pub status: Option<AuthCheck>,
}

/// Upstream `formatAuthSelectorProviderType`.
pub fn format_auth_selector_provider_type(auth_type: &str) -> &'static str {
    if auth_type == "oauth" {
        "subscription"
    } else {
        "API key"
    }
}

/// Upstream's `/^[A-Z][A-Z0-9_]*(?:, [A-Z][A-Z0-9_]*)*$/`: a source that is
/// nothing but a comma-space separated list of env var names is shown as
/// `env: …` (sources like `~/.aws/credentials` are shown verbatim).
pub fn is_env_var_source(source: &str) -> bool {
    if source.is_empty() {
        return false;
    }
    source.split(", ").all(|name| {
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        first.is_ascii_uppercase()
            && chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    })
}

/// Which flow the selector is serving (upstream the `mode` argument).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSelectorMode {
    Login,
    Logout,
}

impl AuthSelectorMode {
    fn title(self) -> &'static str {
        match self {
            AuthSelectorMode::Login => "Select provider to configure:",
            AuthSelectorMode::Logout => "Select provider to logout:",
        }
    }

    /// The message shown when the provider list (not just the search) is empty.
    fn empty_message(self) -> &'static str {
        match self {
            AuthSelectorMode::Login => "No providers available",
            AuthSelectorMode::Logout => "No providers logged in. Use /login first.",
        }
    }
}

/// What the host must do after a key (upstream the component callbacks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSelectorOutcome {
    /// Handled inside the selector (navigation, filtering).
    Consumed,
    /// Enter: configure/logout this provider (upstream `onSelect`).
    Select {
        provider_id: String,
        auth_type: String,
    },
    /// Escape / Ctrl+C (upstream `onCancel`).
    Cancel,
}

/// Provider list for the auth flows (upstream `OAuthSelectorComponent`).
pub struct OAuthSelectorComponent {
    mode: AuthSelectorMode,
    all_providers: Vec<AuthSelectorProvider>,
    filtered_providers: Vec<AuthSelectorProvider>,
    selected_index: usize,
    show_auth_type_labels: bool,
    search_input: Input,
    focused: bool,
}

impl OAuthSelectorComponent {
    pub fn new(
        mode: AuthSelectorMode,
        providers: Vec<AuthSelectorProvider>,
        initial_search_input: Option<&str>,
    ) -> Self {
        let show_auth_type_labels = providers
            .iter()
            .map(|provider| provider.auth_type.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 1;
        let mut search_input = Input::new();
        if let Some(initial) = initial_search_input {
            search_input.set_value(initial);
        }
        let mut selector = Self {
            mode,
            all_providers: providers,
            filtered_providers: Vec::new(),
            selected_index: 0,
            show_auth_type_labels,
            search_input,
            focused: false,
        };
        let query = selector.search_input.get_value().to_string();
        selector.filter_providers(&query);
        selector
    }

    /// The providers the viewport can show, in order.
    pub fn filtered_providers(&self) -> &[AuthSelectorProvider] {
        &self.filtered_providers
    }

    /// The highlighted provider.
    pub fn selected_provider(&self) -> Option<&AuthSelectorProvider> {
        self.filtered_providers.get(self.selected_index)
    }

    /// The current search text.
    pub fn search_value(&self) -> &str {
        self.search_input.get_value()
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> AuthSelectorOutcome {
        if with_global_keybindings(|kb| kb.matches(data, "tui.select.up")) {
            if self.filtered_providers.is_empty() {
                return AuthSelectorOutcome::Consumed;
            }
            self.selected_index = self.selected_index.saturating_sub(1);
            return AuthSelectorOutcome::Consumed;
        }
        if with_global_keybindings(|kb| kb.matches(data, "tui.select.down")) {
            if self.filtered_providers.is_empty() {
                return AuthSelectorOutcome::Consumed;
            }
            self.selected_index = (self.selected_index + 1).min(self.filtered_providers.len() - 1);
            return AuthSelectorOutcome::Consumed;
        }
        if with_global_keybindings(|kb| kb.matches(data, "tui.select.confirm")) {
            if let Some(provider) = self.selected_provider() {
                return AuthSelectorOutcome::Select {
                    provider_id: provider.id.clone(),
                    auth_type: provider.auth_type.clone(),
                };
            }
            return AuthSelectorOutcome::Consumed;
        }
        if with_global_keybindings(|kb| kb.matches(data, "tui.select.cancel")) {
            return AuthSelectorOutcome::Cancel;
        }

        // Everything else is search input (upstream passes it to
        // `searchInput.handleInput` and re-filters).
        if !dispatch_input_keybinding(&mut self.search_input, data) {
            self.search_input.handle_input(data);
        }
        let query = self.search_input.get_value().to_string();
        self.filter_providers(&query);
        AuthSelectorOutcome::Consumed
    }

    /// Upstream `filterProviders`: fuzzy-match `name id authType method.name`.
    fn filter_providers(&mut self, query: &str) {
        self.filtered_providers = if query.is_empty() {
            self.all_providers.clone()
        } else {
            fuzzy_filter_by(&self.all_providers, query, |provider| {
                format!(
                    "{} {} {} {}",
                    provider.name,
                    provider.id,
                    provider.auth_type,
                    provider.method_name.clone().unwrap_or_default()
                )
            })
            .into_iter()
            .cloned()
            .collect()
        };
        self.selected_index = self
            .selected_index
            .min(self.filtered_providers.len().saturating_sub(1));
    }

    /// Upstream `formatStatusIndicator`.
    fn status_indicator(&self, provider: &AuthSelectorProvider) -> String {
        let theme_handle = theme();
        let Some(status) = &provider.status else {
            return theme_handle.fg("muted", " • unconfigured");
        };
        if status.kind != provider.auth_type {
            let label = if status.kind == "oauth" {
                "subscription configured"
            } else {
                "API key configured"
            };
            return format!(
                "{}{}",
                theme_handle.fg("muted", " • "),
                theme_handle.fg("warning", label)
            );
        }
        let source = status.source.as_deref().filter(|source| !source.is_empty());
        let Some(source) = source else {
            return theme_handle.fg("success", " ✓ configured");
        };
        if source == "OAuth" || source == "stored credential" {
            return theme_handle.fg("success", " ✓ configured");
        }
        let source = if is_env_var_source(source) {
            format!("env: {source}")
        } else {
            source.to_string()
        };
        theme_handle.fg("success", &format!(" ✓ {source}"))
    }

    /// Upstream `updateList`'s viewport window.
    fn viewport(&self) -> (usize, usize) {
        let len = self.filtered_providers.len();
        let last_start = len as isize - AUTH_SELECTOR_MAX_VISIBLE as isize;
        let centered = self.selected_index as isize - (AUTH_SELECTOR_MAX_VISIBLE / 2) as isize;
        let start = centered.min(last_start).max(0) as usize;
        let end = (start + AUTH_SELECTOR_MAX_VISIBLE).min(len);
        (start, end)
    }

    fn provider_line(&self, provider: &AuthSelectorProvider, selected: bool) -> String {
        let theme_handle = theme();
        let auth_type_label = if self.show_auth_type_labels {
            theme_handle.fg(
                "muted",
                &format!(
                    " [{}]",
                    format_auth_selector_provider_type(&provider.auth_type)
                ),
            )
        } else {
            String::new()
        };
        let status = self.status_indicator(provider);
        if selected {
            format!(
                "{}{}{}{}",
                theme_handle.fg("accent", "→ "),
                theme_handle.fg("accent", &provider.name),
                auth_type_label,
                status
            )
        } else {
            format!(
                "  {}{}{}",
                theme_handle.fg("text", &provider.name),
                auth_type_label,
                status
            )
        }
    }
}

impl Component for OAuthSelectorComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(DynamicBorder::new().render(width));
        lines.push(String::new());
        lines.extend(
            TruncatedText::new(
                &theme_handle.fg("accent", &theme_handle.bold(self.mode.title())),
                1,
                0,
            )
            .render(width),
        );
        lines.push(String::new());
        lines.extend(self.search_input.render(width));
        lines.push(String::new());

        let (start, end) = self.viewport();
        for index in start..end {
            let Some(provider) = self.filtered_providers.get(index) else {
                continue;
            };
            let line = self.provider_line(provider, index == self.selected_index);
            lines.extend(TruncatedText::new(&line, 1, 0).render(width));
        }

        if start > 0 || end < self.filtered_providers.len() {
            lines.extend(
                TruncatedText::new(
                    &theme_handle.fg(
                        "muted",
                        &format!(
                            "  ({}/{})",
                            self.selected_index + 1,
                            self.filtered_providers.len()
                        ),
                    ),
                    1,
                    0,
                )
                .render(width),
            );
        }

        if self.filtered_providers.is_empty() {
            let message = if self.all_providers.is_empty() {
                self.mode.empty_message()
            } else {
                "No matching providers"
            };
            lines.extend(
                TruncatedText::new(&theme_handle.fg("muted", &format!("  {message}")), 1, 0)
                    .render(width),
            );
        }

        lines.push(String::new());
        lines.extend(DynamicBorder::new().render(width));
        lines
    }
}

impl Focusable for OAuthSelectorComponent {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search_input.set_focused(focused);
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(
        id: &str,
        name: &str,
        auth_type: &str,
        status: Option<AuthCheck>,
    ) -> AuthSelectorProvider {
        AuthSelectorProvider {
            id: id.to_string(),
            name: name.to_string(),
            auth_type: auth_type.to_string(),
            method_name: Some(format!("{name} auth")),
            status,
        }
    }

    #[test]
    fn env_var_sources_are_recognised_like_upstream() {
        assert!(is_env_var_source("ANTHROPIC_API_KEY"));
        assert!(is_env_var_source("AWS_PROFILE, AWS_ACCESS_KEY_ID"));
        assert!(is_env_var_source("A1_B2"));
        assert!(!is_env_var_source(""));
        // Lowercase, a bare comma, a leading space, or a path are verbatim.
        assert!(!is_env_var_source("ANTHROPIC_API_KEY,aws"));
        assert!(!is_env_var_source("AWS_PROFILE,AWS_ACCESS_KEY_ID"));
        assert!(!is_env_var_source(" AWS_PROFILE"));
        assert!(!is_env_var_source("~/.aws/credentials"));
        assert!(!is_env_var_source("stored credential"));
    }

    #[test]
    fn filtering_and_selection_outcomes() {
        let _guard = crate::modes::interactive::components::test_support::setup();
        let mut selector = OAuthSelectorComponent::new(
            AuthSelectorMode::Login,
            vec![
                provider("openai", "OpenAI", "api_key", None),
                provider("anthropic", "Anthropic", "api_key", None),
            ],
            None,
        );
        assert_eq!(selector.filtered_providers().len(), 2);
        // Up clamps at the top.
        assert_eq!(selector.handle_key("\x1b[A"), AuthSelectorOutcome::Consumed);
        assert_eq!(
            selector.handle_key("\r"),
            AuthSelectorOutcome::Select {
                provider_id: "openai".to_string(),
                auth_type: "api_key".to_string(),
            }
        );
        assert_eq!(selector.handle_key("\x1b[B"), AuthSelectorOutcome::Consumed);

        // Typing re-filters. The highlighted *index* is preserved across
        // filters (upstream only clamps it into range), and the fuzzy match is
        // a subsequence match, so "anth" also matches "…api_key OpenAI auth".
        for ch in "anth".chars() {
            selector.handle_key(&ch.to_string());
        }
        let ids: Vec<&str> = selector
            .filtered_providers()
            .iter()
            .map(|provider| provider.id.as_str())
            .collect();
        assert_eq!(ids, ["anthropic", "openai"], "best match first");
        assert_eq!(
            selector.selected_provider().map(|p| p.id.as_str()),
            Some("openai"),
            "the index survives the filter"
        );

        // A query that matches a single provider clamps the index onto it.
        for ch in "anthropic".chars() {
            selector.handle_key(&ch.to_string());
        }
        assert_eq!(selector.filtered_providers().len(), 1);
        assert_eq!(
            selector.selected_provider().map(|p| p.id.as_str()),
            Some("anthropic")
        );

        // A query that matches nothing yields no selection, and Enter is a no-op.
        selector.handle_key("zzz");
        assert!(selector.filtered_providers().is_empty());
        assert_eq!(selector.handle_key("\r"), AuthSelectorOutcome::Consumed);
        assert_eq!(selector.handle_key("\x1b"), AuthSelectorOutcome::Cancel);
    }
}
