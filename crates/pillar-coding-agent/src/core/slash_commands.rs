//! Port of packages/coding-agent/src/core/slash-commands.ts (pi v0.84.3):
//! the built-in slash commands and the shape of a slash command surfaced to
//! selectors (`SlashCommandInfo`).
//!
//! divergence: upstream keeps `BUILTIN_SLASH_COMMANDS` as a plain array
//! literal whose `quit` description interpolates `APP_NAME`; the port builds
//! the same list once behind a `LazyLock` so it can interpolate too.

use std::sync::LazyLock;

use crate::cli::args::APP_NAME;
use crate::core::source_info::SourceInfo;

/// Where a slash command comes from (upstream `SlashCommandSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// A slash command offered by the session (upstream `SlashCommandInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandInfo {
    pub name: String,
    pub description: Option<String>,
    pub source: SlashCommandSource,
    pub source_info: SourceInfo,
}

/// A command built into the TUI (upstream `BuiltinSlashCommand`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinSlashCommand {
    pub name: String,
    pub description: String,
    /// Argument placeholder shown by the autocomplete, e.g. `"<level>"`.
    pub argument_hint: Option<String>,
}

fn command(name: &str, description: &str, argument_hint: Option<&str>) -> BuiltinSlashCommand {
    BuiltinSlashCommand {
        name: name.to_string(),
        description: description.to_string(),
        argument_hint: argument_hint.map(str::to_string),
    }
}

/// The built-in slash commands in upstream order (upstream
/// `BUILTIN_SLASH_COMMANDS`).
pub static BUILTIN_SLASH_COMMANDS: LazyLock<Vec<BuiltinSlashCommand>> = LazyLock::new(|| {
    vec![
        command("settings", "Open settings menu", None),
        command(
            "model",
            "Select model (opens selector UI)",
            Some("<provider/model>"),
        ),
        command("tree", "Navigate session tree (switch branches)", None),
        command("thinking", "Set thinking level", Some("<level>")),
        command(
            "scoped-models",
            "Enable/disable models for Ctrl+P cycling",
            None,
        ),
        command(
            "export",
            "Export session (HTML default, or specify path: .html/.jsonl)",
            None,
        ),
        command(
            "import",
            "Import and resume a session from a JSONL file",
            None,
        ),
        command("share", "Share session as a secret GitHub gist", None),
        command("copy", "Copy last agent message to clipboard", None),
        command("name", "Set session display name", None),
        command("session", "Show session info and stats", None),
        command("changelog", "Show changelog entries", None),
        command("hotkeys", "Show all keyboard shortcuts", None),
        command(
            "fork",
            "Create a new fork from a previous user message",
            None,
        ),
        command(
            "clone",
            "Duplicate the current session at the current position",
            None,
        ),
        command(
            "trust",
            "Save project trust decision for future sessions",
            None,
        ),
        command(
            "login",
            "Configure provider authentication",
            Some("<provider>"),
        ),
        command("logout", "Remove provider authentication", None),
        command("new", "Start a new session", None),
        command("compact", "Manually compact the session context", None),
        command("resume", "Resume a different session", None),
        command(
            "reload",
            "Reload keybindings, extensions, skills, prompts, themes, and context files",
            None,
        ),
        command("quit", &format!("Quit {APP_NAME}"), None),
    ]
});

/// Look up a built-in command by name.
pub fn builtin_slash_command(name: &str) -> Option<&'static BuiltinSlashCommand> {
    BUILTIN_SLASH_COMMANDS
        .iter()
        .find(|command| command.name == name)
}
