//! Port of packages/coding-agent/src/core/keybindings.ts (pi v0.84.3).
//!
//! App-level keybinding definitions layered on the TUI registry, Windows/WSL
//! default detection, legacy name migration, and keybindings.json loading.
//!
//! divergence: the editor-history precedence behavior (upstream
//! custom-editor-history-keybindings.test.ts) is exercised by the TUI
//! manager's per-binding override semantics — user-bound
//! `tui.editor.historyPrevious` replaces that binding's defaults while
//! `app.model.cycleForward` keeps its own; precedence resolution between the
//! two belongs to the editor component (packages/tui), not the registry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pillar_tui::keybindings::{
    KeybindingDefinition, KeybindingsConfig, KeybindingsManager as TuiKeybindingsManager,
    tui_keybindings,
};

/// App keybinding ids (upstream `AppKeybindings` interface keys).
pub const APP_KEYBINDINGS: &[&str] = &[
    "app.interrupt",
    "app.clear",
    "app.exit",
    "app.suspend",
    "app.thinking.cycle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.thinking.toggle",
    "app.session.toggleNamedFilter",
    "app.editor.external",
    "app.message.copy",
    "app.message.followUp",
    "app.message.dequeue",
    "app.clipboard.pasteImage",
    "app.session.new",
    "app.session.tree",
    "app.session.fork",
    "app.session.resume",
    "app.tree.foldOrUp",
    "app.tree.unfoldOrDown",
    "app.tree.editLabel",
    "app.tree.toggleLabelTimestamp",
    "app.session.togglePath",
    "app.session.toggleSort",
    "app.session.rename",
    "app.session.delete",
    "app.session.deleteNoninvasive",
    "app.models.save",
    "app.models.enableAll",
    "app.models.clearAll",
    "app.models.toggleProvider",
    "app.models.reorderUp",
    "app.models.reorderDown",
    "app.tree.filter.default",
    "app.tree.filter.noTools",
    "app.tree.filter.userOnly",
    "app.tree.filter.labeledOnly",
    "app.tree.filter.all",
    "app.tree.filter.cycleForward",
    "app.tree.filter.cycleBackward",
];

/// Upstream `useWindowsKeybindings`: native Windows, or WSL on Linux.
pub fn use_windows_keybindings(platform: &str, env: &BTreeMap<String, String>) -> bool {
    if platform == "win32" {
        return true;
    }
    if platform == "linux" {
        return env.contains_key("WSL_DISTRO_NAME") || env.contains_key("WSL_INTEROP");
    }
    false
}

fn keys(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

/// Upstream `KEYBINDINGS`: TUI definitions plus app overrides/extends.
pub fn keybindings(
    platform: &str,
    env: &BTreeMap<String, String>,
) -> BTreeMap<&'static str, KeybindingDefinition> {
    let windows = use_windows_keybindings(platform, env);
    let native_windows = platform == "win32";
    let darwin = platform == "darwin";

    let mut definitions = tui_keybindings();

    // Platform-adjusted TUI defaults.
    definitions.insert(
        "tui.editor.undo",
        KeybindingDefinition {
            default_keys: if native_windows {
                keys(&["ctrl+z"])
            } else if windows {
                keys(&["alt+z"])
            } else {
                keys(&["ctrl+-"])
            },
            description: Some("Undo".to_string()),
        },
    );
    definitions.insert(
        "tui.altScreen.previousPrompt",
        KeybindingDefinition {
            default_keys: if windows {
                keys(&["ctrl+up"])
            } else {
                keys(&["ctrl+shift+up", "ctrl+up"])
            },
            description: Some("Jump to previous semantic prompt".to_string()),
        },
    );
    definitions.insert(
        "tui.altScreen.nextPrompt",
        KeybindingDefinition {
            default_keys: if windows {
                keys(&["ctrl+down"])
            } else {
                keys(&["ctrl+shift+down", "ctrl+down"])
            },
            description: Some("Jump to next semantic prompt".to_string()),
        },
    );
    definitions.insert(
        "tui.altScreen.search",
        KeybindingDefinition {
            default_keys: if windows {
                keys(&["ctrl+f"])
            } else {
                keys(&["ctrl+shift+f"])
            },
            description: Some("Search the primary scroll view".to_string()),
        },
    );

    let mut def = |id: &'static str, default_keys: Vec<String>, description: &'static str| {
        definitions.insert(
            id,
            KeybindingDefinition {
                default_keys,
                description: Some(description.to_string()),
            },
        );
    };

    def("app.interrupt", keys(&["escape"]), "Cancel or abort");
    def("app.clear", keys(&["ctrl+c"]), "Clear editor");
    def("app.exit", keys(&["ctrl+d"]), "Exit when editor is empty");
    def(
        "app.suspend",
        if native_windows {
            Vec::new()
        } else {
            keys(&["ctrl+z"])
        },
        "Suspend to background",
    );
    def(
        "app.thinking.cycle",
        keys(&["shift+tab"]),
        "Cycle thinking level",
    );
    def(
        "app.model.cycleForward",
        keys(&["ctrl+p"]),
        "Cycle to next model",
    );
    def(
        "app.model.cycleBackward",
        if windows {
            keys(&["alt+p"])
        } else {
            keys(&["shift+ctrl+p"])
        },
        "Cycle to previous model",
    );
    def("app.model.select", keys(&["ctrl+l"]), "Open model selector");
    def("app.tools.expand", keys(&["ctrl+o"]), "Toggle tool output");
    def(
        "app.thinking.toggle",
        keys(&["ctrl+t"]),
        "Toggle thinking blocks",
    );
    def(
        "app.session.toggleNamedFilter",
        keys(&["ctrl+n"]),
        "Toggle named session filter",
    );
    def(
        "app.editor.external",
        keys(&["ctrl+g"]),
        "Open external editor",
    );
    def(
        "app.message.copy",
        keys(&["ctrl+x"]),
        "Copy message to clipboard",
    );
    def(
        "app.message.followUp",
        if windows {
            keys(&["ctrl+q"])
        } else {
            keys(&["alt+enter"])
        },
        "Queue follow-up message",
    );
    def(
        "app.message.dequeue",
        if windows {
            keys(&["alt+q"])
        } else {
            keys(&["alt+up"])
        },
        "Restore queued messages",
    );
    def(
        "app.clipboard.pasteImage",
        if windows {
            keys(&["alt+v"])
        } else {
            keys(&["ctrl+v"])
        },
        "Paste image from clipboard (text fallback)",
    );
    def("app.session.new", Vec::new(), "Start a new session");
    def("app.session.tree", Vec::new(), "Open session tree");
    def("app.session.fork", Vec::new(), "Fork current session");
    def("app.session.resume", Vec::new(), "Resume a session");
    def(
        "app.tree.foldOrUp",
        if darwin {
            keys(&["alt+left", "ctrl+left"])
        } else {
            keys(&["ctrl+left", "alt+left"])
        },
        "Fold tree branch or move up",
    );
    def(
        "app.tree.unfoldOrDown",
        if darwin {
            keys(&["alt+right", "ctrl+right"])
        } else {
            keys(&["ctrl+right", "alt+right"])
        },
        "Unfold tree branch or move down",
    );
    def("app.tree.editLabel", keys(&["shift+l"]), "Edit tree label");
    def(
        "app.tree.toggleLabelTimestamp",
        keys(&["shift+t"]),
        "Toggle tree label timestamps",
    );
    def(
        "app.session.togglePath",
        keys(&["ctrl+p"]),
        "Toggle session path display",
    );
    def(
        "app.session.toggleSort",
        keys(&["ctrl+s"]),
        "Toggle session sort mode",
    );
    def("app.session.rename", keys(&["ctrl+r"]), "Rename session");
    def("app.session.delete", keys(&["ctrl+d"]), "Delete session");
    def(
        "app.session.deleteNoninvasive",
        keys(&["ctrl+backspace"]),
        "Delete session when query is empty",
    );
    def("app.models.save", keys(&["ctrl+s"]), "Save model selection");
    def(
        "app.models.enableAll",
        keys(&["ctrl+a"]),
        "Enable all models",
    );
    def("app.models.clearAll", keys(&["ctrl+x"]), "Clear all models");
    def(
        "app.models.toggleProvider",
        keys(&["ctrl+p"]),
        "Toggle all models for provider",
    );
    def(
        "app.models.reorderUp",
        keys(&["alt+up"]),
        "Move model up in order",
    );
    def(
        "app.models.reorderDown",
        keys(&["alt+down"]),
        "Move model down in order",
    );
    def(
        "app.tree.filter.default",
        keys(&["ctrl+d"]),
        "Tree filter: default view",
    );
    def(
        "app.tree.filter.noTools",
        keys(&["ctrl+t"]),
        "Tree filter: hide tool results",
    );
    def(
        "app.tree.filter.userOnly",
        keys(&["ctrl+u"]),
        "Tree filter: user messages only",
    );
    def(
        "app.tree.filter.labeledOnly",
        keys(&["ctrl+l"]),
        "Tree filter: labeled entries only",
    );
    def(
        "app.tree.filter.all",
        keys(&["ctrl+a"]),
        "Tree filter: show all entries",
    );
    def(
        "app.tree.filter.cycleForward",
        keys(&["ctrl+o"]),
        "Tree filter: cycle forward",
    );
    def(
        "app.tree.filter.cycleBackward",
        keys(&["shift+ctrl+o"]),
        "Tree filter: cycle backward",
    );

    definitions
}

/// Upstream `KEYBINDING_NAME_MIGRATIONS`: legacy names -> namespaced ids.
pub fn keybinding_name_migrations() -> &'static BTreeMap<&'static str, &'static str> {
    static MIGRATIONS: std::sync::OnceLock<BTreeMap<&'static str, &'static str>> =
        std::sync::OnceLock::new();
    MIGRATIONS.get_or_init(|| {
        BTreeMap::from([
            ("cursorUp", "tui.editor.cursorUp"),
            ("cursorDown", "tui.editor.cursorDown"),
            ("cursorLeft", "tui.editor.cursorLeft"),
            ("cursorRight", "tui.editor.cursorRight"),
            ("cursorWordLeft", "tui.editor.cursorWordLeft"),
            ("cursorWordRight", "tui.editor.cursorWordRight"),
            ("cursorLineStart", "tui.editor.cursorLineStart"),
            ("cursorLineEnd", "tui.editor.cursorLineEnd"),
            ("jumpForward", "tui.editor.jumpForward"),
            ("jumpBackward", "tui.editor.jumpBackward"),
            ("pageUp", "tui.editor.pageUp"),
            ("pageDown", "tui.editor.pageDown"),
            ("deleteCharBackward", "tui.editor.deleteCharBackward"),
            ("deleteCharForward", "tui.editor.deleteCharForward"),
            ("deleteWordBackward", "tui.editor.deleteWordBackward"),
            ("deleteWordForward", "tui.editor.deleteWordForward"),
            ("deleteToLineStart", "tui.editor.deleteToLineStart"),
            ("deleteToLineEnd", "tui.editor.deleteToLineEnd"),
            ("yank", "tui.editor.yank"),
            ("yankPop", "tui.editor.yankPop"),
            ("undo", "tui.editor.undo"),
            ("newLine", "tui.input.newLine"),
            ("submit", "tui.input.submit"),
            ("tab", "tui.input.tab"),
            ("copy", "tui.input.copy"),
            ("selectUp", "tui.select.up"),
            ("selectDown", "tui.select.down"),
            ("selectPageUp", "tui.select.pageUp"),
            ("selectPageDown", "tui.select.pageDown"),
            ("selectConfirm", "tui.select.confirm"),
            ("selectCancel", "tui.select.cancel"),
            ("interrupt", "app.interrupt"),
            ("clear", "app.clear"),
            ("exit", "app.exit"),
            ("suspend", "app.suspend"),
            ("cycleThinkingLevel", "app.thinking.cycle"),
            ("cycleModelForward", "app.model.cycleForward"),
            ("cycleModelBackward", "app.model.cycleBackward"),
            ("selectModel", "app.model.select"),
            ("expandTools", "app.tools.expand"),
            ("toggleThinking", "app.thinking.toggle"),
            ("toggleSessionNamedFilter", "app.session.toggleNamedFilter"),
            ("externalEditor", "app.editor.external"),
            ("followUp", "app.message.followUp"),
            ("dequeue", "app.message.dequeue"),
            ("pasteImage", "app.clipboard.pasteImage"),
            ("newSession", "app.session.new"),
            ("tree", "app.session.tree"),
            ("fork", "app.session.fork"),
            ("resume", "app.session.resume"),
            ("treeFoldOrUp", "app.tree.foldOrUp"),
            ("treeUnfoldOrDown", "app.tree.unfoldOrDown"),
            ("treeEditLabel", "app.tree.editLabel"),
            ("treeToggleLabelTimestamp", "app.tree.toggleLabelTimestamp"),
            ("toggleSessionPath", "app.session.togglePath"),
            ("toggleSessionSort", "app.session.toggleSort"),
            ("renameSession", "app.session.rename"),
            ("deleteSession", "app.session.delete"),
            ("deleteSessionNoninvasive", "app.session.deleteNoninvasive"),
        ])
    })
}

fn is_legacy_keybinding_name(key: &str) -> bool {
    keybinding_name_migrations().contains_key(key)
}

#[derive(Debug, Clone, PartialEq)]
enum RawValue {
    Key(String),
    Keys(Vec<String>),
}

impl RawValue {
    fn to_json(&self) -> serde_json::Value {
        match self {
            RawValue::Key(key) => serde_json::Value::String(key.clone()),
            RawValue::Keys(keys) => serde_json::Value::Array(
                keys.iter()
                    .map(|key| serde_json::Value::String(key.clone()))
                    .collect(),
            ),
        }
    }
}

fn normalize_raw(value: &serde_json::Value) -> Option<RawValue> {
    match value {
        serde_json::Value::String(key) => Some(RawValue::Key(key.clone())),
        serde_json::Value::Array(items)
            if !items.is_empty() && items.iter().all(|item| item.is_string()) =>
        {
            Some(RawValue::Keys(
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect(),
            ))
        }
        _ => None,
    }
}

/// Order config entries by KEYBINDINGS definition order, extras sorted after.
fn order_keybindings_config(
    config: BTreeMap<String, RawValue>,
    definition_order: &[&'static str],
) -> BTreeMap<String, RawValue> {
    let mut ordered = BTreeMap::new();
    for keybinding in definition_order {
        if let Some(value) = config.get(*keybinding) {
            ordered.insert(keybinding.to_string(), value.clone());
        }
    }
    let extras: Vec<String> = config
        .keys()
        .filter(|key| !ordered.contains_key(*key))
        .cloned()
        .collect();
    for key in extras {
        if let Some(value) = config.get(&key) {
            ordered.insert(key, value.clone());
        }
    }
    ordered
}

/// Upstream `migrateKeybindingsConfig`: rewrite legacy names, drop old
/// entries shadowed by a namespaced one, and order by definition order.
pub fn migrate_keybindings_config(
    raw_config: &BTreeMap<String, serde_json::Value>,
    definition_order: &[&'static str],
) -> (BTreeMap<String, serde_json::Value>, bool) {
    let mut config: BTreeMap<String, RawValue> = BTreeMap::new();
    let mut migrated = false;

    for (key, value) in raw_config {
        let Some(normalized) = normalize_raw(value) else {
            continue;
        };
        let next_key = if is_legacy_keybinding_name(key) {
            keybinding_name_migrations()
                .get(key.as_str())
                .copied()
                .unwrap_or(key)
        } else {
            key.as_str()
        };
        if next_key != key {
            migrated = true;
        }
        if key != next_key && raw_config.contains_key(next_key) {
            migrated = true;
            continue;
        }
        config.insert(next_key.to_string(), normalized);
    }

    let ordered = order_keybindings_config(config, definition_order);
    let json_config: BTreeMap<String, serde_json::Value> = ordered
        .iter()
        .map(|(key, value)| (key.clone(), value.to_json()))
        .collect();
    (json_config, migrated)
}

/// Upstream `KeybindingsManager`: TUI manager over the app definitions with
/// keybindings.json persistence and reload.
pub struct KeybindingsManager {
    inner: TuiKeybindingsManager,
    definitions: BTreeMap<&'static str, KeybindingDefinition>,
    config_path: Option<PathBuf>,
    user_bindings: KeybindingsConfig,
}

impl KeybindingsManager {
    pub fn new(
        definitions: BTreeMap<&'static str, KeybindingDefinition>,
        user_bindings: KeybindingsConfig,
        config_path: Option<PathBuf>,
    ) -> Self {
        let inner = TuiKeybindingsManager::new(definitions.clone(), user_bindings.clone());
        Self {
            inner,
            definitions,
            config_path,
            user_bindings,
        }
    }

    /// The TUI-level manager over the same definitions (upstream installs the
    /// single merged manager through `setKeybindings`; the port keeps the
    /// app-level manager for its own lookups and installs a TUI-level twin
    /// globally).
    pub fn tui_manager(&self) -> TuiKeybindingsManager {
        TuiKeybindingsManager::new(self.definitions.clone(), self.user_bindings.clone())
    }

    /// Upstream `KeybindingsManager.create`: load user overrides from
    /// `<agentDir>/keybindings.json` (with migration).
    pub fn create(agent_dir: &Path) -> Self {
        let config_path = agent_dir.join("keybindings.json");
        let user_bindings = Self::load_from_file(&config_path);
        Self::new(app_definitions(), user_bindings, Some(config_path))
    }

    pub fn reload(&mut self) {
        let Some(config_path) = &self.config_path else {
            return;
        };
        let user_bindings = Self::load_from_file(config_path);
        self.set_user_bindings(user_bindings);
    }

    pub fn set_user_bindings(&mut self, user_bindings: KeybindingsConfig) {
        self.user_bindings = user_bindings.clone();
        self.inner.set_user_bindings(user_bindings);
    }

    pub fn get_user_bindings(&self) -> KeybindingsConfig {
        self.user_bindings.clone()
    }

    /// Upstream `getEffectiveConfig` (resolved bindings including defaults).
    pub fn get_effective_config(&self) -> KeybindingsConfig {
        self.inner.get_resolved_bindings()
    }

    /// Resolved keys for a binding id.
    pub fn get_keys(&self, keybinding: &str) -> Vec<String> {
        self.inner.get_keys(keybinding)
    }

    /// Match raw input against a binding.
    pub fn matches(&self, data: &str, keybinding: &str) -> bool {
        self.inner.matches(data, keybinding)
    }

    /// Direct user-binding conflicts.
    pub fn get_conflicts(&self) -> Vec<pillar_tui::keybindings::KeybindingConflict> {
        self.inner.get_conflicts()
    }

    fn load_from_file(path: &Path) -> KeybindingsConfig {
        let Some(raw) = load_raw_config(path) else {
            return KeybindingsConfig::new();
        };
        let definitions = app_definitions();
        let definition_order: Vec<&'static str> = definitions.keys().copied().collect();
        let (config, _migrated) = migrate_keybindings_config(&raw, &definition_order);
        config
            .into_iter()
            .filter_map(|(key, value)| match normalize_raw(&value) {
                Some(RawValue::Key(key_id)) => Some((key, vec![key_id])),
                Some(RawValue::Keys(keys)) => Some((key, keys)),
                None => None,
            })
            .collect()
    }
}

/// App definitions for the current platform (upstream `KEYBINDINGS` built
/// with `process.platform` / `process.env`).
pub fn app_definitions() -> BTreeMap<&'static str, KeybindingDefinition> {
    let platform = std::env::var("PI_TEST_PLATFORM").unwrap_or_else(|_| {
        if cfg!(target_os = "windows") {
            "win32".to_string()
        } else if cfg!(target_os = "macos") {
            "darwin".to_string()
        } else {
            "linux".to_string()
        }
    });
    let env: BTreeMap<String, String> = std::env::vars().collect();
    keybindings(&platform, &env)
}

fn load_raw_config(path: &Path) -> Option<BTreeMap<String, serde_json::Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let object = parsed.as_object()?;
    Some(object.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}
