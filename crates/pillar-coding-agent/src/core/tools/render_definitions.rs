//! Port of the tool render layer (pi v0.84.3): the extension-facing render API
//! (`ToolDefinition.renderCall` / `renderResult` / `renderShell`, upstream
//! `core/extensions/types.ts`) plus each built-in tool's `format*Call` /
//! `format*Result` helpers.
//!
//! divergence: upstream keeps the helpers inside its tool modules and has
//! renderers return TUI components (reusing `context.lastComponent` and
//! mutating `context.state`); the port consolidates them here and has
//! renderers answer styled lines, with any per-renderer state owned by the
//! renderer struct itself.

use std::collections::BTreeMap;
use std::time::Instant;

use pillar_ai::types::Content;

use crate::core::tools::path_utils::resolve_to_cwd;
use crate::core::tools::render_utils::{
    ToolResultBlock, format_path_relative_to_cwd_or_absolute, get_readme_path, get_text_output,
    invalid_arg_text, normalize_display_text, render_tool_path, replace_tabs, shorten_path,
    trim_trailing_empty_lines,
};
use crate::core::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size};
use crate::modes::interactive::components::diff::{RenderDiffOptions, render_diff};
use crate::modes::interactive::components::keybinding_hints::{key_hint, key_text};
use crate::modes::interactive::components::visual_truncate::truncate_to_visual_lines;
use crate::modes::interactive::theme::{Theme, get_language_from_path, highlight_code};
use pillar_tui::text_utils::{apply_background_to_line, truncate_to_width};

/// Line limits used by the collapsed previews (upstream's inline constants).
pub const READ_PREVIEW_LINES: usize = 10;
pub const WRITE_PREVIEW_LINES: usize = 10;
pub const GREP_PREVIEW_LINES: usize = 15;
pub const FIND_PREVIEW_LINES: usize = 20;
pub const LS_PREVIEW_LINES: usize = 20;
pub const BASH_PREVIEW_LINES: usize = 20;

/// Renderer result options (upstream `ToolRenderResultOptions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolRenderResultOptions {
    pub expanded: bool,
    pub is_partial: bool,
}

/// Renderer shell (upstream `renderShell`): `default` puts the renderer's
/// lines inside the tool's background box, `self` lets it frame itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolRenderShell {
    #[default]
    Default,
    SelfRendered,
}

/// Context handed to a renderer (upstream `ToolRenderContext`, minus
/// `lastComponent` / `state` — the port's renderers own their own state).
#[derive(Debug, Clone)]
pub struct ToolRenderContext {
    pub args: serde_json::Value,
    pub tool_call_id: String,
    pub cwd: String,
    pub execution_started: bool,
    pub args_complete: bool,
    pub is_partial: bool,
    pub expanded: bool,
    pub show_images: bool,
    pub is_error: bool,
}

/// A tool result as the renderers see it (upstream the `{ content, details }`
/// shape).
#[derive(Debug, Clone, Copy)]
pub struct ToolRenderResult<'a> {
    pub content: &'a [Content],
    pub details: &'a serde_json::Value,
    pub is_error: bool,
}

impl ToolRenderResult<'_> {
    /// The result's content as render blocks (upstream's TextContent |
    /// ImageContent union).
    pub fn blocks(&self) -> Vec<ToolResultBlock> {
        self.content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text, .. } => Some(ToolResultBlock::Text(text.clone())),
                Content::Image { data, mime_type } => Some(ToolResultBlock::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// The sanitized text output (upstream `getTextOutput`).
    pub fn text_output(&self, show_images: bool) -> String {
        get_text_output(&self.blocks(), show_images)
    }

    fn detail(&self, key: &str) -> Option<&serde_json::Value> {
        self.details.get(key).filter(|value| !value.is_null())
    }

    fn detail_usize(&self, key: &str) -> Option<usize> {
        self.detail(key)
            .and_then(|value| value.as_u64())
            .map(|v| v as usize)
    }

    fn detail_bool(&self, key: &str) -> bool {
        self.detail(key)
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    fn detail_str(&self, key: &str) -> Option<String> {
        self.detail(key)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }

    /// `details.truncation` (upstream `details?.truncation`).
    pub fn truncation(&self) -> Option<TruncationView> {
        let value = self.detail("truncation")?;
        Some(TruncationView {
            truncated: value
                .get("truncated")
                .and_then(|truncated| truncated.as_bool())
                .unwrap_or(false),
            truncated_by: value
                .get("truncatedBy")
                .and_then(|by| by.as_str())
                .map(str::to_string),
            output_lines: value
                .get("outputLines")
                .and_then(|lines| lines.as_u64())
                .map(|lines| lines as usize),
            total_lines: value
                .get("totalLines")
                .and_then(|lines| lines.as_u64())
                .map(|lines| lines as usize),
            max_lines: value
                .get("maxLines")
                .and_then(|lines| lines.as_u64())
                .map(|lines| lines as usize),
            max_bytes: value
                .get("maxBytes")
                .and_then(|bytes| bytes.as_u64())
                .map(|bytes| bytes as usize),
            first_line_exceeds_limit: value
                .get("firstLineExceedsLimit")
                .and_then(|flag| flag.as_bool())
                .unwrap_or(false),
        })
    }
}

/// The `details.truncation` fields the renderers read (upstream the
/// `TruncationResult` subset).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TruncationView {
    pub truncated: bool,
    pub truncated_by: Option<String>,
    pub output_lines: Option<usize>,
    pub total_lines: Option<usize>,
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
    pub first_line_exceeds_limit: bool,
}

/// A tool's renderer (upstream `renderCall` / `renderResult` + `renderShell`).
pub trait ToolRenderer: Send {
    /// The shell the renderer expects (upstream `renderShell`).
    fn render_shell(&self) -> ToolRenderShell {
        ToolRenderShell::Default
    }

    /// Render the tool call (upstream `renderCall`).
    fn render_call(
        &mut self,
        width: usize,
        args: &serde_json::Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Vec<String>;

    /// Render the tool result, or `None` when there is nothing to show
    /// (upstream `renderResult` answering `undefined`).
    fn render_result(
        &mut self,
        _width: usize,
        result: &ToolRenderResult<'_>,
        options: &ToolRenderResultOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Vec<String>>;

    /// Whether the renderer has state (used by tests/hosts).
    fn invalidate(&mut self) {}

    /// Whether this renderer defines a call renderer (upstream checking
    /// `renderCall` for `undefined`); lets the component fall back to the
    /// built-in per method.
    fn has_render_call(&self) -> bool {
        true
    }

    /// Whether this renderer defines a result renderer (upstream checking
    /// `renderResult` for `undefined`).
    fn has_render_result(&self) -> bool {
        true
    }
}

/// Coerce a tool argument to a string, mirroring upstream's `str()` behind a
/// `??` chain: a string passes through, a nullish value yields the next key
/// (and finally the empty string), and any other type answers `None` — the
/// renderers' "invalid arg" case.
fn arg_str(args: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        match args.get(*key) {
            Some(serde_json::Value::String(value)) => return Some(value.clone()),
            Some(serde_json::Value::Null) | None => continue,
            Some(_) => return None,
        }
    }
    Some(String::new())
}

fn arg_number(args: &serde_json::Value, key: &str) -> Option<f64> {
    args.get(key).and_then(|value| value.as_f64())
}

fn arg_usize(args: &serde_json::Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
}

// ============================================================================
// read (upstream read.ts render helpers)
// ============================================================================

/// Compact read classification kinds (upstream `CompactReadClassification`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactReadClassification {
    pub kind: &'static str,
    pub label: String,
}

const COMPACT_RESOURCE_FILE_NAMES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// The `:start-end` suffix for a read call (upstream `formatReadLineRange`).
pub fn format_read_line_range(args: &serde_json::Value, theme: &Theme) -> String {
    let offset = arg_number(args, "offset");
    let limit = arg_number(args, "limit");
    if offset.is_none() && limit.is_none() {
        return String::new();
    }
    let start_line = offset.unwrap_or(1.0);
    let end_line = limit.map(|limit| start_line + limit - 1.0);
    let range = match end_line {
        Some(end) => format!(":{}-{}", start_line as i64, end as i64),
        None => format!(":{}", start_line as i64),
    };
    theme.fg("warning", &range)
}

/// The read call line (upstream `formatReadCall`).
pub fn format_read_call(args: &serde_json::Value, theme: &Theme, cwd: &str) -> String {
    let raw_path = arg_str(args, &["file_path", "path"]);
    let path_display = render_tool_path(raw_path.as_deref(), theme, cwd, None);
    format!(
        "{} {}{}",
        theme.fg("toolTitle", &theme.bold("read")),
        path_display,
        format_read_line_range(args, theme)
    )
}

/// Classify a read call for the compact (collapsed) rendering (upstream
/// `getCompactReadClassification`).
pub fn get_compact_read_classification(
    args: &serde_json::Value,
    cwd: &str,
) -> Option<CompactReadClassification> {
    let raw_path = arg_str(args, &["file_path", "path"])?;
    if raw_path.is_empty() {
        return None;
    }
    let absolute = resolve_to_cwd(&raw_path, cwd);
    let file_name = absolute
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    if file_name == "SKILL.md" {
        let label = absolute
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| file_name.clone());
        return Some(CompactReadClassification {
            kind: "skill",
            label,
        });
    }

    // The package's docs/examples/README reads collapse to a short label.
    let package_root = get_readme_path()
        .parent()
        .map(|parent| parent.to_path_buf())
        .unwrap_or_default();
    if let Ok(relative) = absolute.strip_prefix(&package_root)
        && !relative.as_os_str().is_empty()
    {
        let label = relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if label == "README.md" || label.starts_with("docs/") || label.starts_with("examples/") {
            return Some(CompactReadClassification {
                kind: "docs",
                label,
            });
        }
    }

    if COMPACT_RESOURCE_FILE_NAMES.contains(&file_name.as_str()) {
        return Some(CompactReadClassification {
            kind: "resource",
            label: format_path_relative_to_cwd_or_absolute(&absolute.to_string_lossy(), cwd),
        });
    }

    None
}

/// The compact read call line (upstream `formatCompactReadCall`).
pub fn format_compact_read_call(
    classification: &CompactReadClassification,
    args: &serde_json::Value,
    theme: &Theme,
) -> String {
    let expand_hint = theme.fg(
        "dim",
        &format!(" ({} to expand)", key_text("app.tools.expand")),
    );
    if classification.kind == "skill" {
        return format!(
            "{}{}{}{}",
            theme.fg("customMessageLabel", "\u{1b}[1m[skill]\u{1b}[22m "),
            theme.fg("customMessageText", &classification.label),
            format_read_line_range(args, theme),
            expand_hint
        );
    }
    format!(
        "{} {}{}{}",
        theme.fg(
            "toolTitle",
            &theme.bold(&format!("read {}", classification.kind))
        ),
        theme.fg("accent", &classification.label),
        format_read_line_range(args, theme),
        expand_hint
    )
}

/// The read result body (upstream `formatReadResult`).
pub fn format_read_result(
    args: &serde_json::Value,
    result: &ToolRenderResult<'_>,
    options: &ToolRenderResultOptions,
    theme: &Theme,
    show_images: bool,
    is_error: bool,
) -> String {
    if !options.expanded && !is_error {
        return String::new();
    }
    let raw_path = arg_str(args, &["file_path", "path"]);
    let output = result.text_output(show_images);
    let lang = if !is_error {
        raw_path.as_deref().and_then(get_language_from_path)
    } else {
        None
    };
    let rendered_lines: Vec<String> = match lang {
        Some(lang) => highlight_code(&replace_tabs(&output), Some(lang)),
        None => output.split('\n').map(str::to_string).collect(),
    };
    let lines = trim_trailing_empty_lines(&rendered_lines);
    let max_lines = if options.expanded {
        lines.len()
    } else {
        READ_PREVIEW_LINES
    };
    let display_lines = &lines[..lines.len().min(max_lines)];
    let remaining = lines.len().saturating_sub(max_lines);
    let mut text = format!(
        "\n{}",
        display_lines
            .iter()
            .map(|line| if lang.is_some() {
                replace_tabs(line)
            } else {
                theme.fg("toolOutput", &replace_tabs(line))
            })
            .collect::<Vec<_>>()
            .join("\n")
    );
    if remaining > 0 {
        text.push_str(&format!(
            "{}{}{}",
            theme.fg("muted", &format!("\n... ({remaining} more lines,")),
            key_hint("app.tools.expand", "to expand"),
            theme.fg("muted", ")")
        ));
    }

    if let Some(truncation) = result.truncation()
        && truncation.truncated
    {
        if truncation.first_line_exceeds_limit {
            text.push_str(&format!(
                "\n{}",
                theme.fg(
                    "warning",
                    &format!(
                        "[First line exceeds {} limit]",
                        format_size(truncation.max_bytes.unwrap_or(DEFAULT_MAX_BYTES))
                    )
                )
            ));
        } else if truncation.truncated_by.as_deref() == Some("lines") {
            text.push_str(&format!(
                "\n{}",
                theme.fg(
                    "warning",
                    &format!(
                        "[Truncated: showing {} of {} lines ({} line limit)]",
                        truncation.output_lines.unwrap_or_default(),
                        truncation.total_lines.unwrap_or_default(),
                        truncation.max_lines.unwrap_or(DEFAULT_MAX_LINES)
                    )
                )
            ));
        } else {
            text.push_str(&format!(
                "\n{}",
                theme.fg(
                    "warning",
                    &format!(
                        "[Truncated: {} lines shown ({} limit)]",
                        truncation.output_lines.unwrap_or_default(),
                        format_size(truncation.max_bytes.unwrap_or(DEFAULT_MAX_BYTES))
                    )
                )
            ));
        }
    }
    text
}

/// The read tool's renderer (upstream `createReadToolDefinition`'s renderers).
#[derive(Default)]
pub struct ReadToolRenderer;

impl ToolRenderer for ReadToolRenderer {
    fn render_call(
        &mut self,
        _width: usize,
        args: &serde_json::Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Vec<String> {
        let text = match (!context.expanded)
            .then(|| get_compact_read_classification(args, &context.cwd))
            .flatten()
        {
            Some(classification) => format_compact_read_call(&classification, args, theme),
            None => format_read_call(args, theme, &context.cwd),
        };
        vec![text]
    }

    fn render_result(
        &mut self,
        _width: usize,
        result: &ToolRenderResult<'_>,
        options: &ToolRenderResultOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Vec<String>> {
        Some(vec![format_read_result(
            &context.args,
            result,
            options,
            theme,
            context.show_images,
            context.is_error,
        )])
    }
}

// ============================================================================
// write (upstream write.ts render helpers)
// ============================================================================

/// The write call line (upstream `formatWriteCall`).
pub fn format_write_call(
    args: &serde_json::Value,
    options: &ToolRenderResultOptions,
    theme: &Theme,
    cwd: &str,
) -> String {
    let raw_path = arg_str(args, &["file_path", "path"]);
    let file_content = arg_str(args, &["content"]);
    let path_display = render_tool_path(raw_path.as_deref(), theme, cwd, None);
    let mut text = format!(
        "{} {}",
        theme.fg("toolTitle", &theme.bold("write")),
        path_display
    );

    match file_content {
        Some(content) if content.is_empty() => {}
        None => {
            text.push_str(&format!(
                "\n\n{}",
                theme.fg("error", "[invalid content arg - expected string]")
            ));
        }
        Some(content) => {
            let lang = raw_path.as_deref().and_then(get_language_from_path);
            let rendered_lines: Vec<String> = match lang {
                Some(lang) => {
                    highlight_code(&replace_tabs(&normalize_display_text(&content)), Some(lang))
                }
                None => normalize_display_text(&content)
                    .split('\n')
                    .map(str::to_string)
                    .collect(),
            };
            let lines = trim_trailing_empty_lines(&rendered_lines);
            let total_lines = lines.len();
            let max_lines = if options.expanded {
                lines.len()
            } else {
                WRITE_PREVIEW_LINES
            };
            let display_lines = &lines[..lines.len().min(max_lines)];
            let remaining = lines.len().saturating_sub(max_lines);
            text.push_str(&format!(
                "\n\n{}",
                display_lines
                    .iter()
                    .map(|line| if lang.is_some() {
                        line.clone()
                    } else {
                        theme.fg("toolOutput", &replace_tabs(line))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
            if remaining > 0 {
                text.push_str(&format!(
                    "{}{}{}",
                    theme.fg(
                        "muted",
                        &format!("\n... ({remaining} more lines, {total_lines} total,")
                    ),
                    key_hint("app.tools.expand", "to expand"),
                    theme.fg("muted", ")")
                ));
            }
        }
    }

    text
}

/// The write result body: errors only (upstream `formatWriteResult`).
pub fn format_write_result(result: &ToolRenderResult<'_>, theme: &Theme) -> Option<String> {
    if !result.is_error {
        return None;
    }
    let output = result
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if output.is_empty() {
        return None;
    }
    Some(format!("\n{}", theme.fg("error", &output)))
}

/// The write tool's renderer.
#[derive(Default)]
pub struct WriteToolRenderer;

impl ToolRenderer for WriteToolRenderer {
    fn render_call(
        &mut self,
        _width: usize,
        args: &serde_json::Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Vec<String> {
        vec![format_write_call(
            args,
            &ToolRenderResultOptions {
                expanded: context.expanded,
                is_partial: context.is_partial,
            },
            theme,
            &context.cwd,
        )]
    }

    fn render_result(
        &mut self,
        _width: usize,
        result: &ToolRenderResult<'_>,
        _options: &ToolRenderResultOptions,
        theme: &Theme,
        _context: &ToolRenderContext,
    ) -> Option<Vec<String>> {
        format_write_result(result, theme).map(|text| vec![text])
    }
}

// ============================================================================
// grep / find (upstream grep.ts / find.ts render helpers)
// ============================================================================

/// The grep call line (upstream `formatGrepCall`).
pub fn format_grep_call(args: &serde_json::Value, theme: &Theme) -> String {
    let pattern = arg_str(args, &["pattern"]);
    let raw_path = arg_str(args, &["path"]);
    let path = raw_path
        .as_deref()
        .map(|path| if path.is_empty() { "." } else { path })
        .map(shorten_path);
    let glob = arg_str(args, &["glob"]);
    let limit = arg_usize(args, "limit");
    let invalid = invalid_arg_text(theme);
    let mut text = format!(
        "{} {}{}",
        theme.fg("toolTitle", &theme.bold("grep")),
        match &pattern {
            None => invalid.clone(),
            Some(pattern) => theme.fg("accent", &format!("/{pattern}/")),
        },
        theme.fg(
            "toolOutput",
            &format!(" in {}", path.clone().unwrap_or_else(|| invalid.clone()))
        )
    );
    if let Some(glob) = glob.filter(|glob| !glob.is_empty()) {
        text.push_str(&theme.fg("toolOutput", &format!(" ({glob})")));
    }
    if let Some(limit) = limit {
        text.push_str(&theme.fg("toolOutput", &format!(" limit {limit}")));
    }
    text
}

/// The grep result body (upstream `formatGrepResult`).
pub fn format_grep_result(
    result: &ToolRenderResult<'_>,
    options: &ToolRenderResultOptions,
    theme: &Theme,
    show_images: bool,
) -> String {
    let output = result.text_output(show_images).trim().to_string();
    let mut text = String::new();
    if !output.is_empty() {
        let lines: Vec<&str> = output.split('\n').collect();
        let max_lines = if options.expanded {
            lines.len()
        } else {
            GREP_PREVIEW_LINES
        };
        let display_lines = &lines[..lines.len().min(max_lines)];
        let remaining = lines.len().saturating_sub(max_lines);
        text.push_str(&format!(
            "\n{}",
            display_lines
                .iter()
                .map(|line| theme.fg("toolOutput", line))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        if remaining > 0 {
            text.push_str(&format!(
                "{}{}{}",
                theme.fg("muted", &format!("\n... ({remaining} more lines,")),
                key_hint("app.tools.expand", "to expand"),
                theme.fg("muted", ")")
            ));
        }
    }

    let match_limit = result.detail_usize("matchLimitReached");
    let truncation = result.truncation();
    let lines_truncated = result.detail_bool("linesTruncated");
    let truncated_by_bytes = truncation.as_ref().map(|t| t.truncated).unwrap_or(false);
    if match_limit.is_some() || truncated_by_bytes || lines_truncated {
        let mut warnings: Vec<String> = Vec::new();
        if let Some(match_limit) = match_limit {
            warnings.push(format!("{match_limit} matches limit"));
        }
        if truncated_by_bytes {
            warnings.push(format!(
                "{} limit",
                format_size(
                    truncation
                        .as_ref()
                        .and_then(|t| t.max_bytes)
                        .unwrap_or(DEFAULT_MAX_BYTES)
                )
            ));
        }
        if lines_truncated {
            warnings.push("some lines truncated".to_string());
        }
        text.push_str(&format!(
            "\n{}",
            theme.fg("warning", &format!("[Truncated: {}]", warnings.join(", ")))
        ));
    }
    text
}

/// The grep tool's renderer.
#[derive(Default)]
pub struct GrepToolRenderer;

impl ToolRenderer for GrepToolRenderer {
    fn render_call(
        &mut self,
        _width: usize,
        args: &serde_json::Value,
        theme: &Theme,
        _context: &ToolRenderContext,
    ) -> Vec<String> {
        vec![format_grep_call(args, theme)]
    }

    fn render_result(
        &mut self,
        _width: usize,
        result: &ToolRenderResult<'_>,
        options: &ToolRenderResultOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Vec<String>> {
        Some(vec![format_grep_result(
            result,
            options,
            theme,
            context.show_images,
        )])
    }
}

/// The find call line (upstream `formatFindCall`).
pub fn format_find_call(args: &serde_json::Value, theme: &Theme) -> String {
    let pattern = arg_str(args, &["pattern"]);
    let raw_path = arg_str(args, &["path"]);
    let path = raw_path
        .as_deref()
        .map(|path| if path.is_empty() { "." } else { path })
        .map(shorten_path);
    let limit = arg_usize(args, "limit");
    let invalid = invalid_arg_text(theme);
    let mut text = format!(
        "{} {}{}",
        theme.fg("toolTitle", &theme.bold("find")),
        match &pattern {
            None => invalid.clone(),
            Some(pattern) => theme.fg("accent", pattern),
        },
        theme.fg(
            "toolOutput",
            &format!(" in {}", path.unwrap_or_else(|| invalid.clone()))
        )
    );
    if let Some(limit) = limit {
        text.push_str(&theme.fg("toolOutput", &format!(" (limit {limit})")));
    }
    text
}

/// The find result body (upstream `formatFindResult`).
pub fn format_find_result(
    result: &ToolRenderResult<'_>,
    options: &ToolRenderResultOptions,
    theme: &Theme,
    show_images: bool,
) -> String {
    let output = result.text_output(show_images).trim().to_string();
    let mut text = String::new();
    if !output.is_empty() {
        let lines: Vec<&str> = output.split('\n').collect();
        let max_lines = if options.expanded {
            lines.len()
        } else {
            FIND_PREVIEW_LINES
        };
        let display_lines = &lines[..lines.len().min(max_lines)];
        let remaining = lines.len().saturating_sub(max_lines);
        text.push_str(&format!(
            "\n{}",
            display_lines
                .iter()
                .map(|line| theme.fg("toolOutput", line))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        if remaining > 0 {
            text.push_str(&format!(
                "{}{}{}",
                theme.fg("muted", &format!("\n... ({remaining} more lines,")),
                key_hint("app.tools.expand", "to expand"),
                theme.fg("muted", ")")
            ));
        }
    }

    let result_limit = result.detail_usize("resultLimitReached");
    let truncation = result.truncation();
    let truncated = truncation.as_ref().map(|t| t.truncated).unwrap_or(false);
    if result_limit.is_some() || truncated {
        let mut warnings: Vec<String> = Vec::new();
        if let Some(result_limit) = result_limit {
            warnings.push(format!("{result_limit} results limit"));
        }
        if truncated {
            warnings.push(format!(
                "{} limit",
                format_size(
                    truncation
                        .as_ref()
                        .and_then(|t| t.max_bytes)
                        .unwrap_or(DEFAULT_MAX_BYTES)
                )
            ));
        }
        text.push_str(&format!(
            "\n{}",
            theme.fg("warning", &format!("[Truncated: {}]", warnings.join(", ")))
        ));
    }
    text
}

/// The find tool's renderer.
#[derive(Default)]
pub struct FindToolRenderer;

impl ToolRenderer for FindToolRenderer {
    fn render_call(
        &mut self,
        _width: usize,
        args: &serde_json::Value,
        theme: &Theme,
        _context: &ToolRenderContext,
    ) -> Vec<String> {
        vec![format_find_call(args, theme)]
    }

    fn render_result(
        &mut self,
        _width: usize,
        result: &ToolRenderResult<'_>,
        options: &ToolRenderResultOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Vec<String>> {
        Some(vec![format_find_result(
            result,
            options,
            theme,
            context.show_images,
        )])
    }
}

// ============================================================================
// ls (upstream ls.ts render helpers)
// ============================================================================

/// The ls call line (upstream `formatLsCall`).
pub fn format_ls_call(args: &serde_json::Value, theme: &Theme, cwd: &str) -> String {
    let limit = arg_usize(args, "limit");
    let raw_path = arg_str(args, &["path"]);
    let path_display = render_tool_path(raw_path.as_deref(), theme, cwd, Some("."));
    let mut text = format!(
        "{} {}",
        theme.fg("toolTitle", &theme.bold("ls")),
        path_display
    );
    if let Some(limit) = limit {
        text.push_str(&theme.fg("toolOutput", &format!(" (limit {limit})")));
    }
    text
}

/// The ls result body (upstream `formatLsResult`).
pub fn format_ls_result(
    result: &ToolRenderResult<'_>,
    options: &ToolRenderResultOptions,
    theme: &Theme,
    show_images: bool,
) -> String {
    let output = result.text_output(show_images).trim().to_string();
    let mut text = String::new();
    if !output.is_empty() {
        let lines: Vec<&str> = output.split('\n').collect();
        let max_lines = if options.expanded {
            lines.len()
        } else {
            LS_PREVIEW_LINES
        };
        let display_lines = &lines[..lines.len().min(max_lines)];
        let remaining = lines.len().saturating_sub(max_lines);
        text.push_str(&format!(
            "\n{}",
            display_lines
                .iter()
                .map(|line| theme.fg("toolOutput", line))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        if remaining > 0 {
            text.push_str(&format!(
                "{}{}{}",
                theme.fg("muted", &format!("\n... ({remaining} more lines,")),
                key_hint("app.tools.expand", "to expand"),
                theme.fg("muted", ")")
            ));
        }
    }

    let entry_limit = result.detail_usize("entryLimitReached");
    let truncation = result.truncation();
    let truncated = truncation.as_ref().map(|t| t.truncated).unwrap_or(false);
    if entry_limit.is_some() || truncated {
        let mut warnings: Vec<String> = Vec::new();
        if let Some(entry_limit) = entry_limit {
            warnings.push(format!("{entry_limit} entries limit"));
        }
        if truncated {
            warnings.push(format!(
                "{} limit",
                format_size(
                    truncation
                        .as_ref()
                        .and_then(|t| t.max_bytes)
                        .unwrap_or(DEFAULT_MAX_BYTES)
                )
            ));
        }
        text.push_str(&format!(
            "\n{}",
            theme.fg("warning", &format!("[Truncated: {}]", warnings.join(", ")))
        ));
    }
    text
}

/// The ls tool's renderer.
#[derive(Default)]
pub struct LsToolRenderer;

impl ToolRenderer for LsToolRenderer {
    fn render_call(
        &mut self,
        _width: usize,
        args: &serde_json::Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Vec<String> {
        vec![format_ls_call(args, theme, &context.cwd)]
    }

    fn render_result(
        &mut self,
        _width: usize,
        result: &ToolRenderResult<'_>,
        options: &ToolRenderResultOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Vec<String>> {
        Some(vec![format_ls_result(
            result,
            options,
            theme,
            context.show_images,
        )])
    }
}

// ============================================================================
// edit (upstream edit.ts render helpers)
// ============================================================================

/// The edit call line (upstream `formatEditCall`).
pub fn format_edit_call(args: &serde_json::Value, theme: &Theme, cwd: &str) -> String {
    let raw_path = arg_str(args, &["file_path", "path"]);
    let path_display = render_tool_path(raw_path.as_deref(), theme, cwd, None);
    format!(
        "{} {}",
        theme.fg("toolTitle", &theme.bold("edit")),
        path_display
    )
}

/// The edit result body: errors verbatim, else the rendered diff (upstream
/// `formatEditResult`).
pub fn format_edit_result(
    args: &serde_json::Value,
    result: &ToolRenderResult<'_>,
    theme: &Theme,
    _is_error: bool,
) -> Option<String> {
    let raw_path = arg_str(args, &["file_path", "path"]);
    if result.is_error {
        let error_text = result
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if error_text.is_empty() {
            return None;
        }
        return Some(theme.fg("error", &error_text));
    }

    let result_diff = result.detail_str("diff")?;
    Some(render_diff(
        &result_diff,
        RenderDiffOptions {
            file_path: raw_path,
        },
    ))
}

/// The edit tool's renderer.
#[derive(Default)]
pub struct EditToolRenderer;

impl ToolRenderer for EditToolRenderer {
    /// upstream edit.ts: `renderShell: "self"` — the edit block frames itself
    /// (no tool background box).
    fn render_shell(&self) -> ToolRenderShell {
        ToolRenderShell::SelfRendered
    }

    fn render_call(
        &mut self,
        width: usize,
        args: &serde_json::Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Vec<String> {
        // upstream `buildEditCallComponent`: a Box(0,0) whose bg follows the
        // preview/settled state around `Text(formatEditCall)`. The port has no
        // async preview yet, so the header keeps the pending colour (error
        // once the result settled with an error) and fills the row width.
        let bg_key = if context.is_error {
            "toolErrorBg"
        } else {
            "toolPendingBg"
        };
        let header = apply_background_to_line(
            &format_edit_call(args, theme, &context.cwd),
            width,
            &|text| theme.bg(bg_key, text),
        );
        vec![header]
    }

    fn render_result(
        &mut self,
        _width: usize,
        result: &ToolRenderResult<'_>,
        _options: &ToolRenderResultOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Vec<String>> {
        // upstream `renderResult`: Container with `Spacer(1)` and
        // `Text(output, 1, 0)`.
        format_edit_result(&context.args, result, theme, context.is_error)
            .map(|text| vec![String::new(), format!(" {text}")])
    }
}

// ============================================================================
// bash (upstream bash.ts render helpers)
// ============================================================================

/// Format an elapsed time in seconds with one decimal (upstream
/// `formatDuration`).
pub fn format_duration(ms: f64) -> String {
    format!("{:.1}s", ms / 1000.0)
}

/// The shell call line (upstream `formatShellCall`).
pub fn format_shell_call(args: &serde_json::Value, prompt: &str, theme: &Theme) -> String {
    let command = arg_str(args, &["command"]);
    let timeout = arg_number(args, "timeout");
    let timeout_suffix = match timeout {
        Some(timeout) if timeout != 0.0 => theme.fg("muted", &format!(" (timeout {timeout}s)")),
        _ => String::new(),
    };
    let command_display = match command {
        None => invalid_arg_text(theme),
        Some(command) if command.is_empty() => theme.fg("toolOutput", "..."),
        Some(command) => command,
    };
    format!(
        "{}{}",
        theme.fg(
            "toolTitle",
            &theme.bold(&format!("{prompt} {command_display}"))
        ),
        timeout_suffix
    )
}

/// The bash tool's renderer: it keeps the execution timings and the collapsed
/// preview cache (upstream keeps them in the renderer's `state` and a closure
/// child component).
#[derive(Default)]
pub struct BashToolRenderer {
    prompt: String,
    started_at: Option<Instant>,
    ended_at: Option<Instant>,
    cached_preview: Option<(usize, Vec<String>, usize)>,
}

impl BashToolRenderer {
    pub fn new(prompt: &str) -> Self {
        Self {
            prompt: prompt.to_string(),
            ..Default::default()
        }
    }

    /// When the execution started, if it did.
    pub fn started_at(&self) -> Option<Instant> {
        self.started_at
    }

    /// When the execution finished, if it did.
    pub fn ended_at(&self) -> Option<Instant> {
        self.ended_at
    }

    /// The result body (upstream `rebuildBashResultRenderComponent`).
    fn render_bash_result(
        &mut self,
        result: &ToolRenderResult<'_>,
        options: &ToolRenderResultOptions,
        theme: &Theme,
        show_images: bool,
        width: Option<usize>,
    ) -> Vec<String> {
        let mut output = result.text_output(show_images).trim().to_string();
        let truncation = result.truncation();
        let full_output_path = result.detail_str("fullOutputPath");
        if !options.is_partial
            && truncation.as_ref().map(|t| t.truncated).unwrap_or(false)
            && full_output_path.is_some()
            && output.ends_with(']')
        {
            // Drop the "full output" footer the bash tool appends to the text
            // and re-add it as a warning line below.
            if let Some(footer_start) = output.rfind("\n\n[")
                && output[footer_start..].contains(full_output_path.as_deref().unwrap_or_default())
            {
                output = output[..footer_start].trim_end().to_string();
            }
        }

        let mut lines: Vec<String> = Vec::new();
        if !output.is_empty() {
            let styled_output = output
                .split('\n')
                .map(|line| theme.fg("toolOutput", line))
                .collect::<Vec<_>>()
                .join("\n");
            if options.expanded {
                lines.push(String::new());
                lines.extend(format!("\n{styled_output}").split('\n').map(str::to_string));
            } else {
                let width = width.unwrap_or(80);
                let cached = match &self.cached_preview {
                    Some((cached_width, cached_lines, cached_skipped))
                        if *cached_width == width =>
                    {
                        Some((cached_lines.clone(), *cached_skipped))
                    }
                    _ => None,
                };
                let (preview_lines, skipped) = match cached {
                    Some(cached) => cached,
                    None => {
                        let preview =
                            truncate_to_visual_lines(&styled_output, BASH_PREVIEW_LINES, width, 0);
                        self.cached_preview =
                            Some((width, preview.visual_lines.clone(), preview.skipped_count));
                        (preview.visual_lines, preview.skipped_count)
                    }
                };
                if skipped > 0 {
                    let hint = format!(
                        "{}{}{}",
                        theme.fg("muted", &format!("... ({skipped} earlier lines,")),
                        key_hint("app.tools.expand", "to expand"),
                        theme.fg("muted", ")")
                    );
                    lines.push(String::new());
                    lines.push(truncate_to_width(&hint, width, "...", false));
                } else {
                    lines.push(String::new());
                }
                lines.extend(preview_lines);
            }
        }

        let truncated = truncation.as_ref().map(|t| t.truncated).unwrap_or(false);
        if truncated || full_output_path.is_some() {
            let mut warnings: Vec<String> = Vec::new();
            if let Some(path) = &full_output_path {
                warnings.push(format!("Full output: {path}"));
            }
            if truncated {
                let truncation = truncation.as_ref().expect("truncated");
                if truncation.truncated_by.as_deref() == Some("lines") {
                    warnings.push(format!(
                        "Truncated: showing {} of {} lines",
                        truncation.output_lines.unwrap_or_default(),
                        truncation.total_lines.unwrap_or_default()
                    ));
                } else {
                    warnings.push(format!(
                        "Truncated: {} lines shown ({} limit)",
                        truncation.output_lines.unwrap_or_default(),
                        format_size(truncation.max_bytes.unwrap_or(DEFAULT_MAX_BYTES))
                    ));
                }
            }
            lines.push(String::new());
            lines.push(theme.fg("warning", &format!("[{}]", warnings.join(". "))));
        }

        if let Some(started_at) = self.started_at {
            let label = if options.is_partial {
                "Elapsed"
            } else {
                "Took"
            };
            let end_time = self.ended_at.unwrap_or_else(Instant::now);
            lines.push(String::new());
            lines.push(theme.fg(
                "muted",
                &format!(
                    "{label} {}",
                    format_duration(end_time.duration_since(started_at).as_secs_f64() * 1000.0)
                ),
            ));
        }

        lines
    }
}

impl ToolRenderer for BashToolRenderer {
    fn render_call(
        &mut self,
        _width: usize,
        args: &serde_json::Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Vec<String> {
        if context.execution_started && self.started_at.is_none() {
            self.started_at = Some(Instant::now());
            self.ended_at = None;
        }
        vec![format_shell_call(args, &self.prompt, theme)]
    }

    fn render_result(
        &mut self,
        _width: usize,
        result: &ToolRenderResult<'_>,
        options: &ToolRenderResultOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Vec<String>> {
        if !options.is_partial || context.is_error {
            self.ended_at = Some(self.ended_at.unwrap_or_else(Instant::now));
        }
        Some(self.render_bash_result(result, options, theme, context.show_images, None))
    }

    fn invalidate(&mut self) {
        self.cached_preview = None;
    }
}

// ============================================================================
// Renderer registry (upstream `createAllToolDefinitions`)
// ============================================================================

/// The renderer for a tool, or `None` when the tool has none (upstream's
/// optional `renderCall` / `renderResult`).
pub fn create_tool_renderer(tool_name: &str, cwd: &str) -> Option<Box<dyn ToolRenderer>> {
    match tool_name {
        "read" => Some(Box::new(ReadToolRenderer)),
        "bash" => Some(Box::new(BashToolRenderer::new(cwd))),
        "edit" => Some(Box::new(EditToolRenderer)),
        "write" => Some(Box::new(WriteToolRenderer)),
        "grep" => Some(Box::new(GrepToolRenderer)),
        "find" => Some(Box::new(FindToolRenderer)),
        "ls" => Some(Box::new(LsToolRenderer)),
        _ => None,
    }
}

/// Every built-in renderer keyed by tool name (upstream
/// `createAllToolDefinitions`).
pub fn create_all_tool_renderers(cwd: &str) -> BTreeMap<String, Box<dyn ToolRenderer>> {
    let mut renderers: BTreeMap<String, Box<dyn ToolRenderer>> = BTreeMap::new();
    for name in ["read", "bash", "edit", "write", "grep", "find", "ls"] {
        if let Some(renderer) = create_tool_renderer(name, cwd) {
            renderers.insert(name.to_string(), renderer);
        }
    }
    renderers
}

/// Render a tool path for callers that only need the display string (used by
/// tests and hosts).
pub fn render_path_for_tool(raw_path: Option<&str>, theme: &Theme, cwd: &str) -> String {
    render_tool_path(raw_path, theme, cwd, None)
}

/// Resolve a tool argument path against the cwd (re-exported for renderers).
pub fn resolve_argument_path(path: &str, cwd: &str) -> String {
    resolve_to_cwd(path, cwd).to_string_lossy().to_string()
}
