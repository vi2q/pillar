//! Port of packages/coding-agent/src/core/resource-loader.ts (pi v0.84.3):
//! project context file discovery (AGENTS.md/CLAUDE.md chains with
//! worktree shadowing) and the DefaultResourceLoader state machine that
//! merges package-manager resolved paths into skills/prompts/themes plus
//! system prompt discovery.
//!
//! divergences: extension loading (a JS runtime concern) is not ported —
//! `get_extensions` returns an empty result and extension conflict
//! diagnostics apply to the injectable extension list; theme loading
//! returns raw JSON values instead of a Theme object (the theme renderer
//! is not ported); chalk-colored warnings print plainly.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::diagnostics::ResourceDiagnostic as FlatDiagnostic;
use crate::core::package_manager::{
    PathMetadata as PmPathMetadata, PathMetadata, ResourceOrigin, SourceScope,
};
use crate::core::prompt_templates::{PromptTemplate, load_prompt_templates};
use crate::core::settings_manager::SettingsManager;
use crate::core::skills::{LoadSkillsOptions, ResourceDiagnostic, load_skills};
use crate::core::source_info::{
    SourceInfo, SourceOrigin, SourceScope as InfoScope, create_source_info,
};
use crate::core::tools::path_utils::resolve_to_cwd;

const CONFIG_DIR_NAME: &str = crate::core::settings_manager::CONFIG_DIR_NAME;

/// Convert a package-manager metadata into the source_info flavor (the
/// port keeps two shapes because `base_dir` is a PathBuf vs String).
fn pm_to_info_metadata(m: &PmPathMetadata) -> crate::core::source_info::PathMetadata {
    crate::core::source_info::PathMetadata {
        source: m.source.clone(),
        scope: match m.scope {
            SourceScope::User => crate::core::source_info::SourceScope::User,
            SourceScope::Project => crate::core::source_info::SourceScope::Project,
            SourceScope::Temporary => crate::core::source_info::SourceScope::Temporary,
        },
        origin: match m.origin {
            ResourceOrigin::Package => crate::core::source_info::SourceOrigin::Package,
            ResourceOrigin::TopLevel => crate::core::source_info::SourceOrigin::TopLevel,
        },
        base_dir: m.base_dir.as_ref().map(|b| b.to_string_lossy().to_string()),
    }
}

// ============================================================================
// Context files (upstream loadProjectContextFiles)
// ============================================================================

const CONTEXT_FILE_CANDIDATES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

fn load_context_file_from_dir(dir: &Path) -> Option<(PathBuf, String)> {
    for filename in CONTEXT_FILE_CANDIDATES {
        let file_path = dir.join(filename);
        if file_path.is_file() {
            match std::fs::read_to_string(&file_path) {
                Ok(content) => {
                    let content = content
                        .strip_prefix('\u{feff}')
                        .unwrap_or(&content)
                        .to_string();
                    return Some((file_path, content));
                }
                Err(error) => {
                    eprintln!("Warning: Could not read {}: {error}", file_path.display());
                }
            }
        }
    }
    None
}

/// Git paths discovered by walking up from cwd (upstream `findGitPaths`):
/// the worktree repo dir, the common git dir, and the HEAD path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPaths {
    pub repo_dir: PathBuf,
    pub common_git_dir: PathBuf,
    pub head_path: PathBuf,
}

/// Find git paths for a cwd (upstream `findGitPaths` in
/// footer-data-provider.ts): `.git` as a directory, or a `.git` file with
/// a `gitdir:` pointer (worktrees/submodules), resolving `commondir` when
/// present.
pub fn find_git_paths(cwd: &Path) -> Option<GitPaths> {
    let mut dir = cwd.to_path_buf();
    loop {
        let git_path = dir.join(".git");
        if git_path.exists() {
            let Ok(metadata) = std::fs::symlink_metadata(&git_path) else {
                return None;
            };
            if metadata.is_file() {
                let content = std::fs::read_to_string(&git_path).ok()?.trim().to_string();
                if let Some(git_dir_raw) = content.strip_prefix("gitdir: ") {
                    let git_dir = resolve_to_cwd(git_dir_raw.trim(), &dir.to_string_lossy());
                    let head_path = git_dir.join("HEAD");
                    if !head_path.exists() {
                        return None;
                    }
                    let common_dir_path = git_dir.join("commondir");
                    let common_git_dir = if common_dir_path.exists() {
                        let target = std::fs::read_to_string(&common_dir_path)
                            .ok()?
                            .trim()
                            .to_string();
                        resolve_to_cwd(&target, &git_dir.to_string_lossy())
                    } else {
                        git_dir
                    };
                    return Some(GitPaths {
                        repo_dir: dir,
                        common_git_dir,
                        head_path,
                    });
                }
            } else if metadata.is_dir() {
                let head_path = git_path.join("HEAD");
                if !head_path.exists() {
                    return None;
                }
                return Some(GitPaths {
                    repo_dir: dir,
                    common_git_dir: git_path,
                    head_path,
                });
            }
        }
        let parent = dir.parent()?;
        if parent == dir {
            return None;
        }
        dir = parent.to_path_buf();
    }
}

fn canonicalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The main repo's context file that a nested linked worktree's own copy
/// shadows (upstream `findShadowedContextFile`). Returns the canonicalized
/// main-repo context file path when shadowing applies.
fn find_shadowed_context_file(cwd: &Path) -> Option<PathBuf> {
    let git_paths = find_git_paths(cwd)?;
    let common_git_dir = canonicalize(&git_paths.common_git_dir);
    let worktree_root = canonicalize(&git_paths.repo_dir);
    let main_repo_root = common_git_dir.parent()?.to_path_buf();
    if !worktree_root.starts_with(&main_repo_root) || worktree_root == main_repo_root {
        return None;
    }
    // dirname of the common git dir is the main worktree root only when
    // that dir is itself checked out from the same repo.
    if canonicalize(&main_repo_root.join(".git")) != common_git_dir {
        return None;
    }
    let (_, file_name) = load_context_file_from_dir(&worktree_root)?; // second element unused
    let _ = file_name;
    let worktree_context = load_context_file_from_dir(&worktree_root)?;
    Some(
        main_repo_root.join(
            worktree_context
                .0
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default(),
        ),
    )
}

/// Load project context files: the global agent-dir file, then ancestor
/// files from cwd upward with nearest-first ordering (upstream
/// `loadProjectContextFiles`).
pub fn load_project_context_files(cwd: &str, agent_dir: &str) -> Vec<(PathBuf, String)> {
    let resolved_cwd = resolve_to_cwd(cwd, "/");
    let resolved_agent_dir = resolve_to_cwd(agent_dir, "/");

    let mut context_files: Vec<(PathBuf, String)> = Vec::new();
    let mut seen_paths: BTreeSet<PathBuf> = BTreeSet::new();

    if let Some(global_context) = load_context_file_from_dir(&resolved_agent_dir) {
        seen_paths.insert(global_context.0.clone());
        context_files.push(global_context);
    }

    let mut ancestor_context_files: Vec<(PathBuf, String)> = Vec::new();

    let shadowed_context_file = find_shadowed_context_file(&resolved_cwd);
    let mut current_dir = resolved_cwd;

    loop {
        let context_file = load_context_file_from_dir(&current_dir);
        let is_shadowed = shadowed_context_file
            .as_ref()
            .zip(context_file.as_ref())
            .is_some_and(|(shadowed, (path, _))| &canonicalize(path) == shadowed);
        if let Some((path, content)) = context_file {
            if !is_shadowed && !seen_paths.contains(&path) {
                ancestor_context_files.insert(0, (path.clone(), content));
                seen_paths.insert(path);
            }
        }

        let Some(parent_dir) = current_dir.parent() else {
            break;
        };
        if parent_dir == current_dir {
            break;
        }
        current_dir = parent_dir.to_path_buf();
    }

    context_files.extend(ancestor_context_files);
    context_files
}

// ============================================================================
// Resource loader
// ============================================================================

/// A skill with resolved source info (upstream `Skill` + sourceInfo).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub file_path: String,
    pub base_dir: String,
    pub disable_model_invocation: bool,
    pub source_info: SourceInfo,
}

/// A prompt template with resolved source info (upstream `PromptTemplate`
/// + sourceInfo).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPrompt {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub content: String,
    pub file_path: String,
    pub source_info: SourceInfo,
}

/// A theme loaded from a JSON file (upstream `Theme`; the port keeps the
/// raw JSON because the theme renderer is not ported).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedTheme {
    pub name: Option<String>,
    pub source_path: Option<String>,
    pub data: Value,
    pub source_info: SourceInfo,
}

/// The loaded resource snapshot (upstream the getter bundle).
#[derive(Debug, Clone, Default)]
pub struct ResourceSnapshot {
    pub skills: Vec<LoadedSkill>,
    pub skill_diagnostics: Vec<ResourceDiagnostic>,
    pub prompts: Vec<LoadedPrompt>,
    pub prompt_diagnostics: Vec<ResourceDiagnostic>,
    pub themes: Vec<LoadedTheme>,
    pub theme_diagnostics: Vec<ResourceDiagnostic>,
    pub agents_files: Vec<(PathBuf, String)>,
    pub system_prompt: Option<String>,
    pub system_prompt_source_path: Option<PathBuf>,
    pub append_system_prompt: Vec<String>,
    pub append_system_prompt_source_paths: Vec<PathBuf>,
}

/// Paths contributed by extensions (upstream `ResourceExtensionPaths`).
#[derive(Debug, Clone, Default)]
pub struct ResourceExtensionPaths {
    pub skill_paths: Vec<(String, PathMetadata)>,
    pub prompt_paths: Vec<(String, PathMetadata)>,
    pub theme_paths: Vec<(String, PathMetadata)>,
}

/// Options for [`ResourceLoader::new`] (upstream
/// `DefaultResourceLoaderOptions`).
#[derive(Debug, Clone, Default)]
pub struct ResourceLoaderOptions {
    pub agent_dir: String,
    pub additional_extension_paths: Vec<String>,
    pub additional_skill_paths: Vec<String>,
    pub additional_prompt_template_paths: Vec<String>,
    pub additional_theme_paths: Vec<String>,
    pub no_extensions: bool,
    pub no_skills: bool,
    pub no_prompt_templates: bool,
    pub no_themes: bool,
    pub no_context_files: bool,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<Vec<String>>,
}

/// The default resource loader (upstream `DefaultResourceLoader`), minus
/// the extension runtime.
pub struct ResourceLoader {
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings: std::sync::Arc<std::sync::Mutex<SettingsManager>>,
    package_manager: crate::core::package_manager::DefaultPackageManager,
    options: ResourceLoaderOptions,

    extension_skill_source_infos: BTreeMap<String, SourceInfo>,
    extension_prompt_source_infos: BTreeMap<String, SourceInfo>,
    extension_theme_source_infos: BTreeMap<String, SourceInfo>,
    resource_metadata_by_path: BTreeMap<PathBuf, PathMetadata>,
    last_skill_paths: Vec<String>,
    last_prompt_paths: Vec<String>,
    last_theme_paths: Vec<String>,
    loaded: bool,

    snapshot: ResourceSnapshot,
    /// Injected extension paths (host-provided; upstream loads real
    /// extensions through a JS runtime).
    extension_paths: Vec<String>,
}

impl ResourceLoader {
    pub fn new(
        cwd: &str,
        options: ResourceLoaderOptions,
        settings: std::sync::Arc<std::sync::Mutex<SettingsManager>>,
    ) -> Self {
        let resolved_cwd = resolve_to_cwd(cwd, "/");
        let agent_dir = resolve_to_cwd(&options.agent_dir, "/");
        let package_manager = crate::core::package_manager::DefaultPackageManager::new(
            &resolved_cwd.to_string_lossy(),
            &agent_dir,
            settings.clone(),
        );
        Self {
            cwd: resolved_cwd,
            agent_dir,
            settings,
            package_manager,
            options,
            extension_skill_source_infos: BTreeMap::new(),
            extension_prompt_source_infos: BTreeMap::new(),
            extension_theme_source_infos: BTreeMap::new(),
            resource_metadata_by_path: BTreeMap::new(),
            last_skill_paths: Vec::new(),
            last_prompt_paths: Vec::new(),
            last_theme_paths: Vec::new(),
            loaded: false,
            snapshot: ResourceSnapshot::default(),
            extension_paths: Vec::new(),
        }
    }

    /// Provide extension paths from the host (upstream loads real
    /// extensions; the port tracks the paths only).
    pub fn set_extension_paths(&mut self, paths: Vec<String>) {
        self.extension_paths = paths;
    }

    pub fn snapshot(&self) -> &ResourceSnapshot {
        &self.snapshot
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Register extension-contributed resource paths (upstream
    /// `extendResources`).
    pub fn extend_resources(&mut self, paths: ResourceExtensionPaths) {
        let skill_paths = self.normalize_extension_paths(paths.skill_paths);
        let prompt_paths = self.normalize_extension_paths(paths.prompt_paths);
        let theme_paths = self.normalize_extension_paths(paths.theme_paths);

        for (path, metadata) in &skill_paths {
            self.extension_skill_source_infos.insert(
                path.clone(),
                create_source_info(path, &pm_to_info_metadata(metadata)),
            );
        }
        for (path, metadata) in &prompt_paths {
            self.extension_prompt_source_infos.insert(
                path.clone(),
                create_source_info(path, &pm_to_info_metadata(metadata)),
            );
        }
        for (path, metadata) in &theme_paths {
            self.extension_theme_source_infos.insert(
                path.clone(),
                create_source_info(path, &pm_to_info_metadata(metadata)),
            );
        }

        if !skill_paths.is_empty() {
            let names: Vec<String> = skill_paths.iter().map(|(p, _)| p.clone()).collect();
            self.last_skill_paths = self.merge_paths(&self.last_skill_paths.clone(), &names);
            let metadata = self.resource_metadata_by_path.clone();
            self.update_skills_from_paths(&self.last_skill_paths.clone(), Some(&metadata));
        }
        if !prompt_paths.is_empty() {
            let names: Vec<String> = prompt_paths.iter().map(|(p, _)| p.clone()).collect();
            self.last_prompt_paths = self.merge_paths(&self.last_prompt_paths.clone(), &names);
            let metadata = self.resource_metadata_by_path.clone();
            self.update_prompts_from_paths(&self.last_prompt_paths.clone(), Some(&metadata));
        }
        if !theme_paths.is_empty() {
            let names: Vec<String> = theme_paths.iter().map(|(p, _)| p.clone()).collect();
            self.last_theme_paths = self.merge_paths(&self.last_theme_paths.clone(), &names);
            let metadata = self.resource_metadata_by_path.clone();
            self.update_themes_from_paths(&self.last_theme_paths.clone(), Some(&metadata));
        }
    }

    /// Reload all resources (upstream `reload`). `resolve_project_trust`
    /// mirrors the trust-resolution hook: when set, a pre-trust pass runs
    /// with project settings disabled and the callback decides trust.
    pub fn reload(
        &mut self,
        resolve_project_trust: Option<&mut dyn FnMut() -> bool>,
    ) -> Result<(), String> {
        let mut pre_trust_metadata: Option<BTreeMap<PathBuf, PathMetadata>> = None;
        if let Some(callback) = resolve_project_trust {
            // Bootstrap pass: untrusted project settings.
            self.settings.lock().unwrap().set_project_trusted(false);
            self.settings.lock().unwrap().reload();
            let resolved = self.package_manager.resolve(None)?;
            pre_trust_metadata = Some(resolved_metadata(&resolved));
            let project_trusted = callback();
            self.settings
                .lock()
                .unwrap()
                .set_project_trusted(project_trusted);
        }

        // reload() preserves projectTrusted and reloads settings for it.
        self.settings.lock().unwrap().reload();
        let resolved_paths = self.package_manager.resolve(None)?;
        let cli_extension_paths = self.package_manager.resolve_extension_sources(
            &self.options.additional_extension_paths.clone(),
            false,
            true,
        )?;

        self.resource_metadata_by_path = BTreeMap::new();

        let enabled_extensions: Vec<String> = resolved_paths
            .extensions
            .iter()
            .filter(|r| r.enabled)
            .map(|r| {
                self.resource_metadata_by_path
                    .entry(r.path.clone())
                    .or_insert_with(|| r.metadata.clone());
                r.path.to_string_lossy().to_string()
            })
            .collect();
        let enabled_skill_resources: Vec<(String, PathMetadata)> = resolved_paths
            .skills
            .iter()
            .filter(|r| r.enabled)
            .map(|r| {
                self.resource_metadata_by_path
                    .entry(r.path.clone())
                    .or_insert_with(|| r.metadata.clone());
                (r.path.to_string_lossy().to_string(), r.metadata.clone())
            })
            .collect();
        let enabled_prompts: Vec<String> = resolved_paths
            .prompts
            .iter()
            .filter(|r| r.enabled)
            .map(|r| {
                self.resource_metadata_by_path
                    .entry(r.path.clone())
                    .or_insert_with(|| r.metadata.clone());
                r.path.to_string_lossy().to_string()
            })
            .collect();
        let enabled_themes: Vec<String> = resolved_paths
            .themes
            .iter()
            .filter(|r| r.enabled)
            .map(|r| {
                self.resource_metadata_by_path
                    .entry(r.path.clone())
                    .or_insert_with(|| r.metadata.clone());
                r.path.to_string_lossy().to_string()
            })
            .collect();

        // Map skill directories to their SKILL.md files (upstream
        // `mapSkillPath`).
        let enabled_skills: Vec<String> = enabled_skill_resources
            .iter()
            .map(|(path, metadata)| self.map_skill_path(Path::new(path), metadata))
            .collect();

        // CLI paths metadata.
        for r in cli_extension_paths
            .extensions
            .iter()
            .chain(cli_extension_paths.skills.iter())
        {
            self.resource_metadata_by_path
                .entry(r.path.clone())
                .or_insert_with(|| PmPathMetadata {
                    source: "cli".to_string(),
                    scope: SourceScope::Temporary,
                    origin: ResourceOrigin::TopLevel,
                    base_dir: None,
                });
        }

        let cli_enabled_extensions: Vec<String> = cli_extension_paths
            .extensions
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.path.to_string_lossy().to_string())
            .collect();
        let cli_enabled_skills: Vec<String> = cli_extension_paths
            .skills
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.path.to_string_lossy().to_string())
            .collect();
        let cli_enabled_prompts: Vec<String> = cli_extension_paths
            .prompts
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.path.to_string_lossy().to_string())
            .collect();
        let cli_enabled_themes: Vec<String> = cli_extension_paths
            .themes
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.path.to_string_lossy().to_string())
            .collect();

        // Extension paths: merged and checked for missing local entries.
        let extension_paths = if self.options.no_extensions {
            cli_enabled_extensions
        } else {
            let mut merged = self.merge_paths(&cli_enabled_extensions, &enabled_extensions);
            merged.extend(self.extension_paths.iter().cloned());
            merged
        };
        for p in &self.options.additional_extension_paths {
            if crate::core::package_manager::is_local_path(p) {
                let resolved = self.resolve_resource_path(p);
                if !Path::new(&resolved).exists() {
                    eprintln!("Extension path does not exist: {}", resolved);
                }
            }
        }
        let _ = extension_paths; // consumed by the (unported) extension runtime

        let skill_paths = if self.options.no_skills {
            self.merge_paths(&cli_enabled_skills, &self.options.additional_skill_paths)
        } else {
            let mut primary = cli_enabled_skills;
            primary.extend(enabled_skills);
            self.merge_paths(&primary, &self.options.additional_skill_paths)
        };
        self.last_skill_paths = skill_paths.clone();
        let metadata = self.resource_metadata_by_path.clone();
        self.update_skills_from_paths(&skill_paths, Some(&metadata));
        for p in &self.options.additional_skill_paths {
            if crate::core::package_manager::is_local_path(p) {
                let resolved = self.resolve_resource_path(p);
                if !Path::new(&resolved).exists()
                    && !self
                        .snapshot
                        .skill_diagnostics
                        .iter()
                        .any(|d| diagnostic_path(d) == Some(resolved.as_str()))
                {
                    self.snapshot
                        .skill_diagnostics
                        .push(ResourceDiagnostic::Warning {
                            message: "Skill path does not exist".to_string(),
                            path: resolved.clone(),
                        });
                }
            }
        }

        let prompt_paths = if self.options.no_prompt_templates {
            self.merge_paths(
                &cli_enabled_prompts,
                &self.options.additional_prompt_template_paths,
            )
        } else {
            let mut primary = cli_enabled_prompts;
            primary.extend(enabled_prompts);
            self.merge_paths(&primary, &self.options.additional_prompt_template_paths)
        };
        self.last_prompt_paths = prompt_paths.clone();
        let metadata = self.resource_metadata_by_path.clone();
        self.update_prompts_from_paths(&prompt_paths, Some(&metadata));
        for p in &self.options.additional_prompt_template_paths {
            if crate::core::package_manager::is_local_path(p) {
                let resolved = self.resolve_resource_path(p);
                if !Path::new(&resolved).exists()
                    && !self
                        .snapshot
                        .prompt_diagnostics
                        .iter()
                        .any(|d| diagnostic_path(d) == Some(resolved.as_str()))
                {
                    self.snapshot
                        .prompt_diagnostics
                        .push(ResourceDiagnostic::Warning {
                            message: "Prompt template path does not exist".to_string(),
                            path: resolved.clone(),
                        });
                }
            }
        }

        let theme_paths = if self.options.no_themes {
            self.merge_paths(&cli_enabled_themes, &self.options.additional_theme_paths)
        } else {
            let mut primary = cli_enabled_themes;
            primary.extend(enabled_themes);
            self.merge_paths(&primary, &self.options.additional_theme_paths)
        };
        self.last_theme_paths = theme_paths.clone();
        let metadata = self.resource_metadata_by_path.clone();
        self.update_themes_from_paths(&theme_paths, Some(&metadata));
        for p in &self.options.additional_theme_paths {
            let resolved = self.resolve_resource_path(p);
            if !Path::new(&resolved).exists()
                && !self
                    .snapshot
                    .theme_diagnostics
                    .iter()
                    .any(|d| diagnostic_path(d) == Some(resolved.as_str()))
            {
                self.snapshot
                    .theme_diagnostics
                    .push(ResourceDiagnostic::Warning {
                        message: "theme path does not exist".to_string(),
                        path: resolved.clone(),
                    });
            }
        }

        // Context files.
        self.snapshot.agents_files = if self.options.no_context_files {
            Vec::new()
        } else {
            load_project_context_files(
                &self.cwd.to_string_lossy(),
                &self.agent_dir.to_string_lossy(),
            )
        };

        // System prompt discovery.
        let system_prompt_source = self
            .options
            .system_prompt
            .clone()
            .or_else(|| self.discover_system_prompt_file());
        self.snapshot.system_prompt = system_prompt_source
            .as_ref()
            .and_then(|s| resolve_prompt_input(s, "system prompt"));
        self.snapshot.system_prompt_source_path = system_prompt_source
            .as_ref()
            .filter(|s| Path::new(s).exists())
            .map(|s| resolve_to_cwd(s, &self.cwd.to_string_lossy()));

        let append_sources: Vec<String> = self
            .options
            .append_system_prompt
            .clone()
            .or_else(|| self.discover_append_system_prompt_file().map(|s| vec![s]))
            .unwrap_or_default();
        self.snapshot.append_system_prompt = append_sources
            .iter()
            .filter_map(|s| resolve_prompt_input(s, "append system prompt"))
            .collect();
        self.snapshot.append_system_prompt_source_paths = append_sources
            .iter()
            .filter(|s| Path::new(s).exists())
            .map(|s| resolve_to_cwd(s, &self.cwd.to_string_lossy()))
            .collect();

        let _ = pre_trust_metadata;
        self.loaded = true;
        Ok(())
    }

    fn map_skill_path(&mut self, resource_path: &Path, metadata: &PathMetadata) -> String {
        // Only auto-discovered and package resources map to SKILL.md.
        if resource_path.is_file() {
            return resource_path.to_string_lossy().to_string();
        }
        let skill_file = resource_path.join("SKILL.md");
        if skill_file.exists() {
            self.resource_metadata_by_path
                .entry(skill_file.clone())
                .or_insert_with(|| metadata.clone());
            return skill_file.to_string_lossy().to_string();
        }
        resource_path.to_string_lossy().to_string()
    }

    fn normalize_extension_paths(
        &self,
        entries: Vec<(String, PathMetadata)>,
    ) -> Vec<(String, PathMetadata)> {
        entries
            .into_iter()
            .map(|(path, mut metadata)| {
                if let Some(base_dir) = &metadata.base_dir {
                    metadata.base_dir = Some(
                        self.resolve_resource_path(&base_dir.to_string_lossy())
                            .into(),
                    );
                }
                (self.resolve_resource_path(&path), metadata)
            })
            .collect()
    }

    fn update_skills_from_paths(
        &mut self,
        skill_paths: &[String],
        metadata_by_path: Option<&BTreeMap<PathBuf, PathMetadata>>,
    ) {
        let result = if self.options.no_skills && skill_paths.is_empty() {
            crate::core::skills::LoadSkillsResult::default()
        } else {
            load_skills(&LoadSkillsOptions {
                cwd: &self.cwd,
                agent_dir: &self.agent_dir,
                skill_paths,
                include_defaults: false,
            })
        };
        self.snapshot.skill_diagnostics = result.diagnostics;
        self.snapshot.skills = result
            .skills
            .into_iter()
            .map(|skill| {
                let source_info = self
                    .find_source_info_for_path(
                        &skill.file_path,
                        Some(&self.extension_skill_source_infos),
                        metadata_by_path,
                    )
                    .unwrap_or_else(|| self.get_default_source_info_for_path(&skill.file_path));
                LoadedSkill {
                    name: skill.name,
                    description: skill.description,
                    file_path: skill.file_path,
                    base_dir: skill.base_dir,
                    disable_model_invocation: skill.disable_model_invocation,
                    source_info,
                }
            })
            .collect();
    }

    fn update_prompts_from_paths(
        &mut self,
        prompt_paths: &[String],
        metadata_by_path: Option<&BTreeMap<PathBuf, PathMetadata>>,
    ) {
        let (prompts, prompt_diagnostics) = if self.options.no_prompt_templates
            && prompt_paths.is_empty()
        {
            (Vec::new(), Vec::new())
        } else {
            let all =
                load_prompt_templates(&crate::core::prompt_templates::LoadPromptTemplatesOptions {
                    cwd: self.cwd.to_string_lossy().to_string(),
                    agent_dir: self.agent_dir.to_string_lossy().to_string(),
                    prompt_paths: prompt_paths.to_vec(),
                    include_defaults: false,
                });
            self.dedupe_prompts(all)
        };
        self.snapshot.prompts = prompts
            .into_iter()
            .map(|prompt| {
                let source_info = self
                    .find_source_info_for_path(
                        &prompt.file_path,
                        Some(&self.extension_prompt_source_infos),
                        metadata_by_path,
                    )
                    .unwrap_or_else(|| self.get_default_source_info_for_path(&prompt.file_path));
                LoadedPrompt {
                    name: prompt.name,
                    description: prompt.description,
                    argument_hint: prompt.argument_hint,
                    content: prompt.content,
                    file_path: prompt.file_path,
                    source_info,
                }
            })
            .collect();
        self.snapshot.prompt_diagnostics = prompt_diagnostics;
    }

    fn update_themes_from_paths(
        &mut self,
        theme_paths: &[String],
        metadata_by_path: Option<&BTreeMap<PathBuf, PathMetadata>>,
    ) {
        let (mut themes, mut diagnostics) = if self.options.no_themes && theme_paths.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            self.load_themes(theme_paths)
        };
        let (deduped, deduped_diagnostics) = self.dedupe_themes(&mut themes);
        themes = deduped;
        diagnostics.extend(deduped_diagnostics);
        self.snapshot.theme_diagnostics = diagnostics;
        for theme in &mut themes {
            if let Some(source_path) = theme.source_path.clone() {
                theme.source_info = self
                    .find_source_info_for_path(
                        &source_path,
                        Some(&self.extension_theme_source_infos),
                        metadata_by_path,
                    )
                    .unwrap_or_else(|| self.get_default_source_info_for_path(&source_path));
            }
        }
        self.snapshot.themes = themes;
    }

    fn find_source_info_for_path(
        &self,
        resource_path: &str,
        extra_source_infos: Option<&BTreeMap<String, SourceInfo>>,
        metadata_by_path: Option<&BTreeMap<PathBuf, PathMetadata>>,
    ) -> Option<SourceInfo> {
        if resource_path.is_empty() {
            return None;
        }
        if resource_path.starts_with('<') {
            return Some(self.get_default_source_info_for_path(resource_path));
        }

        let normalized = PathBuf::from(resource_path);
        if let Some(extra) = extra_source_infos {
            for (source_path, source_info) in extra {
                let normalized_source = PathBuf::from(source_path);
                if normalized == normalized_source
                    || normalized.starts_with(format!(
                        "{}{}",
                        normalized_source.display(),
                        std::path::MAIN_SEPARATOR
                    ))
                {
                    let mut info = source_info.clone();
                    info.path = resource_path.to_string();
                    return Some(info);
                }
            }
        }

        if let Some(metadata_map) = metadata_by_path {
            if let Some(metadata) = metadata_map.get(&normalized) {
                return Some(create_source_info(
                    resource_path,
                    &pm_to_info_metadata(metadata),
                ));
            }
            for (source_path, metadata) in metadata_map {
                if normalized == *source_path
                    || normalized.starts_with(format!(
                        "{}{}",
                        source_path.display(),
                        std::path::MAIN_SEPARATOR
                    ))
                {
                    return Some(create_source_info(
                        resource_path,
                        &pm_to_info_metadata(metadata),
                    ));
                }
            }
        }

        None
    }

    fn get_default_source_info_for_path(&self, file_path: &str) -> SourceInfo {
        if file_path.starts_with('<') && file_path.ends_with('>') {
            let inner = &file_path[1..file_path.len() - 1];
            let source = inner.split(':').next().unwrap_or("temporary");
            return SourceInfo {
                path: file_path.to_string(),
                source: if source.is_empty() {
                    "temporary".to_string()
                } else {
                    source.to_string()
                },
                scope: InfoScope::Temporary,
                origin: SourceOrigin::TopLevel,
                base_dir: None,
            };
        }

        let normalized = PathBuf::from(file_path);
        let agent_roots = [
            self.agent_dir.join("skills"),
            self.agent_dir.join("prompts"),
            self.agent_dir.join("themes"),
            self.agent_dir.join("extensions"),
        ];
        let project_roots = [
            self.cwd.join(CONFIG_DIR_NAME).join("skills"),
            self.cwd.join(CONFIG_DIR_NAME).join("prompts"),
            self.cwd.join(CONFIG_DIR_NAME).join("themes"),
            self.cwd.join(CONFIG_DIR_NAME).join("extensions"),
        ];

        for root in &agent_roots {
            if self.is_under_path(&normalized, root) {
                return SourceInfo {
                    path: file_path.to_string(),
                    source: "local".to_string(),
                    scope: InfoScope::User,
                    origin: SourceOrigin::TopLevel,
                    base_dir: Some(root.to_string_lossy().to_string()),
                };
            }
        }
        for root in &project_roots {
            if self.is_under_path(&normalized, root) {
                return SourceInfo {
                    path: file_path.to_string(),
                    source: "local".to_string(),
                    scope: InfoScope::Project,
                    origin: SourceOrigin::TopLevel,
                    base_dir: Some(root.to_string_lossy().to_string()),
                };
            }
        }

        let base_dir = if normalized.is_dir() {
            normalized.to_string_lossy().to_string()
        } else {
            normalized
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(normalized.clone())
                .to_string_lossy()
                .to_string()
        };
        SourceInfo {
            path: file_path.to_string(),
            source: "local".to_string(),
            scope: InfoScope::Temporary,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(base_dir),
        }
    }

    fn merge_paths(&self, primary: &[String], additional: &[String]) -> Vec<String> {
        let mut merged = Vec::new();
        let mut seen = BTreeSet::new();
        for p in primary.iter().chain(additional.iter()) {
            let resolved = self.resolve_resource_path(p);
            let canonical = canonicalize(Path::new(&resolved));
            if !seen.insert(canonical) {
                continue;
            }
            merged.push(resolved);
        }
        merged
    }

    fn resolve_resource_path(&self, p: &str) -> String {
        resolve_to_cwd(p.trim(), &self.cwd.to_string_lossy())
            .to_string_lossy()
            .to_string()
    }

    fn load_themes(&self, paths: &[String]) -> (Vec<LoadedTheme>, Vec<ResourceDiagnostic>) {
        let mut themes = Vec::new();
        let mut diagnostics = Vec::new();
        for p in paths {
            let resolved = self.resolve_resource_path(p);
            let path = Path::new(&resolved);
            if !path.exists() {
                diagnostics.push(ResourceDiagnostic::Warning {
                    message: "theme path does not exist".to_string(),
                    path: resolved.clone(),
                });
                continue;
            }
            if path.is_dir() {
                self.load_themes_from_dir(path, &mut themes, &mut diagnostics);
            } else if path.is_file() && resolved.ends_with(".json") {
                self.load_theme_from_file(path, &mut themes, &mut diagnostics);
            } else {
                diagnostics.push(ResourceDiagnostic::Warning {
                    message: "theme path is not a json file".to_string(),
                    path: resolved,
                });
            }
        }
        (themes, diagnostics)
    }

    fn load_themes_from_dir(
        &self,
        dir: &Path,
        themes: &mut Vec<LoadedTheme>,
        diagnostics: &mut Vec<ResourceDiagnostic>,
    ) {
        if !dir.is_dir() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            diagnostics.push(ResourceDiagnostic::Warning {
                message: "failed to read theme directory".to_string(),
                path: dir.to_string_lossy().to_string(),
            });
            return;
        };
        let mut names: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.to_string_lossy().ends_with(".json"))
            .collect();
        names.sort();
        for path in names {
            self.load_theme_from_file(&path, themes, diagnostics);
        }
    }

    fn load_theme_from_file(
        &self,
        file_path: &Path,
        themes: &mut Vec<LoadedTheme>,
        diagnostics: &mut Vec<ResourceDiagnostic>,
    ) {
        match std::fs::read_to_string(file_path)
            .map_err(|e| e.to_string())
            .and_then(|content| {
                serde_json::from_str::<Value>(content.strip_prefix('\u{feff}').unwrap_or(&content))
                    .map_err(|e| e.to_string())
            }) {
            Ok(data) => {
                let name = data.get("name").and_then(Value::as_str).map(str::to_string);
                themes.push(LoadedTheme {
                    name,
                    source_path: Some(file_path.to_string_lossy().to_string()),
                    data,
                    source_info: SourceInfo::default(),
                });
            }
            Err(message) => diagnostics.push(ResourceDiagnostic::Warning {
                message,
                path: file_path.to_string_lossy().to_string(),
            }),
        }
    }

    /// First prompt with a name wins; later duplicates produce collision
    /// diagnostics (upstream `dedupePrompts`).
    fn dedupe_prompts(
        &self,
        prompts: Vec<PromptTemplate>,
    ) -> (Vec<PromptTemplate>, Vec<ResourceDiagnostic>) {
        let mut seen: BTreeMap<String, PromptTemplate> = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for prompt in prompts {
            if seen.contains_key(&prompt.name) {
                diagnostics.push(ResourceDiagnostic::Collision {
                    message: format!("name \"/{}\" collision", prompt.name),
                    path: prompt.file_path.clone(),
                });
            } else {
                seen.insert(prompt.name.clone(), prompt);
            }
        }
        (seen.into_values().collect(), diagnostics)
    }

    /// First theme with a name wins; later duplicates produce collision
    /// diagnostics (upstream `dedupeThemes`).
    fn dedupe_themes(
        &self,
        themes: &mut Vec<LoadedTheme>,
    ) -> (Vec<LoadedTheme>, Vec<ResourceDiagnostic>) {
        let mut seen: BTreeMap<String, LoadedTheme> = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for t in themes.drain(..) {
            let name = t.name.clone().unwrap_or_else(|| "unnamed".to_string());
            if let std::collections::btree_map::Entry::Vacant(entry) = seen.entry(name.clone()) {
                entry.insert(t);
            } else {
                diagnostics.push(ResourceDiagnostic::Collision {
                    message: format!("name \"{name}\" collision"),
                    path: t
                        .source_path
                        .clone()
                        .unwrap_or_else(|| "<builtin>".to_string()),
                });
            }
        }
        (seen.into_values().collect(), diagnostics)
    }

    fn discover_system_prompt_file(&self) -> Option<String> {
        let project_path = self.cwd.join(CONFIG_DIR_NAME).join("SYSTEM.md");
        if self.settings.lock().unwrap().is_project_trusted() && project_path.exists() {
            return Some(project_path.to_string_lossy().to_string());
        }
        let global_path = self.agent_dir.join("SYSTEM.md");
        if global_path.exists() {
            return Some(global_path.to_string_lossy().to_string());
        }
        None
    }

    fn discover_append_system_prompt_file(&self) -> Option<String> {
        let project_path = self.cwd.join(CONFIG_DIR_NAME).join("APPEND_SYSTEM.md");
        if self.settings.lock().unwrap().is_project_trusted() && project_path.exists() {
            return Some(project_path.to_string_lossy().to_string());
        }
        let global_path = self.agent_dir.join("APPEND_SYSTEM.md");
        if global_path.exists() {
            return Some(global_path.to_string_lossy().to_string());
        }
        None
    }

    fn is_under_path(&self, target: &Path, root: &Path) -> bool {
        if target == root {
            return true;
        }
        target.starts_with(root)
    }

    /// Detect tool/flag name conflicts across extensions (upstream
    /// `detectExtensionConflicts`); applies to the host-injected extension
    /// list.
    pub fn detect_extension_conflicts(
        extensions: &[(String, Vec<String>, Vec<String>)],
    ) -> Vec<(String, String)> {
        let mut conflicts = Vec::new();
        let mut tool_owners: BTreeMap<String, String> = BTreeMap::new();
        let mut flag_owners: BTreeMap<String, String> = BTreeMap::new();
        for (path, tools, flags) in extensions {
            for tool_name in tools {
                match tool_owners.get(tool_name) {
                    Some(existing) if existing != path => conflicts.push((
                        path.clone(),
                        format!("Tool \"{tool_name}\" conflicts with {existing}"),
                    )),
                    _ => {
                        tool_owners.insert(tool_name.clone(), path.clone());
                    }
                }
            }
            for flag_name in flags {
                match flag_owners.get(flag_name) {
                    Some(existing) if existing != path => conflicts.push((
                        path.clone(),
                        format!("Flag \"--{flag_name}\" conflicts with {existing}"),
                    )),
                    _ => {
                        flag_owners.insert(flag_name.clone(), path.clone());
                    }
                }
            }
        }
        conflicts
    }
}

fn resolved_metadata(
    paths: &crate::core::package_manager::ResolvedPaths,
) -> BTreeMap<PathBuf, PathMetadata> {
    let mut map = BTreeMap::new();
    for r in paths
        .extensions
        .iter()
        .chain(paths.skills.iter())
        .chain(paths.prompts.iter())
        .chain(paths.themes.iter())
    {
        map.insert(r.path.clone(), r.metadata.clone());
    }
    map
}

fn diagnostic_path(diagnostic: &ResourceDiagnostic) -> Option<&str> {
    match diagnostic {
        ResourceDiagnostic::Warning { path, .. } | ResourceDiagnostic::Collision { path, .. } => {
            Some(path)
        }
    }
}

fn resolve_prompt_input(input: &str, description: &str) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    if Path::new(input).exists() {
        match std::fs::read_to_string(input) {
            Ok(content) => {
                return Some(
                    content
                        .strip_prefix('\u{feff}')
                        .unwrap_or(&content)
                        .to_string(),
                );
            }
            Err(error) => {
                eprintln!("Warning: Could not read {description} file {input}: {error}");
                return Some(input.to_string());
            }
        }
    }
    Some(input.to_string())
}

// Re-export the flat diagnostic shape for hosts bridging both forms.
pub type FlatResourceDiagnostic = FlatDiagnostic;
