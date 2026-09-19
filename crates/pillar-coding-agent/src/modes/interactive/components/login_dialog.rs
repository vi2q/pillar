//! Port of packages/coding-agent/src/modes/interactive/components/
//! login-dialog.ts (pi v0.84.3): the component that replaces the editor while
//! a provider login runs (upstream `LoginDialogComponent`).
//!
//! divergences:
//! - pillar-tui keeps keybinding dispatch host-side, so the `show_*` methods
//!   only publish state and [`LoginDialogComponent::handle_key`] answers a
//!   [`LoginDialogOutcome`]. Upstream's `showPrompt` / `showManualInput`
//!   return a Promise the login flow awaits; here the host owns that channel
//!   (the reply is whatever the host does with `Submit` / `Cancel`).
//! - upstream's `tui.requestRender()` calls are the host's dirty flag, so the
//!   component has none.
//! - [`LoginDialogComponent::show_device_code`] takes the two fields the
//!   upstream `OAuthDeviceCodeInfo` prints (its `intervalSeconds` /
//!   `expiresInSeconds` are not rendered).

use pillar_ai::abort::{AbortReason, AbortSignal};
use pillar_ai::auth_types::AuthInfoLink;
use pillar_tui::components::Text;
use pillar_tui::input::{Input, dispatch_input_keybinding};
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::tui::{Component, Focusable};

use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::theme::theme;
use crate::utils::open_browser::open_browser;

/// One line of the dialog's content area. Upstream appends live component
/// instances; the port keeps data (the input widget is the one exception,
/// because it is a stateful widget).
enum DialogContent {
    /// `Spacer(1)`.
    Blank,
    /// `Text(text, paddingX, 0)`.
    Text { text: String, padding_x: usize },
    /// The `Input` widget itself, appended in place.
    Input,
}

/// What the host must do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginDialogOutcome {
    /// Handled inside the dialog (text editing).
    Consumed,
    /// Enter with a pending prompt: the entered value (upstream
    /// `input.onSubmit` → `inputResolver`). The dialog keeps showing what was
    /// submitted.
    Submit(String),
    /// Escape / Ctrl+C: the login was cancelled. The dialog's signal is
    /// aborted and a pending prompt is dropped (upstream `cancel`).
    Cancel,
}

/// Opens a URL in the system browser; injectable so a test never launches
/// one (upstream calls `openBrowser` directly).
pub type UrlOpener = Box<dyn Fn(&str) + Send + Sync>;

/// Login dialog (upstream `LoginDialogComponent`).
pub struct LoginDialogComponent {
    provider_id: String,
    provider_name: String,
    title: String,
    content: Vec<DialogContent>,
    input: Input,
    signal: AbortSignal,
    url_opener: UrlOpener,
    /// True while a prompt is waiting for input (upstream `inputResolver`).
    awaiting_input: bool,
    focused: bool,
}

impl LoginDialogComponent {
    pub fn new(
        provider_id: &str,
        provider_name_override: Option<&str>,
        title_override: Option<&str>,
    ) -> Self {
        let provider_name = provider_name_override.unwrap_or(provider_id).to_string();
        Self {
            provider_id: provider_id.to_string(),
            title: title_override
                .map(str::to_string)
                .unwrap_or_else(|| format!("Login to {provider_name}")),
            provider_name,
            content: Vec::new(),
            input: Input::new(),
            signal: AbortSignal::new(),
            url_opener: Box::new(open_browser),
            awaiting_input: false,
            focused: false,
        }
    }

    /// Replace the URL opener (tests use this instead of a real browser).
    pub fn set_url_opener(&mut self, opener: UrlOpener) {
        self.url_opener = opener;
    }

    /// The flow's abort signal (upstream `get signal`).
    pub fn signal(&self) -> &AbortSignal {
        &self.signal
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// True while the host is waiting for this dialog's input.
    pub fn awaiting_input(&self) -> bool {
        self.awaiting_input
    }

    /// Show the auth URL and optional instructions, and open the browser
    /// (upstream `showAuth`).
    pub fn show_auth(&mut self, url: &str, instructions: Option<&str>) {
        self.content.clear();
        let theme_handle = theme();
        self.content.push(DialogContent::Blank);
        self.content.push(DialogContent::Text {
            text: theme_handle.fg("accent", &osc8_link(url, url)),
            padding_x: 1,
        });
        let click_hint = if cfg!(target_os = "macos") {
            "Cmd+click to open"
        } else {
            "Ctrl+click to open"
        };
        self.content.push(DialogContent::Text {
            text: theme_handle.fg("dim", &osc8_link(url, click_hint)),
            padding_x: 1,
        });
        if let Some(instructions) = instructions {
            self.content.push(DialogContent::Blank);
            self.content.push(DialogContent::Text {
                text: theme_handle.fg("warning", instructions),
                padding_x: 1,
            });
        }
        (self.url_opener)(url);
    }

    /// Show the verification URL and user code (upstream `showDeviceCode`).
    pub fn show_device_code(&mut self, user_code: &str, verification_uri: &str) {
        self.content.clear();
        let theme_handle = theme();
        self.content.push(DialogContent::Blank);
        self.content.push(DialogContent::Text {
            text: theme_handle.fg("accent", &osc8_link(verification_uri, verification_uri)),
            padding_x: 1,
        });
        let click_hint = if cfg!(target_os = "macos") {
            "Cmd+click to open"
        } else {
            "Ctrl+click to open"
        };
        self.content.push(DialogContent::Text {
            text: theme_handle.fg("dim", &osc8_link(verification_uri, click_hint)),
            padding_x: 1,
        });
        self.content.push(DialogContent::Blank);
        self.content.push(DialogContent::Text {
            text: theme_handle.fg("warning", &format!("Enter code: {user_code}")),
            padding_x: 1,
        });
    }

    /// Show the input for a manual code/URL (upstream `showManualInput`).
    pub fn show_manual_input(&mut self, prompt: &str) {
        let theme_handle = theme();
        self.input.set_value("");
        self.content.push(DialogContent::Blank);
        self.content.push(DialogContent::Text {
            text: theme_handle.fg("dim", prompt),
            padding_x: 1,
        });
        self.content.push(DialogContent::Input);
        self.content.push(DialogContent::Text {
            text: format!("({})", key_hint("tui.select.cancel", "to cancel")),
            padding_x: 1,
        });
        self.awaiting_input = true;
    }

    /// Show a prompt and wait for input (upstream `showPrompt`). Unlike
    /// `showAuth` this appends: the URL from `showAuth` stays visible.
    pub fn show_prompt(&mut self, message: &str, placeholder: Option<&str>) {
        let theme_handle = theme();
        self.content.push(DialogContent::Blank);
        self.content.push(DialogContent::Text {
            text: theme_handle.fg("text", message),
            padding_x: 1,
        });
        if let Some(placeholder) = placeholder {
            self.content.push(DialogContent::Text {
                text: theme_handle.fg("dim", &format!("e.g., {placeholder}")),
                padding_x: 1,
            });
        }
        self.content.push(DialogContent::Input);
        self.content.push(DialogContent::Text {
            text: format!(
                "({} {})",
                key_hint("tui.select.cancel", "to cancel,"),
                key_hint("tui.select.confirm", "to submit")
            ),
            padding_x: 1,
        });
        self.input.set_value("");
        self.awaiting_input = true;
    }

    /// Show informational text before another login step (upstream
    /// `showDetails`). Replaces the content.
    pub fn show_details(&mut self, lines: &[String]) {
        self.content.clear();
        self.content.push(DialogContent::Blank);
        for line in lines {
            self.content.push(DialogContent::Text {
                text: line.clone(),
                padding_x: 1,
            });
        }
    }

    /// Show provider-owned information and links (upstream `showInfo`).
    pub fn show_info(&mut self, message: &str, links: &[AuthInfoLink], show_close_hint: bool) {
        let theme_handle = theme();
        self.content.push(DialogContent::Blank);
        self.content.push(DialogContent::Text {
            text: theme_handle.fg("text", message),
            padding_x: 1,
        });
        for link in links {
            let text = match &link.label {
                Some(label) => format!("{label}: {}", link.url),
                None => link.url.clone(),
            };
            self.content.push(DialogContent::Text {
                text: theme_handle.fg("accent", &osc8_link(&link.url, &text)),
                padding_x: 1,
            });
        }
        if show_close_hint {
            self.content.push(DialogContent::Blank);
            self.content.push(DialogContent::Text {
                text: format!("({})", key_hint("tui.select.cancel", "to close")),
                padding_x: 1,
            });
        }
    }

    /// Show a waiting message for polling flows (upstream `showWaiting`).
    pub fn show_waiting(&mut self, message: &str) {
        let theme_handle = theme();
        self.content.push(DialogContent::Blank);
        self.content.push(DialogContent::Text {
            text: theme_handle.fg("dim", message),
            padding_x: 1,
        });
        self.content.push(DialogContent::Text {
            text: format!("({})", key_hint("tui.select.cancel", "to cancel")),
            padding_x: 1,
        });
    }

    /// Show a progress message (upstream `showProgress`).
    pub fn show_progress(&mut self, message: &str) {
        let text = theme().fg("dim", message);
        self.content
            .push(DialogContent::Text { text, padding_x: 1 });
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> LoginDialogOutcome {
        if with_global_keybindings(|kb| kb.matches(data, "tui.select.cancel")) {
            self.cancel();
            return LoginDialogOutcome::Cancel;
        }
        if with_global_keybindings(|kb| kb.matches(data, "tui.select.confirm")) {
            if !self.awaiting_input {
                return LoginDialogOutcome::Consumed;
            }
            let value = self.input.get_value().to_string();
            self.replace_input_with_submitted_text(&value);
            self.awaiting_input = false;
            return LoginDialogOutcome::Submit(value);
        }
        if !dispatch_input_keybinding(&mut self.input, data) {
            self.input.handle_input(data);
        }
        LoginDialogOutcome::Consumed
    }

    /// Upstream `cancel`: abort the flow, drop the pending prompt.
    fn cancel(&mut self) {
        self.signal.abort(Some(AbortReason::Aborted));
        self.awaiting_input = false;
    }

    /// Upstream `replaceInputWithSubmittedText`.
    fn replace_input_with_submitted_text(&mut self, value: &str) {
        let line = DialogContent::Text {
            text: format!("> {value}"),
            padding_x: 0,
        };
        if let Some(slot) = self
            .content
            .iter_mut()
            .find(|content| matches!(content, DialogContent::Input))
        {
            *slot = line;
        }
    }
}

/// OSC 8 hyperlink (upstream the inline `\x1b]8;;…\x07…\x1b]8;;\x07`).
fn osc8_link(url: &str, text: &str) -> String {
    format!("\u{1b}]8;;{url}\u{7}{text}\u{1b}]8;;\u{7}")
}

impl Component for LoginDialogComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(DynamicBorder::new().render(width));
        lines.extend(
            Text::new(
                &theme_handle.fg("accent", &theme_handle.bold(&self.title)),
                1,
                0,
            )
            .render(width),
        );
        let input_lines = self.input.render(width);
        for content in &self.content {
            match content {
                DialogContent::Blank => lines.push(String::new()),
                DialogContent::Text { text, padding_x } => {
                    lines.extend(Text::new(text, *padding_x, 0).render(width));
                }
                DialogContent::Input => lines.extend(input_lines.iter().cloned()),
            }
        }
        lines.extend(DynamicBorder::new().render(width));
        lines
    }
}

impl Focusable for LoginDialogComponent {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_round_trips_and_the_signal_aborts_on_cancel() {
        let _guard = crate::modes::interactive::components::test_support::setup();
        let mut dialog = LoginDialogComponent::new("anthropic", Some("Anthropic"), None);
        assert_eq!(dialog.provider_id(), "anthropic");
        assert!(!dialog.awaiting_input());
        // Enter is a no-op unless a prompt is pending (upstream
        // `input.onSubmit` does nothing without an `inputResolver`).
        assert_eq!(dialog.handle_key("\r"), LoginDialogOutcome::Consumed);

        dialog.show_prompt("Enter Anthropic API key", None);
        assert!(dialog.awaiting_input());

        for ch in "sk-ant".chars() {
            dialog.handle_key(&ch.to_string());
        }
        assert_eq!(
            dialog.handle_key("\r"),
            LoginDialogOutcome::Submit("sk-ant".to_string())
        );
        assert!(!dialog.awaiting_input());
        assert!(!dialog.signal().is_aborted());

        assert_eq!(dialog.handle_key("\x1b"), LoginDialogOutcome::Cancel);
        assert!(dialog.signal().is_aborted());
    }

    #[test]
    fn show_auth_replaces_the_content_and_show_info_appends_links() {
        let _guard = crate::modes::interactive::components::test_support::setup();
        let mut dialog = LoginDialogComponent::new("openai", None, None);
        assert_eq!(dialog.title, "Login to openai");
        // Never open a real browser from a test.
        dialog.set_url_opener(Box::new(|_url| {}));
        dialog.show_auth("https://example.test/auth", Some("Paste the code"));
        dialog.show_info(
            "Use a service account instead.",
            &[AuthInfoLink {
                url: "https://example.test/docs".to_string(),
                label: Some("Docs".to_string()),
            }],
            true,
        );
        let rendered = dialog.render(60).join("\n");
        let plain = pillar_tui::text_utils::strip_terminal_sequences(&rendered);
        assert!(plain.contains("Login to openai"), "{plain}");
        assert!(plain.contains("https://example.test/auth"), "{plain}");
        assert!(plain.contains("Paste the code"), "{plain}");
        assert!(plain.contains("Use a service account instead."), "{plain}");
        assert!(plain.contains("Docs: https://example.test/docs"), "{plain}");
        // The link itself is an OSC 8 hyperlink.
        assert!(
            rendered.contains("\u{1b}]8;;https://example.test/docs\u{7}"),
            "{rendered:?}"
        );
    }
}
