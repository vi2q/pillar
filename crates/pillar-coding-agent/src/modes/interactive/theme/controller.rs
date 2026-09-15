//! Port of packages/coding-agent/src/modes/interactive/theme/theme-controller.ts
//! (pi v0.84.3): [`InteractiveThemeController`] keeps the active theme in sync
//! with the settings, the terminal's light/dark reports, and the automatic
//! (`light/dark`) theme setting.
//!
//! divergences:
//! - upstream holds the `TUI` and calls `invalidate` / `requestRender` /
//!   `setTerminalColorSchemeNotifications` / `onTerminalColorSchemeChange` on
//!   it; Rust cannot hold a `&mut` TUI inside the controller, so the TUI is
//!   passed in per call as a [`ThemeControllerHost`].
//! - upstream's `applyFromSettings` is async (awaits the terminal queries);
//!   the port's queries are blocking, so it is synchronous.
//! - upstream's listener callback calls straight back into the controller;
//!   Rust cannot re-enter the controller from a `'static` listener, so the
//!   listener records the report and the host pumps it with
//!   [`InteractiveThemeController::pump`] (the port's usual host-driven
//!   pattern).
//! - `SettingsManager.flush()` does not exist: the port persists settings
//!   synchronously, so there is no write queue to await.

use std::sync::{Arc, Mutex};

use pillar_tui::terminal_colors::TerminalColorScheme;
use pillar_tui::tui::TuiBase;

use crate::core::settings_manager::SettingsManager;

use super::{
    TerminalAutoThemeDetector, TerminalTheme, detect_terminal_theme_for_auto, init_theme,
    parse_auto_theme_setting, resolve_theme_setting, set_theme, set_theme_instance,
};

/// The outcome of a theme change (upstream the `ThemeResult` object).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeResult {
    pub success: bool,
    pub error: Option<String>,
}

impl ThemeResult {
    fn ok() -> Self {
        Self {
            success: true,
            error: None,
        }
    }

    fn failed(error: String) -> Self {
        Self {
            success: false,
            error: Some(error),
        }
    }
}

/// The terminal query timeout the controller uses (upstream's inline
/// `timeoutMs: 100`).
const QUERY_TIMEOUT_MS: u64 = 100;

/// The TUI operations the controller drives (upstream the `TUI` instance).
pub trait ThemeControllerHost: TerminalAutoThemeDetector {
    /// Drop cached render state (upstream `invalidate`).
    fn invalidate(&mut self);
    /// Schedule a render (upstream `requestRender`).
    fn request_render(&mut self);
    /// Enable or disable terminal color-scheme reports (upstream
    /// `setTerminalColorSchemeNotifications`).
    fn set_terminal_color_scheme_notifications(&mut self, enabled: bool);
    /// Register a color-scheme listener (upstream
    /// `onTerminalColorSchemeChange`), returning its id.
    fn on_terminal_color_scheme_change(
        &mut self,
        listener: Box<dyn FnMut(TerminalColorScheme) + Send>,
    ) -> u64;
    /// Remove a color-scheme listener (the unsubscribe callback upstream).
    fn remove_terminal_color_scheme_listener(&mut self, id: u64);
}

impl ThemeControllerHost for TuiBase {
    fn invalidate(&mut self) {
        TuiBase::invalidate(self);
    }

    fn request_render(&mut self) {
        TuiBase::request_render(self, false);
    }

    fn set_terminal_color_scheme_notifications(&mut self, enabled: bool) {
        TuiBase::set_terminal_color_scheme_notifications(self, enabled);
    }

    fn on_terminal_color_scheme_change(
        &mut self,
        listener: Box<dyn FnMut(TerminalColorScheme) + Send>,
    ) -> u64 {
        TuiBase::on_terminal_color_scheme_change(self, listener)
    }

    fn remove_terminal_color_scheme_listener(&mut self, id: u64) {
        TuiBase::remove_terminal_color_scheme_listener(self, id);
    }
}

/// Options for [`InteractiveThemeController::new`] (upstream the constructor's
/// options object).
pub struct InteractiveThemeControllerOptions {
    /// Report a failed theme load to the user.
    pub show_error: Box<dyn FnMut(&str) + Send>,
    /// Called after the active theme changed.
    pub on_changed: Box<dyn FnMut() + Send>,
    /// Theme setting captured at construction (upstream
    /// `initialThemeSetting`), overriding the settings value.
    pub initial_theme_setting: Option<String>,
}

/// Keeps the active theme aligned with the settings and the terminal
/// (upstream `InteractiveThemeController`).
pub struct InteractiveThemeController {
    settings: Arc<Mutex<SettingsManager>>,
    show_error: Box<dyn FnMut(&str) + Send>,
    on_changed: Box<dyn FnMut() + Send>,
    current_theme_setting: Option<String>,
    terminal_theme: TerminalTheme,
    active_theme_name: Option<String>,
    auto_sync_enabled: bool,
    color_scheme_listener: Option<u64>,
    pending_terminal_theme: Arc<Mutex<Option<TerminalTheme>>>,
}

impl InteractiveThemeController {
    /// Build the controller, initialize the theme and bind the terminal
    /// color-scheme listener (upstream the constructor).
    pub fn new(
        host: &mut dyn ThemeControllerHost,
        settings: Arc<Mutex<SettingsManager>>,
        options: InteractiveThemeControllerOptions,
    ) -> Self {
        let terminal_theme = super::detect_terminal_background_from_env(
            &crate::utils::clipboard::ClipboardEnv::from_process(),
        )
        .theme;
        let current_theme_setting = options.initial_theme_setting;
        let settings_theme = settings.lock().expect("settings lock").theme_setting();
        let active_theme_name = resolve_theme_setting(
            current_theme_setting
                .as_deref()
                .or(settings_theme.as_deref()),
            terminal_theme,
        );
        // Upstream calls `initTheme(this.activeThemeName, true)`
        // unconditionally: an unresolved name (an automatic `light/dark` pair,
        // or settings that never loaded) still initializes the *default*
        // theme. Skipping it leaves the registry empty and the first `theme()`
        // call panics before the first render.
        init_theme(active_theme_name.as_deref());

        let mut controller = Self {
            settings,
            show_error: options.show_error,
            on_changed: options.on_changed,
            current_theme_setting,
            terminal_theme,
            active_theme_name,
            auto_sync_enabled: false,
            color_scheme_listener: None,
            pending_terminal_theme: Arc::new(Mutex::new(None)),
        };
        controller.bind_terminal_color_scheme_listener(host);
        controller
    }

    /// Re-bind the listener after the TUI was replaced (upstream
    /// `rebindTui`).
    ///
    /// `previous` is the host the current listener was registered on (upstream
    /// keeps its unsubscribe handle); `host` is the replacement.
    pub fn rebind_tui(
        &mut self,
        previous: &mut dyn ThemeControllerHost,
        host: &mut dyn ThemeControllerHost,
    ) {
        self.unbind_terminal_color_scheme_listener(previous);
        self.bind_terminal_color_scheme_listener(host);
        host.set_terminal_color_scheme_notifications(self.auto_sync_enabled);
    }

    /// Apply the theme described by the settings (upstream
    /// `applyFromSettings`).
    pub fn apply_from_settings(&mut self, host: &mut dyn ThemeControllerHost) {
        let settings_theme = self.settings.lock().expect("settings lock").theme_setting();
        let theme_setting = self.current_theme_setting.clone().or(settings_theme);

        if let Some(auto) = theme_setting
            .as_deref()
            .and_then(|setting| parse_auto_theme_setting(Some(setting)))
        {
            self.terminal_theme = detect_terminal_theme_for_auto(host, QUERY_TIMEOUT_MS, None);
            self.set_auto_sync(host, true);
            let name = if self.terminal_theme == TerminalTheme::Light {
                auto.light_theme
            } else {
                auto.dark_theme
            };
            self.apply_theme_name(host, &name, true);
            return;
        }

        self.set_auto_sync(host, false);
        // An empty setting is still a setting (upstream applies it and falls
        // back to dark); only an absent one triggers detection.
        if let Some(setting) = theme_setting.as_deref() {
            self.apply_theme_name(host, setting, true);
            return;
        }

        let detection = super::detect_terminal_background_theme(host, QUERY_TIMEOUT_MS, None);
        self.terminal_theme = detection.theme;
        if !self
            .apply_theme_name(host, detection.theme.as_str(), false)
            .success
        {
            return;
        }
        if detection.confidence == super::TerminalThemeConfidence::High {
            self.settings
                .lock()
                .expect("settings lock")
                .set_theme(detection.theme.as_str());
        }
    }

    /// The theme the settings picker should show (upstream
    /// `getThemeSelection`).
    pub fn get_theme_selection(&self) -> Option<String> {
        self.current_theme_setting
            .clone()
            .or_else(|| self.settings.lock().expect("settings lock").theme_setting())
            .or_else(|| self.active_theme_name.clone())
    }

    /// Switch to a theme by name and remember the choice (upstream
    /// `setThemeName`).
    pub fn set_theme_name(
        &mut self,
        host: &mut dyn ThemeControllerHost,
        theme_name: &str,
        show_error: bool,
    ) -> ThemeResult {
        self.set_auto_sync(host, false);
        let result = self.apply_theme_name(host, theme_name, show_error);
        if result.success {
            self.current_theme_setting = Some(theme_name.to_string());
        }
        result
    }

    /// Adopt a theme setting (including `light/dark` pairs) and apply it
    /// (upstream `setThemeSetting`).
    pub fn set_theme_setting(&mut self, host: &mut dyn ThemeControllerHost, theme_setting: &str) {
        self.current_theme_setting = Some(theme_setting.to_string());
        self.apply_from_settings(host);
    }

    /// Install a programmatic theme instance (upstream `setThemeInstance`).
    pub fn set_theme_instance(
        &mut self,
        host: &mut dyn ThemeControllerHost,
        theme_instance: Arc<super::Theme>,
    ) -> ThemeResult {
        self.set_auto_sync(host, false);
        set_theme_instance(theme_instance);
        self.active_theme_name = Some("<in-memory>".to_string());
        self.notify_changed(host);
        ThemeResult::ok()
    }

    /// Preview a theme setting without persisting it (upstream `preview`).
    pub fn preview(&mut self, host: &mut dyn ThemeControllerHost, theme_setting_or_name: &str) {
        let theme_name = resolve_theme_setting(Some(theme_setting_or_name), self.terminal_theme)
            .or_else(|| self.active_theme_name.clone());
        let Some(theme_name) = theme_name else {
            return;
        };
        if set_theme(&theme_name).is_ok() {
            host.invalidate();
            host.request_render();
        }
    }

    /// Stop following the terminal's color scheme (upstream
    /// `disableAutoSync`).
    pub fn disable_auto_sync(&mut self, host: &mut dyn ThemeControllerHost) {
        self.set_auto_sync(host, false);
    }

    /// The terminal theme last detected (upstream `getTerminalTheme`).
    pub fn get_terminal_theme(&self) -> TerminalTheme {
        self.terminal_theme
    }

    /// Whether color-scheme auto-sync is currently enabled.
    pub fn auto_sync_enabled(&self) -> bool {
        self.auto_sync_enabled
    }

    /// Apply a color-scheme report the listener recorded (upstream the
    /// listener callback body; the host pumps it).
    pub fn pump(&mut self, host: &mut dyn ThemeControllerHost) {
        let pending = self
            .pending_terminal_theme
            .lock()
            .expect("pending theme lock")
            .take();
        if let Some(terminal_theme) = pending {
            self.apply_terminal_theme(host, terminal_theme);
        }
    }

    /// Apply a terminal color-scheme report (upstream `applyTerminalTheme`).
    pub fn apply_terminal_theme(
        &mut self,
        host: &mut dyn ThemeControllerHost,
        terminal_theme: TerminalTheme,
    ) {
        if !self.auto_sync_enabled {
            return;
        }
        self.terminal_theme = terminal_theme;
        let settings_theme = self.settings.lock().expect("settings lock").theme_setting();
        let auto_theme = parse_auto_theme_setting(
            self.current_theme_setting
                .as_deref()
                .or(settings_theme.as_deref()),
        );
        let Some(auto_theme) = auto_theme else {
            self.set_auto_sync(host, false);
            return;
        };
        let theme_name = if terminal_theme == TerminalTheme::Light {
            auto_theme.light_theme
        } else {
            auto_theme.dark_theme
        };
        if self.active_theme_name.as_deref() != Some(theme_name.as_str()) {
            self.apply_theme_name(host, &theme_name, false);
        }
    }

    fn apply_theme_name(
        &mut self,
        host: &mut dyn ThemeControllerHost,
        theme_name: &str,
        show_error: bool,
    ) -> ThemeResult {
        let result = match set_theme(theme_name) {
            Ok(()) => ThemeResult::ok(),
            Err(error) => ThemeResult::failed(error),
        };
        self.active_theme_name = Some(if result.success {
            theme_name.to_string()
        } else {
            "dark".to_string()
        });
        self.notify_changed(host);
        if !result.success && show_error {
            let error = result.error.clone().unwrap_or_default();
            (self.show_error)(&format!(
                "Failed to load theme \"{theme_name}\": {error}\nFell back to dark theme."
            ));
        }
        result
    }

    fn notify_changed(&mut self, host: &mut dyn ThemeControllerHost) {
        host.invalidate();
        (self.on_changed)();
    }

    fn set_auto_sync(&mut self, host: &mut dyn ThemeControllerHost, enabled: bool) {
        if self.auto_sync_enabled == enabled {
            return;
        }
        self.auto_sync_enabled = enabled;
        host.set_terminal_color_scheme_notifications(enabled);
    }

    fn bind_terminal_color_scheme_listener(&mut self, host: &mut dyn ThemeControllerHost) {
        let pending = Arc::clone(&self.pending_terminal_theme);
        let listener = Box::new(move |scheme: TerminalColorScheme| {
            let theme = match scheme {
                TerminalColorScheme::Light => TerminalTheme::Light,
                TerminalColorScheme::Dark => TerminalTheme::Dark,
            };
            if let Ok(mut slot) = pending.lock() {
                *slot = Some(theme);
            }
        });
        self.color_scheme_listener = Some(host.on_terminal_color_scheme_change(listener));
    }

    fn unbind_terminal_color_scheme_listener(&mut self, host: &mut dyn ThemeControllerHost) {
        if let Some(id) = self.color_scheme_listener.take() {
            host.remove_terminal_color_scheme_listener(id);
        }
    }
}
