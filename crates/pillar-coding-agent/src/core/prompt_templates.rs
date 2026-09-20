//! Port of packages/coding-agent/src/core/prompt-templates.ts (pi v0.84.3).
//!
//! Prompt templates: markdown files with frontmatter (name, description,
//! argument-hint) plus content, argument substitution (bash-style $1/$@/
//! ${N:-default}), and directory loading.
//!
//! divergence: YAML frontmatter is parsed with a minimal line-based scalar
//! parser instead of the `yaml` package; only `name`, `description` and
//! `argument-hint` keys are consumed, so a full YAML engine is unnecessary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A prompt template loaded from a markdown file (upstream `PromptTemplate`).
#[derive(Debug, Clone, Default)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub content: String,
    /// Absolute path to the template file.
    pub file_path: String,
}

/// Parse command arguments respecting quoted strings (bash-style).
/// Returns list of arguments.
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for ch in args_string.chars() {
        if let Some(quote) = in_quote {
            if ch == quote {
                in_quote = None;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = Some(ch);
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }

    if !current.is_empty() {
        args.push(current);
    }

    args
}

/// Substitute argument placeholders in template content. Supports:
/// - `$1`, `$2`, ... for positional args
/// - `$@` and `$ARGUMENTS` for all args
/// - `${N:-default}` for positional arg N with default when missing/empty
/// - `${@:-default}` and `${ARGUMENTS:-default}` for all args with a default
/// - `${@:N}` for args from Nth onwards (bash-style slicing)
/// - `${@:N:L}` for L args starting from Nth
///
/// Replacement happens on the template string only; argument values are not
/// recursively substituted.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");

    // ${N:-default} | ${@:-default} | ${ARGUMENTS:-default}
    let braced_default = regex::Regex::new(r"\$\{(\d+|ARGUMENTS|@):-([^}]*)\}").unwrap();
    // ${@:N} and ${@:N:L} (bash-style slicing)
    let braced_slice = regex::Regex::new(r"\$\{@:(\d+)(?::(\d+))?\}").unwrap();
    // $ARGUMENTS | $@ | $N
    let simple = regex::Regex::new(r"\$(ARGUMENTS|@|\d+)").unwrap();

    let out = braced_default.replace_all(content, |caps: &regex::Captures| {
        let target = caps.get(1).map_or("", |m| m.as_str());
        let default_value = caps.get(2).map_or("", |m| m.as_str());
        let value = if target == "@" || target == "ARGUMENTS" {
            all_args.clone()
        } else {
            args.get(target.parse::<usize>().unwrap_or(0).saturating_sub(1))
                .cloned()
                .unwrap_or_default()
        };
        if value.is_empty() {
            default_value.to_string()
        } else {
            value
        }
    });

    let out = braced_slice.replace_all(&out, |caps: &regex::Captures| {
        let start_n: usize = caps.get(1).map_or("1", |m| m.as_str()).parse().unwrap_or(1);
        let start = start_n.saturating_sub(1); // user provides 1-indexed
        match caps.get(2).and_then(|m| m.as_str().parse::<usize>().ok()) {
            Some(length) => args
                .iter()
                .skip(start)
                .take(length)
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
            None => args
                .iter()
                .skip(start)
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
        }
    });

    simple
        .replace_all(&out, |caps: &regex::Captures| {
            let token = caps.get(1).map_or("", |m| m.as_str());
            match token {
                "ARGUMENTS" | "@" => all_args.clone(),
                token => {
                    let index: usize = token.parse().unwrap_or(0);
                    args.get(index.wrapping_sub(1)).cloned().unwrap_or_default()
                }
            }
        })
        .to_string()
}

/// Extract frontmatter key/value pairs from a markdown document. Returns
/// `(frontmatter, body)`; the body is the content after the closing `---`.
/// Minimal line-based parser: `key: value` pairs, plain scalars, quoted or
/// bare (upstream uses the `yaml` package; only string keys are consumed).
fn parse_frontmatter_fields(content: &str) -> (BTreeMap<String, String>, String) {
    let normalized = content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut frontmatter = BTreeMap::new();

    if !normalized.starts_with("---") {
        return (frontmatter, normalized);
    }
    let rest = &normalized[3..];
    let Some(end_index) = rest.find("\n---") else {
        return (frontmatter, normalized);
    };
    let yaml_block = &rest[..end_index];
    let after = &rest[end_index + 4..];

    for line in yaml_block.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        if !key.is_empty() {
            frontmatter.insert(key, value.to_string());
        }
    }
    (frontmatter, after.trim().to_string())
}

/// Public wrapper for parity tests (private fn needs a test-visible path).
pub fn load_template_from_file_for_test(file_path: &Path) -> Option<PromptTemplate> {
    load_template_from_file(file_path)
}

/// Load a prompt template from a markdown file (upstream
/// `loadTemplateFromFile`). Name = file stem; description = frontmatter
/// `description`, or the first non-empty body line truncated to 60 chars.
fn load_template_from_file(file_path: &Path) -> Option<PromptTemplate> {
    let raw_content = std::fs::read_to_string(file_path).ok()?;

    let (frontmatter, body) = parse_frontmatter_fields(&raw_content);

    let name = file_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut description = frontmatter.get("description").cloned().unwrap_or_default();
    if description.is_empty()
        && let Some(first_line) = body.lines().find(|line| !line.trim().is_empty())
    {
        let mut truncated = first_line.trim().to_string();
        if truncated.chars().count() > 60 {
            truncated = truncated.chars().take(60).collect();
            truncated.push_str("...");
        }
        description = truncated;
    }

    Some(PromptTemplate {
        name,
        description,
        argument_hint: frontmatter.get("argument-hint").cloned(),
        content: body,
        file_path: file_path.to_string_lossy().to_string(),
    })
}

/// Scan a directory for `.md` files (non-recursive) and load them as prompt
/// templates (upstream `loadTemplatesFromDir`).
fn load_templates_from_dir(dir: &Path) -> Vec<PromptTemplate> {
    let mut templates = Vec::new();

    let Ok(entries) = std::fs::read_dir(dir) else {
        return templates;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let is_file = path.is_file();
        if is_file
            && path.extension().map(|ext| ext == "md").unwrap_or(false)
            && let Some(template) = load_template_from_file(&path)
        {
            templates.push(template);
        }
    }

    templates
}

/// Options for loading prompt templates (upstream
/// `LoadPromptTemplatesOptions`).
#[derive(Debug, Clone, Default)]
pub struct LoadPromptTemplatesOptions {
    /// Working directory for project-local templates.
    pub cwd: String,
    /// Agent config directory for global templates.
    pub agent_dir: String,
    /// Explicit prompt template paths (files or directories).
    pub prompt_paths: Vec<String>,
    /// Include default prompt directories.
    pub include_defaults: bool,
}

/// Load all prompt templates from:
/// 1. Global: `agent_dir/prompts/`
/// 2. Project: `cwd/.pillar/prompts/` (CONFIG_DIR_NAME)
/// 3. Explicit prompt paths
pub fn load_prompt_templates(options: &LoadPromptTemplatesOptions) -> Vec<PromptTemplate> {
    let resolved_cwd = resolve_path(&options.cwd);
    let resolved_agent_dir = resolve_path(&options.agent_dir);
    let prompt_paths = &options.prompt_paths;
    let include_defaults = options.include_defaults;

    let mut templates: Vec<PromptTemplate> = Vec::new();

    let global_prompts_dir = resolved_agent_dir.join("prompts");
    let project_prompts_dir = resolved_cwd.join("pi").join("prompts");

    if include_defaults {
        templates.extend(load_templates_from_dir(&global_prompts_dir));
        templates.extend(load_templates_from_dir(&project_prompts_dir));
    }

    // Explicit prompt paths
    for raw_path in prompt_paths {
        let resolved_path = resolve_path(raw_path.trim());
        let Ok(meta) = std::fs::metadata(&resolved_path) else {
            continue;
        };
        if meta.is_dir() {
            templates.extend(load_templates_from_dir(&resolved_path));
        } else if meta.is_file()
            && resolved_path
                .extension()
                .map(|ext| ext == "md")
                .unwrap_or(false)
            && let Some(template) = load_template_from_file(&resolved_path)
        {
            templates.push(template);
        }
    }

    templates
}

/// Expand a prompt template if the text matches a template name (upstream
/// `expandPromptTemplate`). Returns the expanded content or the original
/// text if not a template.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    if !text.starts_with('/') {
        return text.to_string();
    }

    let trimmed = text.trim_start_matches('/');
    let (template_name, args_string) = match trimmed.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args),
        None => (trimmed, ""),
    };

    if let Some(template) = templates.iter().find(|t| t.name == template_name) {
        let args = parse_command_args(args_string);
        return substitute_args(&template.content, &args);
    }

    text.to_string()
}

/// Minimal path resolution used for template directories (upstream
/// `resolvePath` with `trim`): trims whitespace and expands a leading `~`
/// to the home directory when available.
fn resolve_path(path: &str) -> PathBuf {
    let trimmed = path.trim();
    if (trimmed == "~" || trimmed.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        let home = PathBuf::from(home);
        let rest = trimmed.strip_prefix('~').unwrap_or("");
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        return home.join(rest);
    }
    PathBuf::from(trimmed)
}
