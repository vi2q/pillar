//! Port of the upstream prompt-templates behavior (pi v0.84.3) exercised via
//! unit tests: bash-style arg parsing, placeholder substitution (positional,
//! $@/$ARGUMENTS, ${N:-default}, ${@:N}, ${@:N:L}), template loading from
//! directories, frontmatter parsing, and /name expansion.

use pillar_coding_agent::core::prompt_templates::{
    LoadPromptTemplatesOptions, PromptTemplate, expand_prompt_template, load_prompt_templates,
    parse_command_args, substitute_args,
};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pillar-coding-agent-prompt-templates-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_file(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write template file");
    path
}

// --- parse_command_args ----------------------------------------------------

#[test]
fn parse_command_args_splits_on_whitespace() {
    assert_eq!(
        parse_command_args("hello world"),
        vec!["hello".to_string(), "world".to_string()]
    );
}

#[test]
fn parse_command_args_respects_double_quotes() {
    assert_eq!(
        parse_command_args(r#"say "hello world" twice"#),
        vec![
            "say".to_string(),
            "hello world".to_string(),
            "twice".to_string()
        ]
    );
}

#[test]
fn parse_command_args_respects_single_quotes() {
    assert_eq!(
        parse_command_args("say 'hello world'"),
        vec!["say".to_string(), "hello world".to_string()]
    );
}

#[test]
fn parse_command_args_empty_and_whitespace_only() {
    assert_eq!(parse_command_args(""), Vec::<String>::new());
    assert_eq!(parse_command_args("   "), Vec::<String>::new());
}

#[test]
fn parse_command_args_unterminated_quote_consumes_rest() {
    assert_eq!(
        parse_command_args("say \"hello world"),
        vec!["say".to_string(), "hello world".to_string()]
    );
}

// --- substitute_args -------------------------------------------------------

#[test]
fn substitutes_positional_args() {
    assert_eq!(
        substitute_args("first $1 second $2", &["a".to_string(), "b".to_string()]),
        "first a second b"
    );
}

#[test]
fn substitutes_all_args_with_at_and_arguments() {
    assert_eq!(
        substitute_args(
            "all: $@ args: $ARGUMENTS",
            &["a".to_string(), "b".to_string()]
        ),
        "all: a b args: a b"
    );
}

#[test]
fn substitutes_braced_default_when_arg_missing() {
    assert_eq!(
        substitute_args("value: ${1:-fallback}", &[]),
        "value: fallback"
    );
    assert_eq!(
        substitute_args("value: ${1:-fallback}", &["present".to_string()]),
        "value: present"
    );
}

#[test]
fn substitutes_all_args_with_default() {
    assert_eq!(substitute_args("all: ${@:-none}", &[]), "all: none");
    assert_eq!(
        substitute_args("all: ${ARGUMENTS:-none}", &["x".to_string()]),
        "all: x"
    );
}

#[test]
fn substitutes_bash_style_slicing() {
    assert_eq!(
        substitute_args(
            "from 2nd: ${@:2}",
            &["a".to_string(), "b".to_string(), "c".to_string()]
        ),
        "from 2nd: b c"
    );
    assert_eq!(
        substitute_args(
            "two from 2nd: ${@:2:2}",
            &[
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string()
            ]
        ),
        "two from 2nd: b c"
    );
}

#[test]
fn missing_positional_arg_substitutes_empty() {
    assert_eq!(
        substitute_args("missing: $5", &["a".to_string()]),
        "missing: "
    );
}

#[test]
fn does_not_recursively_substitute_argument_values() {
    // Argument value contains a pattern; must NOT be recursively substituted.
    assert_eq!(
        substitute_args("value: $1", &["$2".to_string()]),
        "value: $2"
    );
}

// --- load_prompt_templates -------------------------------------------------

#[test]
fn loads_templates_from_global_and_project_dirs() {
    let global = temp_dir("global");
    let project = temp_dir("project");
    // Loader reads agentDir/prompts and cwd/.pillar/prompts.
    let global_prompts = global.join("prompts");
    let project_prompts = project.join(".pillar").join("prompts");
    std::fs::create_dir_all(&global_prompts).expect("create global prompts dir");
    std::fs::create_dir_all(&project_prompts).expect("create project prompts dir");
    write_file(
        &global_prompts,
        "deploy.md",
        "---\ndescription: Deploy to prod\n---\nDeploy body",
    );
    write_file(
        &project_prompts,
        "review.md",
        "---\ndescription: Review code\nargument-hint: [file]\n---\nReview body",
    );

    let templates = load_prompt_templates(&LoadPromptTemplatesOptions {
        cwd: project.to_string_lossy().to_string(),
        agent_dir: global.to_string_lossy().to_string(),
        prompt_paths: Vec::new(),
        include_defaults: true,
    });

    assert_eq!(templates.len(), 2);
    assert_eq!(templates[0].name, "deploy");
    assert_eq!(templates[0].description, "Deploy to prod");
    assert_eq!(templates[0].content, "Deploy body");
    assert_eq!(templates[1].name, "review");
    assert_eq!(templates[1].description, "Review code");
    assert_eq!(templates[1].argument_hint.as_deref(), Some("[file]"));
}

#[test]
fn include_defaults_false_skips_default_directories() {
    let global = temp_dir("global2");
    let project = temp_dir("project2");
    let global_prompts = global.join("prompts");
    let project_prompts = project.join(".pillar").join("prompts");
    std::fs::create_dir_all(&global_prompts).expect("create global prompts dir");
    std::fs::create_dir_all(&project_prompts).expect("create project prompts dir");
    write_file(&global_prompts, "deploy.md", "Deploy body");
    write_file(&project_prompts, "review.md", "Review body");

    let templates = load_prompt_templates(&LoadPromptTemplatesOptions {
        cwd: project.to_string_lossy().to_string(),
        agent_dir: global.to_string_lossy().to_string(),
        prompt_paths: Vec::new(),
        include_defaults: false,
    });

    assert!(templates.is_empty());
}

#[test]
fn loads_explicit_paths_files_and_directories() {
    let root = temp_dir("explicit");
    let subdir = root.join("extra");
    std::fs::create_dir_all(&subdir).expect("create subdir");
    write_file(&root, "one.md", "One body");
    write_file(&subdir, "two.md", "Two body");
    let missing = root.join("missing.md");

    let templates = load_prompt_templates(&LoadPromptTemplatesOptions {
        cwd: root.to_string_lossy().to_string(),
        agent_dir: root.to_string_lossy().to_string(),
        prompt_paths: vec![
            root.join("one.md").to_string_lossy().to_string(),
            subdir.to_string_lossy().to_string(),
            missing.to_string_lossy().to_string(), // nonexistent: skipped
            root.join("notes.txt").to_string_lossy().to_string(), // non-md file: skipped
        ],
        include_defaults: false,
    });

    let names: Vec<String> = templates.iter().map(|t| t.name.clone()).collect();
    assert!(names.iter().any(|n| n == "one"), "explicit file loaded");
    assert!(names.iter().any(|n| n == "two"), "directory .md loaded");
    assert!(
        !names.iter().any(|n| n == "missing"),
        "missing file skipped"
    );
    assert!(!names.iter().any(|n| n == "notes"), "non-md file skipped");
}

#[test]
fn load_template_from_file_falls_back_to_first_body_line_for_description() {
    let dir = temp_dir("fallback");
    let path = write_file(&dir, "longdesc.md", "First line of the body.\nSecond line.");

    let template =
        pillar_coding_agent::core::prompt_templates::load_template_from_file_for_test(&path);

    assert_eq!(template.as_ref().unwrap().name, "longdesc");
    assert_eq!(
        template.as_ref().unwrap().description,
        "First line of the body."
    );
    assert_eq!(
        template.as_ref().unwrap().content,
        "First line of the body.\nSecond line."
    );
}

// --- expand_prompt_template ------------------------------------------------

#[test]
fn expands_template_by_name_with_args() {
    let templates = vec![PromptTemplate {
        name: "deploy".to_string(),
        description: String::new(),
        argument_hint: None,
        content: "Deploy $1 to $2".to_string(),
        file_path: String::new(),
    }];

    assert_eq!(
        expand_prompt_template("/deploy prod now", &templates),
        "Deploy prod to now"
    );
}

#[test]
fn returns_original_text_when_not_a_template() {
    let templates = vec![PromptTemplate {
        name: "deploy".to_string(),
        description: String::new(),
        argument_hint: None,
        content: "Deploy $1".to_string(),
        file_path: String::new(),
    }];

    assert_eq!(
        expand_prompt_template("plain text", &templates),
        "plain text"
    );
    assert_eq!(
        expand_prompt_template("/unknown x", &templates),
        "/unknown x"
    );
}

#[test]
fn substitutes_arguments_and_all_args_in_expansion() {
    let templates = vec![PromptTemplate {
        name: "review".to_string(),
        description: String::new(),
        argument_hint: Some("[file]".to_string()),
        content: "Review $1: $@ (total $ARGUMENTS)".to_string(),
        file_path: String::new(),
    }];

    assert_eq!(
        expand_prompt_template("/review main.rs fix tests", &templates),
        "Review main.rs: main.rs fix tests (total main.rs fix tests)"
    );
}
