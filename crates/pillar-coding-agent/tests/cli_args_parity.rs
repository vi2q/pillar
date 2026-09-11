//! Parity tests for CLI argument parsing (upstream
//! packages/coding-agent/test/args.test.ts). One `#[test]` per upstream case.

use pillar_coding_agent::cli::args::{
    Args, DiagnosticKind, FlagValue, ListModels, Mode, TuiMode, is_valid_thinking_level,
    normalize_session_name, parse_args,
};

fn parse(args: &[&str]) -> Args {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    parse_args(&owned)
}

fn string_flag(args: &Args, name: &str) -> Option<String> {
    match args.unknown_flags.get(name) {
        Some(FlagValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn bool_flag(args: &Args, name: &str) -> Option<bool> {
    match args.unknown_flags.get(name) {
        Some(FlagValue::Boolean(value)) => Some(*value),
        _ => None,
    }
}

// --- --version flag --------------------------------------------------------

#[test]
fn parses_version_flag() {
    assert_eq!(parse(&["--version"]).version, Some(true));
}

#[test]
fn parses_v_shorthand() {
    assert_eq!(parse(&["-v"]).version, Some(true));
}

#[test]
fn version_takes_precedence_over_other_args() {
    let result = parse(&["--version", "--help", "some message"]);
    assert_eq!(result.version, Some(true));
    assert_eq!(result.help, Some(true));
    assert!(result.messages.contains(&"some message".to_string()));
}

// --- --help flag -----------------------------------------------------------

#[test]
fn parses_help_flag() {
    assert_eq!(parse(&["--help"]).help, Some(true));
}

#[test]
fn parses_h_shorthand() {
    assert_eq!(parse(&["-h"]).help, Some(true));
}

// --- --print flag ----------------------------------------------------------

#[test]
fn parses_print_flag() {
    assert_eq!(parse(&["--print"]).print, Some(true));
}

#[test]
fn parses_p_shorthand() {
    assert_eq!(parse(&["-p"]).print, Some(true));
}

#[test]
fn parses_prompt_after_p_even_with_yaml_frontmatter() {
    let prompt = "---\ntitle: hello\n---\nSay hi.";
    let result = parse(&["-p", prompt]);
    assert_eq!(result.print, Some(true));
    assert_eq!(result.messages, vec![prompt.to_string()]);
    assert!(result.unknown_flags.is_empty());
}

#[test]
fn does_not_consume_options_after_p_as_prompts() {
    let result = parse(&["-p", "--provider", "openai", "Say hi."]);
    assert_eq!(result.print, Some(true));
    assert_eq!(result.provider.as_deref(), Some("openai"));
    assert_eq!(result.messages, vec!["Say hi.".to_string()]);
}

// --- --continue / --resume -------------------------------------------------

#[test]
fn parses_continue_flag() {
    assert_eq!(parse(&["--continue"]).continue_session, Some(true));
}

#[test]
fn parses_c_shorthand() {
    assert_eq!(parse(&["-c"]).continue_session, Some(true));
}

#[test]
fn parses_resume_flag() {
    assert_eq!(parse(&["--resume"]).resume, Some(true));
}

#[test]
fn parses_r_shorthand() {
    assert_eq!(parse(&["-r"]).resume, Some(true));
}

// --- flags with values -----------------------------------------------------

#[test]
fn parses_provider() {
    assert_eq!(
        parse(&["--provider", "openai"]).provider.as_deref(),
        Some("openai")
    );
}

#[test]
fn parses_model() {
    assert_eq!(
        parse(&["--model", "gpt-4o"]).model.as_deref(),
        Some("gpt-4o")
    );
}

#[test]
fn parses_api_key() {
    assert_eq!(
        parse(&["--api-key", "sk-test-key"]).api_key.as_deref(),
        Some("sk-test-key")
    );
}

#[test]
fn parses_system_prompt() {
    assert_eq!(
        parse(&["--system-prompt", "You are a helpful assistant"])
            .system_prompt
            .as_deref(),
        Some("You are a helpful assistant")
    );
}

#[test]
fn parses_append_system_prompt() {
    assert_eq!(
        parse(&["--append-system-prompt", "Additional context"]).append_system_prompt,
        Some(vec!["Additional context".to_string()])
    );
}

#[test]
fn parses_multiple_append_system_prompt_flags() {
    assert_eq!(
        parse(&[
            "--append-system-prompt",
            "Context A",
            "--append-system-prompt",
            "Context B"
        ])
        .append_system_prompt,
        Some(vec!["Context A".to_string(), "Context B".to_string()])
    );
}

#[test]
fn parses_mode() {
    assert_eq!(parse(&["--mode", "json"]).mode, Some(Mode::Json));
}

#[test]
fn parses_mode_rpc() {
    assert_eq!(parse(&["--mode", "rpc"]).mode, Some(Mode::Rpc));
}

#[test]
fn parses_session() {
    assert_eq!(
        parse(&["--session", "/path/to/session.jsonl"])
            .session
            .as_deref(),
        Some("/path/to/session.jsonl")
    );
}

#[test]
fn parses_session_id() {
    assert_eq!(
        parse(&["--session-id", "orchestrated-session"])
            .session_id
            .as_deref(),
        Some("orchestrated-session")
    );
}

#[test]
fn parses_fork() {
    let result = parse(&["--fork", "1234abcd"]);
    assert_eq!(result.fork.as_deref(), Some("1234abcd"));
    assert!(result.messages.is_empty());
}

#[test]
fn parses_export() {
    assert_eq!(
        parse(&["--export", "session.jsonl"]).export.as_deref(),
        Some("session.jsonl")
    );
}

#[test]
fn parses_thinking() {
    assert_eq!(
        parse(&["--thinking", "high"]).thinking.as_deref(),
        Some("high")
    );
}

#[test]
fn parses_models_as_comma_separated_list() {
    assert_eq!(
        parse(&["--models", "gpt-4o,claude-sonnet,gemini-pro"]).models,
        Some(vec![
            "gpt-4o".to_string(),
            "claude-sonnet".to_string(),
            "gemini-pro".to_string()
        ])
    );
}

// --- --name flag -----------------------------------------------------------

#[test]
fn parses_name_flag_with_value() {
    assert_eq!(
        parse(&["--name", "my-session"]).name.as_deref(),
        Some("my-session")
    );
}

#[test]
fn parses_n_shorthand() {
    assert_eq!(
        parse(&["-n", "quick-session"]).name.as_deref(),
        Some("quick-session")
    );
}

#[test]
fn preserves_empty_name_values_for_main_validation() {
    assert_eq!(parse(&["--name", ""]).name.as_deref(), Some(""));
}

#[test]
fn normalizes_display_names_and_rejects_whitespace_only() {
    assert_eq!(
        normalize_session_name("  named session  ").as_deref(),
        Some("named session")
    );
    assert_eq!(normalize_session_name("   "), None);
}

#[test]
fn reports_missing_name_value() {
    let result = parse(&["--name"]);
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].kind, DiagnosticKind::Error);
    assert_eq!(result.diagnostics[0].message, "--name requires a value");
}

#[test]
fn name_works_alongside_other_flags() {
    let result = parse(&[
        "--name",
        "named-run",
        "--print",
        "--model",
        "gpt-4o",
        "hello",
    ]);
    assert_eq!(result.name.as_deref(), Some("named-run"));
    assert_eq!(result.print, Some(true));
    assert_eq!(result.model.as_deref(), Some("gpt-4o"));
    assert_eq!(result.messages, vec!["hello".to_string()]);
}

// --- --no-session flag -----------------------------------------------------

#[test]
fn parses_no_session_flag() {
    assert_eq!(parse(&["--no-session"]).no_session, Some(true));
}

#[test]
fn preserves_custom_session_ids_for_non_persisting_commands() {
    let first = parse(&["--session-id", "ephemeral-id", "--help"]);
    assert_eq!(first.session_id.as_deref(), Some("ephemeral-id"));
    assert_eq!(first.help, Some(true));
    let second = parse(&["--session-id", "ephemeral-id", "--list-models"]);
    assert_eq!(second.session_id.as_deref(), Some("ephemeral-id"));
    assert_eq!(second.list_models, Some(ListModels::All));
    let third = parse(&["--session-id", "ephemeral-id", "--no-session"]);
    assert_eq!(third.session_id.as_deref(), Some("ephemeral-id"));
    assert_eq!(third.no_session, Some(true));
}

// --- --extension flag ------------------------------------------------------

#[test]
fn parses_single_extension() {
    assert_eq!(
        parse(&["--extension", "./my-extension.ts"]).extensions,
        Some(vec!["./my-extension.ts".to_string()])
    );
}

#[test]
fn parses_e_shorthand() {
    assert_eq!(
        parse(&["-e", "./my-extension.ts"]).extensions,
        Some(vec!["./my-extension.ts".to_string()])
    );
}

#[test]
fn parses_multiple_extension_flags() {
    assert_eq!(
        parse(&["--extension", "./ext1.ts", "-e", "./ext2.ts"]).extensions,
        Some(vec!["./ext1.ts".to_string(), "./ext2.ts".to_string()])
    );
}

#[test]
fn parses_no_extensions_flag() {
    assert_eq!(parse(&["--no-extensions"]).no_extensions, Some(true));
}

#[test]
fn parses_no_extensions_with_explicit_e_flags() {
    let result = parse(&["--no-extensions", "-e", "foo.ts", "-e", "bar.ts"]);
    assert_eq!(result.no_extensions, Some(true));
    assert_eq!(
        result.extensions,
        Some(vec!["foo.ts".to_string(), "bar.ts".to_string()])
    );
}

// --- --skill flag ----------------------------------------------------------

#[test]
fn parses_single_skill() {
    assert_eq!(
        parse(&["--skill", "./skill-dir"]).skills,
        Some(vec!["./skill-dir".to_string()])
    );
}

#[test]
fn parses_multiple_skill_flags() {
    assert_eq!(
        parse(&["--skill", "./skill-a", "--skill", "./skill-b"]).skills,
        Some(vec!["./skill-a".to_string(), "./skill-b".to_string()])
    );
}

// --- --prompt-template flag ------------------------------------------------

#[test]
fn parses_single_prompt_template() {
    assert_eq!(
        parse(&["--prompt-template", "./prompts"]).prompt_templates,
        Some(vec!["./prompts".to_string()])
    );
}

#[test]
fn parses_multiple_prompt_template_flags() {
    assert_eq!(
        parse(&["--prompt-template", "./one", "--prompt-template", "./two"]).prompt_templates,
        Some(vec!["./one".to_string(), "./two".to_string()])
    );
}

// --- --theme flag ----------------------------------------------------------

#[test]
fn parses_single_theme() {
    assert_eq!(
        parse(&["--theme", "./theme.json"]).themes,
        Some(vec!["./theme.json".to_string()])
    );
}

#[test]
fn parses_multiple_theme_flags() {
    assert_eq!(
        parse(&["--theme", "./dark.json", "--theme", "./light.json"]).themes,
        Some(vec!["./dark.json".to_string(), "./light.json".to_string()])
    );
}

#[test]
fn parses_use_theme() {
    assert_eq!(
        parse(&["--use-theme", "light"]).use_theme.as_deref(),
        Some("light")
    );
}

#[test]
fn reports_missing_use_theme_value() {
    let result = parse(&["--use-theme", "--print"]);
    assert_eq!(result.use_theme, None);
    assert_eq!(result.print, Some(true));
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(
        result.diagnostics[0].message,
        "--use-theme requires a theme name"
    );
}

// --- discovery-disable flags ----------------------------------------------

#[test]
fn parses_no_skills_flag() {
    assert_eq!(parse(&["--no-skills"]).no_skills, Some(true));
}

#[test]
fn parses_no_prompt_templates_flag() {
    assert_eq!(
        parse(&["--no-prompt-templates"]).no_prompt_templates,
        Some(true)
    );
}

#[test]
fn parses_no_themes_flag() {
    assert_eq!(parse(&["--no-themes"]).no_themes, Some(true));
}

#[test]
fn parses_no_context_files_flag() {
    assert_eq!(parse(&["--no-context-files"]).no_context_files, Some(true));
}

#[test]
fn parses_nc_shorthand() {
    assert_eq!(parse(&["-nc"]).no_context_files, Some(true));
}

// --- project approval flags ------------------------------------------------

#[test]
fn parses_approve() {
    assert_eq!(parse(&["--approve"]).project_trust_override, Some(true));
}

#[test]
fn parses_a_shorthand() {
    assert_eq!(parse(&["-a"]).project_trust_override, Some(true));
}

#[test]
fn parses_no_approve() {
    assert_eq!(parse(&["--no-approve"]).project_trust_override, Some(false));
}

#[test]
fn parses_na_shorthand() {
    assert_eq!(parse(&["-na"]).project_trust_override, Some(false));
}

#[test]
fn parses_verbose_flag() {
    assert_eq!(parse(&["--verbose"]).verbose, Some(true));
}

#[test]
fn parses_offline_flag() {
    assert_eq!(parse(&["--offline"]).offline, Some(true));
}

// --- --tui-mode flag -------------------------------------------------------

#[test]
fn parses_tui_mode_regular() {
    assert_eq!(
        parse(&["--tui-mode", "regular"]).tui_mode,
        Some(TuiMode::Regular)
    );
}

#[test]
fn parses_tui_mode_fullscreen() {
    assert_eq!(
        parse(&["--tui-mode", "fullscreen"]).tui_mode,
        Some(TuiMode::Fullscreen)
    );
}

#[test]
fn rejects_invalid_tui_mode() {
    let result = parse(&["--tui-mode", "other"]);
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(
        result.diagnostics[0].message,
        "Invalid TUI mode \"other\". Valid values: regular, fullscreen"
    );
}

#[test]
fn tui_mode_requires_a_mode() {
    let result = parse(&["--tui-mode"]);
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(
        result.diagnostics[0].message,
        "--tui-mode requires regular or fullscreen"
    );
}

#[test]
fn does_not_recognize_old_ui_mode_flag() {
    let result = parse(&["--ui-mode", "fullscreen"]);
    assert_eq!(result.tui_mode, None);
    assert_eq!(
        string_flag(&result, "ui-mode").as_deref(),
        Some("fullscreen")
    );
}

// --- tool flags ------------------------------------------------------------

#[test]
fn parses_no_tools_flag() {
    assert_eq!(parse(&["--no-tools"]).no_tools, Some(true));
}

#[test]
fn parses_nt_shorthand() {
    assert_eq!(parse(&["-nt"]).no_tools, Some(true));
}

#[test]
fn parses_no_builtin_tools_flag() {
    assert_eq!(parse(&["--no-builtin-tools"]).no_builtin_tools, Some(true));
}

#[test]
fn parses_nbt_shorthand() {
    assert_eq!(parse(&["-nbt"]).no_builtin_tools, Some(true));
}

#[test]
fn parses_tools_flag() {
    assert_eq!(
        parse(&["--tools", "read,bash"]).tools,
        Some(vec!["read".to_string(), "bash".to_string()])
    );
}

#[test]
fn parses_t_shorthand() {
    assert_eq!(
        parse(&["-t", "read,bash"]).tools,
        Some(vec!["read".to_string(), "bash".to_string()])
    );
}

#[test]
fn parses_exclude_tools_flag() {
    assert_eq!(
        parse(&["--exclude-tools", "read,bash"]).exclude_tools,
        Some(vec!["read".to_string(), "bash".to_string()])
    );
}

#[test]
fn parses_xt_shorthand() {
    assert_eq!(
        parse(&["-xt", "read,bash"]).exclude_tools,
        Some(vec!["read".to_string(), "bash".to_string()])
    );
}

#[test]
fn parses_no_tools_with_explicit_tools() {
    let result = parse(&["--no-tools", "--tools", "read,bash"]);
    assert_eq!(result.no_tools, Some(true));
    assert_eq!(
        result.tools,
        Some(vec!["read".to_string(), "bash".to_string()])
    );
}

#[test]
fn parses_no_builtin_tools_with_explicit_tools() {
    let result = parse(&["--no-builtin-tools", "--tools", "read,bash"]);
    assert_eq!(result.no_builtin_tools, Some(true));
    assert_eq!(
        result.tools,
        Some(vec!["read".to_string(), "bash".to_string()])
    );
}

// --- messages and file args ------------------------------------------------

#[test]
fn parses_plain_text_messages() {
    assert_eq!(
        parse(&["hello", "world"]).messages,
        vec!["hello".to_string(), "world".to_string()]
    );
}

#[test]
fn parses_file_arguments() {
    assert_eq!(
        parse(&["@README.md", "@src/main.ts"]).file_args,
        vec!["README.md".to_string(), "src/main.ts".to_string()]
    );
}

#[test]
fn parses_mixed_messages_and_file_args() {
    let result = parse(&["@file.txt", "explain this", "@image.png"]);
    assert_eq!(
        result.file_args,
        vec!["file.txt".to_string(), "image.png".to_string()]
    );
    assert_eq!(result.messages, vec!["explain this".to_string()]);
}

#[test]
fn captures_unknown_long_flags_with_string_values() {
    let result = parse(&["--unknown-flag", "message"]);
    assert!(result.messages.is_empty());
    assert_eq!(
        string_flag(&result, "unknown-flag").as_deref(),
        Some("message")
    );
}

#[test]
fn captures_unknown_boolean_long_flags() {
    assert_eq!(
        bool_flag(&parse(&["--unknown-flag"]), "unknown-flag"),
        Some(true)
    );
}

#[test]
fn captures_unknown_long_flags_with_equals_syntax() {
    let result = parse(&["--unknown-flag=value"]);
    assert_eq!(
        string_flag(&result, "unknown-flag").as_deref(),
        Some("value")
    );
}

#[test]
fn parses_multiple_flags_together() {
    let result = parse(&[
        "--provider",
        "anthropic",
        "--model",
        "claude-sonnet",
        "--print",
        "--thinking",
        "high",
        "@prompt.md",
        "Do the task",
    ]);
    assert_eq!(result.provider.as_deref(), Some("anthropic"));
    assert_eq!(result.model.as_deref(), Some("claude-sonnet"));
    assert_eq!(result.print, Some(true));
    assert_eq!(result.thinking.as_deref(), Some("high"));
    assert_eq!(result.file_args, vec!["prompt.md".to_string()]);
    assert_eq!(result.messages, vec!["Do the task".to_string()]);
}

#[test]
fn validates_thinking_levels() {
    assert!(is_valid_thinking_level("xhigh"));
    assert!(!is_valid_thinking_level("bogus"));
}

// --- help rendering --------------------------------------------------------

#[test]
fn help_includes_extension_flags_section() {
    use pillar_coding_agent::cli::help::render_help;
    use pillar_coding_agent::core::extensions_runner::ExtensionFlag;
    let flags = vec![(
        "plan".to_string(),
        ExtensionFlag {
            kind: "boolean",
            description: "Enable planning".to_string(),
        },
    )];
    let text = render_help(&flags);
    assert!(text.contains("pi - AI coding assistant"));
    assert!(text.contains("Extension CLI Flags:"));
    assert!(text.contains("--plan"));
    assert!(text.contains("Enable planning"));
    assert!(!text.contains('%'));
}

#[test]
fn help_without_extension_flags_has_no_section() {
    use pillar_coding_agent::cli::help::render_help;
    let text = render_help(&[]);
    assert!(!text.contains("Extension CLI Flags:"));
    assert!(text.contains("PI_CODING_AGENT_DIR"));
}
