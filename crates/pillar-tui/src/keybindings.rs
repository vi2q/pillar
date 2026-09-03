//! Port of packages/tui/src/keybindings.ts (pi v0.84.3): global
//! keybinding registry with per-action default keys, user overrides,
//! conflict detection, and a thread-local global manager.

use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::keys::matches_key;

/// A keybinding action id (upstream `Keybinding`, e.g.
/// "tui.editor.cursorUp").
pub type KeybindingId = &'static str;

/// Default key definitions (upstream `TUI_KEYBINDINGS`): action →
/// (default keys, description).
pub struct KeybindingDefinition {
    pub default_keys: &'static [&'static str],
    pub description: &'static str,
}

/// The built-in keybinding table (upstream `TUI_KEYBINDINGS`).
pub const TUI_KEYBINDINGS: &[(&str, KeybindingDefinition)] = &[
    (
        "tui.editor.cursorUp",
        KeybindingDefinition {
            default_keys: &["up"],
            description: "Move cursor up",
        },
    ),
    (
        "tui.editor.cursorDown",
        KeybindingDefinition {
            default_keys: &["down"],
            description: "Move cursor down",
        },
    ),
    (
        "tui.editor.historyPrevious",
        KeybindingDefinition {
            default_keys: &[],
            description: "Select previous prompt history entry",
        },
    ),
    (
        "tui.editor.historyNext",
        KeybindingDefinition {
            default_keys: &[],
            description: "Select next prompt history entry",
        },
    ),
    (
        "tui.editor.cursorLeft",
        KeybindingDefinition {
            default_keys: &["left", "ctrl+b"],
            description: "Move cursor left",
        },
    ),
    (
        "tui.editor.cursorRight",
        KeybindingDefinition {
            default_keys: &["right", "ctrl+f"],
            description: "Move cursor right",
        },
    ),
    (
        "tui.editor.cursorWordLeft",
        KeybindingDefinition {
            default_keys: &["alt+left", "ctrl+left", "alt+b"],
            description: "Move cursor word left",
        },
    ),
    (
        "tui.editor.cursorWordRight",
        KeybindingDefinition {
            default_keys: &["alt+right", "ctrl+right", "alt+f"],
            description: "Move cursor word right",
        },
    ),
    (
        "tui.editor.cursorLineStart",
        KeybindingDefinition {
            default_keys: &["home", "ctrl+home", "ctrl+a"],
            description: "Move to line start",
        },
    ),
    (
        "tui.editor.cursorLineEnd",
        KeybindingDefinition {
            default_keys: &["end", "ctrl+end", "ctrl+e"],
            description: "Move to line end",
        },
    ),
    (
        "tui.editor.jumpForward",
        KeybindingDefinition {
            default_keys: &["ctrl+]"],
            description: "Jump forward to character",
        },
    ),
    (
        "tui.editor.jumpBackward",
        KeybindingDefinition {
            default_keys: &["ctrl+alt+]"],
            description: "Jump backward to character",
        },
    ),
    (
        "tui.editor.pageUp",
        KeybindingDefinition {
            default_keys: &["pageUp", "ctrl+pageUp"],
            description: "Page up",
        },
    ),
    (
        "tui.editor.pageDown",
        KeybindingDefinition {
            default_keys: &["pageDown", "ctrl+pageDown"],
            description: "Page down",
        },
    ),
    (
        "tui.editor.deleteCharBackward",
        KeybindingDefinition {
            default_keys: &["backspace"],
            description: "Delete character backward",
        },
    ),
    (
        "tui.editor.deleteCharForward",
        KeybindingDefinition {
            default_keys: &["delete", "ctrl+d"],
            description: "Delete character forward",
        },
    ),
    (
        "tui.editor.deleteWordBackward",
        KeybindingDefinition {
            default_keys: &["ctrl+w", "alt+backspace"],
            description: "Delete word backward",
        },
    ),
    (
        "tui.editor.deleteWordForward",
        KeybindingDefinition {
            default_keys: &["alt+d", "alt+delete"],
            description: "Delete word forward",
        },
    ),
    (
        "tui.editor.deleteToLineStart",
        KeybindingDefinition {
            default_keys: &["ctrl+u"],
            description: "Delete to line start",
        },
    ),
    (
        "tui.editor.deleteToLineEnd",
        KeybindingDefinition {
            default_keys: &["ctrl+k"],
            description: "Delete to line end",
        },
    ),
    (
        "tui.editor.yank",
        KeybindingDefinition {
            default_keys: &["ctrl+y"],
            description: "Yank",
        },
    ),
    (
        "tui.editor.yankPop",
        KeybindingDefinition {
            default_keys: &["alt+y"],
            description: "Yank pop",
        },
    ),
    (
        "tui.editor.undo",
        KeybindingDefinition {
            default_keys: &["ctrl+-"],
            description: "Undo",
        },
    ),
    (
        "tui.input.newLine",
        KeybindingDefinition {
            default_keys: &["shift+enter", "ctrl+j"],
            description: "Insert newline",
        },
    ),
    (
        "tui.input.submit",
        KeybindingDefinition {
            default_keys: &["enter"],
            description: "Submit input",
        },
    ),
    (
        "tui.input.tab",
        KeybindingDefinition {
            default_keys: &["tab"],
            description: "Tab / autocomplete",
        },
    ),
    (
        "tui.input.copy",
        KeybindingDefinition {
            default_keys: &["ctrl+c"],
            description: "Copy selection",
        },
    ),
    (
        "tui.select.up",
        KeybindingDefinition {
            default_keys: &["up"],
            description: "Move selection up",
        },
    ),
    (
        "tui.select.down",
        KeybindingDefinition {
            default_keys: &["down"],
            description: "Move selection down",
        },
    ),
    (
        "tui.select.pageUp",
        KeybindingDefinition {
            default_keys: &["pageUp"],
            description: "Selection page up",
        },
    ),
    (
        "tui.select.pageDown",
        KeybindingDefinition {
            default_keys: &["pageDown"],
            description: "Selection page down",
        },
    ),
    (
        "tui.select.confirm",
        KeybindingDefinition {
            default_keys: &["enter"],
            description: "Confirm selection",
        },
    ),
    (
        "tui.select.cancel",
        KeybindingDefinition {
            default_keys: &["escape", "ctrl+c"],
            description: "Cancel selection",
        },
    ),
    // These intentionally shadow the unmodified editor bindings in fullscreen mode.
    (
        "tui.altScreen.pageUp",
        KeybindingDefinition {
            default_keys: &["pageUp"],
            description: "Scroll viewport up one page",
        },
    ),
    (
        "tui.altScreen.pageDown",
        KeybindingDefinition {
            default_keys: &["pageDown"],
            description: "Scroll viewport down one page",
        },
    ),
    (
        "tui.altScreen.halfPageUp",
        KeybindingDefinition {
            default_keys: &[],
            description: "Scroll viewport up half a page",
        },
    ),
    (
        "tui.altScreen.halfPageDown",
        KeybindingDefinition {
            default_keys: &[],
            description: "Scroll viewport down half a page",
        },
    ),
    (
        "tui.altScreen.lineUp",
        KeybindingDefinition {
            default_keys: &[],
            description: "Scroll viewport up one line",
        },
    ),
    (
        "tui.altScreen.lineDown",
        KeybindingDefinition {
            default_keys: &[],
            description: "Scroll viewport down one line",
        },
    ),
    (
        "tui.altScreen.previousPrompt",
        KeybindingDefinition {
            default_keys: &["ctrl+shift+up", "ctrl+up"],
            description: "Jump to previous semantic prompt",
        },
    ),
    (
        "tui.altScreen.nextPrompt",
        KeybindingDefinition {
            default_keys: &["ctrl+shift+down", "ctrl+down"],
            description: "Jump to next semantic prompt",
        },
    ),
    (
        "tui.altScreen.search",
        KeybindingDefinition {
            default_keys: &["ctrl+shift+f"],
            description: "Search the primary scroll view",
        },
    ),
    (
        "tui.altScreen.searchNext",
        KeybindingDefinition {
            default_keys: &["enter", "ctrl+g"],
            description: "Select the next search match",
        },
    ),
    (
        "tui.altScreen.searchPrevious",
        KeybindingDefinition {
            default_keys: &["shift+enter", "ctrl+shift+g"],
            description: "Select the previous search match",
        },
    ),
    (
        "tui.altScreen.searchClose",
        KeybindingDefinition {
            default_keys: &["escape"],
            description: "Close transcript search",
        },
    ),
    (
        "tui.altScreen.top",
        KeybindingDefinition {
            default_keys: &["home"],
            description: "Scroll viewport to top",
        },
    ),
    (
        "tui.altScreen.bottom",
        KeybindingDefinition {
            default_keys: &["end"],
            description: "Scroll viewport to bottom",
        },
    ),
];

/// A detected conflict: one key claimed by multiple user-bound actions
/// (upstream `KeybindingConflict`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    pub key: String,
    pub keybindings: Vec<String>,
}

/// User binding config (upstream `KeybindingsConfig`): action → keys.
pub type UserBindings = HashMap<String, Vec<String>>;

/// User binding config with a constructor (alias kept for the parity
/// suite).
pub type KeybindingsConfig = UserBindings;

/// The definitions table (upstream `TUI_KEYBINDINGS`), exposed as a
/// function for parity-suite ergonomics.
pub fn tui_keybindings() -> &'static [(&'static str, KeybindingDefinition)] {
    TUI_KEYBINDINGS
}

fn normalize_keys(keys: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for key in keys {
        if seen.insert(key.clone()) {
            result.push(key.clone());
        }
    }
    result
}

/// Keybinding manager (upstream `KeybindingsManager`).
pub struct KeybindingsManager {
    keys_by_id: HashMap<String, Vec<String>>,
    user_bindings: UserBindings,
    conflicts: Vec<KeybindingConflict>,
}

impl KeybindingsManager {
    pub fn new(
        _definitions: &'static [(&'static str, KeybindingDefinition)],
        user_bindings: UserBindings,
    ) -> Self {
        let mut manager = Self {
            keys_by_id: HashMap::new(),
            user_bindings,
            conflicts: Vec::new(),
        };
        manager.rebuild();
        manager
    }

    pub fn with_defaults() -> Self {
        Self::new(TUI_KEYBINDINGS, UserBindings::new())
    }

    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.conflicts.clear();

        // Collect user claims per key to detect multi-action conflicts.
        // BTreeMap keeps deterministic ordering (upstream relies on JS
        // object insertion order).
        let mut user_claims: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (keybinding, keys) in &self.user_bindings {
            for key in normalize_keys(keys) {
                user_claims.entry(key).or_default().push(keybinding.clone());
            }
        }
        for (key, keybindings) in &user_claims {
            if keybindings.len() > 1 {
                self.conflicts.push(KeybindingConflict {
                    key: key.clone(),
                    keybindings: keybindings.clone(),
                });
            }
        }

        for (id, definition) in TUI_KEYBINDINGS {
            let keys = match self.user_bindings.get(*id) {
                Some(user_keys) => normalize_keys(user_keys),
                None => definition
                    .default_keys
                    .iter()
                    .map(|k| k.to_string())
                    .collect(),
            };
            self.keys_by_id.insert(id.to_string(), keys);
        }
    }

    /// Whether raw terminal data matches any of an action's keys
    /// (upstream `matches`).
    pub fn matches(&self, data: &str, keybinding: &str) -> bool {
        self.keys_by_id
            .get(keybinding)
            .map(|keys| keys.iter().any(|key| matches_key(data, key)))
            .unwrap_or(false)
    }

    /// Resolved keys for an action (upstream `getKeys`).
    pub fn get_keys(&self, keybinding: &str) -> Vec<String> {
        self.keys_by_id.get(keybinding).cloned().unwrap_or_default()
    }

    /// Default keys + description for an action (upstream
    /// `getDefinition`).
    pub fn get_definition(
        &self,
        keybinding: &str,
    ) -> Option<(&'static [&'static str], &'static str)> {
        TUI_KEYBINDINGS
            .iter()
            .find(|(id, _)| *id == keybinding)
            .map(|(_, definition)| (definition.default_keys, definition.description))
    }

    pub fn get_conflicts(&self) -> Vec<KeybindingConflict> {
        self.conflicts.clone()
    }

    pub fn set_user_bindings(&mut self, user_bindings: UserBindings) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    pub fn get_user_bindings(&self) -> UserBindings {
        self.user_bindings.clone()
    }

    /// All resolved bindings (upstream `getResolvedBindings`).
    pub fn get_resolved_bindings(&self) -> HashMap<String, Vec<String>> {
        self.keys_by_id.clone()
    }
}

thread_local! {
    static GLOBAL_KEYBINDINGS: std::cell::RefCell<Option<KeybindingsManager>> =
        const { std::cell::RefCell::new(None) };
}

/// Install a global manager (upstream `setKeybindings`).
pub fn set_keybindings(keybindings: KeybindingsManager) {
    GLOBAL_KEYBINDINGS.with_borrow_mut(|global| *global = Some(keybindings));
}

/// Access the global manager mutably, defaulting to the built-in table
/// (upstream `getKeybindings`).
pub fn with_keybindings<R>(f: impl FnOnce(&mut KeybindingsManager) -> R) -> R {
    GLOBAL_KEYBINDINGS
        .with_borrow_mut(|global| f(global.get_or_insert_with(KeybindingsManager::with_defaults)))
}
