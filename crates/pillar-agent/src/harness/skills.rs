//! Port of packages/agent/src/harness/skills.ts (pi v0.84.3).
//!
//! Loads skills from directories: recursive traversal, `SKILL.md` files,
//! direct root `.md` files with skill frontmatter, ignore files, and
//! diagnostics for invalid declared skill files. Missing input directories
//! are skipped.
//!
//! divergence: upstream uses the `ignore` npm crate (gitignore semantics);
//! the port uses `globset` globs over root-relative paths with the same
//! negation (`!pattern`) and anchoring handling from upstream's
//! `prefixIgnorePattern`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use globset::GlobBuilder;

use super::types::{FileErrorCode, FileKind, FileSystem, Skill};

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

/// Stable diagnostic codes for skill loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDiagnosticCode {
    FileInfoFailed,
    ListFailed,
    ReadFailed,
    ParseFailed,
    InvalidMetadata,
}

impl SkillDiagnosticCode {
    /// Upstream snake_case code string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FileInfoFailed => "file_info_failed",
            Self::ListFailed => "list_failed",
            Self::ReadFailed => "read_failed",
            Self::ParseFailed => "parse_failed",
            Self::InvalidMetadata => "invalid_metadata",
        }
    }
}

/// Warning produced while loading skills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    /// Diagnostic severity. Currently only warnings are emitted.
    pub kind: &'static str,
    /// Stable diagnostic code.
    pub code: SkillDiagnosticCode,
    /// Human-readable diagnostic message.
    pub message: String,
    /// Path associated with the diagnostic.
    pub path: String,
}

impl SkillDiagnostic {
    fn warning(code: SkillDiagnosticCode, message: impl Into<String>, path: &str) -> Self {
        Self {
            kind: "warning",
            code,
            message: message.into(),
            path: path.to_owned(),
        }
    }
}

/// Format a skill invocation prompt, optionally appending additional user
/// instructions (upstream `formatSkillInvocation`).
pub fn format_skill_invocation(skill: &Skill, additional_instructions: Option<&str>) -> String {
    let dir = dirname_env_path(&skill.file_path);
    let skill_block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name, skill.file_path, dir, skill.content
    );
    match additional_instructions {
        Some(instructions) => format!("{skill_block}\n\n{instructions}"),
        None => skill_block,
    }
}

/// Gitignore-style matcher over root-relative paths (upstream
/// `IgnoreMatcher`).
#[derive(Default)]
struct IgnoreMatcher {
    /// (glob pattern source, negated)
    patterns: Vec<(String, bool)>,
    /// Prebuilt matchers keyed by pattern source.
    built: HashMap<String, globset::GlobMatcher>,
}

impl IgnoreMatcher {
    fn add(&mut self, patterns: Vec<String>) {
        for pattern in patterns {
            let (negated, source) = if let Some(rest) = pattern.strip_prefix('!') {
                (true, rest.to_owned())
            } else {
                (false, pattern)
            };
            if self.built.contains_key(&source) {
                self.patterns.push((source, negated));
                continue;
            }
            // gitignore semantics: pattern without slash matches basename at
            // any depth; pattern with slash is anchored to root.
            let mut builder = GlobBuilder::new(&source);
            builder.literal_separator(true);
            if !source.contains('/') {
                builder.literal_separator(false);
            }
            if let Ok(glob) = builder.build() {
                self.built.insert(source.clone(), glob.compile_matcher());
            }
            self.patterns.push((source, negated));
        }
    }

    fn ignores(&self, candidate: &str) -> bool {
        let mut ignored = false;
        for (source, negated) in &self.patterns {
            let Some(matcher) = self.built.get(source) else {
                continue;
            };
            let hit = matcher.is_match(candidate)
                || (source.ends_with('/') && candidate.starts_with(source.as_str()));
            if hit {
                ignored = !negated;
            }
        }
        ignored
    }
}

/// Load skills from one or more directories.
pub async fn load_skills(
    env: &impl FileSystem,
    dirs: &[&str],
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    for dir in dirs {
        let root_info = match env.file_info(dir).await {
            Ok(info) => info,
            Err(error) => {
                if error.code != FileErrorCode::NotFound {
                    diagnostics.push(SkillDiagnostic::warning(
                        SkillDiagnosticCode::FileInfoFailed,
                        error.message,
                        dir,
                    ));
                }
                continue;
            }
        };
        if resolve_kind(env, &root_info, &mut diagnostics).await != Some(FileKind::Directory) {
            continue;
        }
        let mut matcher = IgnoreMatcher::default();
        let (dir_skills, dir_diagnostics) = load_skills_from_dir_internal(
            env,
            &root_info.path,
            true,
            &mut matcher,
            &root_info.path,
            &mut diagnostics,
        )
        .await;
        skills.extend(dir_skills);
        diagnostics.extend(dir_diagnostics);
    }
    (skills, diagnostics)
}

/// Load skills from source-tagged directories (upstream
/// `loadSourcedSkills`). Sources are preserved exactly.
pub async fn load_sourced_skills<TSource: Clone + PartialEq + std::fmt::Debug>(
    env: &impl FileSystem,
    inputs: &[(String, TSource)],
) -> (Vec<(Skill, TSource)>, Vec<(SkillDiagnostic, TSource)>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    for (path, source) in inputs {
        let (dir_skills, dir_diagnostics) = load_skills(env, &[path]).await;
        for skill in dir_skills {
            skills.push((skill, source.clone()));
        }
        for diagnostic in dir_diagnostics {
            diagnostics.push((diagnostic, source.clone()));
        }
    }
    (skills, diagnostics)
}

#[allow(clippy::too_many_arguments)]
async fn load_skills_from_dir_internal(
    env: &impl FileSystem,
    dir: &str,
    include_root_files: bool,
    ignore_matcher: &mut IgnoreMatcher,
    root_dir: &str,
    parent_diagnostics: &mut Vec<SkillDiagnostic>,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    // Box::pin breaks the async-fn size cycle from the recursive call.
    Box::pin(load_skills_from_dir_inner(
        env,
        dir,
        include_root_files,
        ignore_matcher,
        root_dir,
        parent_diagnostics,
    ))
    .await
}

async fn load_skills_from_dir_inner(
    env: &impl FileSystem,
    dir: &str,
    include_root_files: bool,
    ignore_matcher: &mut IgnoreMatcher,
    root_dir: &str,
    _parent_diagnostics: &mut Vec<SkillDiagnostic>,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diagnostics: Vec<SkillDiagnostic> = Vec::new();

    let dir_info = match env.file_info(dir).await {
        Ok(info) => info,
        Err(error) => {
            if error.code != FileErrorCode::NotFound {
                diagnostics.push(SkillDiagnostic::warning(
                    SkillDiagnosticCode::FileInfoFailed,
                    error.message,
                    dir,
                ));
            }
            return (skills, diagnostics);
        }
    };
    if resolve_kind(env, &dir_info, &mut diagnostics).await != Some(FileKind::Directory) {
        return (skills, diagnostics);
    }

    add_ignore_rules(env, ignore_matcher, dir, root_dir, &mut diagnostics).await;

    let entries = match env.list_dir(dir).await {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(SkillDiagnostic::warning(
                SkillDiagnosticCode::ListFailed,
                error.message,
                dir,
            ));
            return (skills, diagnostics);
        }
    };

    // First pass: SKILL.md children (upstream returns after the first one).
    for entry in &entries {
        if entry.name != "SKILL.md" {
            continue;
        }
        let full_path = entry.path.clone();
        let kind = resolve_kind(env, entry, &mut diagnostics).await;
        if kind != Some(FileKind::File) {
            continue;
        }
        let rel_path = relative_env_path(root_dir, &full_path);
        if ignore_matcher.ignores(&rel_path) {
            continue;
        }
        let (skill, file_diagnostics) = load_skill_from_file(env, &full_path, &dir_info.name).await;
        if let Some(skill) = skill {
            skills.push(skill);
        }
        diagnostics.extend(file_diagnostics);
        return (skills, diagnostics);
    }

    // Second pass: sorted entries; recurse into directories, load root
    // markdown files only when `include_root_files`.
    let mut sorted_entries = entries.clone();
    sorted_entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in &sorted_entries {
        if entry.name.starts_with('.') || entry.name == "node_modules" {
            continue;
        }
        let full_path = entry.path.clone();
        let kind = resolve_kind(env, entry, &mut diagnostics).await;
        let Some(kind) = kind else {
            continue;
        };

        let rel_path = relative_env_path(root_dir, &full_path);
        let ignore_path = if kind == FileKind::Directory {
            format!("{rel_path}/")
        } else {
            rel_path
        };
        if ignore_matcher.ignores(&ignore_path) {
            continue;
        }

        if kind == FileKind::Directory {
            let (sub_skills, sub_diagnostics) = load_skills_from_dir_internal(
                env,
                &full_path,
                false,
                ignore_matcher,
                root_dir,
                &mut Vec::new(),
            )
            .await;
            skills.extend(sub_skills);
            diagnostics.extend(sub_diagnostics);
            continue;
        }

        if kind != FileKind::File || !include_root_files || !entry.name.ends_with(".md") {
            continue;
        }
        let (skill, file_diagnostics) = load_skill_from_file(env, &full_path, &dir_info.name).await;
        if let Some(skill) = skill {
            skills.push(skill);
        }
        diagnostics.extend(file_diagnostics);
    }

    (skills, diagnostics)
}

async fn add_ignore_rules(
    env: &impl FileSystem,
    ignore_matcher: &mut IgnoreMatcher,
    dir: &str,
    root_dir: &str,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    let relative_dir = relative_env_path(root_dir, dir);
    let prefix = if relative_dir.is_empty() {
        String::new()
    } else {
        format!("{relative_dir}/")
    };

    for filename in IGNORE_FILE_NAMES {
        let ignore_path = match env.join_path(&[dir, filename]).await {
            Ok(path) => path,
            Err(error) => {
                diagnostics.push(SkillDiagnostic::warning(
                    SkillDiagnosticCode::FileInfoFailed,
                    error.message,
                    dir,
                ));
                continue;
            }
        };
        let info = match env.file_info(&ignore_path).await {
            Ok(info) => info,
            Err(error) => {
                if error.code != FileErrorCode::NotFound {
                    diagnostics.push(SkillDiagnostic::warning(
                        SkillDiagnosticCode::FileInfoFailed,
                        error.message,
                        &ignore_path,
                    ));
                }
                continue;
            }
        };
        if info.kind != FileKind::File {
            continue;
        }
        let content = match env.read_text_file(&ignore_path).await {
            Ok(content) => content,
            Err(error) => {
                diagnostics.push(SkillDiagnostic::warning(
                    SkillDiagnosticCode::ReadFailed,
                    error.message,
                    &ignore_path,
                ));
                continue;
            }
        };
        let patterns: Vec<String> = content
            .lines()
            .filter_map(|line| prefix_ignore_pattern(line, &prefix))
            .collect();
        if !patterns.is_empty() {
            ignore_matcher.add(patterns);
        }
    }
}

fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }

    let mut pattern = line.to_owned();
    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest.to_owned();
    } else if let Some(rest) = pattern.strip_prefix("\\!") {
        pattern = rest.to_owned();
    }
    let pattern = pattern.strip_prefix('/').unwrap_or(&pattern).to_owned();
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

/// Shared mutable matcher wrapper used by recursive calls (the recursion
/// passes `&mut IgnoreMatcher` directly; this alias keeps the signature
/// readable).
type IgnoreMatcherRef<'a> = &'a mut IgnoreMatcher;

#[allow(dead_code)]
fn matcher_ref(_: IgnoreMatcherRef<'_>) {}

async fn load_skill_from_file(
    env: &impl FileSystem,
    file_path: &str,
    parent_dir_name: &str,
) -> (Option<Skill>, Vec<SkillDiagnostic>) {
    let mut diagnostics = Vec::new();
    let is_declared_skill = file_path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|name| name == "SKILL.md");
    let raw_content = match env.read_text_file(file_path).await {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(SkillDiagnostic::warning(
                SkillDiagnosticCode::ReadFailed,
                error.message,
                file_path,
            ));
            return (None, diagnostics);
        }
    };

    let parsed = parse_frontmatter(&raw_content);
    if parsed.malformed {
        // Upstream parse() throws -> parse_failed diagnostic for declared
        // skills, plain skip for root markdown docs.
        if is_declared_skill {
            diagnostics.push(SkillDiagnostic::warning(
                SkillDiagnosticCode::ParseFailed,
                "malformed frontmatter YAML".to_owned(),
                file_path,
            ));
        }
        return (None, diagnostics);
    }
    let description = parsed
        .fields
        .iter()
        .find(|(k, _)| k == "description")
        .map(|(_, v)| v.clone())
        .filter(|v| !v.trim().is_empty());
    let disable_model_invocation = parsed
        .fields
        .iter()
        .find(|(k, _)| k == "disable-model-invocation")
        .is_some_and(|(_, v)| v == "true");
    let frontmatter_name = parsed
        .fields
        .iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.clone())
        .filter(|v| !v.trim().is_empty());

    if !is_declared_skill && description.is_none() {
        return (None, diagnostics);
    }

    for error in validate_description(description.as_deref()) {
        diagnostics.push(SkillDiagnostic::warning(
            SkillDiagnosticCode::InvalidMetadata,
            error,
            file_path,
        ));
    }

    let name = frontmatter_name.unwrap_or_else(|| parent_dir_name.to_owned());
    for error in validate_name(&name, parent_dir_name) {
        diagnostics.push(SkillDiagnostic::warning(
            SkillDiagnosticCode::InvalidMetadata,
            error,
            file_path,
        ));
    }

    let Some(description) = description else {
        return (None, diagnostics);
    };

    (
        Some(Skill {
            name,
            description,
            content: parsed.body,
            file_path: file_path.to_owned(),
            disable_model_invocation,
        }),
        diagnostics,
    )
}

fn validate_name(name: &str, parent_dir_name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name != parent_dir_name {
        errors.push(format!(
            "name \"{name}\" does not match parent directory \"{parent_dir_name}\""
        ));
    }
    if name.len() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_owned(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_owned());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_owned());
    }
    errors
}

fn validate_description(description: Option<&str>) -> Vec<String> {
    let mut errors = Vec::new();
    match description {
        None => errors.push("description is required".to_owned()),
        Some(description) if description.trim().is_empty() => {
            errors.push("description is required".to_owned())
        }
        Some(description) if description.len() > MAX_DESCRIPTION_LENGTH => errors.push(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
            description.len()
        )),
        Some(_) => {}
    }
    errors
}

struct Frontmatter {
    fields: Vec<(String, String)>,
    body: String,
    /// Upstream `parse()` threw on malformed YAML (e.g. unterminated flow
    /// sequences like `description: [invalid`) — the loader reports
    /// parse_failed for declared skills and skips the file.
    malformed: bool,
}

/// Flat frontmatter parser matching upstream's usage: `key: value` scalars
/// between `---` fences. Upstream parses full YAML; pi's skill frontmatter
/// keys are flat scalars, and malformed YAML flow syntax is detected the
/// same way upstream's `parse()` throws.
fn parse_frontmatter(content: &str) -> Frontmatter {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Frontmatter {
            fields: Vec::new(),
            body: normalized,
            malformed: false,
        };
    }
    let Some(end_index) = normalized[3..].find("\n---").map(|i| i + 3) else {
        return Frontmatter {
            fields: Vec::new(),
            body: normalized,
            malformed: false,
        };
    };
    let yaml_string = &normalized[4..end_index];
    let body = normalized[end_index + 4..].trim().to_owned();
    let mut fields = Vec::new();
    let mut malformed = false;
    for line in yaml_string.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_owned();
        let mut value = value.trim().to_owned();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_owned();
        }
        // YAML flow scalars: `[`/`{` open a sequence/mapping. An
        // unterminated opener is malformed (upstream parse() throws).
        if (value.starts_with('[') && !value.ends_with(']'))
            || (value.starts_with('{') && !value.ends_with('}'))
        {
            malformed = true;
        }
        if !key.is_empty() {
            fields.push((key, value));
        }
    }
    Frontmatter {
        fields,
        body,
        malformed,
    }
}

async fn resolve_kind(
    env: &impl FileSystem,
    info: &super::types::FileInfo,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Option<FileKind> {
    if info.kind == FileKind::File || info.kind == FileKind::Directory {
        return Some(info.kind);
    }
    let canonical_path = match env.canonical_path(&info.path).await {
        Ok(path) => path,
        Err(error) => {
            if error.code != FileErrorCode::NotFound {
                diagnostics.push(SkillDiagnostic::warning(
                    SkillDiagnosticCode::FileInfoFailed,
                    error.message,
                    &info.path,
                ));
            }
            return None;
        }
    };
    let target = match env.file_info(&canonical_path).await {
        Ok(info) => info,
        Err(error) => {
            if error.code != FileErrorCode::NotFound {
                diagnostics.push(SkillDiagnostic::warning(
                    SkillDiagnosticCode::FileInfoFailed,
                    error.message,
                    &info.path,
                ));
            }
            return None;
        }
    };
    match target.kind {
        FileKind::File | FileKind::Directory => Some(target.kind),
        _ => None,
    }
}

fn dirname_env_path(path: &str) -> String {
    let normalized = path.trim_end_matches(['/', '\\']);
    match normalized.rfind(['/', '\\']) {
        Some(index) if index == 2 && normalized.as_bytes().get(1) == Some(&b':') => {
            normalized[..3].to_owned()
        }
        Some(0) => "/".to_owned(),
        Some(index) => normalized[..index].to_owned(),
        None => "/".to_owned(),
    }
}

fn relative_env_path(root: &str, path: &str) -> String {
    let normalized_root = root.replace('\\', "/");
    let normalized_root = normalized_root.trim_end_matches('/');
    let normalized_path = path.replace('\\', "/");
    let normalized_path = normalized_path.trim_end_matches('/');
    if normalized_path == normalized_root {
        return String::new();
    }
    if let Some(rest) = normalized_path.strip_prefix(&format!("{normalized_root}/")) {
        rest.to_owned()
    } else {
        normalized_path.trim_start_matches('/').to_owned()
    }
}

// Keep the shared-state imports referenced for the sourced-skill loader's
// future Arc-based map callback parity.
type _Shared = Arc<Mutex<()>>;
