//! Parity tests for resource-loader.ts (pi v0.84.3): project context file
//! discovery with worktree shadowing, resource path merging, skill
//! SKILL.md mapping, prompt/theme dedupe collisions, system prompt
//! discovery, and extension conflict detection.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pillar_coding_agent::core::package_manager::{PathMetadata, ResourceOrigin, SourceScope};
use pillar_coding_agent::core::resource_loader::{
    ResourceExtensionPaths, ResourceLoader, ResourceLoaderOptions, find_git_paths,
    load_project_context_files,
};
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-rloader-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_settings(cwd: &Path, agent_dir: &Path) -> Arc<Mutex<SettingsManager>> {
    let options = SettingsManagerCreateOptions {
        project_trusted: Some(true),
    };
    Arc::new(Mutex::new(SettingsManager::create(
        &cwd.to_string_lossy(),
        agent_dir,
        options,
    )))
}

// --- context files -----------------------------------------------------------------------

#[test]
fn context_files_global_then_ancestors_nearest_first() {
    let root = temp_dir("ctx");
    let deep = root.join("a").join("b");
    std::fs::create_dir_all(&deep).unwrap();
    let agent_dir = temp_dir("ctx-agent");

    std::fs::write(agent_dir.join("AGENTS.md"), "global").unwrap();
    std::fs::write(root.join("AGENTS.md"), "root").unwrap();
    std::fs::write(root.join("a").join("AGENTS.md"), "mid").unwrap();
    std::fs::write(deep.join("CLAUDE.md"), "deep").unwrap();

    let files = load_project_context_files(&deep.to_string_lossy(), &agent_dir.to_string_lossy());
    assert_eq!(files.len(), 4, "{files:?}");
    assert_eq!(files[0].1, "global");
    assert_eq!(files[1].1, "root");
    assert_eq!(files[2].1, "mid");
    assert_eq!(files[3].1, "deep");
    // AGENTS.override.md wins within a directory.
    std::fs::write(deep.join("AGENTS.override.md"), "override").unwrap();
    let files = load_project_context_files(&deep.to_string_lossy(), &agent_dir.to_string_lossy());
    assert_eq!(files[3].1, "override");
}

#[test]
fn context_files_dedupe_same_path_via_ancestors() {
    let root = temp_dir("ctx-dedupe");
    let agent_dir = temp_dir("ctx-dedupe-agent");
    std::fs::write(root.join("AGENTS.md"), "root").unwrap();

    let files = load_project_context_files(&root.to_string_lossy(), &agent_dir.to_string_lossy());
    assert_eq!(files.len(), 1);
}

#[test]
fn worktree_shadowing_hides_main_repo_context_file() {
    let main_repo = temp_dir("wt-main");
    std::fs::create_dir_all(main_repo.join(".git")).unwrap();
    std::fs::write(
        main_repo.join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .unwrap();
    std::fs::write(main_repo.join("AGENTS.md"), "main").unwrap();

    // Linked worktree: .git file pointing at main repo's .git/worktrees/x,
    // with commondir pointing back at main .git.
    let worktree = temp_dir("wt-linked");
    let wt_git_dir = main_repo.join(".git").join("worktrees").join("linked");
    std::fs::create_dir_all(&wt_git_dir).unwrap();
    std::fs::write(wt_git_dir.join("HEAD"), "ref: refs/heads/linked\n").unwrap();
    std::fs::write(wt_git_dir.join("commondir"), "../../../.git\n").unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", wt_git_dir.display()),
    )
    .unwrap();
    // The worktree has its own AGENTS.md, which shadows the main repo's.
    std::fs::write(worktree.join("AGENTS.md"), "worktree").unwrap();

    let git_paths = find_git_paths(&worktree).unwrap();
    assert_eq!(git_paths.repo_dir, worktree);
    assert_eq!(git_paths.common_git_dir, main_repo.join(".git"));

    let files = load_project_context_files(
        &worktree.to_string_lossy(),
        &temp_dir("wt-agent").to_string_lossy(),
    );
    // The main repo's AGENTS.md (an ancestor of the worktree) is shadowed.
    assert!(
        !files.iter().any(|(p, _)| p == &main_repo.join("AGENTS.md")),
        "{files:?}"
    );
    assert!(files.iter().any(|(_, c)| c == "worktree"), "{files:?}");
}

#[test]
fn ordinary_repo_has_no_shadowing() {
    let repo = temp_dir("ord-repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(repo.join("AGENTS.md"), "repo").unwrap();
    let nested = repo.join("sub");
    std::fs::create_dir_all(&nested).unwrap();

    let files = load_project_context_files(
        &nested.to_string_lossy(),
        &temp_dir("ord-agent").to_string_lossy(),
    );
    assert!(files.iter().any(|(_, c)| c == "repo"), "{files:?}");
}

// --- resource loader ------------------------------------------------------------------------

#[test]
fn loader_discovers_skills_prompts_themes_and_context() {
    let cwd = temp_dir("load-cwd");
    let agent_dir = temp_dir("load-agent");

    let skill_dir = cwd.join(".pillar").join("skills").join("my-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: A skill\n---\nbody",
    )
    .unwrap();

    let prompt_dir = cwd.join(".pillar").join("prompts");
    std::fs::create_dir_all(&prompt_dir).unwrap();
    std::fs::write(prompt_dir.join("cmd.md"), "# cmd\nrun it").unwrap();

    let theme_dir = agent_dir.join("themes");
    std::fs::create_dir_all(&theme_dir).unwrap();
    std::fs::write(theme_dir.join("dark.json"), r#"{"name": "dark"}"#).unwrap();

    let settings = make_settings(&cwd, &agent_dir);
    let mut loader = ResourceLoader::new(
        &cwd.to_string_lossy(),
        ResourceLoaderOptions {
            agent_dir: agent_dir.to_string_lossy().to_string(),
            ..Default::default()
        },
        settings,
    );
    loader.reload(None).unwrap();

    let snap = loader.snapshot();
    assert!(snap.skills.iter().any(|s| s.name == "my-skill"), "{snap:?}");
    // The skill's source info resolves as project-local.
    let skill = snap.skills.iter().find(|s| s.name == "my-skill").unwrap();
    // Auto-discovered project resources carry the "auto" source.
    assert_eq!(skill.source_info.source, "auto");

    assert!(snap.prompts.iter().any(|p| p.name == "cmd"), "{snap:?}");
    assert!(
        snap.themes
            .iter()
            .any(|t| t.name.as_deref() == Some("dark")),
        "{snap:?}"
    );
    // Dark theme came from the agent dir: user-local source info.
    let dark = snap
        .themes
        .iter()
        .find(|t| t.name.as_deref() == Some("dark"))
        .unwrap();
    assert_eq!(
        dark.source_info.scope,
        pillar_coding_agent::core::source_info::SourceScope::User
    );
}

#[test]
fn loader_context_files_and_system_prompt_discovery() {
    let cwd = temp_dir("sp-cwd");
    let agent_dir = temp_dir("sp-agent");
    std::fs::create_dir_all(cwd.join(".pillar")).unwrap();
    // Context files live at directory roots (not inside .pillar).
    std::fs::write(cwd.join("AGENTS.md"), "project context").unwrap();
    std::fs::write(cwd.join(".pillar").join("SYSTEM.md"), "project system prompt").unwrap();

    let settings = make_settings(&cwd, &agent_dir);
    let mut loader = ResourceLoader::new(
        &cwd.to_string_lossy(),
        ResourceLoaderOptions {
            agent_dir: agent_dir.to_string_lossy().to_string(),
            ..Default::default()
        },
        settings,
    );
    loader.reload(None).unwrap();

    let snap = loader.snapshot();
    assert!(
        snap.agents_files
            .iter()
            .any(|(_, c)| c == "project context"),
        "{snap:?}"
    );
    assert!(
        snap.system_prompt
            .as_deref()
            .is_some_and(|s| s.contains("project system prompt"))
    );
    let source = snap.system_prompt_source_path.as_ref().unwrap();
    assert_eq!(source, &cwd.join(".pillar").join("SYSTEM.md"));
}

#[test]
fn loader_explicit_system_prompt_takes_precedence() {
    let cwd = temp_dir("spexp-cwd");
    let agent_dir = temp_dir("spexp-agent");
    std::fs::create_dir_all(cwd.join(".pillar")).unwrap();
    std::fs::write(cwd.join(".pillar").join("SYSTEM.md"), "project").unwrap();
    let custom = temp_dir("spexp-custom");
    std::fs::write(custom.join("CUSTOM.md"), "custom prompt").unwrap();

    let settings = make_settings(&cwd, &agent_dir);
    let mut loader = ResourceLoader::new(
        &cwd.to_string_lossy(),
        ResourceLoaderOptions {
            agent_dir: agent_dir.to_string_lossy().to_string(),
            system_prompt: Some(custom.join("CUSTOM.md").to_string_lossy().to_string()),
            ..Default::default()
        },
        settings,
    );
    loader.reload(None).unwrap();
    let snap = loader.snapshot();
    assert!(
        snap.system_prompt
            .as_deref()
            .is_some_and(|s| s.contains("custom prompt")),
        "{snap:?}"
    );
    assert_eq!(
        snap.system_prompt_source_path.as_ref().unwrap(),
        &custom.join("CUSTOM.md")
    );
}

#[test]
fn loader_append_system_prompt_from_file() {
    let cwd = temp_dir("app-cwd");
    let agent_dir = temp_dir("app-agent");
    std::fs::write(agent_dir.join("APPEND_SYSTEM.md"), "append this").unwrap();

    let settings = make_settings(&cwd, &agent_dir);
    let mut loader = ResourceLoader::new(
        &cwd.to_string_lossy(),
        ResourceLoaderOptions {
            agent_dir: agent_dir.to_string_lossy().to_string(),
            ..Default::default()
        },
        settings,
    );
    loader.reload(None).unwrap();
    let snap = loader.snapshot();
    assert_eq!(snap.append_system_prompt, vec!["append this".to_string()]);
    assert_eq!(
        snap.append_system_prompt_source_paths,
        vec![agent_dir.join("APPEND_SYSTEM.md")]
    );
}

#[test]
fn loader_no_flags_disable_resource_kinds() {
    let cwd = temp_dir("noflags-cwd");
    let agent_dir = temp_dir("noflags-agent");
    let skill_dir = cwd.join(".pillar").join("skills").join("s");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: s\ndescription: d\n---\n",
    )
    .unwrap();

    let settings = make_settings(&cwd, &agent_dir);
    let mut loader = ResourceLoader::new(
        &cwd.to_string_lossy(),
        ResourceLoaderOptions {
            agent_dir: agent_dir.to_string_lossy().to_string(),
            no_skills: true,
            no_context_files: true,
            ..Default::default()
        },
        settings,
    );
    loader.reload(None).unwrap();
    let snap = loader.snapshot();
    assert!(snap.skills.is_empty(), "{snap:?}");
    assert!(snap.agents_files.is_empty(), "{snap:?}");
}

#[test]
fn loader_missing_additional_skill_path_diagnosed() {
    let cwd = temp_dir("miss-cwd");
    let agent_dir = temp_dir("miss-agent");
    let settings = make_settings(&cwd, &agent_dir);
    let mut loader = ResourceLoader::new(
        &cwd.to_string_lossy(),
        ResourceLoaderOptions {
            agent_dir: agent_dir.to_string_lossy().to_string(),
            additional_skill_paths: vec![
                cwd.join("nope")
                    .join("skill-dir")
                    .to_string_lossy()
                    .to_string(),
            ],
            ..Default::default()
        },
        settings,
    );
    loader.reload(None).unwrap();
    let snap = loader.snapshot();
    assert!(snap
        .skill_diagnostics
        .iter()
        .any(|d| matches!(d, pillar_coding_agent::core::skills::ResourceDiagnostic::Warning { path, .. }
            if path.ends_with("nope/skill-dir") || path.ends_with("nope\\skill-dir"))), "{snap:?}");
}

#[test]
fn loader_dedupe_prompts_and_themes_with_collisions() {
    let cwd = temp_dir("dup-cwd");
    let agent_dir = temp_dir("dup-agent");

    let prompt_dir = cwd.join(".pillar").join("prompts");
    std::fs::create_dir_all(&prompt_dir).unwrap();
    std::fs::write(
        prompt_dir.join("a.md"),
        "---\ndescription: first\n---\nfirst",
    )
    .unwrap();
    // Second dir with the same template name; loaded later, loses.
    let extra_dir = temp_dir("dup-extra");
    std::fs::write(
        extra_dir.join("a.md"),
        "---\ndescription: second\n---\nsecond",
    )
    .unwrap();

    let theme_dir = agent_dir.join("themes");
    std::fs::create_dir_all(&theme_dir).unwrap();
    std::fs::write(theme_dir.join("t1.json"), r#"{"name": "dup"}"#).unwrap();
    std::fs::write(theme_dir.join("t2.json"), r#"{"name": "dup"}"#).unwrap();

    let settings = make_settings(&cwd, &agent_dir);
    let mut loader = ResourceLoader::new(
        &cwd.to_string_lossy(),
        ResourceLoaderOptions {
            agent_dir: agent_dir.to_string_lossy().to_string(),
            additional_prompt_template_paths: vec![extra_dir.to_string_lossy().to_string()],
            ..Default::default()
        },
        settings,
    );
    loader.reload(None).unwrap();

    let snap = loader.snapshot();
    assert_eq!(snap.prompts.iter().filter(|p| p.name == "a").count(), 1);
    assert!(
        snap.prompt_diagnostics.iter().any(|d| matches!(
            d,
            pillar_coding_agent::core::skills::ResourceDiagnostic::Collision { message, .. }
                if message.contains("name \"/a\" collision")
        )),
        "{snap:?}"
    );

    assert_eq!(
        snap.themes
            .iter()
            .filter(|t| t.name.as_deref() == Some("dup"))
            .count(),
        1
    );
    assert!(
        snap.theme_diagnostics.iter().any(|d| matches!(
            d,
            pillar_coding_agent::core::skills::ResourceDiagnostic::Collision { message, .. }
                if message.contains("name \"dup\" collision")
        )),
        "{snap:?}"
    );
}

#[test]
fn loader_extend_resources_registers_extension_paths() {
    let cwd = temp_dir("ext-cwd");
    let agent_dir = temp_dir("ext-agent");
    let skill_file = temp_dir("ext-skill");
    std::fs::write(
        skill_file.join("SKILL.md"),
        "---\nname: ext-skill\ndescription: from ext\n---\n",
    )
    .unwrap();

    let settings = make_settings(&cwd, &agent_dir);
    let mut loader = ResourceLoader::new(
        &cwd.to_string_lossy(),
        ResourceLoaderOptions {
            agent_dir: agent_dir.to_string_lossy().to_string(),
            ..Default::default()
        },
        settings,
    );
    loader.reload(None).unwrap();

    let metadata = PathMetadata {
        source: "npm:ext-pkg".to_string(),
        scope: SourceScope::User,
        origin: ResourceOrigin::Package,
        base_dir: None,
    };
    loader.extend_resources(ResourceExtensionPaths {
        skill_paths: vec![(
            skill_file.join("SKILL.md").to_string_lossy().to_string(),
            metadata,
        )],
        prompt_paths: Vec::new(),
        theme_paths: Vec::new(),
    });

    let snap = loader.snapshot();
    let skill = snap.skills.iter().find(|s| s.name == "ext-skill").unwrap();
    assert_eq!(skill.source_info.source, "npm:ext-pkg");
    assert_eq!(
        skill.source_info.path,
        skill_file.join("SKILL.md").to_string_lossy()
    );
}

#[test]
fn loader_reload_honors_project_trust_callback() {
    let cwd = temp_dir("trust-cwd");
    let agent_dir = temp_dir("trust-agent");
    let settings = make_settings(&cwd, &agent_dir);
    let mut loader = ResourceLoader::new(
        &cwd.to_string_lossy(),
        ResourceLoaderOptions {
            agent_dir: agent_dir.to_string_lossy().to_string(),
            ..Default::default()
        },
        settings,
    );
    let mut called = false;
    loader
        .reload(Some(&mut || {
            called = true;
            true
        }))
        .unwrap();
    assert!(called);
    assert!(loader.is_loaded());
}

// --- conflicts -------------------------------------------------------------------------

#[test]
fn extension_conflict_detection() {
    let conflicts = ResourceLoader::detect_extension_conflicts(&[
        (
            "ext-a".to_string(),
            vec!["tool1".to_string()],
            vec!["flag1".to_string()],
        ),
        (
            "ext-b".to_string(),
            vec!["tool1".to_string()],
            vec!["flag2".to_string()],
        ),
        ("ext-c".to_string(), Vec::new(), vec!["flag1".to_string()]),
    ]);
    assert_eq!(conflicts.len(), 2);
    assert!(conflicts.iter().any(|(path, message)| path == "ext-b"
        && message.contains("Tool \"tool1\" conflicts with ext-a")));
    assert!(conflicts.iter().any(|(path, message)| path == "ext-c"
        && message.contains("Flag \"--flag1\" conflicts with ext-a")));

    // Same extension re-registering is not a conflict.
    let conflicts = ResourceLoader::detect_extension_conflicts(&[(
        "ext-a".to_string(),
        vec!["tool1".to_string(), "tool1".to_string()],
        Vec::new(),
    )]);
    assert!(conflicts.is_empty(), "{conflicts:?}");
}
