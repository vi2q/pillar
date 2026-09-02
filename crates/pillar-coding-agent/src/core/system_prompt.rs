//! Port of packages/coding-agent/src/core/system-prompt.ts (pi v0.84.3).
//!
//! System prompt construction. Skills-related types and formatting live here
//! alongside the prompt builder (upstream splits them into skills.ts, whose
//! filesystem discovery needs an ignore-pattern engine — see the divergence
//! note on [`Skill`]).
//!
//! divergence: `getReadmePath`/`getDocsPath`/`getExamplesPath` resolve
//! against the running binary's package directory upstream; the port takes
//! them as inputs ([`PromptPaths`]) so hosts can point at their own docs.

use std::collections::BTreeSet;

/// Paths shown in the prompt for pi's own documentation.
#[derive(Debug, Clone)]
pub struct PromptPaths {
    pub readme_path: String,
    pub docs_path: String,
    pub examples_path: String,
}

impl Default for PromptPaths {
    fn default() -> Self {
        Self {
            readme_path: "/opt/pi/README.md".to_string(),
            docs_path: "/opt/pi/docs".to_string(),
            examples_path: "/opt/pi/examples".to_string(),
        }
    }
}

/// A skill entry (upstream `Skill`; source metadata omitted — the prompt
/// only consumes name/description/path/invocation flag).
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: String,
    pub base_dir: String,
    pub disable_model_invocation: bool,
}

/// Pre-validated skill metadata parsed from SKILL.md frontmatter.
pub struct SkillFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub disable_model_invocation: bool,
}

/// Validate skill name per Agent Skills spec (upstream `validateName`).
pub fn validate_skill_name(name: &str) -> Vec<String> {
    const MAX_NAME_LENGTH: usize = 64;
    let mut errors = Vec::new();
    if name.chars().count() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.chars().count()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.is_empty()
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

/// Validate description per Agent Skills spec (upstream `validateDescription`).
pub fn validate_skill_description(description: Option<&str>) -> Vec<String> {
    const MAX_DESCRIPTION_LENGTH: usize = 1024;
    let mut errors = Vec::new();
    match description.map(str::trim) {
        None | Some("") => errors.push("description is required".to_string()),
        Some(trimmed) => {
            if trimmed.chars().count() > MAX_DESCRIPTION_LENGTH {
                errors.push(format!(
                    "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
                    trimmed.chars().count()
                ));
            }
        }
    }
    errors
}

/// Options for [`build_system_prompt`].
#[derive(Debug, Clone, Default)]
pub struct BuildSystemPromptOptions {
    /// Custom system prompt (replaces default).
    pub custom_prompt: Option<String>,
    /// Tools to include in prompt. Default: [read, bash, edit, write].
    pub selected_tools: Option<Vec<String>>,
    /// Optional one-line tool snippets keyed by tool name.
    pub tool_snippets: Option<BTreeMapLike>,
    /// Additional guideline bullets appended to the default guidelines.
    pub prompt_guidelines: Option<Vec<String>>,
    /// Text to append to system prompt.
    pub append_system_prompt: Option<String>,
    /// Working directory.
    pub cwd: String,
    /// Pre-loaded context files.
    pub context_files: Option<Vec<ContextFile>>,
    /// Pre-loaded skills.
    pub skills: Option<Vec<Skill>>,
    /// Pi documentation paths (divergence: resolved by the host).
    pub paths: PromptPaths,
}

/// Alias keeping the options struct readable (BTreeMap for deterministic
/// ordering of tool snippets).
pub type BTreeMapLike = std::collections::BTreeMap<String, String>;

/// A pre-loaded context file.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Format skills for inclusion in a system prompt (upstream
/// `formatSkillsForPrompt`, XML per agentskills.io).
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        "Use the read tool to load a skill's file when the task matches its description.".to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path)
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

fn append_project_context(prompt: &mut String, context_files: &[ContextFile]) {
    if context_files.is_empty() {
        return;
    }
    prompt.push_str("\n\n<project_context>\n\n");
    prompt.push_str("Project-specific instructions and guidelines:\n\n");
    for file in context_files {
        prompt.push_str(&format!(
            "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
            file.path, file.content
        ));
    }
    prompt.push_str("</project_context>\n");
}

/// Build the system prompt with tools, guidelines, and context (upstream
/// `buildSystemPrompt`).
pub fn build_system_prompt(options: &BuildSystemPromptOptions) -> String {
    let prompt_cwd = options.cwd.replace('\\', "/");
    let append_section = options
        .append_system_prompt
        .as_deref()
        .map(|text| format!("\n\n{text}"))
        .unwrap_or_default();

    let context_files = options.context_files.clone().unwrap_or_default();
    let skills = options.skills.clone().unwrap_or_default();

    if let Some(custom_prompt) = &options.custom_prompt {
        let mut prompt = custom_prompt.clone();
        prompt.push_str(&append_section);
        append_project_context(&mut prompt, &context_files);

        let custom_prompt_has_read = options
            .selected_tools
            .as_ref()
            .map(|tools| tools.iter().any(|tool| tool == "read"))
            .unwrap_or(true);
        if custom_prompt_has_read && !skills.is_empty() {
            prompt.push_str(&format_skills_for_prompt(&skills));
        }
        prompt.push_str(&format!("\nCurrent working directory: {prompt_cwd}\n"));
        return prompt;
    }

    let paths = &options.paths;
    let tools = options.selected_tools.clone().unwrap_or_else(|| {
        ["read", "bash", "edit", "write"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    });
    let visible_tools: Vec<&String> = tools
        .iter()
        .filter(|name| {
            options
                .tool_snippets
                .as_ref()
                .map(|snippets| snippets.contains_key(*name))
                .unwrap_or(false)
        })
        .collect();
    let tools_list = if visible_tools.is_empty() {
        "(none)".to_string()
    } else {
        visible_tools
            .iter()
            .map(|name| {
                let snippet = options
                    .tool_snippets
                    .as_ref()
                    .and_then(|snippets| snippets.get(*name))
                    .map(String::as_str)
                    .unwrap_or("");
                format!("- {name}: {snippet}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Build guidelines based on which tools are actually available,
    // deduplicated in insertion order.
    let mut guidelines_list: Vec<String> = Vec::new();
    let mut guidelines_set: BTreeSet<String> = BTreeSet::new();
    let mut add_guideline = |guideline: &str| {
        if guidelines_set.insert(guideline.to_string()) {
            guidelines_list.push(guideline.to_string());
        }
    };

    let has = |tool: &str| tools.iter().any(|candidate| candidate == tool);
    let has_bash = has("bash");
    let has_power_shell = has("powershell");
    let has_grep = has("grep");
    let has_find = has("find");
    let has_ls = has("ls");
    let has_read = has("read");

    if (has_bash || has_power_shell) && !has_grep && !has_find && !has_ls {
        if has_bash && has_power_shell {
            add_guideline(
                "Use bash or PowerShell for file operations like listing, searching, and finding files",
            );
        } else if has_power_shell {
            add_guideline(
                "Use PowerShell for file operations like listing, searching, and finding files",
            );
        } else {
            add_guideline("Use bash for file operations like ls, rg, find");
        }
    }

    if let Some(prompt_guidelines) = &options.prompt_guidelines {
        for guideline in prompt_guidelines {
            let normalized = guideline.trim();
            if !normalized.is_empty() {
                add_guideline(normalized);
            }
        }
    }

    add_guideline("Be concise in your responses");
    add_guideline("Show file paths clearly when working with files");

    let guidelines = guidelines_list
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut prompt = format!(
        "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.

Available tools:
{tools_list}

In addition to the tools above, you may have access to other custom tools depending on the project.

Guidelines:
{guidelines}

Pi documentation (read only when the user asks about pi itself, its SDK, extensions, themes, skills, or TUI):
- Main documentation: {}
- Additional docs: {}
- Examples: {} (extensions, custom tools, SDK)
- When reading pi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory
- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), pi packages (docs/packages.md), environment variables (docs/environment-variables.md)
- When working on pi topics, read the docs and examples, and follow .md cross-references before implementing
- Always read pi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)",
        paths.readme_path, paths.docs_path, paths.examples_path
    );

    prompt.push_str(&append_section);
    append_project_context(&mut prompt, &context_files);

    if has_read && !skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(&skills));
    }

    prompt.push_str(&format!("\nCurrent working directory: {prompt_cwd}"));
    prompt
}

// ---------------------------------------------------------------------------
// SKILL.md frontmatter helpers (subset of skills.ts usable without the
// ignore-pattern engine)
// ---------------------------------------------------------------------------

/// Extract the frontmatter name/description fields from a SKILL.md payload
/// using the same `---` fencing as upstream `parseFrontmatter` (YAML values
/// are read as plain scalars; nested YAML is not needed for these two keys).
pub fn parse_skill_frontmatter(content: &str) -> (SkillFrontmatter, String) {
    let normalized = content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let body;
    let mut yaml = String::new();
    if let Some(rest) = normalized.strip_prefix("---") {
        if let Some(offset) = rest.find("\n---") {
            let yaml_start = 3;
            let yaml_end = 3 + offset;
            yaml = normalized[yaml_start..yaml_end].to_string();
            body = normalized[yaml_end + 4..].trim().to_string();
        } else {
            body = normalized.trim().to_string();
        }
    } else {
        body = normalized.trim().to_string();
    }

    let mut name = None;
    let mut description = None;
    let mut disable_model_invocation = false;
    for line in yaml.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "disable-model-invocation" => disable_model_invocation = value == "true",
            _ => {}
        }
    }
    (
        SkillFrontmatter {
            name,
            description,
            disable_model_invocation,
        },
        body,
    )
}
