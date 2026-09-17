//! Port of packages/agent/test/harness/skills.test.ts and
//! resource-formatting.test.ts (pi v0.84.3).

#![cfg(feature = "harness-tools")]

use pillar_agent::harness::env::StdFsExecutionEnv;
use pillar_agent::harness::skills::{format_skill_invocation, load_skills, load_sourced_skills};
use pillar_agent::harness::types::{FileSystem, Skill};

fn temp_root(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "pillar-agent-skills-{tag}-{}",
        pillar_agent::harness::env::test_suffix()
    ));
    std::fs::create_dir_all(&dir).expect("create temp root");
    dir.to_string_lossy().into_owned()
}

async fn write_file(env: &StdFsExecutionEnv, path: &str, content: &str) {
    env.write_file(path, content.as_bytes())
        .await
        .expect("write file");
}

/// upstream test: "loads SKILL.md files through the execution environment"
#[tokio::test]
async fn loads_skill_md_files_through_the_execution_environment() {
    let root = temp_root("basic");
    let env = StdFsExecutionEnv::new(&root);
    env.create_dir(".agents/skills/example", true)
        .await
        .expect("create dir");
    write_file(
        &env,
        ".agents/skills/example/SKILL.md",
        "---\nname: example\ndescription: Example skill\ndisable-model-invocation: true\n---\nUse this skill.\n",
    )
    .await;

    let (skills, diagnostics) = load_skills(&env, &[".agents/skills"]).await;

    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    assert_eq!(
        skills,
        vec![Skill {
            name: "example".to_owned(),
            description: "Example skill".to_owned(),
            content: "Use this skill.".to_owned(),
            file_path: format!("{root}/.agents/skills/example/SKILL.md"),
            disable_model_invocation: true,
        }]
    );
}

/// upstream test: "loads skills through symlinked directories"
#[tokio::test]
async fn loads_skills_through_symlinked_directories() {
    let root = temp_root("symlink");
    let env = StdFsExecutionEnv::new(&root);
    env.create_dir("actual/example", true)
        .await
        .expect("create");
    write_file(
        &env,
        "actual/example/SKILL.md",
        "---\nname: example\ndescription: Example skill\n---\nUse this skill.",
    )
    .await;
    std::os::unix::fs::symlink(format!("{root}/actual"), format!("{root}/skills-link"))
        .expect("symlink");

    let (skills, _diagnostics) = load_skills(&env, &["skills-link"]).await;

    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["example"]);
    assert_eq!(
        skills[0].file_path,
        format!("{root}/skills-link/example/SKILL.md")
    );
}

/// upstream test: "preserves source info for sourced skills"
#[tokio::test]
async fn preserves_source_info_for_sourced_skills() {
    #[derive(Debug, Clone, PartialEq)]
    enum Source {
        User,
    }
    let root = temp_root("sourced");
    let env = StdFsExecutionEnv::new(&root);
    env.create_dir("user/example", true).await.expect("create");
    write_file(
        &env,
        "user/example/SKILL.md",
        "---\nname: example\ndescription: Example skill\n---\nUse this skill.",
    )
    .await;

    let (skills, diagnostics) =
        load_sourced_skills(&env, &[("user".to_owned(), Source::User)]).await;

    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    assert_eq!(skills.len(), 1);
    assert_eq!(
        skills[0].0,
        Skill {
            name: "example".to_owned(),
            description: "Example skill".to_owned(),
            content: "Use this skill.".to_owned(),
            file_path: format!("{root}/user/example/SKILL.md"),
            disable_model_invocation: false,
        }
    );
    assert_eq!(skills[0].1, Source::User);
}

/// upstream test: "attaches source info to diagnostics"
#[tokio::test]
async fn attaches_source_info_to_diagnostics() {
    #[derive(Debug, Clone, PartialEq)]
    enum Source {
        User,
    }
    let root = temp_root("sourced-diag");
    let env = StdFsExecutionEnv::new(&root);
    env.create_dir("user/broken", true).await.expect("create");
    write_file(
        &env,
        "user/broken/SKILL.md",
        "---\nname: broken\n---\nMissing description.",
    )
    .await;

    let (skills, diagnostics) =
        load_sourced_skills(&env, &[("user".to_owned(), Source::User)]).await;

    assert!(skills.is_empty());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].0.message, "description is required",
        "diagnostic: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics[0].0.path,
        format!("{root}/user/broken/SKILL.md")
    );
    assert_eq!(diagnostics[0].1, Source::User);
}

/// upstream test: "loads direct markdown children only from the root directory"
#[tokio::test]
async fn loads_direct_markdown_children_only_from_the_root_directory() {
    let root = temp_root("root-md");
    let env = StdFsExecutionEnv::new(&root);
    env.create_dir("skills/nested", true).await.expect("create");
    write_file(
        &env,
        "skills/root.md",
        "---\ndescription: Root skill\n---\nRoot content",
    )
    .await;
    write_file(
        &env,
        "skills/nested/ignored.md",
        "---\ndescription: Ignored\n---\nIgnored content",
    )
    .await;

    let (skills, _diagnostics) = load_skills(&env, &["skills"]).await;

    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["skills"]);
    assert_eq!(skills[0].content, "Root content");
}

/// upstream test: "ignores root markdown docs that do not declare skills"
#[tokio::test]
async fn ignores_root_markdown_docs_that_do_not_declare_skills() {
    let root = temp_root("docs");
    let env = StdFsExecutionEnv::new(&root);
    env.create_dir("skills/nested-skill", true)
        .await
        .expect("create");
    write_file(
        &env,
        "skills/README.md",
        "# Shared skills\n\nDocumentation.",
    )
    .await;
    write_file(&env, "skills/AGENTS.md", "# Agent notes\n\nDocumentation.").await;
    write_file(
        &env,
        "skills/CLAUDE.md",
        "---\ndescription: [invalid\n---\n\nDocumentation.",
    )
    .await;
    write_file(
        &env,
        "skills/root.md",
        "---\ndescription: Root skill\n---\nRoot content",
    )
    .await;
    write_file(
        &env,
        "skills/nested-skill/SKILL.md",
        "---\nname: nested-skill\ndescription: Nested skill\n---\nNested content",
    )
    .await;

    let (skills, diagnostics) = load_skills(&env, &["skills"]).await;

    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    let mut names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["nested-skill", "skills"]);
}

/// upstream resource-formatting.test.ts: "formats skill invocations with
/// additional instructions"
#[test]
fn formats_skill_invocations_with_additional_instructions() {
    let skill = Skill {
        name: "inspect".to_owned(),
        description: "Inspect things".to_owned(),
        content: "Use inspection tools.".to_owned(),
        file_path: "/project/.pi/skills/inspect/SKILL.md".to_owned(),
        disable_model_invocation: false,
    };

    assert_eq!(
        format_skill_invocation(&skill, Some("Check errors.")),
        "<skill name=\"inspect\" location=\"/project/.pi/skills/inspect/SKILL.md\">\nReferences are relative to /project/.pi/skills/inspect.\n\nUse inspection tools.\n</skill>\n\nCheck errors."
    );
}

/// upstream skills.test.ts does not cover the no-instructions variant
/// directly; keep the formatter's branch pinned.
#[test]
fn formats_skill_invocations_without_instructions() {
    let skill = Skill {
        name: "inspect".to_owned(),
        description: "Inspect things".to_owned(),
        content: "Use inspection tools.".to_owned(),
        file_path: "/project/.pi/skills/inspect/SKILL.md".to_owned(),
        disable_model_invocation: false,
    };
    assert_eq!(
        format_skill_invocation(&skill, None),
        "<skill name=\"inspect\" location=\"/project/.pi/skills/inspect/SKILL.md\">\nReferences are relative to /project/.pi/skills/inspect.\n\nUse inspection tools.\n</skill>"
    );
}

/// Filesystem trait reference to keep the generic loader exercised over
/// the composed env type.
#[allow(dead_code)]
fn _assert_env_composed(env: &StdFsExecutionEnv) -> &impl FileSystem {
    env
}
