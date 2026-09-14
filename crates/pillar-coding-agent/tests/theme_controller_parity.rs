//! Parity tests for modes/interactive/theme/theme-controller.ts (pi v0.84.3):
//! `detectTerminalBackgroundTheme` / `detectTerminalThemeForAuto` and
//! `InteractiveThemeController`.
//!
//! The controller and the detectors are host-driven in the port, so these
//! tests use a fake host; the process-wide theme registry is serialized with a
//! mutex.

use std::sync::{Arc, Mutex};

use pillar_coding_agent::core::settings_manager::SettingsManager;
use pillar_coding_agent::modes::interactive::theme::controller::{
    InteractiveThemeController, InteractiveThemeControllerOptions, ThemeControllerHost,
    ThemeResult,
};
use pillar_coding_agent::modes::interactive::theme::{
    ColorMode, TerminalAutoThemeDetector, TerminalBackgroundThemeDetector,
    TerminalTheme, TerminalThemeConfidence, TerminalThemeDetection, TerminalThemeSource,
    create_theme, detect_terminal_background_theme, detect_terminal_theme_for_auto,
    get_theme_by_name, theme,
};
use pillar_tui::terminal_colors::{RgbColor, TerminalColorScheme};

static THEME_LOCK: Mutex<()> = Mutex::new(());

fn env(pairs: &[(&str, &str)]) -> pillar_coding_agent::utils::clipboard::ClipboardEnv {
    pillar_coding_agent::utils::clipboard::ClipboardEnv::new(
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    )
}

/// Fake terminal: answers the OSC 11 / color-scheme queries from fixed values.
#[derive(Default)]
struct FakeDetector {
    background: Option<RgbColor>,
    scheme: Option<TerminalTheme>,
    in_flight: std::cell::RefCell<Vec<&'static str>>,
}

impl TerminalBackgroundThemeDetector for FakeDetector {
    fn query_terminal_background_color(&mut self, _timeout: std::time::Duration) -> Option<RgbColor> {
        self.in_flight.borrow_mut().push("background");
        self.background
    }
}

impl TerminalAutoThemeDetector for FakeDetector {
    fn query_terminal_color_scheme(&mut self, _timeout: std::time::Duration) -> Option<TerminalTheme> {
        self.in_flight.borrow_mut().push("scheme");
        self.scheme
    }
}

// --- detection ------------------------------------------------------------------------------------

#[test]
fn background_detection_prefers_the_osc11_reply() {
    let mut dark_reply = FakeDetector {
        background: Some(RgbColor {
            r: 0,
            g: 0,
            b: 0,
        }),
        ..Default::default()
    };
    let detection = detect_terminal_background_theme(&mut dark_reply, 100, None);
    assert_eq!(detection.theme, TerminalTheme::Dark);
    assert_eq!(detection.source, TerminalThemeSource::TerminalBackground);
    assert_eq!(detection.confidence, TerminalThemeConfidence::High);
    assert_eq!(detection.detail, "OSC 11 background rgb(0, 0, 0)");

    let mut light_reply = FakeDetector {
        background: Some(RgbColor {
            r: 250,
            g: 250,
            b: 250,
        }),
        ..Default::default()
    };
    assert_eq!(
        detect_terminal_background_theme(&mut light_reply, 100, None).theme,
        TerminalTheme::Light
    );
}

#[test]
fn background_detection_falls_back_to_the_environment() {
    // No OSC 11 reply: COLORFGBG decides, else dark with low confidence.
    let mut silent = FakeDetector::default();
    let detection =
        detect_terminal_background_theme(&mut silent, 100, Some(&env(&[("COLORFGBG", "0;15")])));
    assert_eq!(detection.theme, TerminalTheme::Light);
    assert_eq!(detection.source, TerminalThemeSource::ColorFgBg);
    assert_eq!(detection.confidence, TerminalThemeConfidence::High);

    let detection = detect_terminal_background_theme(&mut silent, 100, Some(&env(&[])));
    assert_eq!(detection.theme, TerminalTheme::Dark);
    assert_eq!(detection.source, TerminalThemeSource::Fallback);
    assert_eq!(detection.confidence, TerminalThemeConfidence::Low);
}

#[test]
fn auto_detection_asks_the_color_scheme_first() {
    let mut host = FakeDetector {
        background: Some(RgbColor {
            r: 0,
            g: 0,
            b: 0,
        }),
        scheme: Some(TerminalTheme::Light),
        ..Default::default()
    };
    // The color-scheme report wins over the (dark) OSC 11 background.
    assert_eq!(
        detect_terminal_theme_for_auto(&mut host, 100, None),
        TerminalTheme::Light
    );
    assert_eq!(
        host.in_flight.borrow().as_slice(),
        ["scheme".to_string()],
        "the background query is skipped"
    );

    // Without a report it falls back to the OSC 11 / environment detection.
    let mut host = FakeDetector {
        background: Some(RgbColor {
            r: 250,
            g: 250,
            b: 250,
        }),
        scheme: None,
        ..Default::default()
    };
    assert_eq!(
        detect_terminal_theme_for_auto(&mut host, 100, None),
        TerminalTheme::Light
    );
    assert_eq!(
        host.in_flight.borrow().as_slice(),
        ["scheme".to_string(), "background".to_string()]
    );
}

// --- controller -----------------------------------------------------------------------------------

/// A color-scheme listener registered on the fake host.
type SchemeListener = Box<dyn FnMut(TerminalColorScheme) + Send>;

#[derive(Default)]
struct FakeHost {
    detector: FakeDetector,
    invalidations: usize,
    renders: usize,
    notifications: Vec<bool>,
    listeners: Vec<(u64, SchemeListener)>,
    removed: Vec<u64>,
}

impl TerminalBackgroundThemeDetector for FakeHost {
    fn query_terminal_background_color(&mut self, timeout: std::time::Duration) -> Option<RgbColor> {
        self.detector.query_terminal_background_color(timeout)
    }
}

impl TerminalAutoThemeDetector for FakeHost {
    fn query_terminal_color_scheme(&mut self, timeout: std::time::Duration) -> Option<TerminalTheme> {
        self.detector.query_terminal_color_scheme(timeout)
    }
}

impl ThemeControllerHost for FakeHost {
    fn invalidate(&mut self) {
        self.invalidations += 1;
    }
    fn request_render(&mut self) {
        self.renders += 1;
    }
    fn set_terminal_color_scheme_notifications(&mut self, enabled: bool) {
        self.notifications.push(enabled);
    }
    fn on_terminal_color_scheme_change(&mut self, listener: SchemeListener) -> u64 {
        let id = self.listeners.len() as u64 + 1;
        self.listeners.push((id, listener));
        id
    }
    fn remove_terminal_color_scheme_listener(&mut self, id: u64) {
        self.removed.push(id);
        self.listeners.retain(|(candidate, _)| *candidate != id);
    }
}

impl FakeHost {
    fn report_scheme(&mut self, scheme: TerminalColorScheme) {
        for (_, listener) in self.listeners.iter_mut() {
            listener(scheme);
        }
    }
}

fn settings(theme: Option<&str>) -> Arc<Mutex<SettingsManager>> {
    let mut value = serde_json::json!({});
    if let Some(theme) = theme {
        value["theme"] = serde_json::Value::String(theme.to_string());
    }
    Arc::new(Mutex::new(SettingsManager::in_memory(
        value,
        Default::default(),
    )))
}

fn controller(
    host: &mut FakeHost,
    settings: Arc<Mutex<SettingsManager>>,
    initial: Option<&str>,
) -> InteractiveThemeController {
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let error_sink = Arc::clone(&errors);
    let change_sink = Arc::clone(&changes);
    InteractiveThemeController::new(
        host,
        settings,
        InteractiveThemeControllerOptions {
            show_error: Box::new(move |message: &str| {
                error_sink.lock().unwrap().push(message.to_string());
            }),
            on_changed: Box::new(move || {
                change_sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
            initial_theme_setting: initial.map(str::to_string),
        },
    )
}

#[test]
fn controller_initializes_the_theme_and_binds_the_listener() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let mut host = FakeHost::default();
    let settings = settings(Some("light"));
    let controller = controller(&mut host, Arc::clone(&settings), None);

    // The theme came from the settings and the listener is registered.
    assert_eq!(controller.get_theme_selection().as_deref(), Some("light"));
    assert_eq!(theme().name(), Some("light"));
    assert_eq!(host.listeners.len(), 1);
    assert!(host.notifications.is_empty(), "auto-sync starts disabled");
}

#[test]
fn controller_applies_plain_and_automatic_theme_settings() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let mut host = FakeHost::default();
    let settings = settings(Some("dark"));
    let mut controller = controller(&mut host, Arc::clone(&settings), None);
    host.invalidations = 0;

    // A plain theme name applies directly and does not sync with the terminal.
    controller.apply_from_settings(&mut host);
    assert_eq!(theme().name(), Some("dark"));
    assert!(!controller.auto_sync_enabled());
    assert_eq!(host.invalidations, 1, "notifyChanged invalidates once");

    // `light/dark` pairs turn auto-sync on and pick by terminal theme.
    settings.lock().unwrap().set_theme("light/dark");
    host.detector.scheme = Some(TerminalTheme::Light);
    controller.set_theme_setting(&mut host, "light/dark");
    assert_eq!(theme().name(), Some("light"));
    assert!(controller.auto_sync_enabled());
    assert_eq!(host.notifications, vec![true]);
    assert_eq!(controller.get_terminal_theme(), TerminalTheme::Light);

    // A later scheme report switches the theme through the pending pump.
    host.report_scheme(TerminalColorScheme::Dark);
    assert_eq!(theme().name(), Some("light"), "not applied before the pump");
    controller.pump(&mut host);
    assert_eq!(theme().name(), Some("dark"));
    assert_eq!(controller.get_terminal_theme(), TerminalTheme::Dark);
}

#[test]
fn controller_prefers_the_terminal_background_when_no_setting_exists() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let mut host = FakeHost {
        detector: FakeDetector {
            background: Some(RgbColor {
                r: 250,
                g: 250,
                b: 250,
            }),
            scheme: None,
            ..Default::default()
        },
        ..Default::default()
    };
    let settings = settings(None);
    let mut controller = controller(&mut host, Arc::clone(&settings), None);
    controller.apply_from_settings(&mut host);

    // A high-confidence detection is persisted to the settings.
    assert_eq!(theme().name(), Some("light"));
    assert_eq!(settings.lock().unwrap().theme_setting().as_deref(), Some("light"));
    assert!(!controller.auto_sync_enabled());
}

#[test]
fn controller_falls_back_to_dark_and_reports_failures() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let mut host = FakeHost::default();
    let settings = settings(Some("dark"));
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&errors);
    let mut controller = InteractiveThemeController::new(
        &mut host,
        Arc::clone(&settings),
        InteractiveThemeControllerOptions {
            show_error: Box::new(move |message: &str| sink.lock().unwrap().push(message.to_string())),
            on_changed: Box::new(|| {}),
            initial_theme_setting: None,
        },
    );

    let result: ThemeResult = controller.set_theme_name(&mut host, "does-not-exist", true);
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap_or("").starts_with("Theme not found:"));
    assert_eq!(theme().name(), Some("dark"), "fell back to dark");
    assert_eq!(controller.get_theme_selection().as_deref(), Some("dark"));
    let errors = errors.lock().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("Failed to load theme \"does-not-exist\""), "{errors:?}");
    assert!(errors[0].contains("Fell back to dark theme."), "{errors:?}");

    // A successful switch records the new name as the choice.
    let result = controller.set_theme_name(&mut host, "light", true);
    assert!(result.success);
    assert_eq!(controller.get_theme_selection().as_deref(), Some("light"));

    // Previewing does not change the recorded choice.
    controller.preview(&mut host, "dark");
    assert_eq!(theme().name(), Some("dark"));
    assert_eq!(controller.get_theme_selection().as_deref(), Some("light"));
}

#[test]
fn controller_installs_an_in_memory_theme_and_disables_auto_sync() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let mut host = FakeHost::default();
    let settings = settings(Some("light/dark"));
    let mut controller = controller(&mut host, Arc::clone(&settings), Some("light/dark"));
    controller.apply_from_settings(&mut host);
    assert!(controller.auto_sync_enabled());

    let sample = pillar_coding_agent::modes::interactive::theme::ThemeJson::parse(
        "sample",
        &serde_json::json!({
            "name": "sample",
            "colors": required_colors(),
        }),
    )
    .expect("sample theme");
    let instance = create_theme(&sample, ColorMode::Truecolor, None);
    let result = controller.set_theme_instance(&mut host, Arc::new(instance));
    assert!(result.success);
    assert!(!controller.auto_sync_enabled());
    assert_eq!(host.notifications, vec![true, false]);
    // `getThemeSelection` still reports the remembered setting; the active
    // theme is the installed instance.
    assert_eq!(controller.get_theme_selection().as_deref(), Some("light/dark"));
    assert_eq!(
        pillar_coding_agent::modes::interactive::theme::current_theme_name().as_deref(),
        Some("<in-memory>")
    );

    // Rebinding moves the listener to the new TUI surface.
    let mut rebound = FakeHost::default();
    controller.rebind_tui(&mut host, &mut rebound);
    assert_eq!(host.removed.len(), 1);
    assert_eq!(rebound.listeners.len(), 1);
    assert_eq!(rebound.notifications, vec![false], "auto-sync is off");
}

#[test]
fn controller_ignores_scheme_reports_while_auto_sync_is_off() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let mut host = FakeHost::default();
    let settings = settings(Some("light/dark"));
    let mut controller = controller(&mut host, Arc::clone(&settings), None);
    assert!(!controller.auto_sync_enabled());

    host.report_scheme(TerminalColorScheme::Light);
    controller.pump(&mut host);
    assert_eq!(theme().name(), Some(get_theme_by_name("dark").unwrap().name().unwrap()));
    assert_eq!(controller.get_terminal_theme(), TerminalTheme::Dark);

    // Disabling auto-sync stops applying reports.
    controller.set_theme_setting(&mut host, "light/dark");
    assert!(controller.auto_sync_enabled());
    controller.disable_auto_sync(&mut host);
    assert!(!controller.auto_sync_enabled());
    assert_eq!(host.notifications, vec![true, false]);
}

fn required_colors() -> serde_json::Map<String, serde_json::Value> {
    use pillar_coding_agent::modes::interactive::theme::{REQUIRED_BG_COLORS, REQUIRED_FG_COLORS};
    REQUIRED_FG_COLORS
        .iter()
        .chain(REQUIRED_BG_COLORS.iter())
        .map(|key| {
            (
                (*key).to_string(),
                serde_json::Value::String("#112233".to_string()),
            )
        })
        .collect()
}

#[test]
fn background_detection_uses_the_process_environment_by_default() {
    // The `env: None` path reads the process environment (COLORFGBG is absent
    // in the test runner, so this is the low-confidence dark fallback).
    let mut host = FakeDetector::default();
    let detection: TerminalThemeDetection = detect_terminal_background_theme(&mut host, 100, None);
    assert!(matches!(
        detection.source,
        TerminalThemeSource::ColorFgBg | TerminalThemeSource::Fallback
    ));
}
