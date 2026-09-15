//! Parity tests for core/slash-commands.ts (pi v0.84.3): the built-in slash
//! command list (names, descriptions, argument hints, and order) plus the
//! `SlashCommandInfo` source tags.

use pillar_coding_agent::core::slash_commands::{
    BUILTIN_SLASH_COMMANDS, SlashCommandSource, builtin_slash_command,
};

/// `(name, description, argumentHint)` exactly as upstream declares them.
const EXPECTED: &[(&str, &str, Option<&str>)] = &[
    ("settings", "Open settings menu", None),
    (
        "model",
        "Select model (opens selector UI)",
        Some("<provider/model>"),
    ),
    ("tree", "Navigate session tree (switch branches)", None),
    ("thinking", "Set thinking level", Some("<level>")),
    (
        "scoped-models",
        "Enable/disable models for Ctrl+P cycling",
        None,
    ),
    (
        "export",
        "Export session (HTML default, or specify path: .html/.jsonl)",
        None,
    ),
    (
        "import",
        "Import and resume a session from a JSONL file",
        None,
    ),
    ("share", "Share session as a secret GitHub gist", None),
    ("copy", "Copy last agent message to clipboard", None),
    ("name", "Set session display name", None),
    ("session", "Show session info and stats", None),
    ("changelog", "Show changelog entries", None),
    ("hotkeys", "Show all keyboard shortcuts", None),
    (
        "fork",
        "Create a new fork from a previous user message",
        None,
    ),
    (
        "clone",
        "Duplicate the current session at the current position",
        None,
    ),
    (
        "trust",
        "Save project trust decision for future sessions",
        None,
    ),
    (
        "login",
        "Configure provider authentication",
        Some("<provider>"),
    ),
    ("logout", "Remove provider authentication", None),
    ("new", "Start a new session", None),
    ("compact", "Manually compact the session context", None),
    ("resume", "Resume a different session", None),
    (
        "reload",
        "Reload keybindings, extensions, skills, prompts, themes, and context files",
        None,
    ),
    ("quit", "Quit pillar", None),
];

#[test]
fn builtin_slash_commands_match_upstream_order_and_text() {
    assert_eq!(BUILTIN_SLASH_COMMANDS.len(), EXPECTED.len());
    for (command, (name, description, hint)) in BUILTIN_SLASH_COMMANDS.iter().zip(EXPECTED) {
        assert_eq!(&command.name, name);
        assert_eq!(&command.description, description);
        assert_eq!(command.argument_hint.as_deref(), *hint);
    }
}

#[test]
fn builtin_slash_commands_are_unique_and_lookupable() {
    let mut names: Vec<&str> = BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|command| command.name.as_str())
        .collect();
    let total = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), total, "command names must be unique");

    let model = builtin_slash_command("model").expect("model command");
    assert_eq!(model.argument_hint.as_deref(), Some("<provider/model>"));
    assert!(builtin_slash_command("nope").is_none());
}

#[test]
fn slash_command_sources_use_upstream_tags() {
    assert_eq!(SlashCommandSource::Extension.as_str(), "extension");
    assert_eq!(SlashCommandSource::Prompt.as_str(), "prompt");
    assert_eq!(SlashCommandSource::Skill.as_str(), "skill");
}
