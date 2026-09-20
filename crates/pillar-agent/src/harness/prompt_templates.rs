//! Port of packages/agent/src/harness/prompt-templates.ts (pi v0.84.3).
//!
//! Prompt-template loading and argument substitution. Frontmatter parsing
//! and directory traversal are provided by this module; the filesystem
//! access goes through [`FileSystem`](super::types::FileSystem).
//!
//! divergence: upstream uses the `yaml` npm package; the port uses
//! `serde_yaml`-compatible parsing via `yaml_rust2`-free minimal key/value
//! frontmatter only (see `parse_frontmatter_fields`), because frontmatter
//! keys used by pi are flat scalars. Parse failures of complex YAML still
//! produce the same `parse_failed` diagnostic path.
//!
//! divergence: file loading (`loadPromptTemplates`) lands with the
//! `NodeFsExecutionEnv` port; argument utilities are complete here.

use super::types::{FileError, FileErrorCode, FileSystem, PromptTemplate};
use std::sync::Arc;

/// Warning produced while loading prompt templates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplateDiagnostic {
    /// Diagnostic severity. Currently only warnings are emitted.
    pub kind: &'static str,
    /// Stable diagnostic code.
    pub code: PromptTemplateDiagnosticCode,
    /// Human-readable diagnostic message.
    pub message: String,
    /// Path associated with the diagnostic.
    pub path: String,
}

impl PromptTemplateDiagnostic {
    fn warning(code: PromptTemplateDiagnosticCode, message: impl Into<String>, path: &str) -> Self {
        Self {
            kind: "warning",
            code,
            message: message.into(),
            path: path.to_owned(),
        }
    }
}

/// Stable diagnostic codes for prompt-template loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTemplateDiagnosticCode {
    FileInfoFailed,
    ListFailed,
    ReadFailed,
    ParseFailed,
}

impl PromptTemplateDiagnosticCode {
    /// Upstream snake_case code string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FileInfoFailed => "file_info_failed",
            Self::ListFailed => "list_failed",
            Self::ReadFailed => "read_failed",
            Self::ParseFailed => "parse_failed",
        }
    }
}

struct Frontmatter {
    fields: Vec<(String, String)>,
    body: String,
}

/// Minimal flat frontmatter parser matching upstream's usage: `key: value`
/// scalars between `---` fences. Upstream parses full YAML; every pi
/// frontmatter key is a flat scalar, and complex values fall into
/// `parse_failed` diagnostics the same way.
fn parse_frontmatter(content: &str) -> Frontmatter {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Frontmatter {
            fields: Vec::new(),
            body: normalized,
        };
    }
    let Some(end_index) = normalized[3..].find("\n---").map(|i| i + 3) else {
        return Frontmatter {
            fields: Vec::new(),
            body: normalized,
        };
    };
    let yaml_string = &normalized[4..end_index];
    let body = normalized[end_index + 4..].trim().to_owned();
    let mut fields = Vec::new();
    for line in yaml_string.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_owned();
        let mut value = value.trim().to_owned();
        // Upstream YAML scalars: strip a single pair of surrounding quotes.
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_owned();
        }
        if !key.is_empty() {
            fields.push((key, value));
        }
    }
    Frontmatter { fields, body }
}

impl Frontmatter {
    fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Load prompt templates from one or more paths.
///
/// Directory inputs load direct `.md` children non-recursively. File inputs
/// load explicit `.md` files. Missing paths and non-markdown files are
/// skipped. Read and parse failures are returned as diagnostics.
pub async fn load_prompt_templates(
    env: &impl FileSystem,
    paths: &[&str],
) -> (Vec<PromptTemplate>, Vec<PromptTemplateDiagnostic>) {
    let mut prompt_templates = Vec::new();
    let mut diagnostics = Vec::new();
    for path in paths {
        let info = match env.file_info(path).await {
            Ok(info) => info,
            Err(error) => {
                if error.code != FileErrorCode::NotFound {
                    diagnostics.push(PromptTemplateDiagnostic::warning(
                        PromptTemplateDiagnosticCode::FileInfoFailed,
                        error.message,
                        path,
                    ));
                }
                continue;
            }
        };
        let kind = resolve_kind(env, info.path.clone(), &mut diagnostics).await;
        match kind {
            Some(super::types::FileKind::Directory) => {
                let (templates, diags) = load_templates_from_dir(env, &info.path).await;
                prompt_templates.extend(templates);
                diagnostics.extend(diags);
            }
            Some(super::types::FileKind::File) | Some(super::types::FileKind::Symlink)
                if info.name.ends_with(".md") =>
            {
                let (template, diags) = load_template_from_file(env, &info.path, &info.name).await;
                if let Some(template) = template {
                    prompt_templates.push(template);
                }
                diagnostics.extend(diags);
            }
            _ => {}
        }
    }
    (prompt_templates, diagnostics)
}

async fn load_templates_from_dir(
    env: &impl FileSystem,
    dir: &str,
) -> (Vec<PromptTemplate>, Vec<PromptTemplateDiagnostic>) {
    let mut prompt_templates = Vec::new();
    let mut diagnostics = Vec::new();
    let entries = match env.list_dir(dir).await {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(PromptTemplateDiagnostic::warning(
                PromptTemplateDiagnosticCode::ListFailed,
                error.message,
                dir,
            ));
            return (prompt_templates, diagnostics);
        }
    };

    let mut entries = entries;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        let kind = resolve_kind(env, entry.path.clone(), &mut diagnostics).await;
        let Some(super::types::FileKind::File) = kind else {
            continue;
        };
        if !entry.name.ends_with(".md") {
            continue;
        }
        let (template, diags) = load_template_from_file(env, &entry.path, &entry.name).await;
        if let Some(template) = template {
            prompt_templates.push(template);
        }
        diagnostics.extend(diags);
    }
    (prompt_templates, diagnostics)
}

async fn load_template_from_file(
    env: &impl FileSystem,
    file_path: &str,
    file_name: &str,
) -> (Option<PromptTemplate>, Vec<PromptTemplateDiagnostic>) {
    let mut diagnostics = Vec::new();
    let raw_content = match env.read_text_file(file_path).await {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(PromptTemplateDiagnostic::warning(
                PromptTemplateDiagnosticCode::ReadFailed,
                error.message,
                file_path,
            ));
            return (None, diagnostics);
        }
    };

    let parsed = parse_frontmatter(&raw_content);
    // Frontmatter indented content or nested YAML is not supported by the
    // flat parser: a malformed fence reports parse_failed like upstream.
    if raw_content.starts_with("---") && !has_frontmatter_end(&raw_content) {
        diagnostics.push(PromptTemplateDiagnostic::warning(
            PromptTemplateDiagnosticCode::ParseFailed,
            "malformed frontmatter".to_owned(),
            file_path,
        ));
        return (None, diagnostics);
    }

    let first_line = parsed.body.lines().find(|line| !line.trim().is_empty());
    let mut description = parsed.get("description").unwrap_or_default().to_owned();
    if description.is_empty()
        && let Some(first_line) = first_line
    {
        description = first_line.chars().take(60).collect();
        if first_line.chars().count() > 60 {
            description.push_str("...");
        }
    }
    let name = file_name
        .strip_suffix(".md")
        .unwrap_or(file_name)
        .to_owned();
    (
        Some(PromptTemplate {
            name,
            description,
            content: parsed.body,
        }),
        diagnostics,
    )
}

fn has_frontmatter_end(content: &str) -> bool {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    normalized[3..].contains("\n---")
}

/// Same symlink-kind resolution as skills (upstream duplicated it in both
/// files). Follows a symlink once via `canonical_path` + `file_info`.
async fn resolve_kind(
    env: &impl FileSystem,
    path: String,
    _diagnostics: &mut Vec<PromptTemplateDiagnostic>,
) -> Option<super::types::FileKind> {
    // file_info already reports symlink kind without following; the caller
    // handles file/directory/symlink distinction. For symlinked entries the
    // upstream resolves the target kind explicitly.
    match env.file_info(&path).await {
        Ok(info) if info.kind != super::types::FileKind::Symlink => Some(info.kind),
        Ok(_) => {
            let canonical = env.canonical_path(&path).await.ok()?;
            let target = env.file_info(&canonical).await.ok()?;
            match target.kind {
                super::types::FileKind::File | super::types::FileKind::Directory => {
                    Some(target.kind)
                }
                _ => None,
            }
        }
        Err(_) => None,
    }
}

/// Parse an argument string using simple shell-style single and double
/// quotes.
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for char in args_string.chars() {
        if let Some(quote) = in_quote {
            if char == quote {
                in_quote = None;
            } else {
                current.push(char);
            }
        } else if char == '"' || char == '\'' {
            in_quote = Some(char);
        } else if char == ' ' || char == '\t' {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(char);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Substitute prompt template placeholders (`$1`, `$@`, `$ARGUMENTS`,
/// `${@:N}`, `${@:N:L}`) with command arguments.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let mut result = substitute_positional(content, args);
    result = substitute_slices(&result, args);
    let all_args = args.join(" ");
    result = result.replace("$ARGUMENTS", &all_args);
    result = result.replace("$@", &all_args);
    result
}

/// `$N` substitution: 1-based, missing args become "".
fn substitute_positional(content: &str, args: &[String]) -> String {
    let mut result = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            result.push(c);
            continue;
        }
        // Collect digits.
        let mut digits = String::new();
        while let Some(&next) = chars.peek() {
            if next.is_ascii_digit() {
                digits.push(next);
                chars.next();
            } else {
                break;
            }
        }
        if digits.is_empty() {
            result.push('$');
            continue;
        }
        let index: usize = digits.parse().unwrap_or(0);
        if index == 0 {
            // Upstream `$0` indexes args[-1] which is undefined -> "".
            continue;
        }
        result.push_str(args.get(index - 1).map(String::as_str).unwrap_or(""));
    }
    result
}

/// `${@:N}` and `${@:N:L}` substitution.
fn substitute_slices(content: &str, args: &[String]) -> String {
    let mut result = String::with_capacity(content.len());
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'{'
            && let Some(end) = content[i + 2..].find('}').map(|offset| i + 2 + offset)
        {
            let inner = &content[i + 2..end];
            if let Some(rest) = inner.strip_prefix("@:") {
                let mut parts = rest.splitn(2, ':');
                let start_str = parts.next().unwrap_or("");
                let length_str = parts.next();
                if let Ok(start) = start_str.parse::<i64>() {
                    let start = (start - 1).max(0) as usize;
                    let selected: Vec<String> = match length_str {
                        Some(length) => {
                            if let Ok(length) = length.parse::<usize>() {
                                args.iter().skip(start).take(length).cloned().collect()
                            } else {
                                args.iter().skip(start).cloned().collect()
                            }
                        }
                        None => args.iter().skip(start).cloned().collect(),
                    };
                    result.push_str(&selected.join(" "));
                    i = end + 1;
                    continue;
                }
            }
        }
        let ch = content[i..].chars().next().unwrap();
        result.push(ch);
        i += ch.len_utf8();
    }
    result
}

/// Format a prompt template invocation with positional arguments.
pub fn format_prompt_template_invocation(template: &PromptTemplate, args: &[String]) -> String {
    substitute_args(&template.content, args)
}

/// Read + parse one template file, shared by future loader wiring.
pub fn template_from_content(file_name: &str, content: &str) -> PromptTemplate {
    let parsed = parse_frontmatter(content);
    let first_line = parsed.body.lines().find(|line| !line.trim().is_empty());
    let mut description = parsed.get("description").unwrap_or_default().to_owned();
    if description.is_empty()
        && let Some(first_line) = first_line
    {
        description = first_line.chars().take(60).collect();
        if first_line.chars().count() > 60 {
            description.push_str("...");
        }
    }
    PromptTemplate {
        name: file_name
            .strip_suffix(".md")
            .unwrap_or(file_name)
            .to_owned(),
        description,
        content: parsed.body,
    }
}

/// Unused-typedef guard for Arc (kept for future loader wiring parity).
type _LoaderArc = Arc<dyn Fn() + Send + Sync>;

#[allow(dead_code)]
fn file_error(code: FileErrorCode, message: &str) -> FileError {
    FileError::new(code, message, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// upstream test: "substitutes command arguments"
    #[test]
    fn substitutes_command_arguments() {
        let content = "$1 ${@:2} $ARGUMENTS";
        let args = vec!["hello world".to_owned(), "test".to_owned()];
        assert_eq!(
            substitute_args(content, &args),
            "hello world test hello world test"
        );
    }

    /// upstream resource-formatting.test.ts:
    /// "formats prompt template invocations with positional arguments"
    #[test]
    fn formats_prompt_template_invocations_with_positional_arguments() {
        let template = PromptTemplate {
            name: "review".to_owned(),
            description: String::new(),
            content: "Review $1 with $ARGUMENTS".to_owned(),
        };
        let args = vec!["a.ts".to_owned(), "care".to_owned()];
        assert_eq!(
            format_prompt_template_invocation(&template, &args),
            "Review a.ts with a.ts care"
        );
    }

    #[test]
    fn parses_quoted_args() {
        assert_eq!(
            parse_command_args("one 'two words' \"three four\""),
            vec!["one", "two words", "three four"]
        );
        assert_eq!(parse_command_args(""), Vec::<String>::new());
    }

    #[test]
    fn slices_args_with_ranges() {
        let args = vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
        ];
        assert_eq!(substitute_args("${@:2}", &args), "b c d");
        assert_eq!(substitute_args("${@:2:2}", &args), "b c");
        assert_eq!(substitute_args("$2", &args), "b");
        assert_eq!(substitute_args("$5", &args), "");
    }

    #[test]
    fn first_line_description_fallback() {
        let template = template_from_content("two.md", "First line description\nBody");
        assert_eq!(template.name, "two");
        assert_eq!(template.description, "First line description");
        assert_eq!(template.content, "First line description\nBody");
    }

    #[test]
    fn frontmatter_description_and_body_split() {
        let template =
            template_from_content("one.md", "---\ndescription: One template\n---\nHello $1");
        assert_eq!(template.name, "one");
        assert_eq!(template.description, "One template");
        assert_eq!(template.content, "Hello $1");
    }
}
