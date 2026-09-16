//! Port of packages/coding-agent/src/core/skills.ts (pi v0.84.3) — the
//! filesystem discovery half (the prompt formatting lives in
//! [`crate::core::system_prompt`]).
//!
//! Discovery rules (upstream `loadSkillsFromDir`):
//! - a directory containing SKILL.md is a skill root; do not recurse further
//! - otherwise, load direct `.md` children in the root and recurse into
//!   subdirectories
//!
//! divergence: the `ignore` crate's gitignore matching is approximated with
//! a pattern check over `.gitignore`/`.ignore`/`.fdignore` contents
//! (substring-style suffix matching); the upstream YAML parser reports
//! "at line" errors, which the port reproduces for unclosed brackets/braces.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::system_prompt::{Skill, validate_skill_description, validate_skill_name};

/// Max name length per spec.
const MAX_NAME_LENGTH: usize = 64;

/// Max description length per spec.
const MAX_DESCRIPTION_LENGTH: usize = 1024;

const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

/// The loader's diagnostics shape lives in the extension contract (the runner
/// reports them; skills / resources / extensions all produce them).
pub use pillar_extensions_contract::ResourceDiagnostic;

/// Result of loading skills from one location.
#[derive(Debug, Clone, Default)]
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// Options for [`load_skills_from_dir`].
pub struct LoadSkillsFromDirOptions<'a> {
    pub dir: &'a Path,
    pub source: &'a str,
}

// ---------------------------------------------------------------------------
// Frontmatter parsing with YAML scalar/multiline support
// ---------------------------------------------------------------------------

/// Parsed SKILL.md frontmatter (upstream `SkillFrontmatter` subset) plus
/// raw fields for unknown-key tolerance.
pub struct ParsedSkillFile {
    pub name: Option<String>,
    pub description: Option<String>,
    pub disable_model_invocation: bool,
}

enum FrontmatterError {
    InvalidYaml(String),
}

/// Parse the YAML frontmatter of a SKILL.md. Supports plain scalars,
/// quoted scalars, and `|`/`>` block scalars (multiline descriptions).
/// Unclosed brackets/braces produce an "at line N" error like the upstream
/// YAML parser.
fn parse_skill_yaml(content: &str) -> Result<(ParsedSkillFile, String), FrontmatterError> {
    let normalized = content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .replace("\r\n", "\n");

    let body;
    let yaml_block: &str = if let Some(rest) = normalized.strip_prefix("---") {
        match rest.find("\n---") {
            Some(offset) => {
                body = rest[offset + 4..].trim().to_string();
                &rest[..offset]
            }
            None => {
                return Err(FrontmatterError::InvalidYaml(
                    "unterminated frontmatter".to_string(),
                ));
            }
        }
    } else {
        body = normalized.clone();
        return Ok((
            ParsedSkillFile {
                name: None,
                description: None,
                disable_model_invocation: false,
            },
            body,
        ));
    };

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut disable_model_invocation = false;

    let mut lines = yaml_block.lines().peekable();
    let mut line_number = 0usize;
    while let Some(line) = lines.next() {
        line_number += 1;
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value_part)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value_part.trim().to_string();

        // Block scalars: | (newline-joined) and > (space-joined).
        if value == "|" || value == ">" {
            let joined = value == "|";
            let mut collected: Vec<String> = Vec::new();
            while let Some(next) = lines.peek() {
                let trimmed_next = next.trim();
                if trimmed_next.is_empty()
                    || (!next.starts_with(' ') && !next.starts_with('\t'))
                    || next.contains(':') && !next.starts_with(' ')
                {
                    break;
                }
                collected.push(trimmed_next.to_string());
                line_number += 1;
                lines.next();
            }
            let joined_text = if joined {
                collected.join("\n")
            } else {
                collected.join(" ")
            };
            if key == "name" {
                name = Some(joined_text);
            } else if key == "description" {
                description = Some(joined_text);
            }
            continue;
        }

        // Unclosed bracket/brace detection (upstream: YAML parse error "at line N").
        if (value.starts_with('[') && !value.ends_with(']'))
            || (value.starts_with('{') && !value.ends_with('}'))
        {
            return Err(FrontmatterError::InvalidYaml(format!(
                "at line {line_number}"
            )));
        }

        let unquoted = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(&value)
            .to_string();
        match key {
            "name" => name = Some(unquoted),
            "description" => description = Some(unquoted),
            "disable-model-invocation" => disable_model_invocation = unquoted == "true",
            _ => {} // unknown fields are tolerated (no warning upstream)
        }
    }

    Ok((
        ParsedSkillFile {
            name,
            description,
            disable_model_invocation,
        },
        body,
    ))
}

// ---------------------------------------------------------------------------
// Ignore matching (approximation of the `ignore` crate)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct IgnoreMatcher {
    rules: Vec<(String, bool)>, // (pattern, negated)
}

impl IgnoreMatcher {
    fn add(&mut self, pattern: &str) {
        let trimmed = pattern.trim();
        if trimmed.is_empty() || (trimmed.starts_with('#') && !trimmed.starts_with("\\#")) {
            return;
        }
        let (pattern, negated) = if let Some(rest) = trimmed.strip_prefix('!') {
            (rest.to_string(), true)
        } else if let Some(rest) = trimmed.strip_prefix("\\!") {
            (rest.to_string(), false)
        } else {
            (trimmed.to_string(), false)
        };
        self.rules.push((pattern, negated));
    }

    /// Suffix-style match: a pattern matches a path when the path ends with
    /// the pattern, or when any path segment matches a bare pattern.
    fn ignores(&self, rel_path: &str) -> bool {
        let mut ignored = false;
        for (pattern, negated) in &self.rules {
            let pattern = pattern.trim_end_matches('/');
            if rel_path == pattern
                || rel_path.ends_with(&format!("/{pattern}"))
                || rel_path.starts_with(&format!("{pattern}/"))
                || rel_path.contains(&format!("/{pattern}/"))
            {
                ignored = !negated;
            }
        }
        ignored
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

    let mut pattern = line.to_string();
    let negated;
    if let Some(rest) = pattern.clone().strip_prefix('!') {
        negated = true;
        pattern = rest.to_string();
    } else if let Some(rest) = pattern.clone().strip_prefix("\\!") {
        negated = false;
        pattern = rest.to_string();
    } else {
        negated = false;
    }

    let pattern = pattern.strip_prefix('/').unwrap_or(&pattern).to_string();
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

fn add_ignore_rules(matcher: &mut IgnoreMatcher, dir: &Path, root_dir: &Path) {
    let Ok(relative_dir) = dir.strip_prefix(root_dir) else {
        return;
    };
    let relative = relative_dir.to_string_lossy().replace('\\', "/");
    let prefix = if relative.is_empty() {
        String::new()
    } else {
        format!("{relative}/")
    };

    for filename in IGNORE_FILE_NAMES {
        let ignore_path = dir.join(filename);
        let Ok(content) = std::fs::read_to_string(&ignore_path) else {
            continue;
        };
        for line in content.lines() {
            if let Some(pattern) = prefix_ignore_pattern(line, &prefix) {
                matcher.add(&pattern);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

fn load_skill_from_file(
    file_path: &Path,
    source: &str,
) -> (Option<Skill>, Vec<ResourceDiagnostic>) {
    let _ = source;
    let mut diagnostics = Vec::new();
    let is_declared_skill = file_path
        .file_name()
        .map(|name| name == "SKILL.md")
        .unwrap_or(false);

    let raw_content = match std::fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(ResourceDiagnostic::Warning {
                message: error.to_string(),
                path: file_path.to_string_lossy().to_string(),
            });
            return (None, diagnostics);
        }
    };

    let (parsed, _body) = match parse_skill_yaml(&raw_content) {
        Ok(parsed) => parsed,
        Err(FrontmatterError::InvalidYaml(message)) => {
            if is_declared_skill {
                diagnostics.push(ResourceDiagnostic::Warning {
                    message,
                    path: file_path.to_string_lossy().to_string(),
                });
            }
            return (None, diagnostics);
        }
    };

    let description = parsed.description.as_deref();
    let has_description = description.map(|d| !d.trim().is_empty()).unwrap_or(false);
    if !is_declared_skill && !has_description {
        return (None, diagnostics);
    }

    let skill_dir = file_path.parent().unwrap_or(Path::new(""));
    // Upstream uses `basename(dirname(filePath))` — the directory's own name.
    let parent_dir_name = skill_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();

    // Validate description
    for error in validate_skill_description(description) {
        diagnostics.push(ResourceDiagnostic::Warning {
            message: error,
            path: file_path.to_string_lossy().to_string(),
        });
    }

    // Use name from frontmatter, or fall back to parent directory name
    let name = parsed.name.clone().unwrap_or(parent_dir_name);

    // Validate name
    for error in validate_name_length_aware(&name) {
        diagnostics.push(ResourceDiagnostic::Warning {
            message: error,
            path: file_path.to_string_lossy().to_string(),
        });
    }

    // Still load the skill even with warnings, unless description is missing.
    if !has_description {
        return (None, diagnostics);
    }

    (
        Some(Skill {
            name,
            description: description.unwrap_or_default().to_string(),
            file_path: file_path.to_string_lossy().to_string(),
            base_dir: skill_dir.to_string_lossy().to_string(),
            disable_model_invocation: parsed.disable_model_invocation,
        }),
        diagnostics,
    )
}

/// Name validation including the length check with actual count (upstream
/// `validateName` renders the character count into the message).
fn validate_name_length_aware(name: &str) -> Vec<String> {
    let mut errors = validate_skill_name(name);
    if name.chars().count() > MAX_NAME_LENGTH {
        // Replace the generic message with the count-aware one.
        errors.retain(|error| !error.starts_with("name exceeds"));
        errors.insert(
            0,
            format!(
                "name exceeds {MAX_NAME_LENGTH} characters ({})",
                name.chars().count()
            ),
        );
    }
    errors
}

fn load_skills_from_dir_internal(
    dir: &Path,
    source: &str,
    include_root_files: bool,
    ignore_matcher: Option<&IgnoreMatcher>,
    root_dir: Option<&Path>,
) -> LoadSkillsResult {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();

    if !dir.exists() {
        return LoadSkillsResult {
            skills,
            diagnostics,
        };
    }

    let root = root_dir.unwrap_or(dir).to_path_buf();
    let mut own_matcher = IgnoreMatcher::default();
    if let Some(matcher) = ignore_matcher {
        own_matcher.rules = matcher.rules.clone();
    }
    add_ignore_rules(&mut own_matcher, dir, &root);

    let Ok(entries) = std::fs::read_dir(dir) else {
        return LoadSkillsResult {
            skills,
            diagnostics,
        };
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    entries.sort();

    // 1. SKILL.md in this directory wins and stops recursion.
    for path in &entries {
        if path
            .file_name()
            .map(|name| name == "SKILL.md")
            .unwrap_or(false)
        {
            if !path.is_file() {
                continue;
            }
            let rel_path = path
                .strip_prefix(&root)
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if own_matcher.ignores(&rel_path) {
                continue;
            }
            let (skill, skill_diagnostics) = load_skill_from_file(path, source);
            if let Some(skill) = skill {
                skills.push(skill);
            }
            diagnostics.extend(skill_diagnostics);
            return LoadSkillsResult {
                skills,
                diagnostics,
            };
        }
    }

    // 2. Recurse into subdirectories; load direct .md children.
    for path in &entries {
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
        else {
            continue;
        };
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }

        let rel_path = path
            .strip_prefix(&root)
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if path.is_dir() {
            if own_matcher.ignores(&format!("{rel_path}/")) {
                continue;
            }
            let sub =
                load_skills_from_dir_internal(path, source, false, Some(&own_matcher), Some(&root));
            skills.extend(sub.skills);
            diagnostics.extend(sub.diagnostics);
            continue;
        }

        if !path.is_file() || !include_root_files || !name.ends_with(".md") {
            continue;
        }
        if own_matcher.ignores(&rel_path) {
            continue;
        }
        let (skill, skill_diagnostics) = load_skill_from_file(path, source);
        if let Some(skill) = skill {
            skills.push(skill);
        }
        diagnostics.extend(skill_diagnostics);
    }

    LoadSkillsResult {
        skills,
        diagnostics,
    }
}

/// Load skills from a directory (upstream `loadSkillsFromDir`).
pub fn load_skills_from_dir(options: &LoadSkillsFromDirOptions) -> LoadSkillsResult {
    load_skills_from_dir_internal(options.dir, options.source, true, None, None)
}

/// Options for [`load_skills`].
pub struct LoadSkillsOptions<'a> {
    /// Working directory for project-local skills.
    pub cwd: &'a Path,
    /// Agent config directory for global skills.
    pub agent_dir: &'a Path,
    /// Explicit skill paths (files or directories).
    pub skill_paths: &'a [String],
    /// Include default skills directories.
    pub include_defaults: bool,
}

fn resolve_path(path: &str) -> PathBuf {
    let trimmed = path.trim();
    if trimmed == "~" || trimmed.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let rest = trimmed.strip_prefix('~').unwrap_or("");
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(trimmed)
}

fn canonicalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Load skills from all configured locations (upstream `loadSkills`):
/// default user/project dirs, then explicit paths; first skill with a name
/// wins, duplicates by real path or name are skipped/diagnosed.
pub fn load_skills(options: &LoadSkillsOptions) -> LoadSkillsResult {
    let resolved_cwd = resolve_path(options.cwd.to_string_lossy().as_ref());
    let resolved_agent_dir = resolve_path(options.agent_dir.to_string_lossy().as_ref());

    let mut skill_map: BTreeMap<String, Skill> = BTreeMap::new();
    let mut real_path_set: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut all_diagnostics: Vec<ResourceDiagnostic> = Vec::new();
    let mut collision_diagnostics: Vec<ResourceDiagnostic> = Vec::new();

    fn add_skills(
        result: LoadSkillsResult,
        skill_map: &mut BTreeMap<String, Skill>,
        real_path_set: &mut std::collections::BTreeSet<PathBuf>,
        all_diagnostics: &mut Vec<ResourceDiagnostic>,
        collision_diagnostics: &mut Vec<ResourceDiagnostic>,
    ) {
        let LoadSkillsResult {
            skills,
            diagnostics,
        } = result;
        all_diagnostics.extend(diagnostics);
        for skill in skills {
            let real_path = canonicalize(Path::new(&skill.file_path));
            if real_path_set.contains(&real_path) {
                continue;
            }
            if let Some(existing) = skill_map.get(&skill.name) {
                collision_diagnostics.push(ResourceDiagnostic::Collision {
                    message: format!("name \"{}\" collision", skill.name),
                    path: skill.file_path.clone(),
                });
                let _ = existing;
            } else {
                real_path_set.insert(real_path);
                skill_map.insert(skill.name.clone(), skill);
            }
        }
    }

    if options.include_defaults {
        add_skills(
            load_skills_from_dir_internal(
                &resolved_agent_dir.join("skills"),
                "user",
                true,
                None,
                None,
            ),
            &mut skill_map,
            &mut real_path_set,
            &mut all_diagnostics,
            &mut collision_diagnostics,
        );
        add_skills(
            load_skills_from_dir_internal(
                &resolved_cwd.join("pi").join("skills"),
                "project",
                true,
                None,
                None,
            ),
            &mut skill_map,
            &mut real_path_set,
            &mut all_diagnostics,
            &mut collision_diagnostics,
        );
    }

    for raw_path in options.skill_paths {
        let resolved_path = resolve_path(raw_path);
        if !resolved_path.exists() {
            all_diagnostics.push(ResourceDiagnostic::Warning {
                message: "skill path does not exist".to_string(),
                path: resolved_path.to_string_lossy().to_string(),
            });
            continue;
        }

        let meta = match std::fs::metadata(&resolved_path) {
            Ok(meta) => meta,
            Err(error) => {
                all_diagnostics.push(ResourceDiagnostic::Warning {
                    message: error.to_string(),
                    path: resolved_path.to_string_lossy().to_string(),
                });
                continue;
            }
        };
        if meta.is_dir() {
            add_skills(
                load_skills_from_dir_internal(&resolved_path, "path", true, None, None),
                &mut skill_map,
                &mut real_path_set,
                &mut all_diagnostics,
                &mut collision_diagnostics,
            );
        } else if meta.is_file()
            && resolved_path
                .extension()
                .map(|ext| ext == "md")
                .unwrap_or(false)
        {
            let (skill, diagnostics) = load_skill_from_file(&resolved_path, "path");
            if let Some(skill) = skill {
                add_skills(
                    LoadSkillsResult {
                        skills: vec![skill],
                        diagnostics,
                    },
                    &mut skill_map,
                    &mut real_path_set,
                    &mut all_diagnostics,
                    &mut collision_diagnostics,
                );
            } else {
                all_diagnostics.extend(diagnostics);
            }
        } else {
            all_diagnostics.push(ResourceDiagnostic::Warning {
                message: "skill path is not a markdown file".to_string(),
                path: resolved_path.to_string_lossy().to_string(),
            });
        }
    }

    LoadSkillsResult {
        skills: skill_map.into_values().collect(),
        diagnostics: {
            all_diagnostics.extend(collision_diagnostics);
            all_diagnostics
        },
    }
}

const _: () = {
    // Description validation constants must stay aligned with the spec.
    assert!(MAX_DESCRIPTION_LENGTH == 1024);
};
