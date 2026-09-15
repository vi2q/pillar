//! Port of packages/coding-agent/src/core/package-manager.ts (pi
//! v0.84.3): package source parsing (npm spec / git URL / local path),
//! resource collection from packages and settings, pattern-based
//! enable/disable filters, install-path computation, and the
//! install/remove/update command runners.
//!
//! divergences: hosted-git-info is not ported — `parse_git_url` (now in
//! "utils/git.rs", upstream `utils/git.ts`) handles protocol URLs, scp-like
//! shorthands, and `git:`-prefixed shorthand `host/owner/repo` forms via the
//! generic parser only; command runners use `std::process::Command` (no
//! stdout-takeover stdio mode); resolve() is synchronous.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use globset::GlobBuilder;
use semver::{Version, VersionReq};

use crate::core::settings_manager::{CONFIG_DIR_NAME, PackageSource, SettingsManager};
// Re-exported for the pre-existing call sites; upstream `utils/git.ts` owns
// this logic and `package-manager.ts` imports it.
use crate::core::tools::path_utils::resolve_to_cwd;
use crate::core::trust_manager::{PiManifest, read_pi_manifest};
pub use crate::utils::git::{GitSource, parse_git_url};

const NETWORK_TIMEOUT_MS: u64 = 10_000;
pub const UPDATE_CHECK_CONCURRENCY: usize = 4;
pub const GIT_UPDATE_CONCURRENCY: usize = 4;

// ============================================================================
// Types
// ============================================================================

/// Source scope (upstream `SourceScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceScope {
    User,
    Project,
    Temporary,
}

/// Resource origin metadata (upstream `PathMetadata`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMetadata {
    pub source: String,
    pub scope: SourceScope,
    pub origin: ResourceOrigin,
    pub base_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceOrigin {
    Package,
    TopLevel,
}

/// A resolved resource path (upstream `ResolvedResource`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedResource {
    pub path: PathBuf,
    pub enabled: bool,
    pub metadata: PathMetadata,
}

/// Resolved resource sets (upstream `ResolvedPaths`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub extensions: Vec<ResolvedResource>,
    pub skills: Vec<ResolvedResource>,
    pub prompts: Vec<ResolvedResource>,
    pub themes: Vec<ResolvedResource>,
}

/// Action when a configured source is missing (upstream
/// `MissingSourceAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingSourceAction {
    Install,
    Skip,
    Error,
}

/// Progress event (upstream `ProgressEvent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressEvent {
    pub kind: ProgressKind,
    pub action: ProgressAction,
    pub source: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressKind {
    Start,
    Progress,
    Complete,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressAction {
    Install,
    Remove,
    Update,
    Clone,
    Pull,
}

/// A package with an available update (upstream `PackageUpdate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageUpdate {
    pub source: String,
    pub display_name: String,
    pub kind: UpdateKind,
    pub scope: SourceScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateKind {
    Npm,
    Git,
}

/// A configured package (upstream `ConfiguredPackage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredPackage {
    pub source: String,
    pub scope: SourceScope,
    pub filtered: bool,
    pub installed_path: Option<PathBuf>,
}

/// An npm source (upstream `NpmSource`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmSource {
    pub spec: String,
    pub name: String,
    pub version: Option<String>,
    pub range: Option<String>,
    pub pinned: bool,
}

/// A parsed package source (upstream `ParsedSource`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSource {
    Npm(NpmSource),
    Git(GitSource),
    Local(String),
}

/// Package filter from object-form sources (upstream `PackageFilter`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageFilter {
    pub autoload: Option<bool>,
    pub extensions: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub prompts: Option<Vec<String>>,
    pub themes: Option<Vec<String>>,
}

// ============================================================================
// npm spec / version helpers
// ============================================================================

/// Parse an npm spec into name + version (upstream `parseNpmSpec`):
/// `^(@?[^@]+(?:\/[^@]+)?)(?:@(.+))?$` — the name cannot contain `@`.
pub fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let bytes = spec.as_bytes();
    let start = if bytes.first() == Some(&b'@') { 1 } else { 0 };
    match spec[start..].find('@') {
        Some(idx) => {
            let at = start + idx;
            (spec[..at].to_string(), Some(spec[at + 1..].to_string()))
        }
        None => (spec.to_string(), None),
    }
}

fn is_exact_npm_version(version: Option<&str>) -> bool {
    version.is_some_and(|v| Version::parse(v).is_ok())
}

fn get_npm_version_range(version: Option<&str>) -> Option<String> {
    version.and_then(|v| {
        if VersionReq::parse(v).is_ok() {
            Some(v.to_string())
        } else {
            None
        }
    })
}

/// Parse an npm source (upstream the `npm:` branch of `parseSource`).
pub fn parse_npm_source(spec: &str) -> NpmSource {
    let (name, version) = parse_npm_spec(spec);
    NpmSource {
        spec: spec.to_string(),
        name,
        version: version.clone(),
        range: get_npm_version_range(version.as_deref()),
        pinned: is_exact_npm_version(version.as_deref()),
    }
}

fn version_gt(target: &str, installed: &str) -> bool {
    match (Version::parse(target), Version::parse(installed)) {
        (Ok(target), Ok(installed)) => target > installed,
        _ => true,
    }
}

fn satisfies_range(version: &str, range: &str) -> bool {
    match (Version::parse(version), VersionReq::parse(range)) {
        (Ok(version), Ok(req)) => req.matches(&version),
        _ => false,
    }
}

/// `maxSatisfying(versions, range)` or the highest version when no range.
fn max_satisfying_version(versions: &[String], range: Option<&str>) -> Option<String> {
    if let Some(range) = range {
        let req = VersionReq::parse(range).ok()?;
        return versions
            .iter()
            .filter_map(|v| Version::parse(v).ok().map(|v| (v.clone(), v)))
            .filter(|(_, parsed)| req.matches(parsed))
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, parsed)| parsed.to_string());
    }
    versions
        .iter()
        .filter_map(|v| Version::parse(v).ok())
        .max()
        .map(|v| v.to_string())
}

/// True when the value is NOT a package source or remote URL protocol
/// (upstream `isLocalPath`).
pub fn is_local_path(value: &str) -> bool {
    let trimmed = value.trim();
    !(trimmed.starts_with("npm:")
        || trimmed.starts_with("git:")
        || trimmed.starts_with("github:")
        || trimmed.starts_with("http:")
        || trimmed.starts_with("https:")
        || trimmed.starts_with("ssh:"))
}

/// Parse a package source string (upstream `parseSource`).
pub fn parse_source(source: &str) -> ParsedSource {
    if let Some(spec) = source.strip_prefix("npm:") {
        return ParsedSource::Npm(parse_npm_source(spec.trim()));
    }
    if is_local_path(source) {
        return ParsedSource::Local(source.to_string());
    }
    if let Some(git) = parse_git_url(source) {
        return ParsedSource::Git(git);
    }
    ParsedSource::Local(source.to_string())
}

// ============================================================================
// Pattern matching (upstream applyPatterns / overrides)
// ============================================================================

fn is_pattern(s: &str) -> bool {
    s.starts_with('!')
        || s.starts_with('+')
        || s.starts_with('-')
        || s.contains('*')
        || s.contains('?')
}

fn is_override_pattern(s: &str) -> bool {
    s.starts_with('!') || s.starts_with('+') || s.starts_with('-')
}

fn has_glob_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

fn to_posix_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn glob_match(pattern: &str, candidate: &str) -> bool {
    // Upstream uses minimatch on POSIX-style paths; globset with literal
    // separator semantics approximates it. An empty pattern matches nothing.
    if pattern.is_empty() {
        return false;
    }
    let glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build();
    match glob {
        Ok(glob) => glob.compile_matcher().is_match(candidate),
        Err(_) => pattern == candidate,
    }
}

fn relative_path_string(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .map(to_posix_path)
        .unwrap_or_else(|_| to_posix_path(path))
}

/// Match a file path against patterns via rel path, basename, or full
/// posix path (upstream `matchesAnyPattern`), with SKILL.md parent-dir
/// variants.
pub fn matches_any_pattern(file_path: &Path, patterns: &[String], base_dir: &Path) -> bool {
    let rel = relative_path_string(base_dir, file_path);
    let name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let file_path_posix = to_posix_path(file_path);
    let is_skill_file = name == "SKILL.md";
    let (parent_rel, parent_name, parent_dir_posix) = if is_skill_file {
        let parent = file_path.parent().unwrap_or(file_path);
        (
            Some(relative_path_string(base_dir, parent)),
            Some(
                parent
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ),
            Some(to_posix_path(parent)),
        )
    } else {
        (None, None, None)
    };

    patterns.iter().any(|pattern| {
        let normalized_pattern = pattern.replace('\\', "/");
        if glob_match(&normalized_pattern, &rel)
            || glob_match(&normalized_pattern, &name)
            || glob_match(&normalized_pattern, &file_path_posix)
        {
            return true;
        }
        if !is_skill_file {
            return false;
        }
        glob_match(&normalized_pattern, parent_rel.as_deref().unwrap_or(""))
            || glob_match(&normalized_pattern, parent_name.as_deref().unwrap_or(""))
            || glob_match(
                &normalized_pattern,
                parent_dir_posix.as_deref().unwrap_or(""),
            )
    })
}

fn normalize_exact_pattern(pattern: &str) -> String {
    let normalized = pattern
        .strip_prefix("./")
        .or_else(|| pattern.strip_prefix(".\\"))
        .unwrap_or(pattern);
    normalized.replace('\\', "/")
}

/// Exact-path matching (upstream `matchesAnyExactPattern`), with SKILL.md
/// parent-dir variants.
pub fn matches_any_exact_pattern(file_path: &Path, patterns: &[String], base_dir: &Path) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let rel = relative_path_string(base_dir, file_path);
    let name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let file_path_posix = to_posix_path(file_path);
    let is_skill_file = name == "SKILL.md";
    let (parent_rel, parent_dir_posix) = if is_skill_file {
        let parent = file_path.parent().unwrap_or(file_path);
        (
            Some(relative_path_string(base_dir, parent)),
            Some(to_posix_path(parent)),
        )
    } else {
        (None, None)
    };

    patterns.iter().any(|pattern| {
        let normalized = normalize_exact_pattern(pattern);
        if normalized == rel || normalized == file_path_posix {
            return true;
        }
        if !is_skill_file {
            return false;
        }
        normalized == parent_rel.as_deref().unwrap_or("")
            || normalized == parent_dir_posix.as_deref().unwrap_or("")
    })
}

/// Apply include/exclude/force-include/force-exclude patterns and return
/// the set of enabled paths (upstream `applyPatterns`).
pub fn apply_patterns(
    all_paths: &[PathBuf],
    patterns: &[String],
    base_dir: &Path,
) -> BTreeSet<PathBuf> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut force_includes = Vec::new();
    let mut force_excludes = Vec::new();

    for p in patterns {
        if let Some(rest) = p.strip_prefix('+') {
            force_includes.push(rest.to_string());
        } else if let Some(rest) = p.strip_prefix('-') {
            force_excludes.push(rest.to_string());
        } else if let Some(rest) = p.strip_prefix('!') {
            excludes.push(rest.to_string());
        } else {
            includes.push(p.clone());
        }
    }

    // Step 1: includes (or all when no includes).
    let mut result: Vec<PathBuf> = if includes.is_empty() {
        all_paths.to_vec()
    } else {
        all_paths
            .iter()
            .filter(|p| matches_any_pattern(p, &includes, base_dir))
            .cloned()
            .collect()
    };

    // Step 2: excludes.
    if !excludes.is_empty() {
        result.retain(|p| !matches_any_pattern(p, &excludes, base_dir));
    }

    // Step 3: force-includes.
    if !force_includes.is_empty() {
        for p in all_paths {
            if !result.contains(p) && matches_any_exact_pattern(p, &force_includes, base_dir) {
                result.push(p.clone());
            }
        }
    }

    // Step 4: force-excludes.
    if !force_excludes.is_empty() {
        result.retain(|p| !matches_any_exact_pattern(p, &force_excludes, base_dir));
    }

    result.into_iter().collect()
}

/// Enabled-state overrides for autoload=false delta filters (upstream
/// `applyAutoloadDisabledPatterns`).
pub fn apply_autoload_disabled_patterns(
    all_paths: &[PathBuf],
    patterns: &[String],
    base_dir: &Path,
) -> HashMap<PathBuf, bool> {
    let mut result = HashMap::new();
    for pattern in patterns {
        let target = pattern
            .strip_prefix('+')
            .or_else(|| pattern.strip_prefix('-'))
            .or_else(|| pattern.strip_prefix('!'))
            .unwrap_or(pattern);
        let enabled = !pattern.starts_with('-') && !pattern.starts_with('!');
        let exact = pattern.starts_with('+') || pattern.starts_with('-');
        for file_path in all_paths {
            let matched = if exact {
                matches_any_exact_pattern(file_path, &[target.to_string()], base_dir)
            } else {
                matches_any_pattern(file_path, &[target.to_string()], base_dir)
            };
            if matched {
                result.insert(file_path.clone(), enabled);
            }
        }
    }
    result
}

fn get_override_patterns(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter(|p| p.starts_with('!') || p.starts_with('+') || p.starts_with('-'))
        .cloned()
        .collect()
}

/// Enabled state for auto-discovered resources under override patterns
/// (upstream `isEnabledByOverrides`).
pub fn is_enabled_by_overrides(file_path: &Path, patterns: &[String], base_dir: &Path) -> bool {
    let overrides = get_override_patterns(patterns);
    let excludes: Vec<String> = overrides
        .iter()
        .filter(|p| p.starts_with('!'))
        .map(|p| p[1..].to_string())
        .collect();
    let force_includes: Vec<String> = overrides
        .iter()
        .filter(|p| p.starts_with('+'))
        .map(|p| p[1..].to_string())
        .collect();
    let force_excludes: Vec<String> = overrides
        .iter()
        .filter(|p| p.starts_with('-'))
        .map(|p| p[1..].to_string())
        .collect();

    let mut enabled = true;
    if !excludes.is_empty() && matches_any_pattern(file_path, &excludes, base_dir) {
        enabled = false;
    }
    if !force_includes.is_empty() && matches_any_exact_pattern(file_path, &force_includes, base_dir)
    {
        enabled = true;
    }
    if !force_excludes.is_empty() && matches_any_exact_pattern(file_path, &force_excludes, base_dir)
    {
        enabled = false;
    }
    enabled
}

/// Numeric precedence rank (upstream `resourcePrecedenceRank`). Lower =
/// higher precedence: project-local < project-auto < user-local <
/// user-auto < package.
pub fn resource_precedence_rank(m: &PathMetadata) -> u8 {
    if m.origin == ResourceOrigin::Package {
        return 4;
    }
    let scope_base = if m.scope == SourceScope::Project {
        0
    } else {
        2
    };
    scope_base + if m.source == "local" { 0 } else { 1 }
}

// ============================================================================
// Resource collection
// ============================================================================

/// Resource type (upstream `ResourceType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

pub const RESOURCE_TYPES: [ResourceType; 4] = [
    ResourceType::Extensions,
    ResourceType::Skills,
    ResourceType::Prompts,
    ResourceType::Themes,
];

fn file_matches_resource_type(name: &str, resource_type: ResourceType) -> bool {
    match resource_type {
        ResourceType::Extensions => name.ends_with(".ts") || name.ends_with(".js"),
        ResourceType::Skills => name.ends_with(".md"),
        ResourceType::Prompts => name.ends_with(".md"),
        ResourceType::Themes => name.ends_with(".json"),
    }
}

const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }

    let mut pattern = line.to_string();
    let mut negated = false;

    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest.to_string();
    } else if let Some(rest) = pattern.strip_prefix("\\!") {
        pattern = rest.to_string();
    }

    if let Some(rest) = pattern.strip_prefix('/') {
        pattern = rest.to_string();
    }

    let prefixed = if prefix.is_empty() {
        pattern
    } else {
        format!("{prefix}{pattern}")
    };
    Some(if negated {
        format!("!{prefixed}")
    } else {
        prefixed
    })
}

/// A gitignore-style matcher over prefixed patterns from ignore files.
struct IgnoreMatcher {
    root: PathBuf,
    patterns: Vec<String>,
}

impl IgnoreMatcher {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            patterns: Vec::new(),
        }
    }

    fn add_ignore_rules(&mut self, dir: &Path) {
        let relative_dir = dir.strip_prefix(&self.root).unwrap_or(Path::new(""));
        let prefix = if relative_dir.as_os_str().is_empty() {
            String::new()
        } else {
            format!("{}/", to_posix_path(relative_dir))
        };

        for filename in IGNORE_FILE_NAMES {
            let ignore_path = dir.join(filename);
            let Ok(content) = std::fs::read_to_string(&ignore_path) else {
                continue;
            };
            let patterns: Vec<String> = content
                .lines()
                .filter_map(|line| prefix_ignore_pattern(line, &prefix))
                .collect();
            if !patterns.is_empty() {
                self.patterns.extend(patterns);
            }
        }
    }

    /// Match a POSIX-style relative path (directories end with `/`).
    fn ignores(&self, rel_path: &str) -> bool {
        use ignore::Match;
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&self.root);
        for pattern in &self.patterns {
            let _ = builder.add_line(None, pattern);
        }
        let Ok(matcher) = builder.build() else {
            return false;
        };
        matches!(
            matcher.matched(rel_path, rel_path.ends_with('/')),
            Match::Ignore(_)
        )
    }
}

fn collect_files(
    dir: &Path,
    resource_type: ResourceType,
    skip_node_modules: bool,
    ignore_matcher: Option<&mut IgnoreMatcher>,
    root_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return files;
    }

    let root = root_dir.unwrap_or(dir).to_path_buf();
    let mut owned_matcher;
    let ig: &mut IgnoreMatcher = match ignore_matcher {
        Some(ig) => ig,
        None => {
            owned_matcher = IgnoreMatcher::new(&root);
            &mut owned_matcher
        }
    };
    ig.add_ignore_rules(dir);

    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    let mut dir_entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    dir_entries.sort_by_key(|e| e.file_name());
    for entry in dir_entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if skip_node_modules && name == "node_modules" {
            continue;
        }

        let full_path = dir.join(&name);
        let metadata = match std::fs::symlink_metadata(&full_path) {
            Ok(m) if m.file_type().is_symlink() => match std::fs::metadata(&full_path) {
                Ok(m) => m,
                Err(_) => continue,
            },
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = metadata.is_dir();
        let is_file = metadata.is_file();

        let rel_path = relative_path_string(&root, &full_path);
        let ignore_path = if is_dir {
            format!("{rel_path}/")
        } else {
            rel_path.clone()
        };
        if ig.ignores(&ignore_path) {
            continue;
        }

        if is_dir {
            files.extend(collect_files(
                &full_path,
                resource_type,
                skip_node_modules,
                Some(ig),
                Some(&root),
            ));
        } else if is_file && file_matches_resource_type(&name, resource_type) {
            files.push(full_path);
        }
    }

    files
}

/// Skill discovery mode (upstream `SkillDiscoveryMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDiscoveryMode {
    Pi,
    Agents,
}

fn collect_skill_entries(
    dir: &Path,
    mode: SkillDiscoveryMode,
    ignore_matcher: Option<&mut IgnoreMatcher>,
    root_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    if !dir.is_dir() {
        return entries;
    }

    let root = root_dir.unwrap_or(dir).to_path_buf();
    let mut owned_matcher;
    let ig: &mut IgnoreMatcher = match ignore_matcher {
        Some(ig) => ig,
        None => {
            owned_matcher = IgnoreMatcher::new(&root);
            &mut owned_matcher
        }
    };
    ig.add_ignore_rules(dir);

    let Ok(read) = std::fs::read_dir(dir) else {
        return entries;
    };
    let mut dir_entries: Vec<_> = read.filter_map(|e| e.ok()).collect();
    dir_entries.sort_by_key(|e| e.file_name());

    // First pass: SKILL.md in this directory wins immediately.
    for entry in &dir_entries {
        if entry.file_name() != "SKILL.md" {
            continue;
        }
        let full_path = dir.join("SKILL.md");
        let metadata = match std::fs::symlink_metadata(&full_path) {
            Ok(m) if m.file_type().is_symlink() => match std::fs::metadata(&full_path) {
                Ok(m) => m,
                Err(_) => continue,
            },
            Ok(m) => m,
            Err(_) => continue,
        };
        let rel_path = relative_path_string(&root, &full_path);
        if metadata.is_file() && !ig.ignores(&rel_path) {
            entries.push(full_path);
            return entries;
        }
    }

    for entry in &dir_entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }

        let full_path = dir.join(&name);
        let metadata = match std::fs::symlink_metadata(&full_path) {
            Ok(m) if m.file_type().is_symlink() => match std::fs::metadata(&full_path) {
                Ok(m) => m,
                Err(_) => continue,
            },
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = metadata.is_dir();
        let is_file = metadata.is_file();

        let rel_path = relative_path_string(&root, &full_path);
        let is_root = root == dir;
        let should_include_markdown_file = is_file
            && name.ends_with(".md")
            && !ig.ignores(&rel_path)
            && ((mode == SkillDiscoveryMode::Pi && is_root)
                || (mode == SkillDiscoveryMode::Agents && !is_root));
        if should_include_markdown_file {
            entries.push(full_path);
            continue;
        }

        if !is_dir {
            continue;
        }
        if ig.ignores(&format!("{rel_path}/")) {
            continue;
        }

        entries.extend(collect_skill_entries(
            &full_path,
            mode,
            Some(ig),
            Some(&root),
        ));
    }

    entries
}

pub fn collect_auto_skill_entries(dir: &Path, mode: SkillDiscoveryMode) -> Vec<PathBuf> {
    collect_skill_entries(dir, mode, None, None)
}

fn find_git_repo_root(start_dir: &Path) -> Option<PathBuf> {
    let mut dir = start_dir.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        let parent = dir.parent()?;
        if parent == dir {
            return None;
        }
        dir = parent.to_path_buf();
    }
}

/// `.agents/skills` directories from startDir up to the git repo root
/// (upstream `collectAncestorAgentsSkillDirs`).
pub fn collect_ancestor_agents_skill_dirs(start_dir: &Path) -> Vec<PathBuf> {
    let mut skill_dirs = Vec::new();
    let git_repo_root = find_git_repo_root(start_dir);

    let mut dir = start_dir.to_path_buf();
    loop {
        skill_dirs.push(dir.join(".agents").join("skills"));
        if let Some(root) = &git_repo_root {
            if &dir == root {
                break;
            }
        }
        let Some(parent) = dir.parent() else {
            break;
        };
        if parent == dir {
            break;
        }
        dir = parent.to_path_buf();
    }

    skill_dirs
}

fn collect_auto_entries_with_suffix(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    if !dir.is_dir() {
        return entries;
    }

    let mut ig = IgnoreMatcher::new(dir);
    ig.add_ignore_rules(dir);

    let Ok(read) = std::fs::read_dir(dir) else {
        return entries;
    };
    let mut dir_entries: Vec<_> = read.filter_map(|e| e.ok()).collect();
    dir_entries.sort_by_key(|e| e.file_name());
    for entry in dir_entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let full_path = dir.join(&name);
        let metadata = match std::fs::symlink_metadata(&full_path) {
            Ok(m) if m.file_type().is_symlink() => match std::fs::metadata(&full_path) {
                Ok(m) => m,
                Err(_) => continue,
            },
            Ok(m) => m,
            Err(_) => continue,
        };
        let rel_path = relative_path_string(dir, &full_path);
        if ig.ignores(&rel_path) {
            continue;
        }
        if metadata.is_file() && name.ends_with(suffix) {
            entries.push(full_path);
        }
    }

    entries
}

pub fn collect_auto_prompt_entries(dir: &Path) -> Vec<PathBuf> {
    collect_auto_entries_with_suffix(dir, ".md")
}

pub fn collect_auto_theme_entries(dir: &Path) -> Vec<PathBuf> {
    collect_auto_entries_with_suffix(dir, ".json")
}

/// Explicit extension entries for a directory: `pi` manifest in
/// package.json, else index.ts / index.js (upstream
/// `resolveExtensionEntries`).
pub fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let package_json_path = dir.join("package.json");
    if package_json_path.is_file() {
        if let Some(manifest) = read_pi_manifest(&package_json_path) {
            if let Some(extensions) = &manifest.extensions {
                if !extensions.is_empty() {
                    let entries: Vec<PathBuf> = extensions
                        .iter()
                        .map(|ext_path| dir.join(ext_path))
                        .filter(|resolved| resolved.exists())
                        .collect();
                    if !entries.is_empty() {
                        return Some(entries);
                    }
                }
            }
        }
    }

    let index_ts = dir.join("index.ts");
    if index_ts.exists() {
        return Some(vec![index_ts]);
    }
    let index_js = dir.join("index.js");
    if index_js.exists() {
        return Some(vec![index_js]);
    }

    None
}

pub fn collect_auto_extension_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    if !dir.is_dir() {
        return entries;
    }

    // First check if this directory itself has explicit extension entries.
    if let Some(root_entries) = resolve_extension_entries(dir) {
        return root_entries;
    }

    let mut ig = IgnoreMatcher::new(dir);
    ig.add_ignore_rules(dir);

    let Ok(read) = std::fs::read_dir(dir) else {
        return entries;
    };
    let mut dir_entries: Vec<_> = read.filter_map(|e| e.ok()).collect();
    dir_entries.sort_by_key(|e| e.file_name());
    for entry in dir_entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }

        let full_path = dir.join(&name);
        let metadata = match std::fs::symlink_metadata(&full_path) {
            Ok(m) if m.file_type().is_symlink() => match std::fs::metadata(&full_path) {
                Ok(m) => m,
                Err(_) => continue,
            },
            Ok(m) => m,
            Err(_) => continue,
        };
        let rel_path = relative_path_string(dir, &full_path);
        if ig.ignores(&rel_path) {
            continue;
        }

        if metadata.is_file() && (name.ends_with(".ts") || name.ends_with(".js")) {
            entries.push(full_path);
        } else if metadata.is_dir() {
            if let Some(resolved_entries) = resolve_extension_entries(&full_path) {
                entries.extend(resolved_entries);
            }
        }
    }

    entries
}

/// Collect resource files from a directory (upstream
/// `collectResourceFiles`): skills use pi-mode skill discovery, extensions
/// use smart discovery, prompts/themes use recursive file collection.
pub fn collect_resource_files(dir: &Path, resource_type: ResourceType) -> Vec<PathBuf> {
    match resource_type {
        ResourceType::Skills => collect_auto_skill_entries(dir, SkillDiscoveryMode::Pi),
        ResourceType::Extensions => collect_auto_extension_entries(dir),
        ResourceType::Prompts => collect_files(dir, resource_type, true, None, None),
        ResourceType::Themes => collect_files(dir, resource_type, true, None, None),
    }
}

fn split_patterns(entries: &[String]) -> (Vec<String>, Vec<String>) {
    let mut plain = Vec::new();
    let mut patterns = Vec::new();
    for entry in entries {
        if is_pattern(entry) {
            patterns.push(entry.clone());
        } else {
            plain.push(entry.clone());
        }
    }
    (plain, patterns)
}

// ============================================================================
// Temporary extension folder
// ============================================================================

/// Ensure and return the extension temp folder (upstream
/// `getExtensionTempFolder`).
pub fn get_extension_temp_folder(agent_dir: &Path) -> PathBuf {
    let temp_folder = agent_dir.join("tmp").join("extensions");
    let _ = std::fs::create_dir_all(&temp_folder);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temp_folder, std::fs::Permissions::from_mode(0o700));
    }
    temp_folder
}

// ============================================================================
// Package manager
// ============================================================================

type ResourceMap = BTreeMap<PathBuf, (PathMetadata, bool)>;
type ProgressCallback = Arc<Mutex<dyn FnMut(ProgressEvent) + Send>>;

#[derive(Default)]
struct ResourceAccumulator {
    extensions: ResourceMap,
    skills: ResourceMap,
    prompts: ResourceMap,
    themes: ResourceMap,
}

impl ResourceAccumulator {
    fn target(&mut self, resource_type: ResourceType) -> &mut ResourceMap {
        match resource_type {
            ResourceType::Extensions => &mut self.extensions,
            ResourceType::Skills => &mut self.skills,
            ResourceType::Prompts => &mut self.prompts,
            ResourceType::Themes => &mut self.themes,
        }
    }
}

/// Extract the filter half of an object-form package source (upstream the
/// `PackageSource` object branch).
pub fn package_filter_of(pkg: &PackageSource) -> Option<PackageFilter> {
    match pkg {
        PackageSource::Source(_) => None,
        PackageSource::Filtered {
            autoload,
            extensions,
            skills,
            prompts,
            themes,
            ..
        } => Some(PackageFilter {
            autoload: *autoload,
            extensions: extensions.clone(),
            skills: skills.clone(),
            prompts: prompts.clone(),
            themes: themes.clone(),
        }),
    }
}

pub fn package_source_string(pkg: &PackageSource) -> String {
    match pkg {
        PackageSource::Source(source) => source.clone(),
        PackageSource::Filtered { source, .. } => source.clone(),
    }
}

fn source_scope_of(scope: SourceScope) -> SourceScope {
    scope
}

struct ConfiguredUpdateSource {
    source: String,
    scope: SourceScope,
}

/// The default package manager (upstream `DefaultPackageManager`).
pub struct DefaultPackageManager {
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings: Arc<Mutex<SettingsManager>>,
    progress_callback: Option<ProgressCallback>,
    global_npm_root_cache: Mutex<Option<(String, String)>>,
}

impl DefaultPackageManager {
    pub fn new(cwd: &str, agent_dir: &Path, settings: Arc<Mutex<SettingsManager>>) -> Self {
        Self {
            cwd: resolve_to_cwd(cwd, "/"),
            agent_dir: resolve_to_cwd(&agent_dir.to_string_lossy(), "/"),
            settings,
            progress_callback: None,
            global_npm_root_cache: Mutex::new(None),
        }
    }

    pub fn set_progress_callback(&mut self, callback: Arc<Mutex<dyn FnMut(ProgressEvent) + Send>>) {
        self.progress_callback = Some(callback);
    }

    fn emit_progress(&self, event: ProgressEvent) {
        if let Some(callback) = &self.progress_callback {
            (callback.lock().unwrap())(event);
        }
    }

    fn with_progress(
        &self,
        action: ProgressAction,
        source: &str,
        message: &str,
        operation: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.emit_progress(ProgressEvent {
            kind: ProgressKind::Start,
            action,
            source: source.to_string(),
            message: Some(message.to_string()),
        });
        match operation() {
            Ok(()) => {
                self.emit_progress(ProgressEvent {
                    kind: ProgressKind::Complete,
                    action,
                    source: source.to_string(),
                    message: None,
                });
                Ok(())
            }
            Err(error) => {
                self.emit_progress(ProgressEvent {
                    kind: ProgressKind::Error,
                    action,
                    source: source.to_string(),
                    message: Some(error.clone()),
                });
                Err(error)
            }
        }
    }

    // --- settings interaction ------------------------------------------------

    fn settings_snapshot(&self) -> (serde_json::Value, serde_json::Value) {
        let settings = self.settings.lock().unwrap();
        (
            settings.global_settings().clone(),
            settings.project_settings().clone(),
        )
    }

    fn packages_of(settings: &serde_json::Value) -> Vec<PackageSource> {
        settings
            .get("packages")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(package_source_from_json).collect())
            .unwrap_or_default()
    }

    fn string_list_of(settings: &serde_json::Value, key: &str) -> Vec<String> {
        settings
            .get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn is_offline_mode() -> bool {
        match std::env::var("PILLAR_OFFLINE") {
            Ok(value) => {
                let v = value.to_lowercase();
                v == "1" || v == "true" || v == "yes"
            }
            Err(_) => false,
        }
    }

    fn assert_project_trusted_for_scope(&self, scope: SourceScope) -> Result<(), String> {
        if scope == SourceScope::Project && !self.settings.lock().unwrap().is_project_trusted() {
            return Err(
                "Project is not trusted; refusing to access project package storage".to_string(),
            );
        }
        Ok(())
    }

    /// Add a source to settings (upstream `addSourceToSettings`).
    pub fn add_source_to_settings(&self, source: &str, local: bool) -> bool {
        let scope = if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        let (global, project) = self.settings_snapshot();
        let current = if scope == SourceScope::Project {
            Self::packages_of(&project)
        } else {
            Self::packages_of(&global)
        };
        let normalized = self.normalize_package_source_for_settings(source, scope);
        if let Some(match_index) = current
            .iter()
            .position(|existing| self.package_sources_match(existing, source, scope))
        {
            let existing = &current[match_index];
            if package_source_string(existing) == normalized {
                return false;
            }
            let mut next = current.clone();
            next[match_index] = PackageSource::Source(normalized);
            self.persist_packages(&next, scope);
            return true;
        }
        let mut next = current;
        next.push(PackageSource::Source(normalized));
        self.persist_packages(&next, scope);
        true
    }

    /// Remove a source from settings (upstream `removeSourceFromSettings`).
    pub fn remove_source_from_settings(&self, source: &str, local: bool) -> bool {
        let scope = if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        let (global, project) = self.settings_snapshot();
        let current = if scope == SourceScope::Project {
            Self::packages_of(&project)
        } else {
            Self::packages_of(&global)
        };
        let next: Vec<PackageSource> = current
            .iter()
            .filter(|existing| !self.package_sources_match(existing, source, scope))
            .cloned()
            .collect();
        if next.len() == current.len() {
            return false;
        }
        self.persist_packages(&next, scope);
        true
    }

    fn persist_packages(&self, packages: &[PackageSource], scope: SourceScope) {
        let json = serde_json::Value::Array(
            packages
                .iter()
                .map(|pkg| match pkg {
                    PackageSource::Source(source) => serde_json::Value::String(source.clone()),
                    PackageSource::Filtered {
                        source,
                        autoload,
                        extensions,
                        skills,
                        prompts,
                        themes,
                    } => {
                        let mut obj = serde_json::Map::new();
                        obj.insert(
                            "source".to_string(),
                            serde_json::Value::String(source.clone()),
                        );
                        if let Some(autoload) = autoload {
                            obj.insert("autoload".to_string(), serde_json::Value::Bool(*autoload));
                        }
                        for (key, list) in [
                            ("extensions", extensions),
                            ("skills", skills),
                            ("prompts", prompts),
                            ("themes", themes),
                        ] {
                            if let Some(list) = list {
                                obj.insert(
                                    key.to_string(),
                                    serde_json::Value::Array(
                                        list.iter()
                                            .map(|s| serde_json::Value::String(s.clone()))
                                            .collect(),
                                    ),
                                );
                            }
                        }
                        serde_json::Value::Object(obj)
                    }
                })
                .collect(),
        );
        let mut settings = self.settings.lock().unwrap();
        if scope == SourceScope::Project {
            let _ = settings.set_project_packages(json);
        } else {
            settings.set_global_setting("packages", json);
        }
    }

    fn package_sources_match(
        &self,
        existing: &PackageSource,
        input_source: &str,
        scope: SourceScope,
    ) -> bool {
        let left = self.get_source_match_key_for_settings(&package_source_string(existing), scope);
        let right = self.get_source_match_key_for_input(input_source);
        left == right
    }

    fn get_source_match_key_for_input(&self, source: &str) -> String {
        match parse_source(source) {
            ParsedSource::Npm(npm) => format!("npm:{}", npm.name),
            ParsedSource::Git(git) => format!("git:{}/{}", git.host, git.path),
            ParsedSource::Local(path) => format!("local:{}", self.resolve_path(&path).display()),
        }
    }

    fn get_source_match_key_for_settings(&self, source: &str, scope: SourceScope) -> String {
        match parse_source(source) {
            ParsedSource::Npm(npm) => format!("npm:{}", npm.name),
            ParsedSource::Git(git) => format!("git:{}/{}", git.host, git.path),
            ParsedSource::Local(path) => {
                let base_dir = self.get_base_dir_for_scope(scope);
                format!(
                    "local:{}",
                    self.resolve_path_from_base(&path, &base_dir).display()
                )
            }
        }
    }

    fn normalize_package_source_for_settings(&self, source: &str, scope: SourceScope) -> String {
        let ParsedSource::Local(path) = parse_source(source) else {
            return source.to_string();
        };
        let base_dir = self.get_base_dir_for_scope(scope);
        let resolved = self.resolve_path(&path);
        let rel = relative_path_string(&base_dir, &resolved);
        if rel.is_empty() { ".".to_string() } else { rel }
    }

    /// Unique identity ignoring version/ref (upstream
    /// `getPackageIdentity`).
    pub fn get_package_identity(&self, source: &str, scope: Option<SourceScope>) -> String {
        match parse_source(source) {
            ParsedSource::Npm(npm) => format!("npm:{}", npm.name),
            ParsedSource::Git(git) => format!("git:{}/{}", git.host, git.path),
            ParsedSource::Local(path) => match scope {
                Some(scope) => {
                    let base_dir = self.get_base_dir_for_scope(scope);
                    format!(
                        "local:{}",
                        self.resolve_path_from_base(&path, &base_dir).display()
                    )
                }
                None => format!("local:{}", self.resolve_path(&path).display()),
            },
        }
    }

    /// Dedupe: project wins over user for the same identity; a project
    /// entry with autoload=false is a delta over the user entry, so both
    /// are kept (upstream `dedupePackages`).
    pub fn dedupe_packages(
        &self,
        packages: Vec<(PackageSource, SourceScope)>,
    ) -> Vec<(PackageSource, SourceScope)> {
        let mut result: Vec<(PackageSource, SourceScope)> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (pkg, scope) in packages {
            let identity = self.get_package_identity(&package_source_string(&pkg), Some(scope));
            let Some(&index) = seen.get(&identity) else {
                seen.insert(identity, result.len());
                result.push((pkg, scope));
                continue;
            };
            let (existing_pkg, existing_scope) = &result[index];
            if *existing_scope == SourceScope::Project && scope == SourceScope::User {
                if let PackageSource::Filtered {
                    autoload: Some(false),
                    ..
                } = existing_pkg
                {
                    result.push((pkg, scope));
                }
            } else if scope == SourceScope::Project {
                result[index] = (pkg, scope);
            }
        }
        result
    }

    // --- path computation ------------------------------------------------------

    fn resolve_path(&self, input: &str) -> PathBuf {
        resolve_to_cwd(input.trim(), &self.cwd.to_string_lossy())
    }

    fn resolve_path_from_base(&self, input: &str, base_dir: &Path) -> PathBuf {
        resolve_to_cwd(input.trim(), &base_dir.to_string_lossy())
    }

    fn get_base_dir_for_scope(&self, scope: SourceScope) -> PathBuf {
        match scope {
            SourceScope::Project => {
                debug_assert!(self.settings.lock().unwrap().is_project_trusted());
                self.cwd.join(CONFIG_DIR_NAME)
            }
            SourceScope::User => self.agent_dir.clone(),
            SourceScope::Temporary => self.cwd.clone(),
        }
    }

    pub fn resolve_managed_path(&self, root: &Path, parts: &[&str]) -> Result<PathBuf, String> {
        let resolved_root = root.to_path_buf();
        let mut resolved_path = resolved_root.clone();
        for part in parts {
            resolved_path = resolved_path.join(part);
        }
        // Lexically normalize `..` segments before the containment check
        // (upstream path.resolve does this).
        let mut normalized: Vec<std::ffi::OsString> = Vec::new();
        for component in resolved_path.components() {
            use std::path::Component;
            match component {
                Component::ParentDir => {
                    normalized.pop();
                }
                Component::CurDir => {}
                component => normalized.push(component.as_os_str().to_os_string()),
            }
        }
        let resolved_path: PathBuf = normalized.into_iter().collect();
        if resolved_path != resolved_root && !resolved_path.starts_with(&resolved_root) {
            return Err(format!(
                "Refusing to use path outside package install root: {}",
                resolved_path.display()
            ));
        }
        Ok(resolved_path)
    }

    /// Npm install root for a scope (upstream `getNpmInstallRoot`).
    pub fn get_npm_install_root(
        &self,
        scope: SourceScope,
        temporary: bool,
    ) -> Result<PathBuf, String> {
        if temporary {
            return Ok(self.get_temporary_dir("npm", None));
        }
        match scope {
            SourceScope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(self.cwd.join(CONFIG_DIR_NAME).join("npm"))
            }
            SourceScope::User => Ok(self.agent_dir.join("npm")),
            SourceScope::Temporary => Ok(self.get_temporary_dir("npm", None)),
        }
    }

    /// Git install root for a scope (upstream `getGitInstallRoot`).
    pub fn get_git_install_root(&self, scope: SourceScope) -> Result<PathBuf, String> {
        match scope {
            SourceScope::Temporary => Err("Missing git install root".to_string()),
            SourceScope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(self.cwd.join(CONFIG_DIR_NAME).join("git"))
            }
            SourceScope::User => Ok(self.agent_dir.join("git")),
        }
    }

    /// Hashed temporary dir (upstream `getTemporaryDir`).
    pub fn get_temporary_dir(&self, prefix: &str, suffix: Option<&str>) -> PathBuf {
        let root = self
            .resolve_managed_path(&get_extension_temp_folder(&self.agent_dir), &[prefix])
            .unwrap_or_else(|_| get_extension_temp_folder(&self.agent_dir).join(prefix));
        let hash = sha256_hex(&format!("{}-{}", prefix, suffix.unwrap_or("")));
        let short = hash.chars().take(8).collect::<String>();
        self.resolve_managed_path(&root, &[&short, suffix.unwrap_or("")])
            .unwrap_or_else(|_| root.join(&short).join(suffix.unwrap_or("")))
    }

    /// Managed npm install path (upstream `getManagedNpmInstallPath`).
    pub fn get_managed_npm_install_path(
        &self,
        source: &NpmSource,
        scope: SourceScope,
    ) -> Result<PathBuf, String> {
        match scope {
            SourceScope::Temporary => Ok(self.resolve_managed_path(
                &self.get_temporary_dir("npm", None),
                &["node_modules", &source.name],
            )?),
            SourceScope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(self
                    .cwd
                    .join(CONFIG_DIR_NAME)
                    .join("npm")
                    .join("node_modules")
                    .join(&source.name))
            }
            SourceScope::User => Ok(self
                .agent_dir
                .join("npm")
                .join("node_modules")
                .join(&source.name)),
        }
    }

    /// Install path for an npm source with legacy global fallback (upstream
    /// `getNpmInstallPath`).
    pub fn get_npm_install_path(&self, source: &NpmSource, scope: SourceScope) -> PathBuf {
        let Ok(managed_path) = self.get_managed_npm_install_path(source, scope) else {
            return PathBuf::from(&source.name);
        };
        if scope != SourceScope::User || managed_path.exists() {
            return managed_path;
        }
        if let Ok(global_root) = self.get_global_npm_root() {
            let legacy_path = global_root.join(&source.name);
            if legacy_path.exists() {
                return legacy_path;
            }
        }
        managed_path
    }

    /// Install path for a git source (upstream `getGitInstallPath`).
    pub fn get_git_install_path(
        &self,
        source: &GitSource,
        scope: SourceScope,
    ) -> Result<PathBuf, String> {
        if scope == SourceScope::Temporary {
            return Ok(self.get_temporary_dir(&format!("git-{}", source.host), Some(&source.path)));
        }
        let install_root = self.get_git_install_root(scope)?;
        self.resolve_managed_path(&install_root, &[&source.host, &source.path])
    }

    fn get_global_npm_root(&self) -> Result<PathBuf, String> {
        let npm_command = self.get_npm_command()?;
        let command_key = npm_command.join("\0");
        if let Some((cached_key, cached_root)) = self.global_npm_root_cache.lock().unwrap().clone()
        {
            if cached_key == command_key {
                return Ok(PathBuf::from(cached_root));
            }
        }
        let root = if self.get_package_manager_name()? == "bun" {
            let bin_dir = self.run_npm_command_sync(&["pm", "bin", "-g"])?.to_string();
            PathBuf::from(bin_dir)
                .parent()
                .map(|p| p.join("install").join("global").join("node_modules"))
                .unwrap_or_default()
        } else {
            PathBuf::from(self.run_npm_command_sync(&["root", "-g"])?.to_string())
        };
        *self.global_npm_root_cache.lock().unwrap() =
            Some((command_key, root.to_string_lossy().to_string()));
        Ok(root)
    }

    // --- npm/git command construction ------------------------------------------

    fn get_npm_command(&self) -> Result<Vec<String>, String> {
        let configured = {
            let settings = self.settings.lock().unwrap();
            settings
                .get_global_setting("npmCommand")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<String>>()
                })
        };
        match configured {
            Some(command) if !command.is_empty() => {
                if command[0].is_empty() {
                    return Err(
                        "Invalid npmCommand: first array entry must be a non-empty command"
                            .to_string(),
                    );
                }
                Ok(command)
            }
            _ => Ok(vec!["npm".to_string()]),
        }
    }

    fn get_package_manager_name(&self) -> Result<String, String> {
        let command_parts = self.get_npm_command()?;
        let separator_index = command_parts.iter().rposition(|p| p == "--");
        let package_manager_command = match separator_index {
            Some(index) => command_parts.get(index + 1).cloned().unwrap_or_default(),
            None => command_parts.first().cloned().unwrap_or_default(),
        };
        let name = Path::new(&package_manager_command)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        Ok(name
            .trim_end_matches(".cmd")
            .trim_end_matches(".exe")
            .trim_end_matches(".CMD")
            .trim_end_matches(".EXE")
            .to_string())
    }

    /// Install args for a package manager (upstream
    /// `getNpmInstallArgs`): peer resolution disabled for managed installs.
    pub fn get_npm_install_args(
        &self,
        specs: &[String],
        install_root: &Path,
    ) -> Result<Vec<String>, String> {
        let package_manager_name = self.get_package_manager_name()?;
        if package_manager_name == "bun" {
            let mut args = vec!["install".to_string()];
            args.extend(specs.iter().cloned());
            args.push("--cwd".to_string());
            args.push(install_root.to_string_lossy().to_string());
            args.push("--omit=peer".to_string());
            return Ok(args);
        }
        if package_manager_name == "pnpm" {
            let mut args = vec![
                "install".to_string(),
                "--prefix".to_string(),
                install_root.to_string_lossy().to_string(),
                "--config.auto-install-peers=false".to_string(),
                "--config.strict-peer-dependencies=false".to_string(),
                "--config.strict-dep-builds=false".to_string(),
            ];
            args.extend(specs.iter().cloned());
            return Ok(args);
        }
        let mut args = vec![
            "install".to_string(),
            "--prefix".to_string(),
            install_root.to_string_lossy().to_string(),
            "--legacy-peer-deps".to_string(),
        ];
        args.extend(specs.iter().cloned());
        Ok(args)
    }

    fn get_git_dependency_install_args(&self) -> Vec<String> {
        let configured = self
            .get_npm_command()
            .map(|c| !c.is_empty() && c != ["npm".to_string()])
            .unwrap_or(false);
        if configured {
            return ["install".to_string()].to_vec();
        }
        ["install".to_string(), "--omit=dev".to_string()].to_vec()
    }

    // --- command runners ---------------------------------------------------------

    fn run_command(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
    ) -> Result<(), String> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let output = cmd
            .output()
            .map_err(|e| format!("Failed to run {command}: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(format!(
            "{command} {} failed with code {:?}: {}",
            args.join(" "),
            output.status.code(),
            if stderr.is_empty() { stdout } else { stderr }
        ))
    }

    fn run_command_capture(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        env: Option<&[(&str, &str)]>,
    ) -> Result<String, String> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        cmd.stdin(Stdio::null());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        if let Some(env) = env {
            for (key, value) in env {
                cmd.env(key, value);
            }
        }
        let started = Instant::now();
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to run {command}: {e}"))?;
        loop {
            match child.try_wait().map_err(|e| format!("{command}: {e}"))? {
                Some(status) => {
                    let mut stdout = String::new();
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.stdout.take() {
                        let _ = pipe.read_to_string(&mut stdout);
                    }
                    if let Some(mut pipe) = child.stderr.take() {
                        let _ = pipe.read_to_string(&mut stderr);
                    }
                    if !status.success() {
                        let exit_status = status
                            .code()
                            .map(|code| format!("code {code}"))
                            .unwrap_or_else(|| "signal unknown".to_string());
                        return Err(format!(
                            "{command} {} failed with {exit_status}: {}",
                            args.join(" "),
                            if stderr.is_empty() { stdout } else { stderr }
                        ));
                    }
                    return Ok(stdout.trim().to_string());
                }
                None => {
                    if started.elapsed() > Duration::from_millis(NETWORK_TIMEOUT_MS) {
                        let _ = child.kill();
                        return Err(format!(
                            "{command} {} timed out after {NETWORK_TIMEOUT_MS}ms",
                            args.join(" ")
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    fn run_command_sync(&self, command: &str, args: &[String]) -> Result<String, String> {
        let output = Command::new(command)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("Failed to run {command}: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "Failed to run {command} {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(if stdout.is_empty() {
            String::from_utf8_lossy(&output.stderr).trim().to_string()
        } else {
            stdout
        })
    }

    fn run_npm_command(&self, args: &[String], cwd: Option<&Path>) -> Result<(), String> {
        let npm_command = self.get_npm_command()?;
        let mut full = npm_command;
        full.extend(args.iter().cloned());
        let (command, rest) = full.split_first().expect("npm command");
        self.run_command(command, rest, cwd)
    }

    fn run_npm_command_sync(&self, args: &[&str]) -> Result<String, String> {
        let npm_command = self.get_npm_command()?;
        let mut full: Vec<String> = npm_command;
        full.extend(args.iter().map(|s| s.to_string()));
        let (command, rest) = full.split_first().expect("npm command");
        self.run_command_sync(command, rest)
    }

    fn run_git_remote_command(
        &self,
        installed_path: &Path,
        args: &[String],
    ) -> Result<String, String> {
        self.run_command_capture(
            "git",
            args,
            Some(installed_path),
            Some(&[("GIT_TERMINAL_PROMPT", "0")]),
        )
    }

    // --- npm versioning -----------------------------------------------------------

    fn get_installed_npm_version(&self, installed_path: &Path) -> Option<String> {
        let package_json_path = installed_path.join("package.json");
        let content = std::fs::read_to_string(package_json_path).ok()?;
        let parsed: serde_json::Value =
            serde_json::from_str(crate::core::auth_storage::strip_bom(&content)).ok()?;
        parsed.get("version")?.as_str().map(str::to_string)
    }

    fn installed_npm_matches_configured_version(
        &self,
        source: &NpmSource,
        installed_path: &Path,
    ) -> bool {
        let Some(installed_version) = self.get_installed_npm_version(installed_path) else {
            return false;
        };
        match &source.range {
            Some(range) => satisfies_range(&installed_version, range),
            None => true,
        }
    }

    fn get_latest_npm_version(
        &self,
        package_spec: &str,
        range: Option<&str>,
    ) -> Result<String, String> {
        let args = vec![
            "view".to_string(),
            package_spec.to_string(),
            "version".to_string(),
            "--json".to_string(),
        ];
        let stdout = self.run_command_capture("npm", &args, Some(&self.cwd), None)?;
        let raw = stdout.trim();
        if raw.is_empty() {
            return Err("Empty response from npm view".to_string());
        }
        let parsed: serde_json::Value = serde_json::from_str(raw).map_err(|e| format!("{e}"))?;
        if let Some(version) = parsed.as_str() {
            return Ok(version.to_string());
        }
        if let Some(versions) = parsed.as_array() {
            let versions: Vec<String> = versions
                .iter()
                .filter_map(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .collect();
            if let Some(latest) = max_satisfying_version(&versions, range) {
                return Ok(latest);
            }
        }
        Err("Unexpected response from npm view".to_string())
    }

    fn should_update_npm_source(&self, source: &NpmSource, scope: SourceScope) -> bool {
        let Ok(installed_path) = self.get_managed_npm_install_path(source, scope) else {
            return true;
        };
        let installed_version = if installed_path.exists() {
            self.get_installed_npm_version(&installed_path)
        } else {
            None
        };
        let Some(installed_version) = installed_version else {
            return true;
        };
        let spec = source
            .version
            .clone()
            .unwrap_or_else(|| source.name.clone());
        match self.get_latest_npm_version(&spec, source.range.as_deref()) {
            Ok(target_version) => version_gt(&target_version, &installed_version),
            // Preserve existing update behavior when version lookup fails.
            Err(_) => true,
        }
    }

    fn npm_has_available_update(&self, source: &NpmSource, installed_path: &Path) -> bool {
        if Self::is_offline_mode() {
            return false;
        }
        let Some(installed_version) = self.get_installed_npm_version(installed_path) else {
            return false;
        };
        let spec = source
            .version
            .clone()
            .unwrap_or_else(|| source.name.clone());
        match self.get_latest_npm_version(&spec, source.range.as_deref()) {
            Ok(target_version) => version_gt(&target_version, &installed_version),
            Err(_) => false,
        }
    }

    // --- git update plumbing ---------------------------------------------------------

    fn git_has_available_update(&self, installed_path: &Path) -> bool {
        if Self::is_offline_mode() {
            return false;
        }
        let Ok(local_head) = self.run_command_capture(
            "git",
            &["rev-parse".to_string(), "HEAD".to_string()],
            Some(installed_path),
            None,
        ) else {
            return false;
        };
        let Ok(remote_head) = self.get_remote_git_head(installed_path) else {
            return false;
        };
        local_head.trim() != remote_head.trim()
    }

    fn get_remote_git_head(&self, installed_path: &Path) -> Result<String, String> {
        if let Some(upstream_ref) = self.get_git_upstream_ref(installed_path) {
            let remote_head = self.run_git_remote_command(
                installed_path,
                &[
                    "ls-remote".to_string(),
                    "origin".to_string(),
                    upstream_ref.clone(),
                ],
            )?;
            if let Some(hash) = first_40_hex(&remote_head) {
                return Ok(hash);
            }
        }

        let remote_head = self.run_git_remote_command(
            installed_path,
            &[
                "ls-remote".to_string(),
                "origin".to_string(),
                "HEAD".to_string(),
            ],
        )?;
        first_40_hex_head(&remote_head).ok_or_else(|| "Failed to determine remote HEAD".to_string())
    }

    fn get_git_upstream_ref(&self, installed_path: &Path) -> Option<String> {
        let upstream = self
            .run_command_capture(
                "git",
                &[
                    "rev-parse".to_string(),
                    "--abbrev-ref".to_string(),
                    "@{upstream}".to_string(),
                ],
                Some(installed_path),
                None,
            )
            .ok()?;
        let upstream = upstream.trim();
        let branch = upstream.strip_prefix("origin/")?;
        if branch.is_empty() {
            return None;
        }
        Some(format!("refs/heads/{branch}"))
    }

    fn get_local_git_update_target(
        &self,
        installed_path: &Path,
    ) -> Result<(String, String, Vec<String>), String> {
        let upstream_result = self.run_command_capture(
            "git",
            &[
                "rev-parse".to_string(),
                "--abbrev-ref".to_string(),
                "@{upstream}".to_string(),
            ],
            Some(installed_path),
            None,
        );
        if let Ok(upstream) = upstream_result {
            let trimmed = upstream.trim().to_string();
            if let Some(branch) = trimmed.strip_prefix("origin/") {
                if !branch.is_empty() {
                    let head = self.run_command_capture(
                        "git",
                        &["rev-parse".to_string(), "@{upstream}".to_string()],
                        Some(installed_path),
                        None,
                    )?;
                    return Ok((
                        "@{upstream}".to_string(),
                        head,
                        [
                            "fetch".to_string(),
                            "--prune".to_string(),
                            "--no-tags".to_string(),
                            "origin".to_string(),
                            format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
                        ]
                        .to_vec(),
                    ));
                }
            }
            return Err(format!("Unsupported upstream remote: {trimmed}"));
        }

        let _ = self.run_command(
            "git",
            &[
                "remote".to_string(),
                "set-head".to_string(),
                "origin".to_string(),
                "-a".to_string(),
            ],
            Some(installed_path),
        );
        let head = self.run_command_capture(
            "git",
            &["rev-parse".to_string(), "origin/HEAD".to_string()],
            Some(installed_path),
            None,
        )?;
        let origin_head_ref = self
            .run_command_capture(
                "git",
                &[
                    "symbolic-ref".to_string(),
                    "refs/remotes/origin/HEAD".to_string(),
                ],
                Some(installed_path),
                None,
            )
            .unwrap_or_default();
        let branch = origin_head_ref
            .trim()
            .strip_prefix("refs/remotes/origin/")
            .unwrap_or("")
            .to_string();
        if !branch.is_empty() {
            return Ok((
                "origin/HEAD".to_string(),
                head,
                [
                    "fetch".to_string(),
                    "--prune".to_string(),
                    "--no-tags".to_string(),
                    "origin".to_string(),
                    format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
                ]
                .to_vec(),
            ));
        }
        Ok((
            "origin/HEAD".to_string(),
            head,
            [
                "fetch".to_string(),
                "--prune".to_string(),
                "--no-tags".to_string(),
                "origin".to_string(),
                "+HEAD:refs/remotes/origin/HEAD".to_string(),
            ]
            .to_vec(),
        ))
    }

    fn get_git_update_marker_path(&self, target_dir: &Path) -> PathBuf {
        let name = target_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        target_dir
            .parent()
            .unwrap_or(target_dir)
            .join(format!(".{name}.pi-update-incomplete"))
    }

    fn has_missing_git_dependencies(&self, target_dir: &Path) -> bool {
        let package_json_path = target_dir.join("package.json");
        let Ok(content) = std::fs::read_to_string(package_json_path) else {
            return false;
        };
        let Ok(manifest) = serde_json::from_str::<serde_json::Value>(
            crate::core::auth_storage::strip_bom(&content),
        ) else {
            return false;
        };
        let Some(deps) = manifest.get("dependencies").and_then(|d| d.as_object()) else {
            return false;
        };
        let node_modules_dir = target_dir.join("node_modules");
        deps.keys().any(|name| {
            let dependency_path = node_modules_dir.join(name);
            dependency_path.starts_with(&node_modules_dir) && !dependency_path.exists()
        })
    }

    fn repair_missing_git_dependencies(&self, target_dir: &Path) -> Result<(), String> {
        if !self.has_missing_git_dependencies(target_dir) {
            return Ok(());
        }
        let args = self.get_git_dependency_install_args();
        self.run_npm_command(&args, Some(target_dir))
    }

    fn clean_and_install_git_dependencies(
        &self,
        target_dir: &Path,
        marker_path: &Path,
    ) -> Result<(), String> {
        // Clean untracked files (extensions should be pristine). If this
        // fails after deleting dependencies, repair them so the existing
        // extension still loads.
        if let Err(error) = self.run_command(
            "git",
            &["clean".to_string(), "-fdx".to_string()],
            Some(target_dir),
        ) {
            let _ = self.repair_missing_git_dependencies(target_dir);
            return Err(error);
        }

        let package_json_path = target_dir.join("package.json");
        if package_json_path.exists() {
            let args = self.get_git_dependency_install_args();
            self.run_npm_command(&args, Some(target_dir))?;
        }
        let _ = std::fs::remove_file(marker_path);
        Ok(())
    }

    fn ensure_git_ref(
        &self,
        target_dir: &Path,
        fetch_args: &[String],
        ref_: &str,
    ) -> Result<(), String> {
        // Fetch only the ref we will reset to.
        self.run_command("git", fetch_args, Some(target_dir))?;

        let local_head = self.run_command_capture(
            "git",
            &["rev-parse".to_string(), "HEAD".to_string()],
            Some(target_dir),
            None,
        )?;
        let commit_ref = format!("{ref_}^{{commit}}");
        let target_head = self.run_command_capture(
            "git",
            &["rev-parse".to_string(), commit_ref.clone()],
            Some(target_dir),
            None,
        )?;
        let marker_path = self.get_git_update_marker_path(target_dir);
        if local_head.trim() == target_head.trim() {
            if marker_path.exists() {
                return self.clean_and_install_git_dependencies(target_dir, &marker_path);
            }
            return self.repair_missing_git_dependencies(target_dir);
        }

        std::fs::write(&marker_path, "").map_err(|e| e.to_string())?;
        self.run_command(
            "git",
            &["reset".to_string(), "--hard".to_string(), commit_ref],
            Some(target_dir),
        )?;
        self.clean_and_install_git_dependencies(target_dir, &marker_path)
    }

    fn prune_empty_git_parents(&self, target_dir: &Path, install_root: Option<&Path>) {
        let Some(install_root) = install_root else {
            return;
        };
        let resolved_root = install_root.to_path_buf();
        let mut current = match target_dir.parent() {
            Some(p) => p.to_path_buf(),
            None => return,
        };
        while current.starts_with(&resolved_root) && current != resolved_root {
            if !current.exists() {
                current = match current.parent() {
                    Some(p) => p.to_path_buf(),
                    None => return,
                };
                continue;
            }
            let Ok(mut entries) = std::fs::read_dir(&current) else {
                return;
            };
            if entries.next().is_some() {
                break;
            }
            if std::fs::remove_dir_all(&current).is_err() {
                break;
            }
            current = match current.parent() {
                Some(p) => p.to_path_buf(),
                None => return,
            };
        }
    }

    fn ensure_npm_project(&self, install_root: &Path) {
        if !install_root.exists() {
            let _ = std::fs::create_dir_all(install_root);
        }
        mark_path_ignored_by_cloud_sync(install_root);
        self.ensure_git_ignore(install_root);
        let package_json_path = install_root.join("package.json");
        if !package_json_path.exists() {
            let pkg_json = serde_json::json!({"name": "pi-extensions", "private": true});
            let _ = std::fs::write(
                &package_json_path,
                serde_json::to_string_pretty(&pkg_json).unwrap_or_default(),
            );
        }
    }

    fn ensure_git_ignore(&self, dir: &Path) {
        if !dir.exists() {
            let _ = std::fs::create_dir_all(dir);
        }
        let ignore_path = dir.join(".gitignore");
        if !ignore_path.exists() {
            let _ = std::fs::write(&ignore_path, "*\n!.gitignore\n");
        }
    }

    // --- install / remove / update -------------------------------------------------

    /// Install a package source (upstream `install`).
    pub fn install(&self, source: &str, local: bool) -> Result<(), String> {
        let parsed = parse_source(source);
        let scope = if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        self.assert_project_trusted_for_scope(scope)?;
        self.with_progress(
            ProgressAction::Install,
            source,
            &format!("Installing {source}..."),
            || match parsed {
                ParsedSource::Npm(npm) => self.install_npm(&npm, scope, false),
                ParsedSource::Git(git) => self.install_git(&git, scope),
                ParsedSource::Local(path) => {
                    let resolved = self.resolve_path(&path);
                    if !resolved.exists() {
                        return Err(format!("Path does not exist: {}", resolved.display()));
                    }
                    Ok(())
                }
            },
        )
    }

    /// Install and persist to settings (upstream `installAndPersist`).
    pub fn install_and_persist(&self, source: &str, local: bool) -> Result<(), String> {
        self.install(source, local)?;
        self.add_source_to_settings(source, local);
        Ok(())
    }

    /// Remove a package (upstream `remove`).
    pub fn remove(&self, source: &str, local: bool) -> Result<(), String> {
        let parsed = parse_source(source);
        let scope = if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        self.assert_project_trusted_for_scope(scope)?;
        self.with_progress(
            ProgressAction::Remove,
            source,
            &format!("Removing {source}..."),
            || match parsed {
                ParsedSource::Npm(npm) => self.uninstall_npm(&npm, scope),
                ParsedSource::Git(git) => self.remove_git(&git, scope),
                ParsedSource::Local(_) => Ok(()),
            },
        )
    }

    /// Remove and persist (upstream `removeAndPersist`).
    pub fn remove_and_persist(&self, source: &str, local: bool) -> Result<bool, String> {
        self.remove(source, local)?;
        Ok(self.remove_source_from_settings(source, local))
    }

    fn install_npm(
        &self,
        source: &NpmSource,
        scope: SourceScope,
        temporary: bool,
    ) -> Result<(), String> {
        let install_root = self.get_npm_install_root(scope, temporary)?;
        self.ensure_npm_project(&install_root);
        let args = self.get_npm_install_args(std::slice::from_ref(&source.spec), &install_root)?;
        self.run_npm_command(&args, None)
    }

    fn install_npm_batch(&self, specs: &[String], scope: SourceScope) -> Result<(), String> {
        let install_root = self.get_npm_install_root(scope, false)?;
        self.ensure_npm_project(&install_root);
        let args = self.get_npm_install_args(specs, &install_root)?;
        self.run_npm_command(&args, None)
    }

    fn uninstall_npm(&self, source: &NpmSource, scope: SourceScope) -> Result<(), String> {
        let install_root = self.get_npm_install_root(scope, false)?;
        if !install_root.exists() {
            return Ok(());
        }
        let package_manager_name = self.get_package_manager_name()?;
        if package_manager_name == "bun" {
            return self.run_npm_command(
                &[
                    "uninstall".to_string(),
                    source.name.clone(),
                    "--cwd".to_string(),
                    install_root.to_string_lossy().to_string(),
                ],
                None,
            );
        }
        let mut args = vec![
            "uninstall".to_string(),
            source.name.clone(),
            "--prefix".to_string(),
            install_root.to_string_lossy().to_string(),
        ];
        if package_manager_name != "pnpm" {
            args.push("--legacy-peer-deps".to_string());
        }
        self.run_npm_command(&args, None)
    }

    fn install_git(&self, source: &GitSource, scope: SourceScope) -> Result<(), String> {
        let target_dir = self.get_git_install_path(source, scope)?;
        if target_dir.exists() {
            if let Some(ref_) = &source.ref_ {
                return self.ensure_git_ref(
                    &target_dir,
                    &["fetch".to_string(), "origin".to_string(), ref_.clone()],
                    "FETCH_HEAD",
                );
            }
            let (ref_, _head, fetch_args) = self.get_local_git_update_target(&target_dir)?;
            return self.ensure_git_ref(&target_dir, &fetch_args, &ref_);
        }
        let git_root = self.get_git_install_root(scope).ok();
        if let Some(root) = &git_root {
            self.ensure_git_ignore(root);
        }
        if let Some(parent) = target_dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(self.get_git_update_marker_path(&target_dir));

        let result = (|| -> Result<(), String> {
            self.run_command(
                "git",
                &[
                    "clone".to_string(),
                    source.repo.clone(),
                    target_dir.to_string_lossy().to_string(),
                ],
                None,
            )?;
            if let Some(ref_) = &source.ref_ {
                self.run_command(
                    "git",
                    &["checkout".to_string(), ref_.clone()],
                    Some(&target_dir),
                )?;
            }
            let package_json_path = target_dir.join("package.json");
            if package_json_path.exists() {
                let args = self.get_git_dependency_install_args();
                self.run_npm_command(&args, Some(&target_dir))?;
            }
            Ok(())
        })();

        if let Err(error) = result {
            let _ = std::fs::remove_dir_all(&target_dir);
            self.prune_empty_git_parents(&target_dir, git_root.as_deref());
            return Err(error);
        }
        Ok(())
    }

    fn update_git(&self, source: &GitSource, scope: SourceScope) -> Result<(), String> {
        let target_dir = self.get_git_install_path(source, scope)?;
        if !target_dir.exists() {
            return self.install_git(source, scope);
        }
        if let Some(ref_) = &source.ref_ {
            return self.ensure_git_ref(
                &target_dir,
                &["fetch".to_string(), "origin".to_string(), ref_.clone()],
                "FETCH_HEAD",
            );
        }
        let (ref_, _head, fetch_args) = self.get_local_git_update_target(&target_dir)?;
        self.ensure_git_ref(&target_dir, &fetch_args, &ref_)
    }

    fn remove_git(&self, source: &GitSource, scope: SourceScope) -> Result<(), String> {
        let target_dir = self.get_git_install_path(source, scope)?;
        let _ = std::fs::remove_dir_all(&target_dir);
        let _ = std::fs::remove_file(self.get_git_update_marker_path(&target_dir));
        let install_root = self.get_git_install_root(scope).ok();
        self.prune_empty_git_parents(&target_dir, install_root.as_deref());
        Ok(())
    }

    fn refresh_temporary_git_source(&self, source: &GitSource, source_str: &str) {
        if Self::is_offline_mode() {
            return;
        }
        let result = self.with_progress(
            ProgressAction::Pull,
            source_str,
            &format!("Refreshing {source_str}..."),
            || self.update_git(source, SourceScope::Temporary),
        );
        // Keep cached temporary checkout if refresh fails.
        let _ = result;
    }

    /// Update configured sources (upstream `update`). When `source` is
    /// given, only packages with the matching identity update.
    pub fn update(&self, source: Option<&str>) -> Result<(), String> {
        if Self::is_offline_mode() {
            return Ok(());
        }
        let (global, project) = self.settings_snapshot();
        let identity = source.map(|s| self.get_package_identity(s, None));
        let mut matched = false;
        let mut update_sources: Vec<ConfiguredUpdateSource> = Vec::new();

        for pkg in Self::packages_of(&global) {
            let source_str = package_source_string(&pkg);
            if let Some(identity) = &identity {
                if self.get_package_identity(&source_str, Some(SourceScope::User)) != *identity {
                    continue;
                }
            }
            matched = true;
            update_sources.push(ConfiguredUpdateSource {
                source: source_str,
                scope: SourceScope::User,
            });
        }
        for pkg in Self::packages_of(&project) {
            let source_str = package_source_string(&pkg);
            if let Some(identity) = &identity {
                if self.get_package_identity(&source_str, Some(SourceScope::Project)) != *identity {
                    continue;
                }
            }
            matched = true;
            update_sources.push(ConfiguredUpdateSource {
                source: source_str,
                scope: SourceScope::Project,
            });
        }

        if let Some(source) = source {
            if !matched {
                let mut configured = Self::packages_of(&global);
                configured.extend(Self::packages_of(&project));
                return Err(self.build_no_matching_package_message(source, &configured));
            }
        }

        self.update_configured_sources(&update_sources)
    }

    fn update_configured_sources(&self, sources: &[ConfiguredUpdateSource]) -> Result<(), String> {
        if Self::is_offline_mode() || sources.is_empty() {
            return Ok(());
        }

        let mut npm_candidates: Vec<(&ConfiguredUpdateSource, NpmSource)> = Vec::new();
        let mut git_candidates: Vec<(&ConfiguredUpdateSource, GitSource)> = Vec::new();

        for entry in sources {
            match parse_source(&entry.source) {
                // Pinned npm versions are fixed; pinned git refs are
                // checkout targets that still reconcile.
                ParsedSource::Npm(npm) if !npm.pinned => npm_candidates.push((entry, npm)),
                ParsedSource::Git(git) => git_candidates.push((entry, git)),
                _ => {}
            }
        }

        // NPM checks run concurrently, then updates batch by scope.
        let checks: Vec<(bool, Result<(), String>)> = npm_candidates
            .iter()
            .map(|(entry, npm)| {
                let should = self.should_update_npm_source(npm, entry.scope);
                (*entry, npm, should)
            })
            .filter(|(_, _, should)| *should)
            .fold(Vec::new(), |mut acc, (entry, npm, _)| {
                acc.push((
                    entry.scope == SourceScope::User,
                    self.update_npm_batch(&[(entry, npm.clone())], entry.scope),
                ));
                acc
            })
            .into_iter()
            .collect();
        for (_, result) in checks {
            result?;
        }

        for (entry, git) in git_candidates {
            let source = entry.source.clone();
            self.with_progress(
                ProgressAction::Update,
                &source,
                &format!("Updating {source}..."),
                || self.update_git(&git, entry.scope),
            )?;
        }

        Ok(())
    }

    fn update_npm_batch(
        &self,
        sources: &[(&ConfiguredUpdateSource, NpmSource)],
        scope: SourceScope,
    ) -> Result<(), String> {
        if sources.is_empty() {
            return Ok(());
        }
        let scope_label = if scope == SourceScope::User {
            "user"
        } else {
            "project"
        };
        let (source_label, message) = if sources.len() == 1 {
            (
                sources[0].0.source.clone(),
                format!("Updating {}...", sources[0].0.source),
            )
        } else {
            (
                format!("{scope_label} npm packages"),
                format!("Updating {scope_label} npm packages..."),
            )
        };
        let specs: Vec<String> = sources
            .iter()
            .map(|(_, npm)| match &npm.version {
                Some(_) => npm.spec.clone(),
                None => format!("{}@latest", npm.name),
            })
            .collect();
        self.with_progress(ProgressAction::Update, &source_label, &message, || {
            self.install_npm_batch(&specs, scope)
        })
    }

    /// Packages with available updates (upstream
    /// `checkForAvailableUpdates`).
    pub fn check_for_available_updates(&self) -> Vec<PackageUpdate> {
        if Self::is_offline_mode() {
            return Vec::new();
        }
        let (global, project) = self.settings_snapshot();
        let mut all_packages: Vec<(PackageSource, SourceScope)> = Vec::new();
        for pkg in Self::packages_of(&project) {
            all_packages.push((pkg, SourceScope::Project));
        }
        for pkg in Self::packages_of(&global) {
            all_packages.push((pkg, SourceScope::User));
        }
        let package_sources = self.dedupe_packages(all_packages);

        let mut updates = Vec::new();
        for (pkg, scope) in package_sources {
            if scope == SourceScope::Temporary {
                continue;
            }
            let source = package_source_string(&pkg);
            match parse_source(&source) {
                ParsedSource::Npm(npm) if !npm.pinned => {
                    let installed_path = self.get_npm_install_path(&npm, scope);
                    if !installed_path.exists() {
                        continue;
                    }
                    if self.npm_has_available_update(&npm, &installed_path) {
                        updates.push(PackageUpdate {
                            source,
                            display_name: npm.name,
                            kind: UpdateKind::Npm,
                            scope,
                        });
                    }
                }
                ParsedSource::Git(git) => {
                    let Ok(installed_path) = self.get_git_install_path(&git, scope) else {
                        continue;
                    };
                    if !installed_path.exists() {
                        continue;
                    }
                    if self.git_has_available_update(&installed_path) {
                        updates.push(PackageUpdate {
                            source,
                            display_name: format!("{}/{}", git.host, git.path),
                            kind: UpdateKind::Git,
                            scope,
                        });
                    }
                }
                _ => {}
            }
        }
        updates
    }

    fn build_no_matching_package_message(
        &self,
        source: &str,
        configured_packages: &[PackageSource],
    ) -> String {
        match self.find_suggested_configured_source(source, configured_packages) {
            Some(suggestion) => {
                format!("No matching package found for {source}. Did you mean {suggestion}?")
            }
            None => format!("No matching package found for {source}"),
        }
    }

    fn find_suggested_configured_source(
        &self,
        source: &str,
        configured_packages: &[PackageSource],
    ) -> Option<String> {
        let trimmed_source = source.trim();
        for pkg in configured_packages {
            let source_str = package_source_string(pkg);
            match parse_source(&source_str) {
                ParsedSource::Npm(npm) => {
                    if trimmed_source == npm.name || trimmed_source == npm.spec {
                        return Some(source_str);
                    }
                }
                ParsedSource::Git(git) => {
                    let shorthand = format!("{}/{}", git.host, git.path);
                    let shorthand_with_ref = git.ref_.as_ref().map(|r| format!("{shorthand}@{r}"));
                    if trimmed_source == shorthand
                        || shorthand_with_ref
                            .as_deref()
                            .is_some_and(|s| trimmed_source == s)
                    {
                        return Some(source_str);
                    }
                }
                ParsedSource::Local(_) => {}
            }
        }
        None
    }

    /// Configured packages with install state (upstream
    /// `listConfiguredPackages`).
    pub fn list_configured_packages(&self) -> Vec<ConfiguredPackage> {
        let (global, project) = self.settings_snapshot();
        let mut configured = Vec::new();

        for pkg in Self::packages_of(&global) {
            let source = package_source_string(&pkg);
            configured.push(ConfiguredPackage {
                installed_path: self.get_installed_path(&source, SourceScope::User),
                source,
                scope: SourceScope::User,
                filtered: matches!(pkg, PackageSource::Filtered { .. }),
            });
        }
        for pkg in Self::packages_of(&project) {
            let source = package_source_string(&pkg);
            configured.push(ConfiguredPackage {
                installed_path: self.get_installed_path(&source, SourceScope::Project),
                source,
                scope: SourceScope::Project,
                filtered: matches!(pkg, PackageSource::Filtered { .. }),
            });
        }

        configured
    }

    /// Install path for a source if it exists (upstream
    /// `getInstalledPath`).
    pub fn get_installed_path(&self, source: &str, scope: SourceScope) -> Option<PathBuf> {
        match parse_source(source) {
            ParsedSource::Npm(npm) => {
                let path = self.get_npm_install_path(&npm, scope);
                path.exists().then_some(path)
            }
            ParsedSource::Git(git) => {
                let path = self.get_git_install_path(&git, scope).ok()?;
                path.exists().then_some(path)
            }
            ParsedSource::Local(path) => {
                let base_dir = self.get_base_dir_for_scope(scope);
                let path = self.resolve_path_from_base(&path, &base_dir);
                path.exists().then_some(path)
            }
        }
    }

    // --- resolve -------------------------------------------------------------------

    /// Resolve all resource paths (upstream `resolve`). npm/git sources
    /// that are missing are handled by `on_missing` (None means skip).
    pub fn resolve(
        &self,
        on_missing: Option<&mut dyn FnMut(&str) -> MissingSourceAction>,
    ) -> Result<ResolvedPaths, String> {
        let mut accumulator = ResourceAccumulator::default();
        let (global_settings, project_settings) = self.settings_snapshot();

        // Project first so cwd resources win collisions.
        let mut all_packages: Vec<(PackageSource, SourceScope)> = Vec::new();
        for pkg in Self::packages_of(&project_settings) {
            all_packages.push((pkg, SourceScope::Project));
        }
        for pkg in Self::packages_of(&global_settings) {
            all_packages.push((pkg, SourceScope::User));
        }
        let package_sources = self.dedupe_packages(all_packages);
        self.resolve_package_sources(&package_sources, &mut accumulator, on_missing)?;

        let global_base_dir = self.agent_dir.clone();
        let project_base_dir = self.cwd.join(CONFIG_DIR_NAME);

        for resource_type in RESOURCE_TYPES {
            let global_entries =
                Self::string_list_of(&global_settings, resource_type_name(resource_type));
            let project_entries =
                Self::string_list_of(&project_settings, resource_type_name(resource_type));
            self.resolve_local_entries(
                &project_entries,
                resource_type,
                accumulator.target(resource_type),
                PathMetadata {
                    source: "local".to_string(),
                    scope: SourceScope::Project,
                    origin: ResourceOrigin::TopLevel,
                    base_dir: None,
                },
                &project_base_dir,
            );
            self.resolve_local_entries(
                &global_entries,
                resource_type,
                accumulator.target(resource_type),
                PathMetadata {
                    source: "local".to_string(),
                    scope: SourceScope::User,
                    origin: ResourceOrigin::TopLevel,
                    base_dir: None,
                },
                &global_base_dir,
            );
        }

        self.add_auto_discovered_resources(
            &mut accumulator,
            &global_settings,
            &project_settings,
            &global_base_dir,
            &project_base_dir,
        );

        Ok(to_resolved_paths(accumulator))
    }

    /// Resolve explicit extension sources (upstream
    /// `resolveExtensionSources`).
    pub fn resolve_extension_sources(
        &self,
        sources: &[String],
        local: bool,
        temporary: bool,
    ) -> Result<ResolvedPaths, String> {
        let mut accumulator = ResourceAccumulator::default();
        let scope = if temporary {
            SourceScope::Temporary
        } else if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        let package_sources: Vec<(PackageSource, SourceScope)> = sources
            .iter()
            .map(|source| (PackageSource::Source(source.clone()), scope))
            .collect();
        self.resolve_package_sources(&package_sources, &mut accumulator, None)?;
        Ok(to_resolved_paths(accumulator))
    }

    fn resolve_package_sources(
        &self,
        sources: &[(PackageSource, SourceScope)],
        accumulator: &mut ResourceAccumulator,
        mut on_missing: Option<&mut dyn FnMut(&str) -> MissingSourceAction>,
    ) -> Result<(), String> {
        for (pkg, scope) in sources {
            let source_str = package_source_string(pkg);
            let filter = package_filter_of(pkg);
            let delta_base = self.find_autoload_delta_base(pkg, *scope, sources);
            let resolved_source = delta_base
                .as_ref()
                .map(|b| b.0.clone())
                .unwrap_or_else(|| source_str.clone());
            let resolved_scope = delta_base.as_ref().map(|b| b.1).unwrap_or(*scope);
            let parsed = parse_source(&resolved_source);
            let mut metadata = PathMetadata {
                source: source_str.clone(),
                scope: source_scope_of(*scope),
                origin: ResourceOrigin::Package,
                base_dir: None,
            };

            let ParsedSource::Local(local_path) = &parsed else {
                // npm / git: install or skip when missing.
                let mut install_missing = |this: &Self| -> Result<bool, String> {
                    if Self::is_offline_mode() {
                        return Ok(false);
                    }
                    match on_missing.as_deref_mut() {
                        None => {
                            this.install_parsed_source(&parsed, resolved_scope)?;
                            Ok(true)
                        }
                        Some(callback) => match callback(&resolved_source) {
                            MissingSourceAction::Skip => Ok(false),
                            MissingSourceAction::Error => {
                                Err(format!("Missing source: {resolved_source}"))
                            }
                            MissingSourceAction::Install => {
                                this.install_parsed_source(&parsed, resolved_scope)?;
                                Ok(true)
                            }
                        },
                    }
                };

                match &parsed {
                    ParsedSource::Npm(npm) => {
                        let mut installed_path = self.get_npm_install_path(npm, resolved_scope);
                        let needs_install = !installed_path.exists()
                            || !self.installed_npm_matches_configured_version(npm, &installed_path);
                        if needs_install {
                            if !install_missing(self)? {
                                continue;
                            }
                            installed_path = self.get_npm_install_path(npm, resolved_scope);
                        }
                        metadata.base_dir = Some(installed_path.clone());
                        self.collect_package_resources(
                            &installed_path,
                            accumulator,
                            filter.as_ref(),
                            &metadata,
                        );
                    }
                    ParsedSource::Git(git) => {
                        let installed_path = self.get_git_install_path(git, resolved_scope)?;
                        if !installed_path.exists() {
                            if !install_missing(self)? {
                                continue;
                            }
                        } else if resolved_scope == SourceScope::Temporary
                            && !git.pinned
                            && !Self::is_offline_mode()
                        {
                            self.refresh_temporary_git_source(git, &resolved_source);
                        }
                        metadata.base_dir = Some(installed_path.clone());
                        self.collect_package_resources(
                            &installed_path,
                            accumulator,
                            filter.as_ref(),
                            &metadata,
                        );
                    }
                    ParsedSource::Local(_) => unreachable!(),
                }
                continue;
            };

            let base_dir = self.get_base_dir_for_scope(resolved_scope);
            self.resolve_local_extension_source(
                local_path,
                accumulator,
                filter.as_ref(),
                &mut metadata,
                &base_dir,
            );
        }
        Ok(())
    }

    fn find_autoload_delta_base(
        &self,
        pkg: &PackageSource,
        scope: SourceScope,
        sources: &[(PackageSource, SourceScope)],
    ) -> Option<(String, SourceScope)> {
        if scope != SourceScope::Project {
            return None;
        }
        let PackageSource::Filtered {
            autoload: Some(false),
            source,
            ..
        } = pkg
        else {
            return None;
        };
        let identity = self.get_package_identity(source, Some(SourceScope::Project));
        sources
            .iter()
            .find(|(entry_pkg, entry_scope)| {
                *entry_scope == SourceScope::User
                    && self.get_package_identity(
                        &package_source_string(entry_pkg),
                        Some(SourceScope::User),
                    ) == identity
            })
            .map(|(entry_pkg, _)| (package_source_string(entry_pkg), SourceScope::User))
    }

    fn install_parsed_source(
        &self,
        parsed: &ParsedSource,
        scope: SourceScope,
    ) -> Result<(), String> {
        match parsed {
            ParsedSource::Npm(npm) => self.install_npm(npm, scope, scope == SourceScope::Temporary),
            ParsedSource::Git(git) => self.install_git(git, scope),
            ParsedSource::Local(_) => Ok(()),
        }
    }

    fn resolve_local_extension_source(
        &self,
        source_path: &str,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageFilter>,
        metadata: &mut PathMetadata,
        base_dir: &Path,
    ) {
        let resolved = self.resolve_path_from_base(source_path, base_dir);
        if !resolved.exists() {
            return;
        }

        let Ok(resolved_metadata) = std::fs::symlink_metadata(&resolved) else {
            return;
        };
        if resolved_metadata.is_file() {
            metadata.base_dir = resolved.parent().map(|p| p.to_path_buf());
            add_resource(
                accumulator.target(ResourceType::Extensions),
                resolved,
                metadata,
                true,
            );
            return;
        }
        if resolved_metadata.is_dir() {
            metadata.base_dir = Some(resolved.clone());
            let had_resources =
                self.collect_package_resources(&resolved, accumulator, filter, metadata);
            if !had_resources {
                add_resource(
                    accumulator.target(ResourceType::Extensions),
                    resolved,
                    metadata,
                    true,
                );
            }
        }
    }

    fn resolve_local_entries(
        &self,
        entries: &[String],
        resource_type: ResourceType,
        target: &mut ResourceMap,
        metadata: PathMetadata,
        base_dir: &Path,
    ) {
        if entries.is_empty() {
            return;
        }

        let (plain, patterns) = split_patterns(entries);
        let resolved_plain: Vec<PathBuf> = plain
            .iter()
            .map(|p| self.resolve_path_from_base(p, base_dir))
            .collect();
        let all_files = self.collect_files_from_paths(&resolved_plain, resource_type);

        let enabled_paths = apply_patterns(&all_files, &patterns, base_dir);

        for f in all_files {
            let enabled = enabled_paths.contains(&f);
            add_resource(target, f, &metadata, enabled);
        }
    }

    fn collect_files_from_paths(
        &self,
        paths: &[PathBuf],
        resource_type: ResourceType,
    ) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for p in paths {
            if !p.exists() {
                continue;
            }
            let Ok(metadata) = std::fs::metadata(p) else {
                continue;
            };
            if metadata.is_file() {
                files.push(p.clone());
            } else if metadata.is_dir() {
                files.extend(collect_resource_files(p, resource_type));
            }
        }
        files
    }

    /// Collect resources from an installed package root (upstream
    /// `collectPackageResources`). Returns whether any resource type was
    /// collected.
    fn collect_package_resources(
        &self,
        package_root: &Path,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageFilter>,
        metadata: &PathMetadata,
    ) -> bool {
        if let Some(filter) = filter {
            for resource_type in RESOURCE_TYPES {
                let patterns = package_filter_field(filter, resource_type);
                let target = accumulator.target(resource_type);
                if filter.autoload == Some(false) {
                    self.apply_package_delta_filter(
                        package_root,
                        &patterns.cloned().unwrap_or_default(),
                        resource_type,
                        target,
                        metadata,
                    );
                } else if let Some(patterns) = patterns {
                    self.apply_package_filter(
                        package_root,
                        patterns,
                        resource_type,
                        target,
                        metadata,
                    );
                } else {
                    self.collect_default_resources(package_root, resource_type, target, metadata);
                }
            }
            return true;
        }

        let manifest = read_pi_manifest(&package_root.join("package.json"));
        if let Some(manifest) = manifest {
            for resource_type in RESOURCE_TYPES {
                let entries = manifest_entries(&manifest, resource_type);
                self.add_manifest_entries(
                    entries,
                    package_root,
                    resource_type,
                    accumulator.target(resource_type),
                    metadata,
                );
            }
            return true;
        }

        let mut has_any_dir = false;
        for resource_type in RESOURCE_TYPES {
            let dir = package_root.join(resource_type_name(resource_type));
            if dir.exists() {
                for f in collect_resource_files(&dir, resource_type) {
                    add_resource(accumulator.target(resource_type), f, metadata, true);
                }
                has_any_dir = true;
            }
        }
        has_any_dir
    }

    fn collect_default_resources(
        &self,
        package_root: &Path,
        resource_type: ResourceType,
        target: &mut ResourceMap,
        metadata: &PathMetadata,
    ) {
        let manifest = read_pi_manifest(&package_root.join("package.json"));
        if let Some(entries) = manifest
            .as_ref()
            .and_then(|m| manifest_entries(m, resource_type))
        {
            self.add_manifest_entries(Some(entries), package_root, resource_type, target, metadata);
            return;
        }
        let dir = package_root.join(resource_type_name(resource_type));
        if dir.exists() {
            for f in collect_resource_files(&dir, resource_type) {
                add_resource(target, f, metadata, true);
            }
        }
    }

    fn apply_package_filter(
        &self,
        package_root: &Path,
        user_patterns: &[String],
        resource_type: ResourceType,
        target: &mut ResourceMap,
        metadata: &PathMetadata,
    ) {
        let all_files = self.collect_manifest_files(package_root, resource_type);

        if user_patterns.is_empty() {
            // Empty array explicitly disables all resources of this type.
            for f in all_files {
                add_resource(target, f, metadata, false);
            }
            return;
        }

        let enabled_by_user = apply_patterns(&all_files, user_patterns, package_root);
        for f in all_files {
            let enabled = enabled_by_user.contains(&f);
            add_resource(target, f, metadata, enabled);
        }
    }

    fn apply_package_delta_filter(
        &self,
        package_root: &Path,
        user_patterns: &[String],
        resource_type: ResourceType,
        target: &mut ResourceMap,
        metadata: &PathMetadata,
    ) {
        if user_patterns.is_empty() {
            return;
        }
        let all_files = self.collect_manifest_files(package_root, resource_type);
        let enabled_by_user =
            apply_autoload_disabled_patterns(&all_files, user_patterns, package_root);
        for (file_path, enabled) in enabled_by_user {
            add_resource(target, file_path, metadata, enabled);
        }
    }

    /// All files for a resource type from manifest entries or the
    /// convention dir (upstream `collectManifestFiles`), with manifest
    /// override patterns applied.
    fn collect_manifest_files(
        &self,
        package_root: &Path,
        resource_type: ResourceType,
    ) -> Vec<PathBuf> {
        let manifest = read_pi_manifest(&package_root.join("package.json"));
        if let Some(entries) = manifest
            .as_ref()
            .and_then(|m| manifest_entries(m, resource_type))
        {
            if !entries.is_empty() {
                let all_files =
                    self.collect_files_from_manifest_entries(entries, package_root, resource_type);
                let manifest_patterns: Vec<String> = entries
                    .iter()
                    .filter(|e| is_override_pattern(e))
                    .cloned()
                    .collect();
                if manifest_patterns.is_empty() {
                    return all_files;
                }
                return apply_patterns(&all_files, &manifest_patterns, package_root)
                    .into_iter()
                    .collect();
            }
        }

        let convention_dir = package_root.join(resource_type_name(resource_type));
        if !convention_dir.exists() {
            return Vec::new();
        }
        collect_resource_files(&convention_dir, resource_type)
    }

    fn add_manifest_entries(
        &self,
        entries: Option<&Vec<String>>,
        root: &Path,
        resource_type: ResourceType,
        target: &mut ResourceMap,
        metadata: &PathMetadata,
    ) {
        let Some(entries) = entries else {
            return;
        };

        let all_files = self.collect_files_from_manifest_entries(entries, root, resource_type);
        let patterns: Vec<String> = entries
            .iter()
            .filter(|e| is_override_pattern(e))
            .cloned()
            .collect();
        let enabled_paths = apply_patterns(&all_files, &patterns, root);

        for f in all_files {
            if enabled_paths.contains(&f) {
                add_resource(target, f, metadata, true);
            }
        }
    }

    fn collect_files_from_manifest_entries(
        &self,
        entries: &[String],
        root: &Path,
        resource_type: ResourceType,
    ) -> Vec<PathBuf> {
        let source_entries: Vec<&String> =
            entries.iter().filter(|e| !is_override_pattern(e)).collect();
        let mut resolved: Vec<PathBuf> = Vec::new();
        for entry in source_entries {
            if has_glob_pattern(entry) {
                resolved.extend(expand_package_glob(entry, root));
            } else {
                resolved.push(root.join(entry));
            }
        }
        self.collect_files_from_paths(&resolved, resource_type)
    }

    fn add_auto_discovered_resources(
        &self,
        accumulator: &mut ResourceAccumulator,
        global_settings: &serde_json::Value,
        project_settings: &serde_json::Value,
        global_base_dir: &Path,
        project_base_dir: &Path,
    ) {
        let user_metadata = PathMetadata {
            source: "auto".to_string(),
            scope: SourceScope::User,
            origin: ResourceOrigin::TopLevel,
            base_dir: Some(global_base_dir.to_path_buf()),
        };
        let project_metadata = PathMetadata {
            source: "auto".to_string(),
            scope: SourceScope::Project,
            origin: ResourceOrigin::TopLevel,
            base_dir: Some(project_base_dir.to_path_buf()),
        };

        let user_overrides: BTreeMap<&str, Vec<String>> = RESOURCE_TYPES
            .iter()
            .map(|rt| {
                (
                    resource_type_name(*rt),
                    Self::string_list_of(global_settings, resource_type_name(*rt)),
                )
            })
            .collect();
        let project_overrides: BTreeMap<&str, Vec<String>> = RESOURCE_TYPES
            .iter()
            .map(|rt| {
                (
                    resource_type_name(*rt),
                    Self::string_list_of(project_settings, resource_type_name(*rt)),
                )
            })
            .collect();

        let project_trusted = self.settings.lock().unwrap().is_project_trusted();
        let home = home_dir();
        let user_agents_skills_dir = home.join(".agents").join("skills");
        let project_agents_skill_dirs: Vec<PathBuf> = if project_trusted {
            collect_ancestor_agents_skill_dirs(&self.cwd)
                .into_iter()
                .filter(|dir| dir != &user_agents_skills_dir)
                .collect()
        } else {
            Vec::new()
        };

        let add_resources = |accumulator: &mut ResourceAccumulator,
                             resource_type: ResourceType,
                             paths: Vec<PathBuf>,
                             metadata: &PathMetadata,
                             overrides: &[String],
                             base_dir: &Path| {
            for path in paths {
                let enabled = is_enabled_by_overrides(&path, overrides, base_dir);
                add_resource(accumulator.target(resource_type), path, metadata, enabled);
            }
        };

        if project_trusted {
            // Project extensions from .pillar/
            let project_dirs = resource_dirs(project_base_dir);
            add_resources(
                accumulator,
                ResourceType::Extensions,
                collect_auto_extension_entries(&project_dirs.extensions),
                &project_metadata,
                &project_overrides["extensions"],
                project_base_dir,
            );
            add_resources(
                accumulator,
                ResourceType::Skills,
                collect_auto_skill_entries(&project_dirs.skills, SkillDiscoveryMode::Pi),
                &project_metadata,
                &project_overrides["skills"],
                project_base_dir,
            );
        }

        // Project skills from .agents/ (each with its own baseDir).
        for agents_skills_dir in &project_agents_skill_dirs {
            let agents_base_dir = agents_skills_dir
                .parent()
                .unwrap_or(agents_skills_dir)
                .to_path_buf();
            let mut agents_metadata = project_metadata.clone();
            agents_metadata.base_dir = Some(agents_base_dir.clone());
            add_resources(
                accumulator,
                ResourceType::Skills,
                collect_auto_skill_entries(agents_skills_dir, SkillDiscoveryMode::Agents),
                &agents_metadata,
                &project_overrides["skills"],
                &agents_base_dir,
            );
        }

        if project_trusted {
            let project_dirs = resource_dirs(project_base_dir);
            add_resources(
                accumulator,
                ResourceType::Prompts,
                collect_auto_prompt_entries(&project_dirs.prompts),
                &project_metadata,
                &project_overrides["prompts"],
                project_base_dir,
            );
            add_resources(
                accumulator,
                ResourceType::Themes,
                collect_auto_theme_entries(&project_dirs.themes),
                &project_metadata,
                &project_overrides["themes"],
                project_base_dir,
            );
        }

        // User resources from ~/.pillar/agent/ and ~/.agents/.
        let user_dirs = resource_dirs(global_base_dir);
        add_resources(
            accumulator,
            ResourceType::Extensions,
            collect_auto_extension_entries(&user_dirs.extensions),
            &user_metadata,
            &user_overrides["extensions"],
            global_base_dir,
        );
        add_resources(
            accumulator,
            ResourceType::Skills,
            collect_auto_skill_entries(&user_dirs.skills, SkillDiscoveryMode::Pi),
            &user_metadata,
            &user_overrides["skills"],
            global_base_dir,
        );
        let user_agents_base_dir = user_agents_skills_dir
            .parent()
            .unwrap_or(&user_agents_skills_dir)
            .to_path_buf();
        let mut user_agents_metadata = user_metadata.clone();
        user_agents_metadata.base_dir = Some(user_agents_base_dir.clone());
        add_resources(
            accumulator,
            ResourceType::Skills,
            collect_auto_skill_entries(&user_agents_skills_dir, SkillDiscoveryMode::Agents),
            &user_agents_metadata,
            &user_overrides["skills"],
            &user_agents_base_dir,
        );
        add_resources(
            accumulator,
            ResourceType::Prompts,
            collect_auto_prompt_entries(&user_dirs.prompts),
            &user_metadata,
            &user_overrides["prompts"],
            global_base_dir,
        );
        add_resources(
            accumulator,
            ResourceType::Themes,
            collect_auto_theme_entries(&user_dirs.themes),
            &user_metadata,
            &user_overrides["themes"],
            global_base_dir,
        );
    }
}

// ============================================================================
// Free helpers
// ============================================================================

fn package_source_from_json(value: &serde_json::Value) -> Option<PackageSource> {
    match value {
        serde_json::Value::String(source) => Some(PackageSource::Source(source.clone())),
        serde_json::Value::Object(obj) => {
            let source = obj.get("source")?.as_str()?.to_string();
            Some(PackageSource::Filtered {
                source,
                autoload: obj.get("autoload").and_then(|v| v.as_bool()),
                extensions: string_array(obj.get("extensions")),
                skills: string_array(obj.get("skills")),
                prompts: string_array(obj.get("prompts")),
                themes: string_array(obj.get("themes")),
            })
        }
        _ => None,
    }
}

fn string_array(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    value.and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    })
}

fn package_filter_field(
    filter: &PackageFilter,
    resource_type: ResourceType,
) -> Option<&Vec<String>> {
    match resource_type {
        ResourceType::Extensions => filter.extensions.as_ref(),
        ResourceType::Skills => filter.skills.as_ref(),
        ResourceType::Prompts => filter.prompts.as_ref(),
        ResourceType::Themes => filter.themes.as_ref(),
    }
}

fn manifest_entries(manifest: &PiManifest, resource_type: ResourceType) -> Option<&Vec<String>> {
    match resource_type {
        ResourceType::Extensions => manifest.extensions.as_ref(),
        ResourceType::Skills => manifest.skills.as_ref(),
        ResourceType::Prompts => manifest.prompts.as_ref(),
        ResourceType::Themes => manifest.themes.as_ref(),
    }
}

fn resource_type_name(resource_type: ResourceType) -> &'static str {
    match resource_type {
        ResourceType::Extensions => "extensions",
        ResourceType::Skills => "skills",
        ResourceType::Prompts => "prompts",
        ResourceType::Themes => "themes",
    }
}

struct ResourceDirs {
    extensions: PathBuf,
    skills: PathBuf,
    prompts: PathBuf,
    themes: PathBuf,
}

fn resource_dirs(base_dir: &Path) -> ResourceDirs {
    ResourceDirs {
        extensions: base_dir.join("extensions"),
        skills: base_dir.join("skills"),
        prompts: base_dir.join("prompts"),
        themes: base_dir.join("themes"),
    }
}

fn add_resource(map: &mut ResourceMap, path: PathBuf, metadata: &PathMetadata, enabled: bool) {
    if path.as_os_str().is_empty() {
        return;
    }
    map.entry(path)
        .or_insert_with(|| (metadata.clone(), enabled));
}

fn to_resolved_paths(accumulator: ResourceAccumulator) -> ResolvedPaths {
    fn map_to_resolved(entries: ResourceMap) -> Vec<ResolvedResource> {
        let mut resolved: Vec<ResolvedResource> = entries
            .into_iter()
            .map(|(path, (metadata, enabled))| ResolvedResource {
                path,
                enabled,
                metadata,
            })
            .collect();
        resolved.sort_by_key(|entry| resource_precedence_rank(&entry.metadata));

        let mut seen = BTreeSet::new();
        resolved
            .into_iter()
            .filter(|entry| {
                let canonical_path = canonicalize_path(&entry.path);
                seen.insert(canonical_path)
            })
            .collect()
    }

    ResolvedPaths {
        extensions: map_to_resolved(accumulator.extensions),
        skills: map_to_resolved(accumulator.skills),
        prompts: map_to_resolved(accumulator.prompts),
        themes: map_to_resolved(accumulator.themes),
    }
}

/// realpath when available, else the path itself (upstream
/// `canonicalizePath`).
pub fn canonicalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

/// Expand a glob entry under root, sorted, skipping dot segments (upstream
/// `expandPackageGlob`).
pub fn expand_package_glob(pattern: &str, root: &Path) -> Vec<PathBuf> {
    let glob = match GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
    {
        Ok(glob) => glob.compile_matcher(),
        Err(_) => return Vec::new(),
    };

    let mut matches: Vec<PathBuf> = Vec::new();
    // Walk the tree collecting candidate paths (globset has no walk; use
    // the ignore crate's walker for dot/node_modules-skipping traversal).
    for entry in ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .build()
    {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path == root {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_posix = to_posix_path(rel);
        if rel_posix.split('/').any(|segment| segment.starts_with('.')) {
            continue;
        }
        if glob.is_match(&rel_posix) {
            matches.push(root.join(rel));
        }
    }
    matches.sort();
    matches
}

fn first_40_hex(output: &str) -> Option<String> {
    for line in output.lines() {
        let hash: String = line.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        if hash.len() == 40 {
            return Some(hash);
        }
    }
    None
}

fn first_40_hex_head(output: &str) -> Option<String> {
    for line in output.lines() {
        let hash: String = line.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        if hash.len() == 40 && line.contains("HEAD") {
            return Some(hash);
        }
    }
    None
}

fn sha256_hex(input: &str) -> String {
    // FNV-1a based 64-bit hash hex, 16 chars — the port only needs a
    // stable short hash for temp dir naming (upstream uses sha256's first
    // 8 hex chars; collision behavior for temp dirs is equivalent in
    // practice).
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Mark a path ignored by cloud sync (upstream
/// `markPathIgnoredByCloudSync`): best-effort xattr on macOS.
fn mark_path_ignored_by_cloud_sync(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr")
            .args(["-w", "com.dropbox.ignored", "1"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::process::Command::new("xattr")
            .args(["-w", "com.apple.fileprovider.ignore#P", "1"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
    }
}
