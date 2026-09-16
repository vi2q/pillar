//! Port of packages/coding-agent/src/modes/interactive/components (pi v0.84.3):
//! the transcript components the interactive mode composes into its containers.
//!
//! Ported so far: the shared helpers (keybinding hints, dynamic borders,
//! visual truncation, the countdown timer, Markdown transforms, diff
//! rendering) plus the loaders/status indicators, the bordered loader and
//! custom-entry rendering. The message and tool-execution components land in
//! later slices; `mermaid.ts` waits on the `grok-mermaid` renderer and a
//! Markdown lexer (see docs/TASKS.md).

pub mod assistant_message;
pub mod bash_execution;
pub mod bordered_loader;
pub mod branch_summary_message;
pub mod compaction_summary_message;
pub mod countdown_timer;
pub mod custom_entry;
pub mod custom_message;
pub mod diff;
pub mod dynamic_border;
pub mod extension_input;
pub mod extension_selector;
pub mod footer;
pub mod settings_submenu;
pub mod keybinding_hints;
pub mod markdown_transform;
pub mod model_picker;
pub mod scoped_models_selector;
pub mod session_selector;
pub mod session_selector_search;
pub mod skill_invocation_message;
pub mod status_indicator;
pub mod thinking_selector;
pub mod tool_execution;
pub mod tree_selector;
pub mod user_message;
pub mod user_message_selector;
pub mod visual_truncate;

/// Shared test setup for components that match the app-level keybindings
/// (`app.*`) and render with a theme: installs the merged keybinding table
/// and a theme under one process-wide lock.
#[cfg(test)]
pub mod test_support {
    use std::sync::{Mutex, MutexGuard};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    pub fn setup() -> MutexGuard<'static, ()> {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::modes::interactive::theme::init_theme(Some("dark"));
        let definitions = crate::core::keybindings::keybindings("darwin", &Default::default());
        pillar_tui::keybindings::set_keybindings(pillar_tui::keybindings::KeybindingsManager::new(
            definitions,
            Default::default(),
        ));
        guard
    }
}
