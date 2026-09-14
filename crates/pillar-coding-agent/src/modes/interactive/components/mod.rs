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
pub mod bordered_loader;
pub mod branch_summary_message;
pub mod compaction_summary_message;
pub mod countdown_timer;
pub mod custom_entry;
pub mod custom_message;
pub mod diff;
pub mod dynamic_border;
pub mod keybinding_hints;
pub mod markdown_transform;
pub mod skill_invocation_message;
pub mod status_indicator;
pub mod user_message;
pub mod visual_truncate;
