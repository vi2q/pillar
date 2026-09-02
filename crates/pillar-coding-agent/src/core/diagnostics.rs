//! Port of packages/coding-agent/src/core/diagnostics.ts and
//! slash-commands.ts (pi v0.84.3): resource-load diagnostics shapes and the
//! builtin slash-command registry.

use crate::core::source_info::SourceInfo;

// --- diagnostics.ts ---------------------------------------------------------

/// A resource collision detected during loading (upstream
/// `ResourceCollision`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceCollision {
    /// "extension" | "skill" | "prompt" | "theme".
    pub resource_type: String,
    /// Skill name, command/tool/flag name, prompt name, theme name.
    pub name: String,
    pub winner_path: String,
    pub loser_path: String,
    /// e.g. "npm:foo", "git:...", "local".
    pub winner_source: Option<String>,
    pub loser_source: Option<String>,
}

/// A diagnostic produced while loading resources (upstream
/// `ResourceDiagnostic`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceDiagnostic {
    /// "warning" | "error" | "collision".
    pub kind: String,
    pub message: String,
    pub path: Option<String>,
    pub collision: Option<ResourceCollision>,
}

// --- slash-commands.ts ---------------------------------------------------------

/// Where a slash command came from (upstream `SlashCommandSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommandSource {
    Extension,
    Prompt,
    Skill,
}

impl SlashCommandSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SlashCommandSource::Extension => "extension",
            SlashCommandSource::Prompt => "prompt",
            SlashCommandSource::Skill => "skill",
        }
    }
}

/// A slash command available in the UI (upstream `SlashCommandInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct SlashCommandInfo {
    pub name: String,
    pub description: Option<String>,
    pub source: SlashCommandSource,
    pub source_info: SourceInfo,
}

/// A builtin slash command (upstream `BuiltinSlashCommand`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinSlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

const fn cmd(name: &'static str, description: &'static str) -> BuiltinSlashCommand {
    BuiltinSlashCommand {
        name,
        description,
        argument_hint: None,
    }
}

const fn cmd_arg(
    name: &'static str,
    description: &'static str,
    argument_hint: &'static str,
) -> BuiltinSlashCommand {
    BuiltinSlashCommand {
        name,
        description,
        argument_hint: Some(argument_hint),
    }
}

/// Builtin slash commands, in upstream order (upstream
/// `BUILTIN_SLASH_COMMANDS`).
pub const BUILTIN_SLASH_COMMANDS: [BuiltinSlashCommand; 23] = [
    cmd("settings", "Open settings menu"),
    cmd_arg(
        "model",
        "Select model (opens selector UI)",
        "<provider/model>",
    ),
    cmd("tree", "Navigate session tree (switch branches)"),
    cmd_arg("thinking", "Set thinking level", "<level>"),
    cmd("scoped-models", "Enable/disable models for Ctrl+P cycling"),
    cmd(
        "export",
        "Export session (HTML default, or specify path: .html/.jsonl)",
    ),
    cmd("import", "Import and resume a session from a JSONL file"),
    cmd("share", "Share session as a secret GitHub gist"),
    cmd("copy", "Copy last agent message to clipboard"),
    cmd("name", "Set session display name"),
    cmd("session", "Show session info and stats"),
    cmd("changelog", "Show changelog entries"),
    cmd("hotkeys", "Show all keyboard shortcuts"),
    cmd("fork", "Create a new fork from a previous user message"),
    cmd(
        "clone",
        "Duplicate the current session at the current position",
    ),
    cmd("trust", "Save project trust decision for future sessions"),
    cmd_arg("login", "Configure provider authentication", "<provider>"),
    cmd("logout", "Remove provider authentication"),
    cmd("new", "Start a new session"),
    cmd("compact", "Manually compact the session context"),
    cmd("resume", "Resume a different session"),
    cmd(
        "reload",
        "Reload keybindings, extensions, skills, prompts, themes, and context files",
    ),
    cmd("quit", "Quit pi"),
];
