//! Parity tests for package-manager.ts (pi v0.84.3): source parsing
//! (npm spec / git URL / local path), pattern filters, precedence ranks,
//! install-path computation, resource collection, and resolve ordering.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pillar_coding_agent::core::package_manager::{
    DefaultPackageManager, GitSource, MissingSourceAction, NpmSource, ParsedSource, ProgressAction,
    ProgressKind, ResourceOrigin, ResourceType, SourceScope, apply_autoload_disabled_patterns,
    apply_patterns, collect_ancestor_agents_skill_dirs, collect_auto_extension_entries,
    collect_auto_skill_entries, get_extension_temp_folder, is_enabled_by_overrides, is_local_path,
    matches_any_exact_pattern, matches_any_pattern, package_filter_of, package_source_string,
    parse_git_url, parse_npm_source, parse_npm_spec, parse_source, resource_precedence_rank,
};
use pillar_coding_agent::core::effects::{EffectDecision, EffectIntent, allow_all};
use pillar_coding_agent::core::settings_manager::{
    PackageSource, SettingsManager, SettingsManagerCreateOptions,
};

fn npm(name: &str, version: Option<&str>) -> NpmSource {
    let mut source = parse_npm_source(&match version {
        Some(v) => format!("{name}@{v}"),
        None => name.to_string(),
    });
    source.name = name.to_string();
    source
}

fn git(host: &str, path: &str, ref_: Option<&str>) -> GitSource {
    GitSource {
        repo: format!("https://{host}/{path}"),
        host: host.to_string(),
        path: path.to_string(),
        ref_: ref_.map(str::to_string),
        pinned: ref_.is_some(),
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-pkgm-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_manager(cwd: &Path, agent_dir: &Path) -> Arc<Mutex<SettingsManager>> {
    let options = SettingsManagerCreateOptions {
        project_trusted: Some(true),
    };
    let manager = SettingsManager::create(&cwd.to_string_lossy(), agent_dir, options);
    Arc::new(Mutex::new(manager))
}

// --- npm spec parsing -------------------------------------------------------------

#[test]
fn parse_npm_spec_name_and_version() {
    assert_eq!(parse_npm_spec("foo"), ("foo".to_string(), None));
    assert_eq!(
        parse_npm_spec("foo@1.2.3"),
        ("foo".to_string(), Some("1.2.3".to_string()))
    );
    assert_eq!(
        parse_npm_spec("@scope/pkg@^2.0.0"),
        ("@scope/pkg".to_string(), Some("^2.0.0".to_string()))
    );
    // Scoped names without version: only the first @ after position 0 splits.
    assert_eq!(
        parse_npm_spec("@scope/pkg"),
        ("@scope/pkg".to_string(), None)
    );
}

#[test]
fn parse_npm_source_pinned_and_range() {
    let pinned = parse_npm_source("foo@1.2.3");
    assert!(pinned.pinned);
    assert_eq!(pinned.name, "foo");
    assert_eq!(pinned.version.as_deref(), Some("1.2.3"));
    assert_eq!(pinned.range.as_deref(), Some("1.2.3"));

    let ranged = parse_npm_source("foo@^1.0.0");
    assert!(!ranged.pinned);
    assert_eq!(ranged.range.as_deref(), Some("^1.0.0"));

    let unpinned = parse_npm_source("foo");
    assert!(!unpinned.pinned);
    assert!(unpinned.range.is_none());
}

// --- git URL parsing -----------------------------------------------------------------

#[test]
fn parse_git_url_protocol_forms() {
    let https = parse_git_url("https://github.com/user/repo.git").unwrap();
    assert_eq!(https.host, "github.com");
    assert_eq!(https.path, "user/repo");
    assert_eq!(https.repo, "https://github.com/user/repo.git");
    assert!(https.ref_.is_none());
    assert!(!https.pinned);

    let with_ref = parse_git_url("https://github.com/user/repo.git@v1.0").unwrap();
    assert_eq!(with_ref.path, "user/repo");
    assert_eq!(with_ref.ref_.as_deref(), Some("v1.0"));
    assert!(with_ref.pinned);
    // The repo URL strips the ref suffix.
    assert_eq!(with_ref.repo, "https://github.com/user/repo.git");

    // Bare scp-like shorthand is rejected without a git: prefix (upstream
    // requires git: or an explicit protocol); with the prefix it parses.
    assert!(parse_git_url("git@github.com:user/repo.git").is_none());
    let scp = parse_git_url("git:git@github.com:user/repo.git").unwrap();
    assert_eq!(scp.host, "github.com");
    // buildGitSource strips the .git suffix from the install path.
    assert_eq!(scp.path, "user/repo");
    assert_eq!(scp.repo, "git@github.com:user/repo.git");

    let ssh = parse_git_url("ssh://git@github.com/user/repo.git").unwrap();
    assert_eq!(ssh.host, "github.com");
    assert_eq!(ssh.path, "user/repo");

    // git: prefix accepts shorthand host/path.
    let shorthand = parse_git_url("git:github.com/user/repo").unwrap();
    assert_eq!(shorthand.host, "github.com");
    assert_eq!(shorthand.path, "user/repo");
    assert_eq!(shorthand.repo, "https://github.com/user/repo");
}

#[test]
fn parse_git_url_rejects_without_prefix_or_protocol() {
    // Without git: prefix, only explicit protocol URLs are accepted.
    assert!(parse_git_url("github.com/user/repo").is_none());
    assert!(parse_git_url("user/repo").is_none());
    assert!(parse_git_url("npm:foo").is_none());
}

#[test]
fn parse_git_url_rejects_unsafe_paths() {
    assert!(parse_git_url("https://github.com/../etc/passwd").is_none());
    // Path with fewer than 2 segments.
    assert!(parse_git_url("https://github.com/onlyone").is_none());
}

#[test]
fn is_local_path_known_prefixes() {
    assert!(!is_local_path("npm:foo"));
    assert!(!is_local_path("git:github.com/a/b"));
    assert!(!is_local_path("https://example.com"));
    assert!(!is_local_path("http://example.com"));
    assert!(!is_local_path("ssh://example.com"));
    assert!(!is_local_path("github:user/repo"));
    // file: URLs and everything else are local.
    assert!(is_local_path("file:///some/path"));
    assert!(is_local_path("./relative"));
    assert!(is_local_path("/absolute"));
}

#[test]
fn parse_source_dispatches_by_prefix() {
    assert!(matches!(
        parse_source("npm:foo@1.0.0"),
        ParsedSource::Npm(_)
    ));
    assert!(matches!(
        parse_source("https://github.com/a/b"),
        ParsedSource::Git(_)
    ));
    assert!(matches!(
        parse_source("./local-ext"),
        ParsedSource::Local(_)
    ));
}

// --- pattern filters -------------------------------------------------------------------

fn files(dir: &Path, names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(|n| dir.join(n)).collect()
}

#[test]
fn apply_patterns_include_exclude_force() {
    let base = temp_dir("patterns");
    let all = files(&base, &["a.ts", "b.ts", "c.ts"]);

    // No includes: everything enabled.
    let result = apply_patterns(&all, &[], &base);
    assert_eq!(result.len(), 3);

    // Include only b.
    let result = apply_patterns(&all, &["b.ts".to_string()], &base);
    assert_eq!(result, [base.join("b.ts")].into_iter().collect());

    // Glob include.
    let result = apply_patterns(&all, &["*.ts".to_string()], &base);
    assert_eq!(result.len(), 3);

    // Exclude with !.
    let result = apply_patterns(&all, &["!b.ts".to_string()], &base);
    assert!(!result.contains(&base.join("b.ts")));
    assert_eq!(result.len(), 2);

    // Force-include overrides exclusion.
    let result = apply_patterns(&all, &["!b.ts".to_string(), "+b.ts".to_string()], &base);
    assert!(result.contains(&base.join("b.ts")));

    // Force-exclude overrides force-include.
    let result = apply_patterns(
        &all,
        &[
            "!b.ts".to_string(),
            "+b.ts".to_string(),
            "-b.ts".to_string(),
        ],
        &base,
    );
    assert!(!result.contains(&base.join("b.ts")));
}

#[test]
fn matches_any_pattern_rel_name_and_skill_parents() {
    let base = temp_dir("match");
    let skill = base.join("skills").join("my-skill").join("SKILL.md");

    assert!(matches_any_pattern(
        &skill,
        &["SKILL.md".to_string()],
        &base
    ));
    assert!(matches_any_pattern(
        &skill,
        &["skills/my-skill/SKILL.md".to_string()],
        &base
    ));
    // SKILL.md also matches via its parent directory variants.
    assert!(matches_any_pattern(
        &skill,
        &["my-skill".to_string()],
        &base
    ));
    assert!(matches_any_pattern(
        &skill,
        &["skills/my-skill".to_string()],
        &base
    ));

    // Non-skill files do not match via parent dirs.
    let ext = base.join("extensions").join("foo.ts");
    assert!(matches_any_pattern(&ext, &["foo.ts".to_string()], &base));
    assert!(!matches_any_pattern(
        &ext,
        &["extensions".to_string()],
        &base
    ));
}

#[test]
fn matches_any_exact_pattern_ignores_globs() {
    let base = temp_dir("exact");
    let file = base.join("foo.ts");
    // Exact matching compares the raw string (with ./ stripped).
    assert!(matches_any_exact_pattern(
        &file,
        &["./foo.ts".to_string()],
        &base
    ));
    assert!(matches_any_exact_pattern(
        &file,
        &["foo.ts".to_string()],
        &base
    ));
    assert!(!matches_any_exact_pattern(
        &file,
        &["*.ts".to_string()],
        &base
    ));
}

#[test]
fn is_enabled_by_overrides_precedence() {
    let base = temp_dir("overrides");
    let file = base.join("a.ts");

    // Plain patterns don't affect enabled state.
    assert!(is_enabled_by_overrides(&file, &["*.ts".to_string()], &base));
    // Exclusion disables.
    assert!(!is_enabled_by_overrides(
        &file,
        &["!a.ts".to_string()],
        &base
    ));
    // Force-include re-enables.
    assert!(is_enabled_by_overrides(
        &file,
        &["!a.ts".to_string(), "+a.ts".to_string()],
        &base
    ));
    // Force-exclude wins over everything.
    assert!(!is_enabled_by_overrides(
        &file,
        &[
            "!a.ts".to_string(),
            "+a.ts".to_string(),
            "-a.ts".to_string()
        ],
        &base
    ));
}

#[test]
fn apply_autoload_disabled_patterns_states() {
    let base = temp_dir("autoload");
    let all = files(&base, &["a.ts", "b.ts"]);

    // Plain pattern enables matched.
    let result = apply_autoload_disabled_patterns(&all, &["a.ts".to_string()], &base);
    assert_eq!(result.get(&base.join("a.ts")), Some(&true));

    // ! disables.
    let result = apply_autoload_disabled_patterns(&all, &["!a.ts".to_string()], &base);
    assert_eq!(result.get(&base.join("a.ts")), Some(&false));

    // - forces disabled (exact).
    let result = apply_autoload_disabled_patterns(&all, &["-a.ts".to_string()], &base);
    assert_eq!(result.get(&base.join("a.ts")), Some(&false));

    // + forces enabled (exact).
    let result = apply_autoload_disabled_patterns(&all, &["+a.ts".to_string()], &base);
    assert_eq!(result.get(&base.join("a.ts")), Some(&true));
}

// --- precedence / dedupe ----------------------------------------------------------------

#[test]
fn resource_precedence_rank_order() {
    let meta = |source: &str, scope: SourceScope, origin: ResourceOrigin| {
        pillar_coding_agent::core::package_manager::PathMetadata {
            source: source.to_string(),
            scope,
            origin,
            base_dir: None,
        }
    };
    assert_eq!(
        resource_precedence_rank(&meta(
            "local",
            SourceScope::Project,
            ResourceOrigin::TopLevel
        )),
        0
    );
    assert_eq!(
        resource_precedence_rank(&meta(
            "auto",
            SourceScope::Project,
            ResourceOrigin::TopLevel
        )),
        1
    );
    assert_eq!(
        resource_precedence_rank(&meta("local", SourceScope::User, ResourceOrigin::TopLevel)),
        2
    );
    assert_eq!(
        resource_precedence_rank(&meta("auto", SourceScope::User, ResourceOrigin::TopLevel)),
        3
    );
    assert_eq!(
        resource_precedence_rank(&meta("npm:x", SourceScope::User, ResourceOrigin::Package)),
        4
    );
}

#[test]
fn package_filter_extraction() {
    let plain = PackageSource::Source("npm:foo".to_string());
    assert!(package_filter_of(&plain).is_none());
    assert_eq!(package_source_string(&plain), "npm:foo");

    let filtered = PackageSource::Filtered {
        source: "npm:foo".to_string(),
        autoload: Some(false),
        extensions: Some(vec!["index.ts".to_string()]),
        skills: None,
        prompts: None,
        themes: None,
    };
    let filter = package_filter_of(&filtered).unwrap();
    assert_eq!(filter.autoload, Some(false));
    assert_eq!(
        filter.extensions.as_deref(),
        Some(&["index.ts".to_string()][..])
    );
    assert_eq!(package_source_string(&filtered), "npm:foo");
}

// --- collection ------------------------------------------------------------------------

#[test]
fn collect_auto_skill_entries_pi_mode_vs_agents_mode() {
    let dir = temp_dir("skills");
    let sub = dir.join("my-skill");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("SKILL.md"), "# skill").unwrap();
    std::fs::write(dir.join("root.md"), "# root prompt").unwrap();

    // pi mode: SKILL.md in subdirs + markdown in the root dir only.
    let pi = collect_auto_skill_entries(
        &dir,
        pillar_coding_agent::core::package_manager::SkillDiscoveryMode::Pi,
    );
    assert!(pi.contains(&sub.join("SKILL.md")));
    assert!(pi.contains(&dir.join("root.md")));

    // agents mode: SKILL.md in subdirs + markdown only in nested dirs
    // (not the root).
    let agents = collect_auto_skill_entries(
        &dir,
        pillar_coding_agent::core::package_manager::SkillDiscoveryMode::Agents,
    );
    assert!(agents.contains(&sub.join("SKILL.md")));
    assert!(!agents.contains(&dir.join("root.md")));
}

#[test]
fn collect_auto_skill_entries_stops_at_first_skill_md() {
    let dir = temp_dir("skill-stop");
    std::fs::write(dir.join("SKILL.md"), "# top").unwrap();
    let sub = dir.join("nested");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("SKILL.md"), "# nested").unwrap();

    let entries = collect_auto_skill_entries(
        &dir,
        pillar_coding_agent::core::package_manager::SkillDiscoveryMode::Pi,
    );
    assert_eq!(entries, vec![dir.join("SKILL.md")]);
}

#[test]
fn collect_auto_extension_entries_uses_manifest_and_index() {
    let dir = temp_dir("ext");

    // Directory with pi manifest in package.json.
    let manifest_pkg = dir.join("manifest-pkg");
    std::fs::create_dir_all(&manifest_pkg).unwrap();
    std::fs::write(
        manifest_pkg.join("package.json"),
        r#"{"pi": {"extensions": ["custom.ts"]}}"#,
    )
    .unwrap();
    std::fs::write(manifest_pkg.join("custom.ts"), "").unwrap();
    std::fs::write(manifest_pkg.join("index.ts"), "").unwrap();

    let entries = collect_auto_extension_entries(&dir);
    // The manifest-pkg dir resolves via its manifest (not its index).
    assert!(entries.contains(&manifest_pkg.join("custom.ts")));
    assert!(!entries.contains(&manifest_pkg.join("index.ts")));

    // Top-level .ts files are also collected.
    std::fs::write(dir.join("top.ts"), "").unwrap();
    let entries = collect_auto_extension_entries(&dir);
    assert!(entries.contains(&dir.join("top.ts")));
}

#[test]
fn collect_auto_extension_entries_skips_hidden_and_node_modules() {
    let dir = temp_dir("ext-skip");
    let node_modules = dir.join("node_modules");
    std::fs::create_dir_all(&node_modules).unwrap();
    std::fs::write(node_modules.join("dep.ts"), "").unwrap();
    std::fs::write(dir.join(".hidden.ts"), "").unwrap();
    std::fs::write(dir.join("real.ts"), "").unwrap();

    let entries = collect_auto_extension_entries(&dir);
    assert_eq!(entries, vec![dir.join("real.ts")]);
}

#[test]
fn collect_ancestor_agents_skill_dirs_up_to_git_root() {
    let repo = temp_dir("agents");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let nested = repo.join("a").join("b");
    std::fs::create_dir_all(&nested).unwrap();

    let dirs = collect_ancestor_agents_skill_dirs(&nested);
    assert_eq!(dirs.len(), 3); // b, a, repo (stops at git root)
    assert!(dirs.contains(&nested.join(".agents").join("skills")));
    assert!(dirs.contains(&repo.join(".agents").join("skills")));
}

#[test]
fn get_extension_temp_folder_creates_private_dir() {
    let agent_dir = temp_dir("tmpagent");
    let folder = get_extension_temp_folder(&agent_dir);
    assert!(folder.exists());
    assert_eq!(folder, agent_dir.join("tmp").join("extensions"));
}

// --- manager: settings + paths ------------------------------------------------------------

#[test]
fn add_and_remove_source_round_trip() {
    let cwd = temp_dir("settings-cwd");
    let agent_dir = temp_dir("settings-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings.clone());

    assert!(manager.add_source_to_settings("npm:foo", false));
    // Adding again with a different spec for the same package is a no-op
    // normalization-wise? Upstream updates the entry (returns true), but
    // adding the identical source returns false.
    assert!(!manager.add_source_to_settings("npm:foo", false));

    let configured = manager.list_configured_packages();
    assert_eq!(configured.len(), 1);
    assert_eq!(configured[0].source, "npm:foo");
    assert_eq!(configured[0].scope, SourceScope::User);
    assert!(!configured[0].filtered);

    // Project scope entries are separate.
    assert!(manager.add_source_to_settings("npm:bar", true));
    let configured = manager.list_configured_packages();
    assert_eq!(configured.len(), 2);
    assert!(configured.iter().any(|p| p.scope == SourceScope::Project));

    assert!(manager.remove_source_from_settings("npm:foo", false));
    assert!(!manager.remove_source_from_settings("npm:foo", false));
    let configured = manager.list_configured_packages();
    assert_eq!(configured.len(), 1);
    assert_eq!(configured[0].source, "npm:bar");
}

#[test]
fn package_identity_ignores_version_and_normalizes_git() {
    let cwd = temp_dir("identity-cwd");
    let agent_dir = temp_dir("identity-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    assert_eq!(
        manager.get_package_identity("npm:foo@1.0.0", None),
        manager.get_package_identity("npm:foo@2.0.0", None)
    );
    assert_eq!(
        manager.get_package_identity("https://github.com/a/b", None),
        manager.get_package_identity("git:github.com/a/b", None)
    );
}

#[test]
fn dedupe_packages_project_wins_and_autoload_delta() {
    let cwd = temp_dir("dedupe-cwd");
    let agent_dir = temp_dir("dedupe-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let user = (
        PackageSource::Source("npm:foo".to_string()),
        SourceScope::User,
    );
    let project = (
        PackageSource::Source("npm:foo".to_string()),
        SourceScope::Project,
    );

    let deduped = manager.dedupe_packages(vec![user.clone(), project.clone()]);
    assert_eq!(deduped, vec![project.clone()]);

    // Reversed order: project still wins.
    let deduped = manager.dedupe_packages(vec![project.clone(), user.clone()]);
    assert_eq!(deduped, vec![project]);

    // Project autoload=false keeps both (delta first).
    let project_delta = (
        PackageSource::Filtered {
            source: "npm:foo".to_string(),
            autoload: Some(false),
            extensions: None,
            skills: None,
            prompts: None,
            themes: None,
        },
        SourceScope::Project,
    );
    let deduped = manager.dedupe_packages(vec![project_delta.clone(), user.clone()]);
    assert_eq!(deduped, vec![project_delta, user.clone()]);

    // Different packages are both kept.
    let other = (
        PackageSource::Source("npm:bar".to_string()),
        SourceScope::User,
    );
    let deduped = manager.dedupe_packages(vec![user.clone(), other]);
    assert_eq!(deduped.len(), 2);
}

#[test]
fn npm_install_args_per_package_manager() {
    let cwd = temp_dir("npmargs-cwd");
    let agent_dir = temp_dir("npmargs-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);
    let root = Path::new("/tmp/install-root");
    let specs = vec!["foo@1.0.0".to_string()];

    // Default npm: --prefix + --legacy-peer-deps, specs after flags.
    let args = manager.get_npm_install_args(&specs, root).unwrap();
    assert_eq!(args[0], "install");
    assert!(args.contains(&"--prefix".to_string()));
    assert!(args.contains(&"--legacy-peer-deps".to_string()));
    assert!(args.contains(&"foo@1.0.0".to_string()));
}

#[test]
fn get_installed_path_missing_and_local() {
    let cwd = temp_dir("installed-cwd");
    let agent_dir = temp_dir("installed-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    // Missing npm package: falls back to the managed path which doesn't exist.
    assert!(
        manager
            .get_installed_path("npm:definitely-not-installed-xyz", SourceScope::User)
            .is_none()
    );

    // Local path: exists after resolution.
    let local = cwd.join(".pillar").join("ext-dir");
    std::fs::create_dir_all(&local).unwrap();
    assert_eq!(
        manager.get_installed_path("./ext-dir", SourceScope::Project),
        Some(local)
    );
}

#[test]
fn resolve_local_entries_and_top_level_paths() {
    let cwd = temp_dir("resolve-cwd");
    let agent_dir = temp_dir("resolve-agent");
    let config_dir = cwd.join(".pillar");
    let prompts_dir = config_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir).unwrap();
    std::fs::write(prompts_dir.join("my-prompt.md"), "prompt").unwrap();
    let settings = make_manager(&cwd, &agent_dir);
    {
        let mut settings = settings.lock().unwrap();
        settings
            .set_project_extension_paths(serde_json::json!(["prompts/my-prompt.md"]))
            .unwrap();
    }
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let resolved = manager
        .resolve(Some(&mut |_source| MissingSourceAction::Skip))
        .unwrap();
    assert!(
        resolved
            .prompts
            .iter()
            .any(|r| r.path == prompts_dir.join("my-prompt.md") && r.enabled)
    );
}

#[test]
fn resolve_auto_discovers_project_and_user_resources() {
    let cwd = temp_dir("auto-cwd");
    let agent_dir = temp_dir("auto-agent");

    // Project prompt.
    let project_prompts = cwd.join(".pillar").join("prompts");
    std::fs::create_dir_all(&project_prompts).unwrap();
    std::fs::write(project_prompts.join("project-prompt.md"), "p").unwrap();

    // Project extension.
    let project_exts = cwd.join(".pillar").join("extensions");
    std::fs::create_dir_all(&project_exts).unwrap();
    std::fs::write(project_exts.join("proj-ext.ts"), "e").unwrap();

    // Project skill via .agents/skills.
    let agents_skills = cwd.join(".agents").join("skills").join("proj-skill");
    std::fs::create_dir_all(&agents_skills).unwrap();
    std::fs::write(agents_skills.join("SKILL.md"), "s").unwrap();

    // User theme + prompt.
    let user_themes = agent_dir.join("themes");
    std::fs::create_dir_all(&user_themes).unwrap();
    std::fs::write(user_themes.join("my-theme.json"), "{}").unwrap();

    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let resolved = manager
        .resolve(Some(&mut |_source| MissingSourceAction::Skip))
        .unwrap();
    assert!(
        resolved
            .prompts
            .iter()
            .any(|r| r.path == project_prompts.join("project-prompt.md"))
    );
    assert!(
        resolved
            .extensions
            .iter()
            .any(|r| r.path == project_exts.join("proj-ext.ts"))
    );
    assert!(
        resolved
            .skills
            .iter()
            .any(|r| r.path == agents_skills.join("SKILL.md"))
    );
    assert!(
        resolved
            .themes
            .iter()
            .any(|r| r.path == user_themes.join("my-theme.json"))
    );

    // Precedence: project resources rank before user resources.
    let project_rank = resolved
        .prompts
        .iter()
        .find(|r| r.path == project_prompts.join("project-prompt.md"))
        .map(|r| resource_precedence_rank(&r.metadata))
        .unwrap();
    assert_eq!(project_rank, 1); // auto + project
}

#[test]
fn resolve_local_package_source_collects_resources() {
    let cwd = temp_dir("localpkg-cwd");
    let agent_dir = temp_dir("localpkg-agent");

    let pkg = cwd.join(".pillar").join("my-pkg");
    let ext_dir = pkg.join("extensions");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("from-pkg.ts"), "x").unwrap();
    let skill_dir = pkg.join("skills").join("pkg-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "s").unwrap();

    seed_settings(&cwd, &agent_dir)
        .set_project_packages(serde_json::json!(["./my-pkg"]))
        .unwrap();

    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let resolved = manager
        .resolve(Some(&mut |_source| MissingSourceAction::Skip))
        .unwrap();
    assert!(
        resolved
            .extensions
            .iter()
            .any(|r| r.path == ext_dir.join("from-pkg.ts"))
    );
    assert!(
        resolved
            .skills
            .iter()
            .any(|r| r.path == skill_dir.join("SKILL.md"))
    );
    // Package resources carry package metadata.
    let ext = resolved
        .extensions
        .iter()
        .find(|r| r.path == ext_dir.join("from-pkg.ts"))
        .unwrap();
    assert_eq!(ext.metadata.origin, ResourceOrigin::Package);
    assert_eq!(ext.metadata.source, "./my-pkg");
}

#[test]
fn resolve_missing_npm_source_reports_via_callback() {
    let cwd = temp_dir("missing-cwd");
    let agent_dir = temp_dir("missing-agent");
    seed_settings(&cwd, &agent_dir).set_global_setting(
        "packages",
        serde_json::json!(["npm:definitely-not-a-real-package-xyz"]),
    );
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let mut on_missing = move |source: &str| -> MissingSourceAction {
        seen2.lock().unwrap().push(source.to_string());
        MissingSourceAction::Skip
    };
    let resolved = manager.resolve(Some(&mut on_missing)).unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.first().map(String::as_str),
        Some("npm:definitely-not-a-real-package-xyz")
    );
    // Skipped: nothing collected from it.
    assert!(resolved.extensions.is_empty());
}

#[test]
fn resolve_extension_sources_temporary_scope() {
    let cwd = temp_dir("tmpsrc-cwd");
    let agent_dir = temp_dir("tmpsrc-agent");
    let pkg = cwd.join("ext-pkg");
    let ext_dir = pkg.join("extensions");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("tmp-ext.ts"), "x").unwrap();

    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let resolved = manager
        .resolve_extension_sources(&["./ext-pkg".to_string()], false, true)
        .unwrap();
    assert!(
        resolved
            .extensions
            .iter()
            .any(|r| r.path == ext_dir.join("tmp-ext.ts"))
    );
    assert_eq!(
        resolved.extensions[0].metadata.scope,
        SourceScope::Temporary
    );
}

#[test]
fn install_local_missing_path_errors() {
    let cwd = temp_dir("instlocal-cwd");
    let agent_dir = temp_dir("instlocal-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let mut manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);
    // Installing needs a host policy; this test is about the path check.
    manager.set_effect_authorizer(allow_all());

    let error = manager
        .install("./does-not-exist-anywhere", false)
        .unwrap_err();
    assert!(error.contains("Path does not exist"), "{error}");
}

#[test]
fn install_emits_progress_events() {
    let cwd = temp_dir("progress-cwd");
    let agent_dir = temp_dir("progress-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let mut manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let events = Arc::new(Mutex::new(Vec::new()));
    let events2 = events.clone();
    manager.set_progress_callback(Arc::new(Mutex::new(move |event| {
        events2.lock().unwrap().push(event);
    })));
    manager.set_effect_authorizer(allow_all());

    let _ = manager.install("./does-not-exist-anywhere", false);
    let events = events.lock().unwrap();
    assert_eq!(events[0].kind, ProgressKind::Start);
    assert_eq!(events[0].action, ProgressAction::Install);
    assert_eq!(events.last().unwrap().kind, ProgressKind::Error);
}

#[test]
fn resolve_managed_path_refuses_outside_root() {
    let cwd = temp_dir("managed-cwd");
    let agent_dir = temp_dir("managed-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let root = agent_dir.join("npm");
    let ok = ["node_modules", "foo"];
    let escaped = ["..", "escape"];

    let resolved = manager.resolve_managed_path(&root, &ok).unwrap();
    assert!(resolved.starts_with(&root));

    let error = manager.resolve_managed_path(&root, &escaped).unwrap_err();
    assert!(
        error.contains("Refusing to use path outside package install root"),
        "{error}"
    );
}

#[test]
fn temporary_dir_is_stable_and_hashed() {
    let cwd = temp_dir("tempdir-cwd");
    let agent_dir = temp_dir("tempdir-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let first = manager.get_temporary_dir("npm", None);
    let second = manager.get_temporary_dir("npm", None);
    assert_eq!(first, second);
    // Hash segment is 8 chars.
    let hash_segment = first.file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(hash_segment.len(), 8, "{hash_segment}");

    // Different suffixes differ.
    let with_suffix = manager.get_temporary_dir("git-github.com", Some("user/repo"));
    assert_ne!(first, with_suffix);
}

#[test]
fn npm_install_path_uses_managed_location() {
    let cwd = temp_dir("npmpath-cwd");
    let agent_dir = temp_dir("npmpath-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let source = npm("some-package", None);
    let path = manager.get_npm_install_path(&source, SourceScope::User);
    assert_eq!(
        path,
        agent_dir
            .join("npm")
            .join("node_modules")
            .join("some-package")
    );

    let project_path = manager.get_npm_install_path(&source, SourceScope::Project);
    assert_eq!(
        project_path,
        cwd.join(".pillar")
            .join("npm")
            .join("node_modules")
            .join("some-package")
    );

    // Temporary scope hashes under the extension temp folder.
    let temp_path = manager.get_npm_install_path(&source, SourceScope::Temporary);
    assert!(temp_path.starts_with(agent_dir.join("tmp").join("extensions")));
    assert!(temp_path.to_string_lossy().contains("node_modules"));
}

#[test]
fn git_install_path_layout() {
    let cwd = temp_dir("gitpath-cwd");
    let agent_dir = temp_dir("gitpath-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let source = git("github.com", "user/repo", None);
    let path = manager
        .get_git_install_path(&source, SourceScope::User)
        .unwrap();
    assert_eq!(
        path,
        agent_dir
            .join("git")
            .join("github.com")
            .join("user")
            .join("repo")
    );

    // Temporary scope uses hashed temp dir.
    let temp_path = manager
        .get_git_install_path(&source, SourceScope::Temporary)
        .unwrap();
    assert!(temp_path.starts_with(agent_dir.join("tmp").join("extensions")));
}

#[test]
fn update_no_matching_package_message() {
    let cwd = temp_dir("update-cwd");
    let agent_dir = temp_dir("update-agent");
    seed_settings(&cwd, &agent_dir)
        .set_global_setting("packages", serde_json::json!(["npm:real-package"]));
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let error = manager.update(Some("npm:real-packag")).unwrap_err();
    assert!(
        error.contains("No matching package found for npm:real-packag"),
        "{error}"
    );

    // Suggestion form when close to a configured source name.
    let error = manager.update(Some("real-package-typo")).unwrap_err();
    if error.contains("Did you mean") {
        assert!(error.contains("npm:real-package"));
    }
}

fn seed_settings(cwd: &Path, agent_dir: &Path) -> SettingsManager {
    let options = SettingsManagerCreateOptions {
        project_trusted: Some(true),
    };
    SettingsManager::create(&cwd.to_string_lossy(), agent_dir, options)
}

#[test]
fn resource_type_naming_matches_upstream_dirs() {
    use pillar_coding_agent::core::package_manager::collect_resource_files;
    let dir = temp_dir("convention");
    let themes = dir.join("themes");
    std::fs::create_dir_all(&themes).unwrap();
    std::fs::write(themes.join("dark.json"), "{}").unwrap();

    let files = collect_resource_files(&themes, ResourceType::Themes);
    assert_eq!(files, vec![themes.join("dark.json")]);

    let prompts = dir.join("prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(prompts.join("p.md"), "x").unwrap();
    let files = collect_resource_files(&prompts, ResourceType::Prompts);
    assert_eq!(files, vec![prompts.join("p.md")]);
}

// --- install policy (docs/ARCHITECTURE-REVIEW-s05c0.md 0) ------------------------

/// Installing fetches code, so it is an effect the host has to authorize: with
/// no policy the manager refuses instead of installing whatever the settings
/// mention.
#[test]
fn a_package_install_is_refused_without_a_policy() {
    let cwd = temp_dir("policy-cwd");
    let agent_dir = temp_dir("policy-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);

    let error = manager.install("npm:anything@1.2.3", false).unwrap_err();
    assert!(error.contains("no package policy"), "{error}");

    let error = manager.remove("npm:anything@1.2.3", false).unwrap_err();
    assert!(error.contains("no package policy"), "{error}");
}

/// The policy sees the normalized source and scope, and a denial is what the
/// caller gets — no network attempt happens.
#[test]
fn a_denied_package_install_reports_the_policy_reason() {
    let cwd = temp_dir("deny-cwd");
    let agent_dir = temp_dir("deny-agent");
    let settings = make_manager(&cwd, &agent_dir);
    let mut manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);
    let seen: Arc<Mutex<Vec<EffectIntent>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_for_policy = Arc::clone(&seen);
    manager.set_effect_authorizer(Arc::new(move |intent| {
        seen_for_policy.lock().unwrap().push(intent.clone());
        EffectDecision::Deny {
            reason: "not now".to_string(),
        }
    }));

    let error = manager.install("npm:anything@1.2.3", true).unwrap_err();
    assert!(error.contains("not now"), "{error}");
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![EffectIntent::PackageInstall {
            source: "npm:anything@1.2.3".to_string(),
            project_scope: true,
        }]
    );
}

/// An allowing policy lets a local source through (a local source is only a
/// path reference, so nothing is fetched).
#[test]
fn an_allowed_local_install_proceeds() {
    let cwd = temp_dir("allow-cwd");
    let agent_dir = temp_dir("allow-agent");
    let source = temp_dir("allow-source");
    let settings = make_manager(&cwd, &agent_dir);
    let mut manager = DefaultPackageManager::new(&cwd.to_string_lossy(), &agent_dir, settings);
    manager.set_effect_authorizer(allow_all());

    manager
        .install(&source.to_string_lossy(), false)
        .expect("an allowed install goes through");
}
