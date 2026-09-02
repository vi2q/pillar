//! Port of the upstream skills tests (pi v0.84.3, skills.test.ts): fixture
//! fixtures are recreated inline since upstream reads its test/fixtures tree.

use std::path::{Path, PathBuf};

use pillar_coding_agent::core::skills::{
    LoadSkillsFromDirOptions, ResourceDiagnostic, load_skills_from_dir,
};
use pillar_coding_agent::core::system_prompt::{Skill, format_skills_for_prompt};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pillar-coding-agent-skills-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_skill(dir: &Path, filename: &str, content: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("create skill dir");
    let path = dir.join(filename);
    std::fs::write(&path, content).expect("write SKILL.md");
    path
}

fn load(dir: &Path) -> (Vec<Skill>, Vec<ResourceDiagnostic>) {
    let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
        dir,
        source: "test",
    });
    (result.skills, result.diagnostics)
}

#[test]
fn loads_a_valid_skill() {
    let dir = temp_dir("valid");
    write_skill(
        &dir,
        "SKILL.md",
        "---\nname: valid-skill\ndescription: A valid skill for testing purposes.\n---\n\n# Body\n",
    );
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "valid-skill");
    assert_eq!(skills[0].description, "A valid skill for testing purposes.");
    assert_eq!(skills[0].base_dir, dir.to_string_lossy());
    assert!(diagnostics.is_empty());
}

#[test]
fn allows_names_that_do_not_match_parent_directory() {
    let dir = temp_dir("name-mismatch");
    write_skill(
        &dir,
        "SKILL.md",
        "---\nname: different-name\ndescription: ok\n---\n",
    );
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "different-name");
    assert!(!diagnostics.iter().any(|d| match d {
        ResourceDiagnostic::Warning { message, .. } => message.contains("does not match parent"),
        _ => false,
    }));
}

#[test]
fn warns_when_name_contains_invalid_characters() {
    let dir = temp_dir("invalid-name-chars");
    write_skill(
        &dir,
        "SKILL.md",
        "---\nname: Invalid Name\ndescription: ok\n---\n",
    );
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 1);
    assert!(diagnostics.iter().any(|d| match d {
        ResourceDiagnostic::Warning { message, .. } => message.contains("invalid characters"),
        _ => false,
    }));
}

#[test]
fn warns_when_name_exceeds_64_characters() {
    let dir = temp_dir("long-name");
    let long_name = "a".repeat(70);
    write_skill(
        &dir,
        "SKILL.md",
        &format!("---\nname: {long_name}\ndescription: ok\n---\n"),
    );
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 1);
    assert!(diagnostics.iter().any(|d| match d {
        ResourceDiagnostic::Warning { message, .. } =>
            message.contains("name exceeds 64 characters (70)"),
        _ => false,
    }));
}

#[test]
fn warns_and_skips_skill_when_description_is_missing() {
    let dir = temp_dir("missing-description");
    write_skill(&dir, "SKILL.md", "---\nname: no-desc\n---\n");
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 0);
    assert!(diagnostics.iter().any(|d| match d {
        ResourceDiagnostic::Warning { message, .. } => message.contains("description is required"),
        _ => false,
    }));
}

#[test]
fn ignores_unknown_frontmatter_fields() {
    let dir = temp_dir("unknown-field");
    write_skill(
        &dir,
        "SKILL.md",
        "---\nname: unknown-field\ndescription: A skill with an unknown frontmatter field.\nauthor: someone\nversion: 1.0\n---\n",
    );
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "unknown-field");
    assert!(!diagnostics.iter().any(|d| match d {
        ResourceDiagnostic::Warning { message, .. } =>
            message.contains("unknown frontmatter field"),
        _ => false,
    }));
}

#[test]
fn loads_nested_skills_recursively() {
    let dir = temp_dir("nested");
    write_skill(
        &dir.join("child-skill"),
        "SKILL.md",
        "---\nname: child-skill\ndescription: A nested skill in a subdirectory.\n---\n",
    );
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "child-skill");
    assert!(diagnostics.is_empty());
}

#[test]
fn prefers_a_directory_root_skill_md_over_nested_files() {
    let dir = temp_dir("root-skill-preferred");
    write_skill(
        &dir,
        "SKILL.md",
        "---\ndescription: Root skill should win.\n---\n",
    );
    write_skill(
        &dir.join("nested-child"),
        "SKILL.md",
        "---\nname: nested-child\ndescription: Nested should lose.\n---\n",
    );
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 1);
    // Name falls back to the parent directory's own basename.
    let expected_name = dir.file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(skills[0].name, expected_name);
    assert_eq!(skills[0].description, "Root skill should win.");
    assert!(diagnostics.is_empty());
}

#[test]
fn skips_files_without_frontmatter_description() {
    let dir = temp_dir("no-frontmatter");
    write_skill(&dir, "SKILL.md", "Just some text without frontmatter.\n");
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 0);
    assert!(diagnostics.iter().any(|d| match d {
        ResourceDiagnostic::Warning { message, .. } => message.contains("description is required"),
        _ => false,
    }));
}

#[test]
fn warns_and_skips_skill_when_yaml_frontmatter_is_invalid() {
    let dir = temp_dir("invalid-yaml");
    write_skill(
        &dir,
        "SKILL.md",
        "---\nname: invalid-yaml\ndescription: [unclosed bracket\n---\n",
    );
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 0);
    assert!(diagnostics.iter().any(|d| match d {
        ResourceDiagnostic::Warning { message, .. } => message.contains("at line"),
        _ => false,
    }));
}

#[test]
fn preserves_multiline_descriptions_from_yaml() {
    let dir = temp_dir("multiline-description");
    write_skill(
        &dir,
        "SKILL.md",
        "---\nname: multiline-description\ndescription: |\n  This is a multiline description.\n  It spans multiple lines.\n---\n",
    );
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 1);
    assert!(skills[0].description.contains('\n'));
    assert!(
        skills[0]
            .description
            .contains("This is a multiline description.")
    );
    assert!(diagnostics.is_empty());
}

#[test]
fn warns_when_name_contains_consecutive_hyphens() {
    let dir = temp_dir("consecutive-hyphens");
    write_skill(&dir, "SKILL.md", "---\nname: a--b\ndescription: ok\n---\n");
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 1);
    assert!(diagnostics.iter().any(|d| match d {
        ResourceDiagnostic::Warning { message, .. } => message.contains("consecutive hyphens"),
        _ => false,
    }));
}

#[test]
fn loads_all_skills_from_a_fixture_directory() {
    let root = temp_dir("fixtures");
    write_skill(
        &root.join("valid-skill"),
        "SKILL.md",
        "---\nname: valid-skill\ndescription: A valid skill for testing purposes.\n---\n",
    );
    write_skill(
        &root.join("name-mismatch"),
        "SKILL.md",
        "---\nname: different-name\ndescription: ok\n---\n",
    );
    write_skill(
        &root.join("missing-description"),
        "SKILL.md",
        "---\nname: no-desc\n---\n",
    );
    write_skill(&root.join("no-frontmatter"), "SKILL.md", "text only\n");
    write_skill(
        &root.join("nested"),
        "SKILL.md",
        "---\nname: nested-root\ndescription: root wins\n---\n",
    );
    write_skill(
        &root.join("nested").join("child-skill"),
        "SKILL.md",
        "---\nname: child-skill\ndescription: child\n---\n",
    );

    let (skills, _diagnostics) = load(&root);
    // Valid ones plus the nested root (child is skipped: nested root wins).
    assert!(skills.len() >= 3, "got {}", skills.len());
}

#[test]
fn returns_empty_for_non_existent_directory() {
    let (skills, diagnostics) = load(Path::new("/non/existent/path"));
    assert_eq!(skills.len(), 0);
    assert!(diagnostics.is_empty());
}

#[test]
fn uses_parent_directory_name_when_name_not_in_frontmatter() {
    let dir = temp_dir("valid-skill");
    write_skill(
        &dir,
        "SKILL.md",
        "---\ndescription: A valid skill for testing purposes.\n---\n",
    );
    let (skills, _) = load(&dir);
    assert_eq!(skills.len(), 1);
    let expected_name = dir.file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(skills[0].name, expected_name);
}

#[test]
fn parses_disable_model_invocation_frontmatter_field() {
    let dir = temp_dir("disable-model-invocation");
    write_skill(
        &dir,
        "SKILL.md",
        "---\nname: disable-model-invocation\ndescription: hidden from prompt\ndisable-model-invocation: true\n---\n",
    );
    let (skills, diagnostics) = load(&dir);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "disable-model-invocation");
    assert!(skills[0].disable_model_invocation);
    assert!(!diagnostics.iter().any(|d| match d {
        ResourceDiagnostic::Warning { message, .. } =>
            message.contains("unknown frontmatter field"),
        _ => false,
    }));
}

#[test]
fn defaults_disable_model_invocation_to_false() {
    let dir = temp_dir("valid-skill2");
    write_skill(
        &dir,
        "SKILL.md",
        "---\nname: valid-skill\ndescription: A valid skill for testing purposes.\n---\n",
    );
    let (skills, _) = load(&dir);
    assert_eq!(skills.len(), 1);
    assert!(!skills[0].disable_model_invocation);
}

// --- formatSkillsForPrompt ---------------------------------------------------

#[test]
fn returns_empty_string_for_no_skills() {
    assert_eq!(format_skills_for_prompt(&[]), "");
}

#[test]
fn formats_skills_as_xml_with_intro_text() {
    let skills = vec![Skill {
        name: "my-skill".to_string(),
        description: "Does a thing".to_string(),
        file_path: "/skills/my-skill/SKILL.md".to_string(),
        base_dir: "/skills/my-skill".to_string(),
        disable_model_invocation: false,
    }];
    let result = format_skills_for_prompt(&skills);
    assert!(result.contains("The following skills provide specialized instructions"));
    assert!(result.contains("<available_skills>"));
    assert!(result.contains("<name>my-skill</name>"));
    assert!(result.contains("<description>Does a thing</description>"));
    assert!(result.contains("<location>/skills/my-skill/SKILL.md</location>"));
    assert!(result.contains("</available_skills>"));
}

#[test]
fn escapes_xml_special_characters() {
    let skills = vec![Skill {
        name: "a<b&c>d".to_string(),
        description: "uses <tags> & \"quotes\"".to_string(),
        file_path: "/x".to_string(),
        base_dir: "/x".to_string(),
        disable_model_invocation: false,
    }];
    let result = format_skills_for_prompt(&skills);
    assert!(result.contains("&lt;tags&gt; &amp; &quot;quotes&quot;"));
    assert!(result.contains("a&lt;b&amp;c&gt;d"));
}

#[test]
fn excludes_disabled_skills_from_the_prompt() {
    let skills = vec![
        Skill {
            name: "visible".to_string(),
            description: "ok".to_string(),
            file_path: "/v".to_string(),
            base_dir: "/v".to_string(),
            disable_model_invocation: false,
        },
        Skill {
            name: "hidden".to_string(),
            description: "no".to_string(),
            file_path: "/h".to_string(),
            base_dir: "/h".to_string(),
            disable_model_invocation: true,
        },
    ];
    let result = format_skills_for_prompt(&skills);
    assert!(result.contains("visible"));
    assert!(!result.contains("hidden"));
}

#[test]
fn returns_empty_string_when_all_skills_have_disable_model_invocation() {
    let skills = vec![Skill {
        name: "hidden".to_string(),
        description: "no".to_string(),
        file_path: "/h".to_string(),
        base_dir: "/h".to_string(),
        disable_model_invocation: true,
    }];
    assert_eq!(format_skills_for_prompt(&skills), "");
}
