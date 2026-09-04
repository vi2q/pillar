//! Port of packages/tui/src/keybindings.ts (pi v0.84.3).
//!
//! Global keybinding registry: definitions with default keys, user overrides
//! (per binding, not evicting defaults of other bindings), and conflict
//! detection for direct user-binding collisions.

use std::collections::BTreeMap;

use crate::keys::matches_key;

/// A keybinding definition: default keys plus an optional description.
#[derive(Debug, Clone, Default)]
pub struct KeybindingDefinition {
    pub default_keys: Vec<String>,
    pub description: Option<String>,
}

/// User overrides: keybinding id -> key ids (single key or list).
pub type KeybindingsConfig = BTreeMap<String, Vec<String>>;

/// A direct user-binding conflict (two user bindings claiming one key).
#[derive(Debug, Clone, PartialEq)]
pub struct KeybindingConflict {
    pub key: String,
    pub keybindings: Vec<String>,
}

/// Upstream `TUI_KEYBINDINGS`: the base definitions.
pub fn tui_keybindings() -> BTreeMap<&'static str, KeybindingDefinition> {
    let mut m = BTreeMap::new();
    let mut def = |id: &'static str, keys: Vec<&'static str>, description: &'static str| {
        m.insert(
            id,
            KeybindingDefinition {
                default_keys: keys.into_iter().map(str::to_string).collect(),
                description: Some(description.to_string()),
            },
        );
    };

    // Editor navigation and editing
    def("tui.editor.cursorUp", vec!["up"], "Move cursor up");
    def("tui.editor.cursorDown", vec!["down"], "Move cursor down");
    def(
        "tui.editor.historyPrevious",
        vec![],
        "Select previous prompt history entry",
    );
    def(
        "tui.editor.historyNext",
        vec![],
        "Select next prompt history entry",
    );
    def(
        "tui.editor.cursorLeft",
        vec!["left", "ctrl+b"],
        "Move cursor left",
    );
    def(
        "tui.editor.cursorRight",
        vec!["right", "ctrl+f"],
        "Move cursor right",
    );
    def(
        "tui.editor.cursorWordLeft",
        vec!["alt+left", "ctrl+left", "alt+b"],
        "Move cursor word left",
    );
    def(
        "tui.editor.cursorWordRight",
        vec!["alt+right", "ctrl+right", "alt+f"],
        "Move cursor word right",
    );
    def(
        "tui.editor.cursorLineStart",
        vec!["home", "ctrl+home", "ctrl+a"],
        "Move to line start",
    );
    def(
        "tui.editor.cursorLineEnd",
        vec!["end", "ctrl+end", "ctrl+e"],
        "Move to line end",
    );
    def(
        "tui.editor.jumpForward",
        vec!["ctrl+]"],
        "Jump forward to character",
    );
    def(
        "tui.editor.jumpBackward",
        vec!["ctrl+alt+]"],
        "Jump backward to character",
    );
    def(
        "tui.editor.pageUp",
        vec!["pageUp", "ctrl+pageUp"],
        "Page up",
    );
    def(
        "tui.editor.pageDown",
        vec!["pageDown", "ctrl+pageDown"],
        "Page down",
    );
    def(
        "tui.editor.deleteCharBackward",
        vec!["backspace"],
        "Delete character backward",
    );
    def(
        "tui.editor.deleteCharForward",
        vec!["delete", "ctrl+d"],
        "Delete character forward",
    );
    def(
        "tui.editor.deleteWordBackward",
        vec!["ctrl+w", "alt+backspace"],
        "Delete word backward",
    );
    def(
        "tui.editor.deleteWordForward",
        vec!["alt+d", "alt+delete"],
        "Delete word forward",
    );
    def(
        "tui.editor.deleteToLineStart",
        vec!["ctrl+u"],
        "Delete to line start",
    );
    def(
        "tui.editor.deleteToLineEnd",
        vec!["ctrl+k"],
        "Delete to line end",
    );
    def("tui.editor.yank", vec!["ctrl+y"], "Yank");
    def("tui.editor.yankPop", vec!["alt+y"], "Yank pop");
    def("tui.editor.undo", vec!["ctrl+-"], "Undo");
    // Generic input actions
    def(
        "tui.input.newLine",
        vec!["shift+enter", "ctrl+j"],
        "Insert newline",
    );
    def("tui.input.submit", vec!["enter"], "Submit input");
    def("tui.input.tab", vec!["tab"], "Tab / autocomplete");
    def("tui.input.copy", vec!["ctrl+c"], "Copy selection");
    // Generic selection actions
    def("tui.select.up", vec!["up"], "Move selection up");
    def("tui.select.down", vec!["down"], "Move selection down");
    def("tui.select.pageUp", vec!["pageUp"], "Selection page up");
    def(
        "tui.select.pageDown",
        vec!["pageDown"],
        "Selection page down",
    );
    def("tui.select.confirm", vec!["enter"], "Confirm selection");
    def(
        "tui.select.cancel",
        vec!["escape", "ctrl+c"],
        "Cancel selection",
    );
    // Alternate-screen viewport navigation (intentionally shadow the
    // unmodified editor bindings in fullscreen mode)
    def(
        "tui.altScreen.pageUp",
        vec!["pageUp"],
        "Scroll viewport up one page",
    );
    def(
        "tui.altScreen.pageDown",
        vec!["pageDown"],
        "Scroll viewport down one page",
    );
    def(
        "tui.altScreen.halfPageUp",
        vec![],
        "Scroll viewport up half a page",
    );
    def(
        "tui.altScreen.halfPageDown",
        vec![],
        "Scroll viewport down half a page",
    );
    def(
        "tui.altScreen.lineUp",
        vec![],
        "Scroll viewport up one line",
    );
    def(
        "tui.altScreen.lineDown",
        vec![],
        "Scroll viewport down one line",
    );
    def(
        "tui.altScreen.previousPrompt",
        vec!["ctrl+shift+up", "ctrl+up"],
        "Jump to previous semantic prompt",
    );
    def(
        "tui.altScreen.nextPrompt",
        vec!["ctrl+shift+down", "ctrl+down"],
        "Jump to next semantic prompt",
    );
    def(
        "tui.altScreen.search",
        vec!["ctrl+shift+f"],
        "Search the primary scroll view",
    );
    def(
        "tui.altScreen.searchNext",
        vec!["enter", "ctrl+g"],
        "Select the next search match",
    );
    def(
        "tui.altScreen.searchPrevious",
        vec!["shift+enter", "ctrl+shift+g"],
        "Select the previous search match",
    );
    def(
        "tui.altScreen.searchClose",
        vec!["escape"],
        "Close transcript search",
    );
    def("tui.altScreen.top", vec!["home"], "Scroll viewport to top");
    def(
        "tui.altScreen.bottom",
        vec!["end"],
        "Scroll viewport to bottom",
    );
    m
}

/// Normalize a key list, preserving order, deduplicating.
fn normalize_keys(keys: Option<&Vec<String>>) -> Vec<String> {
    let Some(keys) = keys else { return Vec::new() };
    let mut seen = std::collections::BTreeSet::new();
    let mut result = Vec::new();
    for key in keys {
        if seen.insert(key.clone()) {
            result.push(key.clone());
        }
    }
    result
}

/// Keybindings manager (upstream `KeybindingsManager`).
pub struct KeybindingsManager {
    definitions: BTreeMap<&'static str, KeybindingDefinition>,
    user_bindings: KeybindingsConfig,
    keys_by_id: BTreeMap<String, Vec<String>>,
    conflicts: Vec<KeybindingConflict>,
}

impl KeybindingsManager {
    pub fn new(
        definitions: BTreeMap<&'static str, KeybindingDefinition>,
        user_bindings: KeybindingsConfig,
    ) -> Self {
        let mut manager = Self {
            definitions,
            user_bindings,
            keys_by_id: BTreeMap::new(),
            conflicts: Vec::new(),
        };
        manager.rebuild();
        manager
    }

    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.conflicts.clear();

        // Track user claims per key (direct conflicts only).
        let mut user_claims: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (keybinding, keys) in &self.user_bindings {
            if !self.definitions.contains_key(keybinding.as_str()) {
                continue;
            }
            for key in normalize_keys(Some(keys)) {
                user_claims.entry(key).or_default().push(keybinding.clone());
            }
        }
        for (key, claimants) in user_claims {
            if claimants.len() > 1 {
                self.conflicts.push(KeybindingConflict {
                    key,
                    keybindings: claimants,
                });
            }
        }

        // Resolve keys per binding: user override replaces the default keys
        // for that binding only; other bindings keep their defaults.
        for (id, definition) in &self.definitions {
            let keys = match self.user_bindings.get(*id) {
                Some(user_keys) => normalize_keys(Some(user_keys)),
                None => normalize_keys(Some(&definition.default_keys)),
            };
            self.keys_by_id.insert(id.to_string(), keys);
        }
    }

    /// Match raw input against a keybinding's resolved keys.
    pub fn matches(&self, data: &str, keybinding: &str) -> bool {
        self.keys_by_id
            .get(keybinding)
            .map(|keys| keys.iter().any(|key| matches_key(data, key)))
            .unwrap_or(false)
    }

    /// Resolved keys for a keybinding (empty when unbound).
    pub fn get_keys(&self, keybinding: &str) -> Vec<String> {
        self.keys_by_id.get(keybinding).cloned().unwrap_or_default()
    }

    /// The definition for a keybinding.
    pub fn get_definition(&self, keybinding: &str) -> Option<&KeybindingDefinition> {
        self.definitions.get(keybinding)
    }

    /// Direct user-binding conflicts.
    pub fn get_conflicts(&self) -> Vec<KeybindingConflict> {
        self.conflicts.clone()
    }

    /// Replace user bindings and rebuild.
    pub fn set_user_bindings(&mut self, user_bindings: KeybindingsConfig) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    /// The user bindings as set.
    pub fn get_user_bindings(&self) -> KeybindingsConfig {
        self.user_bindings.clone()
    }

    /// The fully resolved config: every defined binding with its effective
    /// keys (upstream `getResolvedBindings`).
    pub fn get_resolved_bindings(&self) -> KeybindingsConfig {
        let mut resolved = KeybindingsConfig::new();
        for id in self.definitions.keys() {
            let keys = self.keys_by_id.get(*id).cloned().unwrap_or_default();
            resolved.insert(id.to_string(), keys);
        }
        resolved
    }
}

static GLOBAL_KEYBINDINGS: std::sync::Mutex<Option<KeybindingsManager>> =
    std::sync::Mutex::new(None);

/// Replace the process-global keybindings (upstream `setKeybindings`).
pub fn set_keybindings(keybindings: KeybindingsManager) {
    *GLOBAL_KEYBINDINGS.lock().expect("keybindings lock") = Some(keybindings);
}

/// Access the process-global keybindings, defaulting to `TUI_KEYBINDINGS`
/// (upstream `getKeybindings`).
pub fn with_global_keybindings<R>(f: impl FnOnce(&KeybindingsManager) -> R) -> R {
    let mut guard = GLOBAL_KEYBINDINGS.lock().expect("keybindings lock");
    if guard.is_none() {
        *guard = Some(KeybindingsManager::new(
            tui_keybindings(),
            KeybindingsConfig::new(),
        ));
    }
    f(guard.as_ref().expect("initialized"))
}
