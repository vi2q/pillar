//! Port of the upstream system-prompt tests (pi v0.84.3).

use pillar_coding_agent::core::system_prompt::{
    BuildSystemPromptOptions, ContextFile, PromptPaths, build_system_prompt,
};

fn options() -> BuildSystemPromptOptions {
    BuildSystemPromptOptions {
        cwd: "/tmp/project".to_string(),
        ..Default::default()
    }
}

// --- Empty tools -----------------------------------------------------------

#[test]
fn shows_none_for_empty_tools_list() {
    let mut options = options();
    options.selected_tools = Some(Vec::new());
    let prompt = build_system_prompt(&options);
    assert!(prompt.contains("Available tools:\n(none)"));
}

#[test]
fn shows_file_paths_guideline_even_with_no_tools() {
    let mut options = options();
    options.selected_tools = Some(Vec::new());
    let prompt = build_system_prompt(&options);
    assert!(prompt.contains("Show file paths clearly"));
}

// --- Default tools ---------------------------------------------------------

#[test]
fn includes_all_default_tools_when_snippets_are_provided() {
    let mut options = options();
    options.tool_snippets = Some(
        [
            ("read", "Read file contents"),
            ("bash", "Execute bash commands"),
            ("edit", "Make surgical edits"),
            ("write", "Create or overwrite files"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    );
    let prompt = build_system_prompt(&options);
    assert!(prompt.contains("- read:"));
    assert!(prompt.contains("- bash:"));
    assert!(prompt.contains("- edit:"));
    assert!(prompt.contains("- write:"));
}

#[test]
fn uses_shell_specific_guidance_for_powershell() {
    let mut options = options();
    options.selected_tools = Some(vec!["powershell".to_string()]);
    let prompt = build_system_prompt(&options);
    assert!(prompt.contains("Use PowerShell for file operations"));
}

#[test]
fn uses_shell_specific_guidance_for_bash_and_powershell() {
    let mut options = options();
    options.selected_tools = Some(vec!["bash".to_string(), "powershell".to_string()]);
    let prompt = build_system_prompt(&options);
    assert!(prompt.contains("Use bash or PowerShell for file operations"));
}

#[test]
fn instructs_models_to_resolve_pi_docs_under_absolute_base_paths() {
    let prompt = build_system_prompt(&options());
    assert!(prompt.contains(
        "- When reading pillar docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory"
    ));
    assert!(prompt.contains("environment variables (docs/environment-variables.md)"));
}

// --- Custom tool snippets --------------------------------------------------

#[test]
fn includes_custom_tools_in_available_tools_when_snippet_provided() {
    let mut options = options();
    options.selected_tools = Some(vec!["read".to_string(), "dynamic_tool".to_string()]);
    options.tool_snippets = Some(
        [("dynamic_tool", "Run dynamic test behavior")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    );
    let prompt = build_system_prompt(&options);
    assert!(prompt.contains("- dynamic_tool: Run dynamic test behavior"));
}

#[test]
fn omits_custom_tools_from_available_tools_when_snippet_not_provided() {
    let mut options = options();
    options.selected_tools = Some(vec!["read".to_string(), "dynamic_tool".to_string()]);
    let prompt = build_system_prompt(&options);
    assert!(!prompt.contains("dynamic_tool"));
}

// --- Prompt guidelines -----------------------------------------------------

#[test]
fn appends_prompt_guidelines_to_default_guidelines() {
    let mut options = options();
    options.selected_tools = Some(vec!["read".to_string(), "dynamic_tool".to_string()]);
    options.prompt_guidelines = Some(vec!["Use dynamic_tool for project summaries.".to_string()]);
    let prompt = build_system_prompt(&options);
    assert!(prompt.contains("- Use dynamic_tool for project summaries."));
}

#[test]
fn deduplicates_and_trims_prompt_guidelines() {
    let mut options = options();
    options.selected_tools = Some(vec!["read".to_string(), "dynamic_tool".to_string()]);
    options.prompt_guidelines = Some(vec![
        "Use dynamic_tool for summaries.".to_string(),
        "  Use dynamic_tool for summaries.  ".to_string(),
        "   ".to_string(),
    ]);
    let prompt = build_system_prompt(&options);
    assert_eq!(
        prompt.matches("- Use dynamic_tool for summaries.").count(),
        1
    );
}

// --- Custom prompt / context / skills ---------------------------------------

#[test]
fn custom_prompt_replaces_the_default_body_but_keeps_cwd_line() {
    let mut options = options();
    options.custom_prompt = Some("You are a custom assistant.".to_string());
    let prompt = build_system_prompt(&options);
    assert!(prompt.starts_with("You are a custom assistant."));
    assert!(prompt.contains("Current working directory: /tmp/project"));
    assert!(!prompt.contains("Available tools:"));
}

#[test]
fn appends_project_context_files() {
    let mut options = options();
    options.context_files = Some(vec![ContextFile {
        path: "AGENTS.md".to_string(),
        content: "Be excellent.".to_string(),
    }]);
    let prompt = build_system_prompt(&options);
    assert!(prompt.contains("<project_context>"));
    assert!(prompt.contains("<project_instructions path=\"AGENTS.md\">"));
    assert!(prompt.contains("Be excellent."));
    assert!(prompt.contains("</project_context>"));
}

#[test]
fn appends_skills_section_with_xml_format() {
    use pillar_coding_agent::core::system_prompt::Skill;
    let mut options = options();
    options.selected_tools = Some(vec!["read".to_string()]);
    options.skills = Some(vec![Skill {
        name: "no-ai-prose".to_string(),
        description: "Write copy that does not read like AI prose".to_string(),
        file_path: "/skills/no-ai-prose/SKILL.md".to_string(),
        base_dir: "/skills/no-ai-prose".to_string(),
        disable_model_invocation: false,
    }]);
    let prompt = build_system_prompt(&options);
    assert!(prompt.contains("<available_skills>"));
    assert!(prompt.contains("<name>no-ai-prose</name>"));
    assert!(prompt.contains("<location>/skills/no-ai-prose/SKILL.md</location>"));
    assert!(prompt.contains("</available_skills>"));
}

#[test]
fn omits_disabled_skills_from_the_prompt() {
    use pillar_coding_agent::core::system_prompt::Skill;
    let mut options = options();
    options.selected_tools = Some(vec!["read".to_string()]);
    options.skills = Some(vec![Skill {
        name: "hidden".to_string(),
        description: "Only invocable via command".to_string(),
        file_path: "/skills/hidden/SKILL.md".to_string(),
        base_dir: "/skills/hidden".to_string(),
        disable_model_invocation: true,
    }]);
    let prompt = build_system_prompt(&options);
    assert!(!prompt.contains("<available_skills>"));
}

// --- Frontmatter parsing -----------------------------------------------------

#[test]
fn parses_skill_frontmatter_fields() {
    use pillar_coding_agent::core::system_prompt::{
        parse_skill_frontmatter, validate_skill_description, validate_skill_name,
    };
    let (frontmatter, body) = parse_skill_frontmatter(
        "---\nname: my-skill\ndescription: Does a thing\n---\n\n# Body here\n",
    );
    assert_eq!(frontmatter.name.as_deref(), Some("my-skill"));
    assert_eq!(frontmatter.description.as_deref(), Some("Does a thing"));
    assert!(!frontmatter.disable_model_invocation);
    assert_eq!(body.trim_start(), "# Body here");

    assert!(validate_skill_name("my-skill").is_empty());
    assert_eq!(validate_skill_name("My Skill").len(), 1); // charset violation only
    assert!(
        validate_skill_name("-lead")
            .contains(&"name must not start or end with a hyphen".to_string())
    );
    assert!(
        validate_skill_name("a--b")
            .contains(&"name must not contain consecutive hyphens".to_string())
    );
    assert!(validate_skill_description(Some("ok")).is_empty());
    assert!(validate_skill_description(Some("")).contains(&"description is required".to_string()));
    assert!(validate_skill_description(None).contains(&"description is required".to_string()));
}

// --- Prompt paths (divergence: host-provided) --------------------------------

#[test]
fn prompt_paths_appear_in_the_default_prompt() {
    let mut options = options();
    options.paths = PromptPaths {
        readme_path: "/custom/README.md".to_string(),
        docs_path: "/custom/docs".to_string(),
        examples_path: "/custom/examples".to_string(),
    };
    let prompt = build_system_prompt(&options);
    assert!(prompt.contains("- Main documentation: /custom/README.md"));
    assert!(prompt.contains("- Additional docs: /custom/docs"));
    assert!(prompt.contains("- Examples: /custom/examples"));
}
